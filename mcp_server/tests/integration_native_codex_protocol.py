"""Opt-in real Codex: checkpoint/approval routing and same-backend reconnect.

Uses the deterministic local model fixture; no live sessions or credentials.
"""

import asyncio
from contextlib import suppress
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time

REPO = Path(__file__).resolve().parents[2]
sys.path[:0] = [
    str(REPO),
    str(REPO / "doc/messaging/research/native-delivery-2026-09-07/codex"),
]
os.environ["CM_CODEX_RESEARCH_ROOT"] = tempfile.mkdtemp(prefix="cm-native-protocol-")
os.environ.setdefault("CM_CODEX_BIN", shutil.which("codex") or "codex")
from harness import BIN, Mock, ROOT, config, make_env
from ws_appserver_tui import WSRPC
from websockets.asyncio.server import unix_serve
from mcp_server.native_codex import Relay, terminate
from mcp_server.notifications import Queue, consume
from mcp_server.tests.test_native_notifications import eventually
from mcp_server import control_client

# This protocol fixture has no daemon. Never inherit the developer session's
# control socket for synthetic turn-state reports.
control_client.call = lambda *_args, **_kwargs: {"ok": True}


async def main():
    case = "protocol"
    env, work = make_env(case)
    model = await Mock(case).start()
    config(env, work, model.port)
    directory = ROOT / case
    queue = Queue("ts-protocol-fixture", Path(env["HOME"]) / ".cm")
    backend_socket, frontend_socket = (
        str(directory / "backend.sock"),
        str(directory / "frontend.sock"),
    )
    log = (directory / "backend.log").open("wb")
    backend = await asyncio.create_subprocess_exec(
        BIN,
        "app-server",
        "--listen",
        "unix://" + backend_socket,
        env=env,
        cwd=work,
        stdin=asyncio.subprocess.DEVNULL,
        stdout=log,
        stderr=log,
    )
    relay = Relay(backend_socket, queue)
    client = consumer = None
    result = {}
    try:
        await eventually(lambda: Path(backend_socket).exists(), 10)
        await relay.connect()
        async with unix_serve(relay.serve_frontend, frontend_socket, compression=None):
            client = await WSRPC(case, env, work, frontend_socket, "frontend").start()
            thread = await client.call(
                "thread/start",
                {
                    "cwd": str(work),
                    "modelProvider": "mock",
                    "approvalPolicy": "never",
                    "sandbox": "danger-full-access",
                },
            )
            thread_id = thread["thread"]["id"]
            await eventually(lambda: relay.ready.is_set())
            consumer = asyncio.create_task(consume(queue, relay))
            first = True

            async def reply(request, data, number):
                nonlocal first
                if first:
                    first = False
                    return await model.reply(
                        request,
                        tool={
                            "name": "exec",
                            "namespace": "functions",
                            "input": 'text(await tools.exec_command({cmd:"sleep 2; echo FOREGROUND_COMPLETED",yield_time_ms:10000}));',
                        },
                    )
                return await model.reply(request, "CHECKPOINT_DONE")

            model.handler = reply
            position = len(client.events)
            await client.call(
                "turn/start",
                {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "ACTIVE_WORK"}],
                },
            )
            await client.wait_event(
                "item/started",
                after=position,
                predicate=lambda e: e["params"]["item"]["type"] == "commandExecution",
            )
            queue.publish(
                "active",
                "fixture",
                "[fixture active] automated checkpoint notification",
                "[fixture active]",
            )
            await client.wait_event("turn/completed", after=position)
            await eventually(lambda: queue.get("active")["status"] == "observed")
            result["active_checkpoint"] = any(
                "[fixture active]" in json.dumps(r.get("body"))
                and "FOREGROUND_COMPLETED" in json.dumps(r.get("body"))
                for r in model.requests
            )
            assert result["active_checkpoint"]

            # Drop only the bridge connection. The model backend/process/thread
            # stays in place; reconnect must not create a second conversation.
            revision = relay.identity_revision
            await relay.upstream.close()
            await eventually(
                lambda: (
                    relay.connected.is_set()
                    and relay.ready.is_set()
                    and relay.upstream.state.name == "OPEN"
                ),
                10,
            )
            queue.publish(
                "reconnect",
                "fixture",
                "[fixture reconnect] automated native wake",
                "[fixture reconnect]",
            )
            await eventually(lambda: queue.get("reconnect")["status"] == "observed", 10)
            result["same_backend_reconnect"] = (
                backend.returncode is None
                and relay.thread["id"] == thread_id
                and relay.identity_revision == revision
            )
            assert result["same_backend_reconnect"]

            # Closing and reconnecting the frontend must retain the loaded
            # conversation and route future native replies to the new frontend.
            await client.close()
            await eventually(lambda: relay.frontend is None)
            client = await WSRPC(
                case, env, work, frontend_socket, "frontend-reconnected"
            ).start()
            resumed = await client.call("thread/resume", {"threadId": thread_id})
            assert resumed["thread"]["id"] == thread_id
            result["frontend_reconnect"] = True

            # A durable new thread changes the notification target; a temporary
            # title-generation thread must leave that selection alone.
            other = await client.call(
                "thread/start",
                {
                    "cwd": str(work),
                    "modelProvider": "mock",
                    "approvalPolicy": "never",
                    "sandbox": "danger-full-access",
                },
            )
            other_id = other["thread"]["id"]
            await client.call(
                "thread/start",
                {"cwd": str(work), "modelProvider": "mock", "ephemeral": True},
            )
            assert relay.thread["id"] == other_id
            await client.call("thread/resume", {"threadId": thread_id})
            assert relay.thread["id"] == thread_id
            result["thread_selection_ignores_ephemeral"] = True

            # Native notification must leave an explicit command approval open.
            approved = await client.call(
                "thread/start",
                {
                    "cwd": str(work),
                    "modelProvider": "mock",
                    "approvalPolicy": "untrusted",
                    "sandbox": "workspace-write",
                },
            )
            approval_id = approved["thread"]["id"]
            first = True
            position = len(client.events)
            await client.call(
                "turn/start",
                {
                    "threadId": approval_id,
                    "input": [{"type": "text", "text": "APPROVAL_FIXTURE"}],
                },
            )
            request = await client.wait_event(
                "item/commandExecution/requestApproval", after=position, timeout=15
            )
            queue.publish(
                "approval",
                "fixture",
                "[fixture approval] automated native wake",
                "[fixture approval]",
            )
            await eventually(lambda: queue.get("approval")["status"] == "submitted")
            await asyncio.sleep(0.25)
            assert request["id"] in relay.server_requests
            assert not any(
                e.get("method") == "turn/completed"
                and e.get("params", {}).get("threadId") == approval_id
                for e in client.events[position:]
            )
            await client.send({"id": request["id"], "result": {"decision": "accept"}})
            await client.wait_event("turn/completed", after=position, timeout=15)
            result["approval_preserved_and_routed"] = True
            print(json.dumps(result, indent=2), flush=True)
            (directory / "results.json").write_text(json.dumps(result, indent=2))
            await client.close()
            client = None
    finally:
        if consumer:
            consumer.cancel()
            with suppress(asyncio.CancelledError):
                await consumer
        if client:
            await client.close()
        await relay.close()
        await terminate(backend)
        log.close()
        await model.close()


if __name__ == "__main__":
    asyncio.run(main())
