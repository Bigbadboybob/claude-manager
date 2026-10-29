# Cross-host session roster — proposal for #76

Status: design for coordinator/Trim-Review, based on `7d47b88`; no implementation,
new permissions, deployment, live database query or session control. Coordinator
rulings in CM message `1897f4a7` approve the surface, scope, freshness and limits
below. Trim-Review passed the initial spec conditionally on C1/C2 and N1–N7;
coordinator message `585b05b6` rules C1/C2. Clarifications below are part of the
build contract, not a new policy choice. The aim is
one read-only view for a lead coordinating work across hosts, including about
75 sessions on one host. It must show partial/stale evidence honestly, and must
not infer a session's work or authority from a shared checkout or display name.

## Recommendation

Add a dedicated `session_roster` MCP read, backed by one batched planning-API
snapshot. Keep the existing `list_sessions` host-local and `board()` item-first;
neither can enumerate remote sessions without items, taskless sessions or
sessions holding items on several boards. Do not fan out synchronous SSH or
create per-host operator sessions. A board can later link to this read; it is not
the source of truth.

Candidate call: `session_roster(host?, member?, manager?, state?, task?,
include_exited=false, limit?, cursor?)`. Host accepts an exact daemon ID; a display host name must
resolve uniquely or report ambiguity. Default scope is the caller's org subtree (the caller's member, reporting
members, their mains and explicitly stamped assistants/workers) **plus** the
caller's task tree; preserve the existing taskless same-workspace rule on the
caller daemon, without treating equal workspace strings on different hosts as
proof of shared scope. Owner and daemon-verified global-permissions callers see all rows. Filters narrow
this scope, never widen it. Default excludes shells;
`include_shells` can be a later explicit option, not an assumption that a shell
is an agent. A known `bash`/`shell` engine is excluded; an absent/unclassified
engine cannot prove an agent, so it is excluded by default and counted only
within already-authorized scope as `unknown_kind_omitted` with partial coverage.
A TUI-only row with known non-shell engine is included with unknown activity. A compact row contains stable `(daemon_id, session_uid, pid)`, host
label, name/engine/`recorded_model`/kind, task ID and task name, member/role and
manager with provenance, held open items `(board slug, number, title, status, holder role)`, activity
state and its source/time, and separately reported limit/recovery observations.
Nullable/unknown is explicit. Return `as_of` for the database read, per-host beat
and org-fact timestamps, partial/coverage reasons, and a continuation when the
result is bounded. Never return prompts, transcript, raw environment, account
identifiers, tokens, recovery journal contents or terminal text.

Recommend a single bounded page sized to cover the observed ~75-session use case
(e.g. 100 rows as an engineering starting point, not a measured maximum), with
keyset order `(daemon_id, session_uid, pid)` and `more`/`next`. Apply visibility
and filters **before** counting, sorting or paging. A cursor is opaque, bounded and integrity protected; it binds the verified
caller PID/incarnation, exact normalized filters, scope basis and last key. A
mismatched caller, filter or basis is refused, never silently reused. Each page
uses one read-only repeatable-read transaction; separate page requests are not
the same snapshot.
Say so in the response: a pagination walk is not a consistent census if rows or
permissions change, and a refresh is required to reconcile it. Do not use the
org/task feed version as a heartbeat watermark: heartbeats do not enter that
feed. If a consistent multi-page census becomes necessary, review a bounded
snapshot/token design separately; do not silently keep a DB transaction open.
The initial implementation caps a row at 50 holds and marks `items_more`;
it does not claim the truncated hold list is complete. A visible host summary
is capped at 100 and marks `hosts_more`. A task scope above 10,000 nodes or
containing a cycle fails closed with an explicit error; these are engineering
bounds, not measured fleet limits. Large-scope performance requires isolated
measurement before rollout.

## Existing facts and exact joins

