"""Tests for the P0 tool-ergonomics additions (DESIGN_TOOL_ERGONOMICS.md):

  - `start_session(wait=True)` — the one-call spawn-and-run: spawn, wait for
    the initial prompt's reply, return the worker's final message inline.
    The cm analogue of Claude Code's `Agent(prompt)`.
  - `schema=` structured output on `send_input_and_wait` / `start_session` —
    decorate the prompt, parse + validate the reply as JSON, re-prompt on a
    miss, return the parsed object as `result`.
  - the pure JSON-extraction / schema-validation helpers.

Like the sibling `test_session_monitor`, the async tests drive the REAL
tools with a stubbed `control_client.call` scripting
`resolve_authorized_session` / `send_input` / spawn responses, and back the
transcript reads with real temp JSONL files so the parsers run for real.
"""

from __future__ import annotations

import json
import os
import tempfile
import unittest

from mcp_server import control_client
from mcp_server import server as srv
from mcp_server.server import (
    _NO_TRANSCRIPT_NOTE,
    _extract_json,
    _minimal_validate,
    _validate_schema,
    read_last_turn,
    read_session_output,
    send_input_and_wait,
    start_session,
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


def _pending(path: str | None = None) -> dict:
    return {
        "state": "pending",
        "idle": True,
        "engine": "claude-code",
        "transcript_path": path,
        "generation": 0,
    }


# ── Pure helpers ───────────────────────────────────────────────────────


class ExtractJsonTests(unittest.TestCase):
    def test_bare_object(self):
        self.assertEqual(_extract_json('{"a": 1}'), ({"a": 1}, None))

    def test_fenced_block_after_prose(self):
        text = 'Here is the result:\n```json\n{"ok": true}\n```\nthanks!'
        self.assertEqual(_extract_json(text), ({"ok": True}, None))

    def test_embedded_span_with_nesting(self):
        text = 'prose {"nested": {"x": [1, 2]}} trailing'
        self.assertEqual(_extract_json(text), ({"nested": {"x": [1, 2]}}, None))

    def test_brace_inside_string_is_not_json(self):
        val, err = _extract_json('a note with { an unclosed "b}race" only')
        self.assertIsNone(val)
        self.assertIsNotNone(err)

    def test_empty(self):
        self.assertEqual(_extract_json("")[0], None)
        self.assertEqual(_extract_json("   \n ")[0], None)

    def test_last_fence_wins(self):
        # A model that shows a wrong draft then the corrected final block.
        text = '```json\n{"v": 1}\n```\nactually:\n```json\n{"v": 2}\n```'
        self.assertEqual(_extract_json(text), ({"v": 2}, None))


class MinimalValidateTests(unittest.TestCase):
    def test_required_and_type(self):
        sch = {"type": "object", "required": ["name", "count"]}
        self.assertIsNone(_minimal_validate({"name": "x", "count": 1}, sch))
        self.assertIn("count", _minimal_validate({"name": "x"}, sch))
        self.assertIn("object", _minimal_validate([1, 2], sch))

    def test_scalar_types(self):
        self.assertIsNone(_minimal_validate(5, {"type": "integer"}))
        self.assertIn("integer", _minimal_validate(True, {"type": "integer"}))
        self.assertIn("integer", _minimal_validate(1.5, {"type": "integer"}))
        self.assertIsNone(_minimal_validate(1.5, {"type": "number"}))
        self.assertIn("boolean", _minimal_validate(1, {"type": "boolean"}))

    def test_empty_schema_accepts_anything(self):
        self.assertIsNone(_validate_schema({"anything": [1]}, {}))
        self.assertIsNone(_validate_schema(42, None))


# ── Spawn-and-run ──────────────────────────────────────────────────────


class _SpawnStubMixin:
    def setUp(self):
        self._orig_call = control_client.call
        self._orig_route = control_client.resolve_socket_route
        # Force the spawn to route to the daemon method deterministically.
        control_client.resolve_socket_route = lambda: control_client.SocketRoute(
            path=None, chose_daemon=True
        )

    def tearDown(self):
        control_client.call = self._orig_call
        control_client.resolve_socket_route = self._orig_route


class StartSessionWaitTests(_SpawnStubMixin, unittest.IsolatedAsyncioTestCase):
    async def test_wait_false_unchanged(self):
        # The default path must return exactly {"session_uid": ...} and never
        # touch resolve_authorized_session.
        def _call(method, params, *a, **k):
            self.assertEqual(method, "mcp_start_session")
            return {"session_uid": "ts-new-0"}

        control_client.call = _call
        res = await start_session("claude-code", "worker")
        self.assertEqual(res, {"session_uid": "ts-new-0"})

    async def test_wait_returns_reply_of_fresh_session(self):
        # Fresh session: transcript binds a couple polls in, the reply lands,
        # then the session goes quiet. spawn-and-run returns that reply and
        # always carries session_uid.
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [])

        state = {"n": 0}

        def _call(method, params, *a, **k):
            if method == "mcp_start_session":
                self.assertEqual(params["prompt"], "do the thing")
                return {"session_uid": "ts-new-0"}
            self.assertEqual(method, "resolve_authorized_session")
            state["n"] += 1
            n = state["n"]
            if n == 1:
                return _pending(None)  # not bound yet
            if n == 2:
                _append_lines(path, [_user_line("do the thing"),
                                     _assistant_line("ALL DONE")])
                return _ready(False, path)  # bound, finishing
            return _ready(True, path)  # quiet at prompt

        control_client.call = _call
        res = await start_session(
            "claude-code", "worker", prompt="do the thing",
            wait=True, poll_interval_s=0.02,
        )
        self.assertEqual(res["session_uid"], "ts-new-0")
        self.assertTrue(res["completed"])
        self.assertFalse(res["timed_out"])
        self.assertEqual(res["status"], "awaiting_input")
        self.assertEqual(res["last_message"]["content"], "ALL DONE")

    async def test_wait_bash_transcriptless_returns_null_message(self):
        # A bash session never binds a transcript; wait must return after the
        # pending-idle grace with last_message=None (not hang to timeout).
        def _call(method, params, *a, **k):
            if method == "mcp_start_session":
                return {"session_uid": "ts-bash-0"}
            return _pending(None)  # forever pending+quiet

        control_client.call = _call
        res = await start_session(
            "bash", "sh", prompt="echo hi", wait=True,
            poll_interval_s=0.02, pending_idle_grace_s=0.2, timeout_s=30,
        )
        self.assertTrue(res["completed"])
        self.assertIsNone(res["last_message"])
        self.assertEqual(res["status"], "starting")
        self.assertEqual(res["session_uid"], "ts-bash-0")
        # P1: the transcript-less read dead-end is now explained, not silent.
        self.assertEqual(res["note"], _NO_TRANSCRIPT_NOTE)


