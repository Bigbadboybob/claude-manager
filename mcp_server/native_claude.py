"""Claude native notification adapters. Never changes inbound policy."""

import asyncio
import json
import os
import time
import uuid
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
BG_PROBE_INTERVAL = 30.0


def recovery_record_state(path, session_id, record_id):
    """Confirm the exact failed main turn, or fail closed on an unclear tail."""
    try:
        if str(uuid.UUID(session_id)) != session_id or str(uuid.UUID(record_id)) != record_id:
            return "unknown"
        path = Path(path)
        if path.stem != session_id:
            return "superseded"
        with path.open("rb") as stream:
            size = os.fstat(stream.fileno()).st_size
            start = max(0, size - TAIL_BYTES)
            stream.seek(start)
            data = stream.read(TAIL_BYTES + 1)
        if len(data) > TAIL_BYTES or not data.endswith(b"\n"):
            return "unknown"
        if start:
            _, sep, data = data.partition(b"\n")
            if not sep:
                return "unknown"
        queue_activity = False
        for line in reversed(data.splitlines()):
            row = json.loads(line)
            if not isinstance(row, dict):
                return "unknown"
            if row.get("isSidechain") is True:
                continue
            kind = row.get("type")
            if kind == "user":
                return "superseded"
            if kind == "assistant":
                if row.get("sessionId") != session_id:
                    return "unknown"
                if (row.get("uuid") != record_id or row.get("error") != "rate_limit"
                        or row.get("isApiErrorMessage") is not True
                        or (row.get("message") or {}).get("model") != "<synthetic>"):
                    return "superseded"
                return "superseded" if queue_activity else "ready"
            if kind in {"queue-operation", "attachment"}:
                queue_activity = True
            elif kind not in {"system", "progress", "file-history-snapshot", "last-prompt", "summary"}:
                return "unknown"
        return "unknown" if start else "superseded"
    except (OSError, ValueError, TypeError, AttributeError):
        return "unknown"


def background_match(rows, session_id, cwd):
    """Only an exact native ID and workspace from Claude's own roster count.

    A short ID, missing cwd, malformed roster or multiple matches is not
    enough evidence to label the session as externally backgrounded.
    """
    try:
        sid = str(uuid.UUID(session_id))
    except (ValueError, TypeError, AttributeError):
        return None
    if session_id != sid:
        return None
    if not isinstance(rows, list) or not cwd:
        return None
    found = [r for r in rows if isinstance(r, dict) and r.get("kind") == "background"
             and r.get("sessionId") == sid]
    if (len(found) != 1 or not isinstance(found[0].get("cwd"), str)
            or os.path.realpath(found[0]["cwd"]) != os.path.realpath(cwd)):
        return None
    return found[0]


