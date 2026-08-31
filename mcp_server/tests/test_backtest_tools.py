"""Cloud auto-backtest MCP verbs: `submit_backtest` + `get_backtest_result`.

Both tools are two-branch routed like `update_task`: the cli-routed
PlanningClient when constructible, else the daemon proxy methods
(`backtest.submit` / `backtest.result`) which hold the planning creds on
headless hosts. `run_key` is minted SERVER-SIDE by the API inside POST
/tasks — both branches only read it back off the created row.
"""

from __future__ import annotations

import asyncio
import re
import unittest
from unittest import mock


def _raise_planning_unavailable(*_a, **_k):
    raise RuntimeError(
        "planning tools are unavailable in this deployment: the `cli` package "
        "is not installed alongside the MCP server"
    )


_RUN_KEY_RE = re.compile(r"^\d{8}-[a-z0-9-]{1,24}-[0-9a-f-]{8}$")


class _FakePlanningClient:
    """Records create_task/update_task bodies; returns canned rows."""

    projects = [{"name": "predictionTrading", "repo_url": "https://github.com/x/pt"}]

    def __init__(self):
        self.created: dict | None = None

    def list_projects(self):
        return self.projects

    def create_task(self, body):
        self.created = body
        meta = dict(body.get("metadata") or {})
        bt = dict(meta.get("backtest") or {})
        bt["run_key"] = "20260707-smoke-abcd1234"
        meta["backtest"] = bt
        return {
            "id": "abcd1234-0000-0000-0000-000000000000",
            "status": body.get("status", "backlog"),
            "metadata": meta,
        }