* `daemon/src/items/heartbeat.rs:19`, `:88`–`:112`, `:115`–`:167`, `:272`–`:290`:
  each daemon sends a full session snapshot every 30s (with debounced pokes),
  including stable PID/UID, task, engine/name, state, `reported_done` and
  serialized `agent_state`. TUI-only rows are explicitly `unknown`. Model is
  not currently in the heartbeat: `DaemonSession.model`/`model_source` are
  recorded launch/resume settings (`daemon/src/session.rs:661`–`:668`), not
  proof of an engine's active model. Export them as bounded optional fields and
  persist nullable `recorded_model` and `recorded_model_source` values in the
  same heartbeat upsert. A missing field overwrites the previous value with
  null; old/absent data stays unknown, never inferred from the engine. The API limits a heartbeat to 2,000 session rows (`api/items_models.py:256`–`:267`).
  `dispatch/items_db.py:1154`–`:1170`, `:1193`–`:1266` validates and stores
  per-PID `session_states` (`sql/017_items.sql:115`–`:136`). A missing PID in
  a successfully processed full beat is marked exited; dropped invalid rows
  are returned to the daemon, and a missing row is not independent proof of a
  process exit. Old exited rows are pruned (`api/board_engine.py:830`–`:835`).
* `daemon/src/items/org.rs:819`–`:845` sends workspace, continuous/workflow
  and stamped assistant/worker membership. `api/items.py:205`–`:220` executes
  `org_db.heartbeat_extras` **after** the items heartbeat as a best-effort
  separate transaction; it may fail even when the session beat succeeded.
  `org_session_facts` has its own `reported_at` and can retain an existing
  member when an incoming stamp is missing (`dispatch/org_db.py:2021`–`:2053`).
  Do not label both tables as one atomic daemon snapshot.
* For a main, join the unique open `incarnations` row by PID (`sql/028_org_core.sql:
  140`–`:159`) and current `members`; for assistants/workers use the explicit
  `org_session_facts.member_id/acting_as` stamp (`sql/031_org_assistants.sql:1`–`:8`).
  Main rebind changes incarnation and hands back holds in one org transaction
  (`dispatch/org_db.py:1296`–`:1355`); a stale old session is not the new main.
  Main identity comes from the open canonical incarnation and does not depend
  on freshness of `org_session_facts`; it is a current seat, not proof of a
  running process. The org upsert retains a previous member stamp when a beat
  omits it while still refreshing `reported_at`; that timestamp alone cannot
  prove the stamp was reaffirmed. Persist a roster-only member/role observation
  from the session beat, clearing it on omission, and require the fresh session
  and org observations to agree for assistant/worker attribution. Its own
  API-clock timestamp must equal the session beat timestamp: an older API
  updating the session row during rollback normally cannot refresh a retained
  stamp. Exact repeated API-clock timestamps across such a write are not a
  durable beat identity; the rollout must treat clock discontinuity or an
  uncertain rollback window as partial until a new-writer beat is observed.
  Old writers
  without that observation stay unknown. Closed/promoted, stale, absent or conflicting
  stamps need explicit unknown/conflict rather than guessed membership. Show member status and
  incarnation/generation where available; membership is a roster fact, not a
  grant to control that session.
* Join `session_states.task_id` to `tasks.id` and display `tasks.name`; missing
  task or taskless is distinct from an empty title. Join actual current
  `item_holders.pid` to non-archived, open items and their boards/holder roles
  (`sql/017_items.sql:29`–`:67`, `dispatch/items_db.py:1115`–`:1126`,
  `sql/029_org_s3_groups_placement.sql:64`–`:66`). A session can hold multiple
  items across boards; a task's placed item is not automatically held by all
  of its sessions. Historical unplaced links can be reactivated
  (`dispatch/placement.py:318`–`:345`, `:381`–`:395`); do not duplicate work
  from old task links or infer a hold from task identity.
* State freshness follows the existing 90s `STATE_FRESH_S` convention
  (`dispatch/items_db.py:18`, `:789`–`:802`): label a row `stale` after 90s,
  do not present last-known state as current, and preserve that last-known
  state and beat time separately.
  `org_daemons.seen_at` is independently updated by the best-effort org half
  (`sql/028_org_core.sql:228`–`:232`, `dispatch/org_db.py:2103`–`:2110`); it is
  not a complete host-enrollment inventory. Show missing/stale hosts and an
  incomplete-coverage indicator where known; no row or an old exited row is
  never proof that a whole host is empty. Hosts that never reported cannot be
  discovered from `session_states` alone. Host labels can collide or change.

## Activity, limits and recovery

