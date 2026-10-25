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
| `POST /members/{ref}/retire {reason, request_id?}` | Owner, or a member above the target in its manager chain | Status `retired`. Its reports move up to its manager (one `move` event each, `via: "retire"`) and the teams it leads lose their lead. Returns `{member, …, reports_moved, teams_unled}`. Incarnations and sessions are untouched: nothing is killed, and the handle stays reserved. `409 already_retired`. |
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
