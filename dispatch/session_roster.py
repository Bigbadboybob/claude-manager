"""Scoped roster snapshot. The API's trusted host supplies the caller stamp.

This module must not be exposed without the daemon's before/after identity
fence. The shared API bearer authenticates a host, not an individual session.
"""

from __future__ import annotations

from collections import deque
from datetime import timedelta

from dispatch import session_roster_cursor as cursors
from dispatch.items_rules import ItemsError
from dispatch.session_roster_projection import project_activity

# An explicit fail-closed engineering ceiling, not a production measurement.
# A very large scope must use a separately reviewed graph strategy.
MAX_TASK_SCOPE = 10_000

# All candidate authorization happens here, before filters or counts. UNION
# makes both graph walks terminate even with corrupt cycles; it never widens
# either graph across a different organization.
_SCOPED = r"""
WITH RECURSIVE
task_scope(id) AS (
    SELECT id FROM tasks WHERE id::text = $1
    UNION
    SELECT t.id FROM tasks t JOIN task_scope p
      ON t.parent_task_id = p.id OR t.metadata #>> '{filer,task_id}' = p.id::text
),
member_scope(id, org_id) AS (
    SELECT id, org_id FROM members WHERE id = $2
    UNION
    SELECT m.id, m.org_id FROM members m JOIN member_scope p
      ON m.manager_member_id = p.id AND m.org_id = p.org_id
),
facts AS MATERIALIZED (
    SELECT s.pid, s.daemon_id, s.session_uid, left(s.host_label, 200) AS host_label,
           s.task_id, left(s.name, 200) AS name, left(s.engine, 40) AS engine,
           left(s.state, 40) AS state, s.state_since, s.exited_at, s.reported_at,
           left(s.recorded_model, 200) AS recorded_model,
           left(s.recorded_model_source, 64) AS recorded_model_source,
           jsonb_build_object(
             'source', left(s.agent_state->>'source', 16),
             'since', CASE WHEN jsonb_typeof(s.agent_state->'since') = 'number'
                                AND length(s.agent_state->>'since') <= 32
                           THEN s.agent_state->'since' END,
             'observed_at', CASE WHEN jsonb_typeof(s.agent_state->'observed_at') = 'number'
                                      AND length(s.agent_state->>'observed_at') <= 32
                                 THEN s.agent_state->'observed_at' END,
             'engine_turn_started_at', CASE WHEN jsonb_typeof(s.agent_state->'engine_turn_started_at') = 'number'
                                      AND length(s.agent_state->>'engine_turn_started_at') <= 32
                                 THEN s.agent_state->'engine_turn_started_at' END,
             'last_turn', jsonb_build_object(
               'status', left(s.agent_state#>>'{last_turn,status}', 16),
               'ended_at', CASE WHEN jsonb_typeof(s.agent_state#>'{last_turn,ended_at}') = 'number'
                                     AND length(s.agent_state#>>'{last_turn,ended_at}') <= 32
                                THEN s.agent_state#>'{last_turn,ended_at}' END),
             'detail', jsonb_build_object(
               'error_kind', left(s.agent_state#>>'{detail,error_kind}', 80),
               'resumes_at', CASE WHEN jsonb_typeof(s.agent_state#>'{detail,resumes_at}') = 'number'
                                       AND length(s.agent_state#>>'{detail,resumes_at}') <= 32
                                  THEN s.agent_state#>'{detail,resumes_at}' END,
               'dialog_visible', CASE WHEN jsonb_typeof(s.agent_state#>'{detail,dialog_visible}') = 'boolean'
                                  THEN s.agent_state#>'{detail,dialog_visible}' END,
               'retrying', CASE WHEN jsonb_typeof(s.agent_state#>'{detail,retrying}') = 'boolean'
                                THEN s.agent_state#>'{detail,retrying}' END)
           ) AS agent_state,
           f.reported_at AS org_as_of, f.workspace_id,
           CASE WHEN i.id IS NOT NULL THEN i.member_id
                WHEN f.reported_at >= now() - interval '90 seconds'
                 AND s.reported_at >= now() - interval '90 seconds'
                 AND s.roster_stamp_as_of = s.reported_at
                 AND f.acting_as IN ('assistant','worker') AND fm.id IS NOT NULL
                 AND f.member_id = s.roster_stamp_member AND f.acting_as = s.roster_stamp_role
                THEN f.member_id END AS resolved_member,
           CASE WHEN i.id IS NOT NULL THEN 'main'
                WHEN f.reported_at >= now() - interval '90 seconds'
                 AND s.reported_at >= now() - interval '90 seconds'
                 AND s.roster_stamp_as_of = s.reported_at
                 AND f.acting_as IN ('assistant','worker') AND fm.id IS NOT NULL
                 AND f.member_id = s.roster_stamp_member AND f.acting_as = s.roster_stamp_role
                THEN f.acting_as END AS resolved_role,
           i.id AS incarnation_id, i.occupancy_generation AS incarnation_generation,
           ((i.id IS NOT NULL AND f.acting_as IN ('assistant','worker')
            AND f.member_id IS DISTINCT FROM i.member_id
            AND f.reported_at >= now() - interval '90 seconds')
            OR (i.id IS NULL AND f.member_id IS NOT NULL AND s.roster_stamp_member IS NOT NULL
                AND (f.member_id IS DISTINCT FROM s.roster_stamp_member
                     OR f.acting_as IS DISTINCT FROM s.roster_stamp_role))) AS stamp_conflict
      FROM session_states s
      LEFT JOIN org_session_facts f ON f.pid = s.pid
      LEFT JOIN incarnations i ON i.pid = s.pid AND i.role = 'main' AND i.ended_at IS NULL
      LEFT JOIN members fm ON fm.id = f.member_id
),
scoped AS MATERIALIZED (
    SELECT f.*, m.handle AS member_handle, m.status AS member_status,
           m.manager_member_id, mgr.handle AS manager_handle,
           left(t.name, 200) AS task_name, length(t.name) > 200 AS task_name_truncated,
           CASE WHEN f.reported_at < now() - interval '90 seconds' THEN 'stale'
                WHEN f.state IN ('working','working-background','waiting-on-human','errored',
                                 'idle','starting','exited','unknown') THEN f.state
                ELSE 'unknown' END AS visible_state
      FROM facts f
      LEFT JOIN members m ON m.id = f.resolved_member
      LEFT JOIN members mgr ON mgr.id = m.manager_member_id
      LEFT JOIN tasks t ON t.id = f.task_id
     WHERE ($3::boolean OR f.pid = $4 OR f.task_id IN (SELECT id FROM task_scope)
            OR f.resolved_member IN (SELECT id FROM member_scope)
            OR ($1::text IS NULL AND f.daemon_id = $5 AND f.session_uid = ANY($6::text[])))
       AND f.pid = 'agent:' || f.daemon_id || ':' || f.session_uid
       AND ($7::boolean OR f.exited_at IS NULL)
)
"""


