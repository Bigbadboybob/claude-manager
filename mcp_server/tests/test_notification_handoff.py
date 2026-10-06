"""Consumer handoff, delivery health, channel opt-in detection, wake content
and the slim chat_read projection (2026-10-06 stale-consumer incident)."""
import asyncio
import json
import os
import tempfile
import time
import unittest
from pathlib import Path
from unittest import mock

from mcp_server import control_client, notifications, server
from mcp_server.claude_channel import CHAT_CONTENT_MAX, ClaudeChannel, channel_content
from mcp_server.native_claude import (
    ClaudeAdapter,
    argv_session_id,
    channel_flag_present,
)
from mcp_server.notifications import Queue, consume, delivery_health


async def eventually(predicate, timeout=5):
    end = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= end:
            raise AssertionError("condition did not become true")
        await asyncio.sleep(0.02)


class Fixture:
    engine = "fixture"

    def __init__(self, name, stale=(), claim=None):
        self.name = name
        self.stale = list(stale)
        self.claim = claim
        self.sent = []

    def identity(self):
        return {"fixture": self.name}

    async def send(self, event):
        self.sent.append(event["id"])
        return {"kind": "fixture"}

    async def observed(self, event):
        return False

    async def diagnostics(self, events):
        return {"stale": self.stale}

    async def takeover_claim(self):
        return self.claim


def ev(id, status, at):
    return {"id": id, "status": status, "created_at": at, "updated_at": at, "marker": f"[m {id}]"}


class DeliveryHealthTests(unittest.TestCase):
    def test_stall_is_measured_from_the_latest_observation(self):
        now = 10_000.0
        self.assertFalse(delivery_health([ev("a", "submitted", now - 10)], now)["stalled"])
        stalled = delivery_health([ev("a", "submitted", now - 400), ev("b", "uncertain", now - 350)], now)
        self.assertTrue(stalled["stalled"])
        self.assertEqual((stalled["unobserved"], stalled["oldest_unobserved_s"]), (2, 400))
        recovered = delivery_health([ev("a", "submitted", now - 400), ev("b", "observed", now - 5)], now)
        self.assertFalse(recovered["stalled"])
        self.assertEqual(recovered["unobserved"], 0)

    def test_snapshot_reports_degraded_not_connected_while_submissions_vanish(self):
        with tempfile.TemporaryDirectory() as tmp:
            q = Queue("s", Path(tmp))
            q.health("claude-code", "ready")
            q.publish("one", "chat", "wake", "[cm-chat one]")
            self.assertEqual(q.snapshot()["health"]["status"], "ready")
            self.assertTrue(q.snapshot()["transport"]["connected"])
            q.change("one", {"pending"}, status="submitted")
            with mock.patch.object(notifications.time, "time", return_value=time.time() + 3600):
                q.health("claude-code", "ready")
                snap = q.snapshot()
            self.assertEqual(snap["health"]["status"], "degraded")
            self.assertIn("unobserved_submissions", snap["health"]["reasons"])
            self.assertFalse(snap["transport"]["connected"])
            q.health("claude-code", "degraded", reason="channels_disabled", reasons=["channels_disabled"])
            self.assertEqual(q.snapshot()["health"]["reasons"], ["channels_disabled"])

    def test_mismatch_alone_is_informational_but_degrades_with_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            q = Queue("s", Path(tmp))
            # Same client after an in-pane resume: delivery still works.
            resumed = Fixture("same", stale=["transcript_mismatch"])
            resumed.diagnostics = mock.AsyncMock(return_value={
                "transcript_mismatch": True, "stale": ["transcript_mismatch"]})
            self.assertEqual(asyncio.run(notifications._diagnose(q, resumed, []))["status"], "ready")
            forked = Fixture("old")
            forked.diagnostics = mock.AsyncMock(return_value={
                "transcript_mismatch": True, "stale": ["transcript_mismatch", "continued_in"]})
            result = asyncio.run(notifications._diagnose(q, forked, []))
            self.assertEqual((result["status"], result["extra"]["reason"]), ("degraded", "transcript_mismatch"))

    def test_takeover_requests_are_not_events_and_expire_with_their_process(self):
        with tempfile.TemporaryDirectory() as tmp:
            q = Queue("s", Path(tmp))
            q.request_takeover({"session_id": "new"})
            self.assertEqual(q.snapshot()["notifications"], [])  # never parsed as an event
            self.assertEqual(q.takeover_requests(), [])  # own request is not a successor
            other = q.takeover_path(999_999_999)
            notifications.atomic_write(other, {"pid": 999_999_999, "requested_at": time.time()})
            self.assertEqual(q.takeover_requests(), [])  # dead pid
            self.assertFalse(other.exists())


class HandoffTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.queue = Queue("session-a", Path(self.tmp.name))
        self.tasks = []
        patcher = mock.patch.object(notifications, "HANDOFF_MIN_HOLD_S", 0)
        patcher.start()
        self.addCleanup(patcher.stop)

    async def asyncTearDown(self):
        for task in self.tasks:
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)
        self.tmp.cleanup()

    def run_consumer(self, adapter, loop=True):
        async def forever():
            while True:
                if not await consume(self.queue, adapter):
                    return

        task = asyncio.create_task(forever() if loop else consume(self.queue, adapter))
        self.tasks.append(task)
        return task

    def foreign_request(self, claim):
        # Same-process fixtures: file the successor's request under another
        # live pid (our parent) so the holder does not treat it as its own.
        notifications.atomic_write(self.queue.takeover_path(os.getppid()),
                                   dict(claim, pid=os.getppid(), requested_at=time.time()))

    async def test_stale_holder_hands_off_to_live_waiter(self):
        old = Fixture("old", stale=["transcript_mismatch"])
        first = self.run_consumer(old, loop=False)
        await asyncio.sleep(0.1)
        self.foreign_request({"session_id": "new"})
        reason = await asyncio.wait_for(first, 8)
        self.assertEqual(reason, "transcript_mismatch")
        self.assertEqual(json.loads((self.queue.path / "transport.json").read_text())["status"], "handed_off")
        new = Fixture("new", claim={"session_id": "new"})
        self.run_consumer(new)
        self.queue.publish("one", "chat", "wake", "[cm-chat one]")
        await eventually(lambda: new.sent == ["one"])
        self.assertEqual(old.sent, [])

    async def test_no_handoff_without_holder_evidence_or_without_a_live_waiter(self):
        healthy = Fixture("healthy")
        first = self.run_consumer(healthy, loop=False)
        self.foreign_request({"session_id": "other"})
        await asyncio.sleep(5.5)  # one full heartbeat/assessment cycle
        self.assertFalse(first.done(), "a healthy holder never releases")
        first.cancel()
        await asyncio.gather(first, return_exceptions=True)
        for path in self.queue.path.glob("takeover-*.request"):
            path.unlink()
        stale = Fixture("stale", stale=["continued_in"])
        second = self.run_consumer(stale, loop=False)
        await asyncio.sleep(5.5)
        self.assertFalse(second.done(), "evidence alone is not a successor")

    async def test_handed_off_consumer_does_not_reclaim_without_its_own_claim(self):
        old = Fixture("old", stale=["transcript_mismatch"])
        self.run_consumer(old)
        await asyncio.sleep(0.1)
        self.foreign_request({"session_id": "new"})
        await eventually(lambda: json.loads((self.queue.path / "transport.json").read_text())["status"] == "handed_off", 8)
        new = Fixture("new")
        self.run_consumer(new)
        self.queue.publish("one", "chat", "wake", "[cm-chat one]")
        await eventually(lambda: new.sent == ["one"])
        # The old process waits as a non-holder and files no request.
        self.assertFalse(self.queue.takeover_path(os.getpid()).exists())
        self.assertEqual(old.sent, [])


