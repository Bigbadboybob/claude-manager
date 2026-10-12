"""Postgres access for work items and boards (doc/items-board.md).

Every write locks the board row (`SELECT … FOR UPDATE`), loads the board's live
items, runs the rules in `dispatch.items_rules.BoardTx`, and writes back what
the transaction recorded, all in one database transaction.
"""

from __future__ import annotations

import uuid
from datetime import datetime, timedelta, timezone

import asyncpg

from dispatch.items_rules import CLOSED, OWNER_BLOCKED, BoardTx, Item, ItemsError, iso

STATE_FRESH_S = 90
BOARD_SETTINGS = (
    "idle_s", "stale_s", "unassigned_s", "repush_s", "escalate_s", "digest_s",
    "stale_background_s", "nudge_grace_s",
)
MAX_HISTORY = 50


def utcnow() -> datetime:
    return datetime.now(timezone.utc)


def _board_ref(ref: str) -> tuple[str, str]:
    try:
        return "id", str(uuid.UUID(str(ref)))
    except (TypeError, ValueError):
        if not ref or len(ref) > 120:
            raise ItemsError(404, "not_found", "board ref must be a UUID or slug") from None
        return "slug", ref


def _str_ids(row: dict) -> dict:
    return {k: str(v) if isinstance(v, uuid.UUID) else v for k, v in row.items()}


def _board_settings(board: dict) -> dict:
    out = {k: board[k] for k in BOARD_SETTINGS}
    out["holder_idle_enabled"] = board["holder_idle_enabled"]
    out["orchestrator_pid"] = board["orchestrator_pid"]
    return out


# ---- board resolution ---------------------------------------------------

async def _board_by_ref(conn, ref: str, *, lock: bool = False) -> dict:
    column, value = _board_ref(ref)
    row = await conn.fetchrow(
        f"SELECT * FROM boards WHERE {column} = $1{' FOR UPDATE' if lock else ''}",
        value if column == "slug" else uuid.UUID(value),
    )
    if row is None:
        raise ItemsError(404, "not_found", f"no board {ref}")
    return _str_ids(dict(row))


async def _insert_board(conn, *, slug: str, name: str, initiative_id=None,
                        root_task_id=None) -> dict:
    anchor_col = "initiative_id" if initiative_id else "root_task_id"
    anchor = initiative_id or root_task_id
    for candidate in (slug, f"{slug}-{str(anchor)[:8]}"):
        await conn.execute(
            """INSERT INTO boards (slug, name, initiative_id, root_task_id)
               VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING""",
            candidate, name, initiative_id, root_task_id,
        )
        row = await conn.fetchrow(f"SELECT * FROM boards WHERE {anchor_col} = $1", anchor)
        if row is not None:
            return _str_ids(dict(row))
    raise ItemsError(409, "slug_conflict", f"could not allocate a board slug for {slug}")


async def resolve_board(pool: asyncpg.Pool, *, task_id: str | None = None,
                        ref: str | None = None) -> dict:
    """The board for a task (its initiative's, else its root task's), created on
    first use; or an existing board by ref."""
    async with pool.acquire() as conn:
        if ref:
            board = await _board_by_ref(conn, ref)
        else:
            try:
                tid = uuid.UUID(str(task_id))
            except (TypeError, ValueError):
                raise ItemsError(422, "invalid_field", "task_id must be a UUID",
                                 field="task_id") from None
            chain = await conn.fetch(
                """WITH RECURSIVE chain AS (
                       SELECT id, parent_task_id, initiative_id, name, slug, 0 AS depth
                         FROM tasks WHERE id = $1
                       UNION ALL
                       SELECT t.id, t.parent_task_id, t.initiative_id, t.name, t.slug,
                              c.depth + 1
                         FROM tasks t JOIN chain c ON t.id = c.parent_task_id
                        WHERE c.depth < 64)
                   SELECT * FROM chain ORDER BY depth""",
                tid,
            )
            if not chain:
                raise ItemsError(404, "not_found", f"no task {task_id}")
            initiative_id = next(
                (r["initiative_id"] for r in chain if r["initiative_id"]), None
            )
            async with conn.transaction():
                if initiative_id:
                    existing = await conn.fetchrow(
                        "SELECT * FROM boards WHERE initiative_id = $1", initiative_id
                    )
                    if existing:
                        board = _str_ids(dict(existing))
                    else:
                        ini = await conn.fetchrow(
                            "SELECT slug, name FROM initiatives WHERE id = $1", initiative_id
                        )
                        board = await _insert_board(
                            conn, slug=ini["slug"], name=ini["name"],
                            initiative_id=initiative_id,
                        )
                else:
                    root = chain[-1]
                    existing = await conn.fetchrow(
                        "SELECT * FROM boards WHERE root_task_id = $1", root["id"]
                    )
                    if existing:
                        board = _str_ids(dict(existing))
                    else:
                        slug = f"task-{str(root['id'])[:8]}"
                        board = await _insert_board(
                            conn, slug=slug, name=root["name"] or root["slug"] or slug,
                            root_task_id=root["id"],
                        )
        return await _board_header(conn, board, utcnow())


