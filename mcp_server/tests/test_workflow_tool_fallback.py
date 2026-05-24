"""10d-2b review round-1: workflow_transition / workflow_done
branch correctly between daemon-routed call and TUI-local
file-write fallback based on `daemon_socket_pinned()`.

Without this branch, A-f-launched (TUI-local) participants
spawned with `CM_DAEMON_SOCKET=""` would have the per-method
resolver fall back to the TUI socket, where the TUI has no
handler — every transition would round-trip `UnknownMethod`.
"""

from __future__ import annotations

import unittest
from unittest import mock

from mcp_server import server


def _unwrap(tool_obj):
    """FastMCP wraps `@mcp.tool()` callables; the original
    function is reachable via `.fn` on the FastMCP `Tool`
    instance. If the decorator ever stops wrapping, the obj is
    already callable — handle both."""
    if callable(tool_obj):
        return tool_obj
    return tool_obj.fn


class WorkflowTransitionBranchTests(unittest.TestCase):
    """`workflow_transition` should:
    - call `control_client.call("workflow_transition", ...)` when
      `daemon_socket_pinned()` returns True (daemon-spawn path).
    - fall back to `_append_event("workflow_transition", ...)`
      when False (TUI-local path)."""

    def test_routes_to_daemon_when_socket_pinned(self):
        fn = _unwrap(server.workflow_transition)
        env = {"CM_WORKFLOW_RUN_ID": "wf_test_daemon", "CM_ROLE": "worker"}
        with mock.patch.dict("os.environ", env, clear=False):
            with mock.patch.object(
                server.control_client,
                "daemon_socket_pinned",
                return_value=True,
            ):
                with mock.patch.object(
                    server.control_client,
                    "call",
                    return_value={"event_id": "evt-abc"},
                ) as mock_call:
                    with mock.patch.object(server, "_append_event") as mock_append:
                        result = fn("reviewer", "diff lgtm?")

                    mock_call.assert_called_once()
                    args, _ = mock_call.call_args
                    self.assertEqual(args[0], "workflow_transition")
                    payload = args[1]
                    self.assertEqual(payload["to"], "reviewer")
                    self.assertEqual(payload["prompt"], "diff lgtm?")
                    self.assertEqual(payload["run_id"], "wf_test_daemon")
                    self.assertEqual(payload["role"], "worker")

                    mock_append.assert_not_called()
                    self.assertIn("evt-abc", result)

    def test_falls_back_to_append_event_when_not_pinned(self):
        fn = _unwrap(server.workflow_transition)
        env = {"CM_WORKFLOW_RUN_ID": "wf_test_local", "CM_ROLE": "worker"}
        with mock.patch.dict("os.environ", env, clear=False):
            with mock.patch.object(
                server.control_client,
                "daemon_socket_pinned",
                return_value=False,
            ):
                with mock.patch.object(
                    server.control_client,
                    "call",
                ) as mock_call:
                    with mock.patch.object(
                        server,
                        "_append_event",
                        return_value={"id": "evt-file-1"},
                    ) as mock_append:
                        result = fn("reviewer", "p")

                    mock_append.assert_called_once_with(
                        "workflow_transition",
                        {"to": "reviewer", "prompt": "p"},
                    )
                    mock_call.assert_not_called()
                    self.assertIn("evt-file-1", result)


class WorkflowDoneBranchTests(unittest.TestCase):
    """Same branch shape as workflow_transition. Distinct event
    payload (`{reason}` vs `{to, prompt}`)."""

    def test_routes_to_daemon_when_socket_pinned(self):
        fn = _unwrap(server.workflow_done)
        env = {"CM_WORKFLOW_RUN_ID": "wf_done_d", "CM_ROLE": "manager"}
        with mock.patch.dict("os.environ", env, clear=False):
            with mock.patch.object(
                server.control_client,
                "daemon_socket_pinned",
                return_value=True,
            ):
                with mock.patch.object(
                    server.control_client,
                    "call",
                    return_value={"event_id": "evt-d-1"},
                ) as mock_call:
                    with mock.patch.object(server, "_append_event") as mock_append:
                        result = fn("approved")

                    mock_call.assert_called_once()
                    args, _ = mock_call.call_args
                    self.assertEqual(args[0], "workflow_done")
                    payload = args[1]
                    self.assertEqual(payload["reason"], "approved")
                    self.assertEqual(payload["run_id"], "wf_done_d")
                    self.assertEqual(payload["role"], "manager")

                    mock_append.assert_not_called()
                    self.assertIn("evt-d-1", result)

    def test_falls_back_to_append_event_when_not_pinned(self):
        fn = _unwrap(server.workflow_done)
        env = {"CM_WORKFLOW_RUN_ID": "wf_done_l", "CM_ROLE": "manager"}
        with mock.patch.dict("os.environ", env, clear=False):
            with mock.patch.object(
                server.control_client,
                "daemon_socket_pinned",
                return_value=False,
            ):
                with mock.patch.object(
                    server.control_client,
                    "call",
                ) as mock_call:
                    with mock.patch.object(
                        server,
                        "_append_event",
                        return_value={"id": "evt-d-file"},
                    ) as mock_append:
                        result = fn("done locally")

                    mock_append.assert_called_once_with(
                        "workflow_done",
                        {"reason": "done locally"},
                    )
                    mock_call.assert_not_called()
                    self.assertIn("evt-d-file", result)


if __name__ == "__main__":
    unittest.main()
