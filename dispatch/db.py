import asyncio
import json
import uuid
import asyncpg
from dispatch.config import DB_DSN


class InvalidTaskId(ValueError):
    """A task lookup was attempted with anything but a full UUID.

    Task IDs are UUID primary keys.  PostgreSQL/asyncpg otherwise rejects a
    short prefix while binding ``WHERE id = $1`` and that driver exception used
    to escape as an opaque HTTP 500.  Keep the validation next to the queries so
    every API lookup has the same contract, including future endpoints.
    """


def normalize_task_id(task_id: str) -> str:
    """Return a canonical task UUID or raise ``InvalidTaskId``.

    Short-id prefixes are display-only.  Resolving them at the HTTP layer would
    disagree with daemon task-tree authorization, which is keyed by exact UUID,
    and would add an ambiguity case once two tasks share a prefix.
    """
    try:
        parsed = uuid.UUID(task_id)
    except (AttributeError, TypeError, ValueError) as exc:
        raise InvalidTaskId(
            "task_id must be a full UUID (for example "
            "123e4567-e89b-12d3-a456-426614174000); short prefixes are not accepted"
        ) from exc
    canonical = str(parsed)
    if task_id.lower() != canonical:
        raise InvalidTaskId(
            "task_id must be a full hyphenated UUID (36 characters); "
            "short prefixes are not accepted"
        )
    return canonical


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
                   initiative_id: str | None = None,
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
                                      initiative_id, wip_branch, metadata)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                           $14, $15, $16, $17, $18, $19)
                   RETURNING *""",
                repo_url, repo_branch, prompt, priority,
                status, project, slug, name, description,
                difficulty, depends or [], source, is_cloud,
                kind, parent_task_id, worktree_mode, initiative_id, wip_branch, metadata,
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
                                          initiative_id, wip_branch, metadata)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
                               $14, $15, $16, $17, $18, $19)
                       RETURNING *""",
                    repo_url, repo_branch, prompt, priority,
                    status, project, attempt_slug, name, description,
                    difficulty, depends or [], source, is_cloud,
                    kind, parent_task_id, worktree_mode, initiative_id, wip_branch, metadata,
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


# Shared SELECT for task rows: the task columns plus the embedded initiative
# display object. Every reader that hands rows to clients (list, snapshot,
# change delivery) must use the same shape.
TASK_ROW_SELECT = """SELECT t.*, i.slug AS initiative_slug, i.name AS initiative_name,
                          i.status AS initiative_status, i.color AS initiative_color,
                          CASE WHEN i.id IS NULL THEN NULL ELSE
                            jsonb_build_object('id', i.id, 'slug', i.slug,
                                               'name', i.name, 'status', i.status,
                                               'color', i.color,
                                               'coordinator_task_id', i.coordinator_task_id) END AS initiative
                   FROM tasks t
                   LEFT JOIN initiatives i ON i.id = t.initiative_id"""

TASK_ROW_ORDER = """ORDER BY
                   CASE t.status
                       WHEN 'blocked' THEN 0
                       WHEN 'running' THEN 1
                       WHEN 'backlog' THEN 2
                       WHEN 'draft' THEN 3
                       WHEN 'done' THEN 4
                       WHEN 'archived' THEN 5
                   END,
                   t.priority, t.created_at"""


def _task_filter(status: str | None, project: str | None,
                 initiative_id: str | None, include_archived: bool) -> tuple[str, list]:
    """WHERE clause (already prefixed with `t.`) + params for the task readers."""
    conditions = []
    params: list = []
    if status:
        params.append(status)
        conditions.append(f"t.status = ${len(params)}")
    if project:
        params.append(project)
        conditions.append(f"t.project = ${len(params)}")
    if initiative_id:
        params.append(initiative_id)
        conditions.append(f"t.initiative_id = ${len(params)}")
    # Exclude archived rows by default: they're hidden in the TUI behind
    # A-V and bloat the response (e.g. 262 of 431 rows / ~600KB) that the
    # TUI re-fetches over a slow WAN. Callers needing them pass
    # include_archived=True or filter explicitly by status='archived'.
    if not include_archived and status != "archived":
        conditions.append("t.status != 'archived'")
    where = f"WHERE {' AND '.join(conditions)}" if conditions else ""
    return where, params