# ---- loading and applying a write ----------------------------------------

def _item_from_row(row) -> Item:
    return Item(
        id=row["id"], n=row["number"], title=row["title"], status=row["status"],
        note=row["note"], group=row["grp"], blocked_on=row["blocked_on"],
        check_back_at=row["check_back_at"], eta_at=row["eta_at"],
        waiting_set_at=row["waiting_set_at"], links=list(row["links"] or []),
        touched_at=row["touched_at"], clock_reset_at=row["clock_reset_at"],
        closed_at=row["closed_at"], archived_at=row["archived_at"],
        created_by=row["created_by"], created_at=row["created_at"],
        stale_nudged_at=row["stale_nudged_at"],
    )


async def _load_items(conn, board_id: str, extra_ns=(), *, archived: bool = False) -> dict[int, Item]:
    """A board's live items (plus `extra_ns` and any blocker they reference)."""
    rows = await conn.fetch(
        """WITH base AS (
               SELECT * FROM items
                WHERE board_id = $1
                  AND ($3 OR archived_at IS NULL OR number = ANY($2::int[])))
           SELECT * FROM base
           UNION
           SELECT i.* FROM items i
             JOIN item_deps d ON d.blocker_id = i.id
             JOIN base b ON b.id = d.item_id""",
        uuid.UUID(board_id), list(extra_ns), archived,
    )
    items = {r["number"]: _item_from_row(r) for r in rows}
    by_id = {it.id: it for it in items.values()}
    ids = list(by_id)
    if not ids:
        return items
    for h in await conn.fetch(
        "SELECT * FROM item_holders WHERE item_id = ANY($1::bigint[]) ORDER BY added_at, pid",
        ids,
    ):
        by_id[h["item_id"]].holders.append({
            "pid": h["pid"], "name": h["name"], "session_uid": h["session_uid"],
            "daemon_id": h["daemon_id"], "added_at": h["added_at"],
        })
    for d in await conn.fetch(
        "SELECT item_id, blocker_id FROM item_deps WHERE item_id = ANY($1::bigint[])", ids
    ):
        blocker = by_id.get(d["blocker_id"])
        if blocker is not None:
            by_id[d["item_id"]].blocked_by.add(blocker.n)
    for f in await conn.fetch(
        """SELECT item_id, kind, detail, raised_at FROM item_flags
            WHERE item_id = ANY($1::bigint[]) AND resolved_at IS NULL""",
        ids,
    ):
        by_id[f["item_id"]].open_flags[f["kind"]] = {
            "raised_at": f["raised_at"], "detail": f["detail"],
        }
    return items


