"""Org model S2: members, managers, teams, the roster and adoption
(doc/org-model.md §4, §6, §9.1-9.2, §12, §14; schema in sql/028_org_core.sql).

Agents reach these functions only through their host daemon, which stamps the
actor; Owner is the actor pid ``owner`` (the daemon's operator caller).
Nothing here enforces anything outside the org tables: holds stay keyed by
pid and session scope is unchanged until S6/S7.

Every write transaction takes the task feed's advisory lock first (the same
lock the org tables' statement triggers take), so the manager and team cycle
checks, the handle checks and the org change feed are serialised.
"""

from __future__ import annotations

import os
import re
import secrets
import time
import uuid
from datetime import datetime, timedelta, timezone

import asyncpg

from dispatch.items_rules import ItemsError

ORG = "main"
OWNER_ID = "mem_owner"
OWNER_PID = "owner"
# Applies and acks pushed home/stream marks (doc/org-model.md §12). Marks go
# only to a daemon advertising this: an S0b brain preserves the org object
# but would drop the marks and burn their attempts.
ORG_MARKS = "org_marks_v1"

# A session this old that holds open board items (or orchestrates a board)
# is proposed as a long-lived lane.
LANE_MIN_AGE_S = float(os.environ.get("CM_ORG_LANE_MIN_AGE_S", 24 * 3600))
MAX_MARK_ATTEMPTS = 10
# What a daemon stamps a spawned session as (O5): a durable assistant, or a
# continuous member's worker (attributed, torn down as today).
STAMPED_ROLES = ("assistant", "worker")
MAX_ROWS = 200

HANDLE_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$")
# Words a handle may not be: they already mean something in mentions and
# references.
RESERVED_HANDLES = {"owner", "me", "none", "all", "here", "everyone", "channel", "main"}
MEMBER_KINDS = {"agent", "continuous"}
TEAM_KINDS = {"team", "initiative", "division"}
TEAM_STATUSES = {"active", "standby", "archived"}
CLOSED_INITIATIVE = ("archived", "cancelled", "completed")
OPEN_ITEM = ("open", "active", "waiting", "blocked")
AGENT_ENGINES = {"claude", "claude-code", "codex"}


class OrgError(ItemsError):
    """A refused org write; the API answers `status` with `detail()`."""


def _err(status: int, code: str, message: str, **extra) -> OrgError:
    return OrgError(status, code, message, **extra)


def utcnow() -> datetime:
    return datetime.now(timezone.utc)


# ---- identifiers and handles ---------------------------------------------------

_CROCKFORD = "0123456789abcdefghjkmnpqrstvwxyz"


def new_member_id() -> str:
    """`mem_<ulid>`: 48 bits of milliseconds, 80 random bits, Crockford base32."""
    value = (int(time.time() * 1000) << 80) | secrets.randbits(80)
    chars = []
    for _ in range(26):
        chars.append(_CROCKFORD[value & 31])
        value >>= 5
    return "mem_" + "".join(reversed(chars))


def normalize_handle(text: str | None) -> str | None:
    """Lowercase `[a-z0-9-]`, dashes collapsed, at most 40 characters; None
    when nothing usable is left."""
    if not text:
        return None
    handle = re.sub(r"[^a-z0-9]+", "-", str(text).lower()).strip("-")
    handle = re.sub(r"-+", "-", handle)[:40].strip("-")
    return handle if len(handle) >= 2 else None


def normalize_alias(text: str | None) -> str | None:
    """An alias in handle form (it may be longer than a handle, e.g.
    `<slug>-orchestrator`); None when unusable or a reserved word."""
    if not text:
        return None
    alias = re.sub(r"-+", "-", re.sub(r"[^a-z0-9]+", "-", str(text).lower())).strip("-")[:80]
    if len(alias) < 2 or alias in RESERVED_HANDLES:
        return None
    return alias


def check_handle(handle) -> str:
    if not isinstance(handle, str) or not HANDLE_RE.match(handle):
        raise _err(422, "invalid_handle",
                   f"handle {handle!r} must be 2-40 characters of a-z, 0-9 and '-', "
                   "starting and ending with a letter or digit")
    if handle in RESERVED_HANDLES:
        raise _err(422, "invalid_handle", f"handle {handle!r} is reserved")
    return handle


def _free_handle(base: str, taken: set[str]) -> str:
    """`base`, else `base-2`, `base-3`, ... (never a taken handle or alias)."""
    if base not in taken and base not in RESERVED_HANDLES:
        return base
    for n in range(2, 1000):
        suffix = f"-{n}"
        candidate = base[: 40 - len(suffix)].rstrip("-") + suffix
        if candidate not in taken:
            return candidate
    raise _err(409, "handle_taken", f"no free handle near {base!r}")


def spawn_order(uid: str) -> tuple[int, int, int]:
    """The daemon's restart-stable spawn order of a session uid
    (`ts-<nanos hex>-<counter hex>`; daemon/src/org.rs::spawn_order)."""
    try:
        nanos, counter = uid.removeprefix("ts-").split("-", 1) if uid.startswith("ts-") else ("", "")
        return (0, int(nanos, 16), int(counter, 16))
    except ValueError:
        return (1, 0, 0)


def lead_session(pin: str | None, candidates: list[dict]) -> str | None:
    """The lead rule (daemon/src/org.rs::task_lead) over `session_states`
    rows bound to one task: `"owner"` for an Owner pin, else the pinned live
    agent session, else the earliest-spawned live agent session."""
    pin = (pin or "").strip()
    if pin.lower() == OWNER_PID:
        return OWNER_PID
    eligible = [c for c in candidates if (c.get("engine") or "").lower() in AGENT_ENGINES]
    if pin and any(c["session_uid"] == pin for c in eligible):
        return pin
    if not eligible:
        return None
    return min(eligible, key=lambda c: (spawn_order(c["session_uid"]), c["session_uid"]))["session_uid"]


def _clip(value, limit: int):
    if value is None:
        return None
    text = str(value)
    return text[:limit] if text else None


def _json_safe(value):
    if isinstance(value, dict):
        return {k: _json_safe(v) for k, v in value.items()}
    if isinstance(value, (list, tuple)):
        return [_json_safe(v) for v in value]
    if isinstance(value, uuid.UUID):
        return str(value)
    if isinstance(value, datetime):
        return value.isoformat()
    return value


def _uuid(value, field: str):
    if value is None:
        return None
    try:
        return uuid.UUID(str(value))
    except ValueError:
        raise _err(422, "invalid_field", f"{field} must be a UUID") from None


async def _lock(conn) -> None:
    await conn.execute("SELECT pg_advisory_xact_lock(hashtext('cm_task_changes'))")


def _is_owner(actor: dict) -> bool:
    return (actor or {}).get("pid") == OWNER_PID


def _require_owner(actor: dict, what: str) -> None:
    if not _is_owner(actor):
        raise _err(403, "owner_only", f"{what} is Owner-only in S2")


def _db_error(exc: Exception) -> OrgError:
    text = str(exc)
    if "org_cycle" in text:
        return _err(409, "cycle", text.split(":", 1)[-1].strip() or "would create a cycle")
    if "org_unknown_manager" in text:
        return _err(422, "unknown_manager", text.split(":", 1)[-1].strip())
    if "org_retired_manager" in text or "org_cross_org_manager" in text:
        return _err(409, "invalid_manager", text.split(":", 1)[-1].strip())
    if isinstance(exc, asyncpg.UniqueViolationError):
        if "handle" in text or "member_aliases" in text:
            return _err(409, "handle_taken", "that handle or alias already belongs to a member")
        if "incarnations_open_pid" in text:
            return _err(409, "pid_already_member", "that session is already a member's session")
        if "initiatives_slug" in text:
            return _err(409, "slug_taken", "a team or initiative already has that slug")
    if isinstance(exc, asyncpg.ForeignKeyViolationError):
        return _err(422, "unknown_ref", "a referenced member, team or task does not exist")
    return _err(409, "conflict", f"refused by the database ({type(exc).__name__})")


class _Replayed(Exception):
    """A request_id already answered (found again under the lock)."""

    def __init__(self, response):
        super().__init__("replayed")
        self.response = response


async def _replay_under_lock(conn, request_id, actor, method) -> None:
    stored = await _stored_request(conn, request_id, actor, method)
    if stored is not None:
        raise _Replayed(stored)


# ---- reads ---------------------------------------------------------------------

def _profile_value(profile: dict, field: str):
    value = (profile or {}).get(field)
    if isinstance(value, dict):
        return value.get("value")
    return value


_MEMBER_SELECT = """
SELECT m.*,
       (SELECT coalesce(array_agg(a.alias ORDER BY a.created_at, a.alias), '{}')
          FROM member_aliases a WHERE a.member_id = m.id AND a.alias <> m.handle) AS aliases,
       inc.pid AS main_pid, inc.daemon_id AS main_daemon_id, inc.session_uid AS main_session_uid,
       inc.occupancy_generation AS main_generation, inc.started_at AS main_started_at,
       inc.workspace_id AS main_workspace_id,
       hm.daemon_id AS home_daemon_id, hm.workspace_id AS home_workspace_id,
       hm.path AS home_path, hm.state AS home_state,
       mgr.handle AS manager_handle
  FROM members m
  LEFT JOIN incarnations inc
         ON inc.member_id = m.id AND inc.role = 'main' AND inc.ended_at IS NULL
  LEFT JOIN LATERAL (SELECT * FROM org_workspace_marks w
                      WHERE w.member_id = m.id AND w.kind = 'home'
                      ORDER BY w.id DESC LIMIT 1) hm ON TRUE
  LEFT JOIN members mgr ON mgr.id = m.manager_member_id
"""


def _shape_member(row) -> dict:
    r = dict(row)
    profile = r.get("profile") or {}
    main = None
    if r.get("main_pid"):
        main = {
            "pid": r["main_pid"], "daemon_id": r["main_daemon_id"],
            "session_uid": r["main_session_uid"],
            "occupancy_generation": r["main_generation"],
            "workspace_id": r["main_workspace_id"],
            "since": _json_safe(r["main_started_at"]),
        }
    home = None
    if r.get("home_workspace_id"):
        home = {"daemon_id": r["home_daemon_id"], "workspace_id": r["home_workspace_id"],
                "path": r["home_path"], "state": r["home_state"]}
    return {
        "id": r["id"],
        "org_id": r["org_id"],
        "handle": r["handle"],
        "qualified": f"{r['org_id']}/{r['handle']}",
        "kind": r["kind"],
        "status": r["status"],
        "title": _profile_value(profile, "title") or "",
        "mandate": _profile_value(profile, "mandate") or "",
        "focus_pinned": _profile_value(profile, "focus_pinned"),
        "focus_note": _profile_value(profile, "focus_note"),
        "focus": None,  # derived from S5
        "manager_member_id": r["manager_member_id"],
        "manager_handle": r.get("manager_handle"),
        "primary_team_id": _json_safe(r["primary_team_id"]),
        "main": main,
        # A vacant member's stand-in is its manager until deputies exist
        # (nothing marks a member vacant before S6).
        "stand_in": r["manager_member_id"] if r["status"] == "vacant" else None,
        "home_workspace": home,
        "aliases": list(r.get("aliases") or []),
        "standing_rules": r.get("standing_rules") or [],
        "revision": r["revision"],
        "created_at": _json_safe(r["created_at"]),
        "retired_at": _json_safe(r["retired_at"]),
    }


def member_summary(member: dict | None) -> dict | None:
    """The small form other rows embed (tasks, the heartbeat cache, `org`)."""
    if not member:
        return None
    return {key: member.get(key) for key in (
        "id", "handle", "qualified", "kind", "status", "title", "manager_member_id",
        "manager_handle", "primary_team_id")}


