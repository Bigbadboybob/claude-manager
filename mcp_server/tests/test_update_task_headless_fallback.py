"""`update_task` headless fallback.

On a headless host the cli-routed `PlanningClient` is unavailable (the `cli`
package isn't deployed alongside the MCP server, and/or CM_API_URL/CM_API_TOKEN
aren't in the agent's env). EVERY field set — status-only included — routes
through the daemon's general `update_task` handler (which holds the planning
creds, column-allowlists + status-validates, and re-reads the row so the return
is a full, uniformly `_shape_task`-shaped task). The status-only case used to
short-circuit to `set_subtask_status` (a bare `{task_id, status}` shape); it no
longer does, so all three environments return the same shape.
"""

from __future__ import annotations

import unittest
from unittest import mock


def _raise_planning_unavailable(*_a, **_k):
    raise RuntimeError(
        "planning tools are unavailable in this deployment: the `cli` package "
        "is not installed alongside the MCP server"
    )


class UpdateTaskHeadlessFallbackTests(unittest.TestCase):
    def test_status_only_routes_to_daemon_update_task(self):
        """A status-only headless update routes through the daemon's general
        `update_task` (not `set_subtask_status`), so its return is the same
        full, `_shape_task`-shaped task as the cli path and the multi-field
        path — no bare `{task_id, status}` special case."""
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"id": "t1", "status": "blocked"}

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            result = server.update_task(task_id="t1", status="blocked")

        self.assertEqual(captured.get("method"), "update_task")
        self.assertEqual(
            captured.get("params"), {"task_id": "t1", "fields": {"status": "blocked"}}
        )
        self.assertEqual(result.get("status"), "blocked")

    def test_non_status_update_routes_to_daemon_update_task(self):
        """A non-status field has no cli path headless, so it routes through the
        daemon's general `update_task` handler (which holds the planning creds)
        — not a re-raise, and not the status-only `set_subtask_status` path."""
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"id": "t1", "name": "new name"}

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            result = server.update_task(task_id="t1", name="new name")

        self.assertEqual(captured.get("method"), "update_task")
        self.assertEqual(
            captured.get("params"), {"task_id": "t1", "fields": {"name": "new name"}}
        )
        self.assertEqual(result.get("name"), "new name")

    def test_status_plus_other_field_routes_to_daemon_update_task(self):
        """status + another field is NOT status-only, so the whole field set
        routes through the daemon's general `update_task`, not
        `set_subtask_status` (which only handles the lone status field)."""
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"id": "t1", "status": "blocked", "priority": 1}

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            server.update_task(task_id="t1", status="blocked", priority=1)

        self.assertEqual(captured.get("method"), "update_task")
        self.assertEqual(
            captured.get("params"),
            {"task_id": "t1", "fields": {"status": "blocked", "priority": 1}},
        )


if __name__ == "__main__":
    unittest.main()
