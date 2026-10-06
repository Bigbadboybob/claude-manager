import asyncio
import json
from pathlib import Path
import threading
import unittest
from unittest.mock import patch, AsyncMock

from mcp_server.codex_state import CodexState, REQUEST_KINDS
from mcp_server.native_codex import Relay, NativeRPCError
from mcp_server import control_client


def selected():
    state = CodexState()
    state.connection(True)
    state.load_thread({"id": "main", "status": {"type": "idle"}}, 100, select=True)
    return state


def event(state, method, data, at=101):
    state.observe({"method": method, "params": {"threadId": "main", **data}}, at)


def turn(state, method, status, at, uid="t1"):
    event(state, method, {"turn": {"id": uid, "status": status}}, at)


class ModelTests(unittest.TestCase):
    def test_recorded_0160_notification_sequence(self):
        state = CodexState()
        state.connection(True)
        messages = json.loads(
            (
                Path(__file__).parent
                / "fixtures/codex-app-server/success-background.json"
            ).read_text()
        )
        for number, message in enumerate(messages):
            if message["method"] == "thread/started" and state.foreground is None:
                state.load_thread(
                    message["params"]["thread"], 100 + number, select=True
                )
            state.observe(message, 100 + number)
        snap = state.snapshot()
        self.assertTrue(snap["backend_connected"])
        self.assertEqual(snap["foreground"], "idle")
        self.assertEqual(snap["turn_seq"], 3)
        self.assertEqual(snap["last_turn"]["status"], "completed")

    def test_old_protocol_turn_fallback_retry_failure_and_next_start(self):
        state = selected()
        turn(state, "turn/started", "inProgress", 101)
        event(state, "error", {"turnId": "t1", "willRetry": True}, 102)
        self.assertTrue(state.snapshot()["retrying"])
        event(
            state,
            "turn/completed",
            {
                "turn": {
                    "id": "t1",
                    "status": "failed",
                    "error": {"codexErrorInfo": "usageLimitExceeded"},
                }
            },
            103,
        )
        snap = state.snapshot()
        self.assertEqual(snap["foreground"], "idle")
        self.assertEqual(snap["error_kind"], "usageLimitExceeded")
        self.assertEqual(snap["last_turn"], {"ended_at": 103, "status": "failed"})
        self.assertFalse(snap["retrying"])
        turn(state, "turn/started", "inProgress", 104, "t2")
        self.assertEqual(state.snapshot()["turn_seq"], 2)
        self.assertIsNone(state.snapshot()["error_kind"])
        turn(state, "turn/completed", "interrupted", 105, "t2")
        self.assertEqual(state.snapshot()["last_turn"]["status"], "interrupted")

    def test_duplicate_replies_do_not_reopen_completed_turn_or_shift_end(self):
        state = selected()
        turn(state, "turn/started", "inProgress", 101)
        turn(state, "turn/completed", "completed", 102)
        state.observe(
            {"result": {"turn": {"id": "t1", "status": "inProgress"}}},
            103,
            method="turn/start",
            params={"threadId": "main"},
        )
        state.load_thread(
            {
                "id": "main",
                "status": {"type": "idle"},
                "turns": [{"id": "t1", "status": "completed"}],
            },
            104,
            select=True,
        )
        self.assertEqual(state.snapshot()["last_turn"]["ended_at"], 102)
        self.assertEqual(state.snapshot()["turn_seq"], 1)
        turn(state, "turn/started", "inProgress", 105, "t2")
        turn(state, "turn/completed", "completed", 106, "t2")
        self.assertEqual(state.snapshot()["last_turn"]["ended_at"], 106)

    def test_children_flags_requests_and_ephemeral_selection(self):
        state = selected()
        event(
            state,
            "thread/started",
            {
                "thread": {
                    "id": "child",
                    "parentThreadId": "main",
                    "status": {"type": "active"},
                }
            },
        )
        state.load_thread(
            {
                "id": "grandchild",
                "source": {"subagent": {"thread_spawn": {"parent_thread_id": "child"}}},
                "status": {"type": "active", "activeFlags": ["waitingOnApproval"]},
            },
            102,
        )
        state.load_thread(
            {"id": "title", "ephemeral": True, "status": {"type": "active"}},
            102,
            select=True,
        )
        state.load_thread({"id": "other", "status": {"type": "active"}}, 102)
        self.assertEqual(state.foreground, "main")
        self.assertTrue(state.snapshot()["child_active"])
        self.assertEqual(state.snapshot()["active_flags"], ["waitingOnApproval"])
        for number, (method, kind) in enumerate(REQUEST_KINDS.items()):
            state.observe(
                {"id": number, "method": method, "params": {"threadId": "child"}}, 103
            )
            self.assertIn(
                {"kind": kind, "since": 103}, state.snapshot()["pending_requests"]
            )
            event(state, "serverRequest/resolved", {"requestId": number}, 104)
        self.assertEqual(state.snapshot()["pending_requests"], [])
        event(
            state,
            "thread/status/changed",
            {"threadId": "child", "status": {"type": "idle"}},
        )
        event(
            state,
            "thread/status/changed",
            {"threadId": "grandchild", "status": {"type": "idle"}},
        )
        self.assertFalse(state.snapshot()["child_active"])

    def test_new_status_is_authoritative_and_disconnect_is_unknown(self):
        state = selected()
        event(
            state,
            "thread/status/changed",
            {"status": {"type": "active", "activeFlags": ["waitingOnUserInput"]}},
        )
        turn(state, "turn/completed", "completed", 102)
        self.assertEqual(state.snapshot()["foreground"], "active")
        event(state, "thread/status/changed", {"status": {"type": "systemError"}})
        self.assertEqual(state.snapshot()["foreground"], "systemError")
        state.connection(False)
        self.assertFalse(state.snapshot()["backend_connected"])
        state.connection(True)
        event(state, "thread/status/changed", {"status": {"type": "notLoaded"}})
        self.assertFalse(state.snapshot()["backend_connected"])

    def test_background_inventory_requires_complete_enumerations(self):
        state = selected()
        terminal = {
            "processId": "7",
            "command": "sleep 30",
            "osPid": 42,
            "cpuPercent": 0.1,
        }
        state.background("main", [terminal], 101, complete=True)
        state.background("main", [terminal], 102, complete=True)
        self.assertEqual(
            state.snapshot()["background"]["jobs"][0]["first_seen_at"], 101
        )
        self.assertFalse(state.snapshot()["background"]["jobs"][0]["wakes_agent"])
        state.background("other", [], 103, complete=True)
        state.background("main", [], 103, complete=True)
        self.assertEqual(
            state.snapshot()["background"]["ended"],
            [{"id": "7", "label": "sleep 30", "ended_at": 103}],
        )
        state.background("main", [terminal], 104, complete=True)
        state.background("main", [], 105, complete=False)
        self.assertEqual(len(state.snapshot()["background"]["jobs"]), 1)
        state.background("main", [], 106, complete=True)
        self.assertEqual(len(state.snapshot()["background"]["ended"]), 1)

    def test_partial_background_page_still_proves_live_work(self):
        state = selected()
        state.background(
            "main", [{"processId": "7", "command": "sleep 30"}], 101, complete=False
        )
        self.assertFalse(state.snapshot()["background"]["complete"])
        self.assertEqual(state.snapshot()["background"]["jobs"][0]["id"], "7")

    def test_bounds_fail_unknown_instead_of_losing_children(self):
        state = selected()
        for i in range(300):
            state.load_thread({"id": str(i)}, 100)
        self.assertEqual(len(state.threads), 256)
        self.assertFalse(state.snapshot()["backend_connected"])
        event(state, "thread/closed", {"threadId": "0"})
        self.assertTrue(state.snapshot()["backend_connected"])

    def test_request_overflow_clears_after_resolution(self):
        state = selected()
        for i in range(257):
            state.observe(
                {
                    "id": i,
                    "method": "item/tool/requestUserInput",
                    "params": {"threadId": "main"},
                },
                101,
            )
        self.assertFalse(state.snapshot()["backend_connected"])
        event(state, "serverRequest/resolved", {"requestId": 0})
        self.assertTrue(state.snapshot()["backend_connected"])

    def test_background_method_detection_does_not_disable_transient_errors(self):
        self.assertTrue(NativeRPCError({"code": -32601}).unsupported)
        self.assertTrue(
            NativeRPCError(
                {
                    "code": -32600,
                    "message": "unknown variant `thread/backgroundTerminals/list`",
                }
            ).unsupported
        )
        self.assertFalse(
            NativeRPCError({"code": -32602, "message": "Thread not loaded"}).unsupported
        )

    def test_version_uses_runtime_version_after_client_name(self):
        state = selected()
        state.observe(
            {"result": {"userAgent": "cm_fixture_frontend/0.160.1 (Linux)"}},
            100,
            method="initialize",
        )
        self.assertEqual(state.snapshot()["engine_version"], "0.160.1")


