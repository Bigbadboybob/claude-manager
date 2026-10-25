"""Shell panes stay controllable without becoming agent workers."""

import os
import unittest
from unittest.mock import patch

from mcp_server import async_monitor, control_client, monitor, server


SHELL = {"kind": "shell", "engine": "bash", "state": "pending", "idle": True}


class ShellLists(unittest.TestCase):
    def test_lists_hide_shells_and_explicit_inclusion_labels_them(self):
        def call(method, params=None):
            if method == "ping":
                return {}
            return [
                {"session_uid": "agent", "type": "codex", "state": "ready"},
                {"session_uid": "shell", "type": "bash", "state": "pending"},
                {"session_uid": "old-shell", "type": "bash", "state": "exited"},
            ]

        with patch.object(control_client, "call", side_effect=call):
            self.assertEqual([s["session_uid"] for s in server.list_sessions(include_exited=True)], ["agent"])
            rows = server.list_sessions(include_exited=True, include_shells=True)
            self.assertEqual([s["kind"] for s in rows], ["agent", "shell", "shell"])
            self.assertEqual(server.list_sessions_grouped(include_exited=True)["total"], 1)
            self.assertEqual(server.list_sessions_grouped(include_shells=True)["total"], 3)

    def test_shared_checkout_hint_labels_shells_from_an_older_daemon(self):
        rows = [{"session_uid": "worker", "type": "codex", "worktree_path": "/wt"},
                {"session_uid": "sh", "type": "bash", "worktree_path": "/wt"}]
        with patch.object(control_client, "call", return_value=rows):
            result = server.list_sessions()
        self.assertEqual(len(result), 1)
        self.assertEqual(result[0]["workspace_shared_with"][0]["kind"], "shell")

    def test_transcript_reads_refuse_before_parsing(self):
        with patch.object(control_client, "call", return_value=SHELL):
            for read in (server.read_last_turn, server.read_session_output):
                result = read("shell")
                self.assertEqual(result["error"], "agent_only")
                self.assertIn("bash is a shell", result["message"])

    def test_retained_shell_watch_explains_its_refusal(self):
        message = async_monitor._format_fire_message({"monitor_id": "old-shell-watch"},
            {"error": "agent_only", "message": "bash is a shell"})
        self.assertIn("Watch refused: bash is a shell", message)

    def test_cli_shell_watch_exits_with_a_clear_error(self):
        import contextlib
        import io
        import sys
        from mcp_server import wait
        with patch.object(control_client, "call", return_value=SHELL), \
             patch.object(sys, "argv", ["cm-wait", "shell"]), \
             contextlib.redirect_stdout(io.StringIO()) as output, \
             contextlib.redirect_stderr(io.StringIO()) as errors:
            code = wait.main()
        self.assertEqual(code, 2)
        self.assertIn('"error": "agent_only"', output.getvalue())
        self.assertIn("bash is a shell", errors.getvalue())


class ShellAsync(unittest.IsolatedAsyncioTestCase):
    async def test_command_send_does_not_register_a_worker_monitor(self):
        with patch.object(control_client, "call", return_value={"ok": True, "kind": "shell"}) as call, \
             patch.object(async_monitor, "register_monitor") as register:
            result = await server.send_input("shell", "printf ok")
        self.assertTrue(result["ok"])
        self.assertTrue(call.call_args.args[1]["submit"])
        register.assert_not_called()

    async def test_reply_wait_refuses_before_sending_command(self):
        with patch.object(control_client, "call", return_value=SHELL) as call:
            result = await server.send_input_and_wait("shell", "touch should-not-exist")
        self.assertEqual(result["error"], "agent_only")
        self.assertEqual([c.args[0] for c in call.call_args_list], ["resolve_authorized_session"])

    async def test_wait_spawn_refuses_before_creating_anything(self):
        with patch.object(control_client, "call") as call:
            result = await server.start_session("bash", "sh", wait=True, isolated=True)
        self.assertEqual(result["error"], "agent_only")
        call.assert_not_called()

    async def test_monitor_refusal_applies_to_turn_and_final_and_level_watches(self):
        with patch.object(control_client, "call", return_value=SHELL), \
             patch.dict(os.environ, {"CM_TUI_SESSION_ID": "caller"}):
            for until in ("turn_end", "final"):
                for edge in (True, False):
                    with self.assertRaises(async_monitor.RegistrationError) as caught:
                        async_monitor.register_monitor(["shell"], until=until, edge=edge)
                    self.assertEqual(caught.exception.code, "agent_only")
                result = await monitor._monitor_sessions(["shell"], until=until)
                self.assertEqual(result["error"], "agent_only")
            result = await server.wait_for_session_idle("shell")
            self.assertEqual(result["error"], "agent_only")
