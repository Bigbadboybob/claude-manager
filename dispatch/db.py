import asyncio
import json
import uuid
import asyncpg
from dispatch.config import DB_DSN


def _serialize(row: dict) -> dict:
    """Convert UUID and other non-JSON types to strings."""
    return {k: str(v) if isinstance(v, uuid.UUID) else v for k, v in row.items()}


async def _init_connection(conn: asyncpg.Connection) -> None:
    """Per-connection setup. Registers a JSONB codec so columns like
    `metadata` and `resume_metadata` round-trip as Python dicts in both
    directions — without this, asyncpg returns raw JSON strings and the
    Pydantic `dict | None` fields fail validation."""
    await conn.set_type_codec(
        "jsonb",
        encoder=json.dumps,
        decoder=json.loads,
        schema="pg_catalog",
    )


async def get_pool() -> asyncpg.Pool:
    return await asyncpg.create_pool(
        DB_DSN, min_size=1, max_size=5, init=_init_connection,
    )


async def init_db(pool: asyncpg.Pool):
    """Run all schema migrations."""
    from pathlib import Path
    sql_dir = Path(__file__).parent.parent / "sql"
    async with pool.acquire() as conn:
        for sql_file in sorted(sql_dir.glob("*.sql")):
            await conn.execute(sql_file.read_text())


async def add_task(pool: asyncpg.Pool, repo_url: str, repo_branch: str,
                   prompt: str, priority: int = 0, *,
                   status: str = "backlog",
                   project: str | None = None, slug: str | None = None,
                   name: str | None = None, description: str | None = None,
                   difficulty: int | None = None, depends: list[str] | None = None,
                   source: str = "user", is_cloud: bool = False,
                   kind: str = "oneshot",
                   parent_task_id: str | None = None,
                   worktree_mode: str = "inherit",
                   wip_branch: str | None = None,
                   metadata: dict | None = None) -> dict:
    """Insert a task. If `slug` collides with an existing row in the same
    project (idx_tasks_project_slug), auto-increment by appending `-2`,
    `-3`, ... until a free slot is found.

    Background: archived tasks keep their slug, and the unique index has
    no status filter, so re-proposing a task with the same name as an
    archived one used to 500 with a UniqueViolationError. Auto-increment
    handles both that case AND legitimate concurrent inserts (the second
    insert hits the constraint, retries, and lands on slug-2).
    """
    async with pool.acquire() as conn:
        # Slug-less rows can't trip the (project, slug) WHERE slug IS NOT
        # NULL index, so the simple path is fine for them.
        if slug is None or project is None:
            row = await conn.fetchrow(
                """INSERT INTO tasks (repo_url, repo_branch, prompt, priority,
                                      status, project, slug, name, description,
                                      difficulty, depends, source, is_cloud,
                                      kind, parent_task_id, worktree_mode,
                                      wip_branch, metadata)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                           $14, $15, $16, $17, $18)
                   RETURNING *""",
                repo_url, repo_branch, prompt, priority,
                status, project, slug, name, description,
                difficulty, depends or [], source, is_cloud,
                kind, parent_task_id, worktree_mode, wip_branch, metadata,
            )
            return _serialize(dict(row))

        # Slug is set — try the original first, then -2, -3, ... up to a
        # cap. The cap is defensive; a project legitimately needing 100
        # variants of the same slug is a sign of something else wrong.
        max_attempts = 100
        attempt_slug = slug
        last_err: Exception | None = None
        for n in range(max_attempts):
            try:
                row = await conn.fetchrow(
                    """INSERT INTO tasks (repo_url, repo_branch, prompt, priority,
                                          status, project, slug, name, description,
                                          difficulty, depends, source, is_cloud,
                                          kind, parent_task_id, worktree_mode,
                                          wip_branch, metadata)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                               $14, $15, $16, $17, $18)
                       RETURNING *""",
                    repo_url, repo_branch, prompt, priority,
                    status, project, attempt_slug, name, description,
                    difficulty, depends or [], source, is_cloud,
                    kind, parent_task_id, worktree_mode, wip_branch, metadata,
                )
                return _serialize(dict(row))
            except asyncpg.UniqueViolationError as e:
                # Only retry on the (project, slug) index. Other unique
                # violations (e.g. the future tasks_pkey if a UUID
                # collision ever happened, or any new constraint) should
                # propagate so the caller sees the real cause.
                if "idx_tasks_project_slug" not in str(e):
                    raise
                last_err = e
                attempt_slug = f"{slug}-{n + 2}"

        # Should be unreachable in practice — 100 colliding slugs in one
        # project means something is very wrong upstream.
        raise RuntimeError(
            f"add_task: slug collision after {max_attempts} attempts for "
            f"project={project!r}, base_slug={slug!r}. Last error: {last_err}"
        )


