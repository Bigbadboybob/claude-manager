# Org core API (org model S2)

The schema, API and daemon surface behind [the org model](org-model.md) §4, §6,
§9.1–9.2, §12 and §14. S2 records who is who; it enforces nothing. Holds stay
keyed by participant id, and session scope is unchanged until S6/S7.

## Schema (`sql/028_org_core.sql`)

| Table | Holds |
|---|---|
| `members` | One durable role per row: `id` (`mem_…`), `handle` (unique per `org_id`), `kind` (`agent`/`continuous`/`owner`), `status`, `manager_member_id` (only Owner has none), `primary_team_id`, `occupancy_generation`, profile fields, `revision`. |
| `member_aliases` | Every handle a member has had, plus `<slug>-orchestrator` for continuous members. Never deleted, so a handle is never reused for someone else. |
| `incarnations` | The sessions that worked as a member. At most one open `main` per member and one open row per pid. |
| `continuous_members` | `(daemon_id, continuous_id)` → member. |
| `team_memberships` | Member × team, with a role. |
| `initiatives` (altered) | Teams are initiatives: `kind` gains `team`, `coordinator_task_id` becomes nullable, and new columns `parent_team_id`, `lead_member_id`, `charter`, `standing_rules`, `team_status`, `org_id` and `team_revision`. |
| `org_events`, `org_changes` | The audit log, and the change feed (`seq`, `subject_type` member\|team, `subject_id`, `op`). |
| `org_workspace_marks` | Members' home workspaces: `central` → `pushed` → `confirmed`. |

Two triggers refuse manager cycles, unknown or retired managers, and parent-team
cycles in the same transaction, with `409 {code: "cycle" | "unknown_manager" | …}`.
Every org write takes the task feed's advisory lock, so cycle checks serialize
and `org_changes.seq` is commit-ordered. The migration takes that lock as its
first statement, before its ALTERs lock `initiatives`, so it waits for live API
writers instead of deadlocking with them. It is idempotent, makes no row
UPDATEs, and seeds `mem_owner` (handle `owner`). A CHECK keeps every kind but
`team` tied to a coordinator task, and `GET /initiatives` leaves out teams
unless `kind=team` is asked for, so older clients never meet a taskless row.

## HTTP (`api/org.py`, prefix `/org`)

Reads: `GET /snapshot?since_version=` (`{unchanged: true, version}` when nothing
changed), `/changes?since=`, `/members/by-pid?pid=`, `/members/{ref}` (id, handle,
alias, `main/<handle>` or `owner`), `/teams/{id|slug}`, `/roster`, `/events`.
`GET /tasks/{id}` also carries `org: {team, lead_member, members}`.

Writes. The daemon stamps `actor`, and Owner is pid `owner`.

**Trust model.** "Owner-only" is enforced by the daemon, not by the API. The
daemon sends pid `owner` only for operator-socket callers, which carry the
operator token. The API sits behind the shared bearer token and trusts
`actor.pid` as sent, so any holder of that token could claim to be Owner. In S2
this is cooperative, like the items API. A real server-side check is a tracked
S6/S7 item.