async def _members(conn, where: str = "", *args) -> list[dict]:
    rows = await conn.fetch(f"{_MEMBER_SELECT} {where} ORDER BY m.created_at, m.id", *args)
    return [_shape_member(r) for r in rows]


_TEAM_SELECT = """
SELECT i.id, i.slug, i.name, i.kind, i.status, i.team_status, i.parent_team_id,
       i.lead_member_id, i.coordinator_task_id, i.shared_channel, i.charter,
       i.standing_rules, i.org_id, i.team_revision,
       coalesce(b.slug, rb.slug) AS board,
       (SELECT coalesce(jsonb_agg(jsonb_build_object('member_id', tm.member_id, 'role', tm.role)
                                  ORDER BY tm.since, tm.member_id), '[]'::jsonb)
          FROM team_memberships tm WHERE tm.team_id = i.id AND tm.until IS NULL) AS members
  FROM initiatives i
  LEFT JOIN boards b ON b.initiative_id = i.id
  LEFT JOIN boards rb ON b.id IS NULL AND i.coordinator_task_id IS NOT NULL
                     AND rb.root_task_id = i.coordinator_task_id AND rb.initiative_id IS NULL
"""


def _shape_team(row) -> dict:
    r = dict(row)
    return {
        "id": str(r["id"]),
        "slug": r["slug"],
        "name": r["name"],
        "kind": r["kind"],
        "status": r["team_status"],
        "initiative_status": r["status"],
        "parent_team_id": _json_safe(r["parent_team_id"]),
        "lead_member_id": r["lead_member_id"],
        "coordinator_task_id": _json_safe(r["coordinator_task_id"]),
        "board": r["board"],
        "channel": r["shared_channel"],
        "charter": r["charter"],
        "standing_rules": r["standing_rules"] or [],
        "org_id": r["org_id"],
        "members": r["members"] or [],
        "revision": r["team_revision"],
    }


async def _teams(conn, where: str = "", *args) -> list[dict]:
    rows = await conn.fetch(f"{_TEAM_SELECT} {where} ORDER BY i.created_at, i.slug", *args)
    return [_shape_team(r) for r in rows]


async def version(conn) -> int:
    return int(await conn.fetchval("SELECT coalesce(max(seq), 0) FROM org_changes"))


async def snapshot(pool, since_version: int | None = None) -> dict:
    """Every member and every live team, at one feed version."""
    async with pool.acquire() as conn:
        async with conn.transaction(isolation="repeatable_read", readonly=True):
            current = await version(conn)
            if since_version is not None and current <= since_version:
                return {"unchanged": True, "version": current}
            members = await _members(conn)
            teams = await _teams(
                conn,
                """WHERE i.status NOT IN ('archived', 'cancelled')
                      OR i.lead_member_id IS NOT NULL
                      OR EXISTS (SELECT 1 FROM team_memberships tm WHERE tm.team_id = i.id)""")
    return {"version": current, "members": members, "teams": teams}


