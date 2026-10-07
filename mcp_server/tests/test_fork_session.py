"""`fork_session` — the MCP face of the daemon's `session.fork` RPC.

The daemon owns the work (task + worktree mint + engine-native fork spawn);
the tool forwards the arguments, routes to the daemon socket, and registers
the usual self-waking monitor when it handed the fork a first prompt.
"""

from __future__ import annotations

import unittest
from pathlib import Path

from mcp_server import async_monitor, control_client
from mcp_server.server import fork_session


class ForkSessionToolTests(unittest.IsolatedAsyncioTestCase):
    def setUp(self):
        self._orig_call = control_client.call
        self._orig_register = async_monitor.register_monitor
        self.calls: list[tuple[str, dict, dict]] = []
        self.monitors: list[tuple[list, dict]] = []

        def _register(uids, **kwargs):
            self.monitors.append((uids, kwargs))
            return {"monitor_id": "mon-1"}

        async_monitor.register_monitor = _register

    def tearDown(self):
        control_client.call = self._orig_call
        async_monitor.register_monitor = self._orig_register

    def _stub(self, result: dict):
        def _call(method, params, *a, **k):
            self.calls.append((method, params, k))
            return dict(result)

        control_client.call = _call

    async def test_forwards_arguments_to_daemon_rpc(self):
        self._stub({"session_uid": "ts-fork-0", "task_id": "task-f", "prompt_source": "none"})
        res = await fork_session(
            "ts-src-0", "try plan B", base="trunk", parent_task="task-p", label="planB",
        )
        method, params, kwargs = self.calls[0]
        self.assertEqual(method, "session.fork")
        self.assertEqual(params, {
            "source_uid": "ts-src-0", "task_name": "try plan B", "base": "trunk",
            "parent_task_id": "task-p", "label": "planB",
        })
        self.assertEqual(
            kwargs["socket_path"], control_client.resolve_socket_for_method("session.fork"),
        )
        self.assertEqual(res["task_id"], "task-f")
        self.assertNotIn("monitor", res, "no prompt → nothing to wake the caller for")
        self.assertEqual(self.monitors, [])

    async def test_prompt_registers_completion_monitor_with_receipt(self):
        receipt = {"status": "pending", "submitted": None, "turn_seq_before": 3}
        self._stub({
            "session_uid": "ts-fork-1", "task_id": "task-f", "engine": "codex",
            "prompt_source": "caller", "prompt_delivery": receipt,
        })
        res = await fork_session("ts-src-0", "fork", prompt="carry on with option B")
        self.assertEqual(self.calls[0][1]["prompt"], "carry on with option B")
        self.assertEqual(res["monitor"], {"monitor_id": "mon-1"})
        uids, kwargs = self.monitors[0]
        self.assertEqual(uids, ["ts-fork-1"])
        self.assertEqual(kwargs["until"], "turn_end")
        self.assertEqual(kwargs["launch_receipt"], receipt)
        self.assertFalse(kwargs["edge"], "receipt-gated watch, like start_session")
        self.assertNotIn("submitted", res, "a pending receipt never claims submitted")

    async def test_blank_prompt_is_not_sent_and_opt_out_skips_monitor(self):
        self._stub({"session_uid": "ts-fork-2", "prompt_source": "none"})
        await fork_session("ts-src-0", "fork", prompt="   ")
        self.assertNotIn("prompt", self.calls[0][1])
        self.assertEqual(self.monitors, [])
        self._stub({"session_uid": "ts-fork-2", "prompt_source": "caller"})
        await fork_session("ts-src-0", "fork", prompt="go", notify_on_done=False)
        self.assertEqual(self.monitors, [])


class ForkSessionRouteTests(unittest.TestCase):
    def test_session_fork_routes_to_the_daemon_socket(self):
        self.assertIn("session.fork", control_client.DAEMON_METHODS)
        from unittest import mock

        with mock.patch.dict("os.environ", {
            "HOME": "/home/x", "CM_DAEMON_SOCKET": "/tmp/d.sock", "CM_TUI_SOCKET": "/tmp/t.sock",
        }, clear=True):
            self.assertEqual(
                control_client.resolve_socket_for_method("session.fork"), Path("/tmp/d.sock"),
            )


if __name__ == "__main__":
    unittest.main()
