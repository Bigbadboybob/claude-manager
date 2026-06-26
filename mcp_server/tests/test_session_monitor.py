"""Tests for the async multi-session monitor + the transcript-anchored
send/wait/read tools added to close the cm-MCP orchestration UX gaps:

  - `_session_status` — the legible (state, idle) → one-word mapping.
  - `read_last_turn` — tail read of a transcript (last assistant message).
  - `send_input_and_wait` — send + wait, anchored on transcript progress
    so it returns the reply to THIS input, not the prior turn (the
    post-send idle race).
  - `wait_for_any_session_idle` — watch N sessions, return on the first
    completion.

Like `test_wait_for_session_idle`, these drive the REAL tools with a
stubbed `control_client.call` returning scripted
`resolve_authorized_session` / `send_input` responses, and back the
transcript reads with real temp JSONL files so the parsers run for real.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import tempfile
import unittest

from mcp_server import control_client, wait
from mcp_server.monitor import _monitor_sessions
from mcp_server.server import (
    _session_status,
    read_last_turn,
    send_input_and_wait,
    wait_for_any_session_idle,
)


def _assistant_line(text: str) -> str:
    return json.dumps(
        {"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}
    )


def _user_line(text: str) -> str:
    return json.dumps({"type": "user", "message": {"content": text}})


def _write_lines(path: str, lines: list[str]) -> None:
    with open(path, "w", encoding="utf-8") as f:
        for ln in lines:
            f.write(ln + "\n")


def _append_lines(path: str, lines: list[str]) -> None:
    with open(path, "a", encoding="utf-8") as f:
        for ln in lines:
            f.write(ln + "\n")


def _ready(idle: bool, path: str | None) -> dict:
    return {
        "state": "ready",
        "idle": idle,
        "engine": "claude-code",
        "transcript_path": path,
        "generation": 0,
    }


class SessionStatusTests(unittest.TestCase):
    def test_mapping(self):
        self.assertEqual(_session_status("exited", True), "exited")
        self.assertEqual(_session_status("exited", False), "exited")
        self.assertEqual(_session_status("ready", True), "awaiting_input")
        self.assertEqual(_session_status("ready", False), "working")
        # pending is always "starting" — a quiet pending session is NOT
        # claimed to be awaiting input (transcript not bound yet).
        self.assertEqual(_session_status("pending", True), "starting")
        self.assertEqual(_session_status("pending", False), "starting")


class _SocketStubMixin:
    def setUp(self):
        self._orig_call = control_client.call

    def tearDown(self):
        control_client.call = self._orig_call


class ReadLastTurnTests(_SocketStubMixin, unittest.TestCase):
    def test_returns_final_assistant_and_tail(self):
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [
            _user_line("first ask"),
            _assistant_line("first answer"),
            _user_line("second ask"),
            _assistant_line("FINAL"),
        ])

        def _call(method, params, *a, **k):
            self.assertEqual(method, "resolve_authorized_session")
            return _ready(True, path)

        control_client.call = _call
        res = read_last_turn("ts-x-0", context_messages=2)

        self.assertEqual(res["last_assistant"]["content"], "FINAL")
        self.assertEqual(res["status"], "awaiting_input")
        # context_messages=2 → only the trailing two messages.
        self.assertEqual([m["content"] for m in res["messages"]],
                         ["second ask", "FINAL"])

    def test_pending_no_transcript(self):
        def _call(method, params, *a, **k):
            return {"state": "pending", "idle": False, "transcript_path": None,
                    "engine": "claude-code", "generation": 0}

        control_client.call = _call
        res = read_last_turn("ts-x-0")
        self.assertIsNone(res["last_assistant"])
        self.assertEqual(res["status"], "starting")
        self.assertEqual(res["messages"], [])


class SendInputAndWaitTests(_SocketStubMixin, unittest.IsolatedAsyncioTestCase):
    async def test_anchors_on_new_assistant_message_not_the_race(self):
        # The core race fix. The transcript already holds a prior answer
        # ("OLD"). Right after we send, the session momentarily reads
        # ready+idle with NO new assistant message yet — a plain idle-wait
        # would return "OLD" here. send_input_and_wait must keep waiting
        # until the NEW reply appears and the session goes quiet, then
        # return the NEW reply.
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [_assistant_line("OLD")])

        state = {"resolve_n": 0, "sent": False}

        def _call(method, params, *a, **k):
            if method == "send_input":
                state["sent"] = True
                return {}
            self.assertEqual(method, "resolve_authorized_session")
            state["resolve_n"] += 1
            n = state["resolve_n"]
            # n=1 pre-send anchor read; n=2 poll1 (the race: idle, no new
            # message); n=3 poll2 (new reply lands, still busy); n>=4 quiet.
            if n == 3:
                _append_lines(path, [_user_line("the ask"),
                                     _assistant_line("NEW")])
                return _ready(False, path)  # busy while finishing the turn
            return _ready(True, path)

        control_client.call = _call
        res = await send_input_and_wait(
            "ts-x-0", "the ask", poll_interval_s=0.02
        )

        self.assertTrue(state["sent"])
        self.assertTrue(res["completed"])
        self.assertFalse(res["timed_out"])
        self.assertEqual(res["status"], "awaiting_input")
        # The reply to THIS input, not the stale prior turn.
        self.assertEqual(res["last_message"]["content"], "NEW")
        # It must NOT have returned on the first idle poll (the race);
        # that required reaching at least the 4th resolve.
        self.assertGreaterEqual(state["resolve_n"], 4)

    async def test_exit_after_reply_returns_final_message(self):
        # A one-shot agent that answers then exits: the eviction
        # (not_found after being seen) is the exit signal; the reply is
        # still readable from the captured transcript path.
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [])

        state = {"resolve_n": 0}

        def _call(method, params, *a, **k):
            if method == "send_input":
                return {}
            state["resolve_n"] += 1
            n = state["resolve_n"]
            if n == 1:
                return _ready(False, path)  # pre-send: bound, busy
            if n == 2:
                _append_lines(path, [_user_line("ask"),
                                     _assistant_line("DONE")])
                return _ready(False, path)  # reply lands, still busy
            # Evicted == exited.
            raise control_client.ControlError("not_found", "gone")

        control_client.call = _call
        res = await send_input_and_wait("ts-x-0", "ask", poll_interval_s=0.02)

        self.assertTrue(res["completed"])
        self.assertEqual(res["status"], "exited")
        self.assertEqual(res["last_message"]["content"], "DONE")


class WaitForAnyTests(_SocketStubMixin, unittest.IsolatedAsyncioTestCase):
    async def test_returns_first_completer(self):
        # Two workers: "a" still busy, "b" finished. The call must return
        # promptly with b completed and a still running.
        responses = {"a": _ready(False, None), "b": _ready(True, None)}

        def _call(method, params, *a, **k):
            self.assertEqual(method, "resolve_authorized_session")
            return dict(responses[params["session_uid"]])

        control_client.call = _call
        res = await wait_for_any_session_idle(
            ["a", "b"], poll_interval_s=0.02, return_last_message=False
        )

        self.assertFalse(res["timed_out"])
        self.assertEqual([c["session_uid"] for c in res["completed"]], ["b"])
        self.assertEqual(res["completed"][0]["status"], "awaiting_input")
        self.assertEqual(res["still_running"], ["a"])

    async def test_bad_uid_reported_not_hung(self):
        # A uid that never resolves (bad / unauthorized) is reported once
        # as an error entry rather than blocking the whole monitor.
        def _call(method, params, *a, **k):
            if params["session_uid"] == "bad":
                raise control_client.ControlError("unauthorized", "nope")
            return _ready(False, None)  # "ok" stays busy

        control_client.call = _call
        res = await wait_for_any_session_idle(
            ["ok", "bad"], poll_interval_s=0.02, return_last_message=False
        )
        errs = [c for c in res["completed"] if c["session_uid"] == "bad"]
        self.assertEqual(len(errs), 1)
        self.assertEqual(errs[0]["status"], "error")
        self.assertEqual(errs[0]["error"], "unauthorized")
        self.assertEqual(res["still_running"], ["ok"])

    async def test_empty_returns_immediately(self):
        res = await wait_for_any_session_idle([])
        self.assertEqual(
            res, {"completed": [], "still_running": [], "timed_out": False}
        )


def _seq_stub(per_uid):
    """control_client.call replacement scripting resolve responses per uid;
    each uid's last response repeats forever."""
    counters = {u: 0 for u in per_uid}

    def _call(method, params, *a, **k):
        assert method == "resolve_authorized_session", method
        uid = params["session_uid"]
        seq = per_uid[uid]
        i = min(counters[uid], len(seq) - 1)
        counters[uid] += 1
        return dict(seq[i])

    return _call


