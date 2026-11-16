"""Owner-as-leader review (2026-10-22): the lead pin changes only through
POST /tasks/{id}/lead. That endpoint stores "owner" in one spelling; a metadata
PATCH keeps the stored pin and refuses a different one; MCP update_task refuses
the key outright."""

from __future__ import annotations

import os
import unittest
from unittest import mock

import pytest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

pytest.importorskip("fastapi")
pytest.importorskip("asyncpg")

from fastapi import HTTPException  # noqa: E402

from api import main as api_main  # noqa: E402
from api.models import TaskLeadUpdate, TaskUpdate  # noqa: E402


class LeadPinApiTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.row = {"id": "t-1", "status": "running", "metadata": {"lead_session_uid": "owner", "filer": {"a": 1}}}
        self.written: dict = {}
        self.lead: list = []

        async def fake_get(_pool, _task_id):
            return self.row

        async def fake_update(_pool, _task_id, **fields):
            self.written = fields
            return {**self.row, **fields}

        async def fake_set_lead(_pool, _task_id, uid):
            self.lead.append(uid)
            return {**self.row, "metadata": {**self.row["metadata"], "lead_session_uid": uid}}

        self._patches = [
            mock.patch.object(api_main.db, "get_task", fake_get),
            mock.patch.object(api_main.db, "update_task", fake_update),
            mock.patch.object(api_main.db, "set_task_lead", fake_set_lead),
        ]
        for p in self._patches:
            p.start()

    async def asyncTearDown(self):
        for p in self._patches:
            p.stop()

    async def test_lead_endpoint_normalises_owner(self):
        for spelling in ["OWNER", " Owner ", "owner"]:
            await api_main.set_task_lead("t-1", TaskLeadUpdate(session_uid=spelling), pool=None)
        await api_main.set_task_lead("t-1", TaskLeadUpdate(session_uid="ts-1-0"), pool=None)
        self.assertEqual(self.lead, ["owner", "owner", "owner", "ts-1-0"])

    async def test_metadata_patch_keeps_the_pin(self):
        await api_main.update_task("t-1", TaskUpdate.model_validate({"metadata": {"resume": {"x": 1}}}), pool=None)
        self.assertEqual(self.written["metadata"], {"resume": {"x": 1}, "lead_session_uid": "owner"})
        await api_main.update_task("t-1", TaskUpdate.model_validate({"metadata": None}), pool=None)
        self.assertEqual(self.written["metadata"], {"lead_session_uid": "owner"}, "clearing the bag keeps the pin")
        # Re-sending the stored pin (read-modify-write) is fine.
        await api_main.update_task("t-1", TaskUpdate.model_validate({"metadata": {"lead_session_uid": "owner"}}), pool=None)

    async def test_metadata_patch_refuses_a_different_pin(self):
        for pin in [None, "ts-9-0", ""]:
            self.written = {}
            with self.assertRaises(HTTPException) as e:
                await api_main.update_task("t-1", TaskUpdate.model_validate({"metadata": {"lead_session_uid": pin}}), pool=None)
            self.assertEqual(e.exception.status_code, 400)
            self.assertIn("/lead", e.exception.detail)
            self.assertEqual(self.written, {}, "nothing written")

    async def test_unpinned_tasks_patch_as_before(self):
        self.row = {"id": "t-1", "status": "running", "metadata": None}
        await api_main.update_task("t-1", TaskUpdate.model_validate({"metadata": {"a": 1}}), pool=None)
        self.assertEqual(self.written["metadata"], {"a": 1})


class UpdateTaskToolTests(unittest.TestCase):
    def test_update_task_refuses_the_lead_key(self):
        from mcp_server import server

        with mock.patch.object(server, "PlanningClient") as client:
            with self.assertRaises(ValueError) as e:
                server.update_task("t-1", metadata={"lead_session_uid": None, "x": 1})
            self.assertIn('org(action="set_lead")', str(e.exception))
            client.assert_not_called()
            client.return_value.update_task.return_value = {"id": "t-1"}
            server.update_task("t-1", metadata={"x": 1})
            client.return_value.update_task.assert_called_once_with("t-1", metadata={"x": 1})


if __name__ == "__main__":
    unittest.main()
