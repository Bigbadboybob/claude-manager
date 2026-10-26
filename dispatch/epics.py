"""Epics: points of contact, staffing and the close plan (doc/org-model.md
§7.3, §7.6).

An epic is a `groups` row with `kind='epic'`. Setting its PoC makes it run by
a real team (Owner, O2): a `kind='team'` team led by the PoC, created through
the org core's in-process `create_team_conn` (org S2) or an existing team the
PoC leads. The PoC is never stored on the group; it is read from the team's
lead. Closing an epic builds a persisted close plan (items, team, members,
workspaces), then applies it step by step, each step recorded so a retry
resumes where it stopped and never repeats a done step.

The member and team pieces need org S2 (members, teams, team_memberships);
without it they answer 409 `org_core_missing`, and the close plan still
covers the items.
"""

from __future__ import annotations

import uuid

from dispatch import items_db, placement
from dispatch.items_rules import CLOSED, OWNER_PID, ItemsError, invalid, iso

OUTCOMES = ("done", "cancelled")
ITEM_CHOICES = ("move", "unplace", "drop")
TEAM_CHOICES = ("keep", "standby", "archive")
PRIMARY_CHOICES = ("keep", "move", "retire")
GUEST_CHOICES = ("end", "keep")


def _org():
    try:
        from dispatch import org_db  # org model S2
    except ImportError:  # pragma: no cover - only before S2 merges
        raise ItemsError(409, "org_core_missing",
                         "points of contact and epic teams need the org core (S2: members and "
                         "teams); it is not deployed on this API") from None
    return org_db


async def _has_org_tables(conn) -> bool:
    return bool(await conn.fetchval(
        "SELECT to_regclass('public.team_memberships') IS NOT NULL"))


async def _epic(conn, board: dict, group_id: str, *, lock: bool = False):
    groups = await items_db._load_groups(conn, board["id"])
    g = groups.get(str(group_id))
    if g is None:
        raise ItemsError(404, "not_found", f"no group {group_id} on board {board['slug']}")
    if g.kind != "epic":
        raise ItemsError(409, "not_an_epic",
                         f"{g.name!r} is a heading; promote it first: group.set(kind=\"epic\")")
    if lock:
        await conn.execute("SELECT 1 FROM groups WHERE id = $1 FOR UPDATE", uuid.UUID(g.id))
    return g, groups


async def _authorize(conn, board: dict, actor: dict, what: str, *, poc_team: str | None = None) -> str:
    """Setting a PoC and closing an epic are acts of authority over the board
    (§7.3, §7.6): Owner, the board's lead (its orchestrator), its steward, or
    (with `poc_team`, the epic's current team) the current PoC or a member up
    the PoC's manager chain, acting as its main session. The last two
    apply to set_poc only. Returns the authority used, recorded on the org
    event."""
    if actor["pid"] == OWNER_PID:
        return f"{what}:owner"
    orch = await placement._orchestrator_pid(conn, board)
    if orch and actor["pid"] == orch:
        return f"{what}:board_lead"
    # The steward and the PoC chain may name a PoC (#28); closing an epic
    # stays the board lead's or Owner's.
    if what == "set_poc" and board.get("steward_pid") and actor["pid"] == board["steward_pid"]:
        return f"{what}:steward"
    if poc_team and await _has_org_tables(conn):
        org = _org()
        poc = (await placement._team_leads(conn, [poc_team])).get(str(poc_team))
        actor_member = await conn.fetchval(
            """SELECT member_id FROM incarnations
                WHERE pid = $1 AND role = 'main' AND ended_at IS NULL""", actor["pid"])
        if poc and actor_member:
            if actor_member == poc:
                return f"{what}:poc"
            if actor_member in [m["id"] for m in await org._chain(conn, poc)]:
                return f"{what}:poc_chain"
    raise ItemsError(403, "forbidden",
                     f"only Owner, the board's lead (its orchestrator), its steward or the epic's "
                     f"current PoC chain may {what.replace('_', ' ')}")


async def _member_of(conn, org, member: str) -> str | None:
    """A member reference (id, handle, alias, `owner`) or a session's
    participant id (`agent:<daemon>:<uid>`), which names the member whose
    main session it is."""
    if isinstance(member, str) and member.startswith("agent:"):
        return await conn.fetchval(
            """SELECT member_id FROM incarnations
                WHERE pid = $1 AND role = 'main' AND ended_at IS NULL""", member)
    return await org._resolve_member_id(conn, member)


