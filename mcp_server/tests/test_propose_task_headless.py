"""`propose_task` works headless — its daemon path detects the repo URL with a
plain git shell-out (``_git_origin_url``), NOT via the ``cli`` package.

On cm-manager the ``cli/`` package isn't deployed alongside the MCP server, so the
old ``from cli.planning_client import _detect_repo_url`` in propose_task's daemon
path raised ``ModuleNotFoundError`` and returned "propose_task unavailable" — even
though the task creation itself routes through the daemon (which needs nothing
from ``cli``, exactly like create_subtask). This pins the cli-free daemon path.
"""

from __future__ import annotations

import types
import unittest
from unittest import mock


def _raise_planning_unavailable(*_a, **_k):
    raise RuntimeError(
        "planning tools are unavailable in this deployment: the `cli` package "
        "is not installed alongside the MCP server"
    )


class ProposeTaskHeadlessTests(unittest.TestCase):
    def test_daemon_path_detects_repo_url_without_cli(self):
        """The daemon path forwards a git-detected repo_url and never touches
        PlanningClient (so a missing `cli` package can't break it)."""
        from mcp_server import server, control_client

        captured: dict = {}

        def fake_call(method, params, **kw):
            captured["method"] = method
            captured["params"] = params
            return {"id": "task-123"}

        route = types.SimpleNamespace(chose_daemon=True, path="/tmp/fake.sock")

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=route
        ), mock.patch.object(
            server, "_git_origin_url", return_value="git@github.com:x/y.git"
        ), mock.patch.object(
            control_client, "call", side_effect=fake_call
        ), mock.patch.object(
            server, "PlanningClient", side_effect=_raise_planning_unavailable
        ):
            out = server.propose_task(
                project="y", name="fix a bug", description="d", prompt="p"
            )

        self.assertEqual(captured.get("method"), "propose_task")
        self.assertEqual(captured["params"]["repo_url"], "git@github.com:x/y.git")
        self.assertEqual(captured["params"]["project"], "y")
        self.assertIn("task-123", out)

    def test_daemon_path_repo_url_detect_failure_is_clean(self):
        """If git can't detect an origin, propose_task returns a clear message
        and does NOT dial the daemon."""
        from mcp_server import server, control_client

        route = types.SimpleNamespace(chose_daemon=True, path="/tmp/fake.sock")

        def boom():
            raise RuntimeError("no origin")

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=route
        ), mock.patch.object(
            server, "_git_origin_url", side_effect=boom
        ), mock.patch.object(control_client, "call") as call_mock:
            out = server.propose_task(
                project="y", name="fix", description="d", prompt="p"
            )

        call_mock.assert_not_called()
        self.assertIn("could not detect the repo URL", out)

    def test_direct_api_fallback_stamps_filer_metadata(self):
        """Ad-hoc/TUI-socket callers that bypass the daemon still persist the
        same versioned filer object on the planning row."""
        from mcp_server import server, control_client

        captured: dict = {}

        class FakePlanningClient:
            def propose_task(self, **kwargs):
                captured.update(kwargs)
                return {"id": "task-local"}

        route = types.SimpleNamespace(chose_daemon=False, path="/tmp/tui.sock")
        filer = {
            "schema_version": 1,
            "agent": "codex",
            "session_id": "ts-local",
            "task_id": "parent-local",
            "submitted_via": "mcp.propose_task",
        }
        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=route
        ), mock.patch.object(
            server, "PlanningClient", return_value=FakePlanningClient()
        ), mock.patch.object(
            server, "_caller_filer_context", return_value=filer
        ) as filer_context:
            out = server.propose_task(
                project="y", name="local task", description="d", prompt="p"
            )

        filer_context.assert_called_once_with("mcp.propose_task")
        self.assertEqual(captured["metadata"], {"filer": filer})
        self.assertIn("task-local", out)


if __name__ == "__main__":
    unittest.main()