class PublisherTests(unittest.IsolatedAsyncioTestCase):
    def relay(self):
        relay = Relay("unused", type("Queue", (), {"uid": "fixture"})())
        relay.state = selected()
        relay.connected.set()
        return relay

    async def test_observer_failure_keeps_forwarding_and_native_wakes_alive(self):
        relay = self.relay()
        relay.thread = {"id": "main", "path": "/fixture.jsonl"}
        messages = [
            {
                "method": "turn/started",
                "params": {
                    "threadId": "main",
                    "turn": {"id": "t1", "status": "inProgress"},
                },
            },
            {
                "method": "turn/completed",
                "params": {
                    "threadId": "main",
                    "turn": {"id": "t1", "status": "completed"},
                },
            },
        ]

        async def incoming():
            for message in messages:
                yield json.dumps(message)

        relay.upstream = incoming()
        forwarded = []

        async def forward(message):
            forwarded.append(message)

        relay.to_frontend = forward
        with (
            patch.object(
                relay.state, "observe", side_effect=RuntimeError("fixture model bug")
            ),
            patch("builtins.print", side_effect=OSError("closed stderr")),
        ):
            await relay.read_connection()
        self.assertEqual(forwarded, messages)
        self.assertFalse(relay.state_enabled)
        relay.ready.set()
        relay.call = AsyncMock(return_value={"turn": {"id": "wake"}})
        receipt = await relay.send({"binding": {"thread_id": "main"}, "text": "wake"})
        self.assertEqual(receipt["turn_id"], "wake")

    async def test_publisher_and_poller_faults_disable_snapshots_without_task_exit(
        self,
    ):
        for component in ("snapshot", "background"):
            relay = self.relay()
            relay.call = AsyncMock(return_value={"data": [], "nextCursor": None})
            with patch.object(
                relay.state, component, side_effect=RuntimeError("fixture bug")
            ):
                if component == "snapshot":
                    relay.publisher = asyncio.create_task(relay.publish_state())
                    relay.report_dirty.set()
                    task = relay.publisher
                else:
                    relay.background_poller = asyncio.create_task(
                        relay.poll_background()
                    )
                    relay.background_dirty.set()
                    task = relay.background_poller
                try:
                    for _ in range(100):
                        if not relay.state_enabled:
                            break
                        await asyncio.sleep(0.01)
                    self.assertFalse(relay.state_enabled)
                    self.assertFalse(task.done())
                finally:
                    await relay.close()

    async def test_old_brain_receives_turn_edges_with_capability_backoff(self):
        relay = self.relay()
        relay.thread = {"id": "main", "path": "/fixture.jsonl"}
        relay.observe(
            {
                "method": "turn/started",
                "params": {
                    "threadId": "main",
                    "turn": {"id": "t", "status": "inProgress"},
                },
            }
        )
        calls = []

        def old_brain(method, params, **kwargs):
            calls.append((method, params))
            if method == "session.agent_report":
                raise control_client.ControlError("unknown_method", "old brain")
            return {"ok": True}

        with patch.object(control_client, "call", old_brain):
            await relay.publish_once()
            retry_at = relay.legacy_until
            await relay.publish_once()
            relay.observe(
                {
                    "method": "turn/completed",
                    "params": {
                        "threadId": "main",
                        "turn": {"id": "t", "status": "completed"},
                    },
                }
            )
            await relay.publish_once()
        self.assertEqual(
            [method for method, _ in calls],
            ["session.agent_report", "session.turn_ended", "session.turn_ended"],
        )
        self.assertTrue(calls[1][1]["continuing"])
        self.assertFalse(calls[2][1]["continuing"])
        self.assertEqual(relay.legacy_until, retry_at)
        self.assertEqual(relay.legacy_probe_delay, 120)
        relay.legacy_until = 0
        with patch.object(
            control_client, "call", return_value={"ok": True}
        ) as upgraded:
            await relay.publish_once()
            self.assertEqual(upgraded.call_args.args[0], "session.agent_report")
        self.assertEqual(relay.legacy_probe_delay, 60)

    async def test_invalid_snapshot_rejection_disables_reports_without_stopping_task(
        self,
    ):
        relay = self.relay()
        with patch.object(
            control_client,
            "call",
            side_effect=control_client.ControlError("invalid_params", "bad snapshot"),
        ) as rpc:
            relay.publisher = asyncio.create_task(relay.publish_state())
            relay.report_dirty.set()
            try:
                for _ in range(100):
                    if not relay.state_enabled:
                        break
                    await asyncio.sleep(0.01)
                self.assertFalse(relay.state_enabled)
                self.assertFalse(relay.publisher.done())
                self.assertEqual(rpc.call_count, 1)
            finally:
                await relay.close()

    async def test_handshake_filters_late_poll_replies_despite_observer_failure(self):
        relay = self.relay()
        relay.init_request = {}
        relay.queue.health = lambda *args, **kwargs: None
        relay.thread = {"id": "main", "path": "/fixture.jsonl"}

        class Socket:
            def __init__(self):
                self.messages = []

            async def send(self, raw):
                request = json.loads(raw)
                if "id" in request:
                    result = (
                        {}
                        if request["method"] == "initialize"
                        else {
                            "thread": {
                                "id": "main",
                                "path": "/fixture.jsonl",
                                "status": {"type": "idle"},
                            }
                        }
                    )
                    self.messages += [
                        {
                            "id": relay.prefix + "old-poll",
                            "result": {"data": [], "nextCursor": None},
                        },
                        {"id": request["id"], "result": result},
                    ]

            async def recv(self):
                return json.dumps(self.messages.pop(0))

            async def close(self):
                pass

        relay.to_frontend = AsyncMock()
        with (
            patch(
                "mcp_server.native_codex.unix_connect", AsyncMock(return_value=Socket())
            ),
            patch.object(
                relay.state, "observe", side_effect=RuntimeError("fixture bug")
            ),
        ):
            await relay.reconnect()
        relay.to_frontend.assert_not_awaited()
        self.assertTrue(relay.connected.is_set())
        self.assertTrue(relay.ready.is_set())
        self.assertFalse(relay.state_enabled)

    async def test_delayed_failure_retries_latest_without_concurrent_stale_report(self):
        relay = self.relay()
        started, release = threading.Event(), threading.Event()
        calls, active = [], []

        def rpc(method, params, **kwargs):
            self.assertEqual(method, "session.agent_report")
            active.append(params["seq"])
            self.assertEqual(len(active), 1)
            calls.append(params)
            if len(calls) == 1:
                started.set()
                release.wait(3)
                active.pop()
                raise control_client.TransportError("fixture disconnect")
            active.pop()
            return {"ok": True}

        with patch.object(control_client, "call", rpc):
            relay.publisher = asyncio.create_task(relay.publish_state())
            turn(relay.state, "turn/started", "inProgress", 101)
            relay.report_dirty.set()
            try:
                self.assertTrue(await asyncio.to_thread(started.wait, 2))
                turn(relay.state, "turn/completed", "completed", 102)
                relay.report_dirty.set()
                await asyncio.sleep(0.03)
                self.assertEqual(len(calls), 1)
                release.set()
                for _ in range(150):
                    if len(calls) == 2:
                        break
                    await asyncio.sleep(0.01)
                self.assertEqual(len(calls), 2)
                self.assertEqual(calls[1]["snapshot"]["foreground"], "idle")
                self.assertEqual(calls[1]["snapshot"]["last_turn"]["ended_at"], 102)
                self.assertEqual(calls[1]["snapshot"]["turn_seq"], 1)
                self.assertGreater(calls[1]["seq"], calls[0]["seq"])
                self.assertEqual(calls[1]["epoch"], calls[0]["epoch"])
            finally:
                release.set()
                await relay.close()

    async def test_poll_pagination_and_method_not_found(self):
        relay = self.relay()
        calls = []

        async def call(method, params, **kwargs):
            calls.append(params)
            if len(calls) == 1:
                return {
                    "data": [{"processId": "7", "command": "sleep 30"}],
                    "nextCursor": "page2",
                }
            return {"data": [], "nextCursor": None}

        relay.call = call
        relay.background_dirty.set()
        relay.background_poller = asyncio.create_task(relay.poll_background())
        try:
            await asyncio.wait_for(relay.report_dirty.wait(), 1)
            self.assertEqual(calls[1]["cursor"], "page2")
            self.assertTrue(relay.state.snapshot()["background"]["complete"])
            relay.report_dirty.clear()

            async def unsupported(*args, **kwargs):
                raise NativeRPCError({"code": -32601})

            relay.call = unsupported
            relay.background_dirty.set()
            await asyncio.wait_for(relay.report_dirty.wait(), 1)
            self.assertFalse(relay.background_supported)
            self.assertFalse(relay.state.snapshot()["background"]["complete"])
        finally:
            await relay.close()
