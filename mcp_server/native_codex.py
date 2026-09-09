"""CM-owned Codex app-server and the normal remote terminal frontend.

The local websocket relay shares the frontend's upstream connection: approval
requests keep their normal UI, and successful thread/start/resume/fork replies
identify the conversation the user actually selected. CM inserts only named
tool outputs, using its own RPC IDs, never keyboard bytes or human messages.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import shutil
import signal
import sys
import tempfile
import uuid
from contextlib import suppress
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from websockets.asyncio.client import unix_connect
from websockets.asyncio.server import unix_serve
from websockets.exceptions import ConnectionClosed

from mcp_server import control_client
from mcp_server.notifications import (
    NotSubmitted,
    Queue,
    consume_supervised,
    transcript_observed,
)


class Relay:
    engine = "codex"

    def __init__(self, socket: str, queue: Queue):
        self.socket = socket
        self.queue = queue
        self.upstream = None
        self.frontend = None
        self.thread = None
        self.init_request = None
        self.init_result = None
        self.requests = {}
        self.pending = {}
        self.server_requests = {}
        self.prefix = "cm-native-" + uuid.uuid4().hex + "-"
        self.sequence = 0
        self.ready = asyncio.Event()
        self.connected = asyncio.Event()
        self.closing = False
        self.reader = None
        self.identity_revision = 0
        self.selecting = set()
        self.report_tasks = set()

    def identity(self):
        return {
            "session_uid": self.queue.uid,
            "adapter": "codex-app-server-v1",
            "thread_id": (self.thread or {}).get("id"),
            "pid": os.getpid(),
            "transcript_path": (self.thread or {}).get("path"),
        }

    def health_status(self):
        return "ready" if self.ready.is_set() else "awaiting_thread"

    async def connect(self):
        self.upstream = await unix_connect(
            self.socket,
            uri="ws://localhost",
            compression=None,
            max_size=None,  # thread/resume may contain a long existing history
            open_timeout=5,
        )
        self.connected.set()
        self.reader = asyncio.create_task(self.read())

    async def call(self, method: str, params: dict, timeout=30):
        if not self.upstream:
            raise NotSubmitted("backend disconnected")
        self.sequence += 1
        request_id = self.prefix + str(self.sequence)
        future = asyncio.get_running_loop().create_future()
        self.pending[request_id] = future
        try:
            await self.upstream.send(
                json.dumps({"id": request_id, "method": method, "params": params})
            )
            result = await asyncio.wait_for(future, timeout)
            if "error" in result:
                raise RuntimeError("Codex rejected native RPC")
            return result["result"]
        finally:
            self.pending.pop(request_id, None)

    async def to_frontend(self, message):
        if self.frontend:
            with suppress(ConnectionClosed):
                await self.frontend.send(json.dumps(message))

    def report(self, continuing: bool):
        if not self.thread:
            return
        params = {
            "session_uid": self.queue.uid,
            "continuing": continuing,
            "transcript_path": self.thread.get("path"),
        }

        async def work():
            # Registry publication can race the initial frontend handshake.
            for _ in range(10):
                try:
                    await asyncio.to_thread(
                        control_client.call, "session.turn_ended", params, timeout=2
                    )
                    return
                except (control_client.ControlError, control_client.TransportError):
                    await asyncio.sleep(0.5)

        task = asyncio.create_task(work())
        self.report_tasks.add(task)
        task.add_done_callback(self.report_tasks.discard)

    async def read(self):
        while not self.closing:
            await self.read_connection()
            if self.closing:
                return
            self.connected.clear()
            self.queue.health(self.engine, "disconnected", **self.identity())
            for request_id in list(self.requests):
                await self.to_frontend(
                    {
                        "id": request_id,
                        "error": {
                            "code": -32001,
                            "message": "CM app-server connection lost; operation outcome is unknown",
                        },
                    }
                )
            self.requests.clear()
            self.selecting.clear()
            while not self.closing:
                try:
                    await self.reconnect()
                    break
                except (TimeoutError, OSError, ConnectionClosed, RuntimeError):
                    await asyncio.sleep(0.5)

    async def reconnect(self):
        """Rejoin the same owning backend; never resume on a second server."""
        self.upstream = await unix_connect(
            self.socket,
            uri="ws://localhost",
            compression=None,
            max_size=None,
            open_timeout=3,
        )

        async def handshake(method, params):
            self.sequence += 1
            request_id = self.prefix + str(self.sequence)
            await self.upstream.send(
                json.dumps({"id": request_id, "method": method, "params": params})
            )
            while True:
                message = json.loads(await asyncio.wait_for(self.upstream.recv(), 10))
                if message.get("id") == request_id and (
                    "result" in message or "error" in message
                ):
                    if "error" in message:
                        raise RuntimeError("native reconnect rejected")
                    return message["result"]
                await self.to_frontend(message)

        try:
            if self.init_request is not None:
                await handshake("initialize", self.init_request)
                await self.upstream.send(json.dumps({"method": "initialized"}))
                if self.thread:
                    result = await handshake(
                        "thread/resume", {"threadId": self.thread["id"]}
                    )
                    if result["thread"]["id"] != self.thread["id"]:
                        raise RuntimeError("native reconnect changed thread identity")
                    self.thread = {
                        key: result["thread"].get(key)
                        for key in ("id", "path", "status")
                    }
                    self.ready.set()
            self.connected.set()
            self.queue.health(self.engine, self.health_status(), **self.identity())
        except BaseException:
            await self.upstream.close()
            raise

    async def read_connection(self):
        try:
            async for raw in self.upstream:
                message = json.loads(raw)
                request_id = message.get("id")
                if request_id in self.pending and (
                    "result" in message or "error" in message
                ):
                    future = self.pending[request_id]
                    if not future.done():
                        future.set_result(message)
                    continue
                method = (
                    self.requests.pop(request_id, None)
                    if "result" in message or "error" in message
                    else None
                )
                if method == "initialize" and "result" in message:
                    self.init_result = message["result"]
                if method in {"thread/start", "thread/resume", "thread/fork"}:
                    self.selecting.discard(request_id)
                    thread = message.get("result", {}).get("thread")
                    # Codex's TUI also starts ephemeral title-generation jobs.
                    # Those are never the operator's conversation.
                    if (
                        thread
                        and not thread.get("ephemeral")
                        and not thread.get("parentThreadId")
                    ):
                        self.thread = {
                            key: thread.get(key) for key in ("id", "path", "status")
                        }
                        self.identity_revision += 1
                        self.ready.set()
                        self.queue.health(self.engine, "ready", **self.identity())
                        self.report(thread.get("status", {}).get("type") == "active")
                event = message.get("method")
                params = message.get("params", {})
                if event in {"turn/started", "turn/completed"} and params.get(
                    "threadId"
                ) == (self.thread or {}).get("id"):
                    self.report(event == "turn/started")
                if "id" in message and "method" in message:
                    self.server_requests[request_id] = message
                if event == "serverRequest/resolved":
                    self.server_requests.pop(params.get("requestId"), None)
                await self.to_frontend(message)
        except (ConnectionClosed, OSError, ValueError):
            pass
        finally:
            self.ready.clear()
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(
                        ConnectionError("native backend connection lost")
                    )

    async def serve_frontend(self, ws):
        if self.frontend:
            await ws.close(
                code=1013, reason="CM session already has a terminal frontend"
            )
            return
        self.frontend = ws
        try:
            async for raw in ws:
                message = json.loads(raw)
                method = message.get("method")
                request_id = message.get("id")
                if isinstance(request_id, str) and request_id.startswith(self.prefix):
                    await ws.close(code=1008, reason="reserved RPC ID")
                    return
                if method == "initialize":
                    message.setdefault("params", {}).setdefault("capabilities", {})[
                        "experimentalApi"
                    ] = True
                    if self.init_result is not None:
                        await ws.send(
                            json.dumps({"id": request_id, "result": self.init_result})
                        )
                        continue
                    self.init_request = message["params"]
                if method == "initialized" and self.init_result is not None:
                    # Replaying initialized is harmless; unresolved approvals
                    # are replayed only to a newly attached frontend.
                    for request in list(self.server_requests.values()):
                        await ws.send(json.dumps(request))
                if "id" in message and method:
                    self.requests[request_id] = method
                    if method in {
                        "thread/start",
                        "thread/resume",
                        "thread/fork",
                    } and not message.get("params", {}).get("ephemeral"):
                        self.selecting.add(request_id)
                # Keep approvals until the backend's serverRequest/resolved
                # acknowledgement; a dropped connection may lose this response.
                if not self.connected.is_set():
                    self.requests.pop(request_id, None)
                    self.selecting.discard(request_id)
                    if request_id is not None:
                        await ws.send(
                            json.dumps(
                                {
                                    "id": request_id,
                                    "error": {
                                        "code": -32002,
                                        "message": "CM app-server reconnecting; this operation was not submitted",
                                    },
                                }
                            )
                        )
                    continue
                await self.upstream.send(json.dumps(message))
        except (ConnectionClosed, OSError):
            pass
        finally:
            self.frontend = None

    async def send(self, event):
        if not self.ready.is_set() or self.selecting or not self.thread:
            raise NotSubmitted("no selected live thread")
        # An event claimed before a thread switch must not drift to that thread.
        if event["binding"].get("thread_id") != self.thread["id"]:
            raise NotSubmitted("thread changed before submission")
        selected = dict(self.thread)
        result = await self.call(
            "turn/start",
            {
                "threadId": selected["id"],
                "input": [],
                "toolOutput": {
                    "name": "cm_notification",
                    "namespace": None,
                    "output": event["text"],
                },
            },
        )
        return {
            "kind": "app_server_accepted",
            "turn_id": result["turn"]["id"],
            "transcript_path": selected.get("path"),
        }

    async def observed(self, event):
        # Bind positive receipt checks to the exact target rollout, including
        # after the frontend switches to another conversation.
        path = event.get("receipt", {}).get("transcript_path") or event.get(
            "binding", {}
        ).get("transcript_path")
        if not path and event.get("binding", {}).get("thread_id") == (
            self.thread or {}
        ).get("id"):
            path = (self.thread or {}).get("path")
        return await asyncio.to_thread(
            transcript_observed, path, "codex", event["marker"]
        )

    async def close(self):
        self.closing = True
        if self.upstream:
            await self.upstream.close()
        if self.reader:
            self.reader.cancel()
            await asyncio.gather(self.reader, return_exceptions=True)
        for task in list(self.report_tasks):
            task.cancel()
        await asyncio.gather(*self.report_tasks, return_exceptions=True)


def split_args(args):
    """CM-generated embedded argv -> backend configuration + remote UI intent."""
    args = list(args)
    resume = None
    if args and args[0] == "resume":
        args.pop(0)
        resume = args.pop()
    backend = []
    frontend = []
    index = 0
    while index < len(args):
        arg = args[index]
        if arg == "-c":
            backend.extend(args[index : index + 2])
            index += 2
        elif arg == "--dangerously-bypass-approvals-and-sandbox":
            # Exactly CM's existing launch policy, applied to the backend.
            backend.extend(
                [
                    "-c",
                    'approval_policy="never"',
                    "-c",
                    'sandbox_mode="danger-full-access"',
                ]
            )
            frontend.append(arg)
            index += 1
        elif arg == "--no-alt-screen":
            frontend.append(arg)
            index += 1
        else:
            raise ValueError(f"unsupported CM Codex launch argument: {arg}")
    if resume:
        frontend.extend(["resume", resume])
    return backend, frontend


async def terminate(child):
    if child is None or child.returncode is not None:
        return
    with suppress(ProcessLookupError):
        child.terminate()
    try:
        await asyncio.wait_for(child.wait(), 5)
    except TimeoutError:
        with suppress(ProcessLookupError):
            child.kill()
        await child.wait()


def configure_external_editor():
    """Headless CM spawns do not source the operator's interactive shell config."""
    if any(os.environ.get(key, "").strip() for key in ("VISUAL", "EDITOR")):
        return
    for name in ("nvim", "vim", "vi"):
        if editor := shutil.which(name):
            os.environ["VISUAL"] = editor
            os.environ["EDITOR"] = editor
            return


