"""Tests for the worker done-signal (UX 4a) and `until="final"` monitors
(UX 3a) — the pair from the 2026-08-10 agent-side MCP UX note.

The two are one feature: `report_done` is the termination predicate that
makes a re-arming watch possible. Without it "finished" can only mean
"stopped talking", which is what made every interim turn end look like a
completion.

Coverage:
  - `_session_status` — where "reported" sits in the precedence order.
  - `_monitor_sessions(until="final")` — interim turn ends re-arm and are
    counted; a report or an exit completes; a report mid-turn waits for
    the turn boundary; a report already present at arm time is anchored.
  - `register_monitor` — the `mode="final"` / `until="task_done"`
    spellings, validation, and `already_reported`.
  - fire-message rendering for reported sessions.
  - `read_last_turn` / `read_session_output` outcome passthrough.

Scaffolding matches the sibling monitor tests: a scripted
`control_client.call` plus real temp JSONL transcripts, so the transcript
parsers and the edge/re-arm logic run for real.
"""

from __future__ import annotations

import asyncio
import json
import os
import tempfile
import unittest

from mcp_server import async_monitor, control_client
from mcp_server.monitor import _monitor_sessions, baseline_for
from mcp_server.server import (
    _session_status,
    read_last_turn,
    read_session_output,
)

# Reused rather than re-declared: it is the real control-plane stub +
# monitor tunables the async-monitor suite already maintains, and a second
# copy would drift. It declares no `test_*` methods, so importing it here
# adds no duplicate test runs.
from mcp_server.tests.test_async_monitor import (
    _MonitorEnv,
    _assistant_end_turn,
    _user,
    _write_transcript,
)

WORKER = "ts-worker"


def _append(path: str, entries: list[dict]) -> None:
    with open(path, "a", encoding="utf-8") as f:
        for e in entries:
            f.write(json.dumps(e) + "\n")


class SessionStatusReportedTests(unittest.TestCase):
    """UX 4a: "reported" is the agent's own claim, and it outranks the
    PTY-derived words — but never `exited`."""

    def test_reported_outranks_idle_and_busy(self):
        self.assertEqual(
            _session_status("ready", True, True), "reported"
        )
        # Still flushing its final message: it has DECLARED done, which is
        # the fact a watcher wants; `idle` remains available for the
        # "has the turn ended?" question.
        self.assertEqual(
            _session_status("ready", False, True), "reported"
        )

    def test_exited_still_wins(self):
        self.assertEqual(_session_status("exited", True, True), "exited")

    def test_default_preserves_the_old_mapping(self):
        self.assertEqual(_session_status("ready", True), "awaiting_input")
        self.assertEqual(_session_status("ready", False), "working")
        self.assertEqual(_session_status("pending", True), "starting")


