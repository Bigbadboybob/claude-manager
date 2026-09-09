import asyncio
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
from contextlib import suppress
from pathlib import Path
from unittest import mock

from mcp_server.native_claude import ClaudeSocket
from mcp_server.native_codex import Relay, configure_external_editor
from mcp_server.notifications import (
    NotSubmitted,
    Queue,
    atomic_write,
    consume,
    consume_supervised,
    transcript_observed,
)


async def eventually(predicate, timeout=3):
    end = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= end:
            raise AssertionError("condition did not become true")
        await asyncio.sleep(0.01)


class Adapter:
    engine = "fixture"

    def __init__(self):
        self.sent = []
        self.seen = set()
        self.block = None
        self.available = True

    def identity(self):
        return {"thread_id": "fixture-thread"}

    async def send(self, event):
        if not self.available:
            raise NotSubmitted()
        self.sent.append(event)
        if self.block:
            await self.block.wait()
        return {"kind": "fixture_ack"}

    async def observed(self, event):
        return event["id"] in self.seen


class ExternalEditorTests(unittest.TestCase):
    def test_headless_launch_uses_available_neovim(self):
        with mock.patch.dict(os.environ, {"VISUAL": " ", "EDITOR": ""}, clear=True), mock.patch(
            "mcp_server.native_codex.shutil.which", return_value="/usr/local/bin/nvim"
        ) as which:
            configure_external_editor()
            self.assertEqual(os.environ["VISUAL"], "/usr/local/bin/nvim")
            self.assertEqual(os.environ["EDITOR"], "/usr/local/bin/nvim")
            which.assert_called_once_with("nvim")

    def test_explicit_editor_or_visual_is_preserved_including_arguments(self):
        for env in ({"EDITOR": "vim -f"}, {"VISUAL": "code --wait"},
                    {"VISUAL": "nvim", "EDITOR": "vi"}):
            with self.subTest(env=env), mock.patch.dict(os.environ, env, clear=True), mock.patch(
                "mcp_server.native_codex.shutil.which"
            ) as which:
                configure_external_editor()
                self.assertEqual(dict(os.environ), env)
                which.assert_not_called()

    def test_fallback_requires_an_installed_editor(self):
        for available in ("vim", "vi", None):
            with self.subTest(available=available), mock.patch.dict(os.environ, {}, clear=True), mock.patch(
                "mcp_server.native_codex.shutil.which",
                side_effect=lambda name: f"/usr/bin/{name}" if name == available else None,
            ):
                configure_external_editor()
                expected = {} if available is None else {
                    "EDITOR": f"/usr/bin/{available}", "VISUAL": f"/usr/bin/{available}"
                }
                self.assertEqual(dict(os.environ), expected)


class QueueTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.queue = Queue("session-a", Path(self.tmp.name))
        self.tasks = []

    async def asyncTearDown(self):
        for task in self.tasks:
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)
        self.tmp.cleanup()

    def start(self, adapter):
        task = asyncio.create_task(consume(self.queue, adapter))
        self.tasks.append(task)
        return task

    def post(self, event="one"):
        return self.queue.publish(
            event, "fixture", f"[fixture {event}] CM event", f"[fixture {event}]"
        )

    async def test_commit_wakes_consumer_and_receipt_is_separate_from_submission(self):
        adapter = Adapter()
        self.start(adapter)
        await asyncio.sleep(0.05)
        start = time.monotonic()
        self.post()
        await eventually(lambda: self.queue.get("one")["status"] == "submitted")
        self.assertLess(time.monotonic() - start, 0.5)
        self.assertEqual(len(adapter.sent), 1)
        adapter.seen.add("one")
        await eventually(lambda: self.queue.get("one")["status"] == "observed")
        self.post()  # producer replay remains idempotent
        self.assertEqual(self.queue.get("one")["status"], "observed")

    async def test_ambiguous_disconnect_is_retained_and_not_replayed_after_restart(
        self,
    ):
        adapter = Adapter()
        adapter.block = asyncio.Event()
        task = self.start(adapter)
        self.post()
        await eventually(lambda: len(adapter.sent) == 1)
        task.cancel()
        with suppress(asyncio.CancelledError):
            await task
        self.assertEqual(self.queue.get("one")["status"], "uncertain")
        replacement = Adapter()
        self.start(replacement)
        await asyncio.sleep(0.1)
        self.assertEqual(replacement.sent, [])
        replacement.seen.add("one")
        await eventually(lambda: self.queue.get("one")["status"] == "observed")

    async def test_process_crash_after_claim_is_uncertain(self):
        self.post()
        self.queue.change("one", {"pending"}, status="submitting")
        adapter = Adapter()
        self.start(adapter)
        await eventually(lambda: self.queue.get("one")["status"] == "uncertain")
        self.assertEqual(adapter.sent, [])

    async def test_cancel_before_claim_retracts_but_after_claim_cannot(self):
        self.post("cancelled")
        self.queue.cancel("cancelled")
        self.post("claimed")
        adapter = Adapter()
        adapter.block = asyncio.Event()
        self.start(adapter)
        await eventually(lambda: len(adapter.sent) == 1)
        self.assertEqual(adapter.sent[0]["id"], "claimed")
        self.assertEqual(self.queue.cancel("claimed")["status"], "submitting")
        adapter.block.set()
        await eventually(lambda: self.queue.get("claimed")["status"] == "submitted")

    async def test_two_mcp_consumers_cannot_duplicate_delivery(self):
        a, b = Adapter(), Adapter()
        first = self.start(a)
        self.start(b)
        self.post()
        await eventually(lambda: len(a.sent) + len(b.sent) == 1)
        first.cancel()
        await asyncio.gather(first, return_exceptions=True)
        await asyncio.sleep(0.4)
        self.assertEqual(len(a.sent) + len(b.sent), 1)

    async def test_unavailable_native_transport_keeps_pending_without_attempt(self):
        adapter = Adapter()
        adapter.available = False
        self.start(adapter)
        self.post()
        await asyncio.sleep(0.1)
        self.assertEqual(self.queue.get("one")["status"], "pending")
        self.assertEqual(adapter.sent, [])
        adapter.available = True
        await eventually(lambda: self.queue.get("one")["status"] == "submitted")

    async def test_new_wake_precedes_verification_of_older_submissions(self):
        self.post("old")
        self.queue.change("old", {"pending"}, status="submitted")
        self.post("fresh")
        adapter = Adapter()

        async def observed(_event):
            self.assertEqual(adapter.sent[0]["id"], "fresh")
            return False

        adapter.observed = observed
        self.start(adapter)
        await eventually(lambda: self.queue.get("fresh")["status"] == "submitted")

    async def test_other_session_and_unknown_envelopes_are_not_delivered(self):
        self.post()
        event = self.queue.get("one")
        event["recipient"] = "someone-else"
        atomic_write(self.queue.event_path("one"), event)
        self.post("future")
        event = self.queue.get("future")
        event["version"] = 2
        atomic_write(self.queue.event_path("future"), event)
        adapter = Adapter()
        self.start(adapter)
        await asyncio.sleep(0.1)
        self.assertEqual(adapter.sent, [])
        self.assertIsNone(Queue("someone-else", Path(self.tmp.name)).get("one"))

    async def test_queue_bounds_and_conflicting_retry(self):
        self.post()
        with self.assertRaises(ValueError):
            self.queue.publish("one", "fixture", "different", "marker")
        with mock.patch("mcp_server.notifications.MAX_EVENTS", 1):
            with self.assertRaises(RuntimeError):
                self.post("two")
            self.queue.cancel("one")
            self.post("two")
            self.assertIsNone(self.queue.get("one"))
        with self.assertRaises(ValueError):
            self.queue.publish("long", "fixture", "a" * 65537, "marker")

    async def test_failed_publication_sync_is_not_visible_to_consumer(self):
        calls = 0
        original = os.fsync

        def fsync(fd):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("fixture directory sync failure")
            original(fd)

        with mock.patch("os.fsync", fsync), self.assertRaises(OSError):
            self.post()
        self.assertIsNone(self.queue.get("one"))

    async def test_corrupt_queue_disables_wakes_and_recovers_without_stopping_owner(
        self,
    ):
        self.post()
        path = self.queue.event_path("one")
        original = path.read_text()
        path.write_text("broken fixture JSON")
        adapter = Adapter()
        task = asyncio.create_task(
            consume_supervised(self.queue, adapter, retry_s=0.05)
        )
        self.tasks.append(task)
        await asyncio.sleep(0.1)
        self.assertFalse(task.done())
        self.assertEqual(adapter.sent, [])
        atomic_write(path, json.loads(original))
        await eventually(lambda: self.queue.get("one")["status"] == "submitted")

    async def test_claude_socket_uses_auth_and_peer_frame_without_claiming_ack(self):
        socket = str(Path(self.tmp.name) / "native.sock")
        frames = []

        async def receive(reader, writer):
            while line := await reader.readline():
                frames.append(json.loads(line))
            writer.close()
            await writer.wait_closed()

        async with await asyncio.start_unix_server(receive, socket):
            with mock.patch.dict(
                "os.environ",
                {
                    "CLAUDE_CODE_MESSAGING_SOCKET": "uds:" + socket,
                    "CLAUDE_CODE_MESSAGING_TOKEN": "fixture-token",
                },
            ):
                adapter = ClaudeSocket("session-a")
                receipt = await adapter.send(self.post())
                await eventually(lambda: len(frames) == 2)
        self.assertEqual(frames[0], {"type": "auth", "token": "fixture-token"})
        self.assertEqual(frames[1]["from"], "claude-manager")
        self.assertEqual(receipt["kind"], "socket_write_only")
        self.assertNotIn("token", json.dumps(adapter.identity()))