async def changes(pool, since: int, limit: int = 500) -> dict:
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT seq, subject_type, subject_id, op, changed_at FROM org_changes
                WHERE seq > $1 ORDER BY seq LIMIT $2""", since, limit + 1)
        current = await version(conn)
    more = len(rows) > limit
    rows = rows[:limit]
    return {
        "version": current,
        "changes": [_json_safe(dict(r)) for r in rows],
        "more": more,
        "next": rows[-1]["seq"] if rows else since,
    }


async def _resolve_member_id(conn, ref) -> str | None:
    """A member id, `owner`, a handle or alias, or `<org>/<handle>`."""
    if not isinstance(ref, str):
        return None
    text = ref.strip().lstrip("@").lower()
    if not text:
        return None
    if text == OWNER_PID:
        return OWNER_ID
    if text.startswith("mem_"):
        return await conn.fetchval("SELECT id FROM members WHERE id = $1", text)
    org, _, handle = text.rpartition("/")
    return await conn.fetchval(
        "SELECT member_id FROM member_aliases WHERE org_id = $1 AND alias = $2",
        org or ORG, handle)


async def _chain(conn, member_id: str) -> list[dict]:
    chain, seen, cur = [], {member_id}, member_id
    for _ in range(64):
        cur = await conn.fetchval("SELECT manager_member_id FROM members WHERE id = $1", cur)
        if not cur or cur in seen:
            break
        seen.add(cur)
        found = await _members(conn, "WHERE m.id = $1", cur)
        if not found:
            break
        chain.append(member_summary(found[0]))
    return chain


async def _detail(conn, member_id: str) -> dict:
    found = await _members(conn, "WHERE m.id = $1", member_id)
    if not found:
        raise _err(404, "not_found", f"no member {member_id}")
    member = found[0]
    reports = await _members(conn, "WHERE m.manager_member_id = $1 AND m.status <> 'retired'",
                             member_id)
    teams = await _teams(
        conn,
        """WHERE i.lead_member_id = $1 OR EXISTS (
               SELECT 1 FROM team_memberships tm
                WHERE tm.team_id = i.id AND tm.member_id = $1 AND tm.until IS NULL)""",
        member_id)
    for team in teams:
        team["my_role"] = "lead" if team["lead_member_id"] == member_id else next(
            (m["role"] for m in team["members"] if m["member_id"] == member_id), None)
    return {
        "member": member,
        "chain": await _chain(conn, member_id),
        "reports": [member_summary(r) for r in reports],
        "teams": teams,
        "version": await version(conn),
    }


async def get_member(pool, ref: str) -> dict:
    async with pool.acquire() as conn:
        member_id = await _resolve_member_id(conn, ref)
        if not member_id:
            raise _err(404, "not_found", f"no member {ref!r}")
        return await _detail(conn, member_id)


async def member_for_pid(pool, pid: str) -> dict:
    """Whom a session works as: the member whose open main row it is
    (`acting_as: main`), else the member whose assistant its daemon reports
    it to be (`acting_as: assistant`, O5), else none."""
    async with pool.acquire() as conn:
        member_id = await conn.fetchval(
            """SELECT member_id FROM incarnations
                WHERE pid = $1 AND role = 'main' AND ended_at IS NULL""", pid)
        if not member_id:
            # O5: an assistant, as its daemon last reported it.
            row = await conn.fetchrow(
                """SELECT f.member_id, f.acting_as FROM org_session_facts f
                     JOIN members m ON m.id = f.member_id
                    WHERE f.pid = $1 AND f.acting_as IN ('assistant', 'worker')""", pid)
            if row:
                detail = await _detail(conn, row["member_id"])
                return {"pid": pid, "acting_as": row["acting_as"], **detail}
            return {"pid": pid, "member": None, "acting_as": None,
                    "version": await version(conn)}
        detail = await _detail(conn, member_id)
        return {"pid": pid, "acting_as": "main", **detail}


async def get_team(pool, ref: str) -> dict:
    async with pool.acquire() as conn:
        team = await _team_row(conn, ref)
        teams = await _teams(conn, "WHERE i.id = $1", team["id"])
        result = teams[0]
        result["lead"] = None
        if result["lead_member_id"]:
            found = await _members(conn, "WHERE m.id = $1", result["lead_member_id"])
            result["lead"] = member_summary(found[0]) if found else None
        ids = [m["member_id"] for m in result["members"]]
        result["member_rows"] = [member_summary(m) for m in await _members(
            conn, "WHERE m.id = ANY($1::text[])", ids)] if ids else []
        result["sub_teams"] = await _teams(conn, "WHERE i.parent_team_id = $1", team["id"])
        result["version"] = await version(conn)
        return result


async def _team_row(conn, ref) -> dict:
    try:
        row = await conn.fetchrow("SELECT * FROM initiatives WHERE id = $1", uuid.UUID(str(ref)))
    except ValueError:
        row = await conn.fetchrow("SELECT * FROM initiatives WHERE slug = $1", str(ref))
    if not row:
        raise _err(404, "not_found", f"no team {ref!r}")
    return dict(row)


async def task_org(pool, task: dict) -> dict:
    """The org facts `GET /tasks/{id}` (and so `get_task` and A-i) carry: the
    task's team, the member leading it, and members whose main session works
    on it."""
    task_id = task.get("id")
    async with pool.acquire() as conn:
        team = None
        if task.get("initiative_id"):
            found = await _teams(conn, "WHERE i.id = $1", uuid.UUID(str(task["initiative_id"])))
            team = found[0] if found else None
        sessions = await conn.fetch(
            """SELECT s.pid, s.session_uid, s.engine, inc.member_id
                 FROM session_states s
                 LEFT JOIN incarnations inc ON inc.pid = s.pid AND inc.role = 'main'
                                           AND inc.ended_at IS NULL
                WHERE s.task_id = $1 AND s.exited_at IS NULL AND s.state <> 'exited'
                  AND s.engine IS DISTINCT FROM 'bash'""",
            uuid.UUID(str(task_id)))
        ids = sorted({r["member_id"] for r in sessions if r["member_id"]})
        if team and team["lead_member_id"]:
            ids = sorted(set(ids) | {team["lead_member_id"]})
        by_id = {m["id"]: m for m in await _members(conn, "WHERE m.id = ANY($1::text[])", ids)}
        pin = ((task.get("metadata") or {}).get("lead_session_uid") or "")
        lead = lead_session(pin, [dict(r) for r in sessions])
        lead_member = None
        if lead == OWNER_PID:
            found = await _members(conn, "WHERE m.id = $1", OWNER_ID)
            lead_member = member_summary(found[0]) if found else None
        elif lead:
            member_id = next((r["member_id"] for r in sessions if r["session_uid"] == lead), None)
            lead_member = member_summary(by_id.get(member_id))
        team_lead = member_summary(by_id.get(team["lead_member_id"])) if team and team["lead_member_id"] else None
        return {
            "team": None if team is None else {
                "id": team["id"], "slug": team["slug"], "name": team["name"],
                "kind": team["kind"], "status": team["status"], "lead": team_lead},
            "lead_member": lead_member,
            "members": [
                {**member_summary(by_id[r["member_id"]]), "session_uid": r["session_uid"],
                 "acting_as": "main"}
                for r in sessions if r["member_id"] in by_id
            ],
        }


# ---- the roster ---------------------------------------------------------------

async def _roster_inputs(conn, continuous_states) -> dict:
    live = [dict(r) for r in await conn.fetch(
        """SELECT s.pid, s.daemon_id, s.session_uid, s.task_id, s.name, s.engine,
                  f.workspace_id, f.worktree_path, f.continuous_task_id, f.managed_by_uid,
                  f.workflow_run_id, f.spawned_at
             FROM session_states s LEFT JOIN org_session_facts f ON f.pid = s.pid
            WHERE s.exited_at IS NULL AND s.state <> 'exited'
            ORDER BY s.pid""")]
    member_pids = {r["pid"]: r["member_id"] for r in await conn.fetch(
        "SELECT pid, member_id FROM incarnations WHERE ended_at IS NULL")}
    mapped = {(r["daemon_id"], r["continuous_id"]) for r in await conn.fetch(
        "SELECT daemon_id, continuous_id FROM continuous_members")}
    taken = {r["alias"] for r in await conn.fetch(
        "SELECT alias FROM member_aliases WHERE org_id = $1", ORG)}
    declined = {r["proposal_id"] for r in await conn.fetch(
        "SELECT proposal_id FROM org_roster_declines")}
    initiatives = [dict(r) for r in await conn.fetch(
        f"""SELECT id, slug, name, kind, coordinator_task_id, lead_member_id, parent_team_id
              FROM initiatives
             WHERE coordinator_task_id IS NOT NULL AND status NOT IN {CLOSED_INITIATIVE}
             ORDER BY created_at, slug""")]
    # Boards anchored by a root task (no initiative) get a team (§9.2), once
    # they have open work or a live orchestrator.
    root_boards = [dict(r) for r in await conn.fetch(
        f"""SELECT b.id, b.slug, b.name, b.root_task_id, b.orchestrator_pid
              FROM boards b
             WHERE b.initiative_id IS NULL AND b.root_task_id IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM initiatives i
                                WHERE i.coordinator_task_id = b.root_task_id)
               -- The team takes its root task as coordinator, which a task
               -- already in an initiative cannot be (coordinator_taken).
               AND EXISTS (SELECT 1 FROM tasks t
                            WHERE t.id = b.root_task_id AND t.initiative_id IS NULL)
               AND EXISTS (SELECT 1 FROM items it
                            WHERE it.board_id = b.id AND it.archived_at IS NULL
                              AND it.status IN {OPEN_ITEM})
             ORDER BY b.created_at, b.slug""")]
    anchor_tasks = [r["coordinator_task_id"] for r in initiatives] + [r["root_task_id"] for r in root_boards]
    pins = {r["id"]: (r["metadata"] or {}).get("lead_session_uid") for r in await conn.fetch(
        "SELECT id, metadata FROM tasks WHERE id = ANY($1::uuid[])", anchor_tasks)} if anchor_tasks else {}
    task_names = {r["id"]: r["name"] for r in await conn.fetch(
        "SELECT id, name FROM tasks WHERE id = ANY($1::uuid[])",
        sorted({r["task_id"] for r in live if r["task_id"]}))} if live else {}
    # Open holds and board orchestration: pid -> boards, in board order.
    holds: dict[str, list] = {}
    for r in await conn.fetch(
            f"""SELECT DISTINCT h.pid, b.id AS board_id, b.initiative_id, b.root_task_id, b.created_at
                  FROM item_holders h JOIN items it ON it.id = h.item_id
                  JOIN boards b ON b.id = it.board_id
                 WHERE it.archived_at IS NULL AND it.status IN {OPEN_ITEM}
                UNION
                SELECT b.orchestrator_pid, b.id, b.initiative_id, b.root_task_id, b.created_at
                  FROM boards b WHERE b.orchestrator_pid IS NOT NULL
                 ORDER BY 5, 2"""):
        holds.setdefault(r["pid"], []).append(dict(r))
    return {"live": live, "member_pids": member_pids, "mapped": mapped, "taken": taken,
            "declined": declined, "initiatives": initiatives, "root_boards": root_boards,
            "pins": pins, "task_names": task_names, "holds": holds,
            "continuous": list(continuous_states or [])}


def _home_of(session: dict | None) -> dict | None:
    if not session or not session.get("workspace_id"):
        return None
    return {"daemon_id": session["daemon_id"], "workspace_id": session["workspace_id"],
            "path": session.get("worktree_path")}


def generate_roster(inputs: dict) -> list[dict]:
    """Proposed members, deterministic for the same inputs.

    Sources, in priority order (a session is proposed once, under the first
    source that claims it): continuous tasks, then team leads (initiatives
    and divisions by today's lead rule, and boards anchored by a root task),
    then long-lived lanes. Handles are normalised and collision-suffixed;
    a taken alias is never stolen."""
    # Only shells are left out; a row with no engine (an older TUI) is an agent.
    live = [s for s in inputs["live"] if (s.get("engine") or "").lower() != "bash"]
    by_uid: dict[str, list[dict]] = {}
    for s in live:
        by_uid.setdefault(s["session_uid"], []).append(s)
    taken = set(inputs["taken"])
    claimed: dict[str, dict] = {}  # pid -> proposal
    proposals: list[dict] = []

    def new(proposal_id, source, *, name, kind, title, session=None, team_id=None,
            aliases=(), continuous=None):
        conflicts = []
        base = normalize_handle(name) or "member"
        handle = _free_handle(base, taken)
        if handle != base:
            conflicts.append(f"handle {base} taken → {handle}")
        taken.add(handle)
        extra = []
        for alias in aliases:
            alias = normalize_alias(alias)
            if not alias or alias == handle or alias in extra:
                continue
            if alias in taken:
                conflicts.append(f"alias {alias} belongs to another member; not added")
                continue
            taken.add(alias)
            extra.append(alias)
        p = {
            "proposal_id": proposal_id, "source": source, "member_id": None,
            "handle": handle, "kind": kind, "title": title, "team_id": team_id,
            "lead_of": [], "manager_member_id": OWNER_ID, "manager_handle": "owner",
            "home_workspace": _home_of(session), "pids": [session["pid"]] if session else [],
            "aliases": extra, "continuous": continuous, "conflicts": conflicts,
        }
        if session:
            claimed[session["pid"]] = p
        proposals.append(p)
        return p

    # 1. Continuous tasks: the record's orchestrator, keyed (daemon, id).
    seen_continuous = set()
    orchestrators = {}
    for s in live:
        if s.get("continuous_task_id") and not s.get("managed_by_uid"):
            orchestrators.setdefault((s["daemon_id"], s["continuous_task_id"]), s)
    records = []
    for st in sorted(inputs["continuous"], key=lambda st: str(st.get("task_id"))):
        cid = st.get("task_id")
        if not cid:
            continue
        session = next(iter(by_uid.get(st.get("current_session_uid") or "", [])), None)
        if session is None:
            session = next((s for (d, c), s in sorted(orchestrators.items()) if c == cid), None)
        if session is None:
            continue
        records.append((session["daemon_id"], cid, st, session))
    for (daemon_id, cid), session in sorted(orchestrators.items()):
        if not any(r[0] == daemon_id and r[1] == cid for r in records):
            records.append((daemon_id, cid, {"task_id": cid}, session))
    for daemon_id, cid, st, session in records:
        key = (daemon_id, cid)
        proposal_id = f"continuous:{daemon_id}:{cid}"
        if key in inputs["mapped"] or key in seen_continuous or proposal_id in inputs["declined"]:
            continue
        seen_continuous.add(key)
        if session["pid"] in inputs["member_pids"] or session["pid"] in claimed:
            session = None
        p = new(proposal_id, "continuous", name=cid, kind="continuous",
                title=st.get("label") or cid, session=session,
                aliases=[cid, f"{cid}-orchestrator"],
                continuous={"daemon_id": daemon_id, "continuous_id": cid,
                            "planning_task_id": st.get("planning_task_id")})
        if session is None:
            p["conflicts"].append("orchestrator session is already a member or not live; "
                                  "the member starts with no main session")

    # 2. Team leads.
    anchors = []
    for init in inputs["initiatives"]:
        if init["lead_member_id"]:
            continue
        anchors.append({"team_id": str(init["id"]), "name": init["name"], "slug": init["slug"],
                        "task_id": init["coordinator_task_id"], "kind": init["kind"],
                        "proposal_id": f"lead:{init['id']}"})
    for board in inputs["root_boards"]:
        anchors.append({"team_id": None, "name": board["name"], "slug": board["slug"],
                        "task_id": board["root_task_id"], "kind": "team",
                        "board_id": str(board["id"]), "proposal_id": f"lead:board:{board['id']}"})
    for anchor in anchors:
        if anchor["proposal_id"] in inputs["declined"]:
            continue
        candidates = [s for s in live if s["task_id"] == anchor["task_id"]]
        lead = lead_session(inputs["pins"].get(anchor["task_id"]), candidates)
        lead_of = {"team_id": anchor["team_id"], "name": anchor["name"], "slug": anchor["slug"],
                   "kind": anchor["kind"]}
        if anchor["team_id"] is None:
            lead_of.update(create=True, board_id=anchor["board_id"],
                           coordinator_task_id=str(anchor["task_id"]))
        if lead is None:
            continue
        if lead == OWNER_PID:
            proposals.append({
                "proposal_id": anchor["proposal_id"], "source": "lead", "member_id": OWNER_ID,
                "handle": "owner", "kind": "owner", "title": "", "team_id": None,
                "lead_of": [lead_of], "manager_member_id": None, "manager_handle": None,
                "home_workspace": None, "pids": [], "aliases": [], "continuous": None,
                "conflicts": []})
            continue
        session = next(s for s in candidates if s["session_uid"] == lead)
        if session["pid"] in inputs["member_pids"]:
            proposals.append({
                "proposal_id": anchor["proposal_id"], "source": "lead",
                "member_id": inputs["member_pids"][session["pid"]], "handle": None,
                "kind": None, "title": None, "team_id": None, "lead_of": [lead_of],
                "manager_member_id": None, "manager_handle": None, "home_workspace": None,
                "pids": [session["pid"]], "aliases": [], "continuous": None, "conflicts": []})
            continue
        if session["pid"] in claimed:
            # One member leading several teams (or a continuous member leading one).
            owner = claimed[session["pid"]]
            owner["lead_of"].append(lead_of)
            owner["team_id"] = owner["team_id"] or anchor["team_id"]
            continue
        p = new(anchor["proposal_id"], "lead", name=session.get("name") or anchor["slug"],
                kind="agent", title=f"Lead, {anchor['name']}", session=session,
                team_id=anchor["team_id"])
        p["lead_of"].append(lead_of)

    # 3. Long-lived lanes: live agent sessions that hold open items (or run a
    # board) and have been alive at least LANE_MIN_AGE_S.
    leads_by_team: dict[str, dict] = {}
    leads_by_board: dict[str, dict] = {}
    for p in proposals:
        for lo in p["lead_of"]:
            if lo.get("team_id"):
                leads_by_team[lo["team_id"]] = p
            if lo.get("board_id"):
                leads_by_board[lo["board_id"]] = p
    team_leads = {str(i["id"]): i["lead_member_id"] for i in inputs["initiatives"]}
    team_of_task = {i["coordinator_task_id"]: str(i["id"]) for i in inputs["initiatives"]}
    now = inputs.get("now") or utcnow()
    for s in live:
        pid = s["pid"]
        if (pid in claimed or pid in inputs["member_pids"]
                or (s.get("engine") or "").lower() not in AGENT_ENGINES
                or s.get("continuous_task_id") or s.get("workflow_run_id")):
            continue
        boards = inputs["holds"].get(pid)
        spawned = s.get("spawned_at")
        if not boards or spawned is None or (now - spawned).total_seconds() < LANE_MIN_AGE_S:
            continue
        proposal_id = f"lane:{pid}"
        if proposal_id in inputs["declined"]:
            continue
        board = boards[0]
        team_id = (str(board["initiative_id"]) if board["initiative_id"]
                   else team_of_task.get(board["root_task_id"]))
        p = new(proposal_id, "lane", name=s.get("name") or s["session_uid"], kind="agent",
                title=inputs["task_names"].get(s["task_id"]) or "", session=s, team_id=team_id)
        lead = leads_by_team.get(team_id) if team_id else leads_by_board.get(str(board["board_id"]))
        if lead is not None and lead is not p:
            p["manager_member_id"] = lead["member_id"]
            p["manager_handle"] = lead["handle"]
        elif team_id and team_leads.get(team_id):
            p["manager_member_id"], p["manager_handle"] = team_leads[team_id], None
        if team_id is None and lead is not None:
            p["team_ref"] = lead["proposal_id"]
    return proposals


async def roster(pool, continuous_states=()) -> dict:
    async with pool.acquire() as conn:
        inputs = await _roster_inputs(conn, continuous_states)
        last = await conn.fetchval("SELECT roster_applied_at FROM org_meta")
        declined = sorted(inputs["declined"])
        current = await version(conn)
    return {"proposals": generate_roster(inputs), "declined": declined,
            "last_applied_at": _json_safe(last), "version": current}


async def _stored_request(conn, request_id, actor, method):
    if not request_id:
        return None
    row = await conn.fetchrow(
        """SELECT response FROM org_requests
            WHERE actor_pid = $1 AND method = $2 AND request_id = $3""",
        (actor or {}).get("pid") or "", method, request_id)
    return row["response"] if row else None


async def _store_request(conn, request_id, actor, method, response) -> None:
    if request_id:
        await conn.execute(
            """INSERT INTO org_requests (request_id, actor_pid, method, response)
               VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING""",
            request_id, actor.get("pid") or "", method, _json_safe(response))


async def _event(conn, subject_type, subject_id, type_, actor, *, prev=None, new=None,
                 reason=None, request_id=None) -> None:
    await conn.execute(
        """INSERT INTO org_events (subject_type, subject_id, type, actor, prev, new, reason, request_id)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8)""",
        subject_type, str(subject_id), type_, _json_safe(actor),
        _json_safe(prev), _json_safe(new), reason, request_id)


async def _home_mark(conn, member_id: str, home: dict | None) -> dict | None:
    """Record a member's home workspace centrally. The owning daemon gets it
    in a heartbeat reply once it advertises org_marks_v1."""
    if not home or not home.get("daemon_id") or not home.get("workspace_id"):
        return None
    row = await conn.fetchrow(
        """INSERT INTO org_workspace_marks (daemon_id, workspace_id, path, kind, member_id)
           VALUES ($1, $2, $3, 'home', $4)
           ON CONFLICT (daemon_id, workspace_id) DO UPDATE SET
               member_id = EXCLUDED.member_id, path = coalesce(EXCLUDED.path, org_workspace_marks.path),
               kind = 'home',
               state = CASE WHEN org_workspace_marks.member_id = EXCLUDED.member_id
                            THEN org_workspace_marks.state ELSE 'central' END,
               attempts = CASE WHEN org_workspace_marks.member_id = EXCLUDED.member_id
                               THEN org_workspace_marks.attempts ELSE 0 END
           RETURNING daemon_id, workspace_id, path, state""",
        _clip(home["daemon_id"], 200), _clip(home["workspace_id"], 200),
        _clip(home.get("path"), 1000), member_id)
    return dict(row)


async def _create_team(conn, actor, *, name, slug=None, kind="team", parent_team_id=None,
                       lead_member_id=None, charter="", standing_rules=None,
                       coordinator_task_id=None, request_id=None, authorized_by=None) -> str:
    if kind not in TEAM_KINDS:
        raise _err(422, "invalid_field", f"kind must be one of {sorted(TEAM_KINDS)}")
    slug = normalize_handle(slug or name)
    if not slug:
        raise _err(422, "invalid_field", "a team needs a name or slug")
    if kind != "team" and coordinator_task_id is None:
        raise _err(422, "invalid_field", f"a {kind} needs coordinator_task_id; only kind 'team' may be taskless")
    if await conn.fetchval("SELECT 1 FROM initiatives WHERE slug = $1", slug):
        raise _err(409, "slug_taken", f"a team or initiative already has slug {slug!r}")
    if lead_member_id and not await conn.fetchval(
            "SELECT 1 FROM members WHERE id = $1 AND status <> 'retired'", lead_member_id):
        raise _err(422, "unknown_member", f"no active member {lead_member_id}")
    team_id = await conn.fetchval(
        """INSERT INTO initiatives (slug, name, kind, status, coordinator_task_id, parent_team_id,
                                    lead_member_id, charter, standing_rules, approved_at, approved_by)
           VALUES ($1, $2, $3, 'active', $4, $5, $6, $7, $8, now(), $9) RETURNING id""",
        slug, _clip(name, 200) or slug, kind, coordinator_task_id, parent_team_id,
        lead_member_id, charter or "", standing_rules or [], actor.get("pid"))
    if coordinator_task_id is not None:
        # As db.create_initiative: the coordinator is the team's anchor, and
        # a task already in another initiative would make the two ambiguous.
        attached = await conn.fetchval(
            """UPDATE tasks SET initiative_id = $2, updated_at = now()
                WHERE id = $1 AND initiative_id IS NULL RETURNING id""",
            coordinator_task_id, team_id)
        if attached is None:
            exists = await conn.fetchval("SELECT 1 FROM tasks WHERE id = $1", coordinator_task_id)
            if not exists:
                raise _err(422, "unknown_ref", f"no task {coordinator_task_id}")
            raise _err(409, "coordinator_taken",
                       f"task {coordinator_task_id} already belongs to an initiative or team")
    await _event(conn, "team", team_id, "team_created", actor,
                 new={"slug": slug, "kind": kind, "lead_member_id": lead_member_id,
                      "parent_team_id": parent_team_id,
                      "authorized_by": authorized_by or "owner"}, request_id=request_id)
    return str(team_id)


async def _set_leads(conn, actor, member_id, lead_of, request_id) -> list[str]:
    """Make `member_id` the lead of each team in `lead_of` (creating a
    board-anchored team first). A team that already has a lead keeps it."""
    notes = []
    for lo in lead_of:
        team_id = lo.get("team_id")
        if team_id is None and lo.get("create"):
            existing = await conn.fetchval(
                "SELECT id FROM initiatives WHERE coordinator_task_id = $1",
                uuid.UUID(lo["coordinator_task_id"]))
            team_id = str(existing) if existing else await _create_team(
                conn, actor, name=lo["name"], slug=lo["slug"], kind="team",
                coordinator_task_id=uuid.UUID(lo["coordinator_task_id"]), request_id=request_id)
            lo["team_id"] = team_id
        current = await conn.fetchval(
            "SELECT lead_member_id FROM initiatives WHERE id = $1", uuid.UUID(team_id))
        if current and current != member_id:
            notes.append(f"team {lo.get('slug')} already has lead {current}; unchanged")
            continue
        if current == member_id:
            continue
        await conn.execute(
            """UPDATE initiatives SET lead_member_id = $2, team_revision = team_revision + 1,
                                      updated_at = now() WHERE id = $1""",
            uuid.UUID(team_id), member_id)
        await _event(conn, "team", team_id, "lead_set", actor, prev={"lead_member_id": None},
                     new={"lead_member_id": member_id}, request_id=request_id)
    return notes


async def _create_member(conn, actor, *, handle, kind, title, manager_ref, team_id, pid,
                         home, aliases=(), continuous=None, source, request_id,
                         reason=None) -> tuple[str, list[str]]:
    """One new member, its main occupancy row, aliases, continuous mapping,
    primary team and home mark. Raises OrgError with a row-level code."""
    check_handle(handle)
    if kind not in MEMBER_KINDS:
        raise _err(422, "invalid_field", f"kind must be one of {sorted(MEMBER_KINDS)}")
    if await conn.fetchval("SELECT 1 FROM member_aliases WHERE org_id = $1 AND alias = $2",
                           ORG, handle):
        raise _err(409, "handle_taken", f"handle {handle!r} already belongs to a member (or is a retired handle)")
    manager_id = await _resolve_member_id(conn, manager_ref or OWNER_PID)
    if not manager_id:
        raise _err(422, "unknown_manager", f"no member {manager_ref!r}")
    session = None
    if pid:
        if await conn.fetchval("SELECT 1 FROM incarnations WHERE pid = $1 AND ended_at IS NULL", pid):
            raise _err(409, "pid_already_member", f"{pid} is already a member's session")
        session = await conn.fetchrow(
            """SELECT s.pid, s.daemon_id, s.session_uid, f.workspace_id
                 FROM session_states s LEFT JOIN org_session_facts f ON f.pid = s.pid
                WHERE s.pid = $1 AND s.exited_at IS NULL AND s.state <> 'exited'""", pid)
        if session is None:
            raise _err(409, "stale", f"{pid} is not a live session")
    if team_id is not None and not await conn.fetchval(
            "SELECT 1 FROM initiatives WHERE id = $1", _uuid(team_id, "team_id")):
        raise _err(422, "unknown_team", f"no team {team_id}")
    if continuous and await conn.fetchval(
            "SELECT 1 FROM continuous_members WHERE daemon_id = $1 AND continuous_id = $2",
            continuous["daemon_id"], continuous["continuous_id"]):
        raise _err(409, "stale", f"continuous task {continuous['continuous_id']} already has a member")
    member_id = new_member_id()
    now = utcnow().isoformat()
    profile = {"title": {"value": _clip(title, 200) or "", "set_by": actor.get("pid"),
                         "via": source, "set_at": now}} if title else {}
    defaults = {"home_workspace": home} if home else None
    await conn.execute(
        """INSERT INTO members (id, handle, kind, status, manager_member_id, primary_team_id,
                                occupancy_generation, profile, defaults, revision, created_by)
           VALUES ($1, $2, $3, 'active', $4, $5, $6, $7, $8, 1, $9)""",
        member_id, handle, kind, manager_id, _uuid(team_id, "team_id"),
        1 if session else 0, profile, defaults, actor.get("pid"))
    await conn.execute(
        "INSERT INTO member_aliases (org_id, alias, member_id, kind) VALUES ($1, $2, $3, 'handle')",
        ORG, handle, member_id)
    notes = []
    for raw_alias in aliases:
        alias = normalize_alias(raw_alias)
        if not alias or alias == handle:
            continue
        alias_kind = "orchestrator" if alias.endswith("-orchestrator") else "continuous_slug"
        inserted = await conn.fetchval(
            """INSERT INTO member_aliases (org_id, alias, member_id, kind) VALUES ($1, $2, $3, $4)
               ON CONFLICT DO NOTHING RETURNING alias""", ORG, alias, member_id, alias_kind)
        if not inserted:
            notes.append(f"alias {alias} belongs to another member; not added")
    if session:
        await conn.execute(
            """INSERT INTO incarnations (member_id, role, occupancy_generation, pid, daemon_id,
                                         session_uid, workspace_id)
               VALUES ($1, 'main', 1, $2, $3, $4, $5)""",
            member_id, session["pid"], session["daemon_id"], session["session_uid"],
            session["workspace_id"])
    if continuous:
        await conn.execute(
            """INSERT INTO continuous_members (daemon_id, continuous_id, member_id, planning_task_id)
               VALUES ($1, $2, $3, $4)""",
            continuous["daemon_id"], continuous["continuous_id"], member_id,
            _uuid(continuous.get("planning_task_id"), "planning_task_id"))
    if team_id is not None:
        await conn.execute(
            """INSERT INTO team_memberships (team_id, member_id, role) VALUES ($1, $2, 'primary')
               ON CONFLICT DO NOTHING""", _uuid(team_id, "team_id"), member_id)
    mark = await _home_mark(conn, member_id, home)
    await _event(conn, "member", member_id, "hire", actor, reason=reason, request_id=request_id,
                 new={"handle": handle, "kind": kind, "via": source, "manager_member_id": manager_id,
                      "primary_team_id": team_id, "pid": pid, "home_workspace": mark})
    return member_id, notes


_SOURCE_ORDER = {"continuous": 0, "lead": 1, "lane": 2}


async def roster_apply(pool, actor: dict, rows: list[dict], request_id: str | None,
                       continuous_states=()) -> dict:
    """Owner's decisions on roster proposals. Each row is applied in its own
    locked transaction against a generation made inside it, so one refusal
    (stale, handle taken, cycle) names its row and leaves the others, and a
    row sees the rows applied before it."""
    _require_owner(actor, "approving the roster")
    if not isinstance(rows, list) or not rows or len(rows) > MAX_ROWS:
        raise _err(422, "invalid_field", f"rows must hold 1-{MAX_ROWS} decisions")
    async with pool.acquire() as conn:
        stored = await _stored_request(conn, request_id, actor, "roster_apply")
        if stored is not None:
            return {**stored, "version": await version(conn)}
        # Only for ordering (continuous, then leads, then lanes); each row is
        # re-checked against a generation made under the lock.
        initial = {p["proposal_id"]: p for p in generate_roster(
            await _roster_inputs(conn, continuous_states))}
        created, updated, declined, rejected = [], [], [], []
        order = sorted(enumerate(rows), key=lambda ir: (
            _SOURCE_ORDER.get((initial.get(ir[1].get("proposal_id")) or {}).get("source"), 9), ir[0]))
        for _, row in order:
            proposal_id = row.get("proposal_id") if isinstance(row, dict) else None
            if not isinstance(proposal_id, str) or not proposal_id:
                rejected.append({"proposal_id": proposal_id, "code": "invalid_field",
                                 "message": "each row needs a proposal_id"})
                continue
            try:
                async with conn.transaction():
                    await _lock(conn)
                    if row.get("approve") is False:
                        await conn.execute(
                            """INSERT INTO org_roster_declines (proposal_id, declined_by, reason)
                               VALUES ($1, $2, $3) ON CONFLICT DO NOTHING""",
                            proposal_id, actor["pid"], _clip(row.get("reason"), 500))
                        await _event(conn, "roster", proposal_id, "roster_declined", actor,
                                     reason=row.get("reason"), request_id=request_id)
                        declined.append(proposal_id)
                        continue
                    fresh = {p["proposal_id"]: p for p in generate_roster(
                        await _roster_inputs(conn, continuous_states))}
                    proposal = fresh.get(proposal_id)
                    if proposal is None:
                        raise _err(409, "stale", "this proposal is no longer generated "
                                   "(already applied, declined, or its session or team changed)")
                    result = await _apply_proposal(conn, actor, proposal, row, request_id)
                    (updated if proposal["member_id"] else created).append(result)
            except OrgError as exc:
                rejected.append({"proposal_id": proposal_id, **exc.detail()})
            except asyncpg.PostgresError as exc:
                rejected.append({"proposal_id": proposal_id, **_db_error(exc).detail()})
        response = {"created": created, "updated": updated, "declined": declined,
                    "rejected": rejected}
        try:
            async with conn.transaction():
                await _lock(conn)
                # A concurrent call with the same request_id stored first: its
                # answer stands (this call's rows were refused as stale).
                await _replay_under_lock(conn, request_id, actor, "roster_apply")
                if created or updated or declined:
                    await conn.execute("UPDATE org_meta SET roster_applied_at = now()")
                await _store_request(conn, request_id, actor, "roster_apply", response)
        except _Replayed as replay:
            response = dict(replay.response)
        response["version"] = await version(conn)
        return response


async def _apply_proposal(conn, actor, proposal, row, request_id) -> dict:
    lead_of = [dict(lo) for lo in proposal["lead_of"]]
    if proposal["member_id"]:
        notes = await _set_leads(conn, actor, proposal["member_id"], lead_of, request_id)
        detail = await _detail(conn, proposal["member_id"])
        return {"proposal_id": proposal["proposal_id"], "member": detail["member"], "notes": notes}
    # Board-anchored teams this member leads are created before the member,
    # so it can join one as its primary team.
    for lo in lead_of:
        if lo.get("team_id") is None and lo.get("create"):
            existing = await conn.fetchval(
                "SELECT id FROM initiatives WHERE coordinator_task_id = $1",
                uuid.UUID(lo["coordinator_task_id"]))
            lo["team_id"] = str(existing) if existing else await _create_team(
                conn, actor, name=lo["name"], slug=lo["slug"], kind="team",
                coordinator_task_id=uuid.UUID(lo["coordinator_task_id"]), request_id=request_id)
    team_id = row["team_id"] if "team_id" in row else proposal["team_id"]
    if team_id is None and "team_id" not in row:
        team_id = next((lo["team_id"] for lo in lead_of if lo.get("team_id")), None)
        if team_id is None and proposal.get("team_ref"):
            team_id = await conn.fetchval(
                """SELECT i.id::text FROM initiatives i JOIN boards b ON b.root_task_id = i.coordinator_task_id
                    WHERE 'lead:board:' || b.id::text = $1""", proposal["team_ref"])
    manager = row.get("manager") or proposal["manager_member_id"] or proposal["manager_handle"]
    try:
        member_id, notes = await _create_member(
            conn, actor, handle=row.get("handle") or proposal["handle"],
            kind=row.get("kind") or proposal["kind"],
            title=row["title"] if "title" in row else proposal["title"],
            manager_ref=manager, team_id=team_id,
            pid=proposal["pids"][0] if proposal["pids"] else None,
            home=row["home_workspace"] if "home_workspace" in row else proposal["home_workspace"],
            aliases=proposal["aliases"], continuous=proposal["continuous"],
            source=f"roster:{proposal['source']}", request_id=request_id)
    except OrgError as exc:
        if exc.code == "unknown_manager" and proposal["manager_handle"] and not row.get("manager"):
            raise _err(422, "unknown_manager",
                       f"manager {proposal['manager_handle']!r} is not a member yet; "
                       "approve its proposal in the same apply or pick another manager") from None
        raise
    notes += await _set_leads(conn, actor, member_id, lead_of, request_id)
    detail = await _detail(conn, member_id)
    return {"proposal_id": proposal["proposal_id"], "member": detail["member"], "notes": notes}


# ---- adoption, moves, teams -------------------------------------------------------

async def adopt(pool, actor: dict, *, pid: str, handle: str, title: str | None = None,
                kind: str = "agent", team_id=None, manager: str | None = None,
                home_workspace: dict | None = None, request_id: str | None = None) -> dict:
    """Owner binds a live session to a new member (after the roster)."""
    _require_owner(actor, "adopting a session")
    async with pool.acquire() as conn:
        stored = await _stored_request(conn, request_id, actor, "adopt")
        if stored is not None:
            return {**stored, "version": await version(conn)}
        try:
            async with conn.transaction():
                await _lock(conn)
                await _replay_under_lock(conn, request_id, actor, "adopt")
                facts = await conn.fetchrow(
                    """SELECT s.daemon_id, s.session_uid, s.engine, f.workspace_id, f.worktree_path,
                              f.continuous_task_id
                         FROM session_states s LEFT JOIN org_session_facts f ON f.pid = s.pid
                        WHERE s.pid = $1 AND s.exited_at IS NULL AND s.state <> 'exited'""", pid)
                if facts is None:
                    raise _err(404, "not_found", f"{pid} is not a live session")
                if facts["engine"] == "bash":
                    raise _err(422, "agent_only", "adopt requires an agent session; bash is a shell")
                continuous = None
                aliases = []
                if kind == "continuous":
                    cid = facts["continuous_task_id"]
                    if not cid:
                        raise _err(422, "invalid_field", f"{pid} is not a continuous orchestrator")
                    continuous = {"daemon_id": facts["daemon_id"], "continuous_id": cid,
                                  "planning_task_id": None}
                    aliases = [cid, f"{cid}-orchestrator"]
                home = home_workspace or _home_of({**dict(facts), "pid": pid})
                member_id, notes = await _create_member(
                    conn, actor, handle=handle, kind=kind, title=title, manager_ref=manager,
                    team_id=team_id, pid=pid, home=home, aliases=aliases, continuous=continuous,
                    source="adopt", request_id=request_id)
                detail = await _detail(conn, member_id)
                home_mark = detail["member"]["home_workspace"]
                response = {"member": detail["member"], "notes": notes,
                            "home_mark": {"state": home_mark["state"]} if home_mark else None}
                await _store_request(conn, request_id, actor, "adopt", response)
        except _Replayed as replay:
            response = dict(replay.response)
        except asyncpg.PostgresError as exc:
            raise _db_error(exc) from None
        response["version"] = await version(conn)
        return response


async def _require_chain(conn, actor: dict, member_id: str, what: str, *,
                         self_ok: bool = False) -> str | None:
    """Owner, or an active member up `member_id`'s manager chain acting as its
    main session (doc/org-model.md §10: move / retire a member). With
    `self_ok`, also the member's own main session (closing its own assistant,
    §4.3). The one authority check for move, retire and close_assistant.
    Returns the acting member (None for Owner)."""
    if _is_owner(actor):
        return None
    actor_member = await conn.fetchval(
        """SELECT i.member_id FROM incarnations i JOIN members m ON m.id = i.member_id
            WHERE i.pid = $1 AND i.role = 'main' AND i.ended_at IS NULL
              AND m.status = 'active'""", actor.get("pid"))
    if not actor_member:
        raise _err(403, "not_member",
                   f"only Owner or a member up this member's manager chain may {what} it; "
                   "this session is not an active member's main session")
    if self_ok and actor_member == member_id:
        return actor_member
    chain = [m["id"] for m in await _chain(conn, member_id)]
    if actor_member not in chain:
        raise _err(403, "not_manager",
                   f"only Owner or a member up this member's manager chain may {what} it")
    return actor_member


async def authority(pool, actor: dict, ref: str, *, self_ok: bool = False,
                    session: str | None = None) -> dict:
    """Whether `actor` may control member `ref`'s sessions (a lead
    rotating or handing off a report's session): exactly `_require_chain`,
    the check for move, retire and close_assistant. With `session` (a pid),
    that session must also be the member's: its open main row, or an
    assistant/worker its daemon reported for it. Read-only."""
    async with pool.acquire() as conn:
        member_id = await _resolve_member_id(conn, ref)
        if not member_id:
            raise _err(404, "not_found", f"no member {ref!r}")
        if session is not None and not await conn.fetchval(
                """SELECT 1 FROM incarnations
                    WHERE pid = $1 AND member_id = $2 AND role = 'main' AND ended_at IS NULL
                   UNION ALL
                   SELECT 1 FROM org_session_facts
                    WHERE pid = $1 AND member_id = $2 AND acting_as IN ('assistant', 'worker')
                   LIMIT 1""", session, member_id):
            raise _err(403, "not_member_session",
                       f"{session} is not a session of member {member_id}")
        acting = await _require_chain(conn, actor, member_id, "control", self_ok=self_ok)
        return {"allowed": True, "member_id": member_id, "acting_member": acting}


async def authority_batch(pool, actor: dict, targets: list[dict], *, self_ok: bool = True) -> dict:
    """Return the member ids in ``targets`` that ``actor`` may read.

    This is the one-request read companion to :func:`authority`.  Each target
    is checked with the same ``_require_chain`` manager rule; a target that is
    unknown, stale or outside the chain is omitted while the rest of the
    batch remains useful.  ``session`` is optional for list callers and, when
    present, gets the same member-session check as the single-target route.
    """
    allowed: list[dict] = []
    seen: set[tuple[str, str | None]] = set()
    async with pool.acquire() as conn:
        for target in targets:
            if not isinstance(target, dict):
                continue
            ref = target.get("member")
            session = target.get("session")
            key = (str(ref), session if isinstance(session, str) else None)
            if key in seen:
                continue
            seen.add(key)
            member_id = await _resolve_member_id(conn, ref)
            if not member_id:
                continue
            if session is not None and not await conn.fetchval(
                    """SELECT 1 FROM incarnations
                        WHERE pid = $1 AND member_id = $2 AND role = 'main' AND ended_at IS NULL
                       UNION ALL
                       SELECT 1 FROM org_session_facts
                        WHERE pid = $1 AND member_id = $2 AND acting_as IN ('assistant', 'worker')
                       LIMIT 1""", session, member_id):
                continue
            try:
                acting = await _require_chain(conn, actor, member_id, "control", self_ok=self_ok)
            except OrgError:
                continue
            allowed.append({"member": ref, "member_id": member_id,
                            "session": session, "acting_member": acting})
    return {"allowed": allowed}


async def _require_destination(conn, actor_member: str | None, manager_id: str, what: str) -> None:
    """A non-Owner may place a member only under itself or a member of its
    own reporting subtree; anywhere else (another lead's branch, Owner) is
    Owner's call until an accept step exists. Shared by move and promote."""
    if actor_member is None or manager_id == actor_member:
        return
    if actor_member not in [m["id"] for m in await _chain(conn, manager_id)]:
        raise _err(403, "needs_owner",
                   f"a lead may {what} only under itself or a member of its own subtree; "
                   "anywhere else is Owner's call")


async def _main_pid(conn, member_id: str | None) -> str | None:
    """The member's current main session's pid, if it has one."""
    if not member_id:
        return None
    return await conn.fetchval(
        """SELECT pid FROM incarnations
            WHERE member_id = $1 AND role = 'main' AND ended_at IS NULL""", member_id)


async def hand_back_holds(conn, actor: dict, from_pid: str, to_pid: str | None, *,
                          reason: str) -> dict:
    """Hand every open board item `from_pid` holds to `to_pid` in its role
    (an owner's hold transfers ownership; the receiver keeps a role it
    already has). Call inside a transaction that holds the task-change lock;
    each board is locked and written through the item rules, so holder
    history, mirrors and pushes behave as for any reassignment. With no
    `to_pid` (a vacant member) nothing moves and the holds are listed as
    `kept`. Shared by close_assistant and retire."""
    from dispatch import items_db
    from dispatch.items_rules import BoardTx

    boards = [r["board_id"] for r in await conn.fetch(
        """SELECT DISTINCT i.board_id FROM item_holders h JOIN items i ON i.id = h.item_id
            WHERE h.pid = $1 AND i.closed_at IS NULL AND i.archived_at IS NULL
              AND i.link_archived_at IS NULL ORDER BY i.board_id""", from_pid)]
    handed, kept = [], []
    to = None
    if to_pid:
        state = await conn.fetchrow(
            "SELECT name, session_uid, daemon_id FROM session_states WHERE pid = $1", to_pid)
        uid = to_pid.rsplit(":", 1)[-1]
        daemon = to_pid.split(":")[1] if to_pid.count(":") >= 2 else None
        to = {"pid": to_pid, "name": state["name"] if state else None,
              "session_uid": state["session_uid"] if state else uid,
              "daemon_id": state["daemon_id"] if state else daemon}
    from dispatch.items_rules import ItemsError as _ItemsError

    now = utcnow()
    for board_id in boards:
        board = await items_db._board_by_ref(conn, str(board_id), lock=True)
        items = await items_db._load_items(conn, board["id"])
        held = [(n, h.get("role")) for n, it in sorted(items.items())
                if it.closed_at is None and it.archived_at is None and it.link_archived_at is None
                for h in it.holders if h["pid"] == from_pid]
        if to is None:
            kept += [{"board": board["slug"], "n": n, "role": role} for n, role in held]
            continue
        groups = await items_db._load_groups(conn, board["id"])
        for n, role in held:
            # One savepoint per item: an item the rules refuse (say, blocked
            # on Owner) keeps its holder and the others still move.
            try:
                async with conn.transaction():
                    tx = BoardTx(board, await items_db._load_items(conn, board["id"], [n]),
                                 actor, now, groups)
                    if role == "owner":
                        tx.apply(n, {}, remove=[from_pid], owner=dict(to), reason=reason)
                    else:
                        tx.apply(n, {}, add=[{**to, "role": role}] if role else [dict(to)],
                                 remove=[from_pid], reason=reason)
                    await items_db._apply(conn, tx, mirror=True)
            except _ItemsError as exc:
                kept.append({"board": board["slug"], "n": n, "role": role, "error": exc.detail()})
                continue
            handed.append({"board": board["slug"], "n": n, "role": role, "to": to_pid})
    return {"handed_back": handed, "kept": kept}


async def close_assistant(pool, actor: dict, *, pid: str, member_id: str, reason: str,
                          ended_by: str | None = None, request_id: str | None = None) -> dict:
    """Deliberately close one of a member's assistants (doc/org-model.md
    §4.3, O5): Owner, the member's main session or its manager chain. Its
    open holds go back to the member's current main session; the daemon
    then closes the session. Recorded as an `org_event`. With
    `ended_by="kill_session"` the daemon already killed it under
    kill_session's scope: no authority check, the holds still go back."""
    if not reason or not str(reason).strip():
        raise _err(422, "invalid_field", "closing an assistant needs a reason")
    async with pool.acquire() as conn:
        stored = await _stored_request(conn, request_id, actor, "close_assistant")
        if stored is not None:
            return stored
        try:
            async with conn.transaction():
                await _lock(conn)
                await _replay_under_lock(conn, request_id, actor, "close_assistant")
                member = await conn.fetchrow(
                    "SELECT id, status FROM members WHERE id = $1", member_id)
                if member is None:
                    raise _err(404, "not_found", f"no member {member_id}")
                if await conn.fetchval(
                        """SELECT 1 FROM incarnations
                            WHERE pid = $1 AND role = 'main' AND ended_at IS NULL""", pid):
                    raise _err(409, "not_an_assistant",
                               f"{pid} is a member's main session; replace or retire the member")
                # The session's daemon must have reported it as THIS member's
                # assistant: a caller cannot name another member to take an
                # assistant's holds (with or without ended_by).
                facts = await conn.fetchrow(
                    "SELECT member_id, acting_as FROM org_session_facts WHERE pid = $1", pid)
                if facts and facts["acting_as"] == "worker":
                    raise _err(409, "not_an_assistant",
                               f"{pid} is a continuous member's worker; kill_session it instead")
                if not facts or facts["acting_as"] not in ("assistant", "closed") \
                        or facts["member_id"] != member_id:
                    raise _err(409, "not_an_assistant",
                               f"{pid} is not reported as an assistant of {member_id} (its daemon "
                               "reports assistants on each heartbeat; retry after the next one)",
                               reported_member_id=facts["member_id"] if facts else None)
                if ended_by != "kill_session":
                    await _require_chain(conn, actor, member_id, "close an assistant of",
                                         self_ok=True)
                main_pid = await _main_pid(conn, member_id)
                holds = await hand_back_holds(conn, actor, pid, main_pid,
                                              reason=f"assistant closed: {reason}")
                await conn.execute(
                    "UPDATE org_session_facts SET acting_as = 'closed' WHERE pid = $1", pid)
                await _event(conn, "member", member_id,
                             "assistant_killed" if ended_by else "close_assistant", actor,
                             prev={"assistant": pid}, new={"holds": holds},
                             reason=reason, request_id=request_id)
                response = {"pid": pid, "member_id": member_id, "main_pid": main_pid,
                            "holds": holds}
                await _store_request(conn, request_id, actor, "close_assistant", response)
        except _Replayed as replay:
            response = replay.response
        except asyncpg.PostgresError as exc:
            raise _db_error(exc) from None
        return response


async def _promote_home(conn, facts, pid: str, boss: str, refused: str | None):
    """The checkout a promoted assistant may take as its new member's home,
    or why not: `(home, None)` or `(None, reason)`.

    Only a checkout that is no one else's: no central `home` mark, no
    `stream` mark owned by another member than `boss`, no other member's
    live main session in it, and no refusal from the owning daemon (which
    knows the local kind: only an unowned or boss-owned `stream` /
    `ephemeral` checkout qualifies there). Otherwise a plain spawn in its
    member's own checkout would take that member's home away."""
    home = _home_of({**dict(facts), "pid": pid})
    if home is None:
        return None, "no_checkout"
    if refused:
        return None, refused
    mark = await conn.fetchrow(
        """SELECT kind, member_id FROM org_workspace_marks
            WHERE daemon_id = $1 AND workspace_id = $2""", home["daemon_id"], home["workspace_id"])
    if mark is not None and (mark["kind"] == "home" or (mark["member_id"] and mark["member_id"] != boss)):
        return None, "shared_checkout"
    other_main = await conn.fetchval(
        """SELECT 1 FROM org_session_facts f
             JOIN session_states s ON s.pid = f.pid AND s.exited_at IS NULL AND s.state <> 'exited'
             JOIN incarnations i ON i.pid = f.pid AND i.ended_at IS NULL
            WHERE f.pid <> $1 AND s.daemon_id = $2
              AND (f.workspace_id = $3 OR ($4::text IS NOT NULL AND f.worktree_path = $4))
            LIMIT 1""", pid, home["daemon_id"], home["workspace_id"], home.get("path"))
    if other_main:
        return None, "shared_checkout"
    return home, None


async def promote(pool, actor: dict, *, pid: str, handle: str, title: str | None = None,
                  team=None, manager: str | None = None, reason: str,
                  home_refused: str | None = None, request_id: str | None = None) -> dict:
    """Promote a member's assistant to a new member (doc/org-model.md §4.2):
    the session becomes the main session of a new member managed by the
    assistant's member M, keeping its session, uid, context, checkout and
    holds; its checkout becomes the new member's `home` when it is no one
    else's (`_promote_home`), else the member has no home and the result
    says why (`home: null, home_reason: "shared_checkout"`).

    Allowed for M's main session, a member up M's manager chain acting as its
    main, or Owner (`_require_chain(..., self_ok=True)`). `manager` defaults
    to M; a non-Owner may name only itself or a member of its own subtree
    (`needs_owner`, as for move), and may set `team` only on a team it leads.
    Refused: a session that is not an agent (`not_an_agent`), already a
    member's session (`already_member`), a continuous orchestrator
    (`continuous`), or not an assistant (`not_an_assistant`). The member is
    created by `_create_member`, the path adopt and the roster use."""
    if not reason or not str(reason).strip():
        raise _err(422, "invalid_field", "promoting an assistant needs a reason")

    async def body(conn):
        facts = await conn.fetchrow(
            """SELECT s.daemon_id, s.session_uid, s.engine, f.workspace_id, f.worktree_path,
                      f.continuous_task_id, f.member_id, f.acting_as
                 FROM session_states s LEFT JOIN org_session_facts f ON f.pid = s.pid
                WHERE s.pid = $1 AND s.exited_at IS NULL AND s.state <> 'exited'""", pid)
        if facts is None:
            raise _err(404, "not_found", f"{pid} is not a live session")
        if facts["engine"] not in (None, "claude-code", "claude", "codex"):
            raise _err(409, "not_an_agent", f"{pid} is a {facts['engine']} session, not an agent")
        holder = await conn.fetchval(
            """SELECT m.handle FROM incarnations i JOIN members m ON m.id = i.member_id
                WHERE i.pid = $1 AND i.ended_at IS NULL""", pid)
        if holder:
            raise _err(409, "already_member", f"{pid} is already {holder}'s main session")
        if facts["continuous_task_id"]:
            raise _err(409, "continuous", f"{pid} is a continuous orchestrator; its scheduler owns it")
        boss = facts["member_id"] if facts["acting_as"] == "assistant" else None
        boss_row = await conn.fetchrow("SELECT id, status FROM members WHERE id = $1", boss) if boss else None
        if boss_row is None or boss_row["status"] == "retired":
            raise _err(409, "not_an_assistant", f"{pid} is not an assistant of a member")
        actor_member = await _require_chain(conn, actor, boss, "promote an assistant of", self_ok=True)
        manager_id = boss
        if manager is not None:
            manager_id = await _resolve_member_id(conn, manager)
            if not manager_id:
                raise _err(422, "unknown_manager", f"no member {manager!r}")
            await _require_destination(conn, actor_member, manager_id, "place a promoted member")
        team_id = None
        if team is not None and str(team).strip():
            try:
                team_row = await _team_row(conn, team)
            except OrgError:
                raise _err(422, "unknown_team", f"no team {team}") from None
            if actor_member is not None and team_row["lead_member_id"] != actor_member:
                raise _err(403, "needs_owner", "a lead may give a promoted member only a team it leads")
            team_id = str(team_row["id"])
        home, home_reason = await _promote_home(conn, facts, pid, boss, home_refused)
        member_id, notes = await _create_member(
            conn, actor, handle=handle, kind="agent", title=title, manager_ref=manager_id,
            team_id=team_id, pid=pid, home=home, source="promote", request_id=request_id,
            reason=reason)
        # No longer an assistant: sticky, so a stale beat cannot restore it.
        await conn.execute(
            "UPDATE org_session_facts SET acting_as = 'promoted' WHERE pid = $1", pid)
        await _event(conn, "member", boss, "assistant_promoted", actor,
                     prev={"assistant": pid}, new={"member_id": member_id, "handle": handle},
                     reason=reason, request_id=request_id)
        detail = await _detail(conn, member_id)
        home_mark = detail["member"]["home_workspace"]
        return {"member": detail["member"], "promoted_from": boss, "notes": notes,
                "home": home, "home_reason": home_reason,
                "home_mark": {"state": home_mark["state"]} if home_mark else None}

    response = await _member_write(pool, actor, request_id, "promote", body)
    async with pool.acquire() as conn:
        return {**response, "version": await version(conn)}


async def move(pool, actor: dict, ref: str, *, reason: str, manager=None, primary_team_id=None,
               fields: set[str], expected_revision: int | None = None,
               request_id: str | None = None) -> dict:
    """Change a member's manager and/or primary team: Owner, or a member up
    the target's current manager chain (acting as its main session)."""
    if not reason or not str(reason).strip():
        raise _err(422, "invalid_field", "a move needs a reason")
    if not fields & {"manager", "primary_team_id"}:
        raise _err(422, "invalid_field", "pass manager and/or primary_team_id")
    async with pool.acquire() as conn:
        stored = await _stored_request(conn, request_id, actor, "move")
        if stored is not None:
            return stored
        try:
            async with conn.transaction():
                await _lock(conn)
                await _replay_under_lock(conn, request_id, actor, "move")
                member_id = await _resolve_member_id(conn, ref)
                if not member_id or member_id == OWNER_ID:
                    raise _err(404, "not_found", f"no movable member {ref!r}")
                row = await conn.fetchrow("SELECT * FROM members WHERE id = $1", member_id)
                actor_member = await _require_chain(conn, actor, member_id, "move")
                if expected_revision is not None and row["revision"] != expected_revision:
                    raise _err(409, "revision_conflict", "the member changed; re-read it",
                               current_revision=row["revision"])
                prev, new = {}, {}
                if "manager" in fields:
                    manager_id = await _resolve_member_id(conn, manager)
                    if not manager_id:
                        raise _err(422, "unknown_manager", f"no member {manager!r}")
                    await _require_destination(conn, actor_member, manager_id, "move a report")
                    prev["manager_member_id"], new["manager_member_id"] = row["manager_member_id"], manager_id
                if "primary_team_id" in fields:
                    team = None
                    if primary_team_id is not None:
                        try:
                            team = (await _team_row(conn, primary_team_id))["id"]
                        except OrgError:
                            raise _err(422, "unknown_team", f"no team {primary_team_id}") from None
                    prev["primary_team_id"], new["primary_team_id"] = row["primary_team_id"], team
                # Only the columns that move: a team-only move must not fire
                # the manager-chain trigger (UPDATE OF manager_member_id).
                cols = [c for c in ("manager_member_id", "primary_team_id") if c in new]
                assign = ", ".join(f"{c} = ${n + 2}" for n, c in enumerate(cols))
                await conn.execute(
                    f"""UPDATE members SET {assign}, revision = revision + 1, updated_at = now()
                         WHERE id = $1""",
                    member_id, *[new[c] for c in cols])
                if "primary_team_id" in new:
                    await conn.execute(
                        """UPDATE team_memberships SET until = now()
                            WHERE member_id = $1 AND role = 'primary' AND until IS NULL
                              AND team_id IS DISTINCT FROM $2""", member_id, new["primary_team_id"])
                    if new["primary_team_id"] is not None:
                        await conn.execute(
                            """INSERT INTO team_memberships (team_id, member_id, role)
                               VALUES ($1, $2, 'primary')
                               ON CONFLICT (team_id, member_id, role)
                               DO UPDATE SET until = NULL, since = now()""",
                            new["primary_team_id"], member_id)
                await _event(conn, "member", member_id, "move", actor, prev=prev, new=new,
                             reason=reason, request_id=request_id)
                response = await _detail(conn, member_id)
                await _store_request(conn, request_id, actor, "move", response)
        except _Replayed as replay:
            response = replay.response
        except asyncpg.PostgresError as exc:
            raise _db_error(exc) from None
        return response


async def _member_write(pool, actor, request_id, method, body) -> dict:
    """Run `body(conn)` in one locked transaction, idempotent per request id."""
    async with pool.acquire() as conn:
        stored = await _stored_request(conn, request_id, actor, method)
        if stored is not None:
            return stored
        try:
            async with conn.transaction():
                await _lock(conn)
                await _replay_under_lock(conn, request_id, actor, method)
                response = await body(conn)
                await _store_request(conn, request_id, actor, method, response)
        except _Replayed as replay:
            response = replay.response
        except asyncpg.PostgresError as exc:
            raise _db_error(exc) from None
        return response


async def retire(pool, actor: dict, ref: str, *, reason: str, request_id: str | None = None) -> dict:
    """Mark a member retired: Owner, or an active member up its manager chain.

    Its reports move to its manager and the teams it leads lose their lead
    (both recorded on the event, so a reinstate can give the leads back).
    Its sessions are never touched: the main session keeps running and keeps
    its occupancy row. The handle stays reserved. Owner can reverse it."""
    if not reason or not str(reason).strip():
        raise _err(422, "invalid_field", "retiring a member needs a reason")

    async def body(conn):
        member_id = await _resolve_member_id(conn, ref)
        if not member_id or member_id == OWNER_ID:
            raise _err(404, "not_found", f"no retirable member {ref!r}")
        row = await conn.fetchrow("SELECT * FROM members WHERE id = $1", member_id)
        await _require_chain(conn, actor, member_id, "retire")
        if row["status"] == "retired":
            raise _err(409, "already_retired", f"{row['handle']} is already retired")
        reports = [r["id"] for r in await conn.fetch(
            """SELECT id FROM members WHERE manager_member_id = $1 AND status <> 'retired'
                ORDER BY id""", member_id)]
        for report in reports:
            await conn.execute(
                """UPDATE members SET manager_member_id = $2, revision = revision + 1,
                                      updated_at = now() WHERE id = $1""",
                report, row["manager_member_id"])
            await _event(conn, "member", report, "move", actor,
                         prev={"manager_member_id": member_id},
                         new={"manager_member_id": row["manager_member_id"], "via": "retire"},
                         reason=f"manager {row['handle']} retired: {reason}", request_id=request_id)
        led = [str(r["id"]) for r in await conn.fetch(
            "SELECT id FROM initiatives WHERE lead_member_id = $1 ORDER BY id", member_id)]
        for team_id in led:
            await conn.execute(
                """UPDATE initiatives SET lead_member_id = NULL, team_revision = team_revision + 1,
                                          updated_at = now() WHERE id = $1""", uuid.UUID(team_id))
            await _event(conn, "team", team_id, "lead_set", actor, prev={"lead_member_id": member_id},
                         new={"lead_member_id": None, "via": "retire"}, request_id=request_id)
        # Its main session's open holds go back to its manager (§4.3): the
        # manager's main session, or Owner; a vacant manager's stay, listed.
        main_pid = await _main_pid(conn, member_id)
        holds = {"handed_back": [], "kept": []}
        if main_pid:
            to_pid = (OWNER_PID if row["manager_member_id"] == OWNER_ID
                      else await _main_pid(conn, row["manager_member_id"]))
            holds = await hand_back_holds(conn, actor, main_pid, to_pid,
                                          reason=f"{row['handle']} retired: {reason}")
        await conn.execute(
            """UPDATE members SET status = 'retired', retired_at = now(), revision = revision + 1,
                                  updated_at = now() WHERE id = $1""", member_id)
        await _event(conn, "member", member_id, "retire", actor,
                     prev={"status": row["status"], "reports": reports, "lead_of": led},
                     new={"status": "retired", "reports_to": row["manager_member_id"]},
                     reason=reason, request_id=request_id)
        detail = await _detail(conn, member_id)
        return {**detail, "reports_moved": reports, "teams_unled": led, "holds": holds}

    return await _member_write(pool, actor, request_id, "retire", body)


async def reinstate(pool, actor: dict, ref: str, *, reason: str | None = None,
                    request_id: str | None = None) -> dict:
    """Owner reverses a retirement: the member gets back its pre-retire status
    and the leads of teams that have had none since. Reports that moved up stay
    where they are (move them back by hand)."""
    _require_owner(actor, "reinstating a member")

    async def body(conn):
        member_id = await _resolve_member_id(conn, ref)
        if not member_id or member_id == OWNER_ID:
            raise _err(404, "not_found", f"no member {ref!r}")
        row = await conn.fetchrow("SELECT * FROM members WHERE id = $1", member_id)
        if row["status"] != "retired":
            raise _err(409, "not_retired", f"{row['handle']} is not retired")
        if await conn.fetchval("SELECT status = 'retired' FROM members WHERE id = $1",
                               row["manager_member_id"]):
            raise _err(409, "retired_manager", f"{row['handle']}'s manager is retired; "
                       "reinstate it first, or move this member after reinstating")
        last = await conn.fetchval(
            """SELECT prev FROM org_events WHERE subject_type = 'member' AND subject_id = $1
                 AND type = 'retire' ORDER BY id DESC LIMIT 1""", member_id)
        restored = []
        for team_id in (last or {}).get("lead_of") or []:
            done = await conn.fetchval(
                """UPDATE initiatives SET lead_member_id = $2, team_revision = team_revision + 1,
                                          updated_at = now()
                    WHERE id = $1 AND lead_member_id IS NULL RETURNING id""",
                uuid.UUID(team_id), member_id)
            if done:
                restored.append(team_id)
                await _event(conn, "team", team_id, "lead_set", actor, prev={"lead_member_id": None},
                             new={"lead_member_id": member_id, "via": "reinstate"},
                             request_id=request_id)
        # Back to the status it had before the retire: a vacant or suspended
        # member does not come back active with a dead main session.
        status = (last or {}).get("status")
        if status not in ("active", "vacant", "suspended"):
            status = "active"
        await conn.execute(
            """UPDATE members SET status = $2, retired_at = NULL, revision = revision + 1,
                                  updated_at = now() WHERE id = $1""", member_id, status)
        await _event(conn, "member", member_id, "reinstate", actor, prev={"status": "retired"},
                     new={"status": status, "lead_of": restored}, reason=reason,
                     request_id=request_id)
        detail = await _detail(conn, member_id)
        return {**detail, "teams_led_again": restored}

    return await _member_write(pool, actor, request_id, "reinstate", body)


async def team_create(pool, actor: dict, *, name: str, slug=None, kind="team", parent_team_id=None,
                      lead=None, charter="", standing_rules=None, coordinator_task_id=None,
                      request_id=None) -> dict:
    _require_owner(actor, "creating a team")
    async with pool.acquire() as conn:
        stored = await _stored_request(conn, request_id, actor, "team_create")
        if stored is not None:
            return stored
        try:
            async with conn.transaction():
                await _lock(conn)
                await _replay_under_lock(conn, request_id, actor, "team_create")
                lead_id = None
                if lead:
                    lead_id = await _resolve_member_id(conn, lead)
                    if not lead_id:
                        raise _err(422, "unknown_member", f"no member {lead!r}")
                parent = _uuid(parent_team_id, "parent_team_id")
                if parent is not None and not await conn.fetchval(
                        "SELECT 1 FROM initiatives WHERE id = $1", parent):
                    raise _err(422, "unknown_team", f"no team {parent_team_id}")
                team_id = await _create_team(
                    conn, actor, name=name, slug=slug, kind=kind, parent_team_id=parent,
                    lead_member_id=lead_id, charter=charter, standing_rules=standing_rules,
                    coordinator_task_id=_uuid(coordinator_task_id, "coordinator_task_id"),
                    request_id=request_id)
                response = (await _teams(conn, "WHERE i.id = $1", uuid.UUID(team_id)))[0]
                await _store_request(conn, request_id, actor, "team_create", response)
        except _Replayed as replay:
            response = replay.response
        except asyncpg.PostgresError as exc:
            raise _db_error(exc) from None
        return response


async def create_team_conn(conn, actor: dict, *, name: str, slug=None, kind="team",
                           parent_team_id=None, lead=None, charter="", standing_rules=None,
                           coordinator_task_id=None, authorized_by: str,
                           request_id=None) -> str:
    """Create a team inside the caller's transaction, for a trusted in-process
    caller that has already checked authority (the S3 epic flow).
    `authorized_by` names that check and is recorded on the org event."""
    if not authorized_by:
        raise _err(422, "invalid_field", "authorized_by names the check the caller made")
    await _lock(conn)
    lead_id = None
    if lead:
        lead_id = await _resolve_member_id(conn, lead)
        if not lead_id:
            raise _err(422, "unknown_member", f"no member {lead!r}")
    try:
        async with conn.transaction():
            team_id = await _create_team(
                conn, actor, name=name, slug=slug, kind=kind,
                parent_team_id=_uuid(parent_team_id, "parent_team_id"), lead_member_id=lead_id,
                charter=charter, standing_rules=standing_rules,
                coordinator_task_id=_uuid(coordinator_task_id, "coordinator_task_id"),
                request_id=request_id, authorized_by=authorized_by)
    except asyncpg.PostgresError as exc:
        raise _db_error(exc) from None
    return team_id


TEAM_FIELDS = {"name", "parent_team_id", "lead", "charter", "standing_rules", "status"}


async def team_update(pool, actor: dict, ref: str, changes: dict, *, reason=None,
                      expected_revision=None, request_id=None) -> dict:
    _require_owner(actor, "editing a team")
    unknown = set(changes) - TEAM_FIELDS
    if unknown or not changes:
        raise _err(422, "invalid_field", f"editable team fields: {sorted(TEAM_FIELDS)}")
    async with pool.acquire() as conn:
        stored = await _stored_request(conn, request_id, actor, "team_update")
        if stored is not None:
            return stored
        try:
            async with conn.transaction():
                await _lock(conn)
                await _replay_under_lock(conn, request_id, actor, "team_update")
                team = await _team_row(conn, ref)
                if expected_revision is not None and team["team_revision"] != expected_revision:
                    raise _err(409, "revision_conflict", "the team changed; re-read it",
                               current_revision=team["team_revision"])
                sets: dict = {}
                if "name" in changes:
                    sets["name"] = _clip(changes["name"], 200) or team["name"]
                if "charter" in changes:
                    sets["charter"] = str(changes["charter"] or "")[:20000]
                if "standing_rules" in changes:
                    if not isinstance(changes["standing_rules"], list):
                        raise _err(422, "invalid_field", "standing_rules must be a list")
                    sets["standing_rules"] = changes["standing_rules"]
                if "status" in changes:
                    if changes["status"] not in TEAM_STATUSES:
                        raise _err(422, "invalid_field", f"status must be one of {sorted(TEAM_STATUSES)}")
                    sets["team_status"] = changes["status"]
                if "parent_team_id" in changes:
                    parent = _uuid(changes["parent_team_id"], "parent_team_id")
                    if parent is not None and not await conn.fetchval(
                            "SELECT 1 FROM initiatives WHERE id = $1", parent):
                        raise _err(422, "unknown_team", f"no team {changes['parent_team_id']}")
                    sets["parent_team_id"] = parent
                if "lead" in changes:
                    lead_id = None
                    if changes["lead"]:
                        lead_id = await _resolve_member_id(conn, changes["lead"])
                        if lead_id and await conn.fetchval(
                                "SELECT status = 'retired' FROM members WHERE id = $1", lead_id):
                            raise _err(409, "invalid_member", f"{changes['lead']!r} is retired")
                        if not lead_id:
                            raise _err(422, "unknown_member", f"no member {changes['lead']!r}")
                    sets["lead_member_id"] = lead_id
                keys = list(sets)
                assign = ", ".join(f"{k} = ${n + 2}" for n, k in enumerate(keys))
                await conn.execute(
                    f"""UPDATE initiatives SET {assign}, team_revision = team_revision + 1,
                                               updated_at = now() WHERE id = $1""",
                    team["id"], *[sets[k] for k in keys])
                await _event(conn, "team", team["id"], "team_updated", actor,
                             prev={k: team[k] for k in keys}, new=sets, reason=reason,
                             request_id=request_id)
                response = (await _teams(conn, "WHERE i.id = $1", team["id"]))[0]
                await _store_request(conn, request_id, actor, "team_update", response)
        except _Replayed as replay:
            response = replay.response
        except asyncpg.PostgresError as exc:
            raise _db_error(exc) from None
        return response


# ---- heartbeat ---------------------------------------------------------------

async def heartbeat_extras(pool, daemon_id: str, sessions: list, *, capabilities=None,
                           acked_mark_ids=()) -> dict:
    """The org half of a daemon heartbeat: record per-session facts, record
    capabilities, confirm acked home marks, and return
    `{members: {session_uid: member summary}, workspace_marks: [...]}` for
    this daemon. Marks are handed out only to a daemon advertising
    org_marks_v1; older daemons ignore the whole `org` reply key. A beat
    without `capabilities` (a brain from before S0b) records none, so a
    rollback never leaves a host marked capable."""
    prefix = f"agent:{daemon_id}:"
    now = utcnow()
    facts = []
    for raw in sessions or []:
        if not isinstance(raw, dict):
            continue
        pid = raw.get("pid")
        if not isinstance(pid, str) or not pid.startswith(prefix) or len(pid) > 300:
            continue
        uid = _clip(raw.get("session_uid"), 200)
        if not uid:
            continue
        age = raw.get("spawned_age_s")
        spawned = None
        if isinstance(age, (int, float)) and 0 <= age < 10 * 365 * 86400:
            spawned = now - timedelta(seconds=float(age))
        facts.append((pid, uid, _clip(raw.get("workspace_id"), 200),
                      _clip(raw.get("worktree_path"), 1000),
                      _clip(raw.get("continuous_task_id"), 200),
                      _clip(raw.get("managed_by_uid"), 200),
                      _clip(raw.get("workflow_run_id"), 200), spawned,
                      _clip(raw.get("member_id"), 64) if raw.get("acting_as") in STAMPED_ROLES else None,
                      raw.get("acting_as") if raw.get("acting_as") in STAMPED_ROLES else None))
    async with pool.acquire() as conn:
        async with conn.transaction():
            for f in facts:
                await conn.execute(
                    """INSERT INTO org_session_facts (pid, daemon_id, session_uid, workspace_id,
                                                      worktree_path, continuous_task_id,
                                                      managed_by_uid, workflow_run_id, spawned_at,
                                                      member_id, acting_as, reported_at)
                       VALUES ($1, $12, $2, $3, $4, $5, $6, $7, $8, $9,
                               CASE WHEN $9::text IS NULL THEN NULL ELSE $10 END, $11)
                       ON CONFLICT (pid) DO UPDATE SET
                           member_id = coalesce(EXCLUDED.member_id, org_session_facts.member_id),
                           acting_as = CASE WHEN org_session_facts.acting_as IN ('closed', 'promoted')
                                            THEN org_session_facts.acting_as
                                            ELSE coalesce(EXCLUDED.acting_as, org_session_facts.acting_as) END,
                           session_uid = EXCLUDED.session_uid, workspace_id = EXCLUDED.workspace_id,
                           worktree_path = EXCLUDED.worktree_path,
                           continuous_task_id = EXCLUDED.continuous_task_id,
                           managed_by_uid = EXCLUDED.managed_by_uid,
                           workflow_run_id = EXCLUDED.workflow_run_id,
                           spawned_at = coalesce(org_session_facts.spawned_at, EXCLUDED.spawned_at),
                           reported_at = EXCLUDED.reported_at""",
                    *f, now, daemon_id)
            await conn.execute(
                """DELETE FROM org_session_facts
                    WHERE daemon_id = $1 AND reported_at < $2 AND NOT (pid = ANY($3::text[]))""",
                daemon_id, now - timedelta(days=7), [f[0] for f in facts])
            caps = sorted({c[:64] for c in capabilities or () if isinstance(c, str)})[:64]
            await conn.execute(
                """INSERT INTO org_daemons (daemon_id, capabilities, seen_at) VALUES ($1, $2, $3)
                   ON CONFLICT (daemon_id) DO UPDATE SET capabilities = EXCLUDED.capabilities,
                                                         seen_at = EXCLUDED.seen_at""",
                daemon_id, caps, now)
            capable = ORG_MARKS in caps
            marks = []
            # The marks table's statement trigger takes the feed lock even for
            # a zero-row UPDATE, so look first (a plain read takes no lock):
            # an idle beat must not queue behind org and task writes.
            acked = [int(i) for i in acked_mark_ids or ()]
            if acked and await conn.fetchval(
                    """SELECT EXISTS (SELECT 1 FROM org_workspace_marks
                                       WHERE daemon_id = $1 AND id = ANY($2::bigint[])
                                         AND state <> 'confirmed')""", daemon_id, acked):
                await conn.execute(
                    """UPDATE org_workspace_marks SET state = 'confirmed', confirmed_at = $3
                        WHERE daemon_id = $1 AND id = ANY($2::bigint[]) AND state <> 'confirmed'""",
                    daemon_id, acked, now)
            if capable and await conn.fetchval(
                    """SELECT EXISTS (SELECT 1 FROM org_workspace_marks
                                       WHERE daemon_id = $1 AND state <> 'confirmed'
                                         AND attempts < $2)""", daemon_id, MAX_MARK_ATTEMPTS):
                rows = await conn.fetch(
                    """UPDATE org_workspace_marks w
                          SET state = 'pushed', attempts = w.attempts + 1, pushed_at = $3
                         FROM members m
                        WHERE w.daemon_id = $1 AND w.member_id = m.id
                          AND w.state <> 'confirmed' AND w.attempts < $2
                    RETURNING w.id, w.workspace_id, w.path, w.kind, w.member_id, m.handle""",
                    daemon_id, MAX_MARK_ATTEMPTS, now)
                marks = [dict(r) for r in rows]
            members = await conn.fetch(
                f"""{_MEMBER_SELECT}
                     WHERE inc.daemon_id = $1 AND left(inc.pid, length($2)) = $2""", daemon_id, prefix)
    return {
        "members": {r["main_session_uid"]: {**member_summary(_shape_member(r)), "acting_as": "main"}
                    for r in members},
        "workspace_marks": marks,
        "capable": bool(capable),
    }


async def list_events(pool, *, subject_type=None, subject_id=None, limit=100) -> list[dict]:
    async with pool.acquire() as conn:
        rows = await conn.fetch(
            """SELECT * FROM org_events
                WHERE ($1::text IS NULL OR subject_type = $1)
                  AND ($2::text IS NULL OR subject_id = $2)
                ORDER BY id DESC LIMIT $3""", subject_type, subject_id, limit)
    return [_json_safe(dict(r)) for r in rows]