async def _apply(conn, tx: BoardTx) -> None:
    now = tx.now
    board_id = uuid.UUID(tx.board["id"])
    if tx.board_changed:
        await conn.execute(
            "UPDATE boards SET next_number = $2, updated_at = $3 WHERE id = $1",
            board_id, tx.board["next_number"], now,
        )
    for n in tx.created:
        it = tx.items[n]
        it.id = await conn.fetchval(
            """INSERT INTO items (board_id, number, title, status, touched_at,
                                  created_by, created_at, updated_at)
               VALUES ($1, $2, $3, $4, $5, $6, $5, $5) RETURNING id""",
            board_id, n, it.title, it.status, now, it.created_by,
        )
    for n in sorted(tx.dirty | set(tx.created)):
        it = tx.items[n]
        await conn.execute(
            """UPDATE items SET title = $2, status = $3, note = $4, grp = $5,
                      blocked_on = $6, check_back_at = $7, eta_at = $8,
                      waiting_set_at = $9, links = $10, touched_at = $11,
                      clock_reset_at = $12, closed_at = $13, archived_at = $14,
                      updated_at = $15, stale_nudged_at = $16
                WHERE id = $1""",
            it.id, it.title, it.status, it.note, it.group, it.blocked_on,
            it.check_back_at, it.eta_at, it.waiting_set_at, list(it.links),
            it.touched_at, it.clock_reset_at, it.closed_at, it.archived_at, now,
            it.stale_nudged_at,
        )
    for n in sorted(tx.holders_changed):
        it = tx.items[n]
        await conn.execute("DELETE FROM item_holders WHERE item_id = $1", it.id)
        for h in it.holders:
            await conn.execute(
                """INSERT INTO item_holders (item_id, pid, session_uid, daemon_id, name, added_at)
                   VALUES ($1, $2, $3, $4, $5, $6)""",
                it.id, h["pid"], h.get("session_uid"), h.get("daemon_id"), h.get("name"),
                h.get("added_at") or now,
            )
    for n in sorted(tx.deps_changed):
        it = tx.items[n]
        await conn.execute("DELETE FROM item_deps WHERE item_id = $1", it.id)
        for b in sorted(it.blocked_by):
            await conn.execute(
                "INSERT INTO item_deps (item_id, blocker_id, created_at) VALUES ($1, $2, $3)",
                it.id, tx.items[b].id, now,
            )
    for f in tx.flags_resolved:
        await conn.execute(
            """UPDATE item_flags SET resolved_at = $3, resolved_by = $4, resolution = $5,
                      snooze_until = $6
                WHERE item_id = $1 AND kind = $2 AND resolved_at IS NULL""",
            tx.items[f["n"]].id, f["kind"], now, tx.actor["pid"], f["resolution"],
            f["snooze_until"],
        )
    for f in tx.flags_detail:
        await conn.execute(
            """UPDATE item_flags SET detail = $3
                WHERE item_id = $1 AND kind = $2 AND resolved_at IS NULL""",
            tx.items[f["n"]].id, f["kind"], f["detail"],
        )
    for f in tx.flags_raised:
        await conn.execute(
            """INSERT INTO item_flags (item_id, kind, detail, raised_at)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (item_id, kind) WHERE resolved_at IS NULL DO NOTHING""",
            tx.items[f["n"]].id, f["kind"], f["detail"], now,
        )
    for e in tx.events:
        await conn.execute(
            """INSERT INTO item_events (board_id, item_id, item_number, actor_pid,
                                        actor_name, type, prev, new, reason, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)""",
            board_id, tx.items[e["n"]].id if e.get("n") else None, e.get("n"),
            tx.actor["pid"], tx.actor.get("name"), e["type"], e["prev"], e["new"],
            e["reason"], now,
        )
    for p in tx.pushes:
        await conn.execute(
            """INSERT INTO item_pushes (board_id, daemon_id, session_uid, pid, kind, text,
                                        dedupe, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (dedupe) DO NOTHING""",
            board_id, p["daemon_id"], p["session_uid"], p["pid"], p["kind"], p["text"],
            p["dedupe"], now,
        )


async def _write(pool: asyncpg.Pool, ref: str, actor: dict, extra_ns, run):
    now = utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await _board_by_ref(conn, ref, lock=True)
            items = await _load_items(conn, board["id"], extra_ns)
            tx = BoardTx(board, items, actor, now)
            result = run(tx)
            await _apply(conn, tx)
            if tx.owner_transitions:
                await _push_owner_transitions(conn, board, tx, now)
            return tx, result


async def _orchestrator_row(conn, board: dict, now: datetime) -> dict | None:
    """The orchestrator's state row (explicit pid, else the best candidate)."""
    if board.get("orchestrator_pid"):
        row = await conn.fetchrow("SELECT * FROM session_states WHERE pid = $1",
                                  board["orchestrator_pid"])
        return dict(row) if row else None
    anchor = await _anchor_task(conn, board)
    if anchor is None:
        return None
    row = await conn.fetchrow(ORCHESTRATOR_CANDIDATES_SQL, anchor,
                              now - timedelta(seconds=STATE_FRESH_S))
    return dict(row) if row else None


async def _anchor_task(conn, board: dict):
    if board.get("initiative_id"):
        return await conn.fetchval("SELECT coordinator_task_id FROM initiatives WHERE id = $1",
                                   uuid.UUID(board["initiative_id"]))
    if board.get("root_task_id"):
        return uuid.UUID(board["root_task_id"])
    return None


async def _push_owner_transitions(conn, board: dict, tx: BoardTx, now: datetime) -> None:
    """Tell the orchestrator at once when an item enters or leaves
    blocked_on_owner (unless the orchestrator made the change itself)."""
    orch = await _orchestrator_row(conn, board, now)
    if not orch or not orch.get("daemon_id") or not orch.get("session_uid") \
            or orch["pid"] == tx.actor["pid"]:
        return
    who = tx.actor.get("name") or tx.actor["pid"]
    parts = []
    for n, entered in tx.owner_transitions:
        it = tx.items[n]
        if entered:
            parts.append(f"#{n} \"{it.title}\" is blocked on Owner: {it.blocked_on or 'decision pending'}")
        else:
            parts.append(f"#{n} \"{it.title}\" is no longer blocked on Owner ({who}; now {it.status})")
    text = f"[cm-board {board['slug']}] " + "; ".join(parts) + f". board(board=\"{board['slug']}\")"
    await conn.execute(
        """INSERT INTO item_pushes (board_id, daemon_id, session_uid, pid, kind, text, dedupe,
                                    created_at)
           VALUES ($1, $2, $3, $4, 'owner_blocked', $5, $6, $7) ON CONFLICT (dedupe) DO NOTHING""",
        uuid.UUID(board["id"]), orch["daemon_id"], orch["session_uid"], orch["pid"],
        text[:1000], f"owner_blocked:{board['id']}:{now.isoformat()}:{tx.owner_transitions}", now,
    )