async def _fetch_tasks_conn(conn, status=None, project=None, initiative_id=None,
                            include_archived=False) -> list[dict]:
    where, params = _task_filter(status, project, initiative_id, include_archived)
    rows = await conn.fetch(f"{TASK_ROW_SELECT} {where} {TASK_ROW_ORDER}", *params)
    return [_serialize(dict(r)) for r in rows]


async def list_tasks(pool: asyncpg.Pool, status: str | None = None,
                     project: str | None = None,
                     initiative_id: str | None = None,
                     include_archived: bool = False) -> list[dict]:
    async with pool.acquire() as conn:
        return await _fetch_tasks_conn(conn, status, project, initiative_id,
                                       include_archived)


# ---------------------------------------------------------------------------
# Task change log (sql/016_task_changes.sql) — incremental task updates
# ---------------------------------------------------------------------------

def _matches_task_filter(task: dict | None, project: str | None,
                         include_archived: bool) -> bool:
    if task is None:
        return False
    if project is not None and task.get("project") != project:
        return False
    if not include_archived and task.get("status") == "archived":
        return False
    return True


async def _change_meta_conn(conn) -> dict:
    row = await conn.fetchrow(
        """SELECT m.epoch, m.pruned_through,
                  COALESCE(pg_sequence_last_value('task_changes_seq_seq'), 0) AS last_seq
           FROM task_change_meta m"""
    )
    return dict(row)


async def task_snapshot(pool: asyncpg.Pool, *, project: str | None = None,
                        include_archived: bool = False) -> dict:
    """Consistent full snapshot: ``cursor`` is read BEFORE the rows, so every
    change with seq <= cursor is reflected in ``tasks`` (the rows may be newer,
    which is harmless: later change rows re-deliver them idempotently)."""
    async with pool.acquire() as conn:
        async with conn.transaction():
            meta = await _change_meta_conn(conn)
            cursor = await conn.fetchval("SELECT COALESCE(max(seq), 0) FROM task_changes")
            # Nothing logged yet (fresh log) — anchor on the sequence so a
            # client cursor of 0 vs a real seq are distinguishable.
            cursor = max(cursor, meta["pruned_through"])
            tasks = await _fetch_tasks_conn(conn, project=project,
                                            include_archived=include_archived)
    return {"epoch": meta["epoch"], "cursor": cursor, "tasks": tasks}


async def list_task_changes(pool: asyncpg.Pool, since: int, limit: int, *,
                            project: str | None = None,
                            include_archived: bool = False) -> dict:
    """Changes strictly after ``since`` (at most ``limit`` log rows), collapsed to
    one entry per task carrying its CURRENT row.

    Returns ``{"epoch", "cursor", "changes", "more", "expired"}``. ``expired`` is
    true when ``since`` predates retention (rows were pruned) or lies beyond the
    sequence (a cursor from another lineage / a restored DB) — the caller must
    resnapshot. Each change is ``{"seq", "task_id", "op", "task"}`` with
    ``op == "upsert"`` (apply ``task``) or ``op == "remove"`` (the task was
    deleted, or no longer matches the subscription filter, e.g. archived).
    Ordering/idempotency: entries are in seq order; the current row is read
    after the log page, so a delivered row is at least as new as ``cursor``.
    """
    async with pool.acquire() as conn:
        async with conn.transaction():
            meta = await _change_meta_conn(conn)
            if since < meta["pruned_through"] or since > meta["last_seq"]:
                return {"epoch": meta["epoch"], "cursor": since, "changes": [],
                        "more": False, "expired": True}
            log = await conn.fetch(
                "SELECT seq, task_id, op FROM task_changes WHERE seq > $1 "
                "ORDER BY seq LIMIT $2",
                since, limit + 1,
            )
            more = len(log) > limit
            log = log[:limit]
            if not log:
                return {"epoch": meta["epoch"], "cursor": since, "changes": [],
                        "more": False, "expired": False}
            cursor = log[-1]["seq"]
            # Collapse to the last log row per task, keeping seq order.
            last_seq: dict[str, int] = {}
            for r in log:
                last_seq[str(r["task_id"])] = r["seq"]
            ids = list(last_seq)
            rows = await conn.fetch(
                f"{TASK_ROW_SELECT} WHERE t.id = ANY($1::uuid[])", ids,
            )
    current = {str(r["id"]): _serialize(dict(r)) for r in rows}
    changes = []
    for task_id, seq in sorted(last_seq.items(), key=lambda kv: kv[1]):
        task = current.get(task_id)
        if _matches_task_filter(task, project, include_archived):
            changes.append({"seq": seq, "task_id": task_id, "op": "upsert", "task": task})
        else:
            changes.append({"seq": seq, "task_id": task_id, "op": "remove", "task": None})
    return {"epoch": meta["epoch"], "cursor": cursor, "changes": changes,
            "more": more, "expired": False}