async def list_tasks(pool: asyncpg.Pool, status: str | None = None,
                     project: str | None = None,
                     include_archived: bool = False) -> list[dict]:
    async with pool.acquire() as conn:
        conditions = []
        params = []
        if status:
            params.append(status)
            conditions.append(f"status = ${len(params)}")
        if project:
            params.append(project)
            conditions.append(f"project = ${len(params)}")
        # Exclude archived rows by default: they're hidden in the TUI behind
        # A-V and bloat the response (e.g. 262 of 431 rows / ~600KB) that the
        # TUI re-fetches over a slow WAN. Callers needing them pass
        # include_archived=True or filter explicitly by status='archived'.
        if not include_archived and status != "archived":
            conditions.append("status != 'archived'")
        where = f"WHERE {' AND '.join(conditions)}" if conditions else ""
        rows = await conn.fetch(
            f"""SELECT * FROM tasks {where} ORDER BY
                   CASE status
                       WHEN 'blocked' THEN 0
                       WHEN 'running' THEN 1
                       WHEN 'backlog' THEN 2
                       WHEN 'draft' THEN 3
                       WHEN 'done' THEN 4
                       WHEN 'archived' THEN 5
                   END,
                   priority, created_at""",
            *params,
        )
        return [_serialize(dict(r)) for r in rows]


async def list_subtasks(pool: asyncpg.Pool, parent_task_id: str) -> list[dict]:
    """Direct subtasks of ``parent_task_id`` (used by the read-only
    continuous-task view). Archived rows excluded; ordered oldest-first."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT * FROM tasks
               WHERE parent_task_id = $1 AND status != 'archived'
               ORDER BY created_at""",
            parent_task_id,
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


async def merge_task_metadata_backtest(
    pool: asyncpg.Pool, task_id: str, fields: dict
) -> dict | None:
    """Merge ``fields`` into ``metadata->'backtest'`` atomically, server-side.

    Used by the live backtest-phase heartbeat: a plain PATCH replaces the whole
    ``metadata`` JSONB (``update_task`` above), which would clobber run_key /
    launched_at / etc., so a naive read-modify-write from the worker races the
    dispatcher's own metadata writes. This does the merge in one statement —
    ``metadata->'backtest'`` (or ``{}``) concatenated with ``fields`` (a shallow
    ``||`` merge, so each key in ``fields`` overwrites its prior value and every
    other backtest key is preserved). ``updated_at`` is bumped like any update;
    that is safe because phase heartbeats only arrive while the task is
    ``running`` — the RUNNING-run reaper anchors on ``metadata.backtest.launched_at``
    and the done/blocked archive sweep only touches terminal rows.
    """
    async with pool.acquire() as conn:
        # $2 is passed as a Python dict; the per-connection jsonb codec encodes it, and the `||`
        # (jsonb || jsonb) operand types it as jsonb — the same dict->jsonb pattern update_task uses
        # for `metadata = $N`. No `::jsonb` cast on the param (that idiom would type it as text and
        # bypass the codec).
        row = await conn.fetchrow(
            """
            UPDATE tasks
            SET metadata = jsonb_set(
                    COALESCE(metadata, '{}'::jsonb),
                    '{backtest}',
                    COALESCE(metadata->'backtest', '{}'::jsonb) || $2,
                    true
                ),
                updated_at = now()
            WHERE id = $1
            RETURNING *
            """,
            task_id, fields,
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
                      vm_zone: str, external_ip: str | None = None) -> dict:
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