# ── Structured output ──────────────────────────────────────────────────


class SchemaSendTests(_SpawnStubMixin, unittest.IsolatedAsyncioTestCase):
    async def test_valid_json_returned_as_result(self):
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [])

        state = {"n": 0}

        def _call(method, params, *a, **k):
            if method == "send_input":
                # The schema instruction must be appended to the body.
                self.assertIn("STRUCTURED OUTPUT REQUIRED", params["text"])
                return {}
            state["n"] += 1
            if state["n"] == 1:
                return _ready(False, path)  # pre-send anchor
            if state["n"] == 2:
                _append_lines(path, [
                    _assistant_line('```json\n{"verdict": "pass", "score": 9}\n```'),
                ])
                return _ready(False, path)
            return _ready(True, path)

        control_client.call = _call
        sch = {"type": "object", "required": ["verdict", "score"]}
        res = await send_input_and_wait(
            "ts-x-0", "review this", schema=sch, poll_interval_s=0.02,
        )
        self.assertTrue(res["completed"])
        self.assertEqual(res["result"], {"verdict": "pass", "score": 9})
        self.assertIsNone(res["schema_error"])

    async def test_reprompt_on_miss_then_success(self):
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [])

        sends: list[str] = []
        state = {"n": 0}

        def _call(method, params, *a, **k):
            if method == "send_input":
                sends.append(params["text"])
                return {}
            state["n"] += 1
            n = state["n"]
            # First send-and-await: anchor(1), reply-with-prose(2), quiet(3).
            if n == 1:
                return _ready(False, path)
            if n == 2:
                _append_lines(path, [_assistant_line("sure, it looks fine to me")])
                return _ready(True, path)
            # Second send-and-await (the correction): anchor(4), JSON(5), quiet(6).
            if n == 4:
                return _ready(False, path)
            if n == 5:
                _append_lines(path, [_assistant_line('{"verdict": "pass"}')])
                return _ready(True, path)
            return _ready(True, path)

        control_client.call = _call
        sch = {"type": "object", "required": ["verdict"]}
        res = await send_input_and_wait(
            "ts-x-0", "review", schema=sch, schema_retries=1,
            poll_interval_s=0.02,
        )
        self.assertEqual(res["result"], {"verdict": "pass"})
        self.assertIsNone(res["schema_error"])
        # Two sends: the original + one correction re-prompt.
        self.assertEqual(len(sends), 2)
        self.assertIn("did not satisfy", sends[1])

    async def test_exhausted_retries_reports_schema_error(self):
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [])

        state = {"n": 0}

        def _call(method, params, *a, **k):
            if method == "send_input":
                return {}
            state["n"] += 1
            n = state["n"]
            # Every turn replies with prose, never JSON.
            if n in (2, 5):
                _append_lines(path, [_assistant_line("no json here, sorry")])
                return _ready(True, path)
            if n in (1, 4):
                return _ready(False, path)
            return _ready(True, path)

        control_client.call = _call
        sch = {"type": "object", "required": ["verdict"]}
        res = await send_input_and_wait(
            "ts-x-0", "review", schema=sch, schema_retries=1,
            poll_interval_s=0.02,
        )
        self.assertIsNone(res["result"])
        self.assertIsNotNone(res["schema_error"])
        self.assertTrue(res["completed"])  # it DID reply, just not conformant