class ClaudeEvidenceTests(unittest.IsolatedAsyncioTestCase):
    def test_argv_parsing(self):
        self.assertEqual(argv_session_id(["claude", "--session-id", "abc"]), "abc")
        self.assertEqual(argv_session_id(["claude", "bg-pty-host", "--session-id", "new",
                                          "--fork-session", "--resume", "old"]), "new")
        self.assertIsNone(argv_session_id(["claude", "--fork-session", "--resume", "old"]))
        self.assertEqual(argv_session_id(["claude", "--resume=old"]), "old")
        self.assertTrue(channel_flag_present(
            ["claude", "--dangerously-load-development-channels", "server:claude-manager"]))
        self.assertTrue(channel_flag_present(["claude", "--channels=server:x,server:claude-manager"]))
        self.assertFalse(channel_flag_present(["claude", "bg-pty-host", "--session-id", "a"]))
        self.assertFalse(channel_flag_present(
            ["claude", "--dangerously-load-development-channels", "server:other"]))

    def test_own_transcript_evidence(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "old.jsonl"
            markers = ["[cm-chat a]", "[cm-chat b]", "[cm-chat c]"]

            def write(records):
                path.write_text("".join(json.dumps(r) + "\n" for r in records))

            enq = lambda m: {"type": "queue-operation", "operation": "enqueue",
                             "content": f"<channel>{m} hint</channel>"}
            write([enq(m) for m in markers])
            self.assertEqual(ClaudeAdapter.stale_evidence(str(path), markers), ["enqueued_unobserved"])
            write([enq(markers[0]), enq(markers[1]), {"type": "queue-operation", "operation": "dequeue"},
                   enq(markers[2])])
            self.assertEqual(ClaudeAdapter.stale_evidence(str(path), markers), [])
            write([{"type": "continued-in", "continuedInSessionId": "new"}])
            self.assertEqual(ClaudeAdapter.stale_evidence(str(path), markers), ["continued_in"])

    async def test_fork_leaves_old_consumer_stale_and_new_one_live(self):
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp)
            (project / "old.jsonl").write_text(json.dumps(
                {"type": "continued-in", "continuedInSessionId": "new"}) + "\n")
            (project / "new.jsonl").write_text("")
            bound = {"transcript_path": str(project / "new.jsonl")}
            with mock.patch.object(control_client, "call", return_value=bound):
                with mock.patch.dict(os.environ, {"CLAUDE_CODE_SESSION_ID": "old"}):
                    old = ClaudeAdapter("uid")
                    diag = await old.diagnostics([])
                    self.assertIsNone(await old.takeover_claim())
                self.assertTrue(diag["transcript_mismatch"])
                self.assertEqual(diag["stale"], ["transcript_mismatch", "continued_in"])
                with mock.patch.dict(os.environ, {"CLAUDE_CODE_SESSION_ID": "new"}):
                    new = ClaudeAdapter("uid")
                    self.assertEqual((await new.diagnostics([]))["stale"], [])
                    self.assertEqual((await new.takeover_claim())["session_id"], "new")

    async def test_channels_disabled_client_keeps_notices_pending_and_reports_degraded(self):
        for argv, blocked in ((["claude", "bg-pty-host", "--session-id", "x"], True),
                              (["claude", "--dangerously-load-development-channels",
                                "server:claude-manager"], False),
                              (None, False)):
            with self.subTest(argv=argv), tempfile.TemporaryDirectory() as tmp, \
                    mock.patch("mcp_server.claude_channel.claude_parent_argv", return_value=argv), \
                    mock.patch.object(control_client, "call", return_value={}), \
                    mock.patch.dict(os.environ, {"CLAUDE_CODE_SESSION_ID": ""}):
                q = Queue("fixture", Path(tmp))
                q.publish("one", "chat", "hint", "[cm-chat one]")
                channel = ClaudeChannel(q.uid)
                channel.session = mock.Mock(send_notification=mock.AsyncMock())
                task = asyncio.create_task(consume(q, channel))
                try:
                    await asyncio.sleep(0.3)
                    status = q.get("one")["status"]
                    transport = json.loads((q.path / "transport.json").read_text())
                finally:
                    task.cancel()
                    await asyncio.gather(task, return_exceptions=True)
                if blocked:
                    self.assertEqual(status, "pending")
                    self.assertEqual((transport["status"], transport["reason"]),
                                     ("degraded", "channels_disabled"))
                    channel.session.send_notification.assert_not_called()
                else:
                    self.assertEqual(status, "submitted")
                    self.assertEqual(transport["status"], "ready")