def _write_reply(tx: BoardTx, ns) -> dict:
    return {
        "board": {"id": tx.board["id"], "slug": tx.board["slug"]},
        "items": [tx.items[n].public() for n in ns],
        "unblocked": sorted(set(tx.unblocked)),
        "warnings": tx.warnings,
    }


async def create_items(pool, ref: str, actor: dict, specs: list[dict], *,
                       request_id: str | None = None) -> dict:
    """Create items; a repeated `request_id` on the same board replays the
    first attempt's reply (current state of the items it made)."""
    if not request_id:
        tx, ns = await _write(pool, ref, actor, (), lambda tx: tx.create(specs))
        return _write_reply(tx, ns)
    now = utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await _board_by_ref(conn, ref, lock=True)
            prior = await conn.fetchval(
                "SELECT numbers FROM item_requests WHERE board_id = $1 AND request_id = $2",
                uuid.UUID(board["id"]), request_id,
            )
            if prior is not None:
                items = await _load_items(conn, board["id"], list(prior))
                tx = BoardTx(board, items, actor, now)
                reply = _write_reply(tx, [n for n in prior if n in items])
                reply["replayed"] = True
                return reply
            items = await _load_items(conn, board["id"])
            tx = BoardTx(board, items, actor, now)
            ns = tx.create(specs)
            await _apply(conn, tx)
            await conn.execute(
                """INSERT INTO item_requests (board_id, request_id, actor_pid, numbers, created_at)
                   VALUES ($1, $2, $3, $4, $5)""",
                uuid.UUID(board["id"]), request_id, actor["pid"], ns, now,
            )
            return _write_reply(tx, ns)


async def update_items(pool, ref: str, actor: dict, ns: list[int], fields: dict, *,
                       add_holders=None, remove_holders=None, reason=None) -> dict:
    tx, done = await _write(
        pool, ref, actor, ns,
        lambda tx: tx.update(ns, fields, add_holders=add_holders,
                             remove_holders=remove_holders, reason=reason),
    )
    return _write_reply(tx, done)


async def touch_items(pool, ref: str, actor: dict, ns: list[int], *, source: str,
                      message_id: str | None = None, excerpt: str | None = None) -> dict:
    """Observed activity (doc §4): reset the stale clock of open items the
    actor holds, with a history event; never changes status or note."""
    tx, (touched, skipped) = await _write(
        pool, ref, actor, ns,
        lambda tx: tx.touch(ns, source=source, message_id=message_id, excerpt=excerpt))
    return {"board": {"id": tx.board["id"], "slug": tx.board["slug"]},
            "touched": touched, "skipped": skipped}


async def resolve_item(pool, ref: str, actor: dict, n: int, action: str, **kwargs) -> dict:
    tx, flags = await _write(pool, ref, actor, [n],
                             lambda tx: tx.resolve(n, action, **kwargs))
    return {
        "board": {"id": tx.board["id"], "slug": tx.board["slug"]},
        "item": tx.items[n].public(),
        "flags_resolved": flags,
        "unblocked": sorted(set(tx.unblocked)),
        "warnings": tx.warnings,
    }


async def patch_board(pool, ref: str, actor: dict, changes: dict) -> dict:
    if not actor or not actor.get("pid"):
        raise ItemsError(422, "invalid_field", "actor.pid is required", field="actor")
    sets: dict = {}
    for key, value in changes.items():
        if key in BOARD_SETTINGS:
            if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
                raise ItemsError(422, "invalid_field", f"{key} must be a positive integer",
                                 field=key)
            sets[key] = value
        elif key == "holder_idle_enabled":
            if not isinstance(value, bool):
                raise ItemsError(422, "invalid_field", f"{key} must be a boolean", field=key)
            sets[key] = value
        elif key == "orchestrator_pid":
            if value is not None and (not isinstance(value, str) or len(value) > 200):
                raise ItemsError(422, "invalid_field", "orchestrator_pid is at most 200 characters",
                                 field=key)
            sets[key] = value or None
        elif key == "name":
            if not isinstance(value, str) or not value.strip() or len(value) > 200:
                raise ItemsError(422, "invalid_field", "name must be 1-200 characters",
                                 field=key)
            sets[key] = value.strip()
        else:
            raise ItemsError(422, "invalid_field", f"unknown board setting {key}", field=key)
    now = utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await _board_by_ref(conn, ref, lock=True)
            prev = {k: board[k] for k in sets}
            changed = {k: v for k, v in sets.items() if board[k] != v}
            if changed:
                cols = ", ".join(f"{k} = ${i + 2}" for i, k in enumerate(changed))
                await conn.execute(
                    f"UPDATE boards SET {cols}, updated_at = now() WHERE id = $1",
                    uuid.UUID(board["id"]), *changed.values(),
                )
                await conn.execute(
                    """INSERT INTO item_events (board_id, actor_pid, actor_name, type,
                                                prev, new, created_at)
                       VALUES ($1, $2, $3, 'board_updated', $4, $5, $6)""",
                    uuid.UUID(board["id"]), actor["pid"], actor.get("name"),
                    {k: prev[k] for k in changed}, changed, now,
                )
                board.update(changed)
            return await _board_header(conn, board, now)


