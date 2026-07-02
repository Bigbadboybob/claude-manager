import asyncio
import glob
import json
import logging
import os
import re
from contextlib import asynccontextmanager
from datetime import datetime, timezone

from fastapi import FastAPI, Depends, HTTPException, Query

from api.auth import verify_token
from api.models import TaskCreate, TaskUpdate, TaskResponse
from api.dispatch_daemon import dispatch_loop, warm_pool_loop
from dispatch import db
from dispatch.config import DB_DSN

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger("cm.api")


async def _delete_vm_bg(vm_name: str) -> None:
    """Delete a worker VM via gcloud, swallowing failures with a logged exception.

    Fire-and-forget helper used by request handlers that want VM teardown to
    happen out of band. Callers should schedule this with
    ``_spawn_vm_deletion(vm_name)`` so the task handle is tracked on
    ``app.state.pending_vm_deletions`` and awaited at shutdown.
    """
    try:
        from dispatch.vm import delete_worker
        await asyncio.to_thread(delete_worker, vm_name)
        logger.info(f"Deleted VM {vm_name}")
    except Exception:
        logger.exception(f"Failed to delete VM {vm_name}")


def _spawn_vm_deletion(vm_name: str) -> asyncio.Task:
    """Schedule a background VM deletion and track the handle for shutdown."""
    pending = app.state.pending_vm_deletions
    task = asyncio.create_task(_delete_vm_bg(vm_name))
    pending.add(task)
    task.add_done_callback(pending.discard)
    return task


@asynccontextmanager
async def lifespan(app: FastAPI):
    # Startup
    app.state.pool = await db.get_pool()
    await db.init_db(app.state.pool)
    app.state.pending_vm_deletions: set[asyncio.Task] = set()
    # Two independent loops so a slow warm-pool maintenance pass (serial
    # gcloud probes, gcloud instances create) doesn't drift the dispatch
    # cadence past its 10s target.
    app.state.dispatch_task = asyncio.create_task(dispatch_loop(app.state.pool))
    app.state.warm_pool_task = asyncio.create_task(warm_pool_loop(app.state.pool))
    logger.info("API server started")
    yield
    # Shutdown
    for task in (app.state.dispatch_task, app.state.warm_pool_task):
        task.cancel()
    for task in (app.state.dispatch_task, app.state.warm_pool_task):
        try:
            await task
        except asyncio.CancelledError:
            pass
    if app.state.pending_vm_deletions:
        await asyncio.gather(
            *app.state.pending_vm_deletions, return_exceptions=True
        )
    await app.state.pool.close()
    logger.info("API server stopped")


app = FastAPI(title="Claude Manager", lifespan=lifespan)


def get_pool():
    return app.state.pool


def _slugify(text: str) -> str:
    """Convert text to a URL-friendly slug."""
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:50]


# ---------------------------------------------------------------------------
# Tasks
# ---------------------------------------------------------------------------

# Columns where NULL is a legal value in the DB. PATCH callers are allowed
# to send explicit JSON null for these to clear the column. Everything
# else in TaskUpdate maps to a NOT NULL column; an explicit null there is
# a client bug and we reject it with 400 rather than letting Postgres
# raise a generic 500.
# Note: ``prompt`` is column-nullable but treated as a string downstream
# (cli list views, /workers preview, dispatch_daemon), so allowing null
# would just shift the 500 from Postgres to a TypeError elsewhere.
NULLABLE_TASK_FIELDS = frozenset({
    "name",
    "worker_vm", "worker_zone", "ttyd_url",
    "blocked_at", "session_id", "wip_branch",
    "project", "slug", "description", "difficulty", "depends",
    "parent_task_id",
    "metadata",
})

@app.post("/tasks", response_model=TaskResponse, dependencies=[Depends(verify_token)])
async def create_task(body: TaskCreate, pool=Depends(get_pool)):
    prompt = body.prompt or ""

    # Auto-generate slug from name if not provided
    slug = body.slug
    if not slug and body.name:
        slug = _slugify(body.name)

    task = await db.add_task(
        pool, body.repo_url, body.repo_branch, prompt, body.priority,
        status=body.status, project=body.project, slug=slug, name=body.name,
        description=body.description, difficulty=body.difficulty,
        depends=body.depends, source=body.source, is_cloud=body.is_cloud,
        parent_task_id=body.parent_task_id, worktree_mode=body.worktree_mode,
        wip_branch=body.wip_branch, metadata=body.metadata,
    )
    return task


