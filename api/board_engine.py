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
import json
import logging
import uuid
from datetime import datetime, timedelta

from dispatch import items_db
from dispatch.gate_rules import GATE_FLAG_KINDS, HOLDER_ONLY_FLAGS as GATE_HOLDER_ONLY
from dispatch.items_rules import SUGGESTED

# Flags only the item's holders (and steward) hear about, never the
# orchestrator or Owner: a stale receipt to rerun, a suggestion to confirm.
HOLDER_ONLY_FLAGS = GATE_HOLDER_ONLY | {SUGGESTED}
from dispatch.items_rules import CLOSED, OWNER_BLOCKED, BoardTx, Item, iso
from dispatch.items_rules import strict, strict_violation

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
    "holder_errored", "stale", "stale_background", "overdue", "check_back",
    "blocker_dropped", "eta_passed",
    # Strict boards only (doc/strict-boards.md).
    "needs_triage", "unlaunched", "holder_overloaded",
})
# Warnings for the orchestrator's sweep, never an Owner page: a migration
# backlog (needs_triage) or a holder with too many items (holder_overloaded).
# unlaunched is the orchestrator's to launch or reassign; Owner hears only what
# an orchestrator escalates (norm, 2026-10-23).
NO_ESCALATE = frozenset({"needs_triage", "holder_overloaded", "unlaunched"})
# An active item whose ETA passed with no activity since becomes stale this
# long after the ETA (Board-Steward, 2026-10-20: a fixed grace, not a formula).
ETA_STALE_GRACE_S = 1800
# Kinds the holder hears about first; the orchestrator only after repush_s.
HOLDER_FIRST = frozenset({"overdue", "eta_passed"})
# Each raise of these is a new episode (another permission prompt, another
# error, another exit or drop), news even when the item looks the same.
EPISODIC = frozenset({"holder_errored", "holder_waiting_on_human", "holder_gone",
                      "blocker_dropped"})


def flag_fingerprint(item: Item, kind: str, detail) -> str:
    """The non-time part of the item state a flag is about: its status,
    holders and the flag's own detail. Together with "no activity since the
    last push" (`_news`) it decides whether a flag is news. Any member's
    write touches the item, so every status, ETA, blocker or note change also
    counts as activity."""
    state = {
        "kind": kind,
        "status": item.status,
        "holders": sorted(h["pid"] for h in item.holders),
        "detail": detail,
    }
    return hashlib.sha256(json.dumps(state, sort_keys=True, default=str).encode()).hexdigest()[:16]


def _news(item: Item | None, key: str, kind: str, detail) -> bool:
    """A flag reaches an audience again only when the item changed since it
    last did: a different status, holders or detail, or any activity since.
    Never on a timer, and never just because the flag cleared and came back
    (2026-10-20: EP saw the same seven stale_background flags pushed over and
    over). `key` names the audience: `kind` (orchestrator), `kind:holder`,
    `kind:escalated` (Owner)."""
    if item is None:
        return False
    mark = item.flag_pushed.get(key)
    if not isinstance(mark, dict) or mark.get("fp") != flag_fingerprint(item, kind, detail):
        return True
    episode = max(t for t in (item.touched_at, item.clock_reset_at) if t is not None)
    try:
        at = datetime.fromisoformat(mark["at"])
    except (KeyError, TypeError, ValueError):
        return True
    return episode > at


def _mark(item: Item, key: str, kind: str, detail, now: datetime) -> dict:
    mark = {"fp": flag_fingerprint(item, kind, detail), "at": iso(now)}
    item.flag_pushed[key] = mark
    return mark


def background_busy(row: dict | None, now: datetime) -> bool:
    """A holder whose engine reports background work (W1b agent_state):
    `working-background`, or a live background job that will wake the agent
    (a subagent, monitor or shell it waits on). Stale rows never count."""
    if row is None or row.get("exited_at") is not None:
        return False
    if _seconds(now - row["reported_at"]) > STATE_FRESH_S:
        return False
    if row.get("state") == "working-background":
        return True
    jobs = ((row.get("agent_state") or {}).get("background") or {}).get("jobs") or []
    return any(isinstance(j, dict) and j.get("wakes_agent") for j in jobs)