class SubmitBacktestLaptopTests(unittest.TestCase):
    """The PlanningClient (cli-routed) branch."""

    def _submit(self, fake, **kwargs):
        from mcp_server import server

        with mock.patch.object(server, "PlanningClient", return_value=fake), \
             mock.patch.object(server, "_caller_filer_context", return_value={
                 "schema_version": 1,
                 "agent": "codex",
                 "session_id": "session-1",
                 "task_id": "parent-1",
                 "workspace_id": "workspace-1",
                 "worktree_path": "/worktrees/feature",
                 "machine": "laptop-1",
                 "continuous_task_id": "continuous-1",
                 "workflow_run_id": "workflow-1",
                 "workflow_role": "worker",
                 "managed_by_session_id": "orchestrator-1",
                 "submitted_via": "mcp.submit_backtest",
             }):
            defaults = dict(branch="main", config="configs/smoke.yaml", label="smoke")
            defaults.update(kwargs)
            return server.submit_backtest(**defaults)

    def test_create_body_shape(self):
        fake = _FakePlanningClient()
        result = self._submit(fake)

        body = fake.created
        self.assertEqual(body["kind"], "backtest")
        self.assertTrue(body["is_cloud"])
        self.assertEqual(body["status"], "backlog")
        self.assertEqual(body["source"], "claude")
        self.assertEqual(body["repo_url"], "https://github.com/x/pt")
        self.assertEqual(body["repo_branch"], "main")
        self.assertEqual(body["parent_task_id"], "parent-1")
        bt = body["metadata"]["backtest"]
        self.assertEqual(bt["branch"], "main")
        self.assertEqual(bt["config"], "configs/smoke.yaml")
        self.assertEqual(bt["script"], "analysis.backtests.backtest_actrader_grid")
        vm = body["metadata"]["vm"]
        self.assertEqual(vm["project"], "prediction-market-scalper")
        self.assertEqual(vm["zone"], "us-east4-a")
        self.assertEqual(vm["machine_type"], "n2-standard-4")
        self.assertEqual(vm["image_family"], "cm-backtest-worker")
        filer = body["metadata"]["filer"]
        self.assertEqual(filer["schema_version"], 1)
        self.assertEqual(filer["agent"], "codex")
        self.assertEqual(filer["session_id"], "session-1")
        self.assertEqual(filer["task_id"], "parent-1")
        self.assertEqual(filer["workspace_id"], "workspace-1")
        self.assertEqual(filer["worktree_path"], "/worktrees/feature")
        self.assertEqual(filer["machine"], "laptop-1")
        self.assertEqual(filer["continuous_task_id"], "continuous-1")
        self.assertEqual(filer["workflow_run_id"], "workflow-1")
        self.assertEqual(filer["workflow_role"], "worker")
        self.assertEqual(filer["managed_by_session_id"], "orchestrator-1")
        self.assertEqual(filer["submitted_via"], "mcp.submit_backtest")

        # run_key surfaced from the (server-minted) created row.
        self.assertEqual(result["run_key"], "20260707-smoke-abcd1234")
        self.assertTrue(_RUN_KEY_RE.match(result["run_key"]))
        self.assertEqual(result["task_id"], "abcd1234-0000-0000-0000-000000000000")
        self.assertEqual(result["status"], "backlog")

    def test_vm_override_precedence(self):
        fake = _FakePlanningClient()
        self._submit(fake, machine_type="c2-standard-8", zone="us-east4-b")
        vm = fake.created["metadata"]["vm"]
        self.assertEqual(vm["machine_type"], "c2-standard-8")
        self.assertEqual(vm["zone"], "us-east4-b")
        # Non-overridden defaults survive.
        self.assertEqual(vm["project"], "prediction-market-scalper")

    def test_unknown_project_requires_repo_url(self):
        fake = _FakePlanningClient()
        with self.assertRaisesRegex(ValueError, "unknown project"):
            self._submit(fake, project="nonesuch")
        self.assertIsNone(fake.created)

    def test_explicit_repo_url_skips_project_lookup(self):
        fake = _FakePlanningClient()
        fake.projects = []  # lookup would fail if consulted
        self._submit(fake, repo_url="https://github.com/x/other")
        self.assertEqual(fake.created["repo_url"], "https://github.com/x/other")

    def test_oversized_config_rejected_before_any_call(self):
        from mcp_server import server

        fake = _FakePlanningClient()
        with mock.patch.object(server, "PlanningClient", return_value=fake), \
             mock.patch.object(server, "_caller_filer_context", return_value={
                 "schema_version": 1, "agent": "unknown",
             }):
            with self.assertRaisesRegex(ValueError, "exceeds"):
                server.submit_backtest(
                    branch="main", config="x" * (32 * 1024 + 1)
                )
        self.assertIsNone(fake.created)

    def test_empty_required_fields_rejected(self):
        from mcp_server import server

        with mock.patch.object(server, "_caller_filer_context", return_value={
            "schema_version": 1, "agent": "unknown",
        }):
            with self.assertRaises(ValueError):
                server.submit_backtest(branch="", config="c.yaml")
            with self.assertRaises(ValueError):
                server.submit_backtest(branch="main", config="")
            with self.assertRaises(ValueError):
                server.submit_backtest(branch="main", config="c.yaml", project="")

    def test_unbound_caller_still_submits(self):
        fake = _FakePlanningClient()
        from mcp_server import server

        with mock.patch.object(server, "PlanningClient", return_value=fake), \
             mock.patch.object(server, "_caller_filer_context", return_value={
                 "schema_version": 1, "agent": "unknown",
             }):
            result = server.submit_backtest(branch="main", config="c.yaml")
        self.assertNotIn("parent_task_id", fake.created)
        self.assertEqual(result["status"], "backlog")


