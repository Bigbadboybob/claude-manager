"""Board flag engine (doc/items-board.md §4, §5).

Every 30 s, for each board with live items: compute each item's flags from
its holders' heartbeat states, raise new flags and clear ones whose condition
stopped holding (under the board-row lock, through the same `BoardTx` as the
write path, so history and the board version stay complete), then push the
orchestrator at most once per tick and escalate to Owner when flags sit
unresolved while the orchestrator is idle or gone.

`compute_flags` is pure, for tests.
"""

from __future__ import annotations

import asyncio
import hashlib
import logging
import uuid
from datetime import datetime, timedelta

from dispatch import items_db
from dispatch.items_rules import CLOSED, BoardTx, Item, iso

logger = logging.getLogger("cm.board_engine")

ENGINE_ACTOR = {"pid": "system:board-engine", "name": "board engine"}
STATE_FRESH_S = items_db.STATE_FRESH_S
OVERDUE_GRACE = 0.25
PUSH_TEXT_LIMIT = 1000
DIGEST_SHOWN = 10
RETENTION = timedelta(days=7)
ARCHIVE_AFTER = timedelta(hours=24)
# Flags the engine owns: it raises and clears them. (blocker_dropped is also
# raised by the write path; the engine keeps it consistent.)
ENGINE_KINDS = frozenset({
    "unassigned", "holder_gone", "holder_idle", "holder_waiting_on_human",
    "holder_errored", "stale", "overdue", "check_back", "blocker_dropped",
})


def _seconds(delta: timedelta) -> float:
    return delta.total_seconds()


# A holder only counts as gone after this long, so one late or partial beat
# (a slow brain start, a session spawned between beats) never raises and
# clears holder_gone in quick succession.
GONE_GRACE_S = 90


def holder_status(row: dict | None, now: datetime, live_daemons: set[str], pid: str,
                  added_at: datetime | None = None) -> str:
    """One holder's state for flag purposes: a SESSION_STATE.md word, or
    `gone` (exited for GONE_GRACE_S, or missing for that long from a daemon
    that is heartbeating)."""
    if pid == "owner" or not pid.startswith("agent:"):
        return "unknown"
    if row is None:
        daemon = pid.split(":", 2)[1] if pid.count(":") >= 2 else ""
        settled = added_at is None or _seconds(now - added_at) >= GONE_GRACE_S
        return "gone" if daemon in live_daemons and settled else "unknown"
    if row.get("exited_at") is not None or row.get("state") == "exited":
        exited = row.get("exited_at") or row.get("reported_at")
        if exited is not None and _seconds(now - exited) < GONE_GRACE_S:
            return "unknown"
        return "gone"
    if _seconds(now - row["reported_at"]) > STATE_FRESH_S:
        return "unknown"
    return row.get("state") or "unknown"


def _names(holders, pred) -> list[str]:
    return sorted(h.get("name") or h["pid"] for h in holders if pred(h))