async def delete_board(pool, ref: str) -> dict:
    """Delete a board and everything on it, refusing while any item is open
    (for scratch boards; closed items, history and flags go with it)."""
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await _board_by_ref(conn, ref, lock=True)
            open_ns = [r["number"] for r in await conn.fetch(
                """SELECT number FROM items WHERE board_id = $1 AND closed_at IS NULL
                    ORDER BY number""", uuid.UUID(board["id"]))]
            if open_ns:
                raise ItemsError(409, "board_not_empty",
                                 f"board {board['slug']} still has open items; close or drop them first",
                                 open_items=open_ns)
            count = await conn.fetchval("SELECT count(*) FROM items WHERE board_id = $1",
                                        uuid.UUID(board["id"]))
            await conn.execute("DELETE FROM boards WHERE id = $1", uuid.UUID(board["id"]))
    return {"deleted": board["slug"], "id": board["id"], "items": int(count)}


# ---- reads ---------------------------------------------------------------

def holder_state(row, now: datetime) -> dict:
    """A holder's live state from its session_states row (§3)."""
    if row is None:
        return {"state": "unknown", "for_s": None, "reported_done": False, "seen": False}
    if row["exited_at"] is not None:
        return {"state": "exited", "for_s": int((now - row["exited_at"]).total_seconds()),
                "reported_done": row["reported_done"], "seen": True}
    if (now - row["reported_at"]).total_seconds() > STATE_FRESH_S:
        return {"state": "unknown", "for_s": None, "reported_done": row["reported_done"],
                "seen": True}
    since = row["state_since"]
    return {"state": row["state"],
            "for_s": int((now - since).total_seconds()) if since else None,
            "reported_done": row["reported_done"], "seen": True}


async def _version(conn, board_id: str) -> int:
    return int(await conn.fetchval(
        "SELECT COALESCE(max(id), 0) FROM item_events WHERE board_id = $1",
        uuid.UUID(board_id),
    ))


# The orchestrator when the board does not name one: a live session bound to
# the coordinator (initiative) or root task. Bash panes never qualify (a push
# into a shell is lost), Claude/Codex sessions beat any other engine, and
# ties go to the most recent activity (last state change or turn end, then
# last report), not to whichever session started last.
ORCHESTRATOR_CANDIDATES_SQL = """
SELECT * FROM session_states
 WHERE task_id = $1 AND exited_at IS NULL AND reported_at > $2
   AND COALESCE(engine, '') <> 'bash'
 ORDER BY (engine IN ('claude-code', 'codex')) DESC NULLS LAST,
          GREATEST(state_since,
                   to_timestamp(NULLIF(agent_state->'last_turn'->>'ended_at', '')::float8))
              DESC NULLS LAST,
          reported_at DESC
 LIMIT 1"""


async def _orchestrator(conn, board: dict, now: datetime) -> dict | None:
    fresh = now - timedelta(seconds=STATE_FRESH_S)
    if board.get("orchestrator_pid"):
        row = await conn.fetchrow("SELECT * FROM session_states WHERE pid = $1",
                                  board["orchestrator_pid"])
        st = holder_state(row, now)
        return {"pid": board["orchestrator_pid"], "name": row["name"] if row else None,
                "state": st["state"], "explicit": True}
    anchor = None
    if board.get("initiative_id"):
        anchor = await conn.fetchval(
            "SELECT coordinator_task_id FROM initiatives WHERE id = $1",
            uuid.UUID(board["initiative_id"]),
        )
    elif board.get("root_task_id"):
        anchor = uuid.UUID(board["root_task_id"])
    if anchor is None:
        return None
    row = await conn.fetchrow(
        ORCHESTRATOR_CANDIDATES_SQL,
        anchor, fresh,
    )
    if row is None:
        return None
    return {"pid": row["pid"], "name": row["name"], "state": holder_state(row, now)["state"],
            "explicit": False}


async def _health(conn, board_id: str, now: datetime) -> dict:
    row = await conn.fetchrow(
        """SELECT count(*) AS n, min(f.raised_at) AS oldest
             FROM item_flags f JOIN items i ON i.id = f.item_id
            WHERE i.board_id = $1 AND f.resolved_at IS NULL""",
        uuid.UUID(board_id),
    )
    oldest = row["oldest"]
    owner = await conn.fetch(
        """SELECT number FROM items WHERE board_id = $1 AND status = $2 AND archived_at IS NULL
            ORDER BY number""",
        uuid.UUID(board_id), OWNER_BLOCKED,
    )
    return {"blocked_on_owner": len(owner), "owner_items": [r["number"] for r in owner],
            "unresolved": int(row["n"]),
            "oldest_s": int((now - oldest).total_seconds()) if oldest else None}


