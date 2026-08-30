"""Regression coverage for planning API lookup/list/error failures.

The production incident combined two boundaries: PostgreSQL rejected an
8-character display prefix as a UUID, and daemon-routed list_tasks fetched the
whole board before applying filters.  These HTTP-level tests pin the task-ID
contract and verify list/error responses are complete JSON frames.
"""

from __future__ import annotations

import json
import os
import unittest
import uuid
from datetime import datetime, timezone
from pathlib import Path
from unittest import mock

import pytest

os.environ.setdefault("CM_DB_DSN", "postgres://stub")
os.environ.setdefault("CM_API_TOKEN", "stub")

pytest.importorskip("fastapi")
pytest.importorskip("asyncpg")
httpx = pytest.importorskip("httpx")

from api import main as api_main  # noqa: E402
from cli.planning_client import _response_json  # noqa: E402


TASK_A = "123e4567-e89b-12d3-a456-426614174000"
TASK_B = "123e4567-e89b-12d3-a456-426614174001"
TASK_C = "123e4567-e89b-12d3-a456-426614174002"


def _task(task_id: str, project: str, status: str, *, prompt_size: int = 32) -> dict:
    now = datetime(2026, 8, 30, 17, 30, tzinfo=timezone.utc)
    return {
        "id": task_id,
        "created_at": now,
        "updated_at": now,
        "repo_url": "git@example.com:owner/repo.git",
        "repo_branch": "main",
        "name": f"{project}-{status}",
        "prompt": "p" * prompt_size,
        "status": status,
        "priority": 0,
        "worker_vm": None,
        "worker_zone": None,
        "ttyd_url": None,
        "blocked_at": None,
        "session_id": None,
        "wip_branch": None,
        "resume_metadata": None,
        "project": project,
        "slug": f"{project.lower()}-{status}",
        "description": "realistic planning row",
        "difficulty": 4,
        "depends": [],
        "source": "claude",
        "is_cloud": False,
        "kind": "oneshot",
        "parent_task_id": None,
        "worktree_mode": "branch",
        "metadata": {"resume": {"designer_session_uid": "ts-test"}},
    }


class PlanningApiFailureTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        api_main.app.dependency_overrides[api_main.verify_token] = lambda: True
        api_main.app.dependency_overrides[api_main.get_pool] = lambda: None
        self.transport = httpx.ASGITransport(
            app=api_main.app, raise_app_exceptions=False
        )
        self.client = httpx.AsyncClient(
            transport=self.transport, base_url="http://planning.test"
        )

    async def asyncTearDown(self):
        await self.client.aclose()
        api_main.app.dependency_overrides.clear()

    async def test_get_short_id_is_structured_400(self):
        response = await self.client.get("/tasks/b95e6607")

        self.assertEqual(response.status_code, 400)
        self.assertEqual(response.headers["content-type"], "application/json")
        self.assertIn("full UUID", response.json()["detail"])
        self.assertIn("short prefixes are not accepted", response.json()["detail"])

    async def test_patch_short_id_is_structured_400(self):
        response = await self.client.patch(
            "/tasks/b95e6607", json={"status": "done"}
        )

        self.assertEqual(response.status_code, 400)
        self.assertIn("full UUID", response.json()["detail"])

    async def test_filtered_realistic_task_mix_has_complete_http_body(self):
        rows = [
            _task(TASK_A, "predictionTrading", "running", prompt_size=24_000),
            _task(TASK_B, "predictionTrading", "done", prompt_size=18_000),
            _task(TASK_C, "anotherProject", "running", prompt_size=12_000),
        ]
        captured: dict = {}

        async def fake_list(_pool, *, status=None, project=None, include_archived=False):
            captured.update(
                status=status,
                project=project,
                include_archived=include_archived,
            )
            return [
                row
                for row in rows
                if (status is None or row["status"] == status)
                and (project is None or row["project"] == project)
            ]

        with mock.patch.object(api_main.db, "list_tasks", fake_list):
            response = await self.client.get(
                "/tasks",
                params={"project": "predictionTrading", "status": "running"},
            )

        self.assertEqual(response.status_code, 200, response.text)
        self.assertEqual(
            captured,
            {
                "status": "running",
                "project": "predictionTrading",
                "include_archived": False,
            },
        )
        self.assertEqual([row["id"] for row in response.json()], [TASK_A])
        # Capture the raw bytes at the HTTP boundary: the declared length and
        # actual body agree, and the final byte closes the JSON array.
        self.assertEqual(len(response.content), int(response.headers["content-length"]))
        self.assertTrue(response.content.endswith(b"]"), response.content[-20:])
        json.loads(response.content)

    async def test_response_model_failure_is_structured_json_500(self):
        async def malformed_rows(*_args, **_kwargs):
            return [{"id": TASK_A, "project": "predictionTrading"}]

        with mock.patch.object(api_main.db, "list_tasks", malformed_rows):
            response = await self.client.get(
                "/tasks",
                params={"project": "predictionTrading", "status": "running"},
            )

        self.assertEqual(response.status_code, 500)
        self.assertEqual(response.json()["error"], "response_serialization_failed")
        self.assertIn("serialize", response.json()["detail"])

    async def test_unhandled_lookup_failure_is_structured_json_500(self):
        async def broken_lookup(_pool, _task_id):
            raise RuntimeError("injected database failure")

        with mock.patch.object(api_main.db, "get_task", broken_lookup):
            response = await self.client.get(f"/tasks/{TASK_A}")

        self.assertEqual(response.status_code, 500)
        self.assertEqual(response.json()["error"], "internal_error")
        self.assertTrue(response.json()["detail"])


