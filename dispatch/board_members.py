"""Board membership by an explicit join (#28, doc/items-board.md §8).

A session is on a board when its task resolves to the board, when it holds an
open item there, or when someone with authority over the board added it
(`board_members`). Joining is what lets a creator set up a session it just
spawned, and what lets `set_poc` name a PoC that has not touched the board
yet. The daemon checks the caller's authority for a plain join (it may edit
the board); `set_poc` checks its own.
"""

from __future__ import annotations

import uuid

from dispatch import items_db
from dispatch.items_rules import ItemsError, invalid


def _holder_parts(pid: str) -> tuple[str | None, str | None]:
    parts = pid.split(":", 2)
    if len(parts) == 3 and parts[0] == "agent":
        return parts[1], parts[2]
    return None, None


async def on_board(conn, board: dict, pid: str) -> bool:
    """Whether `pid` is already on `board` by any of the three routes."""
    board_id = uuid.UUID(board["id"])
    if await conn.fetchval("SELECT 1 FROM board_members WHERE board_id = $1 AND pid = $2",
                           board_id, pid):
        return True
    if await conn.fetchval(
            """SELECT 1 FROM item_holders h JOIN items i ON i.id = h.item_id
                WHERE i.board_id = $1 AND h.pid = $2 AND i.closed_at IS NULL LIMIT 1""",
            board_id, pid):
        return True
    return bool(await conn.fetchval(
        """WITH RECURSIVE tree AS (
               SELECT id FROM tasks WHERE id = $2
               UNION SELECT t.id FROM tasks t JOIN tree ON t.parent_task_id = tree.id)
           SELECT 1 FROM session_states s
            WHERE s.pid = $1 AND s.exited_at IS NULL
              AND (s.task_id IN (SELECT id FROM tree)
                   OR ($3::uuid IS NOT NULL
                       AND s.task_id IN (SELECT id FROM tasks WHERE initiative_id = $3)))""",
        pid,
        uuid.UUID(board["root_task_id"]) if board.get("root_task_id") else None,
        uuid.UUID(board["initiative_id"]) if board.get("initiative_id") else None))


async def add(conn, board: dict, pid: str, *, added_by: str, name: str | None = None,
              group_id: str | None = None, reason: str | None = None,
              added_by_member_id: str | None = None, added_by_acting_as: str | None = None) -> bool:
    """Add `pid` to `board` unless it is already on it; True when it joined
    now. A repeated join may move its group."""
    if not isinstance(pid, str) or not pid or len(pid) > 300:
        raise invalid("invalid_field", "pid must be a participant id", field="pid")
    already = await on_board(conn, board, pid)
    daemon_id, uid = _holder_parts(pid)
    gid = uuid.UUID(group_id) if group_id else None
    if gid is not None and not await conn.fetchval(
            "SELECT 1 FROM groups WHERE id = $1 AND board_id = $2", gid, uuid.UUID(board["id"])):
        raise invalid("invalid_group", f"no group {group_id} on this board", field="group_id")
    await conn.execute(
        """INSERT INTO board_members (board_id, pid, name, session_uid, daemon_id, group_id,
                                      added_by, reason, added_by_member_id, added_by_acting_as)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           ON CONFLICT (board_id, pid) DO UPDATE SET
               name = coalesce(EXCLUDED.name, board_members.name),
               group_id = coalesce(EXCLUDED.group_id, board_members.group_id)""",
        uuid.UUID(board["id"]), pid, name, uid, daemon_id, gid, added_by, reason,
        added_by_member_id, added_by_acting_as)
    return not already