def compute_flags(item: Item, states: dict, items: dict[int, Item], board: dict,
                  now: datetime, live_daemons: set[str]) -> dict[str, dict | None]:
    """The flags that hold for `item` right now, kind -> stable detail."""
    if item.status in CLOSED or item.archived_at is not None:
        return {}
    flags: dict[str, dict | None] = {}
    status = {h["pid"]: holder_status(states.get(h["pid"]), now, live_daemons, h["pid"],
                                      h.get("added_at"))
              for h in item.holders}
    # An `open` item that still has holders is being held: it gets the same
    # holder and staleness flags as `active`, so nothing blocked behind it
    # can hide.
    held_open = item.status == "open" and bool(item.holders)
    working = item.status in ("active", "waiting", "blocked") or held_open
    touched = max(t for t in (item.touched_at, item.clock_reset_at) if t is not None)

    if item.status == "open" and not item.holders:
        if _seconds(now - touched) >= board["unassigned_s"]:
            flags["unassigned"] = None

    if working:
        gone = _names(item.holders, lambda h: status[h["pid"]] == "gone")
        if gone:
            flags["holder_gone"] = {"holders": gone}

    for state, kind in (("waiting-on-human", "holder_waiting_on_human"),
                        ("errored", "holder_errored")):
        who = _names(item.holders, lambda h, s=state: status[h["pid"]] == s)
        if who:
            flags[kind] = {"holders": who}

    blockers = [items.get(b) for b in sorted(item.blocked_by)]
    if any(b is not None and b.status == "dropped" for b in blockers):
        flags["blocker_dropped"] = {
            "blockers": sorted(b.n for b in blockers if b is not None and b.status == "dropped")}
    # Waiting only on open items is legitimate waiting: no idle/stale clock.
    exempt = (item.status == "blocked" and bool(item.blocked_by) and not item.blocked_on
              and all(b is not None and b.status not in CLOSED for b in blockers))

    if (board.get("holder_idle_enabled") and (item.status == "active" or held_open) and item.holders
            and all(status[h["pid"]] == "idle" for h in item.holders)):
        since = []
        for h in item.holders:
            row = states.get(h["pid"]) or {}
            since.append(row.get("idle_since") or row.get("state_since") or now)
        # Every holder must have been idle the whole threshold: the clock
        # starts at the latest holder's idle start (or a touch/unblock).
        idle_from = max([max(since), item.touched_at, item.clock_reset_at or item.touched_at])
        if _seconds(now - idle_from) >= board["idle_s"]:
            flags["holder_idle"] = {"holders": _names(item.holders, lambda h: True)}

    if not exempt:
        threshold = board["stale_s"]
        if item.status == "waiting" and item.eta_at and item.waiting_set_at:
            threshold = max(threshold, _seconds(item.eta_at - item.waiting_set_at))
        if working and _seconds(now - touched) >= threshold:
            flags["stale"] = None

    if item.status == "waiting" and item.eta_at and item.waiting_set_at:
        grace = OVERDUE_GRACE * _seconds(item.eta_at - item.waiting_set_at)
        if now > item.eta_at + timedelta(seconds=grace):
            flags["overdue"] = {"eta_at": iso(item.eta_at)}

    if item.blocked_on and item.check_back_at and now >= item.check_back_at:
        flags["check_back"] = {"check_back_at": iso(item.check_back_at)}
    return flags