def stale_step(item: Item, board: dict, now: datetime) -> str:
    """Nudge before flag: what to do now that `stale` holds for `item`.
    'nudge' (tell the holder, start the grace), 'wait' (grace running) or
    'flag'. A touch after the nudge starts a new episode."""
    episode = max(t for t in (item.touched_at, item.clock_reset_at) if t is not None)
    nudged = item.stale_nudged_at
    if nudged is None or nudged < episode:
        return "nudge"
    grace = board.get("nudge_grace_s")
    grace = 1800 if grace is None else grace
    return "flag" if _seconds(now - nudged) >= grace else "wait"


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
    working = item.status in ("active", "waiting", "blocked", OWNER_BLOCKED) or held_open
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
    # Waiting on Owner is never the holder's stall: no idle or stale clock
    # (holder_gone still applies).
    exempt = exempt or item.status == OWNER_BLOCKED

    busy = any(background_busy(states.get(h["pid"]), now) for h in item.holders)

    # An active item with an ETA (§5): inside it, no idle, stale or
    # check-back clock. Errored, waiting-on-human and blocker flags always
    # apply, and a holder that is gone or errored ends the window at once.
    # At the ETA, with no activity since, `eta_passed` (once, anchored on the
    # ETA); ETA_STALE_GRACE_S later, stale. New activity or a new ETA clears
    # both.
    # The quiet window is capped at stale_background_s from when the ETA
    # was set, so eta="30d" cannot silence an item for weeks.
    eta_window = eta_overrun = False
    if item.status == "active" and item.eta_at is not None:
        cap = board.get("stale_background_s")
        cap = 21600 if cap is None else cap
        eta = min(item.eta_at, (item.waiting_set_at or touched) + timedelta(seconds=cap))
        trouble = any(status[h["pid"]] in ("gone", "errored") for h in item.holders)
        if now < eta:
            eta_window = not trouble
        elif touched <= eta:
            flags["eta_passed"] = {"eta_at": iso(eta)}
            eta_overrun = True
            if _seconds(now - eta) >= ETA_STALE_GRACE_S:
                flags["stale"] = {"eta_at": iso(eta)}
    if (not eta_window and not eta_overrun
            and board.get("holder_idle_enabled") and (item.status == "active" or held_open) and item.holders
            and not busy and all(status[h["pid"]] == "idle" for h in item.holders)):
        since = []
        for h in item.holders:
            row = states.get(h["pid"]) or {}
            since.append(row.get("idle_since") or row.get("state_since") or now)
        # Every holder must have been idle the whole threshold: the clock
        # starts at the latest holder's idle start (or a touch/unblock).
        idle_from = max([max(since), item.touched_at, item.clock_reset_at or item.touched_at])
        if _seconds(now - idle_from) >= board["idle_s"]:
            flags["holder_idle"] = {"holders": _names(item.holders, lambda h: True)}

    # A parked item (free-text block with a check-back time) is not stale
    # before its check-back; the check_back flag takes over after it.
    parked = bool(item.blocked_on and item.check_back_at and now < item.check_back_at)
    if not exempt and not parked and working and not eta_window and not eta_overrun:
        threshold = board["stale_s"]
        if item.status == "waiting" and item.eta_at and item.waiting_set_at:
            threshold = max(threshold, _seconds(item.eta_at - item.waiting_set_at))
        age = _seconds(now - touched)
        if busy:
            # Busy in background work: not stale, only the softer flag after
            # a long cap without any touch.
            cap = board.get("stale_background_s")
            if age >= (21600 if cap is None else cap):
                flags["stale_background"] = None
        elif age >= threshold:
            flags["stale"] = None

    if item.status == "waiting" and item.eta_at and item.waiting_set_at:
        grace = OVERDUE_GRACE * _seconds(item.eta_at - item.waiting_set_at)
        if now > item.eta_at + timedelta(seconds=grace):
            flags["overdue"] = {"eta_at": iso(item.eta_at)}

    if (item.blocked_on and item.check_back_at and now >= item.check_back_at
            and not eta_window):
        flags["check_back"] = {"check_back_at": iso(item.check_back_at)}

    if strict(board):
        flags.update(strict_flags(item, status, items, board, now, "holder_gone" in flags))
    return flags


def in_eta(item: Item, now: datetime) -> bool:
    """Inside a declared job's ETA: the holder need not be on it right now."""
    return item.status == "active" and item.eta_at is not None and now < item.eta_at


