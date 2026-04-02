import asyncio
import logging
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


# ---------------------------------------------------------------------------
# Tasks
# ---------------------------------------------------------------------------

@app.post("/tasks", response_model=TaskResponse, dependencies=[Depends(verify_token)])
async def create_task(body: TaskCreate, pool=Depends(get_pool)):
    task = await db.add_task(pool, body.repo_url, body.repo_branch, body.prompt, body.priority)
    return await db.get_task(pool, str(task["id"]))


@app.get("/tasks", response_model=list[TaskResponse], dependencies=[Depends(verify_token)])
async def list_tasks(status: str | None = Query(None), pool=Depends(get_pool)):
    return await db.list_tasks(pool, status=status)


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

    # Side effect: when marking done, delete the worker VM
    if fields.get("status") == "done" and task["worker_vm"]:
        try:
            from dispatch.vm import delete_worker
            await asyncio.to_thread(delete_worker, task["worker_vm"])
            logger.info(f"Deleted VM {task['worker_vm']} for task {task_id}")
        except Exception:
            logger.exception(f"Failed to delete VM {task['worker_vm']}")

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
        try:
            from dispatch.vm import delete_worker
            await asyncio.to_thread(delete_worker, task["worker_vm"])
        except Exception:
            logger.exception(f"Failed to delete VM {task['worker_vm']}")

    await db.update_task(pool, task_id, status="done")
    return {"ok": True}


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
# Health
# ---------------------------------------------------------------------------

@app.get("/health")
async def health():
    return {"status": "ok"}
