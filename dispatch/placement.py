"""Link-first placement, the status mirror and board groups (doc/org-model.md §7).

A planning task is *placed* on a board by creating an item linked to it
(`items.task_id`, one live link per task). While it is placed the task owns
title, description and metadata, and the item owns status, holders, group and
flags; the task's `status` mirrors the item in the same transaction, along
with a `placement` copy the task change feed carries. Unplacing archives the
link (the item stays, read-only) and returns the task to the backlog.

Every transaction here takes the task-change advisory lock FIRST, then the
board and task rows, the order `dispatch/db.py` requires (§7.1, "Lock order").
"""

from __future__ import annotations

import os
import uuid

from dispatch import items_db
from dispatch.items_rules import (CLOSED, GROUP_KINDS, MAX_DESCRIPTION, MAX_GROUP, OWNER_PID,
                                  BoardTx, ItemsError, _holder, _text, duplicate_hints, invalid,
                                  iso)

# The capability a daemon advertises once its terminal sweep honours
# `terminal_via=item` and protects members' workspaces (§12).
GUARD_CAP = "lifecycle_guard_v1"
# Item status -> task status (§7.1).
MIRROR = {
    "open": "backlog", "active": "running", "waiting": "running",
    "blocked": "blocked", "blocked_on_owner": "blocked",
    "done": "done", "dropped": "done",
}
LEGACY_BLOCKED_ON = "legacy status write: no reason given"
LEGACY_CHECK_BACK = "24h"


def mirror_enabled() -> bool:
    """The rollback switch (§13, S3): CM_ORG_MIRROR=0 turns mirroring off;
    links are kept."""
    return os.environ.get("CM_ORG_MIRROR", "1").strip() != "0"


def placement_json(board: dict, it, **extra) -> dict:
    out = {"board": board["slug"], "board_id": board["id"], "n": it.n, "status": it.status,
           "group_id": it.group_id, "group": it.group}
    out.update(extra)
    return out


# A daemon counts toward the gate while its last heartbeat is this fresh.
GATE_FRESH_S = 120


async def mirror_gate(conn, task_id: str) -> list[str]:
    """Why the task's status may not be mirrored now; empty = mirror.

    Fails closed (review of 6522623): an old sweep would kill a task's
    sessions on a mirrored `done`, and a daemon that never beats, an old
    brain or a session in its first 30 s may hold sessions no row shows. So
    the mirror runs only while EVERY freshly heartbeating daemon advertises
    lifecycle_guard_v1 in `org_daemons` (org S2), there is at least one, and
    every daemon holding the task's live sessions is among them. No build
    advertises it yet, so today the mirror is off everywhere."""
    if not mirror_enabled():
        return ["disabled"]
    rows = await conn.fetch(
        """SELECT daemon_id, capabilities FROM org_daemons
            WHERE seen_at > now() - make_interval(secs => $1)""", GATE_FRESH_S)
    if not rows:
        return ["no_fresh_daemons"]
    blocking = {r["daemon_id"] for r in rows if GUARD_CAP not in (r["capabilities"] or [])}
    guarded = {r["daemon_id"] for r in rows} - blocking
    holding = {r["daemon_id"] for r in await conn.fetch(
        """SELECT DISTINCT daemon_id FROM session_states
            WHERE task_id = $1 AND exited_at IS NULL""", uuid.UUID(task_id))}
    blocking |= holding - guarded
    return sorted(blocking)


async def mirror_is_open(pool, task_id: str) -> bool:
    async with pool.acquire() as conn:
        return not await mirror_gate(conn, task_id)


