"""Tests for the self-notifying async monitor (S2): registration,
fire-message formatting, self-delivery + verification + retry, dedupe,
and the `send_input` auto-registration.

Scaffolding matches test_session_monitor: stub `control_client.call`
with scripted responses, back transcript reads with real temp JSONL
files. The delivery stub APPENDS the sent text to the caller's
transcript file — mimicking what a real injection does — so the
verification loop runs for real.
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest
from unittest import mock

from mcp_server import async_monitor, control_client


def _write_transcript(path: str, lines: list[dict]) -> None:
    with open(path, "w", encoding="utf-8") as f:
        for entry in lines:
            f.write(json.dumps(entry) + "\n")


def _assistant_end_turn(text: str) -> dict:
    return {
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": text}],
            "stop_reason": "end_turn",
        },
    }


def _user(text: str) -> dict:
    return {"type": "user", "message": {"content": text}}


class _MonitorEnv(unittest.TestCase):
    """Common scaffolding: temp transcripts for worker + caller, fast
    monitor tunables, CM_TUI_SESSION_ID, and a scripted control plane."""

    CALLER = "ts-caller"
    WORKER = "ts-worker"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.worker_path = os.path.join(self.tmp.name, "worker.jsonl")
        self.caller_path = os.path.join(self.tmp.name, "caller.jsonl")
        _write_transcript(self.worker_path, [
            _user("go"), _assistant_end_turn("WORKER DONE: 42"),
        ])
        _write_transcript(self.caller_path, [
            _user("orchestrate"), _assistant_end_turn("spawned"),
        ])
        self._orig_call = control_client.call
        self.sent: list[dict] = []
        self.deliver_on_send = True
        control_client.call = self._fake_call
        self._env = mock.patch.dict(
            os.environ, {"CM_TUI_SESSION_ID": self.CALLER}
        )
        self._env.start()
        self._patches = [
            mock.patch.object(async_monitor, "POLL_S", 0.05),
            mock.patch.object(async_monitor, "VERIFY_WINDOW_S", 0.5),
            mock.patch.object(async_monitor, "CALLER_IDLE_MAX_WAIT_S", 2.0),
        ]
        for p in self._patches:
            p.start()
        async_monitor._MONITORS.clear()

    def tearDown(self):
        for p in self._patches:
            p.stop()
        self._env.stop()
        control_client.call = self._orig_call
        async_monitor._MONITORS.clear()
        self.tmp.cleanup()

    def _fake_call(self, method, params=None, **kw):
        params = params or {}
        if method == "resolve_authorized_session":
            uid = params["session_uid"]
            path = self.caller_path if uid == self.CALLER else self.worker_path
            return {
                "state": "ready", "idle": True,
                "engine": "claude-code",
                "transcript_path": path, "generation": 0,
            }
        if method == "send_input":
            self.sent.append(dict(params))
            if self.deliver_on_send:
                with open(self.caller_path, "a", encoding="utf-8") as f:
                    f.write(json.dumps(_user(params["text"])) + "\n")
            return {"ok": True}
        raise AssertionError(f"unexpected method {method}")


class RegisterAndFireTests(_MonitorEnv):
    def test_fire_delivers_marker_and_reply_to_caller(self):
        async def scenario():
            reg = async_monitor.register_monitor(
                [self.WORKER], note="wave 1", source="explicit",
            )
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            await rec["task"]
            return reg, rec

        reg, rec = asyncio.run(scenario())
        self.assertEqual(rec["state"], "delivered")
        self.assertTrue(rec["delivered"])
        self.assertEqual(len(self.sent), 1)
        self.assertEqual(self.sent[0]["session_uid"], self.CALLER)
        text = self.sent[0]["text"]
        self.assertIn(f"[cm-monitor {reg['monitor_id']} fired]", text)
        self.assertIn("wave 1", text)
        self.assertIn("WORKER DONE: 42", text)
        self.assertIn(self.WORKER, text)
        # Result retained for list_monitors either way.
        listed = async_monitor.list_monitors()["monitors"]
        self.assertEqual(listed[0]["monitor_id"], reg["monitor_id"])
        self.assertEqual(
            listed[0]["result"]["completed"][0]["session_uid"], self.WORKER,
        )

    def test_failed_verification_retries_then_retains(self):
        self.deliver_on_send = False  # injection never lands

        async def scenario():
            reg = async_monitor.register_monitor(
                [self.WORKER], source="explicit",
            )
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            await rec["task"]
            return rec

        rec = asyncio.run(scenario())
        self.assertEqual(rec["state"], "undelivered")
        self.assertFalse(rec["delivered"])
        # Original attempt + MAX_REDELIVERIES retries.
        self.assertEqual(len(self.sent), 1 + async_monitor.MAX_REDELIVERIES)
        # The watch result itself is retained.
        self.assertEqual(
            rec["result"]["completed"][0]["session_uid"], self.WORKER,
        )

    def test_no_caller_identity_raises(self):
        async def scenario():
            with mock.patch.dict(os.environ, {"CM_TUI_SESSION_ID": ""}):
                async_monitor.register_monitor([self.WORKER])

        with self.assertRaises(async_monitor.RegistrationError) as ctx:
            asyncio.run(scenario())
        self.assertEqual(ctx.exception.code, "no_caller")

    def test_auto_source_replaces_same_target_watch(self):
        async def scenario():
            first = async_monitor.register_monitor(
                [self.WORKER], source="auto",
            )
            second = async_monitor.register_monitor(
                [self.WORKER], source="auto",
            )
            first_rec = async_monitor._MONITORS[first["monitor_id"]]
            second_rec = async_monitor._MONITORS[second["monitor_id"]]
            await asyncio.gather(
                first_rec["task"], second_rec["task"],
                return_exceptions=True,
            )
            return first_rec, second_rec

        first_rec, second_rec = asyncio.run(scenario())
        self.assertEqual(first_rec["state"], "replaced")
        self.assertEqual(second_rec["state"], "delivered")
        # Only the replacement fired a delivery.
        self.assertEqual(len(self.sent), 1)

    def test_cancel_monitor(self):
        async def scenario():
            # Worker that never completes: mid-turn transcript + busy PTY.
            _write_transcript(self.worker_path, [_user("go")])

            def busy_call(method, params=None, **kw):
                if method == "resolve_authorized_session":
                    return {
                        "state": "ready", "idle": False,
                        "engine": "claude-code",
                        "transcript_path": self.worker_path,
                        "generation": 0,
                    }
                raise AssertionError(method)

            control_client.call = busy_call
            reg = async_monitor.register_monitor([self.WORKER])
            out = async_monitor.cancel_monitor(reg["monitor_id"])
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            await asyncio.gather(rec["task"], return_exceptions=True)
            return out, rec

        out, rec = asyncio.run(scenario())
        self.assertEqual(out["state"], "cancelled")
        self.assertEqual(rec["state"], "cancelled")
        self.assertEqual(self.sent, [])

    def test_unknown_cancel_is_not_found(self):
        self.assertEqual(
            async_monitor.cancel_monitor("mon-zzz")["error"], "not_found",
        )


class InboxDeliveryTests(_MonitorEnv):
    """S3: a mid-turn caller gets its notification via the Stop-hook
    inbox (file consumed at the turn boundary) instead of PTY typing;
    a hook-less caller that reaches its prompt gets the message taken
    back and PTY-injected."""

    def setUp(self):
        super().setUp()
        self._inbox_patch = mock.patch.object(
            async_monitor, "INBOX_ROOT",
            os.path.join(self.tmp.name, "inbox"),
        )
        self._inbox_patch.start()

    def tearDown(self):
        self._inbox_patch.stop()
        super().tearDown()

    def _record(self):
        return {"monitor_id": "mon-test", "caller": self.CALLER}

    def test_hook_consumption_counts_as_delivery(self):
        async def scenario():
            async def busy(_caller):
                return False, None

            with mock.patch.object(async_monitor, "_caller_at_prompt", busy):
                task = asyncio.get_running_loop().create_task(
                    async_monitor._deliver_to_caller(
                        self._record(), "[cm-monitor mon-test fired] hi",
                    )
                )
                inbox = os.path.join(async_monitor.INBOX_ROOT, self.CALLER)
                # Wait for the message to land in the inbox...
                for _ in range(100):
                    files = os.listdir(inbox) if os.path.isdir(inbox) else []
                    if files:
                        break
                    await asyncio.sleep(0.02)
                self.assertTrue(files, "inbox message written for busy caller")
                payload = json.load(
                    open(os.path.join(inbox, files[0]), encoding="utf-8")
                )
                self.assertIn("[cm-monitor mon-test", payload["text"])
                # ...then consume it the way the Stop hook does.
                os.remove(os.path.join(inbox, files[0]))
                return await task

        delivered = asyncio.run(scenario())
        self.assertTrue(delivered)
        self.assertEqual(
            self.sent, [], "hook delivery must not touch the PTY",
        )

    def test_prompt_reached_unconsumed_falls_back_to_pty(self):
        async def scenario():
            calls = {"n": 0}

            async def busy_then_prompt(_caller):
                calls["n"] += 1
                if calls["n"] == 1:
                    return False, None
                return True, {
                    "state": "ready", "idle": True,
                    "engine": "claude-code",
                    "transcript_path": self.caller_path, "generation": 0,
                }

            with mock.patch.object(
                async_monitor, "_caller_at_prompt", busy_then_prompt,
            ):
                return await async_monitor._deliver_to_caller(
                    self._record(), "[cm-monitor mon-test fired] hi",
                )

        delivered = asyncio.run(scenario())
        self.assertTrue(delivered, "PTY fallback delivers + verifies")
        # The message went through send_input (PTY), not the inbox.
        self.assertEqual(len(self.sent), 1)
        self.assertEqual(self.sent[0]["session_uid"], self.CALLER)
        # Taken back: nothing left in the inbox.
        inbox = os.path.join(async_monitor.INBOX_ROOT, self.CALLER)
        leftover = os.listdir(inbox) if os.path.isdir(inbox) else []
        self.assertEqual(leftover, [])


class SendInputAutoRegisterTests(_MonitorEnv):
    def test_send_input_registers_monitor_by_default(self):
        from mcp_server.server import send_input

        async def scenario():
            res = await send_input(self.WORKER, "do the thing")
            mon = res.get("monitor") or {}
            rec = async_monitor._MONITORS.get(mon.get("monitor_id"))
            if rec is not None:
                await asyncio.gather(rec["task"], return_exceptions=True)
            return res

        res = asyncio.run(scenario())
        self.assertIn("monitor_id", res["monitor"])
        self.assertIn("async_note", res["monitor"])
        # First send is the prompt itself; the monitor then fired and
        # delivered its notification as a second send (worker transcript
        # already reads end_turn in this scaffold).
        self.assertGreaterEqual(len(self.sent), 1)
        self.assertEqual(self.sent[0]["session_uid"], self.WORKER)

    def test_send_input_notify_opt_out(self):
        from mcp_server.server import send_input

        async def scenario():
            return await send_input(
                self.WORKER, "fire and forget", notify_on_done=False,
            )

        res = asyncio.run(scenario())
        self.assertNotIn("monitor", res)
        self.assertEqual(async_monitor.list_monitors()["monitors"], [])


if __name__ == "__main__":
    unittest.main()