async def prune_task_changes(pool: asyncpg.Pool, keep_seconds: float) -> int:
    """Drop log rows older than ``keep_seconds`` and record the highest dropped
    seq so cursors below it are reported expired. Returns rows deleted."""
    async with pool.acquire() as conn:
        async with conn.transaction():
            row = await conn.fetchrow(
                """WITH d AS (
                       DELETE FROM task_changes
                       WHERE changed_at < now() - ($1::float8 * interval '1 second')
                       RETURNING seq)
                   SELECT count(*) AS n, max(seq) AS max_seq FROM d""",
                keep_seconds,
            )
            if not row["n"]:
                return 0
            await conn.execute(
                "UPDATE task_change_meta SET pruned_through = GREATEST(pruned_through, $1)",
                row["max_seq"],
            )
            return int(row["n"])


# ---------------------------------------------------------------------------
# Initiatives
# ---------------------------------------------------------------------------

INITIATIVE_STATUSES = frozenset(
    {"draft", "active", "paused", "completed", "archived", "cancelled"}
)
INITIATIVE_PROJECT_STATUSES = frozenset({"proposed", "approved", "removed"})


def _initiative_ref(ref: str) -> tuple[str, str]:
    """Return (column, value) for an initiative UUID or slug reference."""
    try:
        parsed = uuid.UUID(ref)
    except (AttributeError, TypeError, ValueError):
        if not ref or len(ref) > 80:
            raise ValueError("initiative_id must be a UUID or non-empty slug")
        return "slug", ref
    return "id", str(parsed)


async def _initiative_projects_conn(conn, initiative_id: str) -> list[dict]:
    rows = await conn.fetch(
        """SELECT initiative_id, project, status, project_channel, role,
                          proposed_by, approved_at, approved_by, created_at
                   FROM initiative_projects
                  WHERE initiative_id = $1 ORDER BY project""",
        initiative_id,
    )
    return [_serialize(dict(row)) for row in rows]


async def _initiative_counts_conn(conn, initiative_id: str) -> dict[str, int]:
    rows = await conn.fetch(
        "SELECT status, count(*) AS n FROM tasks WHERE initiative_id = $1 GROUP BY status",
        initiative_id,
    )
    return {str(row["status"]): int(row["n"]) for row in rows}


async def get_initiative(pool: asyncpg.Pool, ref: str) -> dict | None:
    column, value = _initiative_ref(ref)
    async with pool.acquire() as conn:
        row = await conn.fetchrow(f"SELECT * FROM initiatives WHERE {column} = $1", value)
        if not row:
            return None
        result = _serialize(dict(row))
        result["projects"] = await _initiative_projects_conn(conn, result["id"])
        result["task_counts"] = await _initiative_counts_conn(conn, result["id"])
        return result


async def list_initiatives(
    pool: asyncpg.Pool, *, status: str | None = None,
    project: str | None = None, include_archived: bool = False,
) -> list[dict]:
    async with pool.acquire() as conn:
        conditions: list[str] = []
        params: list[object] = []
        if status:
            params.append(status)
            conditions.append(f"i.status = ${len(params)}")
        if project:
            params.append(project)
            conditions.append(
                f"EXISTS (SELECT 1 FROM initiative_projects ipf "
                f"WHERE ipf.initiative_id = i.id AND ipf.project = ${len(params)} "
                "AND ipf.status != 'removed')"
            )
        if not include_archived and status != "archived":
            conditions.append("i.status != 'archived'")
        where = f"WHERE {' AND '.join(conditions)}" if conditions else ""
        rows = await conn.fetch(
            f"SELECT i.* FROM initiatives i {where} ORDER BY i.updated_at DESC, i.slug",
            *params,
        )
        result: list[dict] = []
        for row in rows:
            item = _serialize(dict(row))
            item["projects"] = await _initiative_projects_conn(conn, item["id"])
            item["task_counts"] = await _initiative_counts_conn(conn, item["id"])
            result.append(item)
        return result


