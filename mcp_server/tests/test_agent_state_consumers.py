"""Engine state governs MCP status and completion, with legacy fallback intact."""

from __future__ import annotations

import asyncio
import time
import unittest
from unittest.mock import patch

from mcp_server import async_monitor, control_client, monitor, server


def observed(state, seq=2, *, ended=20.0, idle=True, source="hooks", lifecycle="ready"):
    return {
        "state": lifecycle,
        "idle": idle,
        "pty_idle": idle,
        "engine": "claude-code",
        "transcript_path": None,
        "generation": 0,
        "agent_state": {
            "state": state,
            "source": source,
            "turn_seq": seq,
            "since": 20.0,
            "last_turn": {"ended_at": ended, "status": "completed"},
        },
    }


class StateReadTests(unittest.TestCase):
    def test_status_mapping_overrides_compatibility_idle_and_transcript_binding(self):
        for state, status in [
            ("working", "working"),
            ("working-background", "awaiting_input"),
            ("idle", "awaiting_input"),
            ("waiting-on-human", "needs_human"),
            ("errored", "errored"),
            ("unknown", "unknown"),
            ("starting", "starting"),
            ("future", "unknown"),
        ]:
            with self.subTest(state=state):
                value = observed(state, lifecycle="pending")
                self.assertEqual(
                    monitor._session_status(
                        "pending", True, False, value["agent_state"]
                    ),
                    status,
                )
                self.assertEqual(
                    monitor._session_status("ready", False, True, value["agent_state"]),
                    "reported",
                )
                self.assertEqual(
                    monitor._session_status("exited", True, True, value["agent_state"]),
                    "exited",
                )

    def test_fallback_sources_retain_legacy_status(self):
        for source in ("pty", "transcript"):
            self.assertEqual(
                monitor._session_status(
                    "pending",
                    True,
                    False,
                    observed("idle", source=source)["agent_state"],
                ),
                "starting",
            )
            self.assertIsNone(
                monitor.engine_turn_complete(observed("idle", source=source))
            )

    def test_list_and_reads_publish_state_and_raw_pty_value(self):
        value = observed("waiting-on-human")
        with patch.object(
            control_client, "call", return_value=[dict(value, session_uid="ts-test")]
        ):
            self.assertEqual(server.list_sessions()[0]["status"], "needs_human")
        with patch.object(control_client, "call", return_value=value):
            for result in (
                server.read_session_output("ts-test"),
                server.read_last_turn("ts-test"),
            ):
                self.assertEqual(result["status"], "needs_human")
                self.assertEqual(result["agent_state"], value["agent_state"])
                self.assertTrue(result["pty_idle"])

    def test_baseline_uses_engine_turns_and_keeps_rollback_edge(self):
        with patch.object(
            monitor,
            "last_completed_turn_fingerprint",
            return_value="old-fingerprint",
        ):
            working = observed("working", seq=7)
            idle = observed("idle", seq=7)
            for initial, expected, at_prompt in [(working, 6, False), (idle, 7, True)]:
                with patch.object(control_client, "call", return_value=initial):
                    baseline, ready, _ = async_monitor._capture_baseline("ts-test")
                self.assertEqual(baseline, {"kind": "agent", "value": expected, "ended_at": 20.0, "no_turn_at": None, "legacy": {"kind": "turn", "value": "old-fingerprint"}})
                self.assertEqual(ready, at_prompt)
            self.assertFalse(
                monitor._edge_passed(
                    {"kind": "agent", "value": 7}, "claude-code", None, idle
                )
            )
            self.assertTrue(
                monitor._edge_passed(
                    {"kind": "agent", "value": 6}, "claude-code", None, idle
                )
            )

    def test_attention_and_background_notifications_do_not_claim_work_done(self):
        for state, phrase in [
            ("waiting-on-human", "human input or permission needed"),
            ("errored", "engine reported an error"),
            ("working-background", "background work is still running"),
        ]:
            value = observed(state)
            value.update(
                session_uid="ts-test",
                status=monitor._session_status(
                    "ready", True, False, value["agent_state"]
                ),
            )
            result = {"completed": [value], "still_running": [], "timed_out": False}
            message = async_monitor._format_fire_message(
                {"monitor_id": "test", "note": ""}, result
            )
            self.assertIn(phrase, message)
            if state != "working-background":
                self.assertNotIn("worker(s) are now awaiting input", message)


