import unittest
import json
import tempfile
from pathlib import Path
from unittest.mock import patch

from mcp_server.native_codex import (
    apply_launch_permissions, launch_permissions, split_args,
)

BYPASS = "--dangerously-bypass-approvals-and-sandbox"


class CodexResumeArgsTests(unittest.TestCase):
    def test_host_policy_is_explicit_and_invalid_policy_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp, patch(
            "mcp_server.native_codex.Path.home", return_value=Path(tmp)
        ):
            self.assertIsNone(launch_permissions())
            path = Path(tmp) / ".cm/codex-permissions.json"
            path.parent.mkdir()
            path.write_text(json.dumps({"mode": "full-access-auto-review"}))
            self.assertEqual(launch_permissions(), {
                "approvalPolicy": "on-request", "approvalsReviewer": "auto_review",
                "permissions": ":danger-full-access",
            })
            path.write_text('{"mode":"typo"}')
            with self.assertRaises(ValueError):
                launch_permissions()
            path.write_text('invalid JSON')
            with self.assertRaises(ValueError):
                launch_permissions()

    def test_opted_in_fresh_and_resume_policy_avoids_frontend_override(self):
        policy = {"approvalPolicy": "on-request", "approvalsReviewer": "auto_review",
                  "permissions": ":danger-full-access"}
        for args in ([BYPASS, "--no-alt-screen"],
                     ["resume", BYPASS, "--no-alt-screen", "saved-thread"],
                     ["resume", "--no-alt-screen", "saved-thread"]):
            with self.subTest(args=args):
                backend, frontend = split_args(args, policy)
                self.assertNotIn(BYPASS, frontend)
                self.assertNotIn('approval_policy="never"', backend)
                self.assertIn('approval_policy="on-request"', backend)
                self.assertIn('approvals_reviewer="auto_review"', backend)
                self.assertIn('sandbox_mode="danger-full-access"', backend)
                if args[0] == "resume":
                    self.assertEqual(frontend[-2:], ["resume", "saved-thread"])

    def test_explicit_thread_policy_replaces_saved_never_without_sandbox_conflict(self):
        original = {"threadId": "saved", "approvalPolicy": "never",
                    "sandbox": "workspace-write", "cwd": "/work", "model": "kept"}
        policy = {"approvalPolicy": "on-request", "approvalsReviewer": "auto_review",
                  "permissions": ":danger-full-access"}
        changed = apply_launch_permissions(original, policy)
        self.assertEqual(changed, {"threadId": "saved", "cwd": "/work", "model": "kept", **policy})
        self.assertEqual(original["sandbox"], "workspace-write")
        self.assertIs(apply_launch_permissions(original, None), original)

    def test_fresh_session_retains_yolo_on_backend_and_frontend(self):
        backend, frontend = split_args([BYPASS, "--no-alt-screen"])
        self.assertEqual(backend, [
            "-c", 'approval_policy="never"',
            "-c", 'sandbox_mode="danger-full-access"',
        ])
        self.assertEqual(frontend, [BYPASS, "--no-alt-screen"])

    def test_remote_resume_inherits_permissions_with_old_or_new_viewer(self):
        for legacy in ([], [BYPASS]):
            with self.subTest(legacy=legacy):
                argv = ["resume", *legacy, "-c", "check_for_update_on_startup=false",
                        "-c", 'mcp_servers.claude-manager.command="launcher.sh"',
                        "--no-alt-screen", "saved-thread"]
                original = list(argv)
                backend, frontend = split_args(argv)
                self.assertEqual(argv, original)
                self.assertEqual(backend, [
                    "-c", "check_for_update_on_startup=false",
                    "-c", 'mcp_servers.claude-manager.command="launcher.sh"',
                ])
                self.assertEqual(frontend, ["--no-alt-screen", "resume", "saved-thread"])


if __name__ == "__main__":
    unittest.main()
