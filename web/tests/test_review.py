"""Regressions for B5 review D/S findings; all state and transports are private."""
import asyncio
from datetime import datetime, timezone
from email.utils import format_datetime
import json
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

from web.deploy.cloudflare_firewall import allows_443, audit
from web.server.auth import AccessMiddleware
from web.server.push import PushService, PushStore, event_payload, retry_after
from web.server.rpc import DaemonClient, DaemonError
from web.tests.test_backend import FakeDaemon, FakeVerifier, device, settings


class ReviewTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        temp = tempfile.TemporaryDirectory(); self.addCleanup(temp.cleanup)
        self.cfg = settings(Path(temp.name))
        self.store = PushStore(self.cfg.push_path)
        self.payload = {"title":"Agent", "body":"Hello", "url":"/#alerts", "tag":"one", "badge":1}

    async def test_endpoint_concurrency_cap_and_one_checkpoint_per_sweep(self):
        active, maximum, finished = 0, 0, []
        lock = threading.Lock()
        gate = threading.Barrier(4)
        for i in range(8): self.store.subscribe(device(str(i)))
        self.store.enqueue([self.payload])
        def send(data, payload):
            nonlocal active, maximum
            with lock:
                active += 1; maximum = max(maximum, active)
            gate.wait(timeout=3)
            with lock:
                active -= 1; finished.append(data["endpoint"])
            return 201
        with patch.object(self.store, "save", wraps=self.store.save) as saved:
            await PushService(self.cfg, FakeDaemon(), store=self.store, sender=send).deliver_due()
        self.assertEqual(maximum, 4)
        self.assertEqual(len(finished), 8)
        self.assertEqual(saved.call_count, 1)
        self.assertFalse(self.store.state["jobs"])

    async def test_slow_endpoint_does_not_prevent_good_endpoint_delivery(self):
        slow, good = device("slow"), device("good")
        self.store.subscribe(slow); self.store.subscribe(good); self.store.enqueue([self.payload])
        entered, release, delivered = threading.Event(), threading.Event(), threading.Event()
        def send(data, payload):
            if data["endpoint"] == slow["endpoint"]:
                entered.set(); release.wait(timeout=3); return 503
            delivered.set(); return 201
        service = PushService(self.cfg, FakeDaemon(), store=self.store, sender=send)
        task = asyncio.create_task(service.deliver_due())
        try:
            self.assertTrue(await asyncio.to_thread(entered.wait, 1))
            self.assertTrue(await asyncio.to_thread(delivered.wait, 1))
        finally:
            release.set(); await task
        self.assertEqual(len(self.store.state["jobs"]), 1)
        self.assertEqual(next(iter(self.store.state["jobs"].values()))["endpoint"], slow["endpoint"])

    async def test_retry_after_applies_to_every_job_for_endpoint_and_survives_restart(self):
        sub = device(); self.store.subscribe(sub)
        self.store.enqueue([self.payload, {**self.payload,"tag":"two"}])
        sender = Mock(return_value=(429, 120))
        service = PushService(self.cfg, FakeDaemon(), store=self.store, sender=sender)
        await service.deliver_due()
        next_at = self.store.state["backoffs"][sub["endpoint"]]["next_at"]
        self.assertGreaterEqual(next_at, time.time() + 119)
        reopened = PushStore(self.cfg.push_path)
        await PushService(self.cfg, FakeDaemon(), store=reopened, sender=sender).deliver_due()
        self.assertEqual(sender.call_count, 1, "another job bypassed endpoint backoff")
        self.assertEqual(len(reopened.state["jobs"]), 2)
        self.assertAlmostEqual(retry_after(format_datetime(datetime.fromtimestamp(time.time()+90,timezone.utc))),90,delta=2)
        self.assertIsNone(retry_after("not a date")); self.assertIsNone(retry_after("NaN"))

    async def test_permanent_client_failures_drop_only_the_job_and_vapid_uses_origin(self):
        sub = device(); self.store.subscribe(sub)
        for code in (400,403,413):
            self.store.enqueue([{**self.payload,"tag":str(code)}])
            service = PushService(self.cfg,FakeDaemon(),store=self.store,sender=lambda *args:code)
            await service.deliver_due()
            self.assertFalse(self.store.state["jobs"])
            self.assertIn(sub["endpoint"],self.store.state["subscriptions"])
        with patch("web.server.push.webpush", return_value=Mock(status_code=201,headers={})) as send:
            PushService(self.cfg,FakeDaemon(),store=self.store)._send(sub,self.payload)
            self.assertEqual(send.call_args.kwargs["vapid_claims"],{"sub":self.cfg.origin})

    async def test_unsubscribe_while_delivery_runs_does_not_restore_jobs(self):
        sub=device();self.store.subscribe(sub);self.store.enqueue([self.payload])
        entered,release=threading.Event(),threading.Event()
        def send(*args): entered.set();release.wait(timeout=3);return 503
        task=asyncio.create_task(PushService(self.cfg,FakeDaemon(),store=self.store,sender=send).deliver_due())
        self.assertTrue(await asyncio.to_thread(entered.wait,1))
        self.store.unsubscribe(sub["endpoint"]);release.set();await task
        self.assertFalse(self.store.state["jobs"]);self.assertFalse(self.store.state["backoffs"])

    async def test_bad_events_are_skipped_and_cursor_advances_through_all_bad_page(self):
        self.store.subscribe(device())
        good={"id":"good","title":"Agent","body":"Hello","url":"/#alerts","created_at":time.time()}
        pages=[{"events":[None,{}, {**good,"url":"//evil"},{**good,"url":"/\n/evil"},good],"cursor":{"sequence":1}},
               {"events":[{**good,"id":None},{**good,"created_at":"bad"}],"cursor":{"sequence":2}}]
        class Daemon(FakeDaemon):
            async def call(self,method,params,**kwargs):
                if method=="owner.notifications.next":
                    if not pages: raise asyncio.CancelledError()
                    return pages.pop(0)
                return await super().call(method,params,**kwargs)
        with self.assertLogs("web.server.push",level="WARNING") as messages:
            with self.assertRaises(asyncio.CancelledError):
                await PushService(self.cfg,Daemon(),store=self.store).feed_loop()
        self.assertEqual(len(messages.records),6)
        self.assertEqual(self.store.state["cursor"],{"sequence":2})
        self.assertEqual(len(self.store.state["jobs"]),1)
        self.assertEqual(next(iter(self.store.state["jobs"].values()))["payload"]["tag"],"good")

    async def test_non_http_scope_and_malformed_headers_fail_closed(self):
        called=[]; output=[]
        async def inner(*args): called.append(True)
        async def send(message): output.append(message)
        async def receive(): return {"type":"http.request","body":b"","more_body":False}
        middleware=AccessMiddleware(inner,FakeVerifier(),self.cfg.origin)
        await middleware({"type":"websocket"},receive,send)
        self.assertEqual(output[-1],{"type":"websocket.close","code":1008})
        with self.assertRaises(ValueError): await middleware({"type":"other"},receive,send)
        await middleware({"type":"http","path":"/api/counts","method":"GET","headers":[(b"broken",b"\xff")]},receive,send)
        self.assertEqual(next(m for m in output if m["type"]=="http.response.start")["status"],403)
        self.assertFalse(called)
        await middleware({"type":"lifespan"},receive,send);self.assertTrue(called)

    async def test_rpc_timeout_includes_waiting_for_a_slot(self):
        client=DaemonClient(self.cfg)
        for _ in range(4): await client.slots.acquire()
        started=time.monotonic()
        with self.assertRaises(DaemonError) as error:
            await client.call("messaging.counts",{},timeout=0.03)
        self.assertEqual(error.exception.code,"daemon_busy")
        self.assertLess(time.monotonic()-started,0.5)
        for _ in range(4): client.slots.release()


