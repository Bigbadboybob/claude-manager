"""Atomic planning registration for operator-authorized, host-local adoption.

The authenticated daemon verifies the live session and stamps actor=owner.
Registration reserves ownership; it never starts a scheduler or fires a prompt.
"""
import asyncio
import json
import os
from pathlib import Path
import socket
import stat
from typing import Literal
from uuid import UUID

from fastapi import APIRouter, Depends, HTTPException, Request
from pydantic import BaseModel, ConfigDict, Field

from api.auth import verify_token


class Binding(BaseModel):
    model_config = ConfigDict(extra="forbid")
    daemon_id: UUID
    host: str = Field(min_length=1, max_length=128)
    continuous_task_id: str = Field(pattern=r"^[A-Za-z0-9_-]{1,128}$")
    session_uid: str = Field(min_length=1, max_length=128)
    workspace_id: str = Field(min_length=1, max_length=256)
    worktree_path: str = Field(min_length=1, max_length=4096)
    engine: Literal["claude", "codex"]


class Register(BaseModel):
    model_config = ConfigDict(extra="forbid")
    actor: Literal["owner"]
    request_id: str = Field(pattern=r"^[A-Za-z0-9_-]{1,128}$")
    expected_kind: Literal["oneshot", "continuous"]
    expected_continuous: dict | None
    expected_binding: dict | None
    binding: Binding


class LegacyOwnership(BaseModel):
    # Validate the fields needed to establish ownership, without treating a
    # malformed/unreadable file as absence or depending on the best-effort UI.
    model_config = ConfigDict(strict=True, extra="ignore")
    task_id: str = Field(min_length=1)
    planning_task_id: str | None = None
    workspace_id: str = Field(min_length=1)
    worktree_path: str = Field(min_length=1)
    engine: Literal["claude", "codex", "bash"]


def _inventory_entries(root: Path):
    """Return inventory entries, distinguishing absent storage from dangling paths."""
    probe = root
    while True:
        try:
            root_stat = probe.lstat()
        except FileNotFoundError:
            parent = probe.parent
            if parent == probe:
                return []
            probe = parent
            continue
        if stat.S_ISLNK(root_stat.st_mode):
            raise ValueError(f"suspicious legacy inventory root {probe}")
        if not stat.S_ISDIR(root_stat.st_mode):
            raise ValueError(f"legacy inventory root is not a directory {probe}")
        if probe != root:
            # The configured root itself is absent, but its nearest existing
            # ancestor is sound. Missing storage is an empty inventory.
            return []
        return sorted(root.iterdir())


def _read_state_file(location: Path) -> str | None:
    try:
        state_stat = location.lstat()
    except FileNotFoundError:
        return None
    if stat.S_ISLNK(state_stat.st_mode) or not stat.S_ISREG(state_stat.st_mode):
        raise ValueError(f"suspicious legacy state entry {location}")
    flags = (
        os.O_RDONLY
        | getattr(os, "O_CLOEXEC", 0)
        | getattr(os, "O_NOFOLLOW", 0)
        | getattr(os, "O_NONBLOCK", 0)
    )
    try:
        fd = os.open(location, flags)
    except FileNotFoundError:
        # A state file removed after lstat is genuinely absent; do not broaden
        # this to catch permission or parse errors.
        return None
    try:
        opened = os.fstat(fd)
        if not stat.S_ISREG(opened.st_mode) or (opened.st_dev, opened.st_ino) != (state_stat.st_dev, state_stat.st_ino):
            raise ValueError(f"suspicious legacy state replacement {location}")
        with os.fdopen(fd, "r", encoding="utf-8") as state:
            fd = -1
            return state.read()
    finally:
        if fd >= 0:
            os.close(fd)


def legacy_owners(request: Request, task_id: UUID):
    root = Path(getattr(request.app.state, "continuous_registration_dir",
                        os.path.expanduser(os.environ.get("CM_CONTINUOUS_TASKS_DIR", "~/.cm/continuous-tasks"))))
    host = getattr(request.app.state, "continuous_registration_host",
                   os.environ.get("HOSTNAME") or socket.gethostname()).strip().split(".")[0]
    location = root
    try:
        entries = _inventory_entries(root)
        owners = []
        for entry in entries:
            entry_stat = entry.lstat()
            if stat.S_ISLNK(entry_stat.st_mode):
                raise ValueError(f"suspicious legacy inventory entry {entry}")
            if not stat.S_ISDIR(entry_stat.st_mode):
                continue
            location = entry / "state.json"
            contents = _read_state_file(location)
            if contents is None:
                # Scheduler run logs and lock files legitimately leave a task
                # directory before its durable state exists. They establish no
                # ownership record and are ignored.
                continue
            row = LegacyOwnership.model_validate(json.loads(contents))
            if row.task_id != entry.name:
                raise ValueError(f"state identity differs from directory {entry.name}")
            if row.planning_task_id == str(task_id) or row.task_id == str(task_id):
                owners.append({"host": host, **row.model_dump()})
        return owners
    except (OSError, ValueError) as exc:
        raise HTTPException(409, f"legacy_inventory_unavailable: cannot establish API-host ownership at {location}: {type(exc).__name__}") from exc


