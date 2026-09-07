"""Tests for the self-notifying async monitor (S2): registration,
fire-message formatting, durable native delivery, cancellation, dedupe,
and the `send_input` auto-registration.

Scaffolding matches test_session_monitor: stub `control_client.call`
with scripted responses, back transcript reads with real temp JSONL
files. Native publication is recorded separately from explicit worker prompts.
Queue/adapter receipt and crash tests live in test_native_notifications.
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest
from unittest import mock

from mcp_server import async_monitor, control_client, notifications
from pathlib import Path


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
        self.rotate_on_deliver = False
        control_client.call = self._fake_call
        self._env = mock.patch.dict(
            os.environ, {"CM_TUI_SESSION_ID": self.CALLER, "HOME": self.tmp.name}
        )
        self._env.start()
        original_publish = notifications.Queue.publish
        def publish(queue, event_id, source, text, marker):
            result = original_publish(queue, event_id, source, text, marker)
            self.sent.append({"session_uid": queue.uid, "text": text, "transport": "native"})
            if self.deliver_on_send:
                queue.change(event_id, {"pending"}, status="observed")
            return result
        self._patches = [
            mock.patch.object(notifications.Queue, "publish", publish),
            mock.patch.object(async_monitor, "VERIFY_WINDOW_S", 0.5),
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
                if (
                    self.rotate_on_deliver
                    and params["session_uid"] == self.CALLER
                ):
                    # Simulate the caller being resumed mid-delivery:
                    # subsequent resolves see a NEW transcript file, and
                    # the daemon delivers into that live file.
                    rotated = os.path.join(
                        self.tmp.name, f"caller-rot{len(self.sent)}.jsonl"
                    )
                    _write_transcript(rotated, [
                        _user("resumed"), _assistant_end_turn("resumed"),
                    ])
                    self.caller_path = rotated
                with open(self.caller_path, "a", encoding="utf-8") as f:
                    f.write(json.dumps(_user(params["text"])) + "\n")
            return {"ok": True}
        raise AssertionError(f"unexpected method {method}")

    def _append_worker_turn(self, text: str) -> None:
        with open(self.worker_path, "a", encoding="utf-8") as f:
            f.write(json.dumps(_user("again")) + "\n")
            f.write(json.dumps(_assistant_end_turn(text)) + "\n")


class RegisterAndFireTests(_MonitorEnv):
    def test_fire_delivers_marker_and_reply_to_caller(self):
        async def scenario():
            # edge=False: the scaffold's worker is ALREADY finished, and
            # this test wants the level-triggered immediate fire.
            reg = async_monitor.register_monitor(
                [self.WORKER], note="wave 1", source="explicit", edge=False,
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

    def test_failed_verification_retains_without_replay(self):
        self.deliver_on_send = False  # injection never lands

        async def scenario():
            reg = async_monitor.register_monitor(
                [self.WORKER], source="explicit", edge=False,
            )
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            await rec["task"]
            return rec

        rec = asyncio.run(scenario())
        self.assertEqual(rec["state"], "undelivered")
        self.assertFalse(rec["delivered"])
        # Native submission is never repeated on ambiguous receipt.
        self.assertEqual(len(self.sent), 1)
        self.assertEqual(rec["delivery_status"], "pending")
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

    def test_late_native_receipt_updates_retained_monitor_without_resending(self):
        self.deliver_on_send = False
        async def scenario():
            reg = async_monitor.register_monitor([self.WORKER], edge=False)
            record = async_monitor._MONITORS[reg["monitor_id"]]
            await record["task"]
            return record
        record = asyncio.run(scenario())
        self.assertEqual(record["state"], "undelivered")
        notifications.Queue(self.CALLER).change(record["notification_id"], {"pending"}, status="observed")
        listed = async_monitor.list_monitors()["monitors"][0]
        self.assertEqual(listed["state"], "delivered")
        self.assertTrue(listed["delivered"])
        self.assertEqual(len(self.sent), 1)

    def test_auto_source_replaces_same_target_watch(self):
        async def scenario():
            first = async_monitor.register_monitor(
                [self.WORKER], source="auto", edge=False,
            )
            second = async_monitor.register_monitor(
                [self.WORKER], source="auto", edge=False,
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


class SendInputAutoRegisterTests(_MonitorEnv):
    def test_send_input_monitor_fires_on_new_turn_not_stale_reply(self):
        """The auto-registered monitor is edge-triggered: the worker's
        PREVIOUS reply (still on disk, worker still at its prompt while
        the prompt delivery is in flight) must not instant-fire; the
        monitor fires with the turn the prompt actually starts."""
        from mcp_server.server import send_input

        async def scenario():
            res = await send_input(self.WORKER, "do the thing")
            mon = res.get("monitor") or {}
            rec = async_monitor._MONITORS.get(mon.get("monitor_id"))
            self.assertIsNotNone(rec)
            # Give the watch a moment: it must NOT fire on the stale
            # "WORKER DONE: 42" turn that predates the prompt.
            await asyncio.sleep(0.3)
            self.assertEqual(rec["state"], "watching")
            # No delivery yet — the only send is the prompt itself.
            self.assertEqual(len(self.sent), 1)
            # The worker now completes the prompted turn.
            self._append_worker_turn("WORKER DONE: ROUND 2")
            await asyncio.gather(rec["task"], return_exceptions=True)
            return res, rec

        res, rec = asyncio.run(scenario())
        self.assertIn("monitor_id", res["monitor"])
        self.assertIn("async_note", res["monitor"])
        # Auto registrations don't nag about the momentarily-idle worker.
        self.assertEqual(res["monitor"]["already_idle"], [])
        self.assertEqual(self.sent[0]["session_uid"], self.WORKER)
        self.assertEqual(rec["state"], "delivered")
        fire = self.sent[-1]["text"]
        self.assertIn("ROUND 2", fire)
        self.assertNotIn("WORKER DONE: 42", fire)

    def test_send_input_notify_opt_out(self):
        from mcp_server.server import send_input

        async def scenario():
            return await send_input(
                self.WORKER, "fire and forget", notify_on_done=False,
            )

        res = asyncio.run(scenario())
        self.assertNotIn("monitor", res)
        self.assertEqual(async_monitor.list_monitors()["monitors"], [])


class EdgeTriggerTests(_MonitorEnv):
    """Regression: a monitor armed on an ALREADY-idle session must not
    instant-fire its stale last message (the notification-spam loop)."""

    def test_register_on_idle_arms_quietly_then_fires_on_new_turn(self):
        async def scenario():
            reg = async_monitor.register_monitor(
                [self.WORKER], source="explicit",
            )
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            await asyncio.sleep(0.3)
            mid_state = rec["state"]
            mid_sent = len(self.sent)
            self._append_worker_turn("ROUND 2")
            await rec["task"]
            return reg, rec, mid_state, mid_sent

        reg, rec, mid_state, mid_sent = asyncio.run(scenario())
        # Registration reports the already-idle target inline...
        self.assertEqual(reg["already_idle"], [self.WORKER])
        self.assertIn("read_last_turn", reg["async_note"])
        # ...and does NOT instant-fire the stale reply.
        self.assertEqual(mid_state, "watching")
        self.assertEqual(mid_sent, 0)
        # The NEXT completed turn fires with the fresh reply.
        self.assertEqual(rec["state"], "delivered")
        fire = self.sent[-1]["text"]
        self.assertIn("ROUND 2", fire)
        self.assertNotIn("WORKER DONE: 42", fire)


class CancelTests(_MonitorEnv):
    """Regression: cancel must be terminal from ANY state — no
    resurrection to "undelivered", no post-cancel ghost deliveries."""

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

    def test_cancel_mid_delivery_is_terminal_and_retracts_native_event(self):
        self.deliver_on_send = False
        async def scenario():
            reg = async_monitor.register_monitor([self.WORKER], edge=False)
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            queue = notifications.Queue(self.CALLER)
            event_id = f"monitor:{reg['monitor_id']}"
            for _ in range(100):
                if queue.get(event_id):
                    break
                await asyncio.sleep(.01)
            self.assertEqual(rec["state"], "fired")
            out = async_monitor.cancel_monitor(reg["monitor_id"])
            await asyncio.gather(rec["task"], return_exceptions=True)
            return out, rec, queue.get(event_id)
        out, rec, event = asyncio.run(scenario())
        self.assertEqual(out["state"], "cancelled")
        self.assertEqual(rec["state"], "cancelled")
        self.assertEqual(event["status"], "cancelled")
        self.assertTrue(rec["cancellation_retracted"])

    def test_cancel_all_cancels_every_live_monitor(self):
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
            a = async_monitor.register_monitor([self.WORKER])
            b = async_monitor.register_monitor([self.WORKER], note="x")
            out = async_monitor.cancel_monitor("all")
            recs = [
                async_monitor._MONITORS[a["monitor_id"]],
                async_monitor._MONITORS[b["monitor_id"]],
            ]
            await asyncio.gather(
                *(r["task"] for r in recs), return_exceptions=True,
            )
            return out, recs

        out, recs = asyncio.run(scenario())
        self.assertEqual(out["count"], 2)
        self.assertEqual(len(out["cancelled"]), 2)
        for rec in recs:
            self.assertEqual(rec["state"], "cancelled")
        self.assertEqual(self.sent, [])


class FingerprintTests(unittest.TestCase):
    """The edge baseline: last_completed_turn_fingerprint."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "t.jsonl")

    def tearDown(self):
        self.tmp.cleanup()

    def test_changes_only_on_new_completed_turn(self):
        from mcp_server.monitor import last_completed_turn_fingerprint as fp

        _write_transcript(self.path, [_user("a"), _assistant_end_turn("one")])
        base = fp("claude-code", self.path)
        self.assertIsNotNone(base)
        self.assertEqual(fp("claude-code", self.path), base)
        # Trailing user line (mid-turn): still the PREVIOUS turn's id.
        with open(self.path, "a", encoding="utf-8") as f:
            f.write(json.dumps(_user("b")) + "\n")
        self.assertEqual(fp("claude-code", self.path), base)
        # A new completed turn changes it.
        with open(self.path, "a", encoding="utf-8") as f:
            f.write(json.dumps(_assistant_end_turn("two")) + "\n")
        self.assertNotEqual(fp("claude-code", self.path), base)

    def test_none_for_codex_missing_or_turnless(self):
        from mcp_server.monitor import last_completed_turn_fingerprint as fp

        self.assertIsNone(fp("codex", self.path))
        self.assertIsNone(fp("claude-code", None))
        self.assertIsNone(fp("claude-code", self.path))  # no such file
        _write_transcript(self.path, [_user("a")])
        self.assertIsNone(fp("claude-code", self.path))

    def test_prefers_uuid_when_present(self):
        from mcp_server.monitor import last_completed_turn_fingerprint as fp

        entry = _assistant_end_turn("x")
        entry["uuid"] = "u-123"
        _write_transcript(self.path, [entry])
        self.assertEqual(fp("claude-code", self.path), "u-123")