async def background_roster():
    """Bounded read-only supported Claude CLI query; failure means unknown."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "claude", "agents", "--json", stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.DEVNULL,
        )
    except OSError:
        return None
    async def collect():
        chunks = []
        size = 0
        while True:
            chunk = await proc.stdout.read(min(65536, 1024 * 1024 + 1 - size))
            if not chunk:
                break
            chunks.append(chunk)
            size += len(chunk)
            if size > 1024 * 1024:
                return None
        await proc.wait()
        return b"".join(chunks)

    try:
        out = await asyncio.wait_for(collect(), 3)
    finally:
        if proc.returncode is None:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
            await proc.wait()
    if out is None or proc.returncode != 0:
        return None
    try:
        return json.loads(out)
    except (ValueError, UnicodeError):
        return None


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


def _start(pid: int) -> str | None:
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()[19]
    except (OSError, IndexError):
        return None


def _is_claude(argv: list[str]) -> bool:
    head = os.path.basename(argv[0])
    return head == "claude" or head.startswith("claude-") or (
        head in {"node", "bun"} and any("claude" in a for a in argv[1:3]))


def claude_parent_argv(pid: int | None = None) -> list[str] | None:
    """The owning Claude process: the MCP server's parent, or a nearer
    ancestor when only CM's launcher shell and/or CM's stdio proxy
    (mcp_proxy.py) sit in between. Nothing further up: a test or a nested
    run must never inspect an unrelated ancestor Claude."""
    current = os.getpid() if pid is None else pid
    for _ in range(3):
        current = _ppid(current)
        if not current or current <= 1:
            return None
        argv = _argv(current)
        if not argv:
            return None
        if _is_claude(argv):
            return argv
        if not _is_cm_intermediary(argv):
            return None
    return None


def _is_cm_intermediary(argv: list[str]) -> bool:
    head = os.path.basename(argv[0])
    if head in {"sh", "bash", "dash"}:
        return any(a.endswith("launcher.sh") for a in argv)
    return head.startswith("python") and len(argv) > 1 and argv[1].endswith("mcp_proxy.py")


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
        self.resolved_session = None
        self.resolve_after = 0.0
        self.initial_bound = None
        self._own_session_id = None
        self._own_transcript_cache = None
        self._bg_probe_after = 0.0
        self._bg_result = None
        self._bg_identity = None

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
                self.resolved_session = resolved
            except (control_client.ControlError, control_client.TransportError):
                self.transcript_path = None
                self.resolved_session = None
            if self.initial_bound is None and self.transcript_path:
                self.initial_bound = self.transcript_path
            self.resolve_after = time.monotonic() + 1
        return self.transcript_path

    def own_session_id(self):
        """The Claude conversation this MCP child's own client writes to.

        A Claude bg/fork hand-off starts a new client with a new id while the
        old client (and its MCP child) can keep running."""
        if self._own_session_id is None:
            # The client parent is evidence about this particular channel;
            # an inherited environment can name a later background fork.
            self._own_session_id = (argv_session_id(claude_parent_argv())
                                    or os.environ.get("CLAUDE_CODE_SESSION_ID") or "")
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
        # An inherited environment variable is useful diagnostics, but does
        # not prove which client owns this MCP child after a native fork.
        # Only the owning Claude process's explicit argv can authorize a
        # channel takeover.
        parent_id = argv_session_id(claude_parent_argv())
        if parent_id and self.peer_start and self.live() is True and parent_id == self.own_session_id():
            return {"session_id": parent_id,
                    "transcript_path": self.transcript_path,
                    "peer_start": self.peer_start}
        return None

    def valid_takeover_request(self, claim):
        """Recheck the waiting process, its parent and the current binding."""
        pid = claim.get("pid")
        if not isinstance(pid, int) or pid <= 0 or not self.transcript_path:
            return False
        sid = Path(self.transcript_path).stem
        return (claim.get("session_id") == sid
                and claim.get("transcript_path") == self.transcript_path
                and claim.get("peer_start") is not None
                and _start(pid) == claim["peer_start"]
                and argv_session_id(claude_parent_argv(pid)) == sid
                and _start(pid) == claim["peer_start"])

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
        own = self.own_transcript()
        if own:
            markers = delivery_health(events)["unobserved_markers"]
            evidence = await asyncio.to_thread(self.stale_evidence, own, markers)
            # `continued-in` alone can be an in-pane resume. Queue-only
            # submissions are stronger evidence even when inherited identity
            # misleadingly equals the daemon's binding.
            if live is True and "continued_in" not in evidence:
                # A busy client can legitimately enqueue notices for a long
                # turn. Enqueues alone do not prove its channel is stale.
                evidence = [x for x in evidence if x != "enqueued_unobserved"]
            stale.extend(x for x in evidence if x != "continued_in" or live is not True)
        out = {
            "own_session_id": self.own_session_id(),
            "bound_session_id": (Path(self.transcript_path).stem
                                 if self.transcript_path else None),
            "transcript_mismatch": live is False,
            "stale": stale,
        }
        blocked = self.channels_blocked()
        # A native /bg can keep a live conversation outside the CM holder
        # while this child's MCP connection is still attached to the old UI.
        # Query only when delivery has stalled; an unavailable roster is not
        # evidence. An actual background-host parent with the matching ID owns
        # its own channel, so don't block it merely for being backgrounded.
        if delivery_health(events)["stalled"] and self.transcript_path:
            identity = (Path(self.transcript_path).stem, os.getcwd())
            if identity != self._bg_identity:
                self._bg_identity = identity
                self._bg_result = None
                self._bg_probe_after = 0.0
            if time.monotonic() >= self._bg_probe_after:
                self._bg_probe_after = time.monotonic() + BG_PROBE_INTERVAL
                try:
                    rows = await background_roster()
                except TimeoutError:
                    rows = None
                self._bg_result = background_match(rows, *identity)
            if self._bg_result:
                sid = Path(self.transcript_path).stem
                argv = claude_parent_argv() or []
                owns_bg = "bg-pty-host" in argv and argv_session_id(argv) == sid
                out["background_session_id"] = sid
                out["background_state"] = self._bg_result.get("state")
                if not owns_bg:
                    blocked = "external_background"
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

    async def recovery_check(self, event):
        parts = event.get("id", "").split(":")
        if len(parts) != 3 or parts[0] != "claude-limit":
            return "unknown"
        # A cached binding is not sufficient at the effect boundary.
        self.resolve_after = 0.0
        await self._resolve()
        resolved = self.resolved_session
        scope = event.get("recovery_scope")
        if not isinstance(resolved, dict) or not isinstance(scope, dict):
            return "unknown"
        if (not isinstance(scope.get("manager"), str) or not scope["manager"]
                or not isinstance(scope.get("generation"), int)):
            return "unknown"
        if (resolved.get("claude_recovery_eligible") is not True
                or resolved.get("managed_by_uid") != scope["manager"]
                or resolved.get("generation") != scope["generation"]
                or resolved.get("workflow_run_id") is not None
                or resolved.get("continuous_task_id") is not None):
            return "superseded"
        if self.live() is False:
            return "superseded"
        if self.live() is not True or not self.transcript_path:
            return "unknown"
        if Path(self.transcript_path).stem != parts[1]:
            return "superseded"
        return await asyncio.to_thread(recovery_record_state, self.transcript_path, parts[1], parts[2])


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
