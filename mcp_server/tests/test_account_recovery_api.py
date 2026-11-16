"""The bounded Claude recovery tools keep selection explicit and native-only."""

import unittest
from unittest.mock import patch

from mcp_server import server


class AccountRecoveryApi(unittest.IsolatedAsyncioTestCase):
    async def test_resume_routes_explicit_targets_and_status_is_read_only(self):
        with patch.object(server.control_client, "call", return_value={"results": []}) as call:
            self.assertEqual(await server.resume_claude_sessions(["uid-a"]), {"results": []})
            self.assertEqual(await server.resume_claude_status(["uid-a"]), {"results": []})
        self.assertEqual(
            [args[0] for args, _ in call.call_args_list],
            ["session.resume_many", "session.resume_status"],
        )
        self.assertEqual(call.call_args_list[0].args[1], {"session_uids": ["uid-a"]})

    async def test_resume_rejects_empty_or_over_cap_without_touching_daemon(self):
        with patch.object(server.control_client, "call") as call:
            for targets in ([], [f"uid-{n}" for n in range(33)]):
                result = await server.resume_claude_sessions(targets)
                self.assertEqual(result["error"], "invalid_params")
                self.assertIn("1..32", result["message"])
            call.assert_not_called()