@app.get("/tasks", response_model=list[TaskResponse], dependencies=[Depends(verify_token)])
async def list_tasks(
    status: str | None = Query(None),
    project: str | None = Query(None),
    include_archived: bool = Query(False),
    pool=Depends(get_pool),
):
    return await db.list_tasks(
        pool, status=status, project=project, include_archived=include_archived
    )


@app.get("/tasks/{task_id}", response_model=TaskResponse, dependencies=[Depends(verify_token)])
async def get_task(task_id: str, pool=Depends(get_pool)):
    task = await db.get_task(pool, task_id)
    if not task:
        raise HTTPException(status_code=404, detail="Task not found")
    return task


# ---------------------------------------------------------------------------
# Continuous tasks (read-only view)
# ---------------------------------------------------------------------------
#
# The daemon persists each continuous orchestrator's state under
# ~/.cm/continuous-tasks/<id>/state.json (co-located with this API on
# cm-manager). We read those files directly for a read-only view rather than
# opening the daemon control socket — no daemon-up dependency, and there is no
# write path here (create/update/pause/delete stay operator-only on the daemon
# socket). Any token-holder may read, so agents on either MCP copy can see it.
CONTINUOUS_TASKS_DIR = os.path.expanduser(
    os.environ.get("CM_CONTINUOUS_TASKS_DIR", "~/.cm/continuous-tasks")
)


def _cadence(schedule: dict) -> str:
    kind = (schedule or {}).get("kind", "on_demand")
    if kind == "periodic":
        secs = schedule.get("every_secs") or 0
        if secs and secs % 3600 == 0:
            return f"every {secs // 3600}h"
        if secs and secs % 60 == 0:
            return f"every {secs // 60}m"
        return f"every {secs}s" if secs else "periodic"
    return kind


def _read_continuous_states() -> list[dict]:
    states = []
    for sf in sorted(glob.glob(os.path.join(CONTINUOUS_TASKS_DIR, "*", "state.json"))):
        try:
            with open(sf) as f:
                states.append(json.load(f))
        except (OSError, ValueError):
            # Missing dir / half-written file — skip; the view is best-effort.
            continue
    return states


def _shape_subtask(t: dict) -> dict:
    status = t.get("status")
    return {
        "task_id": t.get("id"),
        "label": t.get("name") or t.get("slug"),
        "status": status,
        # `blocked` is the orchestrators' convention for "the operator must
        # act" — a fix-ready subtask awaiting review, or an explicit human
        # decision. Everything the orchestrator drives itself stays `running`.
        "needs_human": status == "blocked",
        "branch": t.get("wip_branch"),
    }


@app.get("/continuous", dependencies=[Depends(verify_token)])
async def list_continuous(pool=Depends(get_pool)):
    """Read-only view of the daemon's continuous orchestrators + their subtasks.

    Any token-holder may read; there is no write path here.
    """
    states = await asyncio.to_thread(_read_continuous_states)
    out = []
    for st in states:
        ptid = st.get("planning_task_id")
        subs = await db.list_subtasks(pool, ptid) if ptid else []
        shaped = [_shape_subtask(s) for s in subs]
        out.append({
            "task_id": st.get("task_id"),
            "label": st.get("label"),
            "planning_task_id": ptid,
            "cadence": _cadence(st.get("schedule") or {}),
            "schedule": st.get("schedule"),
            "paused": st.get("paused", False),
            "running_now": bool(st.get("in_flight")),
            "run_count": st.get("run_count"),
            "next_fire_at": st.get("next_fire_at"),
            "last_run": st.get("last_run"),
            "orchestrator_session": st.get("current_session_uid"),
            "worktree_path": st.get("worktree_path"),
            "subtask_count": len(shaped),
            "needs_human_count": sum(1 for s in shaped if s["needs_human"]),
            "subtasks": shaped,
        })
    return {"continuous_tasks": out}