| Call | Who | Notes |
|---|---|---|
| `POST /roster/apply {rows:[{proposal_id, approve?, handle?, kind?, title?, team_id?, manager?, home_workspace?}], request_id}` | Owner | Each row is applied in its own transaction, against proposals regenerated under the lock inside it, so a row sees the rows applied before it. Per-row rejections are `stale`, `handle_taken`, `cycle`, `pid_already_member`, `unknown_manager`, `unknown_ref`, `coordinator_taken` and `conflict`. `approve: false` records a decline. |
| `POST /adopt {pid, handle, title?, kind, team_id?, manager?, home_workspace?, request_id}` | Owner | Returns `{member, home_mark: {state}}`. |
| `POST /members/{ref}/move {manager?, primary_team_id?, reason, expected_revision?}` | Owner, or a member above the target in its manager chain | `primary_team_id` is a team id or slug (`422 unknown_team`). |
| `POST /members/{ref}/retire {reason, request_id?}` | Owner, or a member above the target in its manager chain | Status `retired`. Its reports move up to its manager (one `move` event each, `via: "retire"`) and the teams it leads lose their lead. Its main session's open board holds go to its manager's main session, or Owner (`hand_back_holds`, as for closing an assistant); a vacant manager's stay, listed as `kept`. Returns `{member, …, reports_moved, teams_unled, holds}`. Incarnations and sessions are untouched: nothing is killed, and the handle stays reserved. `409 already_retired`. |
| `POST /members/{ref}/reinstate {reason?, request_id?}` | Owner | Undoes a retire: the status before the retire comes back (`active`, `vacant` or `suspended`), and it leads again any team from the `retire` event that has had no lead since (`teams_led_again`). Reports that moved up stay put. `409 not_retired`; `409 retired_manager` when its manager is retired too (reinstate that one first). |
| `POST /teams`, `PATCH /teams/{ref}` | Owner | Team create and edit. A `coordinator_task_id` is attached to the team as in `create_initiative`; a task that already belongs to another initiative is `409 coordinator_taken`. `org_db.create_team_conn(..., authorized_by=…)` is the trusted in-process creator for the items epic path. |

**Move and retire authority.** Owner, or an *active* member up the target's
manager chain whose main session is the caller (an open `incarnations` row
with `role = 'main'`). Anyone else gets `403`: `not_member` when the caller is
not an active member's main session, `not_manager` when it is but the target
is not below it (sideways, its own manager, or itself). `org_db._require_chain`
is the one check. A non-Owner move may set the new manager only to the caller
itself or to a member in its own reporting subtree; any other destination
(another lead's branch, or Owner) is `403 needs_owner`: a cross-org move is
Owner's until an accept step exists.

**Roster managers.** A lane reports to the lead of its board's team by
default: the lead member, or the lead proposed in the same batch
(`manager_handle`). Apply orders continuous, then leads, then lanes, so a
lane row listed before its lead still lands under it; a row's `manager`
overrides the default.

Every write takes an optional `request_id`. A replay with the same actor,
method and `request_id` returns the stored answer, and that check is repeated
under the lock. Database refusals always come back as org errors: a missing
reference is `422 unknown_ref`, anything else is `409 conflict`, never a 500.

### Roster

`GET /roster` regenerates its proposals on every call. Sources, in priority order:

1. continuous orchestrators (`continuous:<daemon>:<id>`);
2. initiative and division leads by today's lead rule (`lead:<initiative id>`,
   or `lead:board:<board id>` for a root-task board with no team);
3. long-lived lanes (`lane:<pid>`): at least `CM_ORG_LANE_MIN_AGE_S` old
   (default 24 h), holding board items, and not continuous or workflow sessions.

Proposals are deduplicated by pid, coordinator and continuous ownership. A
handle that collides gets a numeric suffix, and an alias is never taken.

## Heartbeat

Session rows gain `workspace_id`, `worktree_path`, `continuous_task_id`,
`managed_by_uid`, `workflow_run_id` and `spawned_age_s`. A heartbeat with no
marks to confirm or push never takes the feed lock. The reply gains
`org: {members: {session_uid: member}, workspace_marks, capable}`. The daemon
caches `members`, and `ping()` returns that cache as `member`.

**Home-mark push.** Each beat replaces the daemon's row in `org_daemons`
with its `capabilities`; a beat without the field (a brain from before S0b)
records none. Marks are sent only to a daemon advertising `org_marks_v1`
(`capable` in the reply); `org_preserve_v1` alone is not enough, since that
brain would drop them. The daemon sets the workspace's `org.workspace_kind`
(`home`/`stream`) and `org.office_of` (the member id), finding it by id and
then by a unique path, persists it, and acks with `acked_mark_ids` on the
next beat, which confirms the mark. A mark for a workspace the daemon does
not hold stays unacked and is re-sent until its attempts run out. Until a
host advertises `org_marks_v1`, its marks stay `central`.