`AgentState` has state/since, observation time, source, last turn status/end,
latest accepted input/engine start and limited `detail` (`daemon/src/agent_state.rs:
13`–`:72`, `:166`–`:183`). None is a universal last-activity timestamp:
`agent_state.observed_at` is host-clock observation, `agent_state.since` is
host-clock state entry, and `last_turn.ended_at` is a host-clock turn boundary.
`session_states.reported_at` and reconstructed `state_since` are API-clock
values. Label clocks and use only API `reported_at` for the 90s freshness rule;
never compare host clocks across machines to establish ordering. Label these individually; a quiet
PTY, heartbeat or `reported_done` does not prove work succeeded. Unsupported,
legacy, TUI-only, stale and malformed data stay unknown. Never scan transcripts
or infer identity from co-tenancy to fill gaps.

The heartbeat can carry observed `detail.error_kind`, `retrying`, `resumes_at`
when the state source supplied them, but these do not by themselves establish
account-wide quota, retry eligibility or a successful recovery. Claude limit
records and per-session recovery episodes are maintained on the owning host
(`daemon/src/claude_account_recovery.rs:46`–`:95`, `:270`–`:303`, `:695`–`:715`);
Codex continuation recovery has separate local receipts and semantics
(`daemon/src/codex_recovery.rs:1`–`:28`, `:60`–`:103`). Continuous tasks have
their own account block/hold state. In the first read, show observed error kind,
source and optional engine-reported resume time, and explicitly mark
`recovery: unknown/not_exported`; do not call absence healthy or conflate a
Codex continuation failure with an account limit.

A later separately reviewed slice can publish a small versioned, content-free
per-session recovery summary from the owning daemon's already maintained state,
with explicit evidence source, episode/phase, as-of and unknown/error states.
It must distinguish detected, deferred, attempted, queued/submitted, confirmed,
failed/uncertain and not-applicable only when the actual engine's receipt proves
that state. Do not export account fingerprints, native IDs, paths or transcript
content; no extra recovery action, timer or collector. Do not read journal files
under the session registry lock. A per-row optional field is compatible with
older heartbeat consumers that ignore unknown nested keys, but old/new readers,
capability negotiation, DB storage, size and rollback must be reviewed before
claiming the summary is available. `api/items_models.py:256` makes the *outer*
heartbeat body strict; compatibility must be tested rather than assumed.

## Authorization and rollout boundary

`list_sessions` currently authorizes local session callers by task tree or
workspace, with a manager-chain widening and global/operator exceptions
(`daemon/src/control/dispatch.rs:1124`–`:1157`,
`daemon/src/control/methods.rs:2687`–`:2720`, `:2722`–`:2769`). The org
read-authority batch is made by the daemon after identifying its local caller
and deliberately falls back on API failure (`daemon/src/items/org.rs:191`–`:269`).
That helper builds target PIDs using the **local** daemon ID (`:237`–`:245`):
it cannot simply be reused unchanged for remote sessions. Snapshot and
revalidate the caller incarnation around any remote request; do not trust a
caller-supplied PID, task, global bit or display name. Unknown scope must not
be counted or returned as authorized; show partial scope without leaking
unreadable rows or their counts. No new control permission follows from read.

The coordinator ruled **trusted-host model**, matching S4 provenance: the
caller daemon authenticates its own local session/operator and stamps the
request from its registry. The API enforces the org manager-chain and task-tree
visibility filter **server-side before pagination**, using its own current
member/incarnation, task and stamped session records, rather than treating an
actor-supplied member or claimed ancestry as the result. The server-side task
closure starts at the validated task root and repeatedly includes both
`tasks.parent_task_id` descendants and rows whose `metadata.filer.task_id` is
already in the closure. This is the central planning-row creator edge for
`propose_task`; daemon-local `agent_task_edges` are not consulted. Missing,
malformed or cyclic links cannot create reachability.

The subtree read applies to a stamped assistant or worker caller as well as a
main, as ruled for this roster. It is read-only and does not change the
main-only widening used by existing session-control or other reads.

Reviewer note N8: task metadata is a writable bag and `metadata.filer.task_id`
is not currently guarded by `update_task`; a permitted writer can change that
edge. This is part of the approved trusted-host scope and has a similar
implication to the existing writable parent-task edge. It is not evidence that
the edge is tamper-proof; guarding task metadata is separate work.

