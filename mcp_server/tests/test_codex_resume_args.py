import unittest

from mcp_server.native_codex import split_args

BYPASS = "--dangerously-bypass-approvals-and-sandbox"


class CodexResumeArgsTests(unittest.TestCase):
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