async def _write_task_side(conn, task_id: str, item_status: str, place: dict, now, *,
                           extra: dict | None = None) -> None:
    """Write a placed task's side of its item: task-owned `extra` fields,
    the placement copy, and, while the gate is open, the mirrored status
    and its terminal marks. With the gate closed the task status is left
    as it is (legacy writes keep updating it) and placement says why."""
    row = await conn.fetchrow(
        "SELECT status, metadata, placement FROM tasks WHERE id = $1 FOR UPDATE",
        uuid.UUID(task_id))
    if row is None:
        return
    sets: dict = dict(extra or {})
    place = dict(place)
    waiting_on = await mirror_gate(conn, task_id)
    if waiting_on:
        place["mirror"] = "off"
        place["mirror_waiting_on"] = waiting_on
    else:
        target = MIRROR.get(item_status)
        if target and target != row["status"]:
            sets["status"] = target
            if target == "blocked":
                sets["blocked_at"] = now
        meta = dict(row["metadata"] or {})
        before = dict(meta)
        meta.pop("terminal_via", None)
        meta.pop("dropped", None)
        if item_status in CLOSED:
            # A board-originated close never arms the terminal sweep against
            # members (§7.1); the daemon's guard reads this.
            meta["terminal_via"] = "item"
            if item_status == "dropped":
                meta["dropped"] = True
        if meta != before:
            sets["metadata"] = meta
    if place != (row["placement"] or None):
        sets["placement"] = place
    if sets:
        cols = ", ".join(f"{k} = ${i + 2}" for i, k in enumerate(sets))
        await conn.execute(f"UPDATE tasks SET {cols}, updated_at = now() WHERE id = $1",
                           uuid.UUID(task_id), *sets.values())


async def apply_task_side(conn, tx: BoardTx) -> None:
    """The task half of an item write, in the same transaction: task-owned
    fields edited on the board, the status mirror, the placement copy, and
    launch grants following the owner."""
    for n in sorted(tx.dirty | set(tx.created)):
        it = tx.items[n]
        if not it.task_id or it.link_archived_at is not None:
            continue
        writes = tx.task_writes.get(n, {})
        extra: dict = {}
        if "name" in writes:
            extra["name"] = writes["name"]
        if "description" in writes:
            extra["description"] = writes["description"]
            extra["description_rev"] = it.description_rev
        await _write_task_side(conn, it.task_id, it.status,
                               placement_json(tx.board, it, item_id=it.id), tx.now, extra=extra)
    await _move_launch_grants(conn, tx)


async def remirror_pending(pool, limit: int = 50) -> int:
    """Placed tasks whose mirror was off (the gate closed): once the gate
    opens, write the item's status through. Run by the board engine loop;
    each task in its own transaction, advisory lock first. Returns how many
    it mirrored."""
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT t.id FROM tasks t WHERE t.placement->>'mirror' = 'off'
                ORDER BY t.updated_at LIMIT $1""", limit)
    mirrored = 0
    for r in rows:
        async with pool.acquire() as conn:
            async with conn.transaction():
                await conn.execute(items_db.TASK_WRITE_LOCK_SQL)
                if await mirror_gate(conn, str(r["id"])):
                    continue
                item = await conn.fetchrow(
                    """SELECT i.id, i.number, i.status, i.group_id, i.grp, b.id AS board_id, b.slug
                         FROM items i JOIN boards b ON b.id = i.board_id
                        WHERE i.task_id = $1 AND i.link_archived_at IS NULL""", r["id"])
                if item is None:
                    continue
                place = {"board": item["slug"], "board_id": str(item["board_id"]),
                         "n": item["number"], "status": item["status"],
                         "group_id": str(item["group_id"]) if item["group_id"] else None,
                         "group": item["grp"], "item_id": item["id"]}
                await _write_task_side(conn, str(r["id"]), item["status"], place,
                                       items_db.utcnow())
                mirrored += 1
    return mirrored


async def _may_grant(conn, tx: BoardTx, old_pid: str | None) -> bool:
    """Who can make an owner a launch grantee (review of 6522623): Owner, the
    board's orchestrator or steward, or the previous owner handing over. A
    session naming itself owner of somebody else's task gets the role but
    no grant, so ownership never widens anyone's launch scope by itself."""
    pid = tx.actor["pid"]
    if pid == OWNER_PID or (old_pid and pid == old_pid):
        return True
    if pid in (tx.board.get("steward_pid"), tx.board.get("orchestrator_pid")):
        return True
    return pid == await _orchestrator_pid(conn, tx.board)


async def _move_launch_grants(conn, tx: BoardTx) -> None:
    """The owner-role holder of a placed item may launch for its task (§7.4).
    The grant follows the owner when an authorized actor moves it; closing,
    handing back or unplacing revokes it."""
    for n, old_pid, new in tx.owner_changes:
        it = tx.items[n]
        if not it.task_id:
            continue
        task = uuid.UUID(it.task_id)
        if old_pid:
            reason = ("item closed" if it.status in CLOSED
                      else "handed back" if new is None else "owner changed")
            await conn.execute(
                """UPDATE launch_grants SET revoked_at = $3, revoked_by = $4, reason = $5
                    WHERE task_id = $1 AND pid = $2 AND revoked_at IS NULL""",
                task, old_pid, tx.now, tx.actor["pid"], reason)
        if (new is not None and it.link_archived_at is None and it.status not in CLOSED
                and await _may_grant(conn, tx, old_pid)):
            await conn.execute(
                """INSERT INTO launch_grants (task_id, pid, daemon_id, session_uid, item_id,
                                              granted_by, granted_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7)
                   ON CONFLICT (task_id, pid) WHERE revoked_at IS NULL DO NOTHING""",
                task, new["pid"], new.get("daemon_id"), new.get("session_uid"), it.id,
                tx.actor["pid"], tx.now)