The daemon stamps its validated global/Owner condition; ordinary caller
parameters cannot request that exemption. Before the call it confirms that a
claimed main/assistant is still the locally registered caller, capturing its
member/role, task, workspace, process/generation and global state. The API
returns `scope_basis` containing the member ID/revision and incarnation (when
applicable), global/Owner basis, task roots and snapshot `as_of`. Afterward the
daemon confirms the same local caller/task/global/process generation and that
current member and revision match the basis; mismatch returns `scope_changed`
and retries the whole read once, then returns the error. For taskless/unbound
callers, null membership is explicit and only independently established scope
can grant rows. A privileged operator is validated through the existing strong
operator path, rather than fabricating a member.

Current `org_db.member_summary` omits revision (`dispatch/org_db.py:299`–`:305`)
and the daemon cache holds only local mains (`dispatch/org_db.py:2139`–`:2144`),
while assistant stamps carry no revision (`daemon/src/assistants.rs:72`–`:83`).
A local heartbeat cache alone therefore cannot perform this current-version
comparison. The proposed implementation uses one O(1) central caller-PID
recheck after the roster query (the existing `member_for_pid` resolves main or
stamped assistant and returns member detail/revision), followed by local caller
revalidation. It is one batched roster query plus one verification request,
never N+1; when that recheck is unavailable or inconsistent, fail closed.
The coordinator approved this concrete mechanism in message `7fcea50a`. A
recheck must not assert cross-table atomicity beyond the actual transaction. A local
API outage returns an explicit unavailable/partial result and never silently
falls back to a broad remote listing.

For a taskless caller the daemon also supplies its bounded, sorted snapshot of
session UIDs currently in the caller's local workspace, and revalidates that set
after the read. The API intersects it with the caller daemon ID. A heartbeat's
workspace label alone is not a current authorization proof. If the local set
exceeds the request bound, refuse rather than return an unlabelled truncation.

The existing service boundary is important to state accurately: `api/auth.py:
7`–`:10` authenticates a shared bearer and `api/items.py:1`–`:4` trusts
host-stamped actors under that bearer. This is the ruled trust assumption, not
cryptographic per-session isolation against a holder of the shared API token.
The server-side graph filter is a correctness/authorization check within that
trusted-host model; it must not claim that the bearer itself proves the
actor's identity. A stronger per-daemon/per-user credential is separate work,
not a prerequisite invented by this spec or a session-control grant.

## Contract fixtures for independent review

Each row's `as_of` is `session_states.reported_at`; org attribution carries its
own `org_as_of` and is unknown if absent, stale or conflicting. The API also
returns its database-snapshot `as_of`, scope basis, coverage and continuation. Illustrative
content-free row (the actual wire uses structured timestamps, no transcript):

```json
{"session":{"pid":"agent:d2:u2","uid":"u2","name":"worker","engine":"codex","recorded_model":"configured-model","recorded_model_source":"explicit"},"host":{"daemon_id":"d2","label":"host-b"},"task":{"id":"t2","name":"task"},"member":{"id":"mem_child","acting_as":"assistant","manager":"mem_lead","org_as_of":"2026-01-01T00:00:00Z"},"items":[{"board":"b","n":7,"title":"work","status":"active","role":"owner"}],"activity":{"state":"working","state_since":"2026-01-01T00:00:00Z","observed_at":"2026-01-01T00:00:01Z","source":"relay"},"limit":{"error_kind":null,"resumes_at":null,"recovery":"not_exported"},"as_of":"2026-01-01T00:00:02Z","freshness":"fresh"}
```

