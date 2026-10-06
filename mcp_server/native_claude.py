"""Claude native notification adapters. Never changes inbound policy."""

import asyncio
import json
import os
import time
from pathlib import Path

from mcp_server import control_client
from mcp_server.notifications import (
    NotSubmitted,
    Queue,
    consume,
    delivery_health,
    transcript_observed,
)

# This many unobserved submissions that Claude only *enqueued* (no dequeue or
# removal afterwards) in this consumer's own transcript mark it stale.
ENQUEUE_ONLY_STALE = 3
TAIL_BYTES = 2 * 1024 * 1024


def _argv(pid: int) -> list[str] | None:
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return None
    return [a.decode(errors="replace") for a in raw.split(b"\x00") if a] or None


def _ppid(pid: int) -> int | None:
    try:
        return int(Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[1])
    except (OSError, IndexError, ValueError):
        return None


def _is_claude(argv: list[str]) -> bool:
    head = os.path.basename(argv[0])
    return head == "claude" or head.startswith("claude-") or (
        head in {"node", "bun"} and any("claude" in a for a in argv[1:3]))


def claude_parent_argv(pid: int | None = None) -> list[str] | None:
    """The owning Claude process: the MCP server's parent, or its grandparent
    when CM's launcher shell sits in between. Nothing further up: a test or a
    nested run must never inspect an unrelated ancestor Claude."""
    current = os.getpid() if pid is None else pid
    for _ in range(2):
        current = _ppid(current)
        if not current or current <= 1:
            return None
        argv = _argv(current)
        if not argv:
            return None
        if _is_claude(argv):
            return argv
        if not (os.path.basename(argv[0]) in {"sh", "bash", "dash"}
                and any(a.endswith("launcher.sh") for a in argv)):
            return None
    return None


def argv_session_id(argv: list[str] | None) -> str | None:
    if not argv:
        return None
    values = {}
    for i, arg in enumerate(argv):
        for flag in ("--session-id", "--resume", "-r"):
            if arg == flag and i + 1 < len(argv):
                values.setdefault(flag, argv[i + 1])
            elif arg.startswith(flag + "="):
                values.setdefault(flag, arg.split("=", 1)[1])
    if values.get("--session-id"):
        return values["--session-id"]
    if "--fork-session" in argv:
        return None  # the fork's new id is not on the command line
    return values.get("--resume") or values.get("-r")


def channel_flag_present(argv: list[str]) -> bool:
    for i, arg in enumerate(argv):
        for flag in ("--dangerously-load-development-channels", "--channels"):
            value = None
            if arg == flag:
                value = " ".join(argv[i + 1:i + 2])
            elif arg.startswith(flag + "="):
                value = arg.split("=", 1)[1]
            if value is not None and "server:claude-manager" in value.replace(",", " ").split():
                return True
    return False


def transcript_tail(path: str | None):
    if not path:
        return
    try:
        with open(path, "rb") as stream:
            stream.seek(0, 2)
            start = max(0, stream.tell() - TAIL_BYTES)
            stream.seek(start)
            if start:
                stream.readline()
            for line in stream:
                try:
                    yield json.loads(line)
                except (ValueError, UnicodeError):
                    continue
    except OSError:
        return


