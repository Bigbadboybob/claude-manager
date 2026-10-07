"""Initial launch receipts are submission evidence, independent of spawn success."""

import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

from mcp_server import control_client, server


def receipt(status="pending", **fields):
    return {
        "id": "launch-1",
        "status": status,
        "submitted": status == "confirmed",
        "attempts": 1,
        "confirmed_by": None,
        "reason": None,
        **fields,
    }


class LaunchConfirmationTests(unittest.IsolatedAsyncioTestCase):
    def test_read_tools_expose_pending_receipt_without_claiming_submission(self):
        with patch.object(
            control_client,
            "call",
            return_value={
                "state": "pending",
                "prompt_delivery": receipt(),
                "generation": 0,
                "idle": True,
            },
        ):
            for read in [server.read_last_turn, server.read_session_output]:
                result = read("worker")
                self.assertEqual(result["prompt_delivery"], receipt())
                self.assertNotIn("submitted", result)

    async def test_pending_then_confirmed_keeps_pinned_socket(self):
        path = Path("/original/daemon.sock")
        with patch.object(
            control_client,
            "call",
            side_effect=[
                {"state": "pending", "prompt_delivery": receipt()},
                {
                    "state": "ready",
                    "prompt_delivery": receipt("confirmed", confirmed_by="relay"),
                },
            ],
        ) as call:
            result = await server._await_launch_confirmation(
                "worker",
                receipt(),
                socket_path=path,
                interval=0,
            )
        self.assertTrue(result["submitted"])
        self.assertEqual(call.call_count, 2)
        for args in call.call_args_list:
            self.assertEqual(args.kwargs["socket_path"], path)
            self.assertLessEqual(args.kwargs["timeout"], 5)

    async def test_missing_replaced_and_exited_never_confirm(self):
        cases = [
            ({"state": "ready"}, "confirmation_lost_or_session_replaced"),
            (
                {
                    "state": "ready",
                    "prompt_delivery": receipt("confirmed", id="replacement"),
                },
                "confirmation_lost_or_session_replaced",
            ),
            (
                {"state": "exited", "prompt_delivery": receipt("confirmed")},
                "session_gone",
            ),
        ]
        for resolved, reason in cases:
            with (
                self.subTest(reason=reason),
                patch.object(control_client, "call", side_effect=[resolved]),
            ):
                result = await server._await_launch_confirmation(
                    "worker", receipt(), socket_path=None
                )
                self.assertFalse(result["submitted"])
                self.assertEqual(result["reason"], reason)

    async def test_transient_poll_failures_retry_until_confirmation_or_deadline(self):
        for transient in [
            control_client.TransportError("timeout"),
            control_client.ControlError("conflict", "restarting"),
        ]:
            with patch.object(
                control_client,
                "call",
                side_effect=[
                    transient,
                    {"state": "ready", "prompt_delivery": receipt("confirmed")},
                ],
            ) as call:
                result = await server._await_launch_confirmation(
                    "worker", receipt(), socket_path=None, interval=0
                )
                self.assertTrue(result["submitted"])
                self.assertEqual(call.call_count, 2)
        with patch.object(
            control_client, "call", side_effect=control_client.TransportError("timeout")
        ) as call:
            result = await server._await_launch_confirmation(
                "worker", receipt(), socket_path=None, timeout_s=0.03, interval=0.001
            )
            self.assertFalse(result["submitted"])
            self.assertEqual(result["reason"], "confirmation_timeout")
            # Under load the first socket attempt can consume this tiny budget.
            # The cases above prove retries; this case proves bounded failure.
            self.assertGreaterEqual(call.call_count, 1)

    async def test_wait_false_returns_pending_without_polling_or_false_submission(self):
        with (
            patch.object(
                server, "_await_launch_confirmation", new_callable=AsyncMock
            ) as confirm,
            patch.object(
                server.async_monitor,
                "register_monitor",
                return_value={"monitor_id": "watch"},
            ) as monitor,
        ):
            result = await self.launch(
                {
                    "session_uid": "worker",
                    "prompt_source": "caller",
                    "prompt_delivery": receipt(),
                },
                prompt="go",
            )
        confirm.assert_not_awaited()
        self.assertNotIn("submitted", result)
        self.assertEqual(result["prompt_delivery"]["status"], "pending")
        self.assertEqual(monitor.call_args.kwargs["launch_receipt"], receipt())
        self.assertEqual(
            monitor.call_args.kwargs["launch_socket_path"], Path("/spawn/daemon.sock")
        )

    async def test_slash_command_delivery_keeps_monitor_without_a_turn(self):
        with patch.object(
            server.async_monitor,
            "register_monitor",
            return_value={"monitor_id": "watch"},
        ) as monitor:
            result = await self.launch(
                {
                    "session_uid": "worker",
                    "prompt_source": "caller",
                    "prompt_delivery": receipt(
                        "delivered", submitted=True, confirmed_by="write"
                    ),
                },
                prompt="/triage-review X",
            )
        self.assertTrue(result["submitted"])
        monitor.assert_called_once()
        with patch.object(control_client, "call") as call:
            delivered = await server._await_launch_confirmation(
                "worker", result["prompt_delivery"], socket_path=None
            )
        call.assert_not_called()
        self.assertTrue(delivered["submitted"])

    async def test_timeout_and_malformed_receipt_do_not_claim_success(self):
        for initial, timeout, reason in [
            (receipt(), 0, "confirmation_timeout"),
            (receipt("confirmed", submitted=False), 1, "invalid_confirmation_receipt"),
        ]:
            with patch.object(control_client, "call") as call:
                result = await server._await_launch_confirmation(
                    "worker",
                    initial,
                    socket_path=None,
                    timeout_s=timeout,
                )
            call.assert_not_called()
            self.assertFalse(result["submitted"])
            self.assertEqual(result["reason"], reason)

    async def launch(self, response, **kwargs):
        with (
            patch.object(
                control_client,
                "resolve_socket_route",
                return_value=control_client.SocketRoute(
                    path=Path("/spawn/daemon.sock"),
                    chose_daemon=True,
                ),
            ),
            patch.object(control_client, "call", return_value=response),
        ):
            return await server.start_session("claude-code", "worker", **kwargs)

    async def test_failed_prompt_skips_wait_and_monitor_retaining_metadata(self):
        for wait in [True]:
            with (
                self.subTest(wait=wait),
                patch.object(server, "_await_reply", new_callable=AsyncMock) as reply,
                patch.object(server.async_monitor, "register_monitor") as monitor,
            ):
                result = await self.launch(
                    {
                        "session_uid": "worker",
                        "task_id": "task",
                        "worktree_path": "/workspace",
                        "prompt_source": "task",
                        "prompt_delivery": receipt(
                            "unconfirmed", attempts=2, reason="no_engine_turn"
                        ),
                    },
                    wait=wait,
                    task_id="task",
                )
            reply.assert_not_awaited()
            monitor.assert_not_called()
            self.assertFalse(result["submitted"])
            self.assertEqual(result["session_uid"], "worker")
            self.assertEqual(result["worktree_path"], "/workspace")
            self.assertEqual(result["prompt_delivery"]["attempts"], 2)

    async def test_task_prompt_confirmation_arms_level_watch_for_fast_completion(self):
        for until in ["turn_end", "final"]:
            with (
                self.subTest(until=until),
                patch.object(
                    server.async_monitor,
                    "register_monitor",
                    return_value={"monitor_id": "watch"},
                ) as monitor,
            ):
                result = await self.launch(
                    {
                        "session_uid": "worker",
                        "prompt_source": "task",
                        "prompt_delivery": receipt("confirmed"),
                    },
                    task_id="task",
                    notify_until=until,
                )
            self.assertTrue(result["submitted"])
            self.assertFalse(monitor.call_args.kwargs["edge"])
            self.assertEqual(monitor.call_args.kwargs["until"], until)

    async def test_wait_carries_confirmation_onto_reply(self):
        with patch.object(
            server,
            "_await_reply",
            new_callable=AsyncMock,
            return_value={"completed": True},
        ) as reply:
            result = await self.launch(
                {
                    "session_uid": "worker",
                    "prompt_source": "caller",
                    "prompt_delivery": receipt("confirmed", confirmed_by="presence", turn_seq_before=4),
                },
                prompt="go",
                wait=True,
            )
        self.assertTrue(result["submitted"])
        self.assertTrue(result["completed"])
        self.assertEqual(result["prompt_delivery"]["confirmed_by"], "presence")
        self.assertEqual(reply.call_args.kwargs["agent_baseline"], {"kind":"agent", "value":4})

    async def test_launch_wait_ignores_idle_from_before_its_prompt(self):
        from mcp_server.tests.test_agent_state_consumers import observed
        states = iter([observed("idle", seq=4), observed("working", seq=5), observed("idle", seq=5)])
        polls = []
        def call(method, params, **kwargs):
            if method == "mcp_start_session":
                return {"session_uid":"worker", "prompt_source":"caller",
                        "prompt_delivery":receipt("confirmed", turn_seq_before=4)}
            self.assertEqual(method, "resolve_authorized_session")
            value = next(states)
            polls.append(value["agent_state"]["state"])
            return value
        with patch.object(control_client, "resolve_socket_route", return_value=control_client.SocketRoute(
                path=Path("/spawn/daemon.sock"), chose_daemon=True)), \
                patch.object(control_client, "call", side_effect=call):
            result = await server.start_session("codex", "worker", prompt="go", wait=True,
                                                timeout_s=3, poll_interval_s=.5)
        self.assertTrue(result["completed"])
        self.assertEqual(polls, ["idle", "working", "idle"])

    async def test_old_daemon_preserves_legacy_behavior_without_claiming_submission(
        self,
    ):
        with patch.object(
            server.async_monitor, "register_monitor", return_value={}
        ) as monitor:
            result = await self.launch(
                {"session_uid": "worker", "prompt_source": "caller"}, prompt="go"
            )
        self.assertNotIn("submitted", result)
        self.assertTrue(monitor.call_args.kwargs["edge"])
