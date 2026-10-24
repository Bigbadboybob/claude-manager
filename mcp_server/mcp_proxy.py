"""Stable stdio proxy between an engine's MCP client and CM's MCP server.

The engine (Claude Code, or Codex's app-server) talks to this process over
stdio for the life of the session; `server.py` runs as a child that can be
replaced. An activation starts a new child, replays the client's original
`initialize` and a `tools/list` as a preflight, stops the old child, and
resumes forwarding. A changed tool list is announced with
`notifications/tools/list_changed`. See doc/mcp-managed-activation.md.

Mechanism only, no policy, standard library only. This file is replaced only
by a real reconnect, so keep it small and keep decisions in `server.py`:
the backend asks for an activation with `notifications/cm/activate`; this
process never forwards any `notifications/cm/*` message to the engine.

Usage: mcp_proxy.py -- <backend argv...>
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import signal
import sys
import tempfile
import time
from pathlib import Path

PROXY_VERSION = "1"
CM_PREFIX = "notifications/cm/"
INIT_TIMEOUT_S = float(os.environ.get("CM_MCP_PROXY_INIT_TIMEOUT_S", 30))
STOP_STEP_S = 5.0
# An idle-mode request is honored only this long after the last client request.
IDLE_S = float(os.environ.get("CM_MCP_PROXY_IDLE_S", 60))
# Crash restarts: back off, then hold down after this many in the window.
CRASH_BACKOFF_S = (1.0, 2.0, 4.0)
CRASH_LIMIT = 3
CRASH_WINDOW_S = 600.0
HOLD_RETRY_S = float(os.environ.get("CM_MCP_PROXY_HOLD_RETRY_S", 60))


def log(msg: str) -> None:
    print(f"cm-mcp-proxy[{os.getpid()}]: {msg}", file=sys.stderr, flush=True)


def tool_names(reply: dict) -> list[str] | None:
    tools = (reply.get("result") or {}).get("tools")
    if not isinstance(tools, list):
        return None
    return sorted(t.get("name", "") for t in tools if isinstance(t, dict))


class Backend:
    def __init__(self, argv: list[str], generation: int):
        self.argv = argv
        self.generation = generation
        self.proc: asyncio.subprocess.Process | None = None
        self.exited = asyncio.Event()

    @property
    def pid(self) -> int | None:
        return self.proc.pid if self.proc else None

    async def start(self, env: dict) -> None:
        self.proc = await asyncio.create_subprocess_exec(
            *self.argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            env=env,
            limit=64 * 1024 * 1024,
        )

    def write(self, message: dict) -> bool:
        stdin = self.proc.stdin if self.proc else None
        if stdin is None or stdin.is_closing() or self.proc.returncode is not None:
            return False
        stdin.write((json.dumps(message) + "\n").encode())
        return True

    async def stop(self) -> None:
        """stdin EOF (the normal MCP shutdown), then SIGTERM, then SIGKILL."""
        if not self.proc:
            return
        if self.proc.returncode is None and self.proc.stdin:
            self.proc.stdin.close()
        for step in (None, signal.SIGTERM, signal.SIGKILL):
            if step is not None and self.proc.returncode is None:
                try:
                    self.proc.send_signal(step)
                except ProcessLookupError:
                    pass
            try:
                await asyncio.wait_for(self.exited.wait(), STOP_STEP_S)
                break
            except TimeoutError:
                continue
        await self.proc.wait()


class Proxy:
    def __init__(self, argv: list[str], out=None):
        self.argv = argv
        self.out = out or sys.stdout.buffer
        self.generation = 0
        self.backend: Backend | None = None
        self.init_request: dict | None = None
        self.initialized_sent = False
        self.client_pending: dict = {}  # client request id -> method
        self.server_pending: set = set()  # backend->client request ids
        self.last_client_request = time.monotonic()
        self.held: list[dict] | None = None  # buffered client input during a switch
        self.tools: list[str] | None = None
        self.internal: dict = {}  # proxy-owned request id -> Future
        self.control: asyncio.Queue = asyncio.Queue()
        self.closing = False
        self.state = "starting"
        self.crashes: list[float] = []
        self.activations = 0
        self.last_activation: dict | None = None
        self.last_error: str | None = None
        uid = os.environ.get("CM_TUI_SESSION_ID", "")
        self.status_path = (
            Path.home() / ".cm" / "mcp" / uid / "proxy.json"
            if re.fullmatch(r"[A-Za-z0-9_-]{1,160}", uid) else None)

    # ---- status (for the release-lead view) -------------------------
    def publish(self, state: str | None = None) -> None:
        if state:
            self.state = state
        if not self.status_path:
            return
        value = {
            "proxy_version": PROXY_VERSION,
            "proxy_pid": os.getpid(),
            "state": self.state,
            "generation": self.generation,
            "backend_pid": self.backend.pid if self.backend else None,
            "activations": self.activations,
            "last_activation": self.last_activation,
            "recent_crashes": len(self.crashes),
            "last_error": self.last_error,
            "updated_at": time.time(),
        }
        try:
            self.status_path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            fd, tmp = tempfile.mkstemp(dir=self.status_path.parent, prefix=".proxy.")
            with os.fdopen(fd, "w") as stream:
                json.dump(value, stream)
            os.replace(tmp, self.status_path)
        except OSError:
            pass  # status is advisory; never fail the session over it

    # ---- client side ------------------------------------------------
    def to_client(self, message: dict) -> None:
        self.out.write((json.dumps(message) + "\n").encode())
        self.out.flush()

    def fail_request(self, rid, why: str) -> None:
        self.to_client({"jsonrpc": "2.0", "id": rid,
                        "error": {"code": -32603, "message": why}})

    async def client_loop(self, reader: asyncio.StreamReader) -> None:
        while line := await reader.readline():
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if not isinstance(message, dict):
                continue
            method = message.get("method")
            rid = message.get("id")
            if method == "initialize" and rid is not None:
                self.init_request = message
            elif method == "notifications/initialized":
                self.initialized_sent = True
            if rid is not None and method:
                self.client_pending[rid] = method
                self.last_client_request = time.monotonic()
            elif rid is not None:
                self.server_pending.discard(rid)  # the client answered the backend
            if self.held is not None:
                self.held.append(message)
            elif not (self.backend and self.backend.write(message)):
                if rid is not None and method:
                    self.client_pending.pop(rid, None)
                    self.fail_request(rid, self.unavailable())
        self.closing = True  # the engine closed the connection
        await self.control.put(("close", None))

    def unavailable(self) -> str:
        if self.state == "held_down":
            return (f"CM MCP backend is unavailable (crashed {len(self.crashes)} times; "
                    f"last error: {self.last_error}). It retries every "
                    f"{int(HOLD_RETRY_S)} s; a fixed deploy recovers it.")
        return "CM MCP backend is restarting; retry the call"

    # ---- backend side -----------------------------------------------
    async def backend_loop(self, backend: Backend) -> None:
        stdout = backend.proc.stdout
        while line := await stdout.readline():
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if not isinstance(message, dict):
                continue
            method = message.get("method")
            rid = message.get("id")
            if rid in self.internal and method is None:
                self.internal.pop(rid).set_result(message)
                continue
            if isinstance(method, str) and method.startswith(CM_PREFIX):
                if backend is self.backend and method == CM_PREFIX + "activate":
                    params = message.get("params") or {}
                    await self.control.put(("activate", params.get("mode", "idle")))
                continue  # never forwarded
            if backend is not self.backend:
                continue  # a candidate or a stopping predecessor never talks to the client
            if method is None and rid is not None:
                sent = self.client_pending.pop(rid, None)
                if sent == "initialize":
                    message = self.advertise_list_changed(message)
                elif sent == "tools/list":
                    self.tools = tool_names(message) or self.tools
            elif method is not None and rid is not None:
                self.server_pending.add(rid)
            self.to_client(message)
        await backend.proc.wait()
        backend.exited.set()
        prefix = f"cm-proxy-{backend.generation}-"
        for rid in [r for r in self.internal if r.startswith(prefix)]:
            self.internal.pop(rid).set_exception(RuntimeError("backend exited"))
        if backend is self.backend and not self.closing:
            await self.control.put(("crashed", backend))

    @staticmethod
    def advertise_list_changed(message: dict) -> dict:
        result = message.get("result")
        if isinstance(result, dict):
            caps = result.setdefault("capabilities", {})
            tools = caps.get("tools")
            caps["tools"] = {**(tools if isinstance(tools, dict) else {}), "listChanged": True}
        return message

    async def internal_call(self, backend: Backend, method: str, params: dict) -> dict:
        rid = f"cm-proxy-{backend.generation}-{method}"
        future = asyncio.get_running_loop().create_future()
        self.internal[rid] = future
        if not backend.write({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}):
            self.internal.pop(rid, None)
            raise RuntimeError("backend stdin closed")
        return await asyncio.wait_for(future, INIT_TIMEOUT_S)

    # ---- lifecycle --------------------------------------------------
    def quiet(self) -> bool:
        return not self.client_pending and not self.server_pending

    async def spawn(self, predecessor: int | None) -> Backend:
        self.generation += 1
        backend = Backend(self.argv, self.generation)
        env = dict(os.environ)
        env["CM_MCP_PROXY_CHILD"] = "1"
        env["CM_MCP_PROXY_VERSION"] = PROXY_VERSION
        env["CM_MCP_PROXY_GENERATION"] = str(self.generation)
        env.pop("CM_MCP_PREDECESSOR_PID", None)
        if predecessor:
            env["CM_MCP_PREDECESSOR_PID"] = str(predecessor)
        await backend.start(env)
        asyncio.create_task(self.backend_loop(backend))
        return backend

    async def preflight(self, candidate: Backend) -> list[str] | None:
        """Replay the handshake. Raises when the candidate is unusable."""
        if not self.init_request:
            return None
        reply = await self.internal_call(candidate, "initialize", self.init_request.get("params") or {})
        if "error" in reply:
            raise RuntimeError(f"initialize failed: {reply['error']}")
        if self.initialized_sent:
            candidate.write({"jsonrpc": "2.0", "method": "notifications/initialized"})
        listed = await self.internal_call(candidate, "tools/list", {})
        if "error" in listed:
            raise RuntimeError(f"tools/list failed: {listed['error']}")
        return tool_names(listed)

    def announce(self, tools: list[str] | None) -> None:
        if tools is not None and self.tools is not None and tools != self.tools:
            self.to_client({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        if tools is not None:
            self.tools = tools

    async def activate(self, mode: str) -> None:
        """Replace a serving backend. mode: ack | idle | operator."""
        if not self.backend or self.state != "running":
            return
        if mode == "idle" and (not self.quiet()
                               or time.monotonic() - self.last_client_request < IDLE_S):
            return  # no longer idle; the backend asks again later
        while not self.quiet():  # ack/operator: let in-flight calls finish
            await asyncio.sleep(0.05)
        old = self.backend
        self.publish("activating")
        started = time.time()
        candidate = await self.spawn(old.pid)
        try:
            tools = await self.preflight(candidate)
        except Exception as exc:  # noqa: BLE001 - a bad candidate leaves the old backend serving
            self.last_error = f"preflight: {exc}"[:300]
            log(f"generation {candidate.generation} failed preflight: {exc}")
            await candidate.stop()
            self.last_activation = {"at": started, "mode": mode, "ok": False,
                                    "error": self.last_error}
            self.publish("running")
            return
        preflight_done = time.time()
        self.held = []
        while not self.quiet():  # a request that raced the preflight
            await asyncio.sleep(0.01)
        self.backend = candidate
        await old.stop()
        stopped = time.time()
        held, self.held = self.held, None
        for message in held:
            if not candidate.write(message) and message.get("id") is not None and message.get("method"):
                self.client_pending.pop(message["id"], None)
                self.fail_request(message["id"], self.unavailable())
        self.activations += 1
        self.last_activation = {"at": started, "mode": mode, "ok": True,
                                "from_pid": old.pid, "to_pid": candidate.pid,
                                "preflight_ms": round((preflight_done - started) * 1000),
                                # Client input was held only for this long.
                                "hold_ms": round((stopped - preflight_done) * 1000),
                                "held_messages": len(held)}
        log(f"activated generation {candidate.generation} (pid {candidate.pid}, {mode})")
        self.announce(tools)
        self.publish("running")

    async def recover(self, dead: Backend) -> None:
        """A serving backend exited on its own. Never replay its requests."""
        for rid in list(self.client_pending):
            if self.client_pending.pop(rid) != "initialize":
                self.fail_request(rid, "CM MCP backend restarted; retry the call")
        self.server_pending.clear()
        now = time.monotonic()
        self.crashes = [t for t in self.crashes if now - t < CRASH_WINDOW_S] + [now]
        self.last_error = f"backend exited with {dead.proc.returncode}"
        log(f"generation {dead.generation} exited ({dead.proc.returncode})")
        attempt = 0
        while not self.closing:
            held_down = len(self.crashes) >= CRASH_LIMIT
            self.publish("held_down" if held_down else "restarting")
            delay = HOLD_RETRY_S if held_down else CRASH_BACKOFF_S[min(attempt, 2)]
            try:
                await asyncio.wait_for(self.wait_close(), delay)
                return
            except TimeoutError:
                pass
            candidate = await self.spawn(dead.pid)
            try:
                tools = await self.preflight(candidate)
            except Exception as exc:  # noqa: BLE001
                self.last_error = f"restart: {exc}"[:300]
                await candidate.stop()
                self.crashes.append(time.monotonic())
                attempt += 1
                continue
            self.backend = candidate
            self.announce(tools)
            log(f"restarted as generation {candidate.generation} (pid {candidate.pid})")
            self.publish("running")
            return

    async def wait_close(self) -> None:
        while not self.closing:
            await asyncio.sleep(0.2)

    async def run(self, reader: asyncio.StreamReader) -> None:
        self.backend = await self.spawn(None)
        self.publish("running")
        loop = asyncio.get_running_loop()
        loop.add_signal_handler(signal.SIGUSR1, self.control.put_nowait, ("activate", "operator"))
        for sig in (signal.SIGTERM, signal.SIGHUP):
            loop.add_signal_handler(sig, self.control.put_nowait, ("close", None))
        asyncio.create_task(self.client_loop(reader))
        while True:
            kind, value = await self.control.get()
            if kind == "close":
                self.closing = True
                break
            if kind == "activate":
                await self.activate(value)
            elif kind == "crashed" and value is self.backend:
                await self.recover(value)
        self.publish("stopped")
        if self.backend:
            await self.backend.stop()


async def amain(argv: list[str]) -> None:
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader(limit=64 * 1024 * 1024)
    await loop.connect_read_pipe(lambda: asyncio.StreamReaderProtocol(reader), sys.stdin)
    await Proxy(argv).run(reader)


def main() -> None:
    argv = sys.argv[1:]
    if argv and argv[0] == "--":
        argv = argv[1:]
    if not argv:
        sys.exit("usage: mcp_proxy.py -- <backend argv...>")
    asyncio.run(amain(argv))


if __name__ == "__main__":
    main()