async def create_initiative(
    pool: asyncpg.Pool, *, slug: str, name: str, description: str = "",
    color: str | None = None, coordinator_task_id: str,
    coordinator_project: str | None = None, docs_path: str = "cm-initiative",
    shared_channel: str | None = None, metadata: dict | None = None,
    actor: str = "owner",
) -> dict:
    coordinator_task_id = normalize_task_id(coordinator_task_id)
    async with pool.acquire() as conn:
        async with conn.transaction():
            task = await conn.fetchrow(
                "SELECT id, project FROM tasks WHERE id = $1", coordinator_task_id,
            )
            if not task:
                raise ValueError("coordinator task does not exist")
            row = await conn.fetchrow(
                """INSERT INTO initiatives
                    (slug, name, description, color, coordinator_task_id,
                     coordinator_project, docs_path, shared_channel, metadata)
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING *""",
                slug, name, description, color, coordinator_task_id,
                coordinator_project or task["project"], docs_path,
                shared_channel, metadata,
            )
            # The coordinator is a normal task, but it is also the durable
            # anchor for this initiative. Attach it in the same transaction so
            # a successfully-created initiative can never lose its coordinator
            # relationship. Reusing a task already assigned elsewhere would
            # make the two initiatives ambiguous, so reject it explicitly.
            attached = await conn.fetchrow(
                """UPDATE tasks SET initiative_id = $2, updated_at = now()
                   WHERE id = $1 AND initiative_id IS NULL
                   RETURNING id""",
                coordinator_task_id, row["id"],
            )
            if not attached:
                raise ValueError("coordinator task already belongs to an initiative")
            await conn.execute(
                """INSERT INTO initiative_events
                   (initiative_id, actor, event_type, new_value, reason)
                   VALUES ($1,$2,'created',$3,$4)""",
                row["id"], actor, {"status": "draft", "slug": slug},
                "initiative created",
            )
        result = _serialize(dict(row))
        result["projects"] = []
        result["task_counts"] = {}
        return result


async def update_initiative(
    pool: asyncpg.Pool, ref: str, *, actor: str = "owner",
    reason: str | None = None, **fields,
) -> dict | None:
    allowed = {"name", "description", "color", "status", "coordinator_task_id",
               "coordinator_project", "docs_path", "shared_channel", "metadata"}
    fields = {key: value for key, value in fields.items() if key in allowed}
    if not fields:
        return await get_initiative(pool, ref)
    if "status" in fields and fields["status"] not in INITIATIVE_STATUSES:
        raise ValueError(f"invalid initiative status: {fields['status']}")
    if "coordinator_task_id" in fields and fields["coordinator_task_id"]:
        fields["coordinator_task_id"] = normalize_task_id(fields["coordinator_task_id"])
    column, value = _initiative_ref(ref)
    async with pool.acquire() as conn:
        async with conn.transaction():
            current = await conn.fetchrow(
                f"SELECT * FROM initiatives WHERE {column} = $1 FOR UPDATE", value,
            )
            if not current:
                return None
            if "coordinator_task_id" in fields:
                coordinator = await conn.fetchrow(
                    "SELECT id, initiative_id FROM tasks WHERE id = $1",
                    fields["coordinator_task_id"],
                )
                if not coordinator:
                    raise ValueError("coordinator task does not exist")
                if coordinator["initiative_id"] not in (None, current["id"]):
                    raise ValueError("coordinator task already belongs to another initiative")
                await conn.execute(
                    "UPDATE tasks SET initiative_id = $2, updated_at = now() WHERE id = $1",
                    fields["coordinator_task_id"], current["id"],
                )
            if "status" in fields and fields["status"] != current["status"]:
                allowed_transitions = {
                    "draft": {"active", "cancelled"},
                    "active": {"paused", "completed", "cancelled"},
                    "paused": {"active", "completed", "cancelled"},
                    "completed": {"archived"},
                    "archived": set(),
                    "cancelled": set(),
                }
                if fields["status"] not in allowed_transitions[current["status"]]:
                    raise ValueError(
                        f"cannot transition initiative from {current['status']} "
                        f"to {fields['status']}"
                    )
                if fields["status"] == "active":
                    approved = await conn.fetchval(
                        """SELECT count(*) FROM initiative_projects
                           WHERE initiative_id = $1 AND status = 'approved'""",
                        current["id"],
                    )
                    if not approved:
                        raise ValueError(
                            "an initiative needs at least one approved project before activation"
                        )
            sets = ", ".join(f"{key} = ${index + 2}" for index, key in enumerate(fields))
            row = await conn.fetchrow(
                f"UPDATE initiatives SET {sets}, updated_at = now() "
                "WHERE id = $1 RETURNING *", current["id"], *fields.values(),
            )
            if fields.get("status") == "active":
                row = await conn.fetchrow(
                    """UPDATE initiatives SET approved_at = now(), approved_by = $2
                       WHERE id = $1 RETURNING *""", current["id"], actor,
                )
            await conn.execute(
                """INSERT INTO initiative_events
                   (initiative_id, actor, event_type, previous_value, new_value, reason)
                   VALUES ($1,$2,$3,$4,$5,$6)""",
                current["id"], actor,
                "status_changed" if "status" in fields else "updated",
                {key: current[key] for key in fields}, fields, reason,
            )
        result = _serialize(dict(row))
        result["projects"] = await _initiative_projects_conn(conn, result["id"])
        result["task_counts"] = await _initiative_counts_conn(conn, result["id"])
        return result


