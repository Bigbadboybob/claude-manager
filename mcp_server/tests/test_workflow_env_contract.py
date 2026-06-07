"""P-CRIT consumer-contract tests for the workflow MCP tools.

This is the OTHER half of the env-threading chain that the daemon-side
verification (`build_args_claude_workflow_participant_carries_run_id_and_role`
and `start_workflow_threads_workflow_meta_into_mcp_config_for_every_role` in
`cm-daemon`) locks down. The daemon writes `CM_WORKFLOW_RUN_ID` + `CM_ROLE`
into the MCP server's config `env` block; here we assert the MCP server
actually READS exactly those names — and HARD-FAILS without them.

Why this test exists: the daemon's poller tests drive worker->reviewer->manager
without ever invoking the real MCP server, so a regression that dropped the
workflow env from the spawned participant's MCP config passed every Rust poller
test while breaking production (the reviewer/manager could no longer call
`workflow_transition` / `workflow_done`). Pinning the consumer contract here
means such a regression fails the suite:

  - the NEGATIVE test proves the tool hard-fails when the env is absent (so a
    missing-env regression is caught, not silently tolerated), and
  - the POSITIVE test proves the EXACT env names the daemon writes are the
    ones the server consumes (a rename on either side breaks this).
"""

from __future__ import annotations

import unittest
from unittest import mock

from mcp_server import server


class RequireWorkflowEnvContractTests(unittest.TestCase):
    def test_missing_run_id_hard_fails(self):
        """No CM_WORKFLOW_RUN_ID → RuntimeError naming the var.

        This is the exact failure mode of P-CRIT: a daemon-launched
        reviewer/manager whose MCP config lacked the workflow env block would
        hit this and stall the run.
        """
        with mock.patch.dict("os.environ", {}, clear=True):
            with self.assertRaises(RuntimeError) as ctx:
                server._require_workflow_env()
        self.assertIn("CM_WORKFLOW_RUN_ID", str(ctx.exception))

    def test_present_env_returns_run_id_and_role(self):
        """The server reads the SAME names the daemon writes."""
        with mock.patch.dict(
            "os.environ",
            {"CM_WORKFLOW_RUN_ID": "wf_42", "CM_ROLE": "reviewer"},
            clear=True,
        ):
            run_id, role = server._require_workflow_env()
        self.assertEqual(run_id, "wf_42")
        self.assertEqual(role, "reviewer")

    def test_blank_run_id_treated_as_missing(self):
        """A whitespace-only run id is rejected like an absent one."""
        with mock.patch.dict(
            "os.environ",
            {"CM_WORKFLOW_RUN_ID": "   ", "CM_ROLE": "manager"},
            clear=True,
        ):
            with self.assertRaises(RuntimeError):
                server._require_workflow_env()


if __name__ == "__main__":
    unittest.main()
