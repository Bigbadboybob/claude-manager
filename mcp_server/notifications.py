"""Host-local durable agent notifications. No terminal input, no implicit retries.

Wire format shared with daemon/src/notifications.rs. All mutations take the
directory's flock; one consumer holds consumer.lock for its entire lifetime.
"""

from __future__ import annotations

import asyncio
import ctypes
import fcntl
import hashlib
import json
import os
import tempfile
import time
from contextlib import contextmanager, suppress
from pathlib import Path

TERMINAL = {"observed", "cancelled"}
MAX_EVENTS = 512
MAX_TEXT_BYTES = 65536
# Submitted notices with no inbound observation for this long mark the
# transport degraded (the daemon raises an Owner alert at its own horizon).
UNOBSERVED_DEGRADED_S = float(os.environ.get("CM_NOTIFY_UNOBSERVED_DEGRADED_S", 300))
# A waiting consumer refreshes its takeover request at least this often.
TAKEOVER_FRESH_S = 15.0
# A holder that just acquired the lock keeps it at least this long, so two
# live processes can never alternate ownership faster than this.
HANDOFF_MIN_HOLD_S = 30.0


def delivery_health(events, now: float | None = None, threshold: float | None = None):
    """Submitted/uncertain notices newer than the latest observation that are
    still unobserved past `threshold`. A later observation proves the path
    works again, so older losses do not keep the session degraded forever."""
    now = time.time() if now is None else now
    threshold = UNOBSERVED_DEGRADED_S if threshold is None else threshold
    last_observed = max(
        (e.get("updated_at", e.get("created_at", 0)) for e in events
         if e.get("status") == "observed"),
        default=0,
    )
    stalled = [
        e for e in events
        if e.get("status") in {"submitted", "uncertain"}
        and e.get("updated_at", e.get("created_at", 0)) > last_observed
    ]
    oldest = min((e.get("updated_at", e.get("created_at", now)) for e in stalled), default=None)
    age = None if oldest is None else max(0.0, now - oldest)
    return {
        "unobserved": len(stalled),
        "oldest_unobserved_s": None if age is None else round(age),
        "stalled": age is not None and age >= threshold,
        "unobserved_markers": [e.get("marker") for e in stalled],
    }


