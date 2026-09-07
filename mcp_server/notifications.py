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
            return {
                "transport": transport,
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


async def consume(queue: Queue, adapter):
    """Claim before I/O; ambiguous outcomes never earn an automatic retry."""
    fd = os.open(queue.path / "consumer.lock", os.O_CREAT | os.O_RDWR, 0o600)
    wake = None
    try:
        # A reconnect can briefly overlap the old MCP child. Wait to take over.
        while True:
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                await asyncio.sleep(0.25)
        wake = DirectoryWake(queue.path)
        for event in queue.snapshot()["notifications"]:
            queue.change(event["id"], {"submitting"}, status="uncertain")
        heartbeat = 0.0
        while True:
            if time.monotonic() >= heartbeat:
                state = (
                    adapter.health_status()
                    if hasattr(adapter, "health_status")
                    else "ready"
                )
                queue.health(adapter.engine, state, **adapter.identity())
                heartbeat = time.monotonic() + 5
            events = queue.snapshot()["notifications"]
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
                if event["status"] != "pending":
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
            queue.health(adapter.engine, "stopped", **adapter.identity())
        os.close(fd)


async def consume_supervised(queue: Queue, adapter, retry_s: float = 5):
    """A queue outage disables notifications, not the owning agent session."""
    while True:
        try:
            await consume(queue, adapter)
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