async def _board_header(conn, board: dict, now: datetime) -> dict:
    return {
        "id": board["id"],
        "slug": board["slug"],
        "name": board["name"],
        "initiative_id": board.get("initiative_id"),
        "root_task_id": board.get("root_task_id"),
        "version": await _version(conn, board["id"]),
        "orchestrator": await _orchestrator(conn, board, now),
        "settings": _board_settings(board),
        "health": await _health(conn, board["id"], now),
    }


async def list_boards(pool, *, open_only: bool = False) -> list[dict]:
    now = utcnow()
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT b.* FROM boards b
                WHERE NOT $1 OR EXISTS (
                      SELECT 1 FROM items i WHERE i.board_id = b.id AND i.closed_at IS NULL)
                ORDER BY b.updated_at DESC, b.slug""",
            open_only,
        )
        return [await _board_header(conn, _str_ids(dict(r)), now) for r in rows]


async def _free_capacity(conn, board: dict, now: datetime) -> list[dict]:
    """Live sessions on the board (task resolves to it, or holding an open item
    there) that hold no active or waiting item on it."""
    fresh = now - timedelta(seconds=STATE_FRESH_S)
    rows = await conn.fetch(
        """WITH RECURSIVE tree AS (
               SELECT id FROM tasks WHERE id = $2
               UNION
               SELECT t.id FROM tasks t JOIN tree ON t.parent_task_id = tree.id),
           board_tasks AS (
               SELECT id FROM tree
               UNION
               SELECT id FROM tasks WHERE $3::uuid IS NOT NULL AND initiative_id = $3),
           holders AS (
               SELECT h.pid, i.status FROM item_holders h JOIN items i ON i.id = h.item_id
                WHERE i.board_id = $1 AND i.closed_at IS NULL)
           SELECT s.pid, s.name, s.state FROM session_states s
            WHERE s.exited_at IS NULL AND s.reported_at > $4
              AND (s.task_id IN (SELECT id FROM board_tasks)
                   OR s.pid IN (SELECT pid FROM holders))
              AND s.pid NOT IN (SELECT pid FROM holders WHERE status IN ('active', 'waiting'))
            ORDER BY s.name NULLS LAST, s.pid""",
        uuid.UUID(board["id"]),
        uuid.UUID(board["root_task_id"]) if board.get("root_task_id") else None,
        uuid.UUID(board["initiative_id"]) if board.get("initiative_id") else None,
        fresh,
    )
    return [dict(r) for r in rows]


async def read_board(pool, ref: str, *, since_version: int | None = None,
                     archived: bool = False, q: str | None = None,
                     history: int = 0) -> dict:
    now = utcnow()
    async with pool.acquire() as conn:
        board = await _board_by_ref(conn, ref)
        version = await _version(conn, board["id"])
        if since_version is not None and since_version == version:
            return {"unchanged": True, "version": version}
        header = await _board_header(conn, board, now)
        items = await _load_items(conn, board["id"], archived=archived)
        pids = sorted({h["pid"] for it in items.values() for h in it.holders})
        states = {
            r["pid"]: r for r in await conn.fetch(
                "SELECT * FROM session_states WHERE pid = ANY($1::text[])", pids)
        }
        flags = {}
        ids = [it.id for it in items.values()]
        for f in await conn.fetch(
            """SELECT f.*, i.number FROM item_flags f JOIN items i ON i.id = f.item_id
                WHERE f.item_id = ANY($1::bigint[]) AND f.resolved_at IS NULL
                ORDER BY f.raised_at, i.number""",
            ids,
        ):
            flags.setdefault(f["number"], []).append(f)
        events: dict[int, list] = {}
        history = max(0, min(int(history or 0), MAX_HISTORY))
        if history and ids:
            for e in await conn.fetch(
                """SELECT * FROM (
                       SELECT e.*, row_number() OVER (PARTITION BY item_id ORDER BY id DESC) rn
                         FROM item_events e WHERE item_id = ANY($1::bigint[])) x
                    WHERE rn <= $2 ORDER BY id DESC""",
                ids, history,
            ):
                events.setdefault(e["item_number"], []).append({
                    "id": e["id"], "type": e["type"], "actor": e["actor_name"] or e["actor_pid"],
                    "actor_pid": e["actor_pid"], "prev": e["prev"], "new": e["new"],
                    "reason": e["reason"], "at": iso(e["created_at"]),
                })
        needle = q.lower() if q else None

        def view(it: Item) -> dict:
            out = it.public()
            for h, src in zip(out["holders"], it.holders):
                h["state"] = holder_state(states.get(src["pid"]), now)
            out["blocks"] = sorted(o.n for o in items.values()
                                   if it.n in o.blocked_by and o.status not in CLOSED)
            if history:
                out["history"] = events.get(it.n, [])
            return out

        def matches(it: Item) -> bool:
            if not needle:
                return True
            hay = " ".join(x for x in (it.title, it.note, it.group) if x).lower()
            return needle in hay

        live, closed, old = [], [], []
        for it in sorted(items.values(), key=lambda it: ((it.group or "").lower(), it.n)):
            if not matches(it):
                continue
            if it.archived_at is not None:
                old.append(view(it))
            elif it.closed_at is not None:
                closed.append(view(it))
            else:
                live.append(view(it))
        closed.sort(key=lambda v: v["closed_at"] or "", reverse=True)
        out = {
            "board": header,
            "flags": [
                {"n": n, "kind": f["kind"], "detail": f["detail"],
                 "raised_at": iso(f["raised_at"]),
                 "age_s": int((now - f["raised_at"]).total_seconds())}
                for n, fs in flags.items() for f in fs
            ],
            "items": live,
            "recently_closed": closed,
            "free_capacity": await _free_capacity(conn, board, now),
        }
        out["flags"].sort(key=lambda f: (-f["age_s"], f["n"]))
        if archived:
            out["archived"] = old
        return out


async def owner_blocked(pool) -> list[dict]:
    """Every item blocked on Owner, across boards, with its holders (for the
    TUI's sidebar marker and status-bar count)."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT b.slug, b.name AS board_name, i.id, i.number, i.title, i.blocked_on,
                      i.touched_at
                 FROM items i JOIN boards b ON b.id = i.board_id
                WHERE i.status = $1 AND i.archived_at IS NULL
                ORDER BY i.touched_at, b.slug, i.number""",
            OWNER_BLOCKED,
        )
        holders = await conn.fetch(
            """SELECT item_id, pid, session_uid, daemon_id, name FROM item_holders
                WHERE item_id = ANY($1::bigint[]) ORDER BY added_at, pid""",
            [r["id"] for r in rows],
        )
    by_item: dict[int, list] = {}
    for h in holders:
        by_item.setdefault(h["item_id"], []).append(
            {"pid": h["pid"], "session_uid": h["session_uid"], "daemon_id": h["daemon_id"],
             "name": h["name"]})
    return [{"board": r["slug"], "board_name": r["board_name"], "n": r["number"],
             "title": r["title"], "decision": r["blocked_on"], "since": iso(r["touched_at"]),
             "holders": by_item.get(r["id"], [])} for r in rows]


async def held_items(pool, holder_pid: str, *, open_only: bool = True) -> list[dict]:
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT b.slug, i.number, i.title, i.status FROM item_holders h
                 JOIN items i ON i.id = h.item_id JOIN boards b ON b.id = i.board_id
                WHERE h.pid = $1 AND (NOT $2 OR i.closed_at IS NULL)
                  AND i.archived_at IS NULL
                ORDER BY b.slug, i.number""",
            holder_pid, open_only,
        )
    return [{"board": r["slug"], "n": r["number"], "title": r["title"],
             "status": r["status"]} for r in rows]


