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

HOOK = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "hooks", "cm_stop_hook.py",
)

STOP_INPUT = json.dumps({
    "hook_event_name": "Stop",
    "session_id": "b74d8490",
    "stop_hook_active": False,
})


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
            "[cm-monitor mon-a fired] first\n\n"
            "[cm-monitor mon-b fired] second",
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


if __name__ == "__main__":
    unittest.main()
