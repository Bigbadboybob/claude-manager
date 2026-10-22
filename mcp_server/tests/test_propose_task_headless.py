"""`propose_task` works headless — its daemon path detects the repo URL with a
plain git shell-out (``_git_origin_url``), NOT via the ``cli`` package.

On cm-manager the ``cli/`` package isn't deployed alongside the MCP server, so the
old ``from cli.planning_client import _detect_repo_url`` in propose_task's daemon
path raised ``ModuleNotFoundError`` and returned "propose_task unavailable" — even
though the task creation itself routes through the daemon (which needs nothing
from ``cli``, exactly like create_subtask). This pins the cli-free daemon path.
"""

from __future__ import annotations

import sys
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

    def test_daemon_path_without_an_origin_lets_the_daemon_resolve(self):
        """The caller's origin is only a hint: with none, the daemon still gets
        the call and resolves the project's repo itself."""
        from mcp_server import server, control_client

        route = types.SimpleNamespace(chose_daemon=True, path="/tmp/fake.sock")

        def boom():
            raise RuntimeError("no origin")

        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=route
        ), mock.patch.object(
            server, "_git_origin_url", side_effect=boom
        ), mock.patch.object(control_client, "call", return_value={"id": "t-1"}) as call_mock:
            out = server.propose_task(
                project="y", name="fix", description="d", prompt="p"
            )

        params = call_mock.call_args.args[1]
        self.assertNotIn("repo_url", params)
        self.assertIn("t-1", out)

    def test_direct_api_fallback_stamps_filer_metadata(self):
        """Ad-hoc/TUI-socket callers that bypass the daemon still persist the
        same versioned filer object on the planning row."""
        from mcp_server import server, control_client

        captured: dict = {}

        class FakePlanningClient:
            def list_projects(self):
                return [{"name": "y", "repo_url": "git@github.com:x/y.git"}]

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
        self.assertEqual(captured["repo_url"], "git@github.com:x/y.git")
        self.assertIn("task-local", out)

    def test_direct_api_path_uses_the_target_projects_repo(self):
        """A caller in another repo's checkout gets the TARGET project's repo;
        its own checkout stands in only for its own repo-less project; any
        other unknown project is refused without a POST."""
        from mcp_server import server, control_client

        posted: list = []

        class FakePlanningClient:
            def list_projects(self):
                return [{"name": "predictionTrading",
                         "repo_url": "https://github.com/o/predictionTrading.git",
                         "repo_branch": "main"}]

            def get_task(self, task_id):
                return {"id": task_id, "project": "fresh"}

            def propose_task(self, **kwargs):
                posted.append(kwargs)
                return {"id": "task-local"}

        route = types.SimpleNamespace(chose_daemon=False, path="/tmp/tui.sock")
        filer = {"schema_version": 1, "task_id": "caller-task"}
        with mock.patch.object(
            control_client, "resolve_socket_route", return_value=route
        ), mock.patch.object(
            server, "PlanningClient", return_value=FakePlanningClient()
        ), mock.patch.object(
            server, "_caller_filer_context", return_value=filer
        ), mock.patch.object(
            server, "_git_origin_url", return_value="git@github.com:o/claude-manager.git"
        ):
            server.propose_task(project="predictionTrading", name="a", description="d", prompt="p")
            server.propose_task(project="fresh", name="b", description="d", prompt="p")
            out = server.propose_task(project="elsewhere", name="c", description="d", prompt="p")

        self.assertEqual(posted[0]["repo_url"], "https://github.com/o/predictionTrading.git")
        self.assertEqual(posted[1]["repo_url"], "git@github.com:o/claude-manager.git")
        self.assertEqual(len(posted), 2)
        self.assertIn("'elsewhere' has no known repo", out)


class ProposeRepoTests(unittest.TestCase):
    """`_propose_repo` directly: the refusal branch and the taskless fallback."""

    class _Client:
        def __init__(self, projects, caller_project=None, get_raises=False):
            self.projects, self.caller_project, self.get_raises = projects, caller_project, get_raises

        def list_projects(self):
            return self.projects

        def get_task(self, task_id):
            if self.get_raises:
                raise RuntimeError("planning API down")
            return {"id": task_id, "project": self.caller_project}

    def test_tasked_caller_from_another_project_is_refused(self):
        from mcp_server import server
        client = self._Client([], caller_project="claude-manager")
        with mock.patch.object(server, "_git_origin_url", return_value="git@x:o/cm.git"):
            with self.assertRaises(ValueError) as cm:
                server._propose_repo(client, "newproj", "caller-task")
        msg = str(cm.exception)
        self.assertIn("'newproj' has no known repo", msg)
        self.assertIn("'claude-manager'", msg)
        # An unreadable caller task is "unknown", still refused.
        client = self._Client([], get_raises=True)
        with mock.patch.object(server, "_git_origin_url", return_value="git@x:o/cm.git"):
            with self.assertRaises(ValueError) as cm:
                server._propose_repo(client, "newproj", "caller-task")
        self.assertIn("caller's own project is unknown", str(cm.exception))

    def test_taskless_caller_falls_back_to_its_checkout_with_a_warning(self):
        from mcp_server import server
        import io
        client = self._Client([])
        err = io.StringIO()
        with mock.patch.object(server, "_git_origin_url", return_value="git@x:o/new.git"), \
                mock.patch.object(sys, "stderr", err):
            self.assertEqual(server._propose_repo(client, "newproj", None), ("git@x:o/new.git", "main"))
        self.assertIn("has no known repo and the caller has no task", err.getvalue())
        # No origin either: refused.
        with mock.patch.object(server, "_git_origin_url", side_effect=RuntimeError("no origin")):
            with self.assertRaises(ValueError):
                server._propose_repo(client, "newproj", None)

    def test_known_project_wins_with_its_branch(self):
        from mcp_server import server
        client = self._Client([{"name": "pt", "repo_url": "https://x/o/pt.git", "repo_branch": "trunk"}])
        with mock.patch.object(server, "_git_origin_url", return_value="git@x:o/cm.git"):
            self.assertEqual(server._propose_repo(client, "pt", None), ("https://x/o/pt.git", "trunk"))


if __name__ == "__main__":
    unittest.main()