# ---- host heartbeat and push pickup (doc §3, §5) ---------------------------

MAX_PUSHES_PER_BEAT = 100
# A push handed out this many times without an ack is given up on (dropped
# with a reason) so it cannot starve newer ones (~5 min at 30 s beats).
MAX_PUSH_ATTEMPTS = 10
_TEXT_LIMITS = {"session_uid": 200, "task_id": 200, "name": 200, "engine": 40,
                "state": 40, "killed_by": 200}


def _clip(value, limit: int):
    if value is None:
        return None
    text = str(value)
    return text[:limit] if text else None


def _seconds(value):
    try:
        x = float(value)
    except (TypeError, ValueError):
        return None
    return x if 0 <= x <= 10**9 else None


def _session_row(raw, prefix: str) -> tuple[dict | None, str | None]:
    """One heartbeat row, clipped to the column limits, or why it was dropped."""
    if not isinstance(raw, dict):
        return None, "not an object"
    pid = raw.get("pid")
    if not isinstance(pid, str) or not pid.startswith(prefix) or len(pid) > 300:
        return None, "pid does not belong to this daemon"
    row = {k: _clip(raw.get(k), n) for k, n in _TEXT_LIMITS.items()}
    if not row["session_uid"] or not row["state"]:
        return None, "session_uid and state are required"
    row["pid"] = pid
    for key in ("state_age_s", "idle_for_s", "age_s"):
        row[key] = _seconds(raw.get(key))
    row["reported_done"] = raw.get("reported_done") is True
    agent_state = raw.get("agent_state")
    row["agent_state"] = agent_state if isinstance(agent_state, dict) else None
    return row, None


