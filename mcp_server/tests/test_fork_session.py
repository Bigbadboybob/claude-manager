"""`fork_session` — the MCP face of the daemon's `session.fork` RPC.

The daemon owns the work (target resolution, task + worktree mint, engine-
native fork spawn, idempotency); the tool forwards the arguments with an
idempotency key and a long timeout, routes to the daemon socket, maps an
old brain's unknown_method to a deploy hint, and registers the usual
self-waking monitor when it handed the fork a first prompt.
"""

from __future__ import annotations

import unittest
from pathlib import Path
from unittest import mock

from mcp_server import async_monitor, control_client
from mcp_server import server as srv
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

    async def test_new_task_arguments_reach_the_daemon_rpc(self):
        self._stub({"session_uid": "ts-fork-0", "task_id": "task-f", "prompt_source": "default"})
        res = await fork_session(
            "ts-src-0", task_name="try plan B", base="trunk",
            parent_task_id="task-p", label="planB",
        )
        method, params, kwargs = self.calls[0]
        self.assertEqual(method, "session.fork")
        request_id = params.pop("request_id")
        self.assertTrue(request_id, "an idempotency key is generated per call")
        self.assertEqual(params, {
            "source_uid": "ts-src-0", "task_name": "try plan B", "base": "trunk",
            "parent_task_id": "task-p", "label": "planB",
        })
        self.assertGreaterEqual(kwargs["timeout"], 150)
        self.assertEqual(
            kwargs["socket_path"], control_client.resolve_socket_for_method("session.fork"),
        )
        self.assertEqual(res["task_id"], "task-f")
        self.assertNotIn("monitor", res, "default prompt → nothing to wake the caller for")
        self.assertEqual(self.monitors, [])

    async def test_existing_task_and_caller_request_id(self):
        self._stub({"session_uid": "ts-fork-1", "prompt_source": "default"})
        await fork_session("ts-src-0", task_id="task-x", request_id="key-1", top_level=False)
        params = self.calls[0][1]
        self.assertEqual(params["task_id"], "task-x")
        self.assertNotIn("task_name", params)
        self.assertEqual(params["request_id"], "key-1")
        await fork_session("ts-src-0", task_name="n", top_level=True)
        self.assertTrue(self.calls[1][1]["top_level"])
        self.assertNotEqual(self.calls[1][1]["request_id"], "key-1")

    async def test_prompt_registers_completion_monitor_with_receipt(self):
        receipt = {"status": "pending", "submitted": None, "turn_seq_before": 3}
        self._stub({
            "session_uid": "ts-fork-1", "task_id": "task-f", "engine": "codex",
            "prompt_source": "caller", "prompt_delivery": receipt,
        })
        res = await fork_session("ts-src-0", task_name="fork", prompt="carry on with option B")
        self.assertEqual(self.calls[0][1]["prompt"], "carry on with option B")
        self.assertEqual(res["monitor"], {"monitor_id": "mon-1"})
        uids, kwargs = self.monitors[0]
        self.assertEqual(uids, ["ts-fork-1"])
        self.assertEqual(kwargs["until"], "turn_end")
        self.assertEqual(kwargs["launch_receipt"], receipt)
        self.assertFalse(kwargs["edge"], "receipt-gated watch, like start_session")
        self.assertNotIn("submitted", res, "a pending receipt never claims submitted")

    async def test_blank_prompt_is_not_sent_and_opt_out_skips_monitor(self):
        self._stub({"session_uid": "ts-fork-2", "prompt_source": "default"})
        await fork_session("ts-src-0", task_name="fork", prompt="   ")
        self.assertNotIn("prompt", self.calls[0][1])
        self.assertEqual(self.monitors, [])
        self._stub({"session_uid": "ts-fork-2", "prompt_source": "caller"})
        await fork_session("ts-src-0", task_name="fork", prompt="go", notify_on_done=False)
        self.assertEqual(self.monitors, [])

    async def test_old_brain_unknown_method_names_the_deploy(self):
        def _call(method, params, *a, **k):
            raise control_client.ControlError("unknown_method", "unknown method session.fork")

        control_client.call = _call
        res = await fork_session("ts-src-0", task_name="fork")
        self.assertEqual(res["error"], "unknown_method")
        self.assertEqual(res["message"], srv.FORK_UNSUPPORTED_MESSAGE)
        self.assertIn("brain deploy", res["message"])


class ForkSessionRouteTests(unittest.TestCase):
    def test_session_fork_routes_to_the_daemon_socket(self):
        self.assertIn("session.fork", control_client.DAEMON_METHODS)
        with mock.patch.dict("os.environ", {
            "HOME": "/home/x", "CM_DAEMON_SOCKET": "/tmp/d.sock", "CM_TUI_SOCKET": "/tmp/t.sock",
        }, clear=True):
            self.assertEqual(
                control_client.resolve_socket_for_method("session.fork"), Path("/tmp/d.sock"),
            )


if __name__ == "__main__":
    unittest.main()