async def find_ready_warm_vm(pool: asyncpg.Pool, repo_url: str,
                              task_id: str) -> dict | None:
    """Atomically claim a ready warm VM for a given repo.

    Selects a ready VM with FOR UPDATE SKIP LOCKED and flips it to busy in the
    same statement, so two concurrent dispatchers cannot claim the same VM.
    """
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """UPDATE warm_vms SET status = 'busy', current_task_id = $1
               WHERE id = (
                   SELECT wv.id FROM warm_vms wv
                   WHERE wv.status = 'ready'
                     AND wv.pool_id IN (
                         SELECT id FROM warm_pools WHERE repo_url = $2
                     )
                   ORDER BY wv.created_at
                   LIMIT 1
                   FOR UPDATE SKIP LOCKED
               )
               RETURNING *""",
            task_id, repo_url,
        )
        return _serialize(dict(row)) if row else None


async def count_dispatchable(pool: asyncpg.Pool) -> int:
    """Count active tasks matching `claim_next_task`'s dispatch predicates.

    Mirrors the WHERE clause of `claim_next_task` so capacity planning only
    counts work the dispatcher would actually pick up.
    """
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """SELECT count(*) AS n FROM tasks
               WHERE status IN ('running', 'blocked')
                 AND is_cloud = true
                 AND project IS NULL
                 AND kind NOT IN ('continuous', 'backtest')""",
        )
        return row["n"]


async def delete_task(pool: asyncpg.Pool, task_id: str):
    """Permanently delete a task row."""
    async with pool.acquire() as conn:
        await conn.execute("DELETE FROM tasks WHERE id = $1", task_id)


async def list_projects(pool: asyncpg.Pool) -> list[dict]:
    """Return distinct project names with their repo URLs."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT DISTINCT project, repo_url FROM tasks
               WHERE project IS NOT NULL
               ORDER BY project""",
        )
        return [dict(r) for r in rows]


async def claim_next_task(pool: asyncpg.Pool) -> dict | None:
    """Atomically claim the next cloud backlog task for execution.

    Only claims tasks with is_cloud=true and no project — planning tasks
    (project IS NOT NULL) are launched manually from the TUI, not auto-dispatched.
    """
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """UPDATE tasks SET status = 'running', updated_at = now()
               WHERE id = (
                   SELECT id FROM tasks
                   WHERE status = 'backlog' AND is_cloud = true
                         AND project IS NULL
                         AND kind NOT IN ('continuous', 'backtest')
                   ORDER BY priority, created_at
                   LIMIT 1
                   FOR UPDATE SKIP LOCKED
               )
               RETURNING *""",
        )
        return _serialize(dict(row)) if row else None


async def claim_next_backtest_task(pool: asyncpg.Pool) -> dict | None:
    """Atomically claim the next backtest task (cloud auto-backtest lane).

    Separate lane from `claim_next_task`: backtests carry a project (for
    board visibility) so the `project IS NULL` restriction doesn't apply,
    and capacity is gated by CM_MAX_BACKTEST_WORKERS instead of
    CM_MAX_WORKERS.
    """
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """UPDATE tasks SET status = 'running', updated_at = now()
               WHERE id = (
                   SELECT id FROM tasks
                   WHERE status = 'backlog' AND kind = 'backtest'
                   ORDER BY priority, created_at
                   LIMIT 1
                   FOR UPDATE SKIP LOCKED
               )
               RETURNING *""",
        )
        return _serialize(dict(row)) if row else None


