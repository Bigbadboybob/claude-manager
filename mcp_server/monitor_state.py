"""Durable monitor obligations, owned by each session's MCP producers.

A producer restart never erases another producer's unfinished deliveries.
The daemon reads this journal for continuous drain; the retained result also
lets an agent inspect a notification after its original MCP process exits.
No credentials, full transcripts, or asyncio objects are written.
"""
from __future__ import annotations

import fcntl
import json
import os
from pathlib import Path
import re
import secrets
import tempfile
import time

PRODUCER_ID = secrets.token_hex(16)
MAX_BYTES = 4 * 1024 * 1024
SETTLED = frozenset({"delivered", "cancelled", "replaced"})
FIELDS = frozenset({
    "monitor_id", "watching", "mode", "until", "note", "source", "state",
    "created_at", "result", "delivered", "delivery_uncertain",
    "notification_id", "delivery_status", "cancellation_retracted",
})


def _refresh_native_receipts(uid: str, records: dict[str, dict]) -> bool:
    """Reconcile retained producers with native receipts, without replaying work.

    A receipt can arrive after the original MCP producer stopped waiting or
    exited. Only records explicitly bound to a native notification qualify;
    legacy delivery ambiguity and interrupted watches remain outstanding.
    """
    from mcp_server.notifications import Queue
    queue = None
    changed = False
    for mid, record in records.items():
        event_id = record.get("notification_id")
        if event_id != f"monitor:{mid}":
            continue
        queue = queue or Queue(uid)
        event = queue.get(event_id)
        if not event or event.get("recipient") != uid or event.get("source") != "session_monitor":
            continue
        status = event["status"]
        fields = {"delivery_status": status,
                  "delivery_uncertain": status not in {"observed", "cancelled"},
                  "delivered": status == "observed"}
        if status == "observed" and record.get("state") not in {"cancelled", "replaced"}:
            fields["state"] = "delivered"
        elif status == "cancelled":
            fields["state"] = "cancelled"
        changed |= any(record.get(key) != value for key, value in fields.items())
        record.update(fields)
    return changed


def _path() -> Path:
    uid = os.environ.get("CM_TUI_SESSION_ID", "")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,160}", uid):
        raise ValueError("missing or unsafe monitor owner UID")
    return Path.home() / ".cm" / "monitor-state" / (uid + ".json")


def _load(path: Path) -> dict:
    try:
        with path.open("rb") as source:
            data = source.read(MAX_BYTES + 1)
    except FileNotFoundError:
        return {"schema_version": 1, "coverage_version": 1 if os.environ.get("CM_MONITOR_TRACKING_V1") == "1" else 0, "session_uid": path.stem, "revision": 0, "producers": {}}
    if len(data) > MAX_BYTES:
        raise ValueError("monitor journal exceeds size limit")
    value = json.loads(data)
    if not isinstance(value, dict) or value.get("schema_version") != 1 or value.get("session_uid") != path.stem or not isinstance(value.get("revision"), int) or value["revision"] < 1 or not isinstance(value.get("producers"), dict):
        raise ValueError("invalid monitor journal")
    for producer in value["producers"].values():
        if not isinstance(producer, dict) or not isinstance(producer.get("records"), dict) or any(not isinstance(record, dict) for record in producer["records"].values()):
            raise ValueError("invalid monitor producer")
    return value


def publish(records: dict[str, dict]) -> None:
    """Atomically merge this producer; retain its pruned unfinished records.

    A failed pre-delivery write must prevent delivery. A failed post-delivery
    write leaves the earlier pending obligation, which blocks drain safely.
    """
    path = _path()
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    lock_fd = os.open(str(path) + ".lock", os.O_CREAT | os.O_RDWR, 0o600)
    with os.fdopen(lock_fd, "a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        journal = _load(path)
        old = journal["producers"].get(PRODUCER_ID, {})
        # Never lose an unverified delivery merely because the in-memory
        # history was pruned. Completed history is bounded per producer.
        retained = {
            key: value for key, value in old.get("records", {}).items()
            if value.get("state") not in SETTLED or value.get("delivery_uncertain")
        }
        retained.update({
            key: {field: value for field, value in record.items() if field in FIELDS}
            for key, record in records.items()
        })
        changed = _refresh_native_receipts(path.stem, retained)
        for producer_id, producer in journal["producers"].items():
            if producer_id != PRODUCER_ID:
                changed |= _refresh_native_receipts(path.stem, producer["records"])
        if old and old.get("records") == retained and not changed:
            return
        journal["revision"] += 1
        journal["producers"][PRODUCER_ID] = {
            "started_at": old.get("started_at", time.time()),
            "updated_at": time.time(),
            "records": retained,
        }
        data = json.dumps(journal, sort_keys=True, allow_nan=False).encode()
        if len(data) > MAX_BYTES:
            raise ValueError("monitor journal exceeds size limit; inspect retained obligations")
        fd, temporary = tempfile.mkstemp(prefix=path.name + ".", suffix=".tmp", dir=path.parent)
        try:
            with os.fdopen(fd, "wb") as output:
                output.write(data)
                output.flush()
                os.fsync(output.fileno())
            os.replace(temporary, path)
            directory_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)


def read() -> dict:
    """Read retained obligations without claiming an absent journal is empty."""
    path = _path()
    if not path.exists():
        return {"available": False, "producers": {}}
    return {"available": True, **_load(path)}
