"""Headless planning READS (list_tasks / get_task / get_current_task).

On a headless host (a daemon-spawned agent with no cli-routed `PlanningClient`)
these route through the DAEMON, which holds the planning-API creds. The daemon
returns RAW task rows; the MCP server filters + shapes them. Pinned here so a
refactor can't silently revert to the cli path that fails headless — the gap
that left bug-fix agents unable to read the board on cm-manager.
"""

from __future__ import annotations

import unittest
from pathlib import Path
from unittest import mock


def _daemon_route():
    from mcp_server import control_client

    return control_client.SocketRoute(Path("/tmp/daemon.sock"), chose_daemon=True)


class HeadlessPlanningReadsTests(unittest.TestCase):
    def test_list_tasks_routes_to_daemon_and_filters_client_side(self):
        from mcp_server import control_client, server

        rows = [
            {"id": "a", "project": "p1", "status": "running", "source": "claude"},
            {"id": "b", "project": "p2", "status": "running", "source": "user"},
            {"id": "c", "project": "p1", "status": "done", "source": "claude"},
        ]
        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["socket"] = kw.get("socket_path")
            return rows

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=_daemon_route()
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            out = server.list_tasks(project="p1", status="running", source="claude")

        self.assertEqual(captured["method"], "list_tasks")
        self.assertIsNotNone(captured["socket"], "must pass explicit socket_path")
        # The daemon returns ALL rows; project+status+source filters apply
        # client-side → only row "a".
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
