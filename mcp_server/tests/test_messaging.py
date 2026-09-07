"""Messaging tool contract and stable workflow identity; all RPCs are mocked."""
import asyncio
import copy
import unittest
from unittest.mock import patch
from mcp_server import server, control_client


class MessagingToolsTests(unittest.TestCase):
    def test_six_tools_route_to_daemon_and_keep_retry_identity(self):
        with patch.object(control_client, "call", return_value={"ok": True}) as call:
            server.chat_send("Ready.", "same-request", channel="general", name="Scout", origin_daemon_id="home")
            first = copy.deepcopy(call.call_args.args)
            server.chat_send("Ready.", "same-request", channel="general", name="Scout", origin_daemon_id="home")
            self.assertEqual(call.call_args.args, first)
            self.assertNotIn("norms_seen", call.call_args.args[1])
            for fn, kwargs in [(server.chat_open, {}), (server.chat_read, {"dms": True, "time": {"since": "10m"}}),
                               (server.chat_dms, {"peer": "agent-id", "cursor": {"last": "id"}}),
                               (server.chat_people, {"query": "Scout"}), (server.chat_channels, {"action": "create", "path": "work/parser", "request_id": "channel"})]:
                fn(**kwargs)
                self.assertIn(call.call_args.args[0], control_client.DAEMON_METHODS)
                self.assertTrue(call.call_args.args[0].startswith("messaging."))
            with patch.dict("os.environ", {"CM_DAEMON_SOCKET": "/tmp/test-daemon", "CM_TUI_SOCKET": "/tmp/test-tui"}):
                self.assertEqual(str(control_client.resolve_socket_for_method("messaging.read")), "/tmp/test-daemon")

    def run_wait(self, binding, sessions):
        clock = [0.0]
        async def sleep(_):
            clock[0] += 5
        async def to_thread(fn, *args, **kwargs):
            return fn(*args, **kwargs)
        def call(method, params):
            if method == "get_workflow_state":
                return {"status": "active", "active_role": "worker", "iteration": 1, "role_sessions": {"worker": binding}}
            return sessions
        with patch.object(control_client, "call", side_effect=call), patch.object(server.time, "monotonic", side_effect=lambda: clock[0]), patch.object(server.asyncio, "sleep", sleep), patch.object(server.asyncio, "to_thread", to_thread):
            return asyncio.run(server.wait_for_workflow_stop("run", timeout_s=15, stuck_after_s=5))

    def test_workflow_wait_uses_uid_even_after_name_changes(self):
        result = self.run_wait({"session_label": "worker", "daemon_session_uid": "actual"}, [
            {"session_uid": "wrong", "label": "worker", "idle": False},
            {"session_uid": "actual", "label": "Parser Gardener", "idle": True},
        ])
        self.assertTrue(result["stuck"])

    def test_legacy_workflow_wait_resolves_run_role_and_reports_ambiguity(self):
        session = {"session_uid": "actual", "label": "Renamed", "workflow_run_id": "run", "workflow_role": "worker", "idle": True}
        self.assertTrue(self.run_wait({"session_label": "old"}, [session])["stuck"])
        result = self.run_wait({"session_label": "old"}, [session, {**session, "session_uid": "duplicate"}])
        self.assertTrue(result["timed_out"])
        self.assertIn("binding_warning", result["state"])

    def test_stop_hook_reports_continuation_for_inbox_work(self):
        from mcp_server.hooks import cm_stop_hook as hook
        import io
        with patch.object(hook, "_session_uid", return_value="a"), patch.object(hook, "_drain_inbox", return_value=["Wake"]), patch.object(hook, "_report_turn_ended") as report, patch("sys.stdin", io.StringIO("{}")), patch("sys.stdout", io.StringIO()):
            self.assertEqual(hook.main(), 0)
        report.assert_called_once_with("a", None, continuing=True)
