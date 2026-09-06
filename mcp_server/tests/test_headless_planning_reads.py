"""Headless planning READS (list_projects / list_tasks / get_task / get_current_task).

On a headless host (a daemon-spawned agent with no cli-routed `PlanningClient`)
these route through the DAEMON, which holds the planning-API creds. The daemon
returns RAW task rows; the MCP server filters + shapes them. Pinned here so a
refactor can't silently revert to the cli path that fails headless — the gap
that left bug-fix agents unable to read the board on cm-manager.
"""

from __future__ import annotations

import os
import unittest
from pathlib import Path
from unittest import mock


def _daemon_route():
    from mcp_server import control_client

    return control_client.SocketRoute(Path("/tmp/daemon.sock"), chose_daemon=True)


class HeadlessPlanningReadsTests(unittest.TestCase):
    def test_list_projects_uses_daemon_without_api_env_or_cli(self):
        from mcp_server import control_client, server

        rows = [{"name": "predictionTrading", "repo_url": "https://example.com/trading"}]
        with mock.patch.dict(
            os.environ, {"CM_DAEMON_SOCKET": "/tmp/daemon.sock"}, clear=True
        ), mock.patch.object(
            server, "PlanningClient", side_effect=RuntimeError("cli unavailable")
        ) as client, mock.patch.object(control_client, "call", return_value=rows) as call:
            self.assertEqual(server.list_projects(), rows)

        client.assert_not_called()
        call.assert_called_once_with(
            "list_projects", {}, socket_path=Path("/tmp/daemon.sock")
        )

    def test_list_projects_without_daemon_uses_direct_client(self):
        from mcp_server import control_client, server

        rows = [{"name": "p1", "repo_url": "https://example.com/p1"}]
        with mock.patch.dict(
            os.environ, {"CM_TUI_SOCKET": "/tmp/tui.sock"}, clear=True
        ), mock.patch.object(server, "PlanningClient") as client, mock.patch.object(
            control_client, "call"
        ) as call:
            client.return_value.list_projects.return_value = rows
            self.assertEqual(server.list_projects(), rows)

        client.return_value.list_projects.assert_called_once_with()
        call.assert_not_called()

    def test_list_projects_surfaces_daemon_error_without_direct_fallback(self):
        from mcp_server import control_client, server

        error = control_client.ControlError("internal", "planning API transport failure")
        with mock.patch.dict(
            os.environ, {"CM_DAEMON_SOCKET": "/tmp/daemon.sock"}, clear=True
        ), mock.patch.object(server, "PlanningClient") as client, mock.patch.object(
            control_client, "call", side_effect=error
        ):
            with self.assertRaises(control_client.ControlError) as raised:
                server.list_projects()

        self.assertIs(raised.exception, error)
        client.assert_not_called()

    def test_list_tasks_routes_to_daemon_with_server_side_filters(self):
        from mcp_server import control_client, server

        rows = [
            {"id": "a", "project": "p1", "status": "running", "source": "claude"},
            {"id": "b", "project": "p2", "status": "running", "source": "user"},
            {"id": "c", "project": "p1", "status": "done", "source": "claude"},
        ]
        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            captured["socket"] = kw.get("socket_path")
            return [rows[0]]

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=_daemon_route()
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            out = server.list_tasks(project="p1", status="running", source="claude")

        self.assertEqual(captured["method"], "list_tasks")
        self.assertEqual(
            captured["params"], {"project": "p1", "status": "running"}
        )
        self.assertIsNotNone(captured["socket"], "must pass explicit socket_path")
        # Project/status were applied by the HTTP API; source remains local.
        self.assertEqual([t["id"] for t in out], ["a"])

    def test_get_task_routes_to_daemon(self):
        from mcp_server import control_client, server

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"id": "t1", "status": "running"}

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=_daemon_route()
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            out = server.get_task("t1")

        self.assertEqual(captured["method"], "get_task")
        self.assertEqual(captured["params"], {"task_id": "t1"})
        self.assertEqual(out["id"], "t1")

    def test_get_current_task_uses_ping_then_get_task(self):
        from mcp_server import control_client, server

        calls: list[str] = []

        def fake_call(method, params, **kw):
            calls.append(method)
            if method == "ping":
                return {"task_id": "t9", "workspace_id": "ws9"}
            if method == "get_task":
                return {"id": "t9", "status": "blocked"}
            raise AssertionError(f"unexpected method {method}")

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=_daemon_route()
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            out = server.get_current_task()

        self.assertEqual(calls, ["ping", "get_task"])
        self.assertEqual(out["workspace_id"], "ws9")
        self.assertEqual(out["task"]["id"], "t9")
        self.assertFalse(out["is_tombstone"])

    def test_get_current_task_taskless_returns_none_without_get_task(self):
        from mcp_server import control_client, server

        def fake_call(method, params, **kw):
            if method == "ping":
                return {"task_id": None, "workspace_id": "ws9"}
            raise AssertionError("get_task must not be called for a taskless caller")

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=_daemon_route()
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            out = server.get_current_task()

        self.assertIsNone(out["task"])
        self.assertEqual(out["workspace_id"], "ws9")


if __name__ == "__main__":
    unittest.main()