def _subtree(groups: dict, root_id: str) -> set[str]:
    out, frontier = {root_id}, [root_id]
    while frontier:
        parent = frontier.pop()
        for g in groups.values():
            if g.parent_id == parent and g.id not in out:
                out.add(g.id)
                frontier.append(g.id)
    return out


def _enclosing_epic_team(groups: dict, g) -> str | None:
    parent = groups.get(g.parent_id) if g.parent_id else None
    while parent is not None:
        if parent.kind == "epic" and parent.team_id:
            return parent.team_id
        parent = groups.get(parent.parent_id) if parent.parent_id else None
    return None


# ---- PoC and staffing -----------------------------------------------------------

async def set_poc(pool, ref: str, actor: dict, group_id: str, member: str, *,
                  team_id: str | None = None, request_id: str | None = None) -> dict:
    """Make `member` the epic's PoC: link `team_id` (a team the member
    already leads), keep the epic's team if the member leads it, else create
    a new `kind='team'` team led by the member, parented on the nearest
    enclosing epic's team, else the board's team. Returns the team and the
    staffing offers: each current holder of the epic's open items, offered
    as a primary member (a move, needing that member's manager chain or
    Owner) or a guest (keeps its manager)."""
    org = _org()
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            await conn.execute(items_db.TASK_WRITE_LOCK_SQL)
            board = await items_db._board_by_ref(conn, ref, lock=True)
            g, groups = await _epic(conn, board, group_id, lock=True)
            if g.closed:
                raise ItemsError(409, "epic_closed", f"epic {g.name!r} is {g.epic_status}")
            authority = await _authorize(conn, board, actor, "set_poc", poc_team=g.team_id)
            member_id = await _member_of(conn, org, member)
            if not member_id:
                if isinstance(member, str) and member.startswith("agent:"):
                    raise invalid("unknown_member",
                                  f"{member} is no member's main session yet: promote it first "
                                  f"(org(action=\"promote\", session=...), or spawn it with "
                                  f"hire={{handle, title}}), then set_poc", field="member")
                raise invalid("unknown_member", f"no member {member!r}", field="member")
            leads = await placement._team_leads(conn, [t for t in (team_id, g.team_id) if t])
            if team_id:
                if leads.get(str(team_id)) != member_id:
                    raise ItemsError(409, "not_team_lead",
                                     f"team {team_id} is not led by {member!r}; link a team the "
                                     f"PoC leads, or omit team_id to create one")
                chosen = str(team_id)
            elif g.team_id and leads.get(g.team_id) == member_id:
                chosen = g.team_id
            else:
                parent = _enclosing_epic_team(groups, g) or board.get("initiative_id")
                # A hand-over to a PoC that leads no team yet creates its team;
                # the epic's earlier team keeps the plain slug.
                slug = None
                if g.team_id:
                    handle = await conn.fetchval("SELECT handle FROM members WHERE id = $1", member_id)
                    slug = f"{g.name}-{handle}"
                chosen = await org.create_team_conn(
                    conn, actor, name=g.name, slug=slug, kind="team", parent_team_id=parent,
                    lead=member_id, charter=g.goal or "", authorized_by=authority,
                    request_id=request_id)
                chosen = str(chosen)
            await conn.execute(
                """UPDATE groups SET team_id = $2, revision = revision + 1, updated_at = $3
                    WHERE id = $1""", uuid.UUID(g.id), uuid.UUID(chosen), now)
            await placement._group_event(conn, board, actor, "poc_set", {"team_id": g.team_id},
                                         {"team_id": chosen, "poc_member_id": member_id,
                                          "authorized_by": authority}, now)
            # A PoC need not have touched the board yet (#28): with the
            # authority checked above, its main session joins it here.
            from dispatch import board_members
            joined = []
            main_pid = await conn.fetchval(
                """SELECT pid FROM incarnations
                    WHERE member_id = $1 AND role = 'main' AND ended_at IS NULL""", member_id)
            if main_pid and await board_members.add(
                    conn, board, main_pid, added_by=actor["pid"], group_id=g.id,
                    reason=f"PoC of epic {g.name}"):
                joined.append(main_pid)
            offers = await _staffing_offers(conn, board, groups, g, chosen)
            return {"group": (await placement._group_rows(conn, board, [g.id]))[0],
                    "team_id": chosen, "poc_member_id": member_id, "staffing": offers,
                    "joined_board": joined}


