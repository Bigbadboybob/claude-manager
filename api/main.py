import asyncio
import logging
import re
from contextlib import asynccontextmanager
from datetime import datetime, timezone

from fastapi import FastAPI, Depends, HTTPException, Query

from api.auth import verify_token
from api.models import TaskCreate, TaskUpdate, TaskResponse
from api.dispatch_daemon import dispatch_loop
from dispatch import db
from dispatch.config import DB_DSN

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger("cm.api")


@asynccontextmanager
async def lifespan(app: FastAPI):
    # Startup
    app.state.pool = await db.get_pool()
    await db.init_db(app.state.pool)
    app.state.dispatch_task = asyncio.create_task(dispatch_loop(app.state.pool))
    logger.info("API server started")
    yield
    # Shutdown
    app.state.dispatch_task.cancel()
    try:
        await app.state.dispatch_task
    except asyncio.CancelledError:
        pass
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

@app.post("/tasks", response_model=TaskResponse, dependencies=[Depends(verify_token)])
async def create_task(body: TaskCreate, pool=Depends(get_pool)):
    # prompt defaults to name if not provided
    prompt = body.prompt or body.name or ""

    # Auto-generate slug from name if not provided
    slug = body.slug
    if not slug and body.name:
        slug = _slugify(body.name)

    task = await db.add_task(
        pool, body.repo_url, body.repo_branch, prompt, body.priority,
        project=body.project, slug=slug, name=body.name,
        description=body.description, difficulty=body.difficulty,
        depends=body.depends, source=body.source, is_cloud=body.is_cloud,
    )
    return task


@app.get("/tasks", response_model=list[TaskResponse], dependencies=[Depends(verify_token)])
async def list_tasks(
    status: str | None = Query(None),
    project: str | None = Query(None),
    pool=Depends(get_pool),
):
    return await db.list_tasks(pool, status=status, project=project)


@app.get("/tasks/{task_id}", response_model=TaskResponse, dependencies=[Depends(verify_token)])
async def get_task(task_id: str, pool=Depends(get_pool)):
    task = await db.get_task(pool, task_id)
    if not task:
        raise HTTPException(status_code=404, detail="Task not found")
    return task


@app.patch("/tasks/{task_id}", response_model=TaskResponse, dependencies=[Depends(verify_token)])
async def update_task(task_id: str, body: TaskUpdate, pool=Depends(get_pool)):
    task = await db.get_task(pool, task_id)
    if not task:
        raise HTTPException(status_code=404, detail="Task not found")

    fields = body.model_dump(exclude_none=True)
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
            async def _delete_vm(vm_name):
                try:
                    from dispatch.vm import delete_worker
                    await asyncio.to_thread(delete_worker, vm_name)
                    logger.info(f"Deleted VM {vm_name}")
                except Exception:
                    logger.exception(f"Failed to delete VM {vm_name}")
            asyncio.create_task(_delete_vm(task["worker_vm"]))

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
        async def _delete_vm(vm_name):
            try:
                from dispatch.vm import delete_worker
                await asyncio.to_thread(delete_worker, vm_name)
                logger.info(f"Deleted VM {vm_name}")
            except Exception:
                logger.exception(f"Failed to delete VM {vm_name}")
        asyncio.create_task(_delete_vm(task["worker_vm"]))
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
            async def _delete(vm_name):
                try:
                    from dispatch.vm import delete_worker
                    await asyncio.to_thread(delete_worker, vm_name)
                except Exception:
                    pass
            asyncio.create_task(_delete(vm["vm_name"]))
        await db.delete_warm_vm(pool, vm["id"])
    await db.delete_warm_pool(pool, pool_id)
    return {"ok": True}


# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

@app.get("/config", dependencies=[Depends(verify_token)])
async def get_config():
    from dispatch.config import MAX_WORKERS, ZOMBIE_TIMEOUT_MINUTES
    return {
        "max_workers": MAX_WORKERS,
        "zombie_timeout_minutes": ZOMBIE_TIMEOUT_MINUTES,
    }


# ---------------------------------------------------------------------------
# Health
# ---------------------------------------------------------------------------

@app.get("/health")
async def health():
    return {"status": "ok"}