class EngineWaitTests(unittest.IsolatedAsyncioTestCase):
    async def test_level_waits_ignore_pty_idle_for_working_unknown_and_starting(self):
        for state in ("working", "unknown", "starting", "future"):
            with (
                self.subTest(state=state),
                patch.object(control_client, "call", return_value=dict(observed(state), pty_idle=False)),
                patch.object(
                    monitor,
                    "transcript_turn_complete",
                    side_effect=AssertionError("legacy read"),
                ),
                patch.object(
                    server,
                    "transcript_turn_complete",
                    side_effect=AssertionError("legacy read"),
                ),
            ):
                single, many = await asyncio.gather(
                    server.wait_for_session_idle(
                        "ts-test", timeout_s=1, poll_interval_s=0.5
                    ),
                    monitor._monitor_sessions(
                        ["ts-test"], timeout_s=1, poll_interval_s=0.5
                    ),
                )
                self.assertTrue(single["timed_out"])
                self.assertTrue(many["timed_out"])
                self.assertEqual(many["completed"], [])
                self.assertEqual(single["agent_state"]["state"], state)

    async def test_engine_boundaries_work_without_a_transcript_and_with_busy_pty(self):
        for state, status in [
            ("idle", "awaiting_input"),
            ("waiting-on-human", "needs_human"),
            ("errored", "errored"),
            ("working-background", "awaiting_input"),
        ]:
            value = observed(state, idle=False, lifecycle="pending")
            with (
                self.subTest(state=state),
                patch.object(control_client, "call", return_value=value),
            ):
                single = await server.wait_for_session_idle("ts-test", timeout_s=1)
                many = await monitor._monitor_sessions(["ts-test"], timeout_s=1)
                self.assertFalse(single["timed_out"])
                self.assertEqual(single["status"], status)
                self.assertEqual(many["completed"][0]["status"], status)
                self.assertEqual(
                    many["completed"][0]["agent_state"], value["agent_state"]
                )

    async def test_background_without_turn_end_holds(self):
        with patch.object(
            control_client,
            "call",
            return_value=observed("working-background", ended=None),
        ):
            result = await monitor._monitor_sessions(
                ["ts-test"], timeout_s=1, poll_interval_s=0.5
            )
        self.assertTrue(result["timed_out"])

    async def test_new_sequence_fires_but_old_idle_turn_does_not(self):
        for sequence, fires in [(4, False), (5, True)]:
            with (
                self.subTest(sequence=sequence),
                patch.object(
                    control_client, "call", return_value=observed("idle", seq=sequence)
                ),
            ):
                result = await monitor._monitor_sessions(
                    ["ts-test"],
                    baselines={"ts-test": {"kind": "agent", "value": 4}},
                    timeout_s=1,
                    poll_interval_s=0.5,
                )
                self.assertEqual(bool(result["completed"]), fires)

    async def test_stop_only_claude_session_uses_new_engine_end_not_heartbeat(self):
        initial = observed("idle", seq=3, ended=20.0)
        baseline = monitor.baseline_for("claude-code", None, initial)
        heartbeat = observed("idle", seq=3, ended=20.0)
        heartbeat["agent_state"]["observed_at"] = 999.0
        self.assertFalse(monitor._edge_passed(baseline, "claude-code", None, heartbeat))
        with patch.object(control_client, "call", return_value=observed("idle", seq=3, ended=25.0)):
            result = await monitor._monitor_sessions(["ts-test"], baselines={"ts-test":baseline},
                                                     timeout_s=1, poll_interval_s=.5)
        self.assertEqual(result["completed"][0]["agent_state"]["last_turn"]["ended_at"], 25.0)

    async def test_final_monitor_rearms_interim_turn_and_requires_new_report(self):
        initial = observed("idle", seq=3)
        ending = dict(
            observed("idle", seq=3), reported_done=True, reported_done_at=123.0
        )
        baselines = {"ts-test": {"kind": "agent", "value": 2}}
        with patch.object(control_client, "call", side_effect=[initial, ending]):
            result = await monitor._monitor_sessions(
                ["ts-test"],
                until="final",
                baselines=baselines,
                timeout_s=2,
                poll_interval_s=0.5,
            )
        self.assertEqual(baselines["ts-test"]["value"], 3)
        entry = result["completed"][0]
        self.assertEqual(entry["status"], "reported")
        self.assertEqual(entry["interim_turn_ends"], 1)

    async def test_reply_wait_returns_error_or_human_wait_without_assistant_text(self):
        for state, status in [
            ("errored", "errored"),
            ("waiting-on-human", "needs_human"),
        ]:
            with (
                self.subTest(state=state),
                patch.object(
                    control_client, "call", return_value=observed(state, seq=9)
                ),
            ):
                result = await server._await_reply(
                    "ts-test",
                    engine="claude-code",
                    transcript_path=None,
                    anchor_cursor=None,
                    generation=0,
                    deadline=time.monotonic() + 2,
                    interval=0.5,
                    grace=1,
                    agent_baseline={"kind": "agent", "value": 8},
                )
                self.assertTrue(result["completed"])
                self.assertEqual(result["status"], status)
                self.assertIsNone(result["last_message"])
                self.assertEqual(result["agent_state"]["turn_seq"], 9)

    async def test_reply_wait_cannot_reuse_previous_turn(self):
        with patch.object(control_client, "call", return_value=observed("idle", seq=8)):
            result = await server._await_reply(
                "ts-test",
                engine="claude-code",
                transcript_path=None,
                anchor_cursor=None,
                generation=0,
                deadline=time.monotonic() + 0.1,
                interval=0.05,
                grace=1,
                agent_baseline={"kind": "agent", "value": 8},
            )
        self.assertFalse(result["completed"])
        self.assertTrue(result["timed_out"])

    async def test_send_and_wait_uses_sequence_from_before_send(self):
        calls = []

        def call(method, params):
            calls.append(method)
            if method == "send_input":
                return {"ok": True}
            return observed("idle", seq=4 if len(calls) == 1 else 5)

        with patch.object(control_client, "call", side_effect=call):
            result = await server._send_and_await(
                "ts-test",
                "new work",
                True,
                deadline=time.monotonic() + 2,
                interval=0.5,
                grace=1,
            )
        self.assertTrue(result["completed"] and result["delivered"])
        self.assertEqual(
            calls,
            ["resolve_authorized_session", "send_input", "resolve_authorized_session"],
        )

    async def test_missing_transcript_cannot_hide_an_engine_error(self):
        value = dict(
            observed("errored", seq=9), transcript_path="/absent/transcript.jsonl"
        )
        with patch.object(control_client, "call", return_value=value):
            result = await server._await_reply(
                "ts-test",
                engine="claude-code",
                transcript_path=None,
                anchor_cursor=None,
                generation=0,
                deadline=time.monotonic() + 1,
                interval=0.5,
                grace=1,
                agent_baseline={"kind": "agent", "value": 8},
            )
        self.assertEqual(result["status"], "errored")
        self.assertTrue(result["completed"])

    async def test_schema_retry_never_prompts_a_worker_needing_attention(self):
        for status in ("needs_human", "errored", "unknown"):
            with (
                self.subTest(status=status),
                patch.object(
                    server,
                    "_send_and_await",
                    side_effect=AssertionError("unexpected prompt"),
                ),
            ):
                result = await server._settle_schema(
                    "ts-test",
                    {"status": status, "last_message": None},
                    {"type": "object"},
                    retries=2,
                    deadline=time.monotonic() + 1,
                    interval=0.5,
                    grace=1,
                )
            self.assertIsNone(result["result"])
            self.assertTrue(result["schema_error"])

    async def test_source_upgrade_checks_legacy_edge_once_then_adopts_engine_counters(
        self,
    ):
        baselines = {"ts-test": {"kind": "turn", "value": "old-fingerprint"}}
        with (
            patch.object(
                control_client,
                "call",
                side_effect=[observed("working", seq=4), observed("idle", seq=4)],
            ),
            patch.object(
                monitor,
                "last_completed_turn_fingerprint",
                return_value="old-fingerprint",
            ) as legacy,
        ):
            result = await monitor._monitor_sessions(
                ["ts-test"], baselines=baselines, timeout_s=2, poll_interval_s=0.5
            )
        legacy.assert_called_once()
        self.assertEqual(baselines["ts-test"], {"kind": "agent", "value": 3, "ended_at": 20.0, "no_turn_at": None, "legacy": {"kind": "turn", "value": "old-fingerprint"}})
        self.assertEqual(result["completed"][0]["status"], "awaiting_input")

    async def test_first_engine_stop_keeps_the_completion_that_passed_legacy_edge(self):
        baselines = {"ts-test": {"kind": "turn", "value": "old-turn"}}
        with patch.object(control_client, "call", return_value=observed("idle", seq=0)), \
                patch.object(monitor, "last_completed_turn_fingerprint", return_value="new-turn") as legacy:
            result = await monitor._monitor_sessions(["ts-test"], baselines=baselines, timeout_s=1)
        legacy.assert_called_once()
        self.assertEqual(result["completed"][0]["status"], "awaiting_input")
        self.assertEqual(baselines["ts-test"]["kind"], "agent")

    async def test_no_turn_escape_returns_label_and_never_old_reply_or_schema_retry(self):
        initial = observed("idle", seq=5, ended=20.0)
        initial["agent_state"]["detail"] = {"input_pending": True}
        baseline = monitor.baseline_for("claude-code", None, initial)
        self.assertFalse(monitor.engine_turn_complete(initial))
        completed = observed("idle", seq=5, ended=20.0)
        completed["agent_state"]["latest_start_at"] = 25.0
        completed["agent_state"]["detail"] = {"no_turn_at": 45.0}
        completed["transcript_path"] = "/old/transcript.jsonl"
        with patch.object(control_client, "call", return_value=completed),                 patch.object(monitor, "_read_all_messages", side_effect=AssertionError("stale reply read")):
            result = await monitor._monitor_sessions(["ts-test"], baselines={"ts-test":baseline},
                                                    timeout_s=1)
        entry = result["completed"][0]
        self.assertEqual(entry["completion_kind"], "no_turn")
        self.assertIsNone(entry["last_message"])
        self.assertIn("no_turn", "\n".join(async_monitor._entry_lines(entry)))
        self.assertFalse(monitor._edge_passed(monitor.baseline_for("claude-code", None, completed),
                                            "claude-code", None, completed), "same no_turn cannot fire twice")
        out = server._with_outcome({"completed":True, "last_message":{"content":"stale"}}, completed)
        self.assertIsNone(out["last_message"])
        self.assertEqual(out["completion_kind"], "no_turn")
        with patch.object(server, "_send_and_await", side_effect=AssertionError("unexpected schema retry")):
            checked = await server._settle_schema("ts-test", out, {"type":"object"}, retries=2,
                deadline=time.monotonic()+1, interval=.05, grace=0)
        self.assertIsNone(checked["result"])

    async def test_escaped_idle_cannot_complete_new_sequence_or_quote_old_reply(self):
        escaped = observed("idle", seq=5, ended=20.0, source="relay")
        escaped["agent_state"]["latest_start_at"] = 25.0
        baseline = {"kind": "agent", "value": 4, "ended_at": 20.0}
        with patch.object(control_client, "call", return_value=escaped):
            result = await monitor._monitor_sessions(["ts-test"], baselines={"ts-test":baseline},
                                                    timeout_s=.1, poll_interval_s=.05)
            self.assertTrue(result["timed_out"])
            self.assertEqual(result["completed"], [])
            reply = await server._await_reply("ts-test", engine="codex", transcript_path=None,
                anchor_cursor=None, generation=0, deadline=time.monotonic()+.1,
                interval=.05, grace=0, agent_baseline=baseline)
            self.assertTrue(reply["timed_out"])
            self.assertFalse(reply["completed"])
        for state in ["idle", "working-background"]:
            ended = observed(state, seq=5, ended=30.0, source="relay")
            ended["agent_state"]["latest_start_at"] = 25.0
            self.assertTrue(monitor.engine_turn_complete(ended))
            ended["agent_state"]["last_turn"]["ended_at"] = 20.0
            self.assertFalse(monitor.engine_turn_complete(ended))
        completed = observed("idle", seq=5, ended=30.0, source="relay")
        completed["agent_state"]["latest_start_at"] = 25.0
        with patch.object(control_client, "call", side_effect=[escaped, completed]):
            result = await monitor._monitor_sessions(["ts-test"], baselines={"ts-test":baseline},
                                                    timeout_s=2, poll_interval_s=.05)
        self.assertEqual(result["completed"][0]["agent_state"]["last_turn"]["ended_at"], 30.0)

    async def test_brain_rollback_keeps_watch_pinned_to_pre_arm_legacy_turn(self):
        legacy = observed("idle")
        del legacy["agent_state"]
        with patch.object(monitor, "last_completed_turn_fingerprint", return_value="old"):
            baseline = monitor.baseline_for("claude-code", "/fixture/t.jsonl", observed("working"))
        for fingerprint, fires in [("old", False), ("new", True)]:
            with self.subTest(fingerprint=fingerprint), \
                    patch.object(control_client, "call", return_value=legacy), \
                    patch.object(monitor, "last_completed_turn_fingerprint", return_value=fingerprint):
                result = await monitor._monitor_sessions(["ts-test"], baselines={"ts-test":baseline},
                                                        timeout_s=.1, poll_interval_s=.05)
            self.assertEqual(bool(result["completed"]), fires)

    def test_codex_rollback_uses_size_and_missing_transcript_keeps_level_behavior(self):
        initial = observed("idle", source="relay")
        with patch.object(monitor.os.path, "getsize", return_value=10):
            baseline = monitor.baseline_for("codex", "/fixture/t.jsonl", initial)
        for size, passed in [(10, False), (11, True)]:
            with patch.object(monitor.os.path, "getsize", return_value=size):
                self.assertEqual(monitor._edge_passed(baseline, "codex", "/fixture/t.jsonl", {}), passed)
        no_path = monitor.baseline_for("codex", None, initial)
        self.assertTrue(monitor._edge_passed(no_path, "codex", None, {}))

    async def test_unknown_quiet_pty_exception_is_limited_to_single_idle_wait(self):
        value = observed("unknown", idle=True)
        with patch.object(control_client, "call", return_value=value):
            single, many = await asyncio.gather(
                server.wait_for_session_idle("ts-test", timeout_s=1),
                monitor._monitor_sessions(["ts-test"], timeout_s=1, poll_interval_s=.5))
        self.assertTrue(single["idle"])
        self.assertFalse(single["timed_out"])
        self.assertEqual(single["status"], "unknown")
        self.assertEqual(single["idle_source"], "pty")
        self.assertEqual(single["agent_state"]["state"], "unknown")
        self.assertTrue(many["timed_out"])


if __name__ == "__main__":
    unittest.main()