class FirewallTests(unittest.TestCase):
    def test_conflicting_ingress_priorities_protocols_and_ranges_fail_closed(self):
        nets={4:["173.245.48.0/20"],6:["2606:4700::/32"]}
        base={"name":"broad","priority":900,"direction":"INGRESS","allowed":[{"IPProtocol":"tcp","ports":["443"]}]}
        for change in ({},{"priority":901},{"priority":0},{"allowed":[{"IPProtocol":"all"}]},
                       {"allowed":[{"IPProtocol":"6"}]},{"allowed":[{"IPProtocol":"tcp","ports":["400-500"]}]}):
            with self.assertRaises(ValueError):audit([{**base,**change}],nets,"cm-board-origin","default",False)
        for change in ({"priority":902},{"disabled":True},{"direction":"EGRESS"},
                       {"allowed":[{"IPProtocol":"tcp","ports":["80"]}]},{"allowed":[{"IPProtocol":"udp","ports":["443"]}]}):
            audit([{**base,**change}],nets,"cm-board-origin","default",False)
        managed={**base,"name":"cm-board-cloudflare-v4","targetTags":["cm-board-origin"],
                 "sourceRanges":nets[4],"network":"projects/p/global/networks/default"}
        audit([managed],nets,"cm-board-origin","default",True)
        for change in ({"sourceRanges":["0.0.0.0/0"]},{"sourceTags":["anything"]},{"targetTags":[]}):
            with self.assertRaises(ValueError):audit([{**managed,**change}],nets,"cm-board-origin","default",True)
