"""Sidebar tools are headless and preserve queued/observed status and auth errors."""
import unittest
from unittest import mock
from pathlib import Path
from mcp_server import control_client, server

class SidebarToolsTests(unittest.TestCase):
    def test_local_and_cloud_route_to_daemon_without_tui(self):
        for env in [{"CM_DAEMON_SOCKET":"/tmp/local-daemon.sock", "CM_TUI_SOCKET":"/missing/tui.sock"}, {"CM_DAEMON_SOCKET":"/tmp/cloud-daemon.sock", "CM_TUI_SOCKET":"/missing/tui.sock"}]:
            with mock.patch.dict("os.environ", env, clear=True):
                for method in ["sidebar.list", "sidebar.assign", "sidebar.publish"]:
                    route = control_client.resolve_socket_for_method(method)
                    self.assertEqual(route, Path(env["CM_DAEMON_SOCKET"]))
    def test_tool_preserves_status_choices_and_errors(self):
        with mock.patch.object(control_client, "call", return_value={"status":"queued"}) as call:
            for choice in ["auto", "none", "sec-123", "Swarm Design"]:
                self.assertEqual(server.set_session_section(choice, "scout"), {"status":"queued"})
                call.assert_called_with("sidebar.assign", {"section":choice,"session_id":"scout"})
            server.set_session_section("auto")
            call.assert_called_with("sidebar.assign", {"section":"auto"})
            server.list_sidebar_sections()
            call.assert_called_with("sidebar.list", {})
        with mock.patch.object(control_client,"call",side_effect=control_client.ControlError("unauthorized","outside task tree")):
            with self.assertRaises(control_client.ControlError):
                server.set_session_section("none","other")
