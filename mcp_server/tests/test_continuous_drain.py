"""Session tools preserve request identity and publish evidence before checkpoint."""
import io
import json
import os
import unittest
from unittest import mock

from mcp_server import server, control_client
from mcp_server.hooks import cm_stop_hook


class ContinuousDrainToolsTests(unittest.TestCase):
    def test_context_and_ack_forward_session_request(self):
        with mock.patch.object(control_client, "call", return_value={"ok": True}) as call:
            server.get_continuous_context()
            call.assert_called_with("continuous.context", {})
            server.acknowledge_continuous_drain("request-a")
            call.assert_called_with("continuous.ack_drain", {"request_id": "request-a"})

    def test_checkpoint_publishes_first_and_forwards_explicit_attestations(self):
        events = []
        with mock.patch.object(server.async_monitor, "persist_state", side_effect=lambda: events.append("persist")), mock.patch.object(control_client, "call", side_effect=lambda method, params: events.append(params)):
            server.checkpoint_continuous_drain("request-a", "memory/checkpoint.md", True, True)
        self.assertEqual(events, ["persist", {"request_id": "request-a", "notes": "memory/checkpoint.md", "background_work_complete": True, "work_reconciled": True}])
        with mock.patch.object(server.async_monitor, "persist_state", side_effect=OSError("disk full")), mock.patch.object(control_client, "call") as call:
            with self.assertRaises(OSError):
                server.checkpoint_continuous_drain("request-a", "checkpoint", True, True)
            call.assert_not_called()

    def test_stop_hook_reports_continuation_when_it_resumes_agent_with_inbox(self):
        for messages in [[], ["monitor final report"]]:
            with self.subTest(messages=messages), mock.patch.dict(os.environ, {"CM_TUI_SESSION_ID": "ts-root"}), mock.patch.object(cm_stop_hook, "_drain_inbox", return_value=messages), mock.patch.object(control_client, "call", return_value={}) as call, mock.patch("sys.stdin", io.StringIO("{}")), mock.patch("sys.stdout", new_callable=io.StringIO) as output:
                self.assertEqual(cm_stop_hook.main(), 0)
                if messages:
                    self.assertEqual(json.loads(output.getvalue())["decision"], "block")
                else:
                    self.assertEqual(output.getvalue(), "")
                self.assertEqual(call.call_count, 1)
                method, params = call.call_args.args
                self.assertEqual(method, "session.agent_report")
                self.assertEqual(params["session_uid"], "ts-root")
                self.assertEqual(params["event"], "Stop")
                self.assertEqual(params["payload"]["continuing"], bool(messages))