def atomic_write(path: Path, value: dict) -> None:
    fd, tmp = tempfile.mkstemp(prefix=".tmp-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, ensure_ascii=False)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, path)
        parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


class Queue:
    def __init__(self, uid: str, root: Path | None = None):
        if not uid:
            raise ValueError("CM session identity is required")
        self.uid = uid
        root = root or Path.home() / ".cm"
        self.path = root / "notifications" / hashlib.sha256(uid.encode()).hexdigest()
        self.path.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.lock_path = self.path / "queue.lock"

    @classmethod
    def own(cls):
        return cls(os.environ.get("CM_TUI_SESSION_ID", ""))

    @contextmanager
    def locked(self):
        fd = os.open(self.lock_path, os.O_CREAT | os.O_RDWR, 0o600)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX)
            yield
        finally:
            os.close(fd)

    def event_path(self, event_id: str) -> Path:
        return self.path / (hashlib.sha256(event_id.encode()).hexdigest() + ".json")

    def _events(self):
        return [
            json.loads(p.read_text())
            for p in self.path.glob("*.json")
            if p.name != "transport.json"
        ]

    # Consumer handoff. A waiting consumer that proves it is attached to the
    # daemon-bound conversation files a request; the holder releases only with
    # its own positive staleness evidence. Not *.json: never parsed as events.
    def takeover_path(self, pid: int) -> Path:
        return self.path / f"takeover-{pid}.request"

    def request_takeover(self, claim: dict) -> None:
        with suppress(OSError):
            atomic_write(self.takeover_path(os.getpid()),
                         dict(claim, pid=os.getpid(), requested_at=time.time()))

    def withdraw_takeover(self, pid: int | None = None) -> None:
        with suppress(OSError):
            self.takeover_path(pid or os.getpid()).unlink(missing_ok=True)

    def takeover_requests(self, now: float | None = None) -> list[dict]:
        now = time.time() if now is None else now
        out = []
        for path in self.path.glob("takeover-*.request"):
            try:
                claim = json.loads(path.read_text())
            except (OSError, ValueError):
                continue
            pid = claim.get("pid")
            if pid == os.getpid() or not isinstance(pid, int):
                continue
            alive = True
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                alive = False
            except PermissionError:
                pass
            if not alive or now - claim.get("requested_at", 0) > TAKEOVER_FRESH_S:
                with suppress(OSError):
                    path.unlink(missing_ok=True)
                continue
            out.append(claim)
        return out

    def get(self, event_id: str):
        with self.locked():
            try:
                return json.loads(self.event_path(event_id).read_text())
            except FileNotFoundError:
                return None

    def publish(self, event_id: str, source: str, text: str, marker: str):
        if not text or len(text.encode()) > MAX_TEXT_BYTES:
            raise ValueError("notification must contain 1–65536 UTF-8 bytes")
        with self.locked():
            path = self.event_path(event_id)
            if path.exists():
                old = json.loads(path.read_text())
                if (old["source"], old["text"], old["marker"]) != (
                    source,
                    text,
                    marker,
                ):
                    raise ValueError("notification ID already has different content")
                return old
            events = self._events()
            if len(events) >= MAX_EVENTS:
                for old in sorted(events, key=lambda e: e["created_at"]):
                    if old["status"] in TERMINAL:
                        self.event_path(old["id"]).unlink()
                        break
                else:
                    raise RuntimeError(
                        "notification queue full; inspect notification_status"
                    )
            event = {
                "version": 1,
                "id": event_id,
                "recipient": self.uid,
                "source": source,
                "text": text,
                "marker": marker,
                "status": "pending",
                "created_at": time.time(),
            }
            try:
                atomic_write(path, event)
            except OSError:
                # Do not expose an uncommitted publication to a consumer after
                # a failed directory sync. We still hold the queue lock.
                path.unlink(missing_ok=True)
                raise
            return event

    def change(self, event_id: str, allowed: set[str], **fields):
        with self.locked():
            path = self.event_path(event_id)
            try:
                event = json.loads(path.read_text())
            except FileNotFoundError:
                return None
            if event["status"] in allowed:
                event.update(fields, updated_at=time.time())
                atomic_write(path, event)
            return event

    def cancel(self, event_id: str):
        # Once claimed, cancellation cannot retract an accepted native event.
        return self.change(event_id, {"pending"}, status="cancelled")

    def snapshot(self):
        with self.locked():
            events = self._events()
            try:
                transport = json.loads((self.path / "transport.json").read_text())
                transport["connected"] = (
                    transport.get("status") == "ready"
                    and time.time() - transport["updated_at"] < 15
                )
            except FileNotFoundError:
                transport = {
                    "connected": False,
                    "status": "native_transport_unavailable",
                }
            delivery = delivery_health(events)
            delivery.pop("unobserved_markers")
            reasons = []
            if transport.get("status") == "degraded":
                reasons.extend(transport.get("reasons") or [transport.get("reason") or "degraded"])
            if delivery["stalled"]:
                reasons.append("unobserved_submissions")
            # Never report a consumer as healthy while its submissions vanish.
            if delivery["stalled"]:
                transport["connected"] = False
            health = {
                "status": ("degraded" if reasons else
                           "ready" if transport["connected"] else "disconnected"),
                "reasons": sorted(set(reasons)),
                **delivery,
            }
            return {
                "transport": transport,
                "health": health,
                "notifications": sorted(events, key=lambda e: e["created_at"]),
            }

    def health(self, engine: str, state: str, **extra):
        # A failed heartbeat expires to disconnected; it must not kill an agent.
        with suppress(OSError):
            atomic_write(
                self.path / "transport.json",
                dict(
                    version=1,
                    engine=engine,
                    status=state,
                    updated_at=time.time(),
                    **extra,
                ),
            )


