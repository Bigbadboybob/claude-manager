"""PROTOTYPE stdio proxy that keeps an MCP client connection while the backend
server is replaced (doc/mcp-managed-activation.md).

The engine (Claude Code, or Codex's app-server) talks to this process over
stdio for the life of the session. The real MCP server runs as a child. On an
activation the proxy starts a new child, replays the client's original
`initialize`, checks it answers, stops the old child, and resumes forwarding.
When the tool list changed it sends `notifications/tools/list_changed`.

Standard library only and deliberately small: this process is never replaced
without a real reconnect, so it holds mechanism, not policy.

Prototype trigger: SIGUSR1 (or a line on the control FIFO is future work).
Usage: mcp_proxy.py -- <backend argv...>
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import sys
import time

PROXY_VERSION = "proto-1"
INIT_TIMEOUT_S = 30.0
STOP_TIMEOUT_S = 10.0


def log(msg: str) -> None:
    print(f"cm-mcp-proxy[{os.getpid()}]: {msg}", file=sys.stderr, flush=True)


class Backend:
    def __init__(self, argv: list[str], generation: int):
        self.argv = argv
        self.generation = generation
        self.proc: asyncio.subprocess.Process | None = None
        self.reader_task: asyncio.Task | None = None

    async def start(self, env: dict) -> None:
        self.proc = await asyncio.create_subprocess_exec(
            *self.argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            env=env,
            limit=64 * 1024 * 1024,
        )

    def write(self, message: dict) -> None:
        assert self.proc and self.proc.stdin
        self.proc.stdin.write((json.dumps(message) + "\n").encode())

    async def stop(self) -> None:
        if not self.proc or self.proc.returncode is not None:
            return
        with _suppress():
            self.proc.stdin.close()  # stdio EOF: the normal MCP shutdown
        try:
            await asyncio.wait_for(self.proc.wait(), STOP_TIMEOUT_S / 2)
            return
        except TimeoutError:
            pass
        with _suppress():
            self.proc.terminate()
        try:
            await asyncio.wait_for(self.proc.wait(), STOP_TIMEOUT_S / 2)
        except TimeoutError:
            with _suppress():
                self.proc.kill()
            await self.proc.wait()


class _suppress:
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return exc[0] is not None and issubclass(exc[0], (OSError, ProcessLookupError))


class Proxy:
    def __init__(self, argv: list[str]):
        self.argv = argv
        self.generation = 0
        self.backend: Backend | None = None
        self.init_request: dict | None = None
        self.initialized_sent = False
        self.client_pending: dict = {}  # client request id -> (method, started)
        self.server_pending: set = set()  # backend->client request ids
        self.paused = False
        self.held: list[dict] = []
        self.tools: list[str] | None = None
        self.swap_requested = asyncio.Event()
        self.swapping = False
        self.internal: dict = {}  # proxy-owned request id -> Future
        self.out = sys.stdout.buffer
        self.activations = 0
        self.closing = False

    # ---- client side ------------------------------------------------
    def to_client(self, message: dict) -> None:
        self.out.write((json.dumps(message) + "\n").encode())
        self.out.flush()

    async def client_loop(self) -> None:
        loop = asyncio.get_running_loop()
        reader = asyncio.StreamReader(limit=64 * 1024 * 1024)
        await loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
        while line := await reader.readline():
            try:
                message = json.loads(line)
            except ValueError:
                continue
            method = message.get("method")
            if method == "initialize" and "id" in message:
                self.init_request = message
            elif method == "notifications/initialized":
                self.initialized_sent = True
            if "id" in message and method:
                self.client_pending[message["id"]] = (method, time.monotonic())
            elif "id" in message and not method:
                self.server_pending.discard(message["id"])  # client answered backend
            if self.paused:
                self.held.append(message)
            else:
                self.backend.write(message)
        # Client went away: the session ended or the engine reconnected.
        self.closing = True
        await self.backend.stop()
        os._exit(0)

    # ---- backend side -----------------------------------------------
    async def backend_loop(self, backend: Backend) -> None:
        stdout = backend.proc.stdout
        while line := await stdout.readline():
            try:
                message = json.loads(line)
            except ValueError:
                continue
            mid = message.get("id")
            if mid in self.internal and "method" not in message:
                self.internal.pop(mid).set_result(message)
                continue
            if backend is not self.backend:
                continue  # a candidate (or a stopping predecessor) never talks to the client
            if "method" not in message and mid is not None:
                entry = self.client_pending.pop(mid, None)
                if entry and entry[0] == "initialize":
                    message = self.advertise_list_changed(message)
                if entry and entry[0] == "tools/list":
                    self.tools = sorted(t["name"] for t in message.get("result", {}).get("tools", []))
            elif "method" in message and mid is not None:
                self.server_pending.add(mid)
            self.to_client(message)
        # A candidate that died before answering fails its preflight now.
        prefix = f"cm-proxy-{backend.generation}-"
        for rid in [r for r in self.internal if r.startswith(prefix)]:
            self.internal.pop(rid).set_exception(RuntimeError("backend exited"))
        if backend is self.backend and not (self.swapping or self.closing):
            log(f"backend generation {backend.generation} exited; restarting")
            # Its open requests will never be answered: fail them, never replay.
            for rid in list(self.client_pending):
                self.client_pending.pop(rid)
                self.to_client({"jsonrpc": "2.0", "id": rid, "error": {
                    "code": -32603, "message": "CM MCP backend restarted; retry the call"}})
            self.server_pending.clear()
            self.swap_requested.set()

    @staticmethod
    def advertise_list_changed(message: dict) -> dict:
        caps = message.get("result", {}).setdefault("capabilities", {})
        tools = caps.get("tools")
        caps["tools"] = {**(tools if isinstance(tools, dict) else {}), "listChanged": True}
        return message

    async def internal_call(self, backend: Backend, method: str, params: dict) -> dict:
        rid = f"cm-proxy-{backend.generation}-{method}"
        future = asyncio.get_running_loop().create_future()
        self.internal[rid] = future
        backend.write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        return await asyncio.wait_for(future, INIT_TIMEOUT_S)

    # ---- activation -------------------------------------------------
    def quiet(self) -> bool:
        return not self.client_pending and not self.server_pending

    async def start_backend(self) -> Backend:
        self.generation += 1
        backend = Backend(self.argv, self.generation)
        env = dict(os.environ)
        env["CM_MCP_PROXY"] = PROXY_VERSION
        env["CM_MCP_PROXY_GENERATION"] = str(self.generation)
        if self.backend and self.backend.proc:
            env["CM_MCP_PREDECESSOR_PID"] = str(self.backend.proc.pid)
        await backend.start(env)
        backend.reader_task = asyncio.create_task(self.backend_loop(backend))
        return backend

    async def activate(self) -> None:
        self.swapping = True
        try:
            # Boundary: never swap with a request in flight either way.
            while not self.quiet():
                await asyncio.sleep(0.05)
            candidate = await self.start_backend()
            try:
                if self.init_request:
                    reply = await self.internal_call(
                        candidate, "initialize", self.init_request["params"])
                    if "error" in reply:
                        raise RuntimeError(f"initialize failed: {reply['error']}")
                    if self.initialized_sent:
                        candidate.write({"jsonrpc": "2.0", "method": "notifications/initialized"})
                    listed = await self.internal_call(candidate, "tools/list", {})
                    tools = sorted(t["name"] for t in listed.get("result", {}).get("tools", []))
                else:
                    tools = None
            except Exception as exc:  # noqa: BLE001 - a bad candidate leaves the old backend serving
                log(f"candidate generation {candidate.generation} failed preflight: {exc!r}")
                await candidate.stop()
                return
            # Hold client input while the old backend drains and exits.
            self.paused = True
            while not self.quiet():
                await asyncio.sleep(0.01)
            old, self.backend = self.backend, candidate
            if old:
                await old.stop()
            self.paused = False
            for message in self.held:
                self.backend.write(message)
            self.held.clear()
            self.activations += 1
            log(f"activated generation {candidate.generation} (pid {candidate.proc.pid})")
            if self.tools is not None and tools is not None and tools != self.tools:
                self.tools = tools
                self.to_client({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        finally:
            self.swapping = False

    async def run(self) -> None:
        self.backend = await self.start_backend()
        loop = asyncio.get_running_loop()
        loop.add_signal_handler(signal.SIGUSR1, self.swap_requested.set)
        client = asyncio.create_task(self.client_loop())
        while True:
            await self.swap_requested.wait()
            self.swap_requested.clear()
            await self.activate()
            if client.done():
                return


def main() -> None:
    argv = sys.argv[1:]
    if argv and argv[0] == "--":
        argv = argv[1:]
    if not argv:
        sys.exit("usage: mcp_proxy.py -- <backend argv...>")
    asyncio.run(Proxy(argv).run())


if __name__ == "__main__":
    main()