async def _caller(conn, pid):
    row = await conn.fetchrow("""
        SELECT i.id AS incarnation_id, i.occupancy_generation AS incarnation_generation,
               i.member_id AS main_member, f.member_id AS stamp_member, f.acting_as,
               f.reported_at AS org_as_of, m.revision AS main_revision,
               fm.revision AS stamp_revision, (s.pid IS NOT NULL) AS stamp_confirmed
          FROM (SELECT $1::text AS pid) p
          LEFT JOIN incarnations i ON i.pid = p.pid AND i.role = 'main' AND i.ended_at IS NULL
          LEFT JOIN members m ON m.id = i.member_id
          LEFT JOIN org_session_facts f ON f.pid = p.pid
             AND f.reported_at >= now() - interval '90 seconds'
             AND f.acting_as IN ('assistant','worker')
          LEFT JOIN session_states s ON s.pid = p.pid
             AND s.reported_at >= now() - interval '90 seconds'
             AND s.roster_stamp_as_of = s.reported_at
             AND s.roster_stamp_member = f.member_id AND s.roster_stamp_role = f.acting_as
          LEFT JOIN members fm ON fm.id = f.member_id
        """, pid)
    if row["main_member"]:
        return {"member_id": row["main_member"], "role": "main",
                "revision": row["main_revision"], "incarnation_id": row["incarnation_id"],
                "occupancy_generation": row["incarnation_generation"]}
    if row["stamp_member"] and row["stamp_revision"] is not None and row["stamp_confirmed"]:
        return {"member_id": row["stamp_member"], "role": row["acting_as"],
                "revision": row["stamp_revision"], "incarnation_id": None,
                "occupancy_generation": None}
    return {"member_id": None, "role": None, "revision": None,
            "incarnation_id": None, "occupancy_generation": None}