async def revoke_task_grants(conn, task_id: str, actor: dict, now, reason: str) -> None:
    await conn.execute(
        """UPDATE launch_grants SET revoked_at = $2, revoked_by = $3, reason = $4
            WHERE task_id = $1 AND revoked_at IS NULL""",
        uuid.UUID(task_id), now, actor["pid"], reason)


async def daemon_launch_grants(conn, daemon_id: str) -> list[dict]:
    """The live grants a daemon enforces, for its heartbeat reply."""
    rows = await conn.fetch(
        """SELECT g.task_id, g.pid, g.session_uid, g.granted_at, i.number AS n, b.slug AS board
             FROM launch_grants g
             LEFT JOIN items i ON i.id = g.item_id
             LEFT JOIN boards b ON b.id = i.board_id
            WHERE g.daemon_id = $1 AND g.revoked_at IS NULL
            ORDER BY g.id""",
        daemon_id)
    return [{"task_id": str(r["task_id"]), "pid": r["pid"], "session_uid": r["session_uid"],
             "granted_at": iso(r["granted_at"]), "board": r["board"], "n": r["n"]}
            for r in rows]


# ---- placing and unplacing ---------------------------------------------------

def _refuse_unplaceable(task: dict) -> None:
    kind = task.get("kind") or "oneshot"
    if kind == "backtest":
        raise ItemsError(409, "not_placeable",
                         "backtests are never placed on boards; their status, VM teardown and "
                         "archive lifecycle stay as they are (doc/org-model.md §9.4)",
                         reason="backtest")
    if kind == "continuous":
        raise ItemsError(409, "not_placeable",
                         "a continuous task is a standing member, not a work item: its scheduler "
                         "owns its status", reason="continuous")
    if task.get("is_cloud") and task.get("project") is None:
        raise ItemsError(409, "not_placeable",
                         "this task is auto-dispatchable (is_cloud with no project): the dispatch "
                         "daemon would claim it from under the board", reason="auto_dispatch")


async def _lock_task(conn, task_id: str) -> dict:
    try:
        tid = uuid.UUID(str(task_id))
    except ValueError:
        raise ItemsError(404, "not_found", f"no task {task_id}") from None
    row = await conn.fetchrow("SELECT * FROM tasks WHERE id = $1 FOR UPDATE", tid)
    if row is None:
        raise ItemsError(404, "not_found", f"no task {task_id}")
    return dict(row)


async def _live_link(conn, task_id: str):
    return await conn.fetchrow(
        """SELECT i.id, i.number, b.slug, b.id AS board_id FROM items i JOIN boards b ON b.id = i.board_id
            WHERE i.task_id = $1 AND i.link_archived_at IS NULL""",
        uuid.UUID(str(task_id)))