class DirectoryWake:
    """inotify on Linux; bounded 250ms fallback elsewhere. Arm before scanning."""

    def __init__(self, path: Path):
        self.fd = -1
        self.event = asyncio.Event()
        if os.name == "posix":
            try:
                libc = ctypes.CDLL(None, use_errno=True)
                self.fd = libc.inotify_init1(os.O_NONBLOCK | os.O_CLOEXEC)
                if self.fd >= 0:
                    # IN_MOVED_TO: only committed atomic writes, not lock chatter.
                    if libc.inotify_add_watch(self.fd, os.fsencode(path), 0x80) < 0:
                        self.close()
                    else:
                        asyncio.get_running_loop().add_reader(self.fd, self._ready)
            except AttributeError:
                pass

    def _ready(self):
        try:
            os.read(self.fd, 65536)
        except BlockingIOError:
            pass
        self.event.set()

    async def wait(self):
        try:
            await asyncio.wait_for(self.event.wait(), 1 if self.fd >= 0 else 0.25)
        except TimeoutError:
            pass
        self.event.clear()

    def close(self):
        if self.fd >= 0:
            asyncio.get_running_loop().remove_reader(self.fd)
            os.close(self.fd)
            self.fd = -1


class NotSubmitted(Exception):
    """Adapter knows that no notification bytes were submitted. Retry is safe."""


async def _diagnose(queue: Queue, adapter, events) -> dict:
    """Health state plus positive staleness evidence for this consumer."""
    diag = {}
    if hasattr(adapter, "diagnostics"):
        try:
            diag = dict(await adapter.diagnostics(events) or {})
        except Exception as exc:  # noqa: BLE001 - diagnostics never stop delivery
            diag = {"diagnostics_error": type(exc).__name__}
    delivery = delivery_health(events)
    reasons = []
    if diag.get("blocked"):
        reasons.append(diag["blocked"])
    stale = list(diag.get("stale") or [])
    # An in-pane resume or clear legitimately moves the bound transcript
    # away from this client's launch id while delivery keeps working, so a
    # mismatch alone is informational; with stalled work or other evidence
    # it is the reason delivery is failing.
    if diag.get("transcript_mismatch") and (
            delivery["stalled"] or any(x != "transcript_mismatch" for x in stale)):
        reasons.append("transcript_mismatch")
    if delivery["stalled"]:
        reasons.append("unobserved_submissions")
    base = adapter.health_status() if hasattr(adapter, "health_status") else "ready"
    extra = {k: v for k, v in diag.items() if k not in {"blocked", "stale"}}
    if stale:
        extra["stale_evidence"] = stale
    if reasons:
        extra["reason"] = reasons[0]
        extra["reasons"] = reasons
        if delivery["unobserved"]:
            extra["unobserved"] = delivery["unobserved"]
            extra["oldest_unobserved_s"] = delivery["oldest_unobserved_s"]
    degraded = diag.get("blocked") or (reasons and base == "ready")
    return {"status": "degraded" if degraded else base,
            "extra": extra, "stale": stale, "blocked": diag.get("blocked")}