async def _check_task_graph(conn, params):
    rows = await conn.fetch(_SCOPED + """
        SELECT t.id::text AS id, t.parent_task_id::text AS parent,
               CASE WHEN length(t.metadata #>> '{filer,task_id}') = 36
                    THEN t.metadata #>> '{filer,task_id}' END AS filer
          FROM tasks t WHERE t.id IN (SELECT id FROM task_scope)
         LIMIT $8""", *params, MAX_TASK_SCOPE + 1)
    if len(rows) > MAX_TASK_SCOPE:
        raise ItemsError(409, "scope_too_large", "task scope exceeds the roster engineering bound")
    # Every edge in the reachable closure must be acyclic. Reject the whole
    # task scope on a cycle; merely stopping a recursive walk at a repeated
    # node would already have admitted earlier nodes in the same cycle.
    nodes = {r["id"] for r in rows}
    children = {node: set() for node in nodes}
    indegree = {node: 0 for node in nodes}
    for row in rows:
        for parent in {row["parent"], row["filer"]} & nodes:
            if row["id"] not in children[parent]:
                children[parent].add(row["id"])
                indegree[row["id"]] += 1
    ready = deque(node for node, degree in indegree.items() if degree == 0)
    visited = 0
    while ready:
        node = ready.popleft()
        visited += 1
        for child in children[node]:
            indegree[child] -= 1
            if indegree[child] == 0:
                ready.append(child)
    if visited != len(nodes):
        raise ItemsError(409, "scope_cycle", "task scope has cyclic parent or filer edges")


