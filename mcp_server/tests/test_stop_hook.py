"""Tests for the cm Stop hook script (S3): inbox draining →
block+reason emission, empty-inbox silence, and fail-open behavior.
The hook is exercised the way Claude Code runs it — as a subprocess
with the hook input on stdin — with HOME pointed at a temp dir (the
inbox root derives from it) and the daemon socket pointed at nowhere
(the turn-end report must fail open, silently)."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from pathlib import Path

from mcp_server import control_client
from mcp_server.hooks import cm_stop_hook, cm_state_hook

HOOK = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "hooks",
    "cm_stop_hook.py",
)

STOP_INPUT = json.dumps(
    {
        "hook_event_name": "Stop",
        "session_id": "b74d8490",
        "stop_hook_active": False,
    }
)


def _run_hook(home: str, uid: str | None) -> subprocess.CompletedProcess:
    env = dict(os.environ)
    env["HOME"] = home
    env["CM_DAEMON_SOCKET"] = os.path.join(home, "no-such-daemon.sock")
    env["CM_TUI_SOCKET"] = os.path.join(home, "no-such-tui.sock")
    if uid is None:
        env.pop("CM_TUI_SESSION_ID", None)
    else:
        env["CM_TUI_SESSION_ID"] = uid
    return subprocess.run(
        [sys.executable, HOOK],
        input=STOP_INPUT,
        capture_output=True,
        text=True,
        env=env,
        timeout=30,
    )


class StopHookTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def _inbox(self, uid: str) -> str:
        path = os.path.join(self.home, ".cm", "inbox", uid)
        os.makedirs(path, exist_ok=True)
        return path

    def test_empty_inbox_allows_stop_silently(self):
        self._inbox("ts-1")
        res = _run_hook(self.home, "ts-1")
        self.assertEqual(res.returncode, 0)
        self.assertEqual(res.stdout.strip(), "")

    def test_missing_inbox_dir_allows_stop(self):
        res = _run_hook(self.home, "ts-nobody")
        self.assertEqual(res.returncode, 0)
        self.assertEqual(res.stdout.strip(), "")

    def test_pending_messages_block_with_joined_reason(self):
        inbox = self._inbox("ts-1")
        with open(os.path.join(inbox, "001-mon-a.json"), "w") as f:
            json.dump({"text": "[cm-monitor mon-a fired] first"}, f)
        with open(os.path.join(inbox, "002-mon-b.json"), "w") as f:
            json.dump({"text": "[cm-monitor mon-b fired] second"}, f)

        res = _run_hook(self.home, "ts-1")
        self.assertEqual(res.returncode, 0)
        out = json.loads(res.stdout)
        self.assertEqual(out["decision"], "block")
        # Oldest first, double-newline separated.
        self.assertEqual(
            out["reason"],
            "[cm-monitor mon-a fired] first\n\n[cm-monitor mon-b fired] second",
        )
        # Consumed: nothing left to double-deliver on the next Stop.
        self.assertEqual(os.listdir(inbox), [])

    def test_second_stop_after_drain_allows(self):
        inbox = self._inbox("ts-1")
        with open(os.path.join(inbox, "001-mon-a.json"), "w") as f:
            json.dump({"text": "msg"}, f)
        first = _run_hook(self.home, "ts-1")
        self.assertEqual(json.loads(first.stdout)["decision"], "block")
        second = _run_hook(self.home, "ts-1")
        self.assertEqual(second.stdout.strip(), "")

    def test_malformed_message_is_dropped_not_fatal(self):
        inbox = self._inbox("ts-1")
        with open(os.path.join(inbox, "001-bad.json"), "w") as f:
            f.write("{not json")
        with open(os.path.join(inbox, "002-good.json"), "w") as f:
            json.dump({"text": "good one"}, f)
        res = _run_hook(self.home, "ts-1")
        self.assertEqual(res.returncode, 0)
        out = json.loads(res.stdout)
        self.assertEqual(out["reason"], "good one")
        self.assertEqual(os.listdir(inbox), [])

    def test_no_session_uid_is_silent_noop(self):
        res = _run_hook(self.home, None)
        self.assertEqual(res.returncode, 0)
        self.assertEqual(res.stdout.strip(), "")

    def test_normalized_stop_forwarding_preserves_continuation(self):
        fixture = Path(__file__).parent / "fixtures/claude-hooks/Stop.json"
        payload = json.loads(fixture.read_text())
        payload["stop_hook_active"] = True
        with patch.object(control_client, "call", return_value={}) as call:
            cm_stop_hook._report_turn_ended(
                "ts-test", payload=payload, observed_at=1000.0, continuing=True
            )
        method, params = call.call_args.args
        self.assertEqual(method, "session.agent_report")
        self.assertEqual(params["session_uid"], "ts-test")
        self.assertEqual(params["event"], "Stop")
        body = params["payload"]
        self.assertTrue(body["continuing"])
        self.assertTrue(body["stop_hook_active"])
        self.assertEqual(body["transcript_path"], payload["transcript_path"])
        self.assertEqual(len(body["background"]["jobs"]), 4)
        self.assertEqual(len(body["background"]["crons"]), 1)
        self.assertEqual(body["last_assistant_message"], "Turn finished.")

    def test_only_method_not_found_permits_legacy_fallback(self):
        for code in ("unknown_method", "method_not_found", -32601):
            with (
                self.subTest(code=code),
                patch.object(
                    control_client,
                    "call",
                    side_effect=[control_client.ControlError(code, "old daemon"), {}],
                ) as call,
            ):
                cm_stop_hook._report_turn_ended(
                    "ts-test", "/fixture/t.jsonl", continuing=True
                )
                self.assertEqual(call.call_count, 2)
                args = call.call_args
                self.assertEqual(
                    args.args,
                    (
                        "session.turn_ended",
                        {
                            "session_uid": "ts-test",
                            "continuing": True,
                            "transcript_path": "/fixture/t.jsonl",
                        },
                    ),
                )
                self.assertGreater(args.kwargs["timeout"], 0)
                self.assertLessEqual(
                    args.kwargs["timeout"], cm_state_hook.REPORT_TIMEOUT
                )
        for error in (
            control_client.ControlError("unauthorized", "no"),
            control_client.ControlError("conflict", "restart"),
            control_client.ControlError("invalid_params", "invalid"),
            control_client.TransportError("offline"),
            TimeoutError(),
        ):
            with (
                self.subTest(error=error),
                patch.object(control_client, "call", side_effect=error) as call,
            ):
                cm_stop_hook._report_turn_ended("ts-test")
                self.assertEqual(call.call_count, 1)

    def test_subagent_stop_does_not_consume_inbox_or_report(self):
        import io

        with (
            patch("sys.stdin", io.StringIO('{"agent_id":"child"}')),
            patch.object(cm_stop_hook, "_drain_inbox") as drain,
            patch.object(cm_stop_hook, "_report_turn_ended") as report,
        ):
            self.assertEqual(cm_stop_hook.main(), 0)
        drain.assert_not_called()
        report.assert_not_called()


if __name__ == "__main__":
    unittest.main()
