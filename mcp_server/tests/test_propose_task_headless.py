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


if __name__ == "__main__":
    unittest.main()
