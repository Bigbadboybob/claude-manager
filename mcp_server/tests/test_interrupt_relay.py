import asyncio
import tempfile
import unittest
from pathlib import Path
from unittest.mock import AsyncMock

from mcp_server.codex_state import Thread
from mcp_server.native_codex import Relay
from mcp_server.notifications import Queue, consume


class InterruptRelayTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.relay = Relay("unused", Queue("uid", Path(self.tmp.name)))
        self.relay.thread = {"id": "thread", "path": "/rollout"}
        self.relay.state.foreground = "thread"
        self.relay.state.threads["thread"] = Thread(turn_id="old", status="active")
        self.relay.ready.set()
        self.req = {"request_id": "request-1", "session_uid": "uid", "thread_id": "thread",
                    "incarnation": 7, "note": "stop TP1"}

    async def test_interrupt_precedes_user_stop_and_queued_wake(self):
        sequence = []
        entered = asyncio.Event()
        release = asyncio.Event()

        async def escape():
            sequence.append("escape")
            entered.set()
            await release.wait()
            thread = self.relay.state.threads["thread"]
            thread.completed_id, thread.last_status = "old", "interrupted"

        async def call(method, params):
            sequence.append((method, params))
            return {"turn": {"id": "stop-turn"}}

        self.relay.call = call
        stop = asyncio.create_task(self.relay.interrupt_and_stop(self.req, escape))
        await entered.wait()
        wake = asyncio.create_task(self.relay.send({"binding": {"thread_id": "thread"}, "text": "wake"}))
        await asyncio.sleep(0)
        self.assertEqual(sequence, ["escape"])
        release.set()
        self.assertEqual((await stop)["status"], "delivered")
        await wake
        self.assertEqual([item if isinstance(item, str) else item[0] for item in sequence],
                         ["escape", "turn/start", "turn/start"])
        self.assertEqual(sequence[1][1]["input"][0]["type"], "text")
        self.assertIn("Do not resume the interrupted work", sequence[1][1]["input"][0]["text"])
        self.assertIn("toolOutput", sequence[2][1])

    async def test_wrong_thread_and_ambiguous_retry_never_send(self):
        escape = AsyncMock(side_effect=OSError("lost"))
        other = dict(self.req, thread_id="other")
        self.assertEqual((await self.relay.interrupt_and_stop(other, escape))["status"], "refused")
        escape.assert_not_awaited()
        result = await self.relay.interrupt_and_stop(self.req, escape)
        self.assertEqual(result["status"], "uncertain")
        self.assertEqual(await self.relay.interrupt_and_stop(self.req, escape), result)
        escape.assert_awaited_once()

    async def test_a_new_foreground_turn_is_not_sent_the_old_stop(self):
        async def escape():
            thread = self.relay.state.threads["thread"]
            thread.completed_id, thread.last_status, thread.turn_id = "old", "interrupted", "new"

        self.relay.call = AsyncMock()
        result = await self.relay.interrupt_and_stop(self.req, escape)
        self.assertEqual(result, {"status": "uncertain", "reason": "turn_changed"})
        self.relay.call.assert_not_awaited()

    async def test_notification_remains_pending_while_interrupt_holds_relay(self):
        self.relay._send_locked = AsyncMock(return_value={"kind": "fixture"})
        self.relay.observed = AsyncMock(return_value=False)
        async with self.relay.delivery_lock:
            task = asyncio.create_task(consume(self.relay.queue, self.relay))
            self.relay.queue.publish("board", "board", "[board] wake", "[board]")
            await asyncio.sleep(0.1)
            self.assertEqual(self.relay.queue.get("board")["status"], "pending")
            self.relay._send_locked.assert_not_awaited()
        try:
            for _ in range(100):
                if self.relay.queue.get("board")["status"] == "submitted":
                    break
                await asyncio.sleep(0.02)
            self.assertEqual(self.relay.queue.get("board")["status"], "submitted")
        finally:
            task.cancel()
            await asyncio.gather(task, return_exceptions=True)

    async def test_frontend_user_turn_waits_for_stop_submission(self):
        import json
        class Frontend:
            def __aiter__(self):
                self.items = iter([json.dumps({"id": 3, "method": "turn/start",
                    "params": {"threadId": "thread", "input": [{"type": "text", "text": "new"}]}})])
                return self
            async def __anext__(self):
                try:
                    return next(self.items)
                except StopIteration:
                    raise StopAsyncIteration

        self.relay.upstream = type("Upstream", (), {"send": AsyncMock()})()
        self.relay.connected.set()
        self.relay.interrupt_waiters = 1
        async with self.relay.delivery_lock:
            task = asyncio.create_task(self.relay.serve_frontend(Frontend()))
            await asyncio.sleep(0)
            self.relay.upstream.send.assert_not_awaited()
        await task
        self.relay.upstream.send.assert_awaited_once()

    async def test_lost_receipt_is_not_replayed_after_relay_restart(self):
        from mcp_server.notifications import atomic_write
        import hashlib
        path = self.relay.queue.path / ("interrupt-" + hashlib.sha256(b"request-1").hexdigest() + ".receipt")
        identity = {k: self.req.get(k) for k in
                    ("request_id", "session_uid", "thread_id", "pid", "incarnation", "note")}
        atomic_write(path, {"identity": identity, "result": None})
        escape = AsyncMock()
        result = await self.relay.interrupt_and_stop(self.req, escape)
        self.assertEqual(result, {"status": "uncertain", "reason": "operation_interrupted"})
        escape.assert_not_awaited()

    async def test_only_the_daemon_peer_can_issue_the_bound_command(self):
        import os
        import json
        from unittest.mock import patch
        self.req["pid"] = os.getpid()
        sock = str(Path(self.tmp.name) / "test.sock")
        server = await asyncio.start_unix_server(self.relay.serve_interrupt, sock)
        self.addAsyncCleanup(server.wait_closed)
        self.addCleanup(server.close)
        self.relay.call = AsyncMock(return_value={"turn": {"id": "stop"}})

        def rpc(method, params):
            if method == "session.interrupt_authority":
                return {"pid": os.getpid(), "session_pid": os.getpid(),
                        "incarnation": 7, "thread_id": "thread"}
            self.assertEqual(method, "session.send_bound_escape")
            thread = self.relay.state.threads["thread"]
            thread.completed_id, thread.last_status = "old", "interrupted"
            return {"ok": True}

        with patch("mcp_server.native_codex.control_client.call", side_effect=rpc):
            reader, writer = await asyncio.open_unix_connection(sock)
            writer.write(json.dumps(self.req).encode() + b"\n")
            await writer.drain()
            reply = json.loads(await asyncio.wait_for(reader.readline(), 3))
            writer.close()
            await writer.wait_closed()
        self.assertEqual(reply["status"], "delivered")
        self.relay.call.assert_awaited_once()

        with patch("mcp_server.native_codex.control_client.call", return_value={"pid": -1}):
            reader, writer = await asyncio.open_unix_connection(sock)
            writer.write(json.dumps(self.req).encode() + b"\n")
            await writer.drain()
            self.assertEqual(await asyncio.wait_for(reader.readline(), 3), b"")
            writer.close()
            await writer.wait_closed()
        self.relay.call.assert_awaited_once()