## Daemon RPCs and the MCP tool

`org.snapshot`, `org.member`, `org.team` and `org.roster` are reads.
`org.roster_apply`, `org.adopt` (`session_uid` may stand in for `pid`; the
daemon fills in its own id), `org.team_create`, `org.team_update`, `org.move`,
`org.retire` and `org.reinstate` are writes. The roster, adopt and reinstate
are refused for session callers. An old API (404, or 405 on a route it lacks)
answers `unknown_method` with `unsupported: … needs an API deploy`, so the TUI
turns the feature off.

The `org` MCP tool keeps `me`, `reports` and `division`, and adds `member`, plus
`team` as `division`'s org-model name. `me` adds `member`, `acting_as`,
`manager_chain`, `teams` and `member_reports`. `action="adopt"` is Owner's. From
the operator socket it routes to `org.adopt`. `action="move"` (`member`,
`manager` and/or `team`, `reason`; `team=""` or `"none"` clears the primary
team), `action="retire"` (`member`, `reason`) and
`action="reinstate"` (`member`) route to `org.move` / `org.retire` /
`org.reinstate` from any caller, with the caller as actor, so the API decides
who may. The tool generates a `request_id` when none is passed.
the operator socket it routes to `org.adopt`.

## Assistants (O5)

Owner decision O5 (doc/org-model.md §4.2, §4.3, §15.1): an assistant is as
durable as its member's main session.

**Who is whose.** The daemon stamps a spawned session's org object with
`member_id`, `acting_as` and `member_via` (`daemon/src/assistants.rs`).
- At spawn, the child of a member's session (its main, or one of its
  assistants) works for that member. A fork works for its source's member.
- Each beat, a backfill covers sessions spawned before this. It walks the
  `managed_by_uid` chain. Only when that chain is broken (a spawner gone with
  no record) does it fall back to the task's parent chain, up to a task with a
  member's main session on this daemon. A chain that ends at a live session
  with no member stays unstamped.
- Never stamped:
  - a session with no spawner (Owner- or operator-started);
  - a shell (`bash`; only Owner closes, kills or restarts one);
  - a member's main, which comes only from the member cache and always wins.
- Nothing is stamped until the cache has heard from the API.

**Assistants and workers.** A continuous member's spawns, and everything
below them, are `acting_as: "worker"`. Workers are attributed but not
durable: `mark_subtask_done`, the terminal sweep and the reaper treat them
exactly as today, and their checkouts are never promoted. Every other
member's spawns are `"assistant"`.

**Where to read it.**
- `list_sessions` rows (live and exited) carry `member: {member_id, acting_as}` or `null`.
- Manifest entries carry the stamp in `org`, and the backfill broadcasts `Updated {member, org}`; the TUI mirrors it onto the row.
- The heartbeat reports `member_id` / `acting_as` per session into `org_session_facts` (`sql/031`).
- `GET /members/by-pid` answers `acting_as: "assistant" | "worker"` with the member's detail.
- The daemon's `org.member` answers for its own assistants locally.

**Lifecycle.**
- Finishing work never closes an assistant or removes its checkout: the terminal sweep (planning or board close), `mark_subtask_done` (daemon and TUI), teardown and Owner's A-D on its task leave it running, only unassigned, as they do a member's main session. `workspace_kept` is `stream` or `home` by kind, else `member` or `assistant`.
- A checkout minted `ephemeral` for an assistant becomes a `stream` of the member's office (`office_of`, `stream_of: "assistant"`). Unclassified and centrally marked workspaces are left alone.
- How an assistant's end releases its checkout:

| How it ends | Effect |
|---|---|
| `org(action="close_assistant")` / `org.close_assistant`, Owner's A-w | Authority from the API; holds go back to the member; `assistant_closed_at` is set; a claimed stream goes back to `ephemeral` at once. |
| An agent's `kill_session` (the daemon route) | The same release at once, under `kill_session`'s own scope. The holds go back asynchronously (`ended_by: "kill_session"`, event `assistant_killed`). |
| Anything else: an operator kill (A-R restart, workflow fresh-role respawn, A-w's fallback, the TUI-routed kill), a crash or memory cap, a natural exit | A plain kill. The daemon notes when the stream first had no live assistant (`assistant_idle_since`) and releases it after 7 days. A revive within that window clears the clock. |

- The reaper protects `home`/`stream` workspaces, and checkouts whose assistant was live within 7 days and never closed (`assistant_closed_at`). It reads the tombstone's `exited_at` and the stream's `assistant_idle_since`.
- Member-less sessions and workers keep today's teardown.

**Closing one deliberately.**

| Call | Who | Effect |
|---|---|---|
| `POST /org/assistants/close {pid, member_id, reason, ended_by?, request_id?}` | Owner, the member's main session, or an active member up its manager chain (`_require_chain(..., self_ok=True)`, the check shared with move and retire). `ended_by: "kill_session"` skips it. | `403 not_member` / `not_manager` otherwise; `409 not_an_assistant` for a main or a worker. Hands the assistant's open holds to the member's main pid in their roles (`hand_back_holds`, shared with retire); a vacant member's holds stay, listed as `kept`. Records the event and marks the facts row `closed`. |
| `org.close_assistant {session_uid, reason?, request_id?}` (daemon), `org(action="close_assistant", …)` (MCP), Owner's A-w (off the input thread) | as above | The API answers first. The daemon re-checks that the session is still that member's assistant, then closes it with `killed_by: "<uid> (close_assistant)"`. Against an API without the route, only Owner or the member's main may close it, and holds stay where they are. |

**Promoting one** (§4.2 "Promote an assistant"). The assistant becomes the main session of a new member. It keeps its session, uid, context, checkout and holds (holders are pids).

| Call | Who | Effect |
|---|---|---|
| `POST /org/assistants/promote {actor, pid, handle, title?, team?, manager?, reason, request_id?}` | Owner, the member M's main session, or an active member up M's manager chain acting as its main (`_require_chain(..., self_ok=True)`) | `403 not_member` / `not_manager` otherwise. `404 not_found` for no live session; `409 not_an_agent` (a shell), `already_member` (an open incarnation), `continuous` (the scheduler owns it), `not_an_assistant` (no stamp, a worker, or a retired member). The new member is created through `_create_member` (the hire/adopt path: `handle_taken`, `pid_already_member`), with `kind=agent` and manager M by default. A `manager` must be in the caller's subtree, and a `team` one it leads. Otherwise it is `403 needs_owner` (the rule move uses, `_require_destination`). The session's checkout becomes the member's `home` (mark pending until the daemon acks). The facts row is marked `promoted`, so a stale beat with the old stamp does not turn it back. Records `assistant_promoted` on M and bumps the org version, so F6 and the roster redraw. Idempotent via `request_id`. Returns `{member, promoted_from, notes, home_mark, version}`. |
| `org.promote {session, handle, reason, title?, team?, manager?, request_id?}` (daemon), `org(action="promote", …)` (MCP) | as above | Refuses locally first, before asking the API: the session must be live on this host and must not be a shell, a main, continuous, a worker or a member-less session. After the API answers, the daemon unstamps the session, records it as the new member's main in its member cache (so the backfill never re-stamps it), marks its workspace `home` with `office_of` set to the new member, and drops `stream_of`. It then sends the session a native `[cm-org]` notice: "You are now member <handle> (main session), reporting to <manager>". |

**Reuse before spawning.** `board().free_capacity` lists the assistants
(not workers) of the board's members, wherever their task is now. That means
members whose main session is on the board, plus the board team's lead and
members. They show as `rev1 (for ep)` and carry `assistant_of`. A
worktree-cap refusal (`worktree limit reached`) names the caller's member's
idle assistants, with how to reuse them (`send_input`) or free their
checkouts (`close_assistant`).

**Gate.** `assistant_guard_v1` is brain-only, so the rollback pin must have
it too. The status mirror writes a task's close only when every fresh daemon
advertises both it and `lifecycle_guard_v1`.