async def place_task(pool, task_id: str, ref: str, actor: dict, *, group: str | None = None,
                     group_id: str | None = None, holder: dict | None = None,
                     role: str | None = None, replace: bool = False) -> dict:
    """Place a planning task on a board (§7.1). Re-placing on the same board
    reactivates the archived link; a task placed elsewhere is refused unless
    `replace` (unplace there and place here, one step)."""
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            await conn.execute(items_db.TASK_WRITE_LOCK_SQL)
            board = await items_db._board_by_ref(conn, ref, lock=True)
            task = await _lock_task(conn, task_id)
            task_id = str(task["id"])
            _refuse_unplaceable(task)
            if task["status"] in ("done", "archived"):
                raise ItemsError(409, "task_closed",
                                 f"task {task_id[:8]} is {task['status']}; reopen it in planning "
                                 f"before placing it")
            if not (task.get("description") or "").strip():
                raise invalid("description_required",
                              "placing needs a description: it is the work contract the holder "
                              "receives (scope, acceptance criteria, constraints)",
                              field="description")
            link = await _live_link(conn, task_id)
            if link is not None and str(link["board_id"]) == board["id"]:
                items = await items_db._load_items(conn, board["id"], [link["number"]])
                it = items[link["number"]]
                return {"board": board["slug"], "n": it.n, "item_status": it.status,
                        "task_status": task["status"], "already_placed": True,
                        "item": it.public()}
            if link is not None:
                if not replace:
                    raise ItemsError(409, "placed_elsewhere",
                                     f"task {task_id[:8]} is placed on board {link['slug']} as "
                                     f"#{link['number']}; unplace it first, or pass replace=true "
                                     f"to move it here in one step",
                                     board=link["slug"], n=link["number"])
                other = await items_db._board_by_ref(conn, str(link["board_id"]), lock=True)
                await _unplace_locked(conn, other, task_id, link["number"], actor, now,
                                      reason=f"re-placed on board {board['slug']}")
            items = await items_db._load_items(conn, board["id"])
            archived = await conn.fetchval(
                """SELECT number FROM items WHERE board_id = $1 AND task_id = $2
                    ORDER BY link_archived_at DESC NULLS LAST LIMIT 1""",
                uuid.UUID(board["id"]), uuid.UUID(task_id))
            if archived is not None and archived not in items:
                items.update(await items_db._load_items(conn, board["id"], [archived]))
            groups = await items_db._load_groups(conn, board["id"])
            tx = BoardTx(board, items, actor, now, groups)
            holders = [_holder({**holder, "role": role or holder.get("role")})] if holder else []
            spec: dict = {}
            if group_id:
                spec["group_id"] = group_id
            elif group:
                spec["group"] = group
            if archived is not None:
                # Re-placing on the same board reactivates the old link: the
                # item keeps its number, history and flags.
                it = tx.items[archived]
                it.link_archived_at = None
                it.archived_at = None
                if it.status in CLOSED:
                    it.closed_at = None
                status = "active" if holders or items_db._strict(board) else "open"
                tx.apply(archived, {"status": status, **spec}, holders=holders,
                         reason="re-placed", reopen=True)
                n = archived
            else:
                (n,) = tx.create([{"title": task["name"] or task_id[:8], "holders": holders,
                                   **spec}])
                it = tx.items[n]
                it.task_id = task_id
                tx._event(it, "placed", new={"task_id": task_id})
            it = tx.items[n]
            it.task_id = task_id
            it.description = task.get("description")
            it.description_rev = task.get("description_rev") or 0
            # Placing a draft accepts it, as A-a does.
            if task["status"] == "draft":
                await conn.execute("UPDATE tasks SET status = 'backlog' WHERE id = $1",
                                   uuid.UUID(task_id))
            if it.holders:
                owner = next((h for h in it.holders if h.get("role") == "owner"), None)
                if owner and not any(c[0] == n for c in tx.owner_changes):
                    tx.owner_changes.append((n, None, owner))
            await items_db._apply(conn, tx, mirror=True)
            status = await conn.fetchval("SELECT status FROM tasks WHERE id = $1",
                                         uuid.UUID(task_id))
            reply = {"board": board["slug"], "n": n, "item_status": it.status,
                     "task_status": status, "item": it.public(), "warnings": tx.warnings}
            dups = duplicate_hints(it, tx.items.values())
            if dups:
                reply["possible_duplicate"] = dups
            return reply


async def _unplace_locked(conn, board: dict, task_id: str, n: int, actor: dict, now,
                          *, reason: str | None) -> None:
    items = await items_db._load_items(conn, board["id"], [n])
    it = items[n]
    blocked_by = sorted(it.blocked_by)
    blocks = sorted(o.n for o in items.values() if n in o.blocked_by and o.status not in CLOSED)
    if (blocked_by and it.status not in CLOSED) or blocks:
        raise ItemsError(409, "has_dependencies",
                         f"#{n} has live blocked_by edges (blocked by {blocked_by or 'none'}, "
                         f"blocks {blocks or 'none'}): clear or re-point them first; a blocker "
                         f"never silently becomes a link", blocked_by=blocked_by, blocks=blocks)
    tx = BoardTx(board, items, actor, now)
    it.link_archived_at = now
    it.archived_at = now
    tx.dirty.add(n)
    tx._event(it, "unplaced", prev={"task_id": task_id, "status": it.status},
              new={"link_archived_at": iso(now)}, reason=reason)
    for kind in sorted(it.open_flags):
        tx.resolve_flag(it, kind, "unplaced")
    await items_db._apply(conn, tx)
    await revoke_task_grants(conn, task_id, actor, now, "unplaced")
    await conn.execute(
        """UPDATE tasks SET status = 'backlog', placement = NULL,
                  metadata = COALESCE(metadata, '{}'::jsonb) - 'terminal_via' - 'dropped',
                  updated_at = now()
            WHERE id = $1""",
        uuid.UUID(task_id))


