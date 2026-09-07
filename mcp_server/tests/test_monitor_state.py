import json
import os
import tempfile
import unittest
from unittest import mock

from mcp_server import monitor_state
from mcp_server.notifications import Queue


class MonitorStateTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.env = mock.patch.dict(os.environ, {"HOME": self.tmp.name, "CM_TUI_SESSION_ID": "ts-owner", "CM_MONITOR_TRACKING_V1": "1"})
        self.env.start()
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(self.env.stop)

    def record(self, state="watching", **extra):
        return {"monitor_id": "mon-a", "state": state, "watching": ["worker"], "task": object(), **extra}

    def test_initial_coverage_and_unchanged_publication_are_stable(self):
        self.assertFalse(monitor_state.read()["available"])
        monitor_state.publish({})
        first = monitor_state._path().read_bytes()
        monitor_state.publish({})
        self.assertEqual(first, monitor_state._path().read_bytes())
        self.assertEqual(monitor_state._path().stat().st_mode & 0o777, 0o600)

    def test_new_producer_cannot_erase_old_unfinished_monitors(self):
        with mock.patch.object(monitor_state, "PRODUCER_ID", "old"):
            monitor_state.publish({"mon-a": self.record()})
        with mock.patch.object(monitor_state, "PRODUCER_ID", "new"):
            monitor_state.publish({})
        journal = monitor_state.read()
        self.assertEqual(journal["producers"]["old"]["records"]["mon-a"]["state"], "watching")
        self.assertEqual(journal["producers"]["new"]["records"], {})

    def test_new_producer_reconciles_late_native_receipt_without_replay(self):
        queue = Queue.own()
        queue.publish("monitor:mon-a", "session_monitor", "result", "marker")
        queue.change("monitor:mon-a", {"pending"}, status="submitted")
        with mock.patch.object(monitor_state, "PRODUCER_ID", "old"):
            monitor_state.publish({"mon-a": self.record(
                "interrupted", notification_id="monitor:mon-a", result={"completed": ["worker"]})})
        with mock.patch.object(monitor_state, "PRODUCER_ID", "new"):
            monitor_state.publish({})
            record = monitor_state.read()["producers"]["old"]["records"]["mon-a"]
            self.assertTrue(record["delivery_uncertain"])
            self.assertEqual(record["state"], "interrupted")
            queue.change("monitor:mon-a", {"submitted"}, status="observed")
            monitor_state.publish({})
        record = monitor_state.read()["producers"]["old"]["records"]["mon-a"]
        self.assertFalse(record["delivery_uncertain"])
        self.assertEqual(record["state"], "delivered")
        self.assertEqual(record["result"], {"completed": ["worker"]})
        self.assertEqual(len(queue.snapshot()["notifications"]), 1)

    def test_cancel_after_native_claim_remains_a_drain_obligation(self):
        queue = Queue.own()
        queue.publish("monitor:mon-a", "session_monitor", "result", "marker")
        queue.change("monitor:mon-a", {"pending"}, status="submitting")
        queue.cancel("monitor:mon-a")
        monitor_state.publish({"mon-a": self.record(
            "cancelled", notification_id="monitor:mon-a", delivery_uncertain=False)})
        record = monitor_state.read()["producers"][monitor_state.PRODUCER_ID]["records"]["mon-a"]
        self.assertTrue(record["delivery_uncertain"])
        self.assertEqual(record["delivery_status"], "submitting")

    def test_pruning_retains_unverified_delivery_and_result(self):
        record = self.record("undelivered", result={"completed": ["worker"]}, delivery_uncertain=True)
        monitor_state.publish({"mon-a": record})
        monitor_state.publish({})
        retained = monitor_state.read()["producers"][monitor_state.PRODUCER_ID]["records"]["mon-a"]
        self.assertNotIn("task", retained)
        self.assertEqual(retained["result"], record["result"])

    def test_verified_terminal_history_can_be_pruned(self):
        monitor_state.publish({"mon-a": self.record("delivered")})
        monitor_state.publish({})
        self.assertEqual(monitor_state.read()["producers"][monitor_state.PRODUCER_ID]["records"], {})

    def test_failed_replace_keeps_previous_pending_obligation(self):
        monitor_state.publish({"mon-a": self.record()})
        before = monitor_state._path().read_bytes()
        with mock.patch.object(os, "replace", side_effect=OSError("disk unavailable")):
            with self.assertRaises(OSError):
                monitor_state.publish({"mon-a": self.record("delivered")})
        self.assertEqual(monitor_state._path().read_bytes(), before)
        self.assertEqual(list(monitor_state._path().parent.glob("*.tmp")), [])

    def test_corrupt_or_wrong_owner_journal_is_not_replaced_with_empty_state(self):
        path = monitor_state._path()
        path.parent.mkdir(parents=True)
        for content in ["broken", json.dumps({"schema_version": 1, "session_uid": "other", "producers": {}})]:
            path.write_text(content)
            with self.assertRaises(ValueError):
                monitor_state.publish({})
            self.assertEqual(path.read_text(), content)

    def test_unsafe_owner_cannot_escape_journal_directory(self):
        with mock.patch.dict(os.environ, {"CM_TUI_SESSION_ID": "../escape"}):
            with self.assertRaises(ValueError):
                monitor_state.publish({})

    def test_legacy_reconnect_cannot_turn_unknown_history_into_empty_coverage(self):
        with mock.patch.dict(os.environ, {"CM_MONITOR_TRACKING_V1": "0"}):
            monitor_state.publish({})
        self.assertEqual(monitor_state.read()["coverage_version"], 0)
        with mock.patch.object(monitor_state, "PRODUCER_ID", "restarted"):
            monitor_state.publish({})
        self.assertEqual(monitor_state.read()["coverage_version"], 0)

    def test_tracked_journal_retains_coverage_and_obligations_across_resume(self):
        monitor_state.publish({"mon-a": self.record()})
        with mock.patch.dict(os.environ, {"CM_MONITOR_TRACKING_V1": "0"}), mock.patch.object(monitor_state, "PRODUCER_ID", "resumed"):
            monitor_state.publish({})
        self.assertEqual(monitor_state.read()["coverage_version"], 1)
        self.assertEqual(monitor_state.read()["producers"][monitor_state.PRODUCER_ID]["records"]["mon-a"]["state"], "watching")