class PlanningClientErrorTests(unittest.TestCase):
    def test_mcp_client_surfaces_structured_api_detail(self):
        response = httpx.Response(
            400,
            request=httpx.Request("PATCH", "http://planning.test/tasks/b95e6607"),
            json={
                "detail": "task_id must be a full UUID; short prefixes are not accepted"
            },
        )

        with self.assertRaises(RuntimeError) as ctx:
            _response_json(response)
        self.assertIn("planning API returned 400", str(ctx.exception))
        self.assertIn("full UUID", str(ctx.exception))


class McpPlanningEndToEndTests(unittest.TestCase):
    """Create one API task, then exercise the three requested MCP verbs."""

    def test_create_get_status_flip_and_filtered_list(self):
        from cli.planning_client import PlanningClient
        from mcp_server import control_client, server

        tasks: dict[str, dict] = {}
        now = datetime(2026, 8, 30, 17, 30, tzinfo=timezone.utc)

        async def add_task(
            _pool, repo_url, repo_branch, prompt, priority, **fields
        ):
            task_id = str(uuid.uuid4())
            row = {
                "id": task_id,
                "created_at": now,
                "updated_at": now,
                "repo_url": repo_url,
                "repo_branch": repo_branch,
                "name": fields.get("name"),
                "prompt": prompt,
                "status": fields.get("status", "backlog"),
                "priority": priority,
                "worker_vm": None,
                "worker_zone": None,
                "ttyd_url": None,
                "blocked_at": None,
                "session_id": None,
                "wip_branch": fields.get("wip_branch"),
                "resume_metadata": None,
                "project": fields.get("project"),
                "slug": fields.get("slug"),
                "description": fields.get("description"),
                "difficulty": fields.get("difficulty"),
                "depends": fields.get("depends") or [],
                "source": fields.get("source", "user"),
                "is_cloud": fields.get("is_cloud", False),
                "kind": fields.get("kind", "oneshot"),
                "parent_task_id": fields.get("parent_task_id"),
                "worktree_mode": fields.get("worktree_mode", "inherit"),
                "metadata": fields.get("metadata"),
            }
            tasks[task_id] = row
            return row

        async def get_task(_pool, task_id):
            canonical = api_main.db.normalize_task_id(task_id)
            return tasks.get(canonical)

        async def update_task(_pool, task_id, **fields):
            canonical = api_main.db.normalize_task_id(task_id)
            row = tasks.get(canonical)
            if row is None:
                return None
            row.update(fields)
            row["updated_at"] = datetime.now(timezone.utc)
            return row

        async def list_tasks(
            _pool, *, status=None, project=None, include_archived=False
        ):
            return [
                row
                for row in tasks.values()
                if (status is None or row["status"] == status)
                and (project is None or row["project"] == project)
                and (include_archived or row["status"] != "archived")
            ]

        api_main.app.dependency_overrides[api_main.verify_token] = lambda: True
        api_main.app.dependency_overrides[api_main.get_pool] = lambda: None
        from fastapi.testclient import TestClient

        test_client = TestClient(api_main.app)
        planning = PlanningClient(api_url="http://testserver", api_token="stub")
        planning._client.client = test_client
        local_route = control_client.SocketRoute(
            Path("/tmp/not-used.sock"), chose_daemon=False
        )

        patches = (
            mock.patch.object(api_main.db, "add_task", add_task),
            mock.patch.object(api_main.db, "get_task", get_task),
            mock.patch.object(api_main.db, "update_task", update_task),
            mock.patch.object(api_main.db, "list_tasks", list_tasks),
            mock.patch.object(server, "PlanningClient", return_value=planning),
            mock.patch.object(
                control_client, "resolve_socket_route", return_value=local_route
            ),
        )
        try:
            for patcher in patches:
                patcher.start()

            created_response = test_client.post(
                "/tasks",
                json={
                    "repo_url": "git@example.com:owner/predictionTrading.git",
                    "repo_branch": "main",
                    "project": "predictionTrading",
                    "name": "planning API regression task",
                    "prompt": "verify MCP planning verbs",
                    "status": "running",
                    "source": "claude",
                },
            )
            self.assertEqual(created_response.status_code, 200, created_response.text)
            task_id = created_response.json()["id"]

            fetched = server.get_task(task_id)
            self.assertEqual(fetched["id"], task_id)
            self.assertEqual(fetched["status"], "running")

            updated = server.update_task(task_id=task_id, status="done")
            self.assertEqual(updated["id"], task_id)
            self.assertEqual(updated["status"], "done")

            filtered = server.list_tasks(
                project="predictionTrading", status="done", source="claude"
            )
            self.assertEqual([row["id"] for row in filtered], [task_id])
        finally:
            for patcher in reversed(patches):
                patcher.stop()
            test_client.close()
            api_main.app.dependency_overrides.clear()


if __name__ == "__main__":
    unittest.main()