async def count_dispatchable_backtests(pool: asyncpg.Pool) -> int:
    """Backtest-lane capacity count. Counts only 'running' — a blocked
    backtest is a terminal failure whose VM the reaper tears down; counting
    blocked rows would let dead runs permanently eat lane slots."""
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            "SELECT count(*) AS n FROM tasks WHERE status = 'running' AND kind = 'backtest'",
        )
        return row["n"]


async def list_active_backtests(pool: asyncpg.Pool) -> list[dict]:
    """Rows the backtest runaway reaper inspects: running backtests, plus
    blocked backtests that still hold a VM (worker PATCHed blocked on
    failure — the VM is kept for a short blocked_at-anchored debugging
    window, BACKTEST_BLOCKED_VM_TTL_SECS)."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT * FROM tasks
               WHERE kind = 'backtest'
                 AND (status = 'running'
                      OR (status = 'blocked' AND worker_vm IS NOT NULL))""",
        )
        return [_serialize(dict(r)) for r in rows]


async def list_terminal_backtests_with_artifacts(pool: asyncpg.Pool) -> list[dict]:
    """Terminal (done/blocked) backtest rows holding at least one result
    artifact — candidates for the dispatch daemon's auto-archive sweep.

    Rows with NO artifact are excluded on purpose: "terminal but resultless"
    is an operator signal (artifact POST exhausted, worker died pre-POST)
    that must stay visible on the board. Rows still holding a VM are
    returned — the sweep itself skips them until the reaper's teardown
    clears worker_vm, so grace-period logic lives in one place."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT t.* FROM tasks t
               WHERE t.kind = 'backtest'
                 AND t.status IN ('done', 'blocked')
                 AND EXISTS (SELECT 1 FROM task_artifacts a WHERE a.task_id = t.id)""",
        )
        return [_serialize(dict(r)) for r in rows]


# ---------------------------------------------------------------------------
# Task artifacts (sql/013_task_artifacts.sql — cloud auto-backtest results)
# ---------------------------------------------------------------------------


async def add_task_artifact(pool: asyncpg.Pool, task_id: str, *,
                            summary: dict,
                            kind: str = "backtest-result",
                            gcs_prefix: str | None = None,
                            partial: bool = False) -> dict:
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """INSERT INTO task_artifacts (task_id, kind, summary, gcs_prefix, partial)
               VALUES ($1, $2, $3, $4, $5) RETURNING *""",
            task_id, kind, summary, gcs_prefix, partial,
        )
        return _serialize(dict(row))


async def list_task_artifacts(pool: asyncpg.Pool, task_id: str) -> list[dict]:
    """All artifacts for a task, newest first (readers take [0] as latest)."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT * FROM task_artifacts WHERE task_id = $1
               ORDER BY created_at DESC""",
            task_id,
        )
        return [_serialize(dict(r)) for r in rows]


# ---------------------------------------------------------------------------
# Named queues (Continuous Tasks Phase 4 — sql/012_queue_items.sql)
# ---------------------------------------------------------------------------
#
# Generic transport for queue-fed Consumer continuous tasks
# (DESIGN_SCRAPER_MIGRATION.md §3). Free-form JSONB payloads; dedup is a
# burst-coalescing partial unique index over not-yet-consumed items — the
# INSERT catches UniqueViolation rather than ON CONFLICT (an ON CONFLICT
# arbiter can't cleanly target the state-filtered partial index).


