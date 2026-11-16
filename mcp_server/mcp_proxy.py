"""Stable stdio proxy between an engine's MCP client and CM's MCP server.

The engine (Claude Code, or Codex's app-server) talks to this process over
stdio for the life of the session; `server.py` runs as a child that can be
replaced. An activation starts a candidate child, replays the client's
original `initialize` and a `tools/list` as a preflight, switches routing
the moment the serving child has nothing in flight, and retires the old
child. A changed tool list is announced with
`notifications/tools/list_changed`. See doc/mcp-managed-activation.md.

Mechanism only, no policy, standard library only. This file is replaced only
by a real reconnect, so keep it small and keep decisions in `server.py`.
The backend talks to the proxy with `notifications/cm/*` (activate, ready);
nothing under that prefix is ever forwarded to the engine.

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

PROXY_VERSION = "2"
CM_PREFIX = "notifications/cm/"
LINE_LIMIT = int(os.environ.get("CM_MCP_PROXY_LINE_LIMIT", 64 * 1024 * 1024))
INIT_TIMEOUT_S = float(os.environ.get("CM_MCP_PROXY_INIT_TIMEOUT_S", 30))
# The serving child must reach "nothing in flight" within this long, or the
# activation is abandoned (candidate killed) and retried at a later boundary.
QUIET_WAIT_S = float(os.environ.get("CM_MCP_PROXY_QUIET_WAIT_S", 10))
# After the switch, client input waits for the new child's `ready` (its
# predecessor has exited and its monitors are adopted), at most this long.
HOLD_MAX_S = float(os.environ.get("CM_MCP_PROXY_HOLD_MAX_S", 5))
STOP_STEP_S = 5.0
# An idle-mode request is honored only this long after the last client request.
IDLE_S = float(os.environ.get("CM_MCP_PROXY_IDLE_S", 60))
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


def start_ticks(pid: int) -> str | None:
    """Process start time (clock ticks since boot): pid plus this is unique."""
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[19]
    except (OSError, IndexError):
        return None


class Backend:
    def __init__(self, argv: list[str], generation: int):
        self.argv = argv
        self.generation = generation
        self.proc: asyncio.subprocess.Process | None = None
        self.exited = asyncio.Event()
        self.ready = asyncio.Event()
        self.error: str | None = None
        self.start_ticks: str | None = None

    @property
    def pid(self) -> int | None:
        return self.proc.pid if self.proc else None

    async def start(self, env: dict) -> None:
        self.proc = await asyncio.create_subprocess_exec(
            *self.argv,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            env=env,
            limit=LINE_LIMIT,
        )
        # Read now: once the child is reaped /proc/<pid> is gone, and a crash
        # restart still has to name it as the successor's predecessor.
        self.start_ticks = start_ticks(self.proc.pid)

    def alive(self) -> bool:
        return bool(self.proc) and self.proc.returncode is None and not self.exited.is_set()

    def write(self, message: dict) -> bool:
        stdin = self.proc.stdin if self.proc else None
        if not self.alive() or stdin is None or stdin.is_closing():
            return False
        try:
            stdin.write((json.dumps(message) + "\n").encode())
        except (OSError, RuntimeError):
            return False
        return True

    def kill(self) -> None:
        if self.proc and self.proc.returncode is None:
            try:
                self.proc.kill()
            except ProcessLookupError:
                pass

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
        self.backends: set[Backend] = set()
        self.init_request: dict | None = None
        self.initialized_sent = False
        self.client_pending: dict = {}  # client request id -> (method, Backend it went to)
        self.server_pending: dict = {}  # backend->client request id -> Backend that asked
        self.last_client_request = time.monotonic()
        self.held: list[dict] | None = None
        self.tools: list[str] | None = None
        self.internal: dict = {}  # proxy-owned request id -> Future
        self.control: asyncio.Queue = asyncio.Queue()
        self.activation: asyncio.Task | None = None
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

    def close(self) -> None:
        if not self.closing:
            self.closing = True
            self.control.put_nowait(("close", None))

    # ---- client side ------------------------------------------------
    def to_client(self, message: dict) -> None:
        try:
            self.out.write((json.dumps(message) + "\n").encode())
            self.out.flush()
        except (BrokenPipeError, ConnectionResetError, OSError) as exc:
            log(f"engine connection lost while writing ({type(exc).__name__}); shutting down")
            self.close()

    def fail_request(self, rid, why: str) -> None:
        self.to_client({"jsonrpc": "2.0", "id": rid,
                        "error": {"code": -32603, "message": why}})

    def unavailable(self) -> str:
        if self.state == "held_down":
            return (f"CM MCP backend is unavailable (crashed {len(self.crashes)} times; "
                    f"last error: {self.last_error}). It retries every "
                    f"{int(HOLD_RETRY_S)} s; a fixed deploy recovers it.")
        return "CM MCP backend is restarting; retry the call"

    def route(self, message: dict) -> None:
        """Send one client message to the backend it belongs to."""
        method = message.get("method")
        rid = message.get("id")
        if rid is not None and method is None:
            # A response to a backend's request goes back to THAT backend.
            target = self.server_pending.pop(rid, None) or self.backend
            if target:
                target.write(message)
            return
        if rid is not None:
            target = self.backend
            if target and target.write(message):
                self.client_pending[rid] = (method, target)
            else:
                self.fail_request(rid, self.unavailable())
            return
        if method == "notifications/cancelled":
            ref = (message.get("params") or {}).get("requestId")
            entry = self.client_pending.get(ref)
            if entry:
                entry[1].write(message)
                return
        if self.backend:
            self.backend.write(message)

    async def client_loop(self, reader: asyncio.StreamReader) -> None:
        while True:
            try:
                line = await reader.readline()
            except ValueError:
                log(f"dropped an engine message over {LINE_LIMIT} bytes")
                continue
            if not line:
                break
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if not isinstance(message, dict):
                continue
            method = message.get("method")
            if method == "initialize" and message.get("id") is not None:
                self.init_request = message
            elif method == "notifications/initialized":
                self.initialized_sent = True
            if message.get("id") is not None and method:
                self.last_client_request = time.monotonic()
            if self.held is not None and (method or message.get("id") is None):
                # Held messages are not in flight anywhere: quiet() ignores them.
                self.held.append(message)
            else:
                self.route(message)
        self.close()  # the engine closed the connection

    # ---- backend side -----------------------------------------------
    async def backend_loop(self, backend: Backend) -> None:
        stdout = backend.proc.stdout
        try:
            while True:
                try:
                    line = await stdout.readline()
                except (ValueError, asyncio.LimitOverrunError):
                    # A reply we cannot read would leave its caller waiting
                    # forever: treat it as a crash, loudly.
                    backend.error = f"backend wrote a line over {LINE_LIMIT} bytes"
                    log(f"generation {backend.generation}: {backend.error}; killing it")
                    backend.kill()
                    break
                if not line:
                    break
                try:
                    message = json.loads(line)
                except ValueError:
                    continue
                if isinstance(message, dict):
                    self.from_backend(backend, message)
        except Exception as exc:  # noqa: BLE001 - never a silent dead reader
            backend.error = f"proxy reader failed: {exc!r}"[:300]
            log(f"generation {backend.generation}: {backend.error}; killing it")
            backend.kill()
        await backend.proc.wait()
        backend.exited.set()
        self.backends.discard(backend)
        prefix = f"cm-proxy-{backend.generation}-"
        for rid in [r for r in self.internal if r.startswith(prefix)]:
            future = self.internal.pop(rid)
            if not future.done():
                future.set_exception(RuntimeError("backend exited"))
        if backend is self.backend and not self.closing:
            self.control.put_nowait(("crashed", backend))

    def from_backend(self, backend: Backend, message: dict) -> None:
        method = message.get("method")
        rid = message.get("id")
        if rid in self.internal and method is None:
            future = self.internal.pop(rid)
            if not future.done():
                future.set_result(message)
            return
        if isinstance(method, str) and method.startswith(CM_PREFIX):
            if method == CM_PREFIX + "ready":
                backend.ready.set()
            elif method == CM_PREFIX + "activate" and backend is self.backend:
                params = message.get("params") or {}
                self.control.put_nowait(("activate", params.get("mode", "idle")))
            return  # never forwarded
        if method is None and rid is not None:
            entry = self.client_pending.get(rid)
            if not entry or entry[1] is not backend:
                return  # not this backend's request (a candidate's or a stale id)
            del self.client_pending[rid]
            if entry[0] == "initialize":
                message = self.advertise_list_changed(message)
            elif entry[0] == "tools/list":
                self.tools = tool_names(message) or self.tools
            self.to_client(message)
            return
        if backend is not self.backend:
            return  # a candidate or a retiring child never starts traffic
        if rid is not None:
            self.server_pending[rid] = backend
        self.to_client(message)

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
    def quiet(self, backend: Backend) -> bool:
        return (all(b is not backend for _, b in self.client_pending.values())
                and all(b is not backend for b in self.server_pending.values()))

    async def wait_quiet(self, backend: Backend, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while not self.quiet(backend):
            if self.closing or not backend.alive() or time.monotonic() > deadline:
                return False
            await asyncio.sleep(0.02)
        return not self.closing and backend.alive()

    async def spawn(self, predecessor: Backend | None) -> Backend:
        self.generation += 1
        backend = Backend(self.argv, self.generation)
        env = dict(os.environ)
        env["CM_MCP_PROXY_CHILD"] = "1"
        env["CM_MCP_PROXY_VERSION"] = PROXY_VERSION
        env["CM_MCP_PROXY_GENERATION"] = str(self.generation)
        for key in ("CM_MCP_PREDECESSOR_PID", "CM_MCP_PREDECESSOR_START"):
            env.pop(key, None)
        if predecessor and predecessor.pid:
            # The successor stays a passive candidate (no notification
            # consumer, no policy, no monitor adoption) until this process
            # is gone; pid plus start time cannot name an unrelated process.
            env["CM_MCP_PREDECESSOR_PID"] = str(predecessor.pid)
            env["CM_MCP_PREDECESSOR_START"] = predecessor.start_ticks or ""
        await backend.start(env)
        self.backends.add(backend)
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

    def record(self, started: float, mode: str, ok: bool, **extra) -> None:
        self.last_activation = {"at": started, "mode": mode, "ok": ok, **extra}

    async def activate(self, mode: str) -> None:
        """Replace the serving backend. mode: ack | idle | operator. Runs as
        its own task, so close and crash events are never stuck behind it."""
        old = self.backend
        if not old or self.state != "running":
            return
        if mode == "idle" and (not self.quiet(old)
                               or time.monotonic() - self.last_client_request < IDLE_S):
            return  # no longer idle; the backend asks again later
        started = time.time()
        if not await self.wait_quiet(old, QUIET_WAIT_S):
            self.record(started, mode, False, error="busy: a call was still in flight")
            self.publish()
            return
        candidate = await self.spawn(old)
        promoted = False
        try:
            self.publish("activating")
            try:
                tools = await self.preflight(candidate)
            except Exception as exc:  # noqa: BLE001 - a bad candidate leaves the old one serving
                self.last_error = f"preflight: {exc}"[:300]
                log(f"generation {candidate.generation} failed preflight: {exc}")
                self.record(started, mode, False, error=self.last_error)
                return
            preflight_done = time.time()
            # A call that reached the old child during the preflight: wait for
            # it, bounded, without holding anything; else give up for now.
            if not await self.wait_quiet(old, QUIET_WAIT_S):
                self.record(started, mode, False, error="busy after preflight; retried later")
                return
            # Nothing is in flight on `old` and no await separates that check
            # from the switch, so no request can be split between the two.
            self.backend, promoted = candidate, True
            self.held = []
            self.announce(tools)
            asyncio.create_task(self.retire(old))
            try:
                await asyncio.wait_for(candidate.ready.wait(), HOLD_MAX_S)
            except TimeoutError:
                pass
            finally:
                self.release()
            self.activations += 1
            self.record(started, mode, True, from_pid=old.pid, to_pid=candidate.pid,
                        preflight_ms=round((preflight_done - started) * 1000),
                        hold_ms=round((time.time() - preflight_done) * 1000))
            log(f"activated generation {candidate.generation} (pid {candidate.pid}, {mode})")
        finally:
            if not promoted:
                candidate.kill()
            if self.state == "activating":
                self.publish("running")
            else:
                self.publish()

    def release(self) -> None:
        held, self.held = self.held, None
        for message in held or ():
            self.route(message)

    async def retire(self, old: Backend) -> None:
        await old.stop()
        self.backends.discard(old)

    async def recover(self, dead: Backend) -> None:
        """A serving backend exited on its own. Never replay its requests."""
        if self.activation and not self.activation.done():
            self.activation.cancel()
            await asyncio.gather(self.activation, return_exceptions=True)
        if self.held is not None:
            self.release()
        for rid, (method, backend) in list(self.client_pending.items()):
            if backend is dead:
                del self.client_pending[rid]
                self.fail_request(rid, "CM MCP backend restarted; retry the call"
                                  if method != "initialize" else
                                  "CM MCP server exited during initialize; reconnect MCP")
        for rid, backend in list(self.server_pending.items()):
            if backend is dead:
                del self.server_pending[rid]
        now = time.monotonic()
        self.crashes = [t for t in self.crashes if now - t < CRASH_WINDOW_S] + [now]
        self.last_error = dead.error or f"backend exited with {dead.proc.returncode}"
        log(f"generation {dead.generation} exited ({dead.proc.returncode})")
        attempt = 0
        while not self.closing:
            held_down = len(self.crashes) >= CRASH_LIMIT
            self.publish("held_down" if held_down else "restarting")
            delay = HOLD_RETRY_S if held_down else CRASH_BACKOFF_S[min(attempt, 2)]
            if await self.sleep_unless_closing(delay):
                return
            candidate = await self.spawn(dead)
            try:
                tools = await self.preflight(candidate)
            except Exception as exc:  # noqa: BLE001
                self.last_error = f"restart: {exc}"[:300]
                candidate.kill()
                self.crashes.append(time.monotonic())
                attempt += 1
                continue
            self.backend = candidate
            self.last_error = None
            self.announce(tools)
            log(f"restarted as generation {candidate.generation} (pid {candidate.pid})")
            self.publish("running")
            return

    async def sleep_unless_closing(self, delay: float) -> bool:
        deadline = time.monotonic() + delay
        while time.monotonic() < deadline:
            if self.closing:
                return True
            await asyncio.sleep(0.1)
        return self.closing

    async def shutdown(self) -> None:
        if self.activation and not self.activation.done():
            self.activation.cancel()
            await asyncio.gather(self.activation, return_exceptions=True)
        self.publish("stopped")
        await asyncio.gather(*(b.stop() for b in list(self.backends)), return_exceptions=True)

    async def run(self, reader: asyncio.StreamReader) -> None:
        self.backend = await self.spawn(None)
        self.publish("running")
        loop = asyncio.get_running_loop()
        loop.add_signal_handler(signal.SIGUSR1, self.control.put_nowait, ("activate", "operator"))
        for sig in (signal.SIGTERM, signal.SIGHUP):
            loop.add_signal_handler(sig, self.close)
        asyncio.create_task(self.client_loop(reader))
        while True:
            kind, value = await self.control.get()
            if kind == "close" or self.closing:
                break
            if kind == "activate":
                if self.activation is None or self.activation.done():
                    self.activation = asyncio.create_task(self.activate(value))
            elif kind == "crashed" and value is self.backend:
                await self.recover(value)
        await self.shutdown()


async def amain(argv: list[str]) -> None:
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader(limit=LINE_LIMIT)
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