def _short(seconds: float) -> str:
    s = max(0, int(seconds))
    if s < 90:
        return f"{s}s"
    if s < 5400:
        return f"{(s + 30) // 60}m"
    if s < 172800:
        h, m = divmod(s // 60, 60)
        return f"{h}h{m}m" if m else f"{h}h"
    return f"{s // 86400}d"


def describe(flag: dict, item: Item | None, states: dict, now: datetime) -> str:
    """`#14 holder_idle (rl-scale-out idle 24m)` for push texts."""
    kind, n, detail = flag["kind"], flag["n"], flag.get("detail") or {}
    why = ""
    if kind == "holder_idle" and item is not None:
        parts = []
        for h in item.holders:
            row = states.get(h["pid"]) or {}
            since = row.get("idle_since") or row.get("state_since")
            parts.append(f"{h.get('name') or h['pid']} idle"
                         + (f" {_short(_seconds(now - since))}" if since else ""))
        why = ", ".join(parts)
    elif kind in ("holder_gone", "holder_waiting_on_human", "holder_errored"):
        word = {"holder_gone": "gone", "holder_waiting_on_human": "waiting on a human",
                "holder_errored": "errored"}[kind]
        why = f"{', '.join(detail.get('holders', []))} {word}"
    elif kind == "stale" and item is not None:
        touched = max(t for t in (item.touched_at, item.clock_reset_at) if t is not None)
        why = f"untouched {_short(_seconds(now - touched))}"
    elif kind == "overdue" and item is not None and item.eta_at:
        why = f"eta passed {_short(_seconds(now - item.eta_at))} ago"
    elif kind == "unassigned" and item is not None:
        # Measured like the flag: since the item was last touched (made open
        # or handed back), not since the flag was raised.
        touched = max(t for t in (item.touched_at, item.clock_reset_at) if t is not None)
        why = f"no holder {_short(_seconds(now - touched))}"
    elif kind == "check_back" and item is not None:
        why = f"check back on \"{item.blocked_on}\""
    elif kind == "blocker_dropped":
        why = "blocker " + ", ".join(f"#{b}" for b in detail.get("blockers", [])) + " dropped"
    return f"#{n} {kind}" + (f" ({why})" if why else "")


def compose(slug: str, flag_parts: list[str], closed: dict[str, list[int]],
            limit: int = PUSH_TEXT_LIMIT) -> str:
    """`[cm-board <slug>] 2 flags: …; … · 3 done (#9,#11). board() / item_resolve(n, action)`."""
    head = f"[cm-board {slug}]"
    tail = f" board(board=\"{slug}\") / item_resolve(n, action)"
    sections = []
    if flag_parts:
        sections.append(f"{len(flag_parts)} flag{'s' if len(flag_parts) != 1 else ''}: "
                        + "; ".join(flag_parts))
    for word in ("done", "dropped", "blocked"):
        ns = closed.get(word) or []
        if ns:
            shown = ",".join(f"#{n}" for n in ns[:DIGEST_SHOWN])
            more = f",+{len(ns) - DIGEST_SHOWN}" if len(ns) > DIGEST_SHOWN else ""
            sections.append(f"{len(ns)} {word} ({shown}{more})")
    text = f"{head} {' · '.join(sections)}.{tail}"
    if len(text) <= limit:
        return text
    # Keep whole flag entries; say how many were left out.
    kept = list(flag_parts)
    while kept and len(text) > limit:
        kept.pop()
        rest = len(flag_parts) - len(kept)
        sections[0] = (f"{len(flag_parts)} flags: " + "; ".join(kept)
                       + f"; +{rest} more")
        text = f"{head} {' · '.join(sections)}.{tail}"
    return text[:limit]


# ---- one board, one tick ---------------------------------------------------

async def _states(conn, pids: list[str]) -> dict:
    rows = await conn.fetch("SELECT * FROM session_states WHERE pid = ANY($1::text[])", pids)
    return {r["pid"]: dict(r) for r in rows}


async def _live_daemons(conn, now: datetime) -> set[str]:
    rows = await conn.fetch(
        "SELECT DISTINCT daemon_id FROM session_states WHERE reported_at > $1",
        now - timedelta(seconds=STATE_FRESH_S),
    )
    return {r["daemon_id"] for r in rows}


async def _snoozed(conn, board_id: str, now: datetime) -> set[tuple[int, str]]:
    rows = await conn.fetch(
        """SELECT i.number, f.kind FROM item_flags f JOIN items i ON i.id = f.item_id
            WHERE i.board_id = $1 AND f.resolved_at IS NOT NULL AND f.snooze_until > $2""",
        uuid.UUID(board_id), now,
    )
    return {(r["number"], r["kind"]) for r in rows}


async def _orchestrator_row(conn, board: dict, now: datetime) -> dict | None:
    """The orchestrator's state row: explicit pid, else the best live agent
    session bound to the coordinator (initiative) or root task
    (`items_db.ORCHESTRATOR_CANDIDATES_SQL`)."""
    if board.get("orchestrator_pid"):
        row = await conn.fetchrow("SELECT * FROM session_states WHERE pid = $1",
                                  board["orchestrator_pid"])
        return dict(row) if row else {"pid": board["orchestrator_pid"]}
    anchor = await _anchor_task(conn, board)
    if anchor is None:
        return None
    row = await conn.fetchrow(
        items_db.ORCHESTRATOR_CANDIDATES_SQL,
        anchor, now - timedelta(seconds=STATE_FRESH_S),
    )
    return dict(row) if row else None


async def _anchor_task(conn, board: dict):
    if board.get("initiative_id"):
        return await conn.fetchval(
            "SELECT coordinator_task_id FROM initiatives WHERE id = $1",
            uuid.UUID(board["initiative_id"]))
    if board.get("root_task_id"):
        return uuid.UUID(board["root_task_id"])
    return None


async def _escalation_target(conn, board: dict, orch: dict | None) -> dict | None:
    """Where an Owner alert is raised: the orchestrator's daemon and row, else
    the coordinator's most recent non-bash session on any daemon."""
    if orch and orch.get("daemon_id") and orch.get("session_uid"):
        return orch
    anchor = await _anchor_task(conn, board)
    if anchor is None:
        return None
    row = await conn.fetchrow(
        """SELECT * FROM session_states
            WHERE task_id = $1 AND COALESCE(engine, '') <> 'bash'
            ORDER BY reported_at DESC LIMIT 1""",
        anchor)
    return dict(row) if row else None


def _dedupe(board_id: str, kind: str, text: str, now: datetime) -> str:
    digest = hashlib.sha256(text.encode()).hexdigest()[:16]
    return f"board:{board_id}:{kind}:{now.strftime('%Y%m%d%H%M')}:{digest}"


async def _push(conn, board_id: str, target: dict, kind: str, text: str, now: datetime,
                owner_alert: bool = False) -> None:
    await conn.execute(
        """INSERT INTO item_pushes (board_id, daemon_id, session_uid, pid, kind, text, dedupe,
                                    owner_alert, created_at)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT (dedupe) DO NOTHING""",
        uuid.UUID(board_id), target["daemon_id"], target["session_uid"], target["pid"], kind,
        text, _dedupe(board_id, kind, text, now), owner_alert, now,
    )


async def tick_board(pool, board_id: str, now: datetime | None = None) -> dict:
    """Run one engine pass over a board. Returns a summary for logs/tests."""
    now = now or items_db.utcnow()
    summary = {"raised": [], "cleared": [], "pushed": False, "escalated": False}
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await items_db._board_by_ref(conn, board_id, lock=True)
            items = await items_db._load_items(conn, board["id"])
            pids = sorted({h["pid"] for it in items.values() for h in it.holders})
            states = await _states(conn, pids)
            live = await _live_daemons(conn, now)
            snoozed = await _snoozed(conn, board["id"], now)
            tx = BoardTx(board, items, ENGINE_ACTOR, now)

            for item in sorted(items.values(), key=lambda i: i.n):
                if item.status in CLOSED or item.archived_at is not None:
                    continue
                want = compute_flags(item, states, items, board, now, live)
                for kind, detail in want.items():
                    if kind in item.open_flags:
                        tx.raise_flag(item, kind, detail)  # updates a changed detail
                        continue
                    if (item.n, kind) in snoozed:
                        continue
                    tx.raise_flag(item, kind, detail)
                    tx.dirty.add(item.n)
                    summary["raised"].append((item.n, kind))
                    if kind == "overdue":
                        for h in item.holders:
                            tx._push(item, h, "overdue",
                                     f"[cm-board {board['slug']}] #{item.n} \"{item.title}\" is "
                                     f"past its eta: update it (new eta, done, or hand back).")
                for kind in sorted(set(item.open_flags) - set(want)):
                    if kind in ENGINE_KINDS and tx.resolve_flag(item, kind, "cleared"):
                        summary["cleared"].append((item.n, kind))
            await items_db._apply(conn, tx)

            orch = await _orchestrator_row(conn, board, now)
            await _push_orchestrator(conn, board, items, states, orch, now, summary)
            await _escalate(conn, board, orch, now, summary)
    return summary


async def _open_flags(conn, board_id: str) -> list[dict]:
    rows = await conn.fetch(
        """SELECT f.id, f.kind, f.detail, f.raised_at, f.last_pushed_at, f.push_count,
                  f.escalated_at, i.number AS n
             FROM item_flags f JOIN items i ON i.id = f.item_id
            WHERE i.board_id = $1 AND f.resolved_at IS NULL
            ORDER BY f.raised_at, i.number""",
        uuid.UUID(board_id),
    )
    return [dict(r) for r in rows]


async def _push_orchestrator(conn, board, items, states, orch, now, summary) -> None:
    flags = await _open_flags(conn, board["id"])
    repush = timedelta(seconds=board["repush_s"])
    # The holder hears about an overdue item first.
    eligible = [f for f in flags
                if not (f["kind"] == "overdue" and now - f["raised_at"] < repush)]
    # One wake carries every open flag: when any is new or due, all go, and
    # their re-push clocks align so k flags never mean k wakes per window.
    trigger = any(f["last_pushed_at"] is None or now - f["last_pushed_at"] >= repush
                  for f in eligible)
    due = eligible if trigger else []
    closed: dict[str, list[int]] = {}
    digest_due = (board.get("last_digest_at") is None
                  or now - board["last_digest_at"] >= timedelta(seconds=board["digest_s"]))
    if digest_due:
        since = board.get("last_digest_at") or now - timedelta(seconds=board["digest_s"])
        rows = await conn.fetch(
            """SELECT DISTINCT item_number, new->>'status' AS status FROM item_events
                WHERE board_id = $1 AND created_at > $2 AND type IN ('updated', 'created')
                  AND new->>'status' IN ('done', 'dropped', 'blocked')
                  AND actor_pid <> ALL($3::text[])
                ORDER BY item_number""",
            uuid.UUID(board["id"]), since,
            [ENGINE_ACTOR["pid"], (orch or {}).get("pid") or ""],
        )
        for r in rows:
            closed.setdefault(r["status"], []).append(r["item_number"])
    if not orch or not orch.get("daemon_id") or not orch.get("session_uid"):
        return
    if digest_due:
        # The window advances even when nothing closed, so the next batch
        # waits a full digest_s.
        await conn.execute("UPDATE boards SET last_digest_at = $2 WHERE id = $1",
                           uuid.UUID(board["id"]), now)
    if not due and not closed:
        return
    parts = [describe(f, items.get(f["n"]), states, now) for f in due]
    text = compose(board["slug"], parts, closed)
    await _push(conn, board["id"], orch, "board", text, now)
    if due:
        await conn.execute(
            """UPDATE item_flags SET last_pushed_at = $2, push_count = push_count + 1
                WHERE id = ANY($1::bigint[])""",
            [f["id"] for f in due], now,
        )
    summary["pushed"] = True


# Boards whose escalation had no session to carry it (logged once).
_UNROUTABLE: set[str] = set()


async def _escalate(conn, board, orch, now, summary) -> None:
    flags = await _open_flags(conn, board["id"])
    threshold = timedelta(seconds=board["escalate_s"])
    stuck = [f for f in flags if f["escalated_at"] is None and now - f["raised_at"] >= threshold]
    if not stuck:
        return
    orch_state = "none"
    if orch:
        orch_state = holder_status(orch if "reported_at" in orch else None, now,
                                   await _live_daemons(conn, now), orch["pid"])
    if orch_state not in ("idle", "unknown", "gone", "none", "errored", "waiting-on-human"):
        return
    target = await _escalation_target(conn, board, orch if orch_state != "none" else None)
    if target is None or not target.get("daemon_id") or not target.get("session_uid"):
        if board["id"] not in _UNROUTABLE:
            logger.warning("board %s: %d flags need Owner but no session can carry the alert",
                           board["slug"], len(stuck))
            _UNROUTABLE.add(board["id"])
        return
    _UNROUTABLE.discard(board["id"])
    who = (orch or {}).get("name") or (orch or {}).get("pid")
    situation = f"while {who} is {orch_state}" if orch_state != "none" else "with no orchestrator"
    oldest = max(_seconds(now - f["raised_at"]) for f in stuck)
    listed = "; ".join(f"#{f['n']} {f['kind']}" for f in stuck[:8])
    if len(stuck) > 8:
        listed += f"; +{len(stuck) - 8} more"
    text = (f"[cm-board {board['slug']}] {len(stuck)} flag{'s' if len(stuck) != 1 else ''} "
            f"unresolved for up to {_short(oldest)} {situation}: {listed}. "
            f"board(board=\"{board['slug']}\")")
    await _push(conn, board["id"], target, "escalation", text[:PUSH_TEXT_LIMIT], now,
                owner_alert=True)
    await conn.execute("UPDATE item_flags SET escalated_at = $2 WHERE id = ANY($1::bigint[])",
                       [f["id"] for f in stuck], now)
    summary["escalated"] = True


# ---- close-out and the loop -------------------------------------------------

async def close_out(pool, now: datetime | None = None) -> dict:
    """Archive items closed for 24 h; prune old pushes, exited states and
    create request keys. Plain runtime UPDATEs/DELETEs: no events."""
    now = now or items_db.utcnow()
    archived = 0
    async with pool.acquire() as conn:
        boards = await conn.fetch(
            """SELECT DISTINCT board_id FROM items
                WHERE archived_at IS NULL AND closed_at IS NOT NULL AND closed_at < $1""",
            now - ARCHIVE_AFTER)
        for b in boards:
            # Under the board lock, so a concurrent reopen cannot interleave.
            async with conn.transaction():
                await conn.execute("SELECT 1 FROM boards WHERE id = $1 FOR UPDATE", b["board_id"])
                result = await conn.execute(
                    """UPDATE items SET archived_at = $2
                        WHERE board_id = $1 AND archived_at IS NULL
                          AND closed_at IS NOT NULL AND closed_at < $3""",
                    b["board_id"], now, now - ARCHIVE_AFTER)
                archived += int(result.split()[-1])
        pushes = await conn.execute(
            "DELETE FROM item_pushes WHERE delivered_at IS NOT NULL AND delivered_at < $1",
            now - RETENTION)
        states = await conn.execute(
            "DELETE FROM session_states WHERE exited_at IS NOT NULL AND exited_at < $1",
            now - RETENTION)
        requests = await conn.execute(
            "DELETE FROM item_requests WHERE created_at < $1", now - RETENTION)
    return {"archived": archived, "pushes": pushes, "states": states, "requests": requests}


async def active_boards(pool) -> list[str]:
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT b.id FROM boards b
                WHERE EXISTS (SELECT 1 FROM items i
                               WHERE i.board_id = b.id AND i.archived_at IS NULL)""")
    return [str(r["id"]) for r in rows]


async def board_loop(pool, interval: float = 30.0, close_out_every: int = 20) -> None:
    """Lifespan task: one engine pass per board every `interval` seconds."""
    tick = 0
    while True:
        try:
            for board_id in await active_boards(pool):
                try:
                    await tick_board(pool, board_id)
                except Exception:
                    logger.exception("board engine: board %s failed", board_id)
            if tick % close_out_every == 0:
                await close_out(pool)
        except asyncio.CancelledError:
            raise
        except Exception:
            logger.exception("board engine pass failed")
        tick += 1
        await asyncio.sleep(interval)