def strict_flags(item: Item, status: dict, items: dict[int, Item], board: dict,
                 now: datetime, gone: bool) -> dict[str, dict]:
    """needs_triage, unlaunched and holder_overloaded (doc/strict-boards.md
    §2.5, §3.2, §4.1). `status` is each holder's flag-purpose state."""
    flags: dict[str, dict] = {}
    reason = strict_violation(item, board, now)
    if reason:
        flags["needs_triage"] = {"reason": reason}
    if item.status != "active":
        return flags

    # Unlaunched: actionable for launch_s and nobody live is on it. A holder
    # is on it when it wrote or posted about the item itself since it became
    # actionable (launched_at, so its own claim or job declaration counts
    # and an orchestrator's note does not) or is not sitting idle (unknown
    # counts as live: it is never proof of absence). A declared job inside
    # its ETA is being worked. Gone holders are holder_gone's, the louder
    # flag; a handback then starts this clock. A passed check_back never
    # makes an item actionable.
    anchor = item.actionable_at or max(
        (t for t in (item.created_at, item.clock_reset_at, item.touched_at) if t is not None),
        default=now)
    on_it = bool(item.holders) and (
        (item.launched_at is not None and item.launched_at >= anchor)
        or any(status[h["pid"]] != "idle" for h in item.holders))
    launch_s = board.get("launch_s") or 900
    if not gone and not on_it and not in_eta(item, now) and _seconds(now - anchor) >= launch_s:
        flags["unlaunched"] = {"actionable_at": iso(anchor)}

    # Overloaded: a holder with more active items than the board allows,
    # not counting items inside a job's ETA. Its most recently touched items
    # are its focus; the rest carry the warning. The orchestrator and the
    # steward hold coordination items and are exempt.
    if not in_eta(item, now):
        limit = board.get("max_active_per_holder") or 2
        exempt = {board.get("orchestrator_pid"), board.get("steward_pid"),
                  board.get("resolved_orchestrator_pid")}
        over = []
        for h in item.holders:
            if h["pid"] in exempt:
                continue
            counted = sorted(
                (o for o in items.values()
                 if o.status == "active" and o.archived_at is None and not in_eta(o, now)
                 and any(x["pid"] == h["pid"] for x in o.holders)),
                key=lambda o: (o.touched_at or now, o.n), reverse=True)
            if len(counted) > limit and item.n not in {o.n for o in counted[:limit]}:
                over.append(h.get("name") or h["pid"])
        if over:
            flags["holder_overloaded"] = {"holders": sorted(over)}
    return flags


def _due(flag: dict, item: Item | None, suffix: str = "") -> bool:
    if _news(item, flag["kind"] + suffix, flag["kind"], flag.get("detail")):
        return True
    if flag["kind"] not in EPISODIC or item is None:
        return False
    # Raised again after it last reached this audience: a new episode.
    mark = item.flag_pushed.get(flag["kind"] + suffix) or {}
    try:
        return flag["raised_at"] > datetime.fromisoformat(mark["at"])
    except (KeyError, TypeError, ValueError):
        return True


async def _mark_pushed(conn, items: dict, flags: list[dict], now: datetime,
                       suffix: str = "") -> None:
    by_item: dict[int, dict] = {}
    for f in flags:
        item = items.get(f["n"])
        if item is None:
            continue
        key = f["kind"] + suffix
        by_item.setdefault(item.id, {})[key] = _mark(item, key, f["kind"], f.get("detail"), now)
    for item_id, marks in by_item.items():
        await conn.execute("UPDATE items SET flag_pushed = flag_pushed || $2::jsonb WHERE id = $1",
                           item_id, marks)


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