async def read(pool, caller, filters, secret):
    """One read-only snapshot; caller/filters must have strict bounded models.

    `caller` is stamped by the daemon, not supplied by the MCP user. Recheck
    the returned basis centrally and locally before returning it to the user.
    """
    if not secret:
        raise ItemsError(503, "unavailable", "roster cursor key unavailable")
    limit = filters["limit"]
    if not isinstance(limit, int) or isinstance(limit, bool) or not 1 <= limit <= 100:
        raise ItemsError(422, "invalid_field", "limit must be between 1 and 100")
    async with pool.acquire() as conn:
        async with conn.transaction(isolation="repeatable_read", readonly=True):
            at = await conn.fetchval("SELECT now()")
            identity = await _caller(conn, caller["pid"]) if not caller["owner"] else {
                "member_id": None, "role": None, "revision": None,
                "incarnation_id": None, "occupancy_generation": None}
            org_version = await conn.fetchval("SELECT coalesce(max(seq), 0) FROM org_changes")
            task_version = await conn.fetchrow("""SELECT epoch, pruned_through,
                greatest(pruned_through, (SELECT coalesce(max(seq),0) FROM task_changes)) AS seq
                FROM task_change_meta""")
            basis = {**identity, "task_roots": [caller["task_id"]] if caller["task_id"] else [],
                     "global": caller["global"], "owner": caller["owner"],
                     "org_version": org_version, "task_epoch": task_version["epoch"],
                     "task_version": task_version["seq"]}
            normalized = {k: filters.get(k) for k in
                          ("host", "member", "manager", "state", "task", "include_exited", "limit")}
            binding = {"caller": caller, "scope": basis, "filters": normalized}
            try:
                after = cursors.read(filters["cursor"], binding, secret) if filters.get("cursor") else None
            except cursors.InvalidCursor as exc:
                raise ItemsError(409, "cursor_changed", str(exc)) from None
            params = [caller["task_id"], identity["member_id"], caller["global"] or caller["owner"],
                      caller["pid"], caller["daemon_id"], caller["workspace_uids"], filters["include_exited"]]
            if caller["task_id"] and not (caller["global"] or caller["owner"]):
                await _check_task_graph(conn, params)
            # Resolve a label only inside the authorized set; duplicate labels
            # report ambiguity without disclosing unrelated host identities.
            host = filters.get("host")
            if host:
                hosts = await conn.fetch(_SCOPED + """
                    SELECT DISTINCT daemon_id, (daemon_id = $8) AS exact FROM scoped
                    WHERE daemon_id = $8 OR host_label = $8
                    ORDER BY exact DESC LIMIT 3""", *params, host)
                exact = next((r["daemon_id"] for r in hosts if r["daemon_id"] == host), None)
                if exact:
                    host = exact
                elif len(hosts) > 1:
                    raise ItemsError(409, "ambiguous_host", "host label matches multiple visible daemons")
                elif hosts:
                    host = hosts[0]["daemon_id"]
                else:
                    host = ""  # no authorized match; no information about other hosts
            args = params + [host, filters.get("member"), filters.get("manager"),
                             filters.get("state"), filters.get("task")]
            where = """
                WHERE ($8::text IS NULL OR daemon_id = $8)
                  AND ($9::text IS NULL OR resolved_member = $9 OR member_handle = $9)
                  AND ($10::text IS NULL OR manager_member_id = $10 OR manager_handle = $10)
                  AND ($11::text IS NULL OR
                       CASE WHEN $11 = 'exited' THEN exited_at IS NOT NULL
                            ELSE visible_state = $11 END)
                  AND ($12::text IS NULL OR task_id::text = $12)
            """
            unknown = await conn.fetchval(_SCOPED + "SELECT count(*) FROM scoped " + where +
                " AND (engine IS NULL OR engine NOT IN ('claude-code','claude','codex','bash','shell'))", *args)
            hosts = await conn.fetch(_SCOPED + """
                , visible_hosts AS (
                    SELECT daemon_id, max(reported_at) AS session_as_of
                    FROM scoped """ + where + """
                    GROUP BY daemon_id
                ), known_hosts AS (
                    SELECT daemon_id, session_as_of FROM visible_hosts
                    UNION
                    SELECT d.daemon_id, NULL::timestamptz FROM org_daemons d
                    WHERE $13::boolean AND NOT EXISTS
                        (SELECT 1 FROM visible_hosts h WHERE h.daemon_id=d.daemon_id)
                )
                SELECT h.daemon_id, h.session_as_of, d.seen_at AS org_as_of
                  FROM known_hosts h LEFT JOIN org_daemons d USING (daemon_id)
                 ORDER BY h.daemon_id LIMIT 101""", *args,
                bool((caller["owner"] or caller["global"]) and
                     not any(filters.get(k) for k in ("host", "member", "manager", "state", "task"))))
            page = await conn.fetch(_SCOPED + "SELECT * FROM scoped " + where + """
                  AND engine IN ('claude-code','claude','codex')
                  AND ($13::text IS NULL OR (daemon_id,session_uid,pid) > ($13,$14,$15))
                ORDER BY daemon_id,session_uid,pid LIMIT $16""", *args,
                *(after or [None, None, None]), limit + 1)
            selected = page[:limit]
            pids = [r["pid"] for r in selected]
            # Bounded per session; truncation is explicit, not a claim of no holds.
            held = await conn.fetch("""
                SELECT * FROM (
                    SELECT h.pid, b.id::text AS board_id,
                           CASE WHEN length(b.slug) <= 200 THEN b.slug END AS slug,
                           length(b.slug) > 200 AS slug_omitted,
                           i.number, left(i.title, 200) AS title, length(i.title) > 200 AS title_truncated,
                           i.status, left(h.role, 40) AS role,
                           row_number() OVER (PARTITION BY h.pid ORDER BY b.slug,i.number) AS rn,
                           count(*) OVER (PARTITION BY h.pid) AS total
                      FROM item_holders h JOIN items i ON i.id = h.item_id
                      JOIN boards b ON b.id = i.board_id
                     WHERE h.pid = ANY($1::text[]) AND i.archived_at IS NULL
                       AND i.link_archived_at IS NULL AND i.status NOT IN ('done','dropped')
                ) x WHERE rn <= 50 ORDER BY pid,rn""", pids)
            by_pid = {}
            for h in held:
                by_pid.setdefault(h["pid"], []).append(h)
            result = []
            for row in selected:
                items = by_pid.get(row["pid"], [])
                activity = project_activity(row["state"], row["reported_at"], row["state_since"], row["agent_state"], at)
                result.append({"session": {"pid": row["pid"], "uid": row["session_uid"],
                               "name": row["name"], "engine": row["engine"],
                               "recorded_model": row["recorded_model"],
                               "recorded_model_source": row["recorded_model_source"]},
                    "host": {"daemon_id": row["daemon_id"], "label": row["host_label"]},
                    "task": {"id": str(row["task_id"]) if row["task_id"] else None,
                             "name": row["task_name"], "name_truncated": bool(row["task_name_truncated"])},
                    "member": {"id": row["resolved_member"], "handle": row["member_handle"],
                               "acting_as": row["resolved_role"], "status": row["member_status"],
                               "manager_id": row["manager_member_id"], "manager_handle": row["manager_handle"],
                               "source": ("open_main_incarnation" if row["resolved_role"] == "main"
                                          else "matching_fresh_heartbeats" if row["resolved_role"] else "unknown"),
                               "stamp_conflict": bool(row["stamp_conflict"]),
                               "attribution_as_of": (at if row["resolved_role"] == "main"
                                   else min(row["org_as_of"], row["reported_at"]) if row["resolved_role"] else None),
                               "incarnation_id": row["incarnation_id"],
                               "occupancy_generation": row["incarnation_generation"], "org_as_of": row["org_as_of"]},
                    "items": [{"board": h["slug"], "board_id": h["board_id"],
                               "board_slug_omitted": h["slug_omitted"], "n": h["number"],
                               "title": h["title"], "title_truncated": h["title_truncated"],
                               "status": h["status"], "role": h["role"]} for h in items],
                    "items_more": bool(items and items[0]["total"] > len(items)),
                    "as_of": row["reported_at"], "exited_at_api_clock": row["exited_at"], **activity})
            next_cursor = None
            if len(page) > limit and selected:
                last = selected[-1]
                next_cursor = cursors.issue([last["daemon_id"], last["session_uid"], last["pid"]], binding, secret)
            return {"as_of": at, "scope_basis": {**basis, "as_of": at}, "rows": result,
                    "more": next_cursor is not None, "next_cursor": next_cursor,
                    "coverage": {"partial": True, "unknown_kind_omitted": unknown,
                                 "reason": "host enrollment and missing heartbeats are not a complete census",
                                 "hosts": [{**dict(h),
                                     "session_freshness": ("unknown" if h["session_as_of"] is None
                                         else "stale" if h["session_as_of"] < at - timedelta(seconds=90)
                                         else "fresh")} for h in hosts[:100]],
                                 "hosts_more": len(hosts) > 100},
                    "pagination": "each page is a separate snapshot"}