@app.patch("/tasks/{task_id}", response_model=TaskResponse, dependencies=[Depends(verify_token)])
async def update_task(task_id: str, body: TaskUpdate, pool=Depends(get_pool)):
    task = await db.get_task(pool, task_id)
    if not task:
        raise HTTPException(status_code=404, detail="Task not found")

    # Use model_fields_set so explicit JSON nulls (clear-the-field intent)
    # are kept and omitted fields are dropped. exclude_none=True collapsed
    # both into "missing", which made nullable columns un-clearable once set.
    dumped = body.model_dump(exclude_unset=True)
    fields = {k: dumped[k] for k in body.model_fields_set if k in dumped}

    bad = sorted(
        k for k, v in fields.items()
        if v is None and k not in NULLABLE_TASK_FIELDS
    )
    if bad:
        raise HTTPException(
            status_code=400,
            detail=(
                f"cannot set non-nullable field(s) to null: {', '.join(bad)}"
            ),
        )

    if not fields:
        return task

    # Side effect: when marking done, handle the worker VM
    if fields.get("status") == "done" and task["worker_vm"]:
        # Check if this is a warm VM — if so, release it back to ready instead of deleting
        warm_vms = await db.list_warm_vms(pool)
        warm_vm = next((v for v in warm_vms if v["vm_name"] == task["worker_vm"]), None)
        if warm_vm:
            await db.update_warm_vm(pool, warm_vm["id"],
                                    status="ready", current_task_id=None)
            logger.info(f"Released warm VM {task['worker_vm']} back to ready")
        else:
            _spawn_vm_deletion(task["worker_vm"])

    # Auto-set blocked_at when transitioning to blocked
    if fields.get("status") == "blocked" and "blocked_at" not in fields:
        fields["blocked_at"] = datetime.now(timezone.utc)

    updated = await db.update_task(pool, task_id, **fields)
    if not updated:
        raise HTTPException(status_code=404, detail="Task not found")
    return updated


@app.delete("/tasks/{task_id}", dependencies=[Depends(verify_token)])
async def delete_task(task_id: str, pool=Depends(get_pool)):
    task = await db.get_task(pool, task_id)
    if not task:
        raise HTTPException(status_code=404, detail="Task not found")

    if task["worker_vm"]:
        _spawn_vm_deletion(task["worker_vm"])
        # VM tasks: mark done so dispatch daemon can clean up
        await db.update_task(pool, task_id, status="done")
    else:
        # No VM: permanently delete the row
        await db.delete_task(pool, task_id)
    return {"ok": True}


# ---------------------------------------------------------------------------
# Projects
# ---------------------------------------------------------------------------

@app.get("/projects", dependencies=[Depends(verify_token)])
async def list_projects(pool=Depends(get_pool)):
    """Return distinct project names and their repo URLs."""
    rows = await db.list_projects(pool)
    # A project may have multiple repo_urls (different tasks) — pick the first
    seen = {}
    for r in rows:
        if r["project"] not in seen:
            seen[r["project"]] = r["repo_url"]
    return [{"name": name, "repo_url": url} for name, url in seen.items()]


# ---------------------------------------------------------------------------
# Workers
# ---------------------------------------------------------------------------

@app.get("/workers", dependencies=[Depends(verify_token)])
async def list_workers(pool=Depends(get_pool)):
    tasks = await db.list_tasks(pool)
    return [
        {
            "task_id": str(t["id"]),
            "worker_vm": t["worker_vm"],
            "status": t["status"],
            "ttyd_url": t["ttyd_url"],
            "prompt": t["prompt"][:80],
        }
        for t in tasks
        if t["status"] in ("running", "blocked") and t["worker_vm"]
    ]


# ---------------------------------------------------------------------------
# Warm Pools
# ---------------------------------------------------------------------------

@app.get("/warm-pools", dependencies=[Depends(verify_token)])
async def list_warm_pools(pool=Depends(get_pool)):
    pools = await db.list_warm_pools(pool)
    for wp in pools:
        wp["vms"] = await db.list_warm_vms(pool, pool_id=wp["id"])
    return pools


@app.post("/warm-pools", dependencies=[Depends(verify_token)])
async def create_warm_pool(body: dict, pool=Depends(get_pool)):
    wp = await db.add_warm_pool(
        pool,
        repo_url=body["repo_url"],
        repo_branch=body.get("repo_branch", "main"),
        pool_size=body.get("pool_size", 1),
        vm_machine_type=body.get("vm_machine_type", "e2-medium"),
    )
    return wp


@app.delete("/warm-pools/{pool_id}", dependencies=[Depends(verify_token)])
async def delete_warm_pool(pool_id: str, pool=Depends(get_pool)):
    # Delete all warm VMs first
    vms = await db.list_warm_vms(pool, pool_id=pool_id)
    for vm in vms:
        if vm["status"] != "dead":
            _spawn_vm_deletion(vm["vm_name"])
        await db.delete_warm_vm(pool, vm["id"])
    await db.delete_warm_pool(pool, pool_id)
    return {"ok": True}


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

@app.get("/config", dependencies=[Depends(verify_token)])
async def get_config():
    from dispatch.config import MAX_WORKERS
    return {
        "max_workers": MAX_WORKERS,
    }


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------

@app.get("/health")
async def health():
    return {"status": "ok"}