def describe(flag: dict, item: Item | None, states: dict, now: datetime,
             free: list[str] | None = None) -> str:
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
    elif kind in ("stale", "stale_background") and item is not None:
        touched = max(t for t in (item.touched_at, item.clock_reset_at) if t is not None)
        why = f"untouched {_short(_seconds(now - touched))}"
        if kind == "stale" and detail.get("eta_at") and item.eta_at:
            why += f", eta passed {_short(_seconds(now - item.eta_at))} ago"
        if kind == "stale_background":
            why += ", holder busy in background work"
    elif kind in ("overdue", "eta_passed") and item is not None and item.eta_at:
        why = f"eta passed {_short(_seconds(now - item.eta_at))} ago"
        if kind == "eta_passed":
            why += ", no activity since"
    elif kind == "unassigned" and item is not None:
        # Measured like the flag: since the item was last touched (made open
        # or handed back), not since the flag was raised.
        touched = max(t for t in (item.touched_at, item.clock_reset_at) if t is not None)
        why = f"no holder {_short(_seconds(now - touched))}"
    elif kind == "check_back" and item is not None:
        why = f"check back on \"{item.blocked_on}\""
    elif kind == SUGGESTED:
        why = f"{detail.get('by')} posted {detail.get('keyword')}: {detail.get('to')}?"
    elif kind in ("runner_void", "input_void"):
        what = (f"runner {detail.get('runner_sha')}" if kind == "runner_void"
                else f"input {detail.get('input')}")
        why = f"{what} VOID ×{detail.get('count')} in 24 h"
    elif kind == "needs_triage":
        why = f"{detail.get('reason')}: not allowed on this board"
    elif kind == "unlaunched" and item is not None:
        since = detail.get("actionable_at")
        try:
            age = _short(_seconds(now - datetime.fromisoformat(since)))
        except (TypeError, ValueError):
            age = "?"
        why = f"no live holder on it {age}"
        if free:
            why += "; free: " + ", ".join(free[:3])
    elif kind == "holder_overloaded":
        why = f"{', '.join(detail.get('holders', []))} holds more active items than this board allows"
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
            orch = await _orchestrator_row(conn, board, now)
            board["resolved_orchestrator_pid"] = (orch or {}).get("pid")
            # Holders already warned as overloaded (before this pass): a
            # holder hears once per episode, not once per flagged item.
            overloaded = {name for it in items.values()
                          for name in ((it.open_flags.get("holder_overloaded") or {})
                                       .get("detail") or {}).get("holders", [])}
            # Holders the orchestrator has already heard about in their
            # current overload episode (an open flag that reached it).
            notified = await _overload_notified(conn, board["id"])
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
                    if kind == "stale" and not (detail or {}).get("eta_at"):
                        step = stale_step(item, board, now)
                        if step == "nudge":
                            # Quiet first: the holder gets a chance to touch
                            # it before the orchestrator hears.
                            item.stale_nudged_at = now
                            tx.dirty.add(item.n)
                            summary.setdefault("nudged", []).append(item.n)
                            for h in item.holders:
                                tx._push(item, h, "stale_nudge",
                                         f"[cm-board {board['slug']}] #{item.n} \"{item.title}\" looks "
                                         f"stale: item_set({item.n}, ...) (status, eta or note) or post "
                                         f"#{item.n} in chat. The orchestrator is told if it stays untouched.")
                            continue
                        if step == "wait":
                            continue
                    tx.raise_flag(item, kind, detail)
                    tx.dirty.add(item.n)
                    summary["raised"].append((item.n, kind))
                    if kind in HOLDER_FIRST and _news(item, f"{kind}:holder", kind, detail):
                        for h in item.holders:
                            tx._push(item, h, kind,
                                     f"[cm-board {board['slug']}] #{item.n} \"{item.title}\" is "
                                     f"past its eta: update it (new eta, done, or hand back).")
                        _mark(item, f"{kind}:holder", kind, detail, now)
                        tx.dirty.add(item.n)
                    if kind == "holder_overloaded":
                        for h in item.holders:
                            name = h.get("name") or h["pid"]
                            if name in detail["holders"] and name not in overloaded:
                                overloaded.add(name)
                                # Counted like the flag: inside a job's ETA an
                                # item does not need its holder.
                                count = sum(1 for o in items.values() if o.status == "active"
                                            and o.archived_at is None and not in_eta(o, now)
                                            and any(x["pid"] == h["pid"] for x in o.holders))
                                tx._push(item, h, kind,
                                         f"[cm-board {board['slug']}] you hold {count} active items "
                                         f"(this board allows {board.get('max_active_per_holder') or 2} "
                                         f"outside a job's eta). Block or hand back the ones you're "
                                         f"not on: item_set(n, blocked_by=[m]) / item_set(n, "
                                         f"holder=\"none\").")
                for kind in sorted(set(item.open_flags) - set(want)):
                    if kind in ENGINE_KINDS and tx.resolve_flag(item, kind, "cleared"):
                        summary["cleared"].append((item.n, kind))
            await items_db._apply(conn, tx)

            overload_news = await _overload_episodes(conn, board["id"], notified, now)
            await _push_orchestrator(conn, board, items, states, orch, now, summary,
                                     overload_news)
            await _escalate(conn, board, items, orch, now, summary)
    return summary


def _flag_holders(detail) -> set[str]:
    if isinstance(detail, str):
        detail = json.loads(detail)
    return set((detail or {}).get("holders", []))