async def _staffing_offers(conn, board, groups, g, team_id) -> list[dict]:
    ids = [uuid.UUID(x) for x in _subtree(groups, g.id)]
    rows = await conn.fetch(
        """SELECT h.pid, h.name, array_agg(i.number ORDER BY i.number) AS holds
             FROM item_holders h JOIN items i ON i.id = h.item_id
            WHERE i.board_id = $1 AND i.group_id = ANY($2::uuid[]) AND i.closed_at IS NULL
              AND i.archived_at IS NULL
            GROUP BY h.pid, h.name ORDER BY h.name""",
        uuid.UUID(board["id"]), ids)
    out = []
    for r in rows:
        member_id = await conn.fetchval(
            """SELECT member_id FROM incarnations WHERE pid = $1 AND ended_at IS NULL
                ORDER BY id DESC LIMIT 1""", r["pid"]) if await _has_org_tables(conn) else None
        on_team = member_id and await conn.fetchval(
            """SELECT role FROM team_memberships WHERE team_id = $1 AND member_id = $2
                 AND (until IS NULL OR until > now()) LIMIT 1""", uuid.UUID(team_id), member_id)
        out.append({"member_id": member_id, "pid": r["pid"], "name": r["name"],
                    "holds": list(r["holds"]), "offer": "guest",
                    "options": ["primary", "guest"], "already": on_team or None})
    return out


async def staff(pool, ref: str, actor: dict, group_id: str, member: str, as_: str, *,
                reason: str | None = None, request_id: str | None = None) -> dict:
    """Accept one staffing offer (§7.3): a guest joins the epic's team (item
    roles and the team channel, never management); a primary member moves to
    the team under the PoC, through org's `move`, which checks the member's
    manager chain or Owner."""
    org = _org()
    if as_ not in ("primary", "guest"):
        raise invalid("invalid_field", "as must be primary or guest", field="as")
    async with pool.acquire() as conn:
        board = await items_db._board_by_ref(conn, ref)
        g, _ = await _epic(conn, board, group_id)
        if not g.team_id:
            raise ItemsError(409, "no_poc", "set the epic's PoC first (epic.set_poc)")
        member_id = await org._resolve_member_id(conn, member)
        if not member_id:
            raise invalid("unknown_member", f"no member {member!r}", field="member")
        poc = (await placement._team_leads(conn, [g.team_id])).get(g.team_id)
    if as_ == "primary":
        return {"member_id": member_id, "as": "primary",
                "move": await org.move(pool, actor, member_id, reason=reason or f"staffed onto epic {g.name}",
                                       manager=poc, primary_team_id=g.team_id,
                                       request_id=request_id)}
    async with pool.acquire() as conn:
        async with conn.transaction():
            await conn.execute(
                """INSERT INTO team_memberships (team_id, member_id, role, since)
                   VALUES ($1, $2, 'guest', now()) ON CONFLICT DO NOTHING""",
                uuid.UUID(g.team_id), member_id)
    return {"member_id": member_id, "as": "guest", "team_id": g.team_id}


# ---- the close plan (§7.6) -------------------------------------------------------

async def close_plan(pool, ref: str, actor: dict, group_id: str, outcome: str) -> dict:
    """Build (or rebuild) the epic's close plan: one row per open item, the
    team, each member, and the workspaces. Persisted, so it is shown to the
    closer and resumable; a rebuild bumps the revision and drops the old
    choices."""
    if outcome not in OUTCOMES:
        raise invalid("invalid_field", "outcome must be done or cancelled", field="outcome")
    async with pool.acquire() as conn:
        async with conn.transaction():
            board = await items_db._board_by_ref(conn, ref, lock=True)
            g, groups = await _epic(conn, board, group_id, lock=True)
            if g.closed:
                raise ItemsError(409, "epic_closed", f"epic {g.name!r} is already {g.epic_status}")
            await _authorize(conn, board, actor, "close_epic")
            active_sub = [s for s in groups.values()
                          if s.parent_id and s.id in _subtree(groups, g.id) - {g.id}
                          and s.kind == "epic" and not s.closed]
            if active_sub:
                raise ItemsError(409, "active_sub_epics",
                                 "close the sub-epics first: "
                                 + ", ".join(repr(s.name) for s in active_sub),
                                 sub_epics=[s.id for s in active_sub])
            plan = await _build_plan(conn, board, groups, g, outcome)
            row = await conn.fetchrow(
                """INSERT INTO epic_close_plans (group_id, outcome, plan, created_by)
                   VALUES ($1, $2, $3, $4)
                   ON CONFLICT (group_id) WHERE status IN ('planned', 'applying')
                   DO UPDATE SET outcome = EXCLUDED.outcome, plan = EXCLUDED.plan,
                                 choices = NULL, progress = '[]'::jsonb,
                                 revision = epic_close_plans.revision + 1, updated_at = now()
                           WHERE epic_close_plans.status = 'planned'
                   RETURNING id, revision, status""",
                uuid.UUID(g.id), outcome, plan, actor["pid"])
            if row is None:
                # A plan being applied is resumed (close_apply again), never
                # replaced under it.
                raise ItemsError(409, "close_in_progress",
                                 "this epic's close plan is being applied; resume it with "
                                 "epic.close_apply and the same plan_id")
            return {"plan_id": row["id"], "revision": row["revision"], **plan}


