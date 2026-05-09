"""PATCH /tasks/{id} must distinguish "field omitted" from "field set to null".

The handler used to call ``body.model_dump(exclude_none=True)``, which
collapsed both into the same shape — meaning once a nullable column had
a value, no client could clear it back to NULL. Switching to
``model_fields_set`` preserves explicit-null intent so clients can blank
``wip_branch``, ``session_id``, ``difficulty``, ``description``, etc.
"""

from __future__ import annotations

import os
import unittest
from datetime import datetime, timezone
from unittest import mock

import pytest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

# Cloud-only deps — the mcp_server test suite runs in environments
# without the FastAPI stack. Skip cleanly when not available rather
# than failing collection.
pytest.importorskip("fastapi")
pytest.importorskip("asyncpg")

from fastapi import HTTPException  # noqa: E402

from api import main as api_main  # noqa: E402
from api.models import TaskUpdate  # noqa: E402


def _row(**overrides):
    base = {
        "id": "t-1",
        "status": "running",
        "priority": 0,
        "name": "demo",
        "prompt": "p",
        "repo_url": "https://example/x",
        "repo_branch": "main",
        "worker_vm": None,
        "worker_zone": None,
        "ttyd_url": None,
        "blocked_at": None,
        "session_id": "sess-abc",
        "wip_branch": "feature/x",
        "resume_metadata": None,
        "project": None,
        "slug": None,
        "description": None,
        "difficulty": 5,
        "depends": None,
        "source": "user",
        "is_cloud": False,
        "parent_task_id": None,
        "worktree_mode": "inherit",
    }
    base.update(overrides)
    return base


class PatchNullClearTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.captured: dict = {}
        self.existing = _row()

        async def fake_get(_pool, _task_id):
            return self.existing

        async def fake_update(_pool, _task_id, **fields):
            self.captured = fields
            return _row(**fields)

        self._patches = [
            mock.patch.object(api_main.db, "get_task", fake_get),
            mock.patch.object(api_main.db, "update_task", fake_update),
        ]
        for p in self._patches:
            p.start()

    async def asyncTearDown(self):
        for p in self._patches:
            p.stop()

    async def test_explicit_null_clears_wip_branch(self):
        body = TaskUpdate.model_validate({"wip_branch": None})
        result = await api_main.update_task("t-1", body, pool=None)
        self.assertIn("wip_branch", self.captured)
        self.assertIsNone(self.captured["wip_branch"])
        self.assertIsNone(result["wip_branch"])

    async def test_explicit_null_clears_difficulty(self):
        body = TaskUpdate.model_validate({"difficulty": None})
        await api_main.update_task("t-1", body, pool=None)
        self.assertIn("difficulty", self.captured)
        self.assertIsNone(self.captured["difficulty"])

    async def test_explicit_null_clears_session_id(self):
        body = TaskUpdate.model_validate({"session_id": None})
        await api_main.update_task("t-1", body, pool=None)
        self.assertIn("session_id", self.captured)
        self.assertIsNone(self.captured["session_id"])

    async def test_empty_body_is_noop(self):
        body = TaskUpdate.model_validate({})
        result = await api_main.update_task("t-1", body, pool=None)
        # update_task should NOT have been called at all
        self.assertEqual(self.captured, {})
        # Returns the existing row unchanged
        self.assertIs(result, self.existing)

    async def test_omitted_field_is_not_in_update(self):
        # Client sends only wip_branch=null; difficulty must NOT be in the
        # SQL update (omitted ≠ null).
        body = TaskUpdate.model_validate({"wip_branch": None})
        await api_main.update_task("t-1", body, pool=None)
        self.assertNotIn("difficulty", self.captured)
        self.assertNotIn("session_id", self.captured)
        self.assertEqual(set(self.captured.keys()), {"wip_branch"})

    async def test_status_blocked_auto_sets_blocked_at(self):
        body = TaskUpdate.model_validate({"status": "blocked"})
        before = datetime.now(timezone.utc)
        await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(self.captured.get("status"), "blocked")
        self.assertIsInstance(self.captured.get("blocked_at"), datetime)
        self.assertGreaterEqual(self.captured["blocked_at"], before)

    async def test_status_blocked_with_explicit_blocked_at_respected(self):
        # Client provides blocked_at explicitly — don't override it.
        ts = datetime(2020, 1, 1, tzinfo=timezone.utc)
        body = TaskUpdate.model_validate(
            {"status": "blocked", "blocked_at": ts.isoformat()}
        )
        await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(self.captured.get("blocked_at"), ts)

    async def test_status_null_returns_400(self):
        body = TaskUpdate.model_validate({"status": None})
        with self.assertRaises(HTTPException) as ctx:
            await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(ctx.exception.status_code, 400)
        self.assertIn("status", ctx.exception.detail)
        # update_task must not be called
        self.assertEqual(self.captured, {})

    async def test_priority_null_returns_400(self):
        body = TaskUpdate.model_validate({"priority": None})
        with self.assertRaises(HTTPException) as ctx:
            await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(ctx.exception.status_code, 400)
        self.assertIn("priority", ctx.exception.detail)
        self.assertEqual(self.captured, {})

    async def test_prompt_null_returns_400(self):
        # prompt is column-nullable but downstream consumers (cli list,
        # /workers preview, dispatch_daemon) treat it as a string. Null
        # would TypeError elsewhere — defend at the API boundary.
        body = TaskUpdate.model_validate({"prompt": None})
        with self.assertRaises(HTTPException) as ctx:
            await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(ctx.exception.status_code, 400)
        self.assertIn("prompt", ctx.exception.detail)
        self.assertEqual(self.captured, {})

    async def test_repo_branch_null_returns_400(self):
        body = TaskUpdate.model_validate({"repo_branch": None})
        with self.assertRaises(HTTPException) as ctx:
            await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(ctx.exception.status_code, 400)
        self.assertIn("repo_branch", ctx.exception.detail)
        self.assertEqual(self.captured, {})

    async def test_multiple_non_nullable_nulls_listed_in_400(self):
        # If a client sends several non-nullable nulls at once, the
        # error message should name all of them so the client can fix
        # the request in one round-trip.
        body = TaskUpdate.model_validate(
            {"priority": None, "source": None, "is_cloud": None}
        )
        with self.assertRaises(HTTPException) as ctx:
            await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(ctx.exception.status_code, 400)
        for f in ("priority", "source", "is_cloud"):
            self.assertIn(f, ctx.exception.detail)
        self.assertEqual(self.captured, {})

    async def test_mixed_nullable_and_non_nullable_nulls_returns_400(self):
        # A valid clear (wip_branch=null) mixed with an invalid one
        # (priority=null) must still 400 — partial application would
        # leak half the request through.
        body = TaskUpdate.model_validate(
            {"wip_branch": None, "priority": None}
        )
        with self.assertRaises(HTTPException) as ctx:
            await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(ctx.exception.status_code, 400)
        self.assertIn("priority", ctx.exception.detail)
        self.assertNotIn("wip_branch", ctx.exception.detail)
        self.assertEqual(self.captured, {})