async def archive_link(pool, task_id: str, actor: dict) -> dict:
    """Archive a closed placed item's link along with its task (planning's
    bulk archive). The item keeps its number and history, read-only."""
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            await conn.execute(items_db.TASK_WRITE_LOCK_SQL)
            task = await _lock_task(conn, task_id)
            link = await _live_link(conn, str(task["id"]))
            if link is None:
                return {"archived": False}
            board = await items_db._board_by_ref(conn, str(link["board_id"]), lock=True)
            items = await items_db._load_items(conn, board["id"], [link["number"]])
            it = items[link["number"]]
            if it.status not in CLOSED:
                raise ItemsError(409, "placed_on_board", f"#{it.n} is still {it.status}")
            tx = BoardTx(board, items, actor, now)
            it.link_archived_at = now
            it.archived_at = it.archived_at or now
            tx.dirty.add(it.n)
            tx._event(it, "unplaced", prev={"task_id": str(task["id"]), "status": it.status},
                      new={"link_archived_at": iso(now)}, reason="task archived")
            await items_db._apply(conn, tx)
            await revoke_task_grants(conn, str(task["id"]), actor, now, "task archived")
            await conn.execute("UPDATE tasks SET placement = NULL WHERE id = $1", task["id"])
            return {"archived": True, "board": board["slug"], "n": it.n}


async def unplace_task(pool, task_id: str, actor: dict, *, reason: str | None = None) -> dict:
    """Archive a task's link and send the task back to the backlog (§7.1)."""
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            await conn.execute(items_db.TASK_WRITE_LOCK_SQL)
            task = await _lock_task(conn, task_id)
            task_id = str(task["id"])
            link = await _live_link(conn, task_id)
            if link is None:
                raise ItemsError(409, "not_placed", f"task {task_id[:8]} is not on a board")
            board = await items_db._board_by_ref(conn, str(link["board_id"]), lock=True)
            await _unplace_locked(conn, board, task_id, link["number"], actor, now, reason=reason)
            return {"board": board["slug"], "n": link["number"], "task_status": "backlog",
                    "unplaced": True}


# ---- legacy task writes --------------------------------------------------------

async def translate_task_status(pool, task: dict, status: str, actor: dict, *,
                                reason: str | None = None,
                                blocked_by: list | None = None) -> dict | None:
    """A legacy status write to a placed task (§7.1 table): translated into
    the item transition, attributed to the caller. Returns the write's reply,
    or None when the write changes nothing."""
    placement = task.get("placement") or {}
    ref, n = placement.get("board_id"), placement.get("n")
    if not ref or not n:
        return None
    current = placement.get("status")
    if MIRROR.get(current) == status and not reason and not blocked_by:
        return None  # the old editor always sends the status it read
    if status == "backlog":
        return await unplace_task(pool, str(task["id"]), actor,
                                  reason=reason or "legacy status write: backlog")
    if status == "archived":
        # A-A bulk archive: a closed item's link is archived with its task;
        # an open one is still work on the board.
        if current in CLOSED:
            return await archive_link(pool, str(task["id"]), actor)
        raise ItemsError(409, "placed_on_board",
                         f"task {str(task['id'])[:8]} is still {current} on board "
                         f"{placement.get('board')} as #{n}; close or unplace it before archiving",
                         board=placement.get("board"), n=n)
    if status == "done":
        return await items_db.update_items(pool, ref, actor, [n], {"status": "done"},
                                           reason=reason or "legacy status write: done",
                                           reopen=True)
    if status == "blocked":
        fields: dict = {"status": "blocked"}
        if blocked_by:
            fields["blocked_by"] = [int(b) for b in blocked_by]
        if reason:
            fields["blocked_on"] = reason[:200]
            fields["check_back"] = LEGACY_CHECK_BACK
        if not blocked_by and not reason:
            # No trigger is invented: a labelled default and a flag asking the
            # holder for the real one.
            fields["blocked_on"] = LEGACY_BLOCKED_ON
            fields["check_back"] = LEGACY_CHECK_BACK
        reply = await items_db.update_items(pool, ref, actor, [n], fields,
                                            reason=reason or "legacy status write: blocked",
                                            reopen=True)
        if not blocked_by and not reason:
            await raise_flag(pool, ref, n, "legacy_blocked",
                             {"blocked_on": LEGACY_BLOCKED_ON, "check_back": LEGACY_CHECK_BACK},
                             push=("the task was marked blocked with no reason; set the real "
                                   "trigger: item_set(n, blocked_on=..., check_back=...) or "
                                   "blocked_by=[m]"))
        return reply
    if status == "running":
        holder = None
        if actor.get("session_uid") and actor.get("daemon_id"):
            holder = _holder(actor)
        held = await _holders_of(pool, ref, n)
        add = [holder] if holder and holder["pid"] not in held else []
        return await items_db.update_items(pool, ref, actor, [n], {"status": "active"},
                                           add_holders=add, reopen=True,
                                           reason=reason or "legacy status write: running")
    raise ItemsError(409, "placed_on_board",
                     f"task {str(task['id'])[:8]} is placed on board {placement.get('board')} as "
                     f"#{n}; its status follows the item. Use item_set({n}, ...) on that board, "
                     f"or unplace it first", board=placement.get("board"), n=n)