class ClaudeAdapter:
    engine = "claude-code"
    adapter_name = "claude-own-child-v1"

    def __init__(self, uid: str):
        self.uid = uid
        try:
            self.peer_start = (
                Path(f"/proc/{os.getpid()}/stat")
                .read_text()
                .rsplit(") ", 1)[1]
                .split()[19]
            )
        except (OSError, IndexError):
            self.peer_start = None
        self.transcript_path = None
        self.resolve_after = 0.0
        self.initial_bound = None
        self._own_session_id = None
        self._own_transcript_cache = None

    def identity(self):
        # No socket credentials in persistent diagnostics.
        return {
            "session_uid": self.uid,
            "adapter": self.adapter_name,
            "peer_pid": os.getpid(),
            "peer_start": self.peer_start,
        }

    async def _resolve(self):
        # Re-resolve the live binding, but share one lookup across outstanding
        # receipts. A daemon outage must not cost a timeout per queued event.
        if time.monotonic() >= self.resolve_after:
            try:
                resolved = await asyncio.to_thread(
                    control_client.call,
                    "resolve_authorized_session",
                    {"session_uid": self.uid},
                    timeout=2,
                )
                self.transcript_path = resolved.get("transcript_path")
            except (control_client.ControlError, control_client.TransportError):
                self.transcript_path = None
            if self.initial_bound is None and self.transcript_path:
                self.initial_bound = self.transcript_path
            self.resolve_after = time.monotonic() + 1
        return self.transcript_path

    def own_session_id(self):
        """The Claude conversation this MCP child's own client writes to.

        A Claude bg/fork hand-off starts a new client with a new id while the
        old client (and its MCP child) can keep running."""
        if self._own_session_id is None:
            self._own_session_id = (os.environ.get("CLAUDE_CODE_SESSION_ID")
                                    or argv_session_id(claude_parent_argv()) or "")
        return self._own_session_id or None

    def own_transcript(self):
        sid = self.own_session_id()
        bound = self.transcript_path
        if not sid:
            return self.initial_bound
        if bound and Path(bound).stem == sid:
            return bound
        if self._own_transcript_cache and Path(self._own_transcript_cache).exists():
            return self._own_transcript_cache
        if not bound:
            return None  # daemon unreachable: no evidence, and no broad search
        # A fork/resume normally stays in the same project directory.
        candidates = [Path(bound).with_name(f"{sid}.jsonl")]
        if not candidates[0].exists():
            candidates.extend(Path.home().glob(f".claude/projects/*/{sid}.jsonl"))
        for candidate in candidates:
            if candidate.exists():
                self._own_transcript_cache = str(candidate)
                return self._own_transcript_cache
        return None

    def live(self):
        """True: this client owns the daemon-bound transcript. False: it
        provably does not. None: unknown (no id or no binding)."""
        sid, bound = self.own_session_id(), self.transcript_path
        if not sid or not bound:
            return None
        return Path(bound).stem == sid

    def channels_blocked(self):
        return None

    async def takeover_claim(self):
        await self._resolve()
        # A live client with channels disabled still claims: holding the
        # queue it keeps notices pending and reports channels_disabled,
        # instead of a stale client submitting them into a void.
        if self.live() is True:
            return {"session_id": self.own_session_id(),
                    "transcript_path": self.transcript_path}
        return None

    @staticmethod
    def stale_evidence(own, markers):
        """Positive evidence in this client's OWN transcript that it no longer
        consumes notices: the conversation continued in another process, or
        our recent submissions were only enqueued, never processed."""
        continued = False
        enqueued = []
        for item in transcript_tail(own):
            kind = item.get("type")
            if kind == "continued-in":
                continued = True
            elif kind == "queue-operation":
                if item.get("operation") == "enqueue":
                    content = json.dumps(item.get("content"))
                    enqueued.extend(m for m in markers if m and m in content)
                else:  # dequeue / remove: the client is processing its queue
                    enqueued = []
        out = []
        if continued:
            out.append("continued_in")
        if len(set(enqueued)) >= ENQUEUE_ONLY_STALE:
            out.append("enqueued_unobserved")
        return out

    async def diagnostics(self, events):
        await self._resolve()
        live = self.live()
        stale = []
        if live is False:
            stale.append("transcript_mismatch")
        if live is not True:
            own = self.own_transcript()
            if own:
                markers = delivery_health(events)["unobserved_markers"]
                stale.extend(await asyncio.to_thread(self.stale_evidence, own, markers))
        out = {
            "own_session_id": self.own_session_id(),
            "bound_session_id": (Path(self.transcript_path).stem
                                 if self.transcript_path else None),
            "transcript_mismatch": live is False,
            "stale": stale,
        }
        blocked = self.channels_blocked()
        if blocked:
            out["blocked"] = blocked
        return out

    async def observed(self, event):
        await self._resolve()
        return await asyncio.to_thread(
            transcript_observed,
            self.transcript_path,
            self.engine,
            event["marker"],
            event.get("binding"),
        )


class ClaudeSocket(ClaudeAdapter):
    def __init__(self, uid: str):
        super().__init__(uid)
        self.socket = os.environ["CLAUDE_CODE_MESSAGING_SOCKET"].removeprefix("uds:")
        self.token = os.environ.get("CLAUDE_CODE_MESSAGING_TOKEN")

    async def send(self, event):
        try:
            _, writer = await asyncio.wait_for(
                asyncio.open_unix_connection(self.socket), 3
            )
        except (TimeoutError, OSError) as exc:
            raise NotSubmitted from exc
        try:
            if self.token:
                writer.write(
                    (json.dumps({"type": "auth", "token": self.token}) + "\n").encode()
                )
            writer.write(
                (
                    json.dumps(
                        {
                            "type": "user",
                            "from": "claude-manager",
                            "message": {"role": "user", "content": event["text"]},
                        }
                    )
                    + "\n"
                ).encode()
            )
            await asyncio.wait_for(writer.drain(), 3)
            # This interface has no acceptance ACK. Receipt stays unverified
            # until the marker occurs in the actual inbound transcript.
            return {"kind": "socket_write_only"}
        finally:
            writer.close()
            await writer.wait_closed()

async def run(adapter=None):
    queue = Queue.own()
    adapter = adapter or ClaudeSocket(queue.uid)
    while True:
        try:
            # A nested launcher can inherit an ancestor's socket environment.
            # Confirm this CM identity is Claude before consuming its queue.
            resolved = await asyncio.to_thread(
                control_client.call,
                "resolve_authorized_session",
                {"session_uid": queue.uid},
                timeout=3,
            )
            if resolved.get("engine") != "claude-code":
                return
            if await consume(queue, adapter):
                continue  # handed off to a live successor; wait as non-holder
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 - isolate adapter failures from unrelated MCP tools
            queue.health(adapter.engine, "error", error=type(exc).__name__)
            await asyncio.sleep(5)