| Fixture | Expected result |
|---|---|
| Lead `L`, main `L`, child main on another daemon and stamped child assistant; unrelated member | All three scoped rows returned; unrelated row omitted before counts/pages. Manager is current canonical parent, not a display-name match. |
| Session on caller's task subtree without an org stamp | Included through task scope, member/role/manager unknown; no inference from co-tenancy or item holder. An unknown target outside an established scope is omitted. |
| Global or Owner, unrelated and unbound agent rows | All agent rows visible; default shell exclusion still applies. A session body claiming global/Owner or another member cannot widen scope. |
| Two daemons share host label, 75 authorized sessions on one | Exact daemon filter selects the requested host; ambiguous label is reported. A bounded page includes 75 without N+1 held-item or authority calls. |
| Task and member filters plus a held item on each of two boards | Filters intersect after authorization; both current exact-PID holds appear once. A task-linked item without that PID is not a hold. Archived A→B→A aliases do not create extra rows. |
| Old main rebinds to new main on another daemon | Current open incarnation identifies the new main, with its generation and moved holds. Old/stale conflicting facts are unknown or explicitly old, never a second current main. |
| Beat older than 90s, missing org half, dropped heartbeat row, previously seen host with no rows | Stale state is labelled stale with original beat timestamp; absent org is unknown; do not claim dropped/missing session or host is empty/exited with certainty. |
| `state_since`, `observed_at`, `reported_at`, turn end all differ | Preserve and label each; do not substitute any for universal last activity. |
| Claude reports `rate_limit` with `resumes_at`; Codex reports `previous_response_not_found`; old writer has neither | Show the reported kinds with their source/time and recovery `not_exported`; do not call either a successful recovery or claim no limit. |
| New host sends optional recorded model/source; old host or TUI-only row omits it | New API reads known values with source; old/absent is unknown. Old API ignores nested fields; old writers/new API and rollback do not reject the heartbeat or preserve a stale model as current. |
| Parent task, a direct filer edge and a filer-of-filer edge | Central closure includes all descendants and filer edges to a fixed point; a forged/unrelated root and daemon-only creator edge do not widen it. |
| Caller rebound, member revision or task/global state changes while read is in flight | API returns scope_basis; central version and local caller revalidation must match, otherwise `scope_changed` and one whole-read retry. Authority/API unavailable is explicit unavailable or safely limited, never a broadened fallback. |
| Same workspace string on two daemons, TUI-only with unknown engine, continuous worker | Local taskless workspace scope does not cross daemon boundary; unclassified engine is omitted and counted after scope; a stamped continuous worker is not promoted to main. |
| More rows than page cap, changed caller/filter, scope changes between pages | Cursor binds caller, normalized filters and scope basis; mismatches are refused. Stable key order and honest `more`/cursor for each page; scope before counts. Separate-page snapshot is explicitly non-atomic and requires refresh for a consistent census. |

## Small implementation route and checks, after design disposition

1. Add a single batched read-only API query and daemon/MCP wrapper under the
   ruled trusted-host scope. Export the recorded session model/source as an
   optional bounded field in the existing heartbeat, store it in nullable
   columns, and leave absent/older writers unknown. Do not call `held_items`
   or `authority-batch` once per row. Return explicit source timestamps and
   `recovery: not_exported`. Proposed boundaries: `sql/<next>_session_roster.sql`,
   `dispatch/items_db.py` (heartbeat persistence, same-upsert nulling) plus new
   `dispatch/session_roster.py` (scoped query), `api/items.py` (+ request
   models), `daemon/src/items/heartbeat.rs`, `daemon/src/items/rpc.rs` or new
   `daemon/src/items/roster.rs`, `mcp_server/server.py`,
   `mcp_server/tool_schemas.json`, `mcp_server/AGENT_GUIDE.md`; focused tests
   beside those modules. Add the read method to
   `RESTART_BARRIER_READ_ONLY_METHODS` and `mcp_server/control_client.py`'s
   `DAEMON_METHODS`, never to the board-token method list; include the optional
   MCP schema and guide. Document global as fleet-wide **read-only** visibility.
   Avoid session lifecycle, terminal input, org writes and recovery workers.
2. Separately specify and implement a safe owner-daemon recovery summary only
   when its semantics, authorization, strict-wire/rollback and capacity are
   reviewed. This is out of scope for the approved first slice; current
   heartbeat error evidence is the only limit observation shown, and recovery
   stays unknown.
3. Test at least two daemons with colliding host labels, ~75 synthetic sessions,
   multiple holds/boards and taskless/zero-hold sessions; main rebind versus
   stale assistant stamp; stale/missing/partial heartbeat and dropped row;
   org subtree plus parent/filer task-tree union, manager/host/member/state filters, spoofed
   member/global input, caller rebind mid-read, global/Owner callers and an
   unavailable authority API; filter before pagination/counts; state-since vs
   heartbeat observation time; legacy/no recovery, Claude and Codex distinct
   error observations, model known/unknown and old/new heartbeat compatibility.
   Verify query count and a bounded response without inventing measured
   production rates. Use an isolated labeled test target and scratch
   DB only under the coordinator's resource reservation. No tests or DB changes
   were run for this design.