# ── P1: worktree isolation ─────────────────────────────────────────────


class IsolatedSpawnTests(_SpawnStubMixin, unittest.IsolatedAsyncioTestCase):
    async def test_isolated_composes_subtask_then_spawn(self):
        calls: list[str] = []

        def _call(method, params, *a, **k):
            calls.append(method)
            if method == "create_subtask":
                self.assertEqual(params["worktree_mode"], "branch")
                self.assertEqual(params["name"], "reviewer")
                return {"task_id": "task-sub-1",
                        "worktree_path": "/wt/cm-sub/reviewer-abc123"}
            if method == "mcp_start_session":
                # The session must bind to the fresh subtask.
                self.assertEqual(params["task_id"], "task-sub-1")
                return {"session_uid": "ts-iso-0"}
            raise AssertionError(f"unexpected method {method}")

        control_client.call = _call
        res = await start_session("claude-code", "reviewer", isolated=True)
        # create_subtask BEFORE the spawn.
        self.assertEqual(calls, ["create_subtask", "mcp_start_session"])
        self.assertEqual(res["session_uid"], "ts-iso-0")
        self.assertEqual(res["task_id"], "task-sub-1")
        self.assertEqual(res["worktree_path"], "/wt/cm-sub/reviewer-abc123")

    async def test_isolated_overrides_explicit_task_id(self):
        seen = {}

        def _call(method, params, *a, **k):
            if method == "create_subtask":
                return {"task_id": "task-sub-2", "worktree_path": "/wt/x"}
            seen["spawn_task_id"] = params.get("task_id")
            return {"session_uid": "ts-iso-1"}

        control_client.call = _call
        # Pass a bogus task_id — isolated must ignore it and use the subtask.
        res = await start_session(
            "claude-code", "w", task_id="some-other-task", isolated=True
        )
        self.assertEqual(seen["spawn_task_id"], "task-sub-2")
        self.assertEqual(res["task_id"], "task-sub-2")

    async def test_isolated_taskless_returns_clear_error(self):
        def _call(method, params, *a, **k):
            if method == "create_subtask":
                raise control_client.ControlError(
                    "unauthorized",
                    "create_subtask requires a tasked caller",
                )
            raise AssertionError("must not spawn when subtask creation failed")

        control_client.call = _call
        res = await start_session("claude-code", "w", isolated=True)
        self.assertEqual(res["error"], "unauthorized")
        self.assertIn("bound task", res["message"])


# ── P1: transcript-less read advisory ──────────────────────────────────


class NoTranscriptNoteTests(_SpawnStubMixin, unittest.TestCase):
    def test_read_last_turn_notes_missing_transcript(self):
        def _call(method, params, *a, **k):
            return {"state": "pending", "idle": False, "transcript_path": None,
                    "engine": "claude-code", "generation": 0}

        control_client.call = _call
        res = read_last_turn("ts-bash-0")
        self.assertIsNone(res["last_assistant"])
        self.assertEqual(res["note"], _NO_TRANSCRIPT_NOTE)

    def test_read_session_output_notes_missing_transcript(self):
        def _call(method, params, *a, **k):
            return {"state": "pending", "idle": False, "transcript_path": None,
                    "engine": "claude-code", "generation": 0}

        control_client.call = _call
        res = read_session_output("ts-bash-0")
        self.assertEqual(res["messages"], [])
        self.assertEqual(res["note"], _NO_TRANSCRIPT_NOTE)

    def test_bound_transcript_has_no_note(self):
        # A real agent read must NOT carry the advisory.
        fd, path = tempfile.mkstemp(suffix=".jsonl")
        os.close(fd)
        self.addCleanup(os.unlink, path)
        _write_lines(path, [_assistant_line("hi")])

        def _call(method, params, *a, **k):
            return _ready(True, path)

        control_client.call = _call
        res = read_last_turn("ts-x-0")
        self.assertNotIn("note", res)
        self.assertEqual(res["last_assistant"]["content"], "hi")


# ── P2: list_sessions field trim ───────────────────────────────────────


class ListSessionsTrimTests(_SpawnStubMixin, unittest.TestCase):
    def test_cols_rows_dropped_status_added(self):
        raw = [{
            "session_uid": "ts-x-0", "label": "w", "type": "claude-code",
            "state": "ready", "idle": True, "cols": 324, "rows": 98,
            "worktree_path": "/wt/x",
        }]

        def _call(method, params, *a, **k):
            self.assertEqual(method, "list_sessions")
            return raw

        control_client.call = _call
        out = srv.list_sessions()
        self.assertEqual(len(out), 1)
        s = out[0]
        # Geometry gone; legible status enriched; useful fields kept.
        self.assertNotIn("cols", s)
        self.assertNotIn("rows", s)
        self.assertEqual(s["status"], "awaiting_input")
        self.assertEqual(s["worktree_path"], "/wt/x")


if __name__ == "__main__":
    unittest.main()
