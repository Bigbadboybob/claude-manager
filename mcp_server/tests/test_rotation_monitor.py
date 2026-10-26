import unittest
from unittest import mock

from mcp_server import monitor


class RotationGraceTests(unittest.TestCase):
    """#24: a transcript rotation stops a session and respawns it at the
    same uid; a monitor must not fire that stop as the worker's exit."""

    def test_rotation_kill_is_not_done_within_the_grace(self):
        seen = {}
        rotated = {"killed_by": monitor.ROTATION_KILLER}
        with mock.patch.object(monitor.time, "monotonic", return_value=1000.0):
            self.assertTrue(monitor._rotating("ts-a", rotated, seen))
        with mock.patch.object(monitor.time, "monotonic", return_value=1000.0 + monitor.ROTATION_GRACE_S - 1):
            self.assertTrue(monitor._rotating("ts-a", rotated, seen))
        with mock.patch.object(monitor.time, "monotonic", return_value=1000.0 + monitor.ROTATION_GRACE_S + 1):
            self.assertFalse(monitor._rotating("ts-a", rotated, seen), "a rotation that never came back is an exit")

    def test_one_turn_is_absorbed_per_rotation_after_arming(self):
        absorbed = {}
        before = {"rotation": {"started_at": 50.0}}
        self.assertFalse(monitor._rotation_turn("ts-a", before, 100.0, absorbed), "rotated before the watch")
        after = {"rotation": {"started_at": 150.0}}
        self.assertTrue(monitor._rotation_turn("ts-a", after, 100.0, absorbed))
        self.assertFalse(monitor._rotation_turn("ts-a", after, 100.0, absorbed), "only the notice's turn")
        self.assertTrue(monitor._rotation_turn("ts-a", {"rotation": {"started_at": 300.0}}, 100.0, absorbed))
        self.assertFalse(monitor._rotation_turn("ts-a", {}, 100.0, absorbed))

    def test_an_engine_switch_absorbs_its_two_turns(self):
        absorbed = {}
        switch = {"handoff": {"started_at": 200.0, "kind": "switch"}}
        self.assertTrue(monitor._rotation_turn("ts-s", switch, 100.0, absorbed), "the handoff document")
        self.assertTrue(monitor._rotation_turn("ts-s", switch, 100.0, absorbed), "the takeover")
        self.assertFalse(monitor._rotation_turn("ts-s", switch, 100.0, absorbed), "then real work")
        repick = {"handoff": {"started_at": 300.0, "kind": "repick"}}
        self.assertTrue(monitor._rotation_turn("ts-s", repick, 100.0, absorbed))
        self.assertFalse(monitor._rotation_turn("ts-s", repick, 100.0, absorbed))

    def test_a_handoff_stop_gets_the_same_grace(self):
        with mock.patch.object(monitor.time, "monotonic", return_value=5.0):
            self.assertTrue(monitor._rotating("ts-h", {"killed_by": monitor.HANDOFF_KILLER}, {}))

    def test_other_exits_are_exits_and_reset_the_clock(self):
        seen = {"ts-a": 1.0}
        self.assertFalse(monitor._rotating("ts-a", {"killed_by": "operator"}, seen))
        self.assertNotIn("ts-a", seen)
        self.assertFalse(monitor._rotating("ts-b", {}, seen))


class RotateTranscriptToolTests(unittest.TestCase):
    def test_tool_routes_to_the_daemon_with_its_params(self):
        from mcp_server import control_client, server

        self.assertIn("session.rotate_transcript", control_client.DAEMON_METHODS)
        with mock.patch.object(control_client, "call", return_value={"queued": True}) as call:
            fn = getattr(server.rotate_transcript, "fn", server.rotate_transcript)
            self.assertEqual(fn(reason="lag"), {"queued": True})
            call.assert_called_with("session.rotate_transcript", {"reason": "lag"})
            fn(status=True)
            call.assert_called_with("session.rotate_transcript", {"reason": "", "status": True})


    def test_handoff_tool_sends_only_what_was_given(self):
        from mcp_server import control_client, server

        self.assertIn("session.handoff", control_client.DAEMON_METHODS)
        with mock.patch.object(control_client, "call", return_value={"queued": True}) as call:
            fn = getattr(server.request_handoff, "fn", server.request_handoff)
            fn(to_engine="codex", to_model=" gpt-6-luna ", reason="cheap work")
            call.assert_called_with(
                "session.handoff", {"to_engine": "codex", "to_model": "gpt-6-luna", "reason": "cheap work"})
            fn(status=True)
            call.assert_called_with("session.handoff", {"status": True})


if __name__ == "__main__":
    unittest.main()