async def join(pool, ref: str, actor: dict, *, pid: str, name: str | None = None,
               group_id: str | None = None, group: str | None = None,
               reason: str | None = None) -> dict:
    """`POST /boards/{ref}/members`: add a session to the board. The daemon has
    already checked that the actor may edit the board."""
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await items_db._board_by_ref(conn, ref)
            if group and not group_id:
                group_id = await conn.fetchval(
                    "SELECT id::text FROM groups WHERE board_id = $1 AND lower(name) = lower($2)",
                    uuid.UUID(board["id"]), group)
                if group_id is None:
                    raise invalid("invalid_group", f"no group {group!r} on this board", field="group")
            joined = await add(conn, board, pid, added_by=actor.get("pid") or "?", name=name,
                               group_id=group_id, reason=reason,
                               added_by_member_id=actor.get("member_id"),
                               added_by_acting_as=actor.get("acting_as"))
            return {"board": board["slug"], "pid": pid, "joined": joined, "group_id": group_id}


async def remove(pool, ref: str, actor: dict, *, pid: str, editor: bool = False,
                 reason: str | None = None) -> dict:
    """`POST /boards/{ref}/members/remove`: end an explicit join. The session
    itself may leave; Owner, the board's orchestrator or steward may remove
    anyone; whoever added it may remove it while it can still edit the board
    (`editor`, checked by the daemon). A session on the board through its
    task or a hold stays on it that way."""
    from dispatch import placement
    from dispatch.items_rules import OWNER_PID

    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await items_db._board_by_ref(conn, ref)
            row = await conn.fetchrow(
                "SELECT added_by FROM board_members WHERE board_id = $1 AND pid = $2 FOR UPDATE",
                uuid.UUID(board["id"]), pid)
            if row is None:
                return {"board": board["slug"], "pid": pid, "removed": False}
            who = actor.get("pid")
            orch = await placement._orchestrator_pid(conn, board)
            allowed = (who == pid or who == OWNER_PID or (orch and who == orch)
                       or (board.get("steward_pid") and who == board["steward_pid"])
                       or (editor and who == row["added_by"]))
            if not allowed:
                raise ItemsError(403, "forbidden",
                                 "only the session itself, Owner, the board's orchestrator or "
                                 "steward, or whoever added it (while it can edit the board) may "
                                 "remove a board member")
            await conn.execute("DELETE FROM board_members WHERE board_id = $1 AND pid = $2",
                               uuid.UUID(board["id"]), pid)
            return {"board": board["slug"], "pid": pid, "removed": True, "reason": reason}


# A session that has been gone this long leaves every board it joined; a
# revive or restart in between clears `exited_at`, so a rotation keeps it.
EXIT_GRACE_S = 1800


async def end_exited(conn, daemon_id: str | None = None) -> int:
    """End the joins of sessions that exited for good (gone EXIT_GRACE_S),
    optionally only for one daemon's sessions. Returns how many ended."""
    result = await conn.execute(
        """DELETE FROM board_members bm USING session_states s
            WHERE s.pid = bm.pid AND s.exited_at IS NOT NULL
              AND s.exited_at < now() - make_interval(secs => $1)
              AND ($2::text IS NULL OR s.daemon_id = $2)""", EXIT_GRACE_S, daemon_id)
    return int(result.split()[-1]) if result else 0


async def members(pool, ref: str, *, pid: str | None = None) -> dict:
    """`GET /boards/{ref}/members`: the explicit joins (optionally one pid)."""
    async with pool.acquire() as conn:
        board = await items_db._board_by_ref(conn, ref)
        rows = await conn.fetch(
            """SELECT pid, name, session_uid, daemon_id, group_id::text AS group_id, added_by,
                      added_by_member_id, added_by_acting_as,
                      reason, added_at
                 FROM board_members WHERE board_id = $1 AND ($2::text IS NULL OR pid = $2)
                ORDER BY added_at, pid""", uuid.UUID(board["id"]), pid)
        out = [{**dict(r), "added_at": items_db.iso(r["added_at"])} for r in rows]
        return {"board": board["slug"], "members": out,
                "on_board": (await on_board(conn, board, pid)) if pid else None}


__all__ = ["EXIT_GRACE_S", "ItemsError", "add", "end_exited", "join", "members", "on_board", "remove"]
