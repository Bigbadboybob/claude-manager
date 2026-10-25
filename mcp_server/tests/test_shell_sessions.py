"""Shell panes stay controllable without becoming agent workers."""

import os
import unittest
from unittest.mock import AsyncMock, patch

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

    def test_cli_shell_watch_waits_for_pty_quiet(self):
        import contextlib
        import io
        import sys
        from mcp_server import wait
        with patch.object(control_client, "call", return_value=SHELL), \
             patch.object(sys, "argv", ["cm-wait", "shell"]), \
             contextlib.redirect_stdout(io.StringIO()) as output, \
             contextlib.redirect_stderr(io.StringIO()) as errors:
            code = wait.main()
        self.assertEqual(code, 0)
        self.assertIn('"kind": "shell"', output.getvalue())
        self.assertIn('"status": "quiet"', output.getvalue())
        self.assertIn("1 finished", errors.getvalue())


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

    async def test_wait_spawn_waits_for_a_shell_without_assistant_reply(self):
        def call(method, params=None, **kwargs):
            if method in ("mcp_start_session", "start_session"):
                return {"session_uid": "shell", "kind": "shell", "prompt_source": "caller"}
            self.assertEqual(method, "resolve_authorized_session")
            return SHELL
        with patch.object(control_client, "call", side_effect=call):
            result = await server.start_session("bash", "sh", prompt="printf ok", wait=True)
        self.assertTrue(result["completed"])
        self.assertEqual(result["kind"], "shell")
        self.assertIsNone(result["last_message"])

    async def test_shell_structured_reply_refuses_before_spawning_or_decorating_a_command(self):
        with patch.object(control_client, "call") as call:
            result = await server.start_session("bash", "sh", prompt="printf ok",
                                                wait=True, schema={"type": "object"}, isolated=True)
        self.assertEqual(result["error"], "agent_only")
        call.assert_not_called()

    async def test_final_monitors_refuse_shells_including_aliases(self):
        with patch.object(control_client, "call", return_value=SHELL), \
             patch.dict(os.environ, {"CM_TUI_SESSION_ID": "caller"}):
            for until in ("final", "task_done"):
                for edge in (True, False):
                    with self.assertRaises(async_monitor.RegistrationError) as caught:
                        async_monitor.register_monitor(["shell"], until=until, edge=edge)
                    self.assertEqual(caught.exception.code, "agent_only")
            result = await monitor._monitor_sessions(["shell"], until="final")
            self.assertEqual(result["error"], "agent_only")

    async def test_shell_quiet_waits_ignore_agent_and_transcript_signals(self):
        busy = {**SHELL, "idle": True, "pty_idle": False,
                "agent_state": {"state": "idle", "source": "hook", "turn_seq": 9}}
        quiet = {**busy, "pty_idle": True}
        with patch.object(control_client, "call", side_effect=[busy, quiet]) as call, \
             patch.object(server.asyncio, "sleep", new_callable=AsyncMock):
            result = await server.wait_for_session_idle("shell")
        self.assertEqual(call.call_count, 2)
        self.assertEqual((result["kind"], result["status"], result["idle_source"]), ("shell", "quiet", "pty"))
        # A stale bound transcript / reported flag must never become a reply.
        quiet.update(transcript_path="/not-a-shell-transcript", reported_done=True)
        with patch.object(control_client, "call", side_effect=[busy, quiet]), \
             patch.object(monitor.asyncio, "sleep", new_callable=AsyncMock):
            result = await monitor._monitor_sessions(["shell"], until="turn_end")
        entry = result["completed"][0]
        self.assertEqual((entry["kind"], entry["status"]), ("shell", "quiet"))
        self.assertIsNone(entry["last_message"])

    async def test_exited_shell_wait_is_labelled_without_agent_completion(self):
        exited = {**SHELL, "state": "exited", "pty_idle": False, "reported_done": True}
        with patch.object(control_client, "call", return_value=exited):
            result = await server.wait_for_session_idle("shell")
        self.assertEqual((result["kind"], result["status"], result["idle_source"]), ("shell", "exited", "pty"))
        self.assertTrue(result["idle"])
        self.assertFalse(result["timed_out"])

    async def test_shell_turn_end_registration_accepts_both_edge_modes(self):
        with patch.object(control_client, "call", return_value=SHELL), \
             patch.dict(os.environ, {"CM_TUI_SESSION_ID": "caller"}), \
             patch.object(async_monitor, "persist_state"), \
             patch.object(async_monitor, "_run_monitor", new_callable=AsyncMock):
            for edge in (True, False):
                result = async_monitor.register_monitor(["shell"], until="turn_end", edge=edge)
                self.assertEqual(result["watching"], ["shell"])
                async_monitor._MONITORS[result["monitor_id"]]["task"].cancel()
                del async_monitor._MONITORS[result["monitor_id"]]

    async def test_legacy_tui_shell_reply_does_not_gain_a_false_receipt_or_monitor_error(self):
        with patch.object(control_client, "call", return_value={"ok": True}), \
             patch.object(async_monitor, "register_monitor", side_effect=async_monitor.RegistrationError("agent_only", "shell")):
            result = await server.send_input("shell", "printf ok", notify_until="final")
        self.assertEqual(result["kind"], "shell")
        self.assertNotIn("monitor", result)
        self.assertNotIn("submitted", result)

    async def test_mixed_send_key_auto_monitor_keeps_only_agents(self):
        def call(method, params=None, **kwargs):
            if method == "send_key":
                return {"sent": 2, "results": [{"session_uid": "shell", "status": "ok"},
                                                {"session_uid": "agent", "status": "ok"}]}
            return SHELL if params["session_uid"] == "shell" else {"engine": "codex"}
        with patch.object(control_client, "call", side_effect=call), \
             patch.object(async_monitor, "register_monitor", return_value={"monitor_id": "agents"}) as register:
            result = await server.send_key(["shell", "agent"], "enter", notify_on_done=True, notify_until="final")
        register.assert_called_once()
        self.assertEqual(register.call_args.args[0], ["agent"])
        self.assertEqual(result["monitor"]["monitor_id"], "agents")
