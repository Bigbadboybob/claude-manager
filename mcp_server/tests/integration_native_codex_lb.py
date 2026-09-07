"""Opt-in live local-pool smoke test; creates only a new disposable thread.

Reads the installed local_pool provider configuration without printing auth.
Runs no tools and never resumes or modifies an existing conversation.
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
import tomllib

REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPO))
from websockets.asyncio.client import unix_connect
from websockets.asyncio.server import unix_serve
from mcp_server import control_client
from mcp_server.native_codex import Relay, terminate
from mcp_server.notifications import Queue, consume
from mcp_server.tests.test_native_notifications import eventually

control_client.call = lambda *_args, **_kwargs: {"ok": True}


def toml_table(data, prefix=""):
    lines = []
    for key, value in data.items():
        if not isinstance(value, dict):
            lines.append(json.dumps(key) + " = " + json.dumps(value))
    for key, value in data.items():
        if isinstance(value, dict):
            table = prefix + ("." if prefix else "") + json.dumps(key)
            lines.extend(["[" + table + "]", toml_table(value, table)])
    return "\n".join(lines)


async def main():
    source = (
        Path(os.environ.get("CODEX_HOME", str(Path.home() / ".codex"))) / "config.toml"
    )
    config = tomllib.loads(source.read_text())
    assert config["model_provider"] == "local_pool", (
        "This fixture is for the configured local LB only"
    )
    model = config["model"]
    provider = config["model_providers"]["local_pool"]
    with tempfile.TemporaryDirectory(prefix="cm-native-lb-", delete=False) as temporary:
        root = Path(temporary)
        home = root / ".codex"
        work = root / "work"
        home.mkdir()
        work.mkdir()
        isolated = {
            "model": model,
            "model_provider": "local_pool",
            "approval_policy": "never",
            "sandbox_mode": "read-only",
            "check_for_update_on_startup": False,
            "model_providers": {"local_pool": provider},
            "analytics": {"enabled": False},
        }
        (home / "config.toml").write_text(toml_table(isolated))
        env = {
            key: value
            for key, value in os.environ.items()
            if key in {"HOME", "PATH", "LANG", "USER", "LOGNAME", "SHELL"}
        }
        env["CODEX_HOME"] = str(home)
        backend_socket, frontend_socket = (
            str(root / "backend.sock"),
            str(root / "frontend.sock"),
        )
        queue = Queue("ts-disposable-lb-fixture", root / ".cm")
        binary = os.environ.get("CM_CODEX_BIN") or shutil.which("codex")
        log = (root / "backend.log").open("wb")
        backend = await asyncio.create_subprocess_exec(
            sys.executable,
            str(REPO / "mcp_server/native_process.py"),
            binary,
            "app-server",
            "--listen",
            "unix://" + backend_socket,
            env=env,
            cwd=work,
            stdin=asyncio.subprocess.PIPE,
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        relay = Relay(backend_socket, queue)
        consumer = None
        events = []
        try:
            await eventually(lambda: Path(backend_socket).exists(), 15)
            await relay.connect()
            async with unix_serve(
                relay.serve_frontend, frontend_socket, compression=None
            ):
                async with unix_connect(
                    frontend_socket, uri="ws://localhost", compression=None
                ) as client:

                    async def call(request_id, method, params):
                        await client.send(
                            json.dumps(
                                {"id": request_id, "method": method, "params": params}
                            )
                        )
                        while True:
                            response = json.loads(
                                await asyncio.wait_for(client.recv(), 45)
                            )
                            if response.get("id") == request_id and (
                                "result" in response or "error" in response
                            ):
                                assert "error" not in response, (
                                    "native LB request failed"
                                )
                                return response["result"]

                    await call(
                        1,
                        "initialize",
                        {
                            "clientInfo": {"name": "cm-native-lb-test", "version": "1"},
                            "capabilities": {"experimentalApi": True},
                        },
                    )
                    await client.send(json.dumps({"method": "initialized"}))
                    result = await call(
                        2,
                        "thread/start",
                        {
                            "cwd": str(work),
                            "modelProvider": "local_pool",
                            "model": model,
                            "approvalPolicy": "never",
                            "sandbox": "read-only",
                            "developerInstructions": "You are an isolated protocol fixture. Do not invoke any tools. Respond to a CM notification with exactly CM_NATIVE_LB_OK.",
                        },
                    )
                    consumer = asyncio.create_task(consume(queue, relay))
                    start = time.monotonic()
                    queue.publish(
                        "live-lb",
                        "fixture",
                        "[cm-test live-lb] Automated CM protocol test. Reply exactly CM_NATIVE_LB_OK. Do not use tools.",
                        "[cm-test live-lb]",
                    )
                    text = ""
                    while True:
                        event = json.loads(await asyncio.wait_for(client.recv(), 180))
                        events.append(event.get("method", "response"))
                        if event.get("method") == "item/agentMessage/delta":
                            text += event["params"]["delta"]
                        if event.get("method") == "turn/completed":
                            assert (
                                event["params"]["turn"].get("status") == "completed"
                            ), "LB turn failed"
                            break
                    assert "CM_NATIVE_LB_OK" in text, (
                        "LB did not return expected fixture response"
                    )
                    await eventually(
                        lambda: queue.get("live-lb")["status"] == "observed"
                    )
                    print(
                        json.dumps(
                            {
                                "local_pool": True,
                                "model": model,
                                "native_tool_output": True,
                                "observed": True,
                                "completion_s": round(time.monotonic() - start, 3),
                                "new_disposable_thread": True,
                            }
                        ),
                        flush=True,
                    )
        except BaseException:
            print(
                json.dumps(
                    {
                        "fixture_directory": str(root),
                        "events": events,
                        "queue": queue.snapshot(),
                    }
                ),
                flush=True,
            )
            raise
        finally:
            if consumer:
                consumer.cancel()
                with suppress(asyncio.CancelledError):
                    await consumer
            await relay.close()
            backend.stdin.close()
            await terminate(backend)
            log.close()


if __name__ == "__main__":
    if "--live-lb" not in sys.argv:
        raise SystemExit(
            "Pass --live-lb to perform one real model request against the configured local LB."
        )
    asyncio.run(main())