class ReceiptTests(unittest.TestCase):
    def test_only_real_inbound_records_confirm_receipt(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "rollout.jsonl"
            path.write_text(
                json.dumps(
                    {
                        "type": "assistant",
                        "message": {"role": "assistant", "content": "[fixture]"},
                    }
                )
                + "\n"
            )
            self.assertFalse(transcript_observed(str(path), "claude-code", "[fixture]"))
            path.write_text(
                json.dumps(
                    {
                        "type": "user",
                        "origin": {
                            "kind": "peer",
                            "from": "claude-manager",
                            "selfSent": True,
                        },
                        "message": {"role": "user", "content": "[fixture]"},
                    }
                )
                + "\n"
            )
            self.assertTrue(transcript_observed(str(path), "claude-code", "[fixture]"))
            attachment = {
                "type": "attachment",
                "attachment": {
                    "type": "queued_command",
                    "prompt": "[fixture]",
                    "origin": {
                        "kind": "peer",
                        "from": "claude-manager",
                        "verifiedPeerPid": 42,
                        "verifiedPeerProcStart": "7",
                    },
                },
            }
            path.write_text(json.dumps(attachment) + "\n")
            self.assertFalse(
                transcript_observed(
                    str(path), "claude-code", "[fixture]", {"peer_pid": 43}
                )
            )
            self.assertTrue(
                transcript_observed(
                    str(path),
                    "claude-code",
                    "[fixture]",
                    {"peer_pid": 42, "peer_start": "7"},
                )
            )
            path.write_text(
                json.dumps(
                    {
                        "type": "response_item",
                        "payload": {
                            "type": "function_call_output",
                            "name": "cm_notification",
                            "output": "[fixture]",
                        },
                    }
                )
                + "\n"
            )
            self.assertTrue(transcript_observed(str(path), "codex", "[fixture]"))


class CodexBindingTests(unittest.IsolatedAsyncioTestCase):
    async def test_receipt_stays_with_original_thread_when_frontend_switches_during_rpc(
        self,
    ):
        with tempfile.TemporaryDirectory() as tmp:
            relay = Relay("unused.sock", Queue("fixture", Path(tmp)))
            relay.thread = {"id": "original", "path": "/original.jsonl"}
            relay.ready.set()

            async def switch_during_call(method, params):
                self.assertEqual(params["threadId"], "original")
                relay.thread = {"id": "new", "path": "/new.jsonl"}
                return {"turn": {"id": "original-turn"}}

            relay.call = switch_during_call
            receipt = await relay.send(
                {"binding": {"thread_id": "original"}, "text": "fixture"}
            )
            self.assertEqual(receipt["transcript_path"], "/original.jsonl")
            with self.assertRaises(NotSubmitted):
                await relay.send(
                    {"binding": {"thread_id": "original"}, "text": "fixture"}
                )


@unittest.skipUnless(sys.platform == "linux", "Linux process-group lifecycle")
class ProcessGuardTests(unittest.TestCase):
    def test_owner_pipe_eof_reaps_backend_and_its_tool(self):
        guard_path = Path(__file__).resolve().parents[1] / "native_process.py"
        code = (
            "import subprocess,sys,time,json,os; "
            "p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)']); "
            "print(json.dumps([os.getpid(),p.pid]),flush=True); time.sleep(60)"
        )
        guard = subprocess.Popen(
            [sys.executable, str(guard_path), sys.executable, "-c", code],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            import select

            self.assertTrue(select.select([guard.stdout], [], [], 5)[0])
            pids = json.loads(guard.stdout.readline())
            guard.stdin.close()  # also happens when the launcher is SIGKILLed
            guard.wait(timeout=6)
            for pid in pids:
                stat = Path(f"/proc/{pid}/stat")
                if stat.exists():
                    self.assertEqual(
                        stat.read_text().rsplit(") ", 1)[1].split()[0], "Z"
                    )
        finally:
            if guard.poll() is None:
                guard.terminate()
                guard.wait(timeout=6)
            guard.stdout.close()