async def _holders_of(pool, ref: str, n: int) -> list[str]:
    async with pool.acquire() as conn:
        board = await items_db._board_by_ref(conn, ref)
        rows = await conn.fetch(
            """SELECT h.pid FROM item_holders h JOIN items i ON i.id = h.item_id
                WHERE i.board_id = $1 AND i.number = $2""", uuid.UUID(board["id"]), n)
    return [r["pid"] for r in rows]


async def raise_flag(pool, ref: str, n: int, kind: str, detail: dict, *, push: str | None) -> None:
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await items_db._board_by_ref(conn, ref, lock=True)
            items = await items_db._load_items(conn, board["id"], [n])
            tx = BoardTx(board, items, {"pid": "system:legacy-task-write",
                                        "name": "legacy task write"}, now)
            it = tx.item(n)
            tx.raise_flag(it, kind, detail)
            tx.dirty.add(n)
            if push:
                for h in it.holders:
                    tx._push(it, h, kind, f"{tx._prefix()} #{n} \"{it.title}\": {push}")
            await items_db._apply(conn, tx)


async def task_description_changed(pool, task: dict, description: str | None,
                                   actor: dict) -> dict | None:
    """A description PATCH on a placed task goes through its item, so the
    revision moves and holders are told (§7.2)."""
    placement = task.get("placement") or {}
    if not placement.get("board_id") or not placement.get("n"):
        return None
    return await items_db.update_items(pool, placement["board_id"], actor, [placement["n"]],
                                       {"description": description})


# ---- groups ------------------------------------------------------------------

def _may_shape_board(board: dict, actor: dict, orch_pid: str | None) -> bool:
    """Promoting a heading to an epic, setting its PoC and closing it are
    Owner's or the board team's lead's acts (§7.3). Until org S2 lands the
    board team's lead is the board's orchestrator."""
    return actor["pid"] == OWNER_PID or (orch_pid is not None and actor["pid"] == orch_pid)


async def _orchestrator_pid(conn, board: dict) -> str | None:
    row = await items_db._orchestrator_row(conn, board, items_db.utcnow())
    return (row or {}).get("pid")


def _check_parent(groups: dict, gid: str | None, parent: str | None) -> None:
    seen = {gid}
    while parent:
        if parent in seen:
            raise ItemsError(409, "cycle", "a group cannot be inside itself", group_id=gid)
        seen.add(parent)
        g = groups.get(parent)
        if g is None:
            raise invalid("invalid_group", f"no parent group {parent} on this board",
                          field="parent_group_id")
        parent = g.parent_id