async def launch(args):
    os.environ.update(json.loads(args.cm_env))
    configure_external_editor()
    os.environ["CM_TUI_SESSION_ID"] = args.session_uid
    os.environ["CM_AGENT_ENGINE"] = "codex"
    for key in (
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_SESSION_ID",
    ):
        os.environ.pop(key, None)
    queue = Queue.own()
    backend_args, frontend_args = split_args(args.codex_args)
    binary = os.environ.get("CM_CODEX_BIN", "codex")
    backend = frontend = relay = consumer = None
    stop = asyncio.Event()
    loop = asyncio.get_running_loop()
    for sig in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set)
    # Short AF_UNIX paths and owner-only access, independent of checkout length.
    with tempfile.TemporaryDirectory(prefix="cm-codex-") as private:
        backend_socket = str(Path(private) / "backend.sock")
        frontend_socket = str(Path(private) / "terminal.sock")
        log_path = queue.path / "backend.log"
        log_fd = os.open(log_path, os.O_CREAT | os.O_TRUNC | os.O_WRONLY, 0o600)
        try:
            backend = await asyncio.create_subprocess_exec(
                sys.executable,
                str(Path(__file__).with_name("native_process.py")),
                binary,
                *backend_args,
                "app-server",
                "--listen",
                "unix://" + backend_socket,
                stdin=asyncio.subprocess.PIPE,
                stdout=log_fd,
                stderr=log_fd,
                start_new_session=True,
            )
            for _ in range(200):
                if Path(backend_socket).exists():
                    break
                if backend.returncode is not None or stop.is_set():
                    raise RuntimeError(
                        f"Codex app-server did not start; see {log_path}"
                    )
                await asyncio.sleep(0.05)
            else:
                raise RuntimeError(
                    f"Codex app-server startup timed out; see {log_path}"
                )
            relay = Relay(backend_socket, queue)
            await relay.connect()
            async with unix_serve(
                relay.serve_frontend,
                frontend_socket,
                compression=None,
                max_size=None,
            ):
                consumer = asyncio.create_task(consume_supervised(queue, relay))
                frontend = await asyncio.create_subprocess_exec(
                    sys.executable,
                    str(Path(__file__).with_name("native_process.py")),
                    "--exec-child",
                    str(os.getpid()),
                    binary,
                    "--remote",
                    "unix://" + frontend_socket,
                    *frontend_args,
                )
                waiters = [
                    asyncio.create_task(frontend.wait()),
                    asyncio.create_task(backend.wait()),
                    asyncio.create_task(stop.wait()),
                    relay.reader,
                    consumer,
                ]
                done, _ = await asyncio.wait(
                    waiters, return_when=asyncio.FIRST_COMPLETED
                )
                for task in waiters[:3]:
                    task.cancel()
                await asyncio.gather(*waiters[:3], return_exceptions=True)
                if relay.reader in done and backend.returncode is None:
                    # Do not attach a second app-server to a live conversation.
                    # A broken ownership connection is explicit and recoverable
                    # with normal CM restart/resume; queued wakes stay durable.
                    raise RuntimeError("Codex native ownership connection closed")
                if consumer in done:
                    await consumer
                    raise RuntimeError("Codex native notification consumer stopped")
                if (
                    backend.returncode is not None
                    and frontend.returncode is None
                    and not stop.is_set()
                ):
                    raise RuntimeError(f"Codex app-server exited; see {log_path}")
                return frontend.returncode or 0
        finally:
            if consumer:
                consumer.cancel()
                with suppress(asyncio.CancelledError):
                    await consumer
            await terminate(frontend)
            if relay:
                await relay.close()
            if backend and backend.stdin:
                backend.stdin.close()
            await terminate(backend)
            os.close(log_fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--session-uid", required=True)
    parser.add_argument("--cm-env", required=True)
    parser.add_argument("codex_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.codex_args[:1] == ["--"]:
        args.codex_args.pop(0)
    try:
        return asyncio.run(launch(args))
    except Exception as exc:  # noqa: BLE001 - CLI boundary reports failure after owned-process cleanup
        print(f"CM native Codex launcher: {type(exc).__name__}: {exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