class PatchWorkerVmCleanupTests(unittest.IsolatedAsyncioTestCase):
    """Worker VM cleanup branch must keep its existing semantics — only
    fire when status flips to "done", and not be tripped by explicit
    None values on unrelated fields.
    """

    async def asyncSetUp(self):
        self.deletions: list[str] = []
        self.warm_released: list[str] = []
        self.captured: dict = {}
        self.existing = _row(worker_vm="vm-7", status="running")

        async def fake_get(_pool, _task_id):
            return self.existing

        async def fake_update(_pool, _task_id, **fields):
            self.captured = fields
            return _row(worker_vm=self.existing["worker_vm"], **fields)

        async def fake_list_warm(_pool):
            return []

        def fake_spawn(vm_name):
            self.deletions.append(vm_name)

        self._patches = [
            mock.patch.object(api_main.db, "get_task", fake_get),
            mock.patch.object(api_main.db, "update_task", fake_update),
            mock.patch.object(api_main.db, "list_warm_vms", fake_list_warm),
            mock.patch.object(api_main, "_spawn_vm_deletion", fake_spawn),
        ]
        for p in self._patches:
            p.start()

    async def asyncTearDown(self):
        for p in self._patches:
            p.stop()

    async def test_status_done_triggers_vm_deletion(self):
        body = TaskUpdate.model_validate({"status": "done"})
        await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(self.deletions, ["vm-7"])

    async def test_unrelated_null_does_not_trigger_vm_deletion(self):
        # Clearing wip_branch must not cascade into VM teardown.
        body = TaskUpdate.model_validate({"wip_branch": None})
        await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(self.deletions, [])

    async def test_status_running_does_not_trigger_vm_deletion(self):
        body = TaskUpdate.model_validate({"status": "running"})
        await api_main.update_task("t-1", body, pool=None)
        self.assertEqual(self.deletions, [])


if __name__ == "__main__":
    unittest.main()