async def create_group(pool, ref: str, actor: dict, *, name: str, kind: str = "heading",
                       parent_group_id: str | None = None, description: str | None = None,
                       goal: str | None = None, ord: int | None = None) -> dict:
    now = items_db.utcnow()
    name = _text(name, MAX_GROUP, "name", required=True)
    if kind not in GROUP_KINDS:
        raise invalid("invalid_field", f"kind must be one of {', '.join(GROUP_KINDS)}", field="kind")
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await items_db._board_by_ref(conn, ref, lock=True)
            groups = await items_db._load_groups(conn, board["id"])
            if kind == "epic" and not _may_shape_board(board, actor, await _orchestrator_pid(conn, board)):
                raise ItemsError(403, "forbidden",
                                 "only Owner or the board's lead creates an epic; create a heading")
            _check_parent(groups, None, parent_group_id)
            if any(g.name.lower() == name.lower() and g.parent_id == parent_group_id
                   for g in groups.values()):
                raise ItemsError(409, "group_exists", f"a group named {name!r} is already there")
            gid = uuid.uuid4()
            await conn.execute(
                """INSERT INTO groups (id, board_id, parent_group_id, name, kind, description, goal,
                                       epic_status, ord, created_by, created_at, updated_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $11)""",
                gid, uuid.UUID(board["id"]),
                uuid.UUID(parent_group_id) if parent_group_id else None, name, kind,
                _text(description, MAX_DESCRIPTION, "description"),
                _text(goal, MAX_DESCRIPTION, "goal"), "active" if kind == "epic" else None,
                ord or 0, actor["pid"], now)
            await _group_event(conn, board, actor, "group_created", None,
                               {"id": str(gid), "name": name, "kind": kind}, now)
            return (await _group_rows(conn, board, [str(gid)]))[0]


async def set_group(pool, ref: str, actor: dict, group_id: str, changes: dict, *,
                    expected_revision: int | None = None) -> dict:
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            await conn.execute(items_db.TASK_WRITE_LOCK_SQL)
            board = await items_db._board_by_ref(conn, ref, lock=True)
            groups = await items_db._load_groups(conn, board["id"])
            g = groups.get(str(group_id))
            if g is None:
                raise ItemsError(404, "not_found", f"no group {group_id} on board {board['slug']}")
            if expected_revision is not None and expected_revision != g.revision:
                raise ItemsError(409, "revision_changed",
                                 f"group {g.name!r} is at revision {g.revision}, not "
                                 f"{expected_revision}: re-read it", revision=g.revision)
            sets: dict = {}
            if "name" in changes:
                sets["name"] = _text(changes["name"], MAX_GROUP, "name", required=True)
            for key in ("description", "goal"):
                if key in changes:
                    sets[key] = _text(changes[key], MAX_DESCRIPTION, key)
            if "ord" in changes:
                sets["ord"] = int(changes["ord"] or 0)
            if "parent_group_id" in changes:
                parent = changes["parent_group_id"] or None
                _check_parent(groups, g.id, parent)
                sets["parent_group_id"] = uuid.UUID(parent) if parent else None
            if "kind" in changes and changes["kind"] != g.kind:
                if changes["kind"] not in GROUP_KINDS:
                    raise invalid("invalid_field", "kind must be heading or epic", field="kind")
                if not _may_shape_board(board, actor, await _orchestrator_pid(conn, board)):
                    raise ItemsError(403, "forbidden",
                                     "only Owner or the board's lead promotes a heading to an "
                                     "epic or demotes one")
                if changes["kind"] == "heading" and g.team_id:
                    raise ItemsError(409, "epic_has_team",
                                     "this epic has a team; close it (epic close) instead")
                sets["kind"] = changes["kind"]
                sets["epic_status"] = "active" if changes["kind"] == "epic" else None
            if not sets:
                return (await _group_rows(conn, board, [g.id]))[0]
            cols = ", ".join(f"{k} = ${i + 3}" for i, k in enumerate(sets))
            await conn.execute(
                f"UPDATE groups SET {cols}, revision = revision + 1, updated_at = $2 WHERE id = $1",
                uuid.UUID(g.id), now, *sets.values())
            if "name" in sets:
                # Old readers see the free-text name.
                await conn.execute("UPDATE items SET grp = $2 WHERE group_id = $1",
                                   uuid.UUID(g.id), sets["name"])
            await _group_event(conn, board, actor, "group_updated", {"id": g.id},
                               {k: (str(v) if isinstance(v, uuid.UUID) else v)
                                for k, v in sets.items()}, now)
            return (await _group_rows(conn, board, [g.id]))[0]


async def _group_event(conn, board, actor, type_, prev, new, now) -> None:
    await conn.execute(
        """INSERT INTO item_events (board_id, actor_pid, actor_name, type, prev, new, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7)""",
        uuid.UUID(board["id"]), actor["pid"], actor.get("name"), type_, prev, new, now)


