"""`update_task` headless fallback.

On a headless host the cli-routed `PlanningClient` is unavailable (the `cli`
package isn't deployed alongside the MCP server, and/or CM_API_URL/CM_API_TOKEN
aren't in the agent's env). A STATUS-ONLY `update_task` must fall back to the
daemon-routed `set_subtask_status`; any other field has no headless path and
must re-raise the original error.
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
    def test_status_only_falls_back_to_set_subtask_status(self):
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"task_id": "t1", "status": "blocked"}

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            result = server.update_task(task_id="t1", status="blocked")

        self.assertEqual(captured.get("method"), "set_subtask_status")
        self.assertEqual(
            captured.get("params"), {"status": "blocked", "task_id": "t1"}
        )
        self.assertEqual(result.get("status"), "blocked")

    def test_non_status_update_reraises_when_planning_unavailable(self):
        """A field other than status can't be set headless — re-raise the
        PlanningClient error rather than silently dropping it or mis-routing."""
        from mcp_server import server, control_client

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(control_client, "call") as call_mock:
            with self.assertRaises(RuntimeError):
                server.update_task(task_id="t1", name="new name")
            call_mock.assert_not_called()

    def test_status_plus_other_field_does_not_fall_back(self):
        """status + another field is NOT status-only → no fallback, re-raise
        (the daemon path only handles status)."""
        from mcp_server import server, control_client

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(control_client, "call") as call_mock:
            with self.assertRaises(RuntimeError):
                server.update_task(task_id="t1", status="blocked", priority=1)
            call_mock.assert_not_called()


if __name__ == "__main__":
    unittest.main()
