"""Tests for transcript-shape semantic idle (S1b of the async-wait
branch): a READY session whose PTY stays noisy (background-task
spinner) must still report idle once its transcript's last main-chain
message is a completed assistant turn.

  - `transcript_turn_complete` — the tail-scan classifier itself.
  - `_monitor_sessions` — busy-PTY sessions complete via the
    transcript signal (debounced), reported as `awaiting_input` with
    `idle_source: "transcript"`.
  - `wait_for_session_idle` — same fallback on the single-session wait.

Same scaffolding as test_session_monitor: stub `control_client.call`,
real temp JSONL transcripts.
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest
from unittest import mock

from mcp_server import control_client, monitor
from mcp_server.monitor import _monitor_sessions, transcript_turn_complete
import mcp_server.server as server_mod
from mcp_server.server import wait_for_session_idle


def _line(entry: dict) -> str:
    return json.dumps(entry)


def _assistant(stop_reason, text="hi", sidechain=False) -> str:
    return _line({
        "type": "assistant",
        "isSidechain": sidechain,
        "message": {
            "content": [{"type": "text", "text": text}],
            "stop_reason": stop_reason,
        },
    })


def _user(text="go", sidechain=False) -> str:
    return _line({
        "type": "user",
        "isSidechain": sidechain,
        "message": {"content": text},
    })


def _meta(kind: str) -> str:
    return _line({"type": kind, "sessionId": "s"})


def _write(path: str, lines: list[str]) -> None:
    with open(path, "w", encoding="utf-8") as f:
        for ln in lines:
            f.write(ln + "\n")


class TranscriptTurnCompleteTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "t.jsonl")

    def tearDown(self):
        self.tmp.cleanup()

    def test_end_turn_last_is_complete(self):
        _write(self.path, [_user(), _assistant("end_turn")])
        self.assertTrue(transcript_turn_complete("claude-code", self.path))

    def test_trailing_meta_lines_are_skipped(self):
        _write(self.path, [
            _user(), _assistant("end_turn"),
            _meta("attachment"), _meta("system"),
            _meta("ai-title"), _meta("mode"), _meta("permission-mode"),
        ])
        self.assertTrue(transcript_turn_complete("claude-code", self.path))

    def test_tool_use_is_mid_turn(self):
        _write(self.path, [_user(), _assistant("tool_use")])
        self.assertFalse(transcript_turn_complete("claude-code", self.path))

    def test_none_stop_reason_is_mid_turn(self):
        _write(self.path, [_user(), _assistant(None)])
        self.assertFalse(transcript_turn_complete("claude-code", self.path))

    def test_user_last_is_mid_turn(self):
        # A tool result / fresh prompt: the next turn is starting.
        _write(self.path, [_assistant("tool_use"), _user()])
        self.assertFalse(transcript_turn_complete("claude-code", self.path))

    def test_sidechain_entries_are_skipped(self):
        # A foreground subagent's own end_turn must not read as the
        # MAIN agent's turn ending — the main chain is still on its
        # tool_use.
        _write(self.path, [
            _user(),
            _assistant("tool_use"),
            _user(sidechain=True),
            _assistant("end_turn", sidechain=True),
        ])
        self.assertFalse(transcript_turn_complete("claude-code", self.path))

    def test_codex_engine_is_never_semantic_idle(self):
        _write(self.path, [_user(), _assistant("end_turn")])
        self.assertFalse(transcript_turn_complete("codex", self.path))

    def test_missing_or_absent_path(self):
        self.assertFalse(transcript_turn_complete("claude-code", None))
        self.assertFalse(transcript_turn_complete(
            "claude-code", os.path.join(self.tmp.name, "nope.jsonl")))

    def test_garbage_lines_are_skipped(self):
        with open(self.path, "w", encoding="utf-8") as f:
            f.write("{truncated partial\n")
            f.write(_assistant("end_turn") + "\n")
            f.write("not json at all\n")
        self.assertTrue(transcript_turn_complete("claude-code", self.path))


class _StubMixin:
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "t.jsonl")
        self._orig_call = control_client.call

    def tearDown(self):
        control_client.call = self._orig_call
        self.tmp.cleanup()

    def _stub_always_busy_ready(self):
        path = self.path

        def fake_call(method, params=None, **kw):
            assert method == "resolve_authorized_session"
            return {
                "state": "ready", "idle": False,
                "engine": "claude-code",
                "transcript_path": path, "generation": 0,
            }

        control_client.call = fake_call


class MonitorSemanticIdleTests(_StubMixin, unittest.TestCase):
    def test_busy_pty_completes_via_transcript(self):
        _write(self.path, [_user(), _assistant("end_turn", text="done!")])
        self._stub_always_busy_ready()
        with mock.patch.object(monitor, "SEMANTIC_IDLE_GRACE_S", 0.2):
            res = asyncio.run(_monitor_sessions(
                ["ts-busy"], mode="any", timeout_s=10.0,
                poll_interval_s=0.5,
            ))
        self.assertFalse(res["timed_out"])
        self.assertEqual(len(res["completed"]), 1)
        entry = res["completed"][0]
        self.assertEqual(entry["status"], "awaiting_input")
        self.assertEqual(entry["idle_source"], "transcript")
        self.assertIn("done!", entry["last_message"]["content"])

    def test_busy_pty_mid_turn_times_out(self):
        _write(self.path, [_user(), _assistant("tool_use")])
        self._stub_always_busy_ready()
        res = asyncio.run(_monitor_sessions(
            ["ts-busy"], mode="any", timeout_s=1.5,
            poll_interval_s=0.5,
        ))
        self.assertTrue(res["timed_out"])
        self.assertEqual(res["completed"], [])


class WaitForSessionIdleSemanticTests(_StubMixin, unittest.TestCase):
    def test_busy_pty_returns_idle_via_transcript(self):
        _write(self.path, [_user(), _assistant("end_turn")])
        self._stub_always_busy_ready()
        with mock.patch.object(server_mod, "SEMANTIC_IDLE_GRACE_S", 0.2):
            res = asyncio.run(wait_for_session_idle(
                "ts-busy", timeout_s=10.0, poll_interval_s=0.5,
            ))
        self.assertTrue(res["idle"])
        self.assertFalse(res["timed_out"])
        self.assertEqual(res["status"], "awaiting_input")
        self.assertEqual(res["idle_source"], "transcript")


if __name__ == "__main__":
    unittest.main()