class SubmitBacktestHeadlessTests(unittest.TestCase):
    """The daemon-proxy (backtest.submit) branch."""

    def test_routes_to_daemon_with_parent(self):
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {
                "task_id": "t-1",
                "run_key": "20260707-smoke-deadbeef",
                "status": "backlog",
            }

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(server, "_caller_filer_context", return_value={
            "schema_version": 1,
            "agent": "codex",
            "task_id": "parent-9",
        }), \
             mock.patch.object(control_client, "call", side_effect=fake_call):
            result = server.submit_backtest(
                branch="cm/feat", config="configs/t1.yaml", label="t1",
                regression=True,
            )

        self.assertEqual(captured["method"], "backtest.submit")
        p = captured["params"]
        self.assertEqual(p["branch"], "cm/feat")
        self.assertEqual(p["config"], "configs/t1.yaml")
        self.assertEqual(p["script"], "analysis.backtests.backtest_actrader_grid")
        self.assertTrue(p["regression"])
        self.assertEqual(p["parent_task_id"], "parent-9")
        self.assertEqual(p["project"], "predictionTrading")
        self.assertNotIn("repo_url", p)  # omitted -> daemon resolves default
        self.assertEqual(result["run_key"], "20260707-smoke-deadbeef")


class BacktestFilerContextTests(unittest.TestCase):
    """Caller provenance is derived from daemon identity, best-effort."""

    def test_daemon_context_normalizes_agent_and_keeps_all_identifiers(self):
        from mcp_server import server, control_client

        route = mock.Mock(chose_daemon=True, path="/tmp/daemon.sock")
        pong = {
            "session_type": "claude-code",
            "task_id": "task-1",
            "workspace_id": "workspace-1",
            "continuous_task_id": "continuous-1",
            "workflow_run_id": "workflow-1",
            "workflow_role": "reviewer",
            "managed_by_session_id": "manager-1",
            "worktree_path": "/repo/worktree",
        }
        with mock.patch.object(control_client, "resolve_socket_route", return_value=route), \
             mock.patch.object(control_client, "call", return_value=pong), \
             mock.patch.object(server.socket, "gethostname", return_value="host-1"), \
             mock.patch.object(server.os, "getcwd", return_value="/fallback/cwd"), \
             mock.patch.dict(server.os.environ, {"CM_TUI_SESSION_ID": "session-1"}):
            filer = server._caller_filer_context()

        self.assertEqual(filer["agent"], "claude")
        self.assertEqual(filer["session_id"], "session-1")
        self.assertEqual(filer["task_id"], "task-1")
        self.assertEqual(filer["workspace_id"], "workspace-1")
        self.assertEqual(filer["continuous_task_id"], "continuous-1")
        self.assertEqual(filer["workflow_run_id"], "workflow-1")
        self.assertEqual(filer["workflow_role"], "reviewer")
        self.assertEqual(filer["managed_by_session_id"], "manager-1")
        self.assertEqual(filer["worktree_path"], "/repo/worktree")
        self.assertEqual(filer["machine"], "host-1")

    def test_unreachable_control_plane_still_returns_local_provenance(self):
        from mcp_server import server, control_client

        with mock.patch.object(
            control_client, "resolve_socket_route", side_effect=OSError("down")
        ), mock.patch.object(server.socket, "gethostname", return_value="host-2"), \
             mock.patch.object(server.os, "getcwd", return_value="/repo"), \
             mock.patch.dict(server.os.environ, {}, clear=True):
            filer = server._caller_filer_context()

        self.assertEqual(filer["agent"], "unknown")
        self.assertEqual(filer["machine"], "host-2")
        self.assertEqual(filer["worktree_path"], "/repo")
        self.assertNotIn("session_id", filer)