def _uuid_or_none(value):
    try:
        return uuid.UUID(str(value)) if value else None
    except ValueError:
        return None


async def heartbeat(pool, daemon_id: str, *, host_label: str | None, sessions: list[dict],
                    exited: list[str], acked_push_ids: list[int]) -> dict:
    """Record one daemon's full session snapshot, ack delivered pushes, and
    return the pushes still pending for it.

    The snapshot is complete for that daemon: a pid of this daemon that is
    missing from it is marked exited. Holder states take no board lock: they
    write no item events or flags (the flag engine does, under the lock).
    """
    prefix = f"agent:{daemon_id}:"
    rows, dropped = [], []
    for raw in sessions:
        row, reason = _session_row(raw, prefix)
        if row is None:
            dropped.append({"pid": str(raw.get("pid"))[:200] if isinstance(raw, dict) else None,
                            "reason": reason})
        else:
            rows.append(row)
    sessions = rows
    host_label = _clip(host_label, 200)
    exited = [e[:200] for e in exited if isinstance(e, str)]
    now = utcnow()

    def ago(seconds):
        return None if seconds is None else now - timedelta(seconds=float(seconds))

    async with pool.acquire() as conn:
        async with conn.transaction():
            for s in sessions:
                gone = s["state"] == "exited"
                await conn.execute(
                    """INSERT INTO session_states (pid, daemon_id, session_uid, host_label, task_id,
                                                   name, engine, state, state_since, idle_since,
                                                   reported_done, killed_by, agent_state,
                                                   started_at, exited_at, reported_at)
                       VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                               CASE WHEN $15 THEN $16::timestamptz END, $16)
                       ON CONFLICT (pid) DO UPDATE SET
                           daemon_id = EXCLUDED.daemon_id, session_uid = EXCLUDED.session_uid,
                           host_label = EXCLUDED.host_label, task_id = EXCLUDED.task_id,
                           name = EXCLUDED.name, engine = EXCLUDED.engine, state = EXCLUDED.state,
                           state_since = EXCLUDED.state_since, idle_since = EXCLUDED.idle_since,
                           reported_done = EXCLUDED.reported_done, killed_by = EXCLUDED.killed_by,
                           agent_state = EXCLUDED.agent_state,
                           started_at = COALESCE(EXCLUDED.started_at, session_states.started_at),
                           exited_at = CASE WHEN $15
                                            THEN COALESCE(session_states.exited_at, $16::timestamptz)
                                       END,
                           reported_at = EXCLUDED.reported_at""",
                    s["pid"], daemon_id, s["session_uid"], host_label,
                    _uuid_or_none(s.get("task_id")), s.get("name"), s.get("engine"), s["state"],
                    ago(s.get("state_age_s")), ago(s.get("idle_for_s")),
                    bool(s.get("reported_done")), s.get("killed_by"), s.get("agent_state"),
                    ago(s.get("age_s")), gone, now,
                )
            live = [s["pid"] for s in sessions if s["state"] != "exited"]
            await conn.execute(
                """UPDATE session_states SET exited_at = $3, state = 'exited', reported_at = $3
                    WHERE daemon_id = $1 AND exited_at IS NULL
                      AND (NOT (pid = ANY($2::text[])) OR pid = ANY($4::text[]))""",
                daemon_id, live, now, list(exited),
            )
            if acked_push_ids:
                await conn.execute(
                    """UPDATE item_pushes SET delivered_at = $3
                        WHERE daemon_id = $1 AND id = ANY($2::bigint[]) AND delivered_at IS NULL""",
                    daemon_id, list(acked_push_ids), now,
                )
            # Give up on pushes handed out too often without an ack.
            await conn.execute(
                """UPDATE item_pushes
                      SET delivered_at = $3,
                          dropped_reason = 'not acked after ' || attempts || ' attempts'
                    WHERE daemon_id = $1 AND delivered_at IS NULL AND attempts >= $2""",
                daemon_id, MAX_PUSH_ATTEMPTS, now,
            )
            rows = await conn.fetch(
                """UPDATE item_pushes p SET attempts = p.attempts + 1
                     FROM (SELECT id FROM item_pushes
                            WHERE daemon_id = $1 AND delivered_at IS NULL
                            ORDER BY id LIMIT $2) pending
                    WHERE p.id = pending.id
                RETURNING p.id, p.session_uid, p.pid, p.kind, p.text, p.owner_alert,
                          p.board_id, p.attempts,
                          (SELECT slug FROM boards b WHERE b.id = p.board_id) AS board""",
                daemon_id, MAX_PUSHES_PER_BEAT,
            )
    out = {
        "pushes": sorted((_str_ids(dict(r)) for r in rows), key=lambda r: r["id"]),
        "server_time": iso(now),
    }
    if dropped:
        out["dropped_sessions"] = dropped
    return out