class _FinalWatchEnv(unittest.IsolatedAsyncioTestCase):
    """A worker whose transcript and daemon payload the test drives by
    hand, one poll at a time."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = os.path.join(self.tmp.name, "worker.jsonl")
        _write_transcript(self.path, [
            _user("go"), _assistant_end_turn("turn A"),
        ])
        self.state = "ready"
        self.idle = True
        self.reported_at: float | None = None
        self.reason: str | None = None
        self.polls = 0
        self.on_poll = {}  # poll number -> callable
        self._orig_call = control_client.call
        control_client.call = self._fake_call
        self.addCleanup(self._restore)

    def _restore(self):
        control_client.call = self._orig_call

    def _fake_call(self, method, params=None, **kw):
        assert method == "resolve_authorized_session", method
        self.polls += 1
        hook = self.on_poll.pop(self.polls, None)
        if hook is not None:
            hook()
        out = {
            "state": self.state,
            "idle": self.idle,
            "engine": "claude-code",
            "transcript_path": self.path,
            "generation": 0,
            "reported_done": self.reported_at is not None,
        }
        if self.reported_at is not None:
            out["reported_done_at"] = self.reported_at
            out["report_reason"] = self.reason
        return out

    def _baselines(self) -> dict:
        return {WORKER: baseline_for("claude-code", self.path)}

    def _report(self, reason: str, at: float = 1_700_000_500.0):
        def _do():
            self.reported_at = at
            self.reason = reason
        return _do

    def _new_turn(self, text: str):
        def _do():
            _append(self.path, [_user("more"), _assistant_end_turn(text)])
        return _do

    async def _watch(self, **kw):
        return await _monitor_sessions(
            [WORKER], timeout_s=2.0, poll_interval_s=0.01,
            baselines=self._baselines(), **kw,
        )


class FinalWatchTests(_FinalWatchEnv):
    async def test_interim_turn_rearms_and_only_a_report_fires(self):
        # Poll 2 ends a NEW turn with no report — under until="turn_end"
        # that would fire (see the regression guard below). Poll 4 reports.
        self.on_poll[2] = self._new_turn("turn B, still working")
        self.on_poll[4] = self._report("wrote the migration")

        res = await self._watch(until="final")

        self.assertFalse(res["timed_out"])
        entry = res["completed"][0]
        self.assertEqual(entry["status"], "reported")
        self.assertTrue(entry["reported_done"])
        self.assertEqual(entry["report_reason"], "wrote the migration")
        self.assertEqual(
            entry["interim_turn_ends"], 1,
            "the turn that ended without a report must be counted, not fired",
        )

    async def test_turn_end_mode_still_fires_on_that_same_interim_turn(self):
        # The regression guard for the above: the default watch is
        # unchanged, so nothing that relied on it changes behavior.
        self.on_poll[2] = self._new_turn("turn B, still working")

        res = await self._watch()

        entry = res["completed"][0]
        self.assertEqual(entry["status"], "awaiting_input")
        self.assertNotIn("interim_turn_ends", entry)

    async def test_exit_completes_a_final_watch_without_any_report(self):
        def _exit():
            self.state = "exited"
        self.on_poll[2] = _exit

        res = await self._watch(until="final")

        entry = res["completed"][0]
        self.assertEqual(entry["status"], "exited")
        self.assertFalse(entry.get("reported_done"))

    async def test_report_mid_turn_waits_for_the_turn_to_end(self):
        # The agent calls report_done and keeps writing its final message.
        # Firing here would quote a half-written reply.
        def _report_mid_turn():
            _append(self.path, [_user("keep going")])  # turn in flight
            self.idle = False
            self.reported_at = 1_700_000_500.0
            self.reason = "done, writing it up"
        self.on_poll[1] = _report_mid_turn

        def _finish():
            _append(self.path, [_assistant_end_turn("FINAL REPORT")])
            self.idle = True
        self.on_poll[4] = _finish

        res = await self._watch(until="final")

        self.assertGreaterEqual(
            self.polls, 4, "must not have completed on the mid-turn report"
        )
        entry = res["completed"][0]
        self.assertEqual(entry["status"], "reported")
        self.assertEqual(entry["last_message"]["content"], "FINAL REPORT")

    async def test_report_present_at_arm_time_is_anchored(self):
        self.reported_at = 1_700_000_000.0
        self.reason = "an OLD report"

        res = await _monitor_sessions(
            [WORKER], until="final", timeout_s=0.3, poll_interval_s=0.01,
            baselines=self._baselines(),
            report_anchors={WORKER: 1_700_000_000.0},
        )

        self.assertTrue(res["timed_out"], "a stale report must not fire")
        self.assertEqual(res["still_running"], [WORKER])

    async def test_a_fresh_report_past_the_anchor_fires(self):
        self.reported_at = 1_700_000_000.0
        self.on_poll[2] = self._report("the NEW one", at=1_700_009_999.0)

        res = await _monitor_sessions(
            [WORKER], until="final", timeout_s=2.0, poll_interval_s=0.01,
            baselines=self._baselines(),
            report_anchors={WORKER: 1_700_000_000.0},
        )

        self.assertEqual(res["completed"][0]["report_reason"], "the NEW one")

    async def test_no_anchor_means_any_report_fires(self):
        # Level-triggered caller (edge=False / a blocking wait): it never
        # captured an anchor, so the report it can see is news to it.
        self.reported_at = 1_700_000_000.0
        self.reason = "already done"

        res = await _monitor_sessions(
            [WORKER], until="final", timeout_s=2.0, poll_interval_s=0.01,
        )

        self.assertEqual(res["completed"][0]["status"], "reported")


class RegisterFinalModeTests(_MonitorEnv):
    """Registration-side: the spellings agents will actually type, and
    the already-reported announcement."""

    def _register(self, **kw) -> dict:
        async def scenario():
            reg = async_monitor.register_monitor(
                [self.WORKER], source="explicit", **kw
            )
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            rec["task"].cancel()
            try:
                await rec["task"]
            except asyncio.CancelledError:
                pass
            return reg, rec

        return asyncio.run(scenario())

    def test_mode_final_is_sugar_for_until_final(self):
        reg, rec = self._register(mode="final")
        self.assertEqual(reg["mode"], "any")
        self.assertEqual(reg["until"], "final")
        self.assertEqual(rec["until"], "final")
        self.assertIn("EXITED or called report_done", reg["async_note"])

    def test_task_done_is_accepted_as_a_synonym(self):
        reg, _ = self._register(until="task_done")
        self.assertEqual(reg["until"], "final")

    def test_all_plus_final_keeps_both_axes(self):
        reg, _ = self._register(mode="all", until="final")
        self.assertEqual((reg["mode"], reg["until"]), ("all", "final"))

    def test_bad_until_is_rejected_with_the_valid_spellings(self):
        with self.assertRaises(async_monitor.RegistrationError) as ctx:
            self._register(until="whenever")
        self.assertEqual(ctx.exception.code, "invalid_params")
        self.assertIn("turn_end", str(ctx.exception))

    def test_final_note_promises_no_interim_wakeups(self):
        reg, _ = self._register(mode="final")
        self.assertIn("Interim turn ends will NOT wake you", reg["async_note"])

    def test_already_idle_note_is_not_used_for_final_watches(self):
        # The scaffold's worker is idle at its prompt. Under a final watch
        # that is unremarkable — telling the caller to go read it would be
        # advice for the wrong mode.
        reg, _ = self._register(mode="final")
        self.assertIn(self.WORKER, reg["already_idle"])
        self.assertNotIn("ALREADY at the prompt", reg["async_note"])


class AlreadyReportedTests(_MonitorEnv):
    def _fake_call(self, method, params=None, **kw):
        out = super()._fake_call(method, params, **kw)
        if (
            method == "resolve_authorized_session"
            and params["session_uid"] == self.WORKER
        ):
            out["reported_done"] = True
            out["reported_done_at"] = 1_700_000_000.0
        return out

    def test_already_reported_is_announced_and_anchored(self):
        async def scenario():
            reg = async_monitor.register_monitor(
                [self.WORKER], mode="final", source="explicit"
            )
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            rec["task"].cancel()
            try:
                await rec["task"]
            except asyncio.CancelledError:
                pass
            return reg, rec

        reg, rec = asyncio.run(scenario())
        self.assertEqual(reg["already_reported"], [self.WORKER])
        self.assertEqual(rec["report_anchors"], {self.WORKER: 1_700_000_000.0})
        self.assertIn("ALREADY reported done", reg["async_note"])

    def test_auto_source_suppresses_the_announcement_but_keeps_the_anchor(self):
        # A just-prompted worker's old report is already superseded
        # daemon-side; naming it would describe a fact that is gone.
        async def scenario():
            reg = async_monitor.register_monitor(
                [self.WORKER], mode="final", source="auto"
            )
            rec = async_monitor._MONITORS[reg["monitor_id"]]
            rec["task"].cancel()
            try:
                await rec["task"]
            except asyncio.CancelledError:
                pass
            return reg, rec

        reg, rec = asyncio.run(scenario())
        self.assertEqual(reg["already_reported"], [])
        self.assertEqual(rec["report_anchors"], {self.WORKER: 1_700_000_000.0})


class FireMessageReportedTests(unittest.TestCase):
    """UX 4a in the wake-up text: a done report is announced, and the
    "this may be an interim turn" caveat is dropped over it — a caveat
    that cried wolf would teach readers to skip it."""

    RECORD = {"monitor_id": "m-1", "note": "", "until": "final"}

    def _format(self, entry: dict, still: list[str] | None = None,
                record: dict | None = None) -> str:
        return async_monitor._format_fire_message(
            dict(record or self.RECORD),
            {"completed": [entry], "still_running": still or []},
        )

    def test_reported_entry_quotes_the_reason_and_drops_the_caveat(self):
        text = self._format({
            "session_uid": "ts-w", "status": "reported", "state": "ready",
            "idle": True, "reported_done": True,
            "reported_done_at": 1_700_000_000.0,
            "report_reason": "migration merged, tests green",
            "last_message": {"content": "VERDICT: done"},
        })
        self.assertIn("- ts-w (reported): VERDICT: done", text)
        self.assertIn(
            "reported done at 2023-11-14T22:13:20Z: "
            "migration merged, tests green",
            text,
        )
        self.assertNotIn("no explicit done-report", text)

    def test_reported_without_a_reason_still_says_reported(self):
        text = self._format({
            "session_uid": "ts-w", "status": "reported", "state": "ready",
            "idle": True, "reported_done": True, "last_message": None,
        })
        self.assertIn("reported done", text)
        self.assertNotIn("no explicit done-report", text)

    def test_exited_after_reporting_is_credited_for_it(self):
        text = self._format({
            "session_uid": "ts-w", "status": "exited", "state": "exited",
            "idle": True, "reported_done": True,
            "reported_done_at": 1_700_000_000.0,
            "report_reason": "all four fixes landed",
            "last_message": {"content": "signing off"},
        })
        self.assertIn("reported done before exiting", text)
        self.assertIn("all four fixes landed", text)

    def test_interim_count_is_surfaced(self):
        text = self._format({
            "session_uid": "ts-w", "status": "reported", "state": "ready",
            "idle": True, "reported_done": True, "interim_turn_ends": 3,
            "last_message": None,
        })
        self.assertIn("3 interim turn ends passed", text)

    def test_unreported_entry_keeps_the_caveat(self):
        text = self._format({
            "session_uid": "ts-w", "status": "awaiting_input",
            "state": "ready", "idle": True, "reported_done": False,
            "last_message": {"content": "hmm"},
        })
        self.assertIn("no explicit done-report", text)

    def test_final_timeout_says_nobody_reported(self):
        text = async_monitor._format_fire_message(
            dict(self.RECORD),
            {"completed": [], "still_running": ["ts-w"], "timed_out": True},
        )
        self.assertIn("never called report_done", text)

    def test_turn_end_timeout_wording_is_unchanged(self):
        text = async_monitor._format_fire_message(
            {"monitor_id": "m-1", "note": "", "until": "turn_end"},
            {"completed": [], "still_running": ["ts-w"], "timed_out": True},
        )
        self.assertIn("TIMED OUT before every watched session finished", text)


class ReadToolOutcomeTests(unittest.TestCase):
    """The read tools carry the daemon's outcome fields, so "is this a
    conclusion or wherever the process stopped?" is answerable without a
    second call."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = os.path.join(self.tmp.name, "w.jsonl")
        _write_transcript(self.path, [
            _user("go"), _assistant_end_turn("VERDICT: shipped"),
        ])
        self._orig_call = control_client.call
        self.addCleanup(self._restore)

    def _restore(self):
        control_client.call = self._orig_call

    def _resolve(self, extra: dict) -> None:
        def _call(method, params=None, **kw):
            return {
                "state": "ready", "idle": True, "engine": "claude-code",
                "transcript_path": self.path, "generation": 0, **extra,
            }
        control_client.call = _call

    def test_read_last_turn_reports_status_and_reason(self):
        self._resolve({
            "reported_done": True,
            "reported_done_at": 1_700_000_000.0,
            "report_reason": "shipped it",
        })
        res = read_last_turn(WORKER)
        self.assertEqual(res["status"], "reported")
        self.assertEqual(res["report_reason"], "shipped it")

    def test_read_last_turn_surfaces_kill_provenance(self):
        self._resolve({
            "state": "exited", "killed": True, "killed_by": "memory-cap",
            "exited_at": 1_700_000_000.0,
        })
        res = read_last_turn(WORKER)
        self.assertEqual(res["status"], "exited")
        self.assertTrue(res["killed"])
        self.assertEqual(res["killed_by"], "memory-cap")

    def test_read_session_output_carries_the_same_fields(self):
        self._resolve({
            "reported_done": True, "reported_done_at": 1_700_000_000.0,
        })
        res = read_session_output(WORKER)
        self.assertEqual(res["status"], "reported")
        self.assertTrue(res["reported_done"])

    def test_absent_outcome_fields_are_not_invented(self):
        self._resolve({})
        res = read_last_turn(WORKER)
        self.assertEqual(res["status"], "awaiting_input")
        for key in ("killed", "killed_by", "reported_done", "report_reason"):
            self.assertNotIn(key, res)


if __name__ == "__main__":
    unittest.main()