async def _wait_for_lock(queue: Queue, adapter, fd: int):
    """A reconnect can briefly overlap the old MCP child. Wait to take over.

    While waiting, a consumer that can prove it serves the daemon-bound
    conversation asks the holder to hand over. The holder decides."""
    claimed_at = 0.0
    handed_off_at = getattr(adapter, "cm_handed_off_at", None)
    try:
        while True:
            # Right after handing off, stand aside while the successor's
            # request is fresh; never race it for the lock just released.
            yielding = (handed_off_at is not None
                        and time.monotonic() - handed_off_at < 2 * TAKEOVER_FRESH_S
                        and queue.takeover_requests())
            if not yielding:
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    return
                except BlockingIOError:
                    pass
            if hasattr(adapter, "takeover_claim") and time.monotonic() - claimed_at >= 2:
                claimed_at = time.monotonic()
                try:
                    claim = await adapter.takeover_claim()
                except Exception:  # noqa: BLE001 - a failed probe only means no claim
                    claim = None
                if claim:
                    queue.request_takeover(claim)
                else:
                    queue.withdraw_takeover()
            await asyncio.sleep(0.25)
    finally:
        queue.withdraw_takeover()


def _handoff_reason(queue: Queue, assessment: dict, held_for: float):
    """Release only with this holder's own positive staleness evidence AND a
    fresh request from a consumer attached to the daemon-bound transcript."""
    if held_for < HANDOFF_MIN_HOLD_S or not assessment["stale"]:
        return None
    if not queue.takeover_requests():
        return None
    return assessment["stale"][0]


async def consume(queue: Queue, adapter):
    """Claim before I/O; ambiguous outcomes never earn an automatic retry.

    Returns (instead of running forever) only after handing the queue to a
    live successor; callers loop and wait again as an ordinary non-holder."""
    fd = os.open(queue.path / "consumer.lock", os.O_CREAT | os.O_RDWR, 0o600)
    wake = None
    handed_off = None
    try:
        await _wait_for_lock(queue, adapter, fd)
        acquired = time.monotonic()
        wake = DirectoryWake(queue.path)
        for event in queue.snapshot()["notifications"]:
            queue.change(event["id"], {"submitting"}, status="uncertain")
        heartbeat = 0.0
        blocked = None
        while True:
            events = queue.snapshot()["notifications"]
            if time.monotonic() >= heartbeat:
                assessment = await _diagnose(queue, adapter, events)
                blocked = assessment["blocked"]
                queue.health(adapter.engine, assessment["status"],
                             **adapter.identity(), **assessment["extra"])
                heartbeat = time.monotonic() + 5
                handed_off = _handoff_reason(queue, assessment, time.monotonic() - acquired)
                if handed_off:
                    with suppress(AttributeError):
                        adapter.cm_handed_off_at = time.monotonic()
                    return handed_off
            # New wakes must not sit behind slow receipt checks for older input.
            events.sort(key=lambda event: event["status"] != "pending")
            for event in events:
                if event["version"] != 1 or event["recipient"] != queue.uid:
                    continue  # Never deliver an unknown envelope or another identity.
                if event["status"] in {"submitted", "uncertain"}:
                    if await adapter.observed(event):
                        queue.change(
                            event["id"], {"submitted", "uncertain"}, status="observed"
                        )
                    continue
                if event["status"] != "pending" or blocked:
                    # A client known to drop these events keeps them pending
                    # (retractable, never ambiguously submitted).
                    continue
                event = queue.change(
                    event["id"],
                    {"pending"},
                    status="submitting",
                    binding=adapter.identity(),
                )
                if not event or event["status"] != "submitting":
                    continue
                try:
                    receipt = await adapter.send(event)
                except NotSubmitted:
                    queue.change(event["id"], {"submitting"}, status="pending")
                    queue.health(adapter.engine, "disconnected", **adapter.identity())
                    await asyncio.sleep(1)
                    break
                except asyncio.CancelledError:
                    with suppress(OSError):
                        queue.change(event["id"], {"submitting"}, status="uncertain")
                    raise
                except Exception as exc:  # noqa: BLE001 - any adapter failure after claim is ambiguous
                    # Error class only: native exceptions can contain auth material.
                    queue.change(
                        event["id"],
                        {"submitting"},
                        status="uncertain",
                        error=type(exc).__name__,
                    )
                else:
                    queue.change(
                        event["id"], {"submitting"}, status="submitted", receipt=receipt
                    )
            await wake.wait()
    finally:
        if wake is not None:
            wake.close()
            if handed_off:
                queue.health(adapter.engine, "handed_off", reason=handed_off,
                             **adapter.identity())
            else:
                queue.health(adapter.engine, "stopped", **adapter.identity())
        os.close(fd)