class ReadBacktestResultTests(unittest.TestCase):
    """_read_backtest_result composition + the wait loop."""

    def _compose(self, task_status, artifacts):
        from mcp_server import server, control_client

        def fake_call(method, params, **kw):
            assert method == "backtest.result"
            return {"task": {"status": task_status}, "artifacts": artifacts}

        with mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ), mock.patch.object(control_client, "call", side_effect=fake_call):
            return server._read_backtest_result("t-1")

    def test_pending(self):
        r = self._compose("running", [])
        self.assertEqual(r["status"], "pending")
        self.assertIsNone(r["summary"])
        self.assertEqual(r["task_status"], "running")

    def test_no_result_when_terminal_without_artifacts(self):
        for status in ("done", "blocked", "archived"):
            r = self._compose(status, [])
            self.assertEqual(r["status"], "no_result", status)

    def test_complete_takes_newest(self):
        arts = [
            {"id": "a2", "kind": "backtest-result", "partial": False,
             "summary": {"total_pnl": 5}, "gcs_prefix": "gs://b/2",
             "created_at": "2026-07-07T02:00:00Z"},
            {"id": "a1", "kind": "backtest-result", "partial": True,
             "summary": {"total_pnl": 1}, "gcs_prefix": "gs://b/1",
             "created_at": "2026-07-07T01:00:00Z"},
        ]
        r = self._compose("done", arts)
        self.assertEqual(r["status"], "complete")
        self.assertFalse(r["partial"])
        self.assertEqual(r["summary"], {"total_pnl": 5})
        self.assertEqual(r["gcs_prefix"], "gs://b/2")
        self.assertEqual(len(r["artifacts"]), 2)

    def test_partial(self):
        arts = [{"id": "a1", "kind": "backtest-result", "partial": True,
                 "summary": {}, "gcs_prefix": "gs://b/1",
                 "created_at": "2026-07-07T01:00:00Z"}]
        r = self._compose("backlog", arts)
        self.assertEqual(r["status"], "partial")
        self.assertTrue(r["partial"])

    def test_archived_task_with_artifact_still_reads_complete(self):
        # The dispatch daemon auto-archives terminal backtest rows (board
        # hygiene); results must stay fully retrievable afterwards.
        arts = [{"id": "a1", "kind": "backtest-result", "partial": False,
                 "summary": {"total_pnl": 5}, "gcs_prefix": "gs://b/1",
                 "created_at": "2026-07-07T01:00:00Z"}]
        r = self._compose("archived", arts)
        self.assertEqual(r["status"], "complete")
        self.assertEqual(r["summary"], {"total_pnl": 5})
        self.assertEqual(r["task_status"], "archived")

    def test_wait_returns_on_artifact(self):
        from mcp_server import server

        reads = [
            {"status": "pending", "partial": False, "summary": None,
             "gcs_prefix": None, "artifacts": [], "task_status": "running"},
            {"status": "complete", "partial": False, "summary": {"x": 1},
             "gcs_prefix": "gs://b", "artifacts": [{"id": "a"}],
             "task_status": "done"},
        ]
        with mock.patch.object(server, "_read_backtest_result", side_effect=reads), \
             mock.patch.object(asyncio, "sleep", new=mock.AsyncMock()):
            r = asyncio.run(
                server.get_backtest_result("t-1", wait=True, timeout_s=60)
            )
        self.assertEqual(r["status"], "complete")
        self.assertNotIn("timed_out", r)

    def test_wait_returns_on_terminal_without_artifact(self):
        from mcp_server import server

        reads = [
            {"status": "no_result", "partial": False, "summary": None,
             "gcs_prefix": None, "artifacts": [], "task_status": "blocked"},
        ]
        with mock.patch.object(server, "_read_backtest_result", side_effect=reads):
            r = asyncio.run(
                server.get_backtest_result("t-1", wait=True, timeout_s=60)
            )
        self.assertEqual(r["status"], "no_result")

    def test_wait_times_out(self):
        from mcp_server import server

        pending = {"status": "pending", "partial": False, "summary": None,
                   "gcs_prefix": None, "artifacts": [], "task_status": "running"}
        with mock.patch.object(
            server, "_read_backtest_result", return_value=dict(pending)
        ), mock.patch.object(asyncio, "sleep", new=mock.AsyncMock()):
            r = asyncio.run(
                server.get_backtest_result("t-1", wait=True, timeout_s=1.0)
            )
        self.assertTrue(r.get("timed_out"))


if __name__ == "__main__":
    unittest.main()