async def _overload_notified(conn, board_id: str) -> set[str]:
    rows = await conn.fetch(
        """SELECT f.detail FROM item_flags f JOIN items i ON i.id = f.item_id
            WHERE i.board_id = $1 AND f.kind = 'holder_overloaded'
              AND f.resolved_at IS NULL AND f.last_pushed_at IS NOT NULL""",
        uuid.UUID(board_id))
    return set().union(*(_flag_holders(r["detail"]) for r in rows))


async def _overload_episodes(conn, board_id: str, notified: set[str], now: datetime) -> set[str]:
    """holder_overloaded reaches the orchestrator once per holder episode.
    A flag that moved to another item (the holder's focus changed) inherits
    the notice already given; a holder nobody has told yet is news, and stays
    news until a push to the orchestrator actually goes out (none goes out
    with no live orchestrator), so a missed notice is sent on the next pass."""
    rows = await conn.fetch(
        """SELECT f.id, f.detail FROM item_flags f JOIN items i ON i.id = f.item_id
            WHERE i.board_id = $1 AND f.kind = 'holder_overloaded'
              AND f.resolved_at IS NULL AND f.last_pushed_at IS NULL""",
        uuid.UUID(board_id))
    news: set[str] = set()
    carried = []
    for r in rows:
        holders = _flag_holders(r["detail"])
        if holders and holders <= notified:
            carried.append(r["id"])
        else:
            news |= holders - notified
    if carried:
        await conn.execute("UPDATE item_flags SET last_pushed_at = $2 WHERE id = ANY($1::bigint[])",
                           carried, now)
    return news


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


async def _push_orchestrator(conn, board, items, states, orch, now, summary,
                             overload_news: set[str] = frozenset()) -> None:
    flags = await _open_flags(conn, board["id"])
    repush = timedelta(seconds=board["repush_s"])
    # The holder hears about an overdue item or a passed ETA first.
    # A stale gate receipt is the holder's to rerun; it never wakes the
    # orchestrator (doc/gate-receipts.md §7).
    eligible = [f for f in flags
                if f["kind"] not in HOLDER_ONLY_FLAGS
                and not (f["kind"] in HOLDER_FIRST and now - f["raised_at"] < repush)]
    # Only flags whose item changed since they last reached the orchestrator.
    # One wake carries all of them.
    # holder_overloaded is news once per holder episode, whichever items carry it.
    due = [f for f in eligible
           if (set((f.get("detail") or {}).get("holders", [])) & overload_news
               if f["kind"] == "holder_overloaded" else _due(f, items.get(f["n"])))]
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
    free = None
    if any(f["kind"] == "unlaunched" for f in due):
        rows, _ = await items_db._free_capacity(
            conn, board, now, [orch.get("pid"), board.get("steward_pid")])
        free = [items_db.free_label(r) for r in rows if r["state"] == "idle"]
    parts = [describe(f, items.get(f["n"]), states, now, free) for f in due]
    text = compose(board["slug"], parts, closed)
    await _push(conn, board["id"], orch, "board", text, now)
    if due:
        await conn.execute(
            """UPDATE item_flags SET last_pushed_at = $2, push_count = push_count + 1
                WHERE id = ANY($1::bigint[])""",
            [f["id"] for f in due], now,
        )
        await _mark_pushed(conn, items, due, now)
    summary["pushed"] = True


# Boards whose escalation had no session to carry it (logged once).
_UNROUTABLE: set[str] = set()


async def _escalate(conn, board, items, orch, now, summary) -> None:
    flags = await _open_flags(conn, board["id"])
    threshold = timedelta(seconds=board["escalate_s"])
    stuck = [f for f in flags if f["escalated_at"] is None and now - f["raised_at"] >= threshold
             and f["kind"] not in GATE_FLAG_KINDS and f["kind"] not in HOLDER_ONLY_FLAGS
             and f["kind"] not in NO_ESCALATE
             and _due(f, items.get(f["n"]), ":escalated")]
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
    await _mark_pushed(conn, items, stuck, now, ":escalated")
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
            # Placed tasks whose status mirror waited on the gate
            # (doc/org-model.md §7.1): write them through once it opens.
            try:
                from dispatch import placement
                await placement.remirror_pending(pool)
            except Exception:
                logger.exception("board engine: re-mirror pass failed")
        except asyncio.CancelledError:
            raise
        except Exception:
            logger.exception("board engine pass failed")
        tick += 1
        await asyncio.sleep(interval)