async def consume_supervised(queue: Queue, adapter, retry_s: float = 5):
    """A queue outage disables notifications, not the owning agent session."""
    while True:
        try:
            if await consume(queue, adapter):
                continue  # handed off: wait again as an ordinary non-holder
        except asyncio.CancelledError:
            raise
        except Exception as exc:  # noqa: BLE001 - preserve agent runtime on queue/adapter failure
            queue.health(
                adapter.engine, "error", error=type(exc).__name__, **adapter.identity()
            )
            await asyncio.sleep(retry_s)


def transcript_observed(
    path: str | None, engine: str, marker: str, binding: dict | None = None
) -> bool:
    """Positive evidence only, in inbound records; assistant quotes don't count."""
    if not path or not marker:
        return False
    try:
        with open(path, "rb") as stream:
            stream.seek(0, 2)
            start = max(0, stream.tell() - 2 * 1024 * 1024)
            stream.seek(start)
            if start:
                stream.readline()
            for line in stream:
                try:
                    item = json.loads(line)
                except (ValueError, UnicodeError):
                    continue
                if engine == "claude-code":
                    content = item.get("message", {})
                    origin = item.get("origin", {})
                    expected = binding or {}
                    channel_server = expected.get("channel_server")
                    if channel_server and expected.get("adapter") == "claude-mcp-channel-v1":
                        # Claude records idle events as meta user records and
                        # busy events as queued_command attachments. Require
                        # native origin; a user/assistant quoting <channel> or
                        # an enqueue record is not positive delivery evidence.
                        attachment = item.get("attachment", {})
                        if (
                            item.get("type") == "user"
                            and item.get("isMeta") is True
                            and content.get("role") == "user"
                            and origin == {"kind": "channel", "server": channel_server}
                            and marker in json.dumps(content.get("content"))
                        ) or (
                            item.get("type") == "attachment"
                            and attachment.get("type") == "queued_command"
                            and attachment.get("origin") == {"kind": "channel", "server": channel_server}
                            and marker in json.dumps(attachment.get("prompt"))
                        ):
                            return True
                        continue
                    if (
                        item.get("type") == "user"
                        and content.get("role") == "user"
                        and origin.get("kind") == "peer"
                        and origin.get("from") == "claude-manager"
                        and origin.get("selfSent") is True
                        and marker in json.dumps(content.get("content"))
                    ):
                        return True
                    # At a tool checkpoint Claude materializes the peer input
                    # as a queued_command attachment, not an idle user record.
                    # A queue-operation alone is never receipt evidence.
                    attachment = item.get("attachment", {})
                    origin = attachment.get("origin", {})
                    expected = binding or {}
                    if (
                        item.get("type") == "attachment"
                        and attachment.get("type") == "queued_command"
                        and origin.get("kind") == "peer"
                        and origin.get("from") == "claude-manager"
                        and expected.get("peer_pid") is not None
                        and origin.get("verifiedPeerPid") == expected["peer_pid"]
                        and (
                            not expected.get("peer_start")
                            or str(origin.get("verifiedPeerProcStart"))
                            == expected["peer_start"]
                        )
                        and marker in json.dumps(attachment.get("prompt"))
                    ):
                        return True
                elif item.get("type") == "response_item":
                    payload = item.get("payload", {})
                    if (
                        payload.get("type") == "function_call_output"
                        and payload.get("name") == "cm_notification"
                        and marker in json.dumps(payload.get("output"))
                    ):
                        return True
    except OSError:
        pass
    return False
