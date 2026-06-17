"""Phase 2 (existing-session binding): the MCP `start_workflow` tool forwards
`role_sessions` in the daemon RPC params when provided, and OMITS the key when
None — so existing callers' wire shape is unchanged (the daemon's
`StartWorkflowParams.role_sessions` is `#[serde(default)]`).

See doc/existing-session-binding.md ("MCP tool" / Phase 2).
"""

from __future__ import annotations

import unittest
from unittest import mock

from mcp_server import server


def _underlying(tool):
    """FastMCP wraps @mcp.tool() functions; the original callable is `.fn`."""
    return getattr(tool, "fn", tool)


def _params_of(mock_call) -> dict:
    """The control_client.call signature is call(method, params, timeout=...)."""
    args, _kwargs = mock_call.call_args
    self_method, params = args[0], args[1]
    assert self_method == "start_workflow"
    return params


class StartWorkflowRoleSessionsTests(unittest.TestCase):
    def test_role_sessions_forwarded_when_provided(self):
        binding = {"worker": "ts-abc123"}
        with mock.patch.object(
            server.control_client, "call", return_value={"run_id": "wf_x"}
        ) as mock_call:
            out = _underlying(server.start_workflow)(
                task_id="t1",
                workflow_name="feedback",
                goal="do it",
                role_sessions=binding,
            )
        self.assertEqual(out, {"run_id": "wf_x"})
        params = _params_of(mock_call)
        self.assertIn("role_sessions", params, "role_sessions must ride in params")
        self.assertEqual(params["role_sessions"], binding, "forwarded verbatim")

    def test_role_sessions_omitted_when_none(self):
        """Default / None → the params dict has NO `role_sessions` key, so the
        wire shape is byte-identical to a pre-Phase-2 caller."""
        with mock.patch.object(
            server.control_client, "call", return_value={"run_id": "wf_y"}
        ) as mock_call:
            _underlying(server.start_workflow)(
                task_id="t1", workflow_name="feedback", goal="do it"
            )
        params = _params_of(mock_call)
        self.assertNotIn(
            "role_sessions",
            params,
            "absent role_sessions must NOT appear in the params dict",
        )

    def test_explicit_none_also_omits(self):
        """Passing role_sessions=None explicitly behaves like omitting it."""
        with mock.patch.object(
            server.control_client, "call", return_value={"run_id": "wf_z"}
        ) as mock_call:
            _underlying(server.start_workflow)(
                task_id="t1",
                workflow_name="feedback",
                role_sessions=None,
            )
        self.assertNotIn("role_sessions", _params_of(mock_call))

    def test_signature_default_is_none(self):
        """Guard: the parameter exists and defaults to None (no wire change for
        existing callers that don't pass it)."""
        import inspect

        sig = inspect.signature(_underlying(server.start_workflow))
        self.assertIn("role_sessions", sig.parameters)
        self.assertIsNone(sig.parameters["role_sessions"].default)


if __name__ == "__main__":
    unittest.main()