class FireMessageFormatTests(unittest.TestCase):
    """UX 3b/3c: the wake-up text must say HOW a session finished. A
    killed session must never have its transcript tail rendered in the
    `- <uid> (<status>): <content>` slot, where it reads as a final
    report; interim-looking fires must be flagged as interim."""

    RECORD = {"monitor_id": "m-1", "note": ""}

    def _format(self, entry: dict, still: list[str] | None = None) -> str:
        return async_monitor._format_fire_message(
            dict(self.RECORD),
            {"completed": [entry], "still_running": still or []},
        )

    def test_killed_entry_labels_killer_and_fragment(self):
        text = self._format({
            "session_uid": "ts-w", "status": "exited", "state": "exited",
            "idle": True, "killed": True, "killed_by": "ts-boss",
            "exited_at": 1_700_000_000.0,
            "last_message": {"content": "I was in the middle of"},
        })
        self.assertIn(
            "- ts-w (killed by ts-boss at 2023-11-14T22:13:20Z)", text
        )
        self.assertIn(
            "last transcript fragment before kill: I was in the middle of",
            text,
        )
        # The fragment must NOT sit in the "final reply" slot.
        self.assertNotIn("- ts-w (exited): I was in the middle of", text)

    def test_killed_without_killer_or_timestamp_still_says_killed(self):
        text = self._format({
            "session_uid": "ts-w", "status": "exited", "state": "exited",
            "idle": True, "killed": True, "last_message": None,
        })
        self.assertIn("- ts-w (killed)", text)

    def test_transcript_idle_flags_live_pty(self):
        text = self._format({
            "session_uid": "ts-w", "status": "awaiting_input",
            "state": "ready", "idle": False, "idle_source": "transcript",
            "last_message": {"content": "done-ish"},
        })
        self.assertIn("- ts-w (awaiting_input): done-ish", text)
        self.assertIn("PTY still active", text)

    def test_idle_but_alive_flags_possible_interim_turn(self):
        text = self._format({
            "session_uid": "ts-w", "status": "awaiting_input",
            "state": "ready", "idle": True,
            "last_message": {"content": "here's a thought"},
        })
        self.assertIn("no explicit done-report", text)

    def test_all_exited_batch_does_not_claim_workers_await_input(self):
        text = self._format({
            "session_uid": "ts-w", "status": "exited", "state": "exited",
            "idle": True, "killed": True, "killed_by": "operator",
            "last_message": None,
        })
        self.assertIn("have EXITED", text)
        self.assertNotIn("are now awaiting input", text)

    def test_live_completer_keeps_the_orchestration_trailer(self):
        text = self._format({
            "session_uid": "ts-w", "status": "awaiting_input",
            "state": "ready", "idle": True, "last_message": None,
        })
        self.assertIn("awaiting input", text)

    def test_exited_completer_never_declares_live_siblings_gone(self):
        # mode="any" returns on the FIRST completion. When that completer
        # is a corpse but other watched sessions are still running, the
        # trailer must not tell the orchestrator everything has EXITED —
        # it would abandon workers listed as running one line above.
        text = self._format({
            "session_uid": "ts-a", "status": "exited", "state": "exited",
            "idle": True, "killed": True, "killed_by": "operator",
            "last_message": None,
        }, still=["ts-b", "ts-c"])
        self.assertIn("Still running: ts-b, ts-c", text)
        self.assertNotIn("watched session(s) have EXITED", text)
        self.assertNotIn("start a fresh session", text)
        # …and it must not claim the corpse is awaiting input either.
        self.assertNotIn("are now awaiting input", text)
        self.assertIn("NO LONGER WATCHED", text)

    def test_live_completer_with_siblings_keeps_orchestration_trailer(self):
        text = self._format({
            "session_uid": "ts-a", "status": "awaiting_input",
            "state": "ready", "idle": True, "last_message": None,
        }, still=["ts-b"])
        self.assertIn("Still running: ts-b", text)
        self.assertIn("are now awaiting input", text)
        self.assertNotIn("EXITED", text)

    def test_plain_exit_is_not_flagged_interim_or_killed(self):
        text = self._format({
            "session_uid": "ts-w", "status": "exited", "state": "exited",
            "idle": True, "last_message": {"content": "all finished"},
        })
        self.assertIn("- ts-w (exited): all finished", text)
        self.assertNotIn("killed", text)
        self.assertNotIn("no explicit done-report", text)

    def test_timeout_with_no_completers_announces_nobody_finished(self):
        # Observed live (mon-7a1643): timed_out with an EMPTY completed
        # list. Every trailer below is written about completers, so the
        # generic one used to fire over zero of them — telling the
        # orchestrator that "the completed worker(s) are now awaiting
        # input" when nothing had finished at all.
        text = async_monitor._format_fire_message(
            dict(self.RECORD),
            {"completed": [], "still_running": ["ts-b"], "timed_out": True},
        )
        self.assertIn("TIMED OUT", text)
        self.assertIn("Still running: ts-b", text)
        self.assertIn("NOTHING finished inside the watch budget", text)
        self.assertNotIn("awaiting input", text)
        self.assertNotIn("EXITED", text)
        # The recovery cue: those sessions are live but unwatched now.
        self.assertIn("NO LONGER WATCHED", text)
        self.assertIn("monitor_sessions", text)

    def test_timeout_with_nothing_left_at_all_says_so(self):
        text = async_monitor._format_fire_message(
            dict(self.RECORD),
            {"completed": [], "still_running": [], "timed_out": True},
        )
        self.assertIn("no session is still being watched", text)
        self.assertNotIn("awaiting input", text)
        self.assertNotIn("NO LONGER WATCHED", text)


if __name__ == "__main__":
    unittest.main()