async def _team_leads(conn, team_ids: list) -> dict:
    """team id -> lead member id, once org S2's `initiatives.lead_member_id`
    exists (absent before: no PoCs)."""
    ids = [uuid.UUID(t) for t in team_ids if t]
    if not ids:
        return {}
    has = await conn.fetchval(
        """SELECT EXISTS (SELECT 1 FROM information_schema.columns
                           WHERE table_name = 'initiatives' AND column_name = 'lead_member_id')""")
    if not has:
        return {}
    rows = await conn.fetch("SELECT id, lead_member_id FROM initiatives WHERE id = ANY($1::uuid[])",
                            ids)
    return {str(r["id"]): r["lead_member_id"] for r in rows}


async def group_rows(conn, board: dict) -> list[dict]:
    """`board.read`'s groups: each with its PoC (the epic team's lead) and
    open/closed item counts."""
    return await _group_rows(conn, board, None)


async def _group_rows(conn, board: dict, ids: list[str] | None) -> list[dict]:
    groups = await items_db._load_groups(conn, board["id"])
    leads = await _team_leads(conn, [g.team_id for g in groups.values()])
    counts = {str(r["group_id"]): r for r in await conn.fetch(
        """SELECT group_id, count(*) FILTER (WHERE closed_at IS NULL) AS open,
                  count(*) FILTER (WHERE closed_at IS NOT NULL) AS closed
             FROM items WHERE board_id = $1 AND group_id IS NOT NULL
              AND archived_at IS NULL AND link_archived_at IS NULL
            GROUP BY group_id""", uuid.UUID(board["id"]))}
    out = []
    for g in groups.values():
        if ids is not None and g.id not in ids:
            continue
        row = g.public(leads.get(g.team_id))
        c = counts.get(g.id)
        row["open_items"] = int(c["open"]) if c else 0
        row["closed_items"] = int(c["closed"]) if c else 0
        out.append(row)
    return out


async def on_boards(pool) -> dict:
    """The planning view's read-only "on boards" section (§11): open epics
    and open board-native items, across boards."""
    async with pool.acquire() as conn:
        epics = await conn.fetch(
            """SELECT g.*, b.slug FROM groups g JOIN boards b ON b.id = g.board_id
                WHERE g.kind = 'epic' AND g.epic_status = 'active'
                ORDER BY b.slug, g.ord, lower(g.name)""")
        leads = await _team_leads(conn, [str(e["team_id"]) for e in epics if e["team_id"]])
        open_counts = {str(r["group_id"]): r["n"] for r in await conn.fetch(
            """SELECT group_id, count(*) AS n FROM items
                WHERE group_id IS NOT NULL AND closed_at IS NULL AND archived_at IS NULL
                  AND link_archived_at IS NULL GROUP BY group_id""")}
        items = await conn.fetch(
            """SELECT i.id, i.number, i.title, i.status, b.slug FROM items i
                 JOIN boards b ON b.id = i.board_id
                WHERE i.task_id IS NULL AND i.closed_at IS NULL AND i.archived_at IS NULL
                ORDER BY b.slug, i.number LIMIT 500""")
        holders: dict = {}
        if items:
            for h in await conn.fetch(
                    """SELECT item_id, pid, name, role FROM item_holders
                        WHERE item_id = ANY($1::bigint[]) ORDER BY added_at""",
                    [r["id"] for r in items]):
                holders.setdefault(h["item_id"], []).append(
                    {"pid": h["pid"], "name": h["name"], "role": h["role"]})
    return {
        "epics": [{"board": e["slug"], "group_id": str(e["id"]), "name": e["name"],
                   "epic_status": e["epic_status"],
                   "poc_member_id": leads.get(str(e["team_id"])) if e["team_id"] else None,
                   "open_items": int(open_counts.get(str(e["id"]), 0))} for e in epics],
        "items": [{"board": r["slug"], "n": r["number"], "title": r["title"],
                   "status": r["status"], "holders": holders.get(r["id"], [])} for r in items],
    }


__all__ = [
    "GUARD_CAP", "MIRROR", "apply_task_side", "create_group", "daemon_launch_grants",
    "group_rows", "mirror_gate", "on_boards", "place_task", "set_group",
    "task_description_changed", "translate_task_status", "unplace_task",
]