async def _build_plan(conn, board, groups, g, outcome) -> dict:
    sub = _subtree(groups, g.id)
    items = await items_db._load_items(conn, board["id"])
    open_items = sorted((it for it in items.values()
                         if it.group_id in sub and it.status not in CLOSED
                         and it.link_archived_at is None), key=lambda it: it.n)
    targets = [{"group_id": o.id, "name": o.name, "kind": o.kind}
               for o in groups.values() if o.id not in sub and not o.closed]
    item_rows = []
    for it in open_items:
        options = (["move"] if targets else []) + (["unplace"] if it.task_id else []) + ["drop"]
        linked = sorted(it.blocked_by | {o.n for o in items.values() if it.n in o.blocked_by})
        item_rows.append({"n": it.n, "title": it.title, "status": it.status,
                          "task_id": it.task_id, "choice_options": options,
                          "blocked_with": linked})
    team = None
    members: list[dict] = []
    if g.team_id and await _has_org_tables(conn):
        other_epics = await conn.fetchval(
            """SELECT count(*) FROM groups WHERE team_id = $1 AND id <> $2 AND kind = 'epic'
                 AND epic_status = 'active'""", uuid.UUID(g.team_id), uuid.UUID(g.id))
        name = await conn.fetchval("SELECT name FROM initiatives WHERE id = $1",
                                   uuid.UUID(g.team_id))
        options = ["detach"] if other_epics else list(TEAM_CHOICES)
        team = {"team_id": g.team_id, "name": name, "options": options,
                "default": "detach" if other_epics else "keep",
                "needs": "closer",
                "needs_by_option": {o: ("owner" if o in ("standby", "archive") else "closer")
                                    for o in options},
                "other_active_epics": int(other_epics)}
        for m in await conn.fetch(
                """SELECT tm.member_id, tm.role, m.handle FROM team_memberships tm
                     JOIN members m ON m.id = tm.member_id
                    WHERE tm.team_id = $1 AND (tm.until IS NULL OR tm.until > now())
                    ORDER BY tm.role, m.handle""", uuid.UUID(g.team_id)):
            if m["role"] == "guest":
                members.append({"member_id": m["member_id"], "handle": m["handle"],
                                "role": "guest", "options": list(GUEST_CHOICES),
                                "default": "end", "needs": "closer",
                                "needs_by_option": {"end": "closer", "keep": "closer"}})
            else:
                # Retiring throws a session's context away: never just the
                # closer's call (O1). A move needs the manager chain too.
                members.append({"member_id": m["member_id"], "handle": m["handle"],
                                "role": m["role"], "options": list(PRIMARY_CHOICES),
                                "default": "keep", "needs": "closer",
                                "needs_by_option": {"keep": "closer", "move": "manager_chain",
                                                    "retire": "manager_chain"}})
    return {"group_id": g.id, "name": g.name, "outcome": outcome, "items": item_rows,
            "move_targets": targets, "team": team, "members": members,
            # Stream and ephemeral workspaces arrive with S6; home workspaces stay.
            "workspaces": []}