def check_legacy_owner(current, binding):
    for old in current["api_host_owners"]:
        if (old["host"] != binding["host"] or old["task_id"] != binding["continuous_task_id"]
                or any(old[k] != binding[k] for k in ("workspace_id", "worktree_path", "engine"))):
            raise HTTPException(409, "legacy_owner_conflict: API-host continuous record already owns this planning task")


router = APIRouter(dependencies=[Depends(verify_token)])


async def snapshot(conn, task_id: UUID, request: Request, *, lock=False):
    row = await conn.fetchrow(
        "SELECT id, kind, status, metadata FROM tasks WHERE id=$1" + (" FOR UPDATE" if lock else ""), task_id)
    if row is None:
        raise HTTPException(404, "Planning task not found")
    binding = await conn.fetchval("SELECT binding FROM continuous_registrations WHERE task_id=$1", task_id)
    metadata = row["metadata"] or {}
    if not isinstance(metadata, dict) or not isinstance(metadata.get("continuous", {}), (dict, type(None))):
        raise HTTPException(409, "Malformed continuous metadata; repair before registration")
    return {"task_id": str(task_id), "kind": row["kind"], "status": row["status"],
            "continuous": metadata.get("continuous"), "binding": binding,
            "api_host_owners": await asyncio.to_thread(legacy_owners, request, task_id)}


@router.get("/tasks/{task_id}/continuous-registration")
async def read_registration(task_id: UUID, request: Request):
    async with request.app.state.pool.acquire() as conn:
        async with conn.transaction(isolation="repeatable_read", readonly=True):
            return await snapshot(conn, task_id, request)


@router.put("/tasks/{task_id}/continuous-registration")
async def register(task_id: UUID, body: Register, request: Request):
    payload = body.model_dump(mode="json")
    binding = payload["binding"]
    async with request.app.state.pool.acquire() as conn:
        async with conn.transaction():
            current = await snapshot(conn, task_id, request, lock=True)
            prior = await conn.fetchrow(
                "SELECT request, binding FROM continuous_registration_requests WHERE task_id=$1 AND request_id=$2",
                task_id, body.request_id)
            if prior:
                if prior["request"] != payload:
                    raise HTTPException(409, "request_id already describes a different adoption")
                # Return the original receipt AND current ownership. An old retry
                # must never steal the task back from a subsequent adoption.
                return {"registered": True, "already_registered": True,
                        "receipt": prior["binding"], "current": current}
            check_legacy_owner(current, binding)
            if current["status"] in ("done", "archived", "cancelled"):
                raise HTTPException(409, "Terminal planning task cannot be adopted")
            if (current["kind"] != body.expected_kind
                    or current["continuous"] != body.expected_continuous
                    or current["binding"] != body.expected_binding):
                raise HTTPException(409, "Planning registration changed; dry-run again")
            if current["binding"] and (current["binding"]["daemon_id"] != binding["daemon_id"]
                                       or current["binding"]["continuous_task_id"] != binding["continuous_task_id"]):
                raise HTTPException(409, "Task belongs to another daemon; cross-host adoption is unsupported")
            if current["continuous"]:
                old = current["continuous"]
                if old.get("host") not in (None, binding["host"]) or old.get("task_id") not in (None, binding["continuous_task_id"]):
                    raise HTTPException(409, "Existing continuous metadata belongs to another scheduler")
            # Merge only our subobject, under the task row lock. No stale whole-
            # metadata PATCH, no status/history/lead/model/description rewrite.
            mirror = dict(current["continuous"] or {})
            mirror.update(task_id=binding["continuous_task_id"], host=binding["host"],
                          engine=binding["engine"], run_mode="persistent")
            await conn.execute(
                "UPDATE tasks SET kind='continuous', metadata=jsonb_set(COALESCE(metadata,'{}'::jsonb), '{continuous}', $2::jsonb), updated_at=now() WHERE id=$1",
                task_id, mirror)
            await conn.execute(
                "INSERT INTO continuous_registrations(task_id,binding) VALUES($1,$2) ON CONFLICT(task_id) DO UPDATE SET binding=EXCLUDED.binding, updated_at=now()",
                task_id, binding)
            await conn.execute(
                "INSERT INTO continuous_registration_requests(task_id,request_id,request,binding) VALUES($1,$2,$3,$4)",
                task_id, body.request_id, payload, binding)
            return {"registered": True, "already_registered": False,
                    "receipt": binding, "current": await snapshot(conn, task_id, request)}