class MonitorAllModeTests(_SocketStubMixin, unittest.IsolatedAsyncioTestCase):
    async def test_all_waits_for_every_session(self):
        # "a" is busy on the first poll then idle; "b" is idle immediately.
        # mode="all" must NOT return after b — it waits until a finishes too,
        # accumulating both across polls.
        control_client.call = _seq_stub({
            "a": [_ready(False, None), _ready(True, None)],
            "b": [_ready(True, None)],
        })
        res = await _monitor_sessions(
            ["a", "b"], mode="all", poll_interval_s=0.02,
            return_last_message=False,
        )
        self.assertFalse(res["timed_out"])
        self.assertEqual(
            sorted(c["session_uid"] for c in res["completed"]), ["a", "b"])
        self.assertEqual(res["still_running"], [])

    async def test_all_times_out_with_partial(self):
        # "a" finishes, "b" never does → timeout returns a in completed and
        # b in still_running.
        control_client.call = _seq_stub({
            "a": [_ready(True, None)],
            "b": [_ready(False, None)],
        })
        res = await _monitor_sessions(
            ["a", "b"], mode="all", timeout_s=0.2, poll_interval_s=0.02,
            return_last_message=False,
        )
        self.assertTrue(res["timed_out"])
        self.assertEqual([c["session_uid"] for c in res["completed"]], ["a"])
        self.assertEqual(res["still_running"], ["b"])