class WakeContentTests(unittest.TestCase):
    def test_daemon_summary_passes_through_and_legacy_text_is_reduced(self):
        summary = ('[cm-chat w] 3 new: #gpu-utilization — rl-scale-out: "rlso: A5 readmit FAILED…" '
                   '(+2 more). Read with chat_read(inbox=True, unread_only=True, view="slim"), '
                   'follow next_cursor, and ack the receipt. Continue your task; automated CM notice, not Owner input.')
        event = {"source": "chat", "marker": "[cm-chat w]", "text": summary}
        self.assertEqual(channel_content(event), summary)
        legacy = dict(event, text="x" * (CHAT_CONTENT_MAX + 1) + " Monitor results: m-1. Continue the existing task.")
        self.assertEqual(channel_content(legacy),
                         "[cm-chat w] New CM chat activity; read your pending inbox. Monitor results: m-1.")
        old_daemon = dict(event, text="[cm-chat w] New chat activity. Before responding, read pending "
                          "messages with chat_read(inbox=true, unread_only=true). Continue the existing task.")
        self.assertEqual(channel_content(old_daemon),
                         "[cm-chat w] New CM chat activity; read your pending inbox.")
        monitor = {"source": "session_monitor", "marker": "[cm-monitor]", "text": "y" * 5000}
        self.assertEqual(channel_content(monitor), monitor["text"])


class SlimReadTests(unittest.TestCase):
    FULL = {
        "items": [{
            "id": "d:1", "created_at": "2026-10-06T06:00:00Z", "conversation_id": "c-gpu",
            "conversation_path": "#gpu-utilization", "conversation_kind": "channel",
            "actor": {"id": "agent:x", "name": "rl-scale-out", "kind": "agent"},
            "body": "rlso: A5 readmit FAILED", "data": {"reply_to": "d:0", "thread_root": "d:0",
                                                       "metadata_seen": {"big": "blob"}},
            "extensions": {}, "request": {"key": "k"}, "replication": {"x": 1}, "pin": None,
        }, {
            "id": "d:2", "created_at": "2026-10-06T06:01:00Z", "conversation_id": "dm-1",
            "conversation_kind": "dm", "actor": {"id": "agent:y", "name": "scout"},
            "body": "ping", "data": {},
        }],
        "context": [], "next_cursor": {"filter": "f", "last_id": "d:2"},
        "receipt": {"actor": "me", "ids": ["d:1", "d:2"]}, "position": {"p": 1},
        "norms": {"text": "long"}, "sync": {"x": 1}, "cache": {}, "coverage": "complete",
        "monitor_status": {"active": 1, "unacknowledged": 2, "badges": 2,
                           "recently_expired": [{"id": "w1"}]},
    }

    def test_projection_keeps_only_reader_fields_and_pagination(self):
        slim = server.slim_read_result(json.loads(json.dumps(self.FULL)))
        self.assertEqual(slim["items"][0], {
            "id": "d:1", "time": "2026-10-06T06:00:00Z", "conversation": "#gpu-utilization",
            "conversation_id": "c-gpu", "sender": "rl-scale-out", "body": "rlso: A5 readmit FAILED",
            "reply_to": "d:0", "thread": "d:0"})
        self.assertEqual(slim["items"][1]["conversation"], "dm:scout")
        self.assertNotIn("reply_to", slim["items"][1])
        for key in ("next_cursor", "receipt", "position", "coverage"):
            self.assertEqual(slim[key], self.FULL[key])
        for key in ("norms", "sync", "cache"):
            self.assertNotIn(key, slim)
        self.assertEqual(slim["monitor_status"], {"unacknowledged": 2, "recently_expired": [{"id": "w1"}]})
        self.assertEqual(slim["view"], "slim")

    def test_tool_strips_view_before_the_daemon_and_defaults_to_full(self):
        with mock.patch.object(control_client, "call", return_value=json.loads(json.dumps(self.FULL))) as call:
            full = server.chat_read(inbox=True, unread_only=True)
            self.assertIn("norms", full)
            slim = server.chat_read(inbox=True, unread_only=True, view="slim")
            self.assertNotIn("view", call.call_args.args[1])
            self.assertEqual(call.call_args.args[1], {"inbox": True, "unread_only": True,
                                                      "time_basis": "created", "freshness": "cached",
                                                      "limit": 50, "dms": False, "newest_first": False})
            self.assertEqual(slim["view"], "slim")
            with self.assertRaises(ValueError):
                server.chat_read(inbox=True, view="tiny")


if __name__ == "__main__":
    unittest.main()
