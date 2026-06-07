"""P-3c: the MCP `start_workflow` tool must use a generous control-client
timeout so it doesn't spuriously report a transport timeout while the daemon is
still serializing per-role transcript-detector binding (~20s/role).

The default `control_client.call` timeout is 30s — below a 3-role feedback
launch's worst case — so the tool must override it. This pins parity with the
TUI launch path (client_session.rs's START_WORKFLOW_RPC_READ_TIMEOUT = 150s).
"""

from __future__ import annotations

import unittest
from unittest import mock

from mcp_server import server


def _underlying(tool):
    """FastMCP wraps @mcp.tool() functions; the original callable is `.fn`."""
    return getattr(tool, "fn", tool)


class StartWorkflowTimeoutTests(unittest.TestCase):
    def test_start_workflow_passes_long_timeout(self):
        with mock.patch.object(
            server.control_client, "call", return_value={"run_id": "wf_x"}
        ) as mock_call:
            out = _underlying(server.start_workflow)(
                task_id="t1", workflow_name="feedback", goal="do it"
            )
        self.assertEqual(out, {"run_id": "wf_x"})
        # The call must carry a timeout well above the 30s default and the
        # daemon's roles×20s worst case.
        _, kwargs = mock_call.call_args
        self.assertIn("timeout", kwargs, "start_workflow must override the timeout")
        self.assertGreaterEqual(
            kwargs["timeout"],
            120.0,
            "timeout must safely exceed roles × per-role slot timeout (20s)",
        )

    def test_default_control_client_timeout_is_still_short(self):
        """Guard: the override is needed precisely because the default is 30s."""
        import inspect

        sig = inspect.signature(server.control_client.call)
        self.assertEqual(sig.parameters["timeout"].default, 30.0)


if __name__ == "__main__":
    unittest.main()