class WaitCliTests(_SocketStubMixin, unittest.TestCase):
    def _run_main(self, argv):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = wait.main(argv)
        return rc, json.loads(buf.getvalue())

    def test_main_any_completed_exit_0(self):
        control_client.call = lambda m, p, *a, **k: _ready(True, None)
        rc, out = self._run_main(["uid-1", "--poll-interval", "0.02"])
        self.assertEqual(rc, 0)
        self.assertEqual(out["mode"], "any")
        self.assertEqual([c["session_uid"] for c in out["completed"]], ["uid-1"])
        self.assertFalse(out["timed_out"])

    def test_main_timeout_exit_1(self):
        control_client.call = lambda m, p, *a, **k: _ready(False, None)
        rc, out = self._run_main(
            ["uid-1", "--timeout", "0.1", "--poll-interval", "0.02"])
        self.assertEqual(rc, 1)
        self.assertTrue(out["timed_out"])
        self.assertEqual(out["still_running"], ["uid-1"])

    def test_main_transport_error_exit_3(self):
        def _boom(m, p, *a, **k):
            raise control_client.TransportError("connect refused")
        control_client.call = _boom
        rc, out = self._run_main(["uid-1", "--poll-interval", "0.02"])
        self.assertEqual(rc, 3)
        self.assertEqual(out["error"], "transport")


if __name__ == "__main__":
    unittest.main()
