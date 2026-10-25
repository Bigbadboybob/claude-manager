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
and `org_changes.seq` is commit-ordered. The migration is idempotent and makes
no row UPDATEs. It seeds `mem_owner` (handle `owner`).

## HTTP (`api/org.py`, prefix `/org`)

Reads: `GET /snapshot?since_version=` (`{unchanged: true, version}` when nothing
changed), `/changes?since=`, `/members/by-pid?pid=`, `/members/{ref}` (id, handle,
alias, `main/<handle>` or `owner`), `/teams/{id|slug}`, `/roster`, `/events`.
`GET /tasks/{id}` also carries `org: {team, lead_member, members}`.

Writes. The daemon stamps `actor`, and Owner is pid `owner`.

| Call | Who | Notes |
|---|---|---|
| `POST /roster/apply {rows:[{proposal_id, approve?, handle?, kind?, title?, team_id?, manager?, home_workspace?}], request_id}` | Owner | Each row is applied in its own transaction against a fresh generation. Per-row rejections are `stale`, `handle_taken`, `cycle`, `pid_already_member` and `unknown_manager`. `approve: false` records a decline. |
| `POST /adopt {pid, handle, title?, kind, team_id?, manager?, home_workspace?, request_id}` | Owner | Returns `{member, home_mark: {state}}`. |
| `POST /members/{ref}/move {manager?, primary_team_id?, reason, expected_revision?}` | Owner, or a member above the target in its manager chain | |
| `POST /teams`, `PATCH /teams/{ref}` | Owner | Team create and edit. `org_db.create_team_conn(..., authorized_by=…)` is the trusted in-process creator for the items epic path. |

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
`managed_by_uid`, `workflow_run_id` and `spawned_age_s`. The reply gains
`org: {members: {session_uid: member}, workspace_marks, capable}`. The daemon
caches `members`, and `ping()` returns that cache as `member`.

**Home-mark push (S0b contract).** Marks are sent only to a daemon whose
heartbeat body has `capabilities: ["org_preserve_v1"]`. That daemon applies
`reply.org.workspace_marks` and acks them with `acked_mark_ids` on the next
beat, which confirms them. Until S0b, marks stay `central`.

## Daemon RPCs and the MCP tool

`org.snapshot`, `org.member`, `org.team` and `org.roster` are reads.
`org.roster_apply`, `org.adopt` (`session_uid` may stand in for `pid`; the daemon
fills in its own id), `org.team_create`, `org.team_update` and `org.move` are
writes. The roster and adopt are refused for session callers. An old API answers
`unknown_method` with `unsupported: …`, so the TUI turns the feature off.

The `org` MCP tool keeps `me`, `reports` and `division`, and adds `member`, plus
`team` as `division`'s org-model name. `me` adds `member`, `acting_as`,
`manager_chain`, `teams` and `member_reports`. `action="adopt"` is Owner's. From
the operator socket it routes to `org.adopt`.