async def add_initiative_project(
    pool: asyncpg.Pool, ref: str, project: str, *, role: str = "",
    project_channel: str | None = None, actor: str = "owner",
) -> dict:
    initiative = await get_initiative(pool, ref)
    if not initiative:
        raise ValueError("initiative does not exist")
    async with pool.acquire() as conn:
        async with conn.transaction():
            row = await conn.fetchrow(
                """INSERT INTO initiative_projects
                   (initiative_id, project, status, role, project_channel, proposed_by)
                   VALUES ($1,$2,'proposed',$3,$4,$5)
                   ON CONFLICT (initiative_id, project) DO UPDATE SET
                     status = CASE WHEN initiative_projects.status = 'removed'
                                   THEN 'proposed' ELSE initiative_projects.status END,
                     role = EXCLUDED.role,
                     project_channel = COALESCE(EXCLUDED.project_channel,
                                                initiative_projects.project_channel)
                   RETURNING *""",
                initiative["id"], project, role, project_channel, actor,
            )
            await conn.execute(
                """INSERT INTO initiative_events
                   (initiative_id, actor, event_type, project, new_value)
                   VALUES ($1,$2,'project_proposed',$3,$4)""",
                initiative["id"], actor, project, {"role": role},
            )
        return _serialize(dict(row))


