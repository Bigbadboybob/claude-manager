import unittest
from datetime import datetime, timedelta, timezone

from dispatch.session_roster_projection import project_activity


class ActivityTests(unittest.TestCase):
    def test_host_observations_and_api_clocks_stay_distinct(self):
        now = datetime(2026, 1, 1, tzinfo=timezone.utc)
        since = now - timedelta(seconds=20)
        out = project_activity("errored", now, since, {
            "source": "hooks", "since": 101.0, "observed_at": 102.0,
            "last_turn": {"ended_at": 100.0, "status": "failed"},
            "detail": {"error_kind": "rate_limit", "resumes_at": 200.0,
                       "retrying": True},
            "terminal_text": "must not be copied",
        }, now)
        self.assertEqual(out["activity"]["state_since_api_clock"], since)
        self.assertEqual(out["activity"]["since_host_clock"], 101.0)
        self.assertEqual(out["activity"]["observed_at_host_clock"], 102.0)
        self.assertEqual(out["limit"]["error_kind"], "rate_limit")
        self.assertEqual(out["limit"]["recovery"], "not_exported")
        self.assertNotIn("terminal_text", repr(out))

    def test_stale_preserves_last_state_and_absent_recovery_is_unknown(self):
        now = datetime(2026, 1, 1, tzinfo=timezone.utc)
        out = project_activity("working", now - timedelta(seconds=91), None, None, now)
        self.assertEqual(out["activity"]["state"], "stale")
        self.assertEqual(out["activity"]["last_known_state"], "working")
        self.assertIsNone(out["limit"]["error_kind"])
        self.assertEqual(out["limit"]["recovery"], "not_exported")

    def test_malformed_host_observations_do_not_become_facts(self):
        now = datetime(2026, 1, 1, tzinfo=timezone.utc)
        out = project_activity("unknown", now, None, {
            "source": ["bad"], "since": float("nan"),
            "last_turn": {"status": {"secret": "value"}, "ended_at": True},
            "detail": {"error_kind": ["bad"], "retrying": 1},
        }, now)
        self.assertIsNone(out["activity"]["source"])
        self.assertIsNone(out["activity"]["since_host_clock"])
        self.assertIsNone(out["activity"]["last_turn_status"])
        self.assertIsNone(out["limit"]["error_kind"])
        self.assertIsNone(out["limit"]["retrying"])

    def test_wait_since_is_host_transition_time_not_poll_or_api_time(self):
        now = datetime(2026, 1, 1, tzinfo=timezone.utc)
        for observed in [1000.0, 1005.0]:
            out = project_activity("waiting-on-human", now, now, {
                "source": "presence", "since": 990.0, "observed_at": observed,
                "detail": {"waiting_for": "permission"},
            }, now)
            self.assertEqual(out["activity"]["state"], "waiting-on-human")
            self.assertEqual(out["activity"]["since_host_clock"], 990.0)
            self.assertEqual(out["activity"]["observed_at_host_clock"], observed)

    def test_engine_start_and_dialog_are_separate_from_input_and_staleness(self):
        now = datetime(2026, 1, 1, tzinfo=timezone.utc)
        raw = {"source": "presence", "latest_start_at": 200.0,
               "engine_turn_started_at": 100.0, "detail": {"dialog_visible": True}}
        fresh = project_activity("waiting-on-human", now, None, raw, now)
        self.assertEqual(fresh["activity"]["engine_turn_started_at_host_clock"], 100.0)
        self.assertIs(fresh["limit"]["dialog_visible"], True)
        stale = project_activity("waiting-on-human", now - timedelta(seconds=91), None, raw, now)
        self.assertIsNone(stale["limit"]["dialog_visible"])
        unknown = project_activity("idle", now, None, {"latest_start_at": 200.0}, now)
        self.assertIsNone(unknown["activity"]["engine_turn_started_at_host_clock"])
        self.assertIsNone(unknown["limit"]["dialog_visible"])