async def close_apply(pool, ref: str, actor: dict, plan_id: int, revision: int,
                      choices: dict) -> dict:
    """Apply a close plan. Each step is recorded in the plan's progress as it
    lands, so a retry with the same plan resumes and never repeats a step.
    Choices the closer cannot authorize become pending; the epic still
    closes."""
    choices = choices or {}
    # One applier per plan at a time: a session-level advisory lock held on
    # this connection for the whole apply (released if the API dies, so a
    # crashed apply is simply resumed by the next call).
    async with pool.acquire() as guard:
        if not await guard.fetchval("SELECT pg_try_advisory_lock(hashtext('epic_close_plan'), $1)",
                                    int(plan_id)):
            raise ItemsError(409, "close_in_progress",
                             f"close plan {plan_id} is being applied right now; retry shortly")
        try:
            return await _close_apply(pool, ref, actor, int(plan_id), revision, choices)
        finally:
            await guard.execute("SELECT pg_advisory_unlock(hashtext('epic_close_plan'), $1)",
                                int(plan_id))


async def _close_apply(pool, ref: str, actor: dict, plan_id: int, revision: int,
                       choices: dict) -> dict:
    async with pool.acquire() as conn:
        row = await conn.fetchrow("SELECT p.*, g.board_id FROM epic_close_plans p "
                                  "JOIN groups g ON g.id = p.group_id WHERE p.id = $1", plan_id)
        if row is None:
            raise ItemsError(404, "not_found", f"no close plan {plan_id}")
        if row["revision"] != revision:
            raise ItemsError(409, "revision_changed",
                             f"plan {plan_id} is at revision {row['revision']}; rebuild it "
                             f"(epic.close_plan) and choose again", revision=row["revision"])
        if row["status"] == "applied":
            return {"applied": True, "pending": (row["choices"] or {}).get("_pending", []),
                    "replayed": True}
        board = await items_db._board_by_ref(conn, ref)
        # A plan applies only on the board its epic is on: item numbers are
        # per board (review of 6522623).
        if str(row["board_id"]) != board["id"]:
            raise ItemsError(404, "not_found",
                             f"close plan {plan_id} belongs to another board, not {board['slug']}")
        await _authorize(conn, board, actor, "close_epic")
        plan = row["plan"]
        # Every open item needs a valid choice before anything is applied.
        item_choices = choices.get("items") or {}
        for it in plan["items"]:
            choice = item_choices.get(str(it["n"]))
            kind = str(choice or "").partition(":")[0]
            if not choice:
                raise invalid("choice_required",
                              f"#{it['n']} \"{it['title']}\" is still open: choose "
                              f"move:<group_id>, unplace or drop", field="choices", n=it["n"])
            if kind not in it["choice_options"]:
                raise invalid("invalid_choice", f"#{it['n']}: {choice!r} is not one of "
                              f"{it['choice_options']}", field="choices", n=it["n"])
        await conn.execute(
            """UPDATE epic_close_plans SET status = 'applying', choices = $2, updated_at = now()
                WHERE id = $1""", row["id"], choices)
    done = set(row["progress"] or [])
    pending: list[dict] = []

    async def mark(step: str) -> None:
        done.add(step)
        async with pool.acquire() as conn:
            await conn.execute(
                """UPDATE epic_close_plans SET progress = progress || to_jsonb($2::text),
                          updated_at = now() WHERE id = $1""", row["id"], step)

    # 1. Open items: moved, unplaced or dropped, each its own write. One
    # closed or unplaced since the plan was built is skipped and recorded.
    skipped: list[dict] = []
    for it in plan["items"]:
        step = f"item:{it['n']}"
        if step in done:
            continue
        async with pool.acquire() as conn:
            now_row = await conn.fetchrow(
                """SELECT status, link_archived_at, archived_at FROM items
                    WHERE board_id = $1 AND number = $2""", uuid.UUID(board["id"]), it["n"])
        if (now_row is None or now_row["status"] in CLOSED or now_row["link_archived_at"]
                or now_row["archived_at"]):
            skipped.append({"n": it["n"], "reason": "no longer open"
                            if now_row is not None and now_row["status"] in CLOSED
                            else "unplaced or gone"})
            await mark(step)
            continue
        kind, _, arg = str(item_choices[str(it["n"])]).partition(":")
        if kind == "move":
            await items_db.update_items(pool, ref, actor, [it["n"]], {"group_id": arg},
                                        reason=f"epic {plan['name']} closing")
        elif kind == "unplace" and it.get("task_id"):
            await placement.unplace_task(pool, it["task_id"], actor,
                                         reason=f"epic {plan['name']} closing")
        else:
            await items_db.update_items(pool, ref, actor, [it["n"]], {"status": "dropped"},
                                        reason=arg or f"epic {plan['name']} {plan['outcome']}")
        await mark(step)

    # 2. Team and members: what the closer may not authorize is pending.
    is_owner = actor["pid"] == OWNER_PID
    team = plan.get("team")
    if team and "team" not in done:
        choice = choices.get("team") or team["default"]
        if choice in ("standby", "archive") and not is_owner:
            pending.append({"row": "team", "choice": choice, "who_must_decide": "owner"})
        elif choice in ("standby", "archive"):
            org = _org()
            await org.team_update(pool, actor, team["team_id"],
                                  {"status": "standby" if choice == "standby" else "archived"},
                                  reason=f"epic {plan['name']} {plan['outcome']}")
        await mark("team")
    for m in plan.get("members") or []:
        step = f"member:{m['member_id']}"
        if step in done:
            continue
        choice = (choices.get("members") or {}).get(m["member_id"]) or m["default"]
        if m["role"] == "guest" and choice == "end":
            async with pool.acquire() as conn:
                await conn.execute(
                    """UPDATE team_memberships SET until = now()
                        WHERE team_id = $1 AND member_id = $2 AND role = 'guest'
                          AND (until IS NULL OR until > now())""",
                    uuid.UUID(team["team_id"]), m["member_id"])
        elif choice in ("move", "retire"):
            # The move or retire itself is the org core's (with its manager
            # chain checks): never done silently here, even for Owner.
            pending.append({"row": f"member:{m['member_id']}", "choice": choice,
                            "who_must_decide": "owner" if is_owner else "manager_chain",
                            "how": f"org(action=\"{'move' if choice == 'move' else 'retire'}\", "
                                   f"member=\"{m['member_id']}\", ...)"})
        await mark(step)

    # 3. The final "nothing left open" check, under the board lock item
    # writers use; then the epic closes and the record is kept.
    now = items_db.utcnow()
    async with pool.acquire() as conn:
        async with conn.transaction():
            await conn.execute(items_db.TASK_WRITE_LOCK_SQL)
            board = await items_db._board_by_ref(conn, ref, lock=True)
            g, groups = await _epic(conn, board, plan["group_id"], lock=True)
            sub = [uuid.UUID(x) for x in _subtree(groups, g.id)]
            still = await conn.fetch(
                """SELECT number FROM items WHERE board_id = $1 AND group_id = ANY($2::uuid[])
                     AND closed_at IS NULL AND archived_at IS NULL AND link_archived_at IS NULL
                    ORDER BY number""", uuid.UUID(board["id"]), sub)
            if still:
                raise ItemsError(409, "items_still_open",
                                 "items were added to the epic after the plan was built: "
                                 + ", ".join(f"#{r['number']}" for r in still)
                                 + "; rebuild the plan", open=[r["number"] for r in still])
            holders = await conn.fetch(
                """SELECT DISTINCT e.actor_pid FROM item_events e JOIN items i ON i.id = e.item_id
                    WHERE i.board_id = $1 AND i.group_id = ANY($2::uuid[])""",
                uuid.UUID(board["id"]), sub)
            record = {"outcome": plan["outcome"], "choices": choices, "pending": pending,
                      "skipped": skipped,
                      "contributors": sorted(r["actor_pid"] for r in holders),
                      "closed_at": iso(now)}
            await conn.execute(
                """UPDATE groups SET epic_status = $2, closed_at = $3, closed_by = $4,
                          close_record = $5, revision = revision + 1, updated_at = $3
                    WHERE id = $1""",
                uuid.UUID(g.id), plan["outcome"], now, actor.get("name") or actor["pid"], record)
            await placement._group_event(conn, board, actor, "epic_closed", None,
                                         {"group_id": g.id, "outcome": plan["outcome"],
                                          "pending": len(pending)}, now)
            await conn.execute(
                """UPDATE epic_close_plans SET status = 'applied', applied_at = $2,
                          choices = COALESCE(choices, '{}'::jsonb) || jsonb_build_object('_pending', $3::jsonb),
                          updated_at = $2 WHERE id = $1""", row["id"], now, pending)
    return {"applied": True, "pending": pending, "skipped": skipped,
            "epic_status": plan["outcome"]}