async def update_initiative_project(
    pool: asyncpg.Pool, ref: str, project: str, *, actor: str = "owner",
    reason: str | None = None, **fields,
) -> dict | None:
    fields = {key: value for key, value in fields.items()
              if key in {"status", "role", "project_channel"}}
    if "status" in fields and fields["status"] not in INITIATIVE_PROJECT_STATUSES:
        raise ValueError(f"invalid initiative project status: {fields['status']}")
    initiative = await get_initiative(pool, ref)
    if not initiative:
        return None
    async with pool.acquire() as conn:
        async with conn.transaction():
            current = await conn.fetchrow(
                """SELECT * FROM initiative_projects
                   WHERE initiative_id = $1 AND project = $2 FOR UPDATE""",
                initiative["id"], project,
            )
            if not current:
                return None
            if not fields:
                return _serialize(dict(current))
            sets = ", ".join(f"{key} = ${index + 3}" for index, key in enumerate(fields))
            row = await conn.fetchrow(
                f"UPDATE initiative_projects SET {sets} "
                "WHERE initiative_id = $1 AND project = $2 RETURNING *",
                initiative["id"], project, *fields.values(),
            )
            if fields.get("status") == "approved":
                row = await conn.fetchrow(
                    """UPDATE initiative_projects
                       SET approved_at = now(), approved_by = $3
                       WHERE initiative_id = $1 AND project = $2 RETURNING *""",
                    initiative["id"], project, actor,
                )
            await conn.execute(
                """INSERT INTO initiative_events
                   (initiative_id, actor, event_type, project,
                    previous_value, new_value, reason)
                   VALUES ($1,$2,'project_updated',$3,$4,$5,$6)""",
                initiative["id"], actor, project,
                {key: current[key] for key in fields}, fields, reason,
            )
        return _serialize(dict(row))


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
    task_id = normalize_task_id(task_id)
    async with pool.acquire() as conn:
        row = await conn.fetchrow(
            """SELECT t.*, CASE WHEN i.id IS NULL THEN NULL ELSE
                       jsonb_build_object('id', i.id, 'slug', i.slug,
                                          'name', i.name, 'status', i.status,
                                          'color', i.color,
                                          'coordinator_task_id', i.coordinator_task_id) END AS initiative
                  FROM tasks t LEFT JOIN initiatives i ON i.id = t.initiative_id
                 WHERE t.id = $1""", task_id,
        )
        return _serialize(dict(row)) if row else None


async def update_task(pool: asyncpg.Pool, task_id: str, **fields) -> dict | None:
    task_id = normalize_task_id(task_id)
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


class QueueRecoveryConflict(ValueError):
    """The item/claim no longer matches the reconciled recovery request."""


async def recover_queue_item(pool, queue: str, item_id: str,
                             claimed_by: str, recovery_key: str) -> dict:
    """Restore one reconciled consumed item, once for this recovery identity.

    The receipt is committed with the row update. A retry after a lost response
    returns the same ID even if a later consumer has already consumed it again.
    Payload, original ID, dedupe key and enqueue order are preserved.
    """
    item_uuid = uuid.UUID(item_id)
    async with pool.acquire() as conn:
        async with conn.transaction():
            # Serialize equal request keys, including requests naming different
            # IDs. Different keys for one item serialize on the item row below.
            await conn.execute("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
                               json.dumps([queue, recovery_key]))
            receipt = await conn.fetchrow(
                "SELECT item_id, claimed_by FROM queue_recovery_receipts "
                "WHERE queue=$1 AND recovery_key=$2", queue, recovery_key)
            if receipt:
                if receipt["item_id"] != item_uuid or receipt["claimed_by"] != claimed_by:
                    raise QueueRecoveryConflict("Recovery key already names another item or claim")
                return {"recovered": True, "already_recovered": True, "id": item_id}
            row = await conn.fetchrow(
                "SELECT state, claimed_by FROM queue_items WHERE queue=$1 AND id=$2 FOR UPDATE",
                queue, item_uuid)
            if not row or row["state"] != "consumed" or row["claimed_by"] != claimed_by:
                raise QueueRecoveryConflict("Item is not consumed by the expected run")
            try:
                async with conn.transaction():
                    await conn.execute(
                        "UPDATE queue_items SET state='pending', claimed_at=NULL, "
                        "claimed_by=NULL, consumed_at=NULL WHERE queue=$1 AND id=$2",
                        queue, item_uuid)
            except asyncpg.UniqueViolationError as exc:
                raise QueueRecoveryConflict(
                    "Another pending item has the same dedupe key; reconcile it first") from exc
            await conn.execute(
                "INSERT INTO queue_recovery_receipts (queue,recovery_key,item_id,claimed_by) "
                "VALUES ($1,$2,$3,$4)", queue, recovery_key, item_uuid, claimed_by)
            return {"recovered": True, "already_recovered": False, "id": item_id}


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
    """claimed -> pending (recovery after a crashed/failed fire). Returns rows
    flipped.

    `ids=None` requeues ALL claimed items in the queue — the API reaches this
    branch only on an explicit `{"all": true}`. An EMPTY list requeues nothing:
    it means "these zero items", never "everything" (the old `if ids:` test
    conflated the two, so `{"ids": []}` blanket-requeued the queue — including
    a batch an in-flight fire had just claimed)."""
    if ids is not None and not ids:
        return 0
    async with pool.acquire() as conn:
        if ids is not None:
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