async def enqueue_queue_item(
    pool: asyncpg.Pool,
    queue: str,
    payload: dict | list,
    dedupe_key: str | None = None,
    source: str | None = None,
) -> dict:
    """Insert one item. Returns {enqueued, deduped, id, depth} where `depth`
    is the queue's pending count after the call (deduped or not)."""
    async with pool.acquire() as conn:
        item_id: str | None = None
        deduped = False
        try:
            row = await conn.fetchrow(
                """INSERT INTO queue_items (queue, payload, dedupe_key, source)
                   VALUES ($1, $2, $3, $4)
                   RETURNING id""",
                queue, payload, dedupe_key, source,
            )
            item_id = str(row["id"])
        except asyncpg.UniqueViolationError:
            # Same dedupe_key already pending/claimed in this queue — coalesce.
            deduped = True
        depth = await conn.fetchval(
            "SELECT count(*) FROM queue_items WHERE queue = $1 AND state = 'pending'",
            queue,
        )
        return {
            "enqueued": not deduped,
            "deduped": deduped,
            "id": item_id,
            "depth": int(depth),
        }


async def queue_stats(pool: asyncpg.Pool, queue: str) -> dict:
    """Pending/claimed counts + oldest pending timestamp for one queue."""
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """SELECT
                   count(*) FILTER (WHERE state = 'pending')  AS pending,
                   count(*) FILTER (WHERE state = 'claimed')  AS claimed,
                   min(enqueued_at) FILTER (WHERE state = 'pending') AS oldest_pending_at
               FROM queue_items WHERE queue = $1""",
            queue,
        )
        oldest = row["oldest_pending_at"]
        return {
            "queue": queue,
            "pending": int(row["pending"]),
            "claimed": int(row["claimed"]),
            "oldest_pending_at": oldest.isoformat() if oldest else None,
        }


async def claim_queue_items(
    pool: asyncpg.Pool, queue: str, max_items: int, claimed_by: str,
) -> list[dict]:
    """Atomically claim up to `max_items` oldest pending items
    (FOR UPDATE SKIP LOCKED — safe under concurrent claimers)."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """UPDATE queue_items
               SET state = 'claimed', claimed_at = now(), claimed_by = $3
               WHERE id IN (
                   SELECT id FROM queue_items
                   WHERE queue = $1 AND state = 'pending'
                   ORDER BY enqueued_at
                   LIMIT $2
                   FOR UPDATE SKIP LOCKED
               )
               RETURNING id, payload, dedupe_key, source, enqueued_at""",
            queue, max_items, claimed_by,
        )
        # Preserve claim (oldest-first) order for the batch file.
        rows = sorted(rows, key=lambda r: r["enqueued_at"])
        return [
            {
                "id": str(r["id"]),
                "payload": r["payload"],
                "dedupe_key": r["dedupe_key"],
                "source": r["source"],
                "enqueued_at": r["enqueued_at"].isoformat(),
            }
            for r in rows
        ]


async def ack_queue_items(pool: asyncpg.Pool, queue: str, ids: list[str]) -> int:
    """claimed -> consumed for the given ids (scoped to `queue`). Returns the
    number of rows flipped; ids not in claimed state are ignored."""
    if not ids:
        return 0
    async with pool.acquire() as conn:
        result = await conn.execute(
            """UPDATE queue_items
               SET state = 'consumed', consumed_at = now()
               WHERE queue = $1 AND state = 'claimed' AND id = ANY($2::uuid[])""",
            queue, ids,
        )
        return int(result.split()[-1])


async def requeue_queue_items(
    pool: asyncpg.Pool, queue: str, ids: list[str] | None = None,
) -> int:
    """claimed -> pending (recovery after a crashed/failed fire). With no ids,
    requeues ALL claimed items in the queue. Returns rows flipped."""
    async with pool.acquire() as conn:
        if ids:
            result = await conn.execute(
                """UPDATE queue_items
                   SET state = 'pending', claimed_at = NULL, claimed_by = NULL
                   WHERE queue = $1 AND state = 'claimed' AND id = ANY($2::uuid[])""",
                queue, ids,
            )
        else:
            result = await conn.execute(
                """UPDATE queue_items
                   SET state = 'pending', claimed_at = NULL, claimed_by = NULL
                   WHERE queue = $1 AND state = 'claimed'""",
                queue,
            )
        return int(result.split()[-1])
