"""`update_task(parent_task_id=...)` on the direct-API route.

The cli-routed branch writes straight to the planning API, so it never
passed through the daemon's descendant-scope check: an agent could move a
foreign task into its own tree, or hang its tasks under someone else's. A
reparent now asks the daemon (`task.scope_check`) first: both the task and
its new parent must be in the caller's tree (or the caller is global), and
an unanswerable check refuses the move.
"""

from __future__ import annotations

import unittest
from unittest import mock


class _FakeClient:
    def __init__(self):
        self.updates: list = []

    def update_task(self, task_id, **fields):
        self.updates.append((task_id, fields))
        return {"id": task_id, **fields}


class UpdateTaskReparentScopeTests(unittest.TestCase):
    def _run(self, in_scope, **kwargs):
        from mcp_server import server, control_client

        client = _FakeClient()
        calls: list = []

        def fake_call(method, params, **_kw):
            calls.append((method, params))
            if isinstance(in_scope, Exception):
                raise in_scope
            return {"global": False, "in_scope": in_scope}

        with mock.patch.object(server, "PlanningClient", return_value=client), \
                mock.patch.object(control_client, "call", side_effect=fake_call):
            try:
                result = server.update_task(**kwargs)
            except ValueError as exc:
                return None, str(exc), client, calls
        return result, None, client, calls

    def test_reparent_into_a_foreign_tree_is_refused(self):
        _, err, client, calls = self._run(
            {"sub-a": True, "foreign": False}, task_id="sub-a", parent_task_id="foreign"
        )
        self.assertIn("foreign not the caller's task", err)
        self.assertEqual(client.updates, [], "the API is never written")
        self.assertEqual(calls, [("task.scope_check", {"task_ids": ["sub-a", "foreign"]})])

    def test_pulling_a_foreign_task_in_is_refused(self):
        _, err, client, _ = self._run(
            {"foreign": False, "orch": True}, task_id="foreign", parent_task_id="orch"
        )
        self.assertIn("foreign", err)
        self.assertEqual(client.updates, [])

    def test_a_move_within_the_tree_goes_through(self):
        result, err, client, _ = self._run(
            {"sub-a": True, "sub-b": True}, task_id="sub-a", parent_task_id="sub-b"
        )
        self.assertIsNone(err)
        self.assertEqual(client.updates, [("sub-a", {"parent_task_id": "sub-b"})])
        self.assertEqual(result["id"], "sub-a")

    def test_an_unanswerable_check_refuses(self):
        _, err, client, _ = self._run(
            RuntimeError("no daemon"), task_id="sub-a", parent_task_id="sub-b"
        )
        self.assertIn("scope check unavailable", err)
        self.assertEqual(client.updates, [])

    def test_detach_and_other_fields_need_no_check(self):
        _, err, client, calls = self._run({}, task_id="sub-a", parent_task_id="null", name="x")
        self.assertIsNone(err)
        self.assertEqual(calls, [])
        self.assertEqual(client.updates, [("sub-a", {"name": "x", "parent_task_id": None})])


if __name__ == "__main__":
    unittest.main()
