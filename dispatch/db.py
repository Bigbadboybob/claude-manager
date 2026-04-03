import asyncio
import uuid
import asyncpg
from dispatch.config import DB_DSN


def _serialize(row: dict) -> dict:
    """Convert UUID and other non-JSON types to strings."""
    return {k: str(v) if isinstance(v, uuid.UUID) else v for k, v in row.items()}


async def get_pool() -> asyncpg.Pool:
    return await asyncpg.create_pool(DB_DSN, min_size=1, max_size=5)


async def init_db(pool: asyncpg.Pool):
    """Run all schema migrations."""
    from pathlib import Path
    sql_dir = Path(__file__).parent.parent / "sql"
    async with pool.acquire() as conn:
        for sql_file in sorted(sql_dir.glob("*.sql")):
            await conn.execute(sql_file.read_text())


async def add_task(pool: asyncpg.Pool, repo_url: str, repo_branch: str,
                   prompt: str, priority: int = 0) -> dict:
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """INSERT INTO tasks (repo_url, repo_branch, prompt, priority)
               VALUES ($1, $2, $3, $4)
               RETURNING id, status, priority, prompt, repo_url, created_at""",
            repo_url, repo_branch, prompt, priority,
        )
        return _serialize(dict(row))


async def list_tasks(pool: asyncpg.Pool, status: str | None = None) -> list[dict]:
    async with pool.acquire() as conn:
        if status:
            rows = await conn.fetch(
                """SELECT * FROM tasks WHERE status = $1
                   ORDER BY priority, created_at""",
                status,
            )
        else:
            rows = await conn.fetch(
                """SELECT * FROM tasks ORDER BY
                       CASE status
                           WHEN 'blocked' THEN 0
                           WHEN 'running' THEN 1
                           WHEN 'backlog' THEN 2
                           WHEN 'done' THEN 3
                       END,
                       priority, created_at""",
            )
        return [_serialize(dict(r)) for r in rows]


async def get_task(pool: asyncpg.Pool, task_id: str) -> dict | None:
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT * FROM tasks WHERE id = $1", task_id,
        )
        return _serialize(dict(row)) if row else None


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
        return _serialize(dict(row)) if row else None


# ---------------------------------------------------------------------------
# Warm pools
# ---------------------------------------------------------------------------

async def list_warm_pools(pool: asyncpg.Pool) -> list[dict]:
    async with pool.acquire() as conn:
        rows = await conn.fetch("SELECT * FROM warm_pools ORDER BY created_at")
        return [_serialize(dict(r)) for r in rows]


async def add_warm_pool(pool: asyncpg.Pool, repo_url: str, repo_branch: str = "main",
                        pool_size: int = 1, vm_machine_type: str = "e2-medium") -> dict:
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """INSERT INTO warm_pools (repo_url, repo_branch, pool_size, vm_machine_type)
               VALUES ($1, $2, $3, $4) RETURNING *""",
            repo_url, repo_branch, pool_size, vm_machine_type,
        )
        return _serialize(dict(row))


async def delete_warm_pool(pool: asyncpg.Pool, pool_id: str):
    async with pool.acquire() as conn:
        await conn.execute("DELETE FROM warm_pools WHERE id = $1", pool_id)


async def list_warm_vms(pool: asyncpg.Pool, pool_id: str | None = None) -> list[dict]:
    async with pool.acquire() as conn:
        if pool_id:
            rows = await conn.fetch(
                "SELECT * FROM warm_vms WHERE pool_id = $1 ORDER BY created_at", pool_id)
        else:
            rows = await conn.fetch("SELECT * FROM warm_vms ORDER BY created_at")
        return [_serialize(dict(r)) for r in rows]


async def add_warm_vm(pool: asyncpg.Pool, pool_id: str, vm_name: str,
                      vm_zone: str, external_ip: str) -> dict:
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """INSERT INTO warm_vms (pool_id, vm_name, vm_zone, external_ip)
               VALUES ($1, $2, $3, $4) RETURNING *""",
            pool_id, vm_name, vm_zone, external_ip,
        )
        return _serialize(dict(row))


async def update_warm_vm(pool: asyncpg.Pool, vm_id: str, **fields) -> dict | None:
    if not fields:
        return None
    sets = ", ".join(f"{k} = ${i+2}" for i, k in enumerate(fields))
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            f"UPDATE warm_vms SET {sets} WHERE id = $1 RETURNING *",
            vm_id, *fields.values(),
        )
        return _serialize(dict(row)) if row else None


async def delete_warm_vm(pool: asyncpg.Pool, vm_id: str):
    async with pool.acquire() as conn:
        await conn.execute("DELETE FROM warm_vms WHERE id = $1", vm_id)


async def find_ready_warm_vm(pool: asyncpg.Pool, repo_url: str) -> dict | None:
    """Find a ready warm VM for a given repo."""
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """SELECT wv.* FROM warm_vms wv
               JOIN warm_pools wp ON wv.pool_id = wp.id
               WHERE wp.repo_url = $1 AND wv.status = 'ready'
               LIMIT 1
               FOR UPDATE SKIP LOCKED""",
            repo_url,
        )
        return _serialize(dict(row)) if row else None


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
        return _serialize(dict(row)) if row else None
