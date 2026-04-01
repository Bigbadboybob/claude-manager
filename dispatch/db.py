import asyncio
import asyncpg
from dispatch.config import DB_DSN


async def get_pool() -> asyncpg.Pool:
    return await asyncpg.create_pool(DB_DSN, min_size=1, max_size=5)


async def init_db(pool: asyncpg.Pool):
    """Run schema migration."""
    from pathlib import Path
    sql = (Path(__file__).parent.parent / "sql" / "001_init.sql").read_text()
    async with pool.acquire() as conn:
        await conn.execute(sql)


async def add_task(pool: asyncpg.Pool, repo_url: str, repo_branch: str,
                   prompt: str, priority: int = 0) -> dict:
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """INSERT INTO tasks (repo_url, repo_branch, prompt, priority)
               VALUES ($1, $2, $3, $4)
               RETURNING id, status, priority, prompt, repo_url, created_at""",
            repo_url, repo_branch, prompt, priority,
        )
        return dict(row)


async def list_tasks(pool: asyncpg.Pool, status: str | None = None) -> list[dict]:
    async with pool.acquire() as conn:
        if status:
            rows = await conn.fetch(
                """SELECT id, status, priority, prompt, repo_url, repo_branch,
                          worker_vm, ttyd_url, blocked_at, created_at
                   FROM tasks WHERE status = $1
                   ORDER BY priority, created_at""",
                status,
            )
        else:
            rows = await conn.fetch(
                """SELECT id, status, priority, prompt, repo_url, repo_branch,
                          worker_vm, ttyd_url, blocked_at, created_at
                   FROM tasks ORDER BY
                       CASE status
                           WHEN 'blocked' THEN 0
                           WHEN 'running' THEN 1
                           WHEN 'backlog' THEN 2
                           WHEN 'done' THEN 3
                       END,
                       priority, created_at""",
            )
        return [dict(r) for r in rows]


async def get_task(pool: asyncpg.Pool, task_id: str) -> dict | None:
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT * FROM tasks WHERE id = $1", task_id,
        )
        return dict(row) if row else None


async def update_task(pool: asyncpg.Pool, task_id: str, **fields) -> dict | None:
    if not fields:
        return await get_task(pool, task_id)
    sets = ", ".join(f"{k} = ${i+2}" for i, k in enumerate(fields))
    sets += ", updated_at = now()"
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            f"UPDATE tasks SET {sets} WHERE id = $1 RETURNING *",
            task_id, *fields.values(),
        )
        return dict(row) if row else None


async def claim_next_task(pool: asyncpg.Pool) -> dict | None:
    """Atomically claim the next backlog task for execution."""
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """UPDATE tasks SET status = 'running', updated_at = now()
               WHERE id = (
                   SELECT id FROM tasks
                   WHERE status = 'backlog'
                   ORDER BY priority, created_at
                   LIMIT 1
                   FOR UPDATE SKIP LOCKED
               )
               RETURNING *""",
        )
        return dict(row) if row else None
