# S6 plan: durable occupancy and work convergence

Status: **design for coordinator review**, not implementation or authorization to
cut over. Source baseline: `org-model.md` at `0c2fcd8`; root assumptions
register `d817f47`, including A23–A25. S4 #47 is in progress on a separate
branch: nothing here claims its capability is shipped. This plan proposes
contracts and rollout gates; schema names below are illustrative until the
migration is reviewed. The planning API remains the work surface.

## Decisions and boundaries

* O1: a member is durable and distinct from any session; O2: PoC leads a real
  team; O3 link-first is **transition only**; O4 deliberate hiring; O5 ordinary
  assistants and their checkouts are durable; O6 work is a board item,
  independent of session lifetime (`org-model.md` §15.1, Owner 2026-10-27 Go
  `48df8a8a-f073-44e9-83ed-fa5298c7fb0d`). Do not re-ask those rulings.
* Continuous members' spawns are workers with their existing disposable
  lifecycle, not durable assistants; Fresh continuous run ownership retains its
  scheduler/run-token rules. Backtest and cloud dispatch workers retain their
  existing lifecycle and are not made members or items. Shells are not agents.
* Earlier O3 text presenting deciding later or link-first as an end state is
  superseded by O6. Earlier O4/§7.6 text ending assistants or throwing away
  their context on retirement, and §9.3 shorthand treating all continuous
  spawns as assistants, are superseded to the extent they conflict with O5 and
  the continuous-worker exception. This plan does not edit historical text.
* S4 only stamps trusted attribution and composes rules, without granting
  authority. Its version-skew and delivery gates A23–A25 apply: typed old API
  models reject extra fields; deploy compatible readers/storage before new
  writers, negotiate support, never blindly retry an uncertain write; preserve
  workflow/Fresh lifecycle and unknown provenance; preserve the 64 KiB input
  guard and full retrievable rules via explicit digest/reference. S6 requires
  reviewed S4 coverage and rollback pin before it relies on its stamps. No
  manifest or holder wire bump is assumed. The cancelled STA sidebar #46 is
  distinct from the required Team sidebar.

## Baseline and proposed identity contracts

`sql/028_org_core.sql` already has `members.occupancy_generation`,
`incarnations` and partial unique indexes for one open main and open PID;
`dispatch/org_db.py:_create_member` creates initial main generation 1. S6
extends these, not a blank occupancy model. The index prevents two committed
main rows but cannot fence a previously authorized writer after replacement.
`sql/017_items.sql` has global internal `items.id` and board-local numbers;
`sql/029_org_s3_groups_placement.sql` links at most one live item to a task,
adds holder `member_id`, `via_member_id` and roles, and mirrors placement.
Neither internal ID nor those nullable columns is already a universal external
work identity or deployed member-holder policy. Current receipts and grants
(`sql/023_item_receipts.sql`) carry board/item and signer information, and
labels and request uniqueness use board scope. Changes must retain their
historical meaning.

### Occupancy and fencing

1. The central API authenticates the daemon and resolves the acting PID and
   its member/incarnation from trusted records, never from a caller-selected
   member field. Keep immutable actor PID, role, member attribution, and
   authorization decision separate. Assistants may act only under their own
   explicit rights/delegation; they never approve or rule as the member.
2. A proposed claim/replace/vacate transaction locks the member row, validates
   expected generation and current main incarnation, and atomically closes
   the prior main, increments generation and opens the new main. It returns
   the member, incarnation, generation and durable operation result. Initial
   creation continues to allocate generation 1. A unique open-main constraint
   is a second defense, not the fence itself. Compare generation **inside the
   same transaction** as each central privileged effect; a read-before-write
   check alone is insufficient. Missing, stale or mismatched authority fails
   closed with a stable error; never retry a rejected claim as a different
   member. Inventory all central operations requiring the fence before
   advertising `occupancy_v1`.
3. Same UID revive, resume or model/engine change preserves the incarnation
   and generation only if the current main still matches; no new claim is
   implied by a transcript generation. Replacement or Fresh obtains a new
   generation. Reviving an old main cannot reclaim that position; it may
   become an assistant under the applicable explicit lifecycle rules, without
   inheriting an assignment or permission. Claim races yield one winner; a
   losing candidate remains unbound/standby until identity is resolved, then
   may become assistant, but is not silently assigned work. Generation is
   monotonic and cannot be reset by rollback or a stale host.
4. Offline launch may create an unbound standby, but it cannot acquire a
   member grant, execute a disputed assignment, or write the member home
   before a successful online claim. It can report its own local failure and
   wait. A local manager-snapshot cache is not an occupancy authority. Central
   writes fence immediately; local pre-S7 controls retain their existing
   fail-static behavior, so revocations can have a stale-cache window. Document
   and measure this window; do not promise instantaneous distributed fencing.
   New relationships/claims are refused when authority is unavailable. The
   draft's five-minute refresh is a proposed timer, not an Owner ruling.
5. A request ID is bound to authenticated actor, operation and canonical
   request hash; replay returns the stored result, and reuse with different
   parameters is rejected. An unknown network outcome is resolved by querying
   the operation receipt, not by a fresh write. Generation checks and receipt
   persistence are atomic with the mutation. External side effects have their
   own durable reconciliation records, not a fictional distributed transaction.
6. Hire resolution returns source and resolved values: explicit authorized
   choice first; otherwise hirer's member as manager, Owner hire under Owner
   with no team unless chosen. Fill requires Owner or the authorized manager
   chain; validate cross-org and cycles transactionally. Preserve actual
   fresh operator, fork/source identity, restore, shell, A-s/A-F and
   continuous distinctions; B9's blanket “top-level auto-hire” must be checked
   against the S4 head rather than assumed from the older sketch.
7. For model resolution propose explicit launch choice > member default >
   task/work default > compatible work-rule model > host default, preserving
   the actual staffing-policy selection contract. Resolve engine and model as a
   compatible pair; report source and reject incompatible explicit choices
   instead of silently replacing them. Existing child/fork/continuous policies
   and explicit choices require fixtures before adopting this ordering.

## Work, launch and workspace contract

O6 requires an externally usable canonical work ID. Propose an opaque stable
work ID (not a board-local number or assumed public use of `items.id`), with
unique aliases for legacy task UUID and `(board identity, number)`. Migration
backfills a deterministic, one-to-one mapping for linked and unlinked rows,
records collisions/ambiguities, and refuses ambiguous legacy writes instead of
choosing a target. Repeated migration/startup is idempotent. Preserve original
board/number aliases after moves; never silently reuse a retired alias. Existing
internal IDs may remain physical keys. Keep old APIs and tables as adapters
until S8 separately authorizes retirement.

`start_session(item=...)` resolves canonical work or an unambiguous legacy
alias, authorizes the authenticated actor and assignment, and returns work ID,
launch ID, session UID, setup step results and operation receipt. A launch
request retry resolves to the same reserved operation and session, including
uncertain replies; hash mismatch is an error. Record work and assignment
independently of session lifetime. Launch failure does not mark the work done,
create a second session, or transfer an old session's private state. Legacy
`task_id` defaulting to caller task (`mcp_server/server.py:start_session`) must
be mapped explicitly; old task-scoped controls receive a compatible view until
cutover, not a silent scope widening. Existing taskless, workflow, backtest,
cloud and continuous launch paths retain their own controls.

Workspace creation proposes a central durable reservation keyed by member,
operation ID and request hash, with a stored workspace ID, absolute location,
branch and `creating → ready | failed/reconciling` state. Create branch/path
only after reservation; record each side effect and adopt an exact matching
previous result on retry. Detect partial branch/worktree/registration effects
and reconcile under ownership and cleanliness checks; do not delete a
preexisting, shared or dirty path. A failed response is not evidence of no
creation. Branch `cm-ws/<handle>-<ws7>` is stored and remains unchanged on
handle rename; preserve current `cm`/`cm-sub` parser and legacy worktrees.
`daemon/src/worktree.rs` and `spawn_setup.rs` have existing recovery/step
semantics; the new reservation must coordinate with them, not merely wrap a
best-effort spawn in a database transaction. No offline cache authorizes a
workspace claim or member-home write.

## Consumer inventory and activation unit

Every row below is required before member holds are activated; the list is a
review checklist, not a claim all code is already changed. Each implementation
stage must re-run a source search on its actual head, enumerate every producer,
reader, typed wire model, filter and background job, and record coverage.

| Consumer / current source | Required transition and checks |
|---|---|
| Holder storage/load and edit rules: `dispatch/items_db.py:_load_items`, `_apply`, `_write`, `update_items*`; `dispatch/items_rules.py:BoardTx`; `api/items_models.py`, `api/items.py`; `daemon/src/items/rpc.rs:authorize_edit`, `item_fields` | Preserve session PID, member, via, role, actual signer and historical events. Add compatible reads/dual projections; one canonical mutation under lock, no delete/reinsert loss of new fields. Validate main/assistant/delegation, actor identity, authorization and old PID clients; unknown is unknown, never inferred authority. |
| Status/flags and liveness: `api/board_engine.py:holder_status`, `compute_flags`, `strict_flags`, `tick_board`, `close_out`; `dispatch/items_db.py:heartbeat`, `read_board`, `held_items`; `daemon/src/items/heartbeat.rs`, `held.rs` | Resolve current acting session separately from durable owner; classify vacancy, offline, lost, idle and stale consistently, including historical/legacy PID rows. No duplicate flags or clearing real blockers on replacement. |
| Capacity and assignment: `_free_capacity` in `dispatch/items_db.py`, `strict_flags` and assignment rules | Count **acting sessions**, not members; assistants are distinct capacity and cannot acquire approval power. Enforce limits atomically under simultaneous assignment and replacement. |
| Pushes, nudges, escalation and chat touches: `api/board_engine.py:_push`, `_push_orchestrator`, `_escalate`; `dispatch/items_db.py:_push_owner_transitions`, `chat_touch`; `daemon/src/items/chat.rs` | Persist original target, delivery state, event identity and ack/deduplication. Handover inventories pending pushes; never silently retarget or treat delivery as acceptance. Explicit reissue is a new auditable event; stale old targets cannot gain authority. |
| Board steward, orchestrator, membership and epic PoC: `dispatch/items_db.py:_stewards`, `_orchestrator`; `dispatch/board_members.py`; `dispatch/epics.py:_authorize`, `set_poc`; `api/board_engine.py:_orchestrator_row`, `_escalation_target` | Separate durable named office from current acting PID and board membership/guest roles; preserve board scope, explicit grants and signer. Resolve vacancy without manufacturing a substitute; preserve add/remove/auto-end semantics and test replacement and assistant cases. |
| Launch grants and task controls: `dispatch/placement.py:_may_grant`, `_orchestrator_pid`, `_move_launch_grants`; `daemon/src/items/grants.rs`, `state.rs`, `control/auth.rs`, `control/methods.rs:resolve_authorized_session`, task scope and spawn paths; `daemon/src/claude_recovery.rs` | Preserve task/PID grants and daemon cache until compatible work-ID adapters exist; identify grantor, recipient, scope, epoch and revocation. Prevent old cached grants becoming replacement authority; keep taskless bootstrap and existing workflow/run token scoped. |
| Receipts, gate editors and waiver grantors: `dispatch/gate_db.py`, `dispatch/gate_rules.py`, `api/items.py` | Immutable original board, item/work ID, labels, signer PID, source message and grant parameters remain verifiable. Current authorization is distinct from historical signer proof: replacing a PID allowlist with member IDs must not retrospectively invalidate or manufacture signatures. Backfill/audit explicit authorization with unresolved cases gated; no alias creates authority. |
| Placement and old planning clients: `dispatch/placement.py:place_task`, `_unplace_locked`, `mirror_gate`, `apply_task_side`, `translate_task_status`, `task_description_changed`; planning/task APIs, `mcp_server/server.py`, TUI planning/board | One status writer under existing task-change advisory then board/task/team row lock order; reads/projections may mirror, never independently decide. Map old task edits, statuses and errors through the adapter; test old/new simultaneous writers and rollback, preserve text/metadata until their migration is explicitly complete. |
| Views and discovery: `tui/src/app/board.rs`, `orgchart*.rs`, planning/on-boards, APIs and MCP docs | Expose member owner, via/acting PID, vacancy and provenance without granting rights or leaking restricted evidence. Team→member→session sidebar must reflect org moves and refresh; preserve task and board clients during transition. |
| Messaging: `daemon/src/messaging/store.rs`, `rpc.rs`, `store/mentions.rs`; identity replication, DM/channel/follow/watch and cursors | Resolve `@handle` to current main only against authorized generation; ordinary PID DMs and transcript access remain private. Reapply only authorized memberships/follows; dedupe delivery, preserve offline unresolved state and do not route stale cached addresses to a replacement. Member mail remains out of scope. |
| Lifecycle and evidence: `daemon/src/assistants.rs`, continuous scheduler/report_done run-token, `spawn_setup.rs`, `worktree.rs`, teardown, audit/history and backtest/cloud paths | Preserve durable assistants and distinct disposable workers, Fresh exact-run completion, workspace cleanup guards and immutable original authorship. Test lifecycle independently of attribution or work completion; no automatic assistant closure. |

## Handover

Propose a durable operation keyed by member and old/new generation, with
`replacing → reconciling → ready` and explicit failure/retry state. Atomic
replacement fences central writes before new main may take member authority.
Snapshot enumerates holds and role/delegation, pending pushes/acks, reviews,
monitors, running jobs, workspace paths/dirty state and unresolved discovery;
record source, timestamp, disposition, and individual evidence. Unknown or
unreachable resources stay **unknown**, not clean or accepted. Incoming main
must explicitly accept or release each applicable hold; an identity change is
not delivery, assignment acceptance, completion, or a transfer of private DMs,
transcripts or credentials. Pushes already addressed remain bound to the old
recipient; explicit authorized reissue uses a new idempotent event. Monitor and
job transfer, if supported, requires the owning subsystem's explicit
capability and run identity; otherwise surface it for disposition. Track source
access only while authorized and fence after replacement. Crash/retry and
concurrent handover must preserve one result and monotonic generation. Ready
requires all required dispositions or a recorded, authorized exception, not an
empty inventory caused by an outage.

## O6 convergence prerequisites and safe moves

| §16.1 prerequisite | Stage contract and evidence before retirement |
|---|---|
| One canonical work ID everywhere | Backfill and API expose stable ID; all task, board, grant, receipt, launch, control, UI and event paths roundtrip it. Alias map handles collisions and former board numbers. Inventory and prove no consumer still requires a session or board-local identifier as identity. |
| One status authority | Keep item authority for linked work. Route old task writes through transaction/adapter, mirror only after the canonical decision, include unlinked/backlog rows in migration. Concurrent old/new client fixtures prove no split brain; reconcile mismatch before cutover. |
| Stable cross-board dependencies | Store dependency edges by stable work ID, backfill current global item references and translate board-local API forms. Preserve blockers on move, detect cycles across boards, authorize visibility without leaking restricted work. Existing per-board loaders and rules must change together. |
| Board-independent receipt/grant provenance | Preserve issuance board, original labels, immutable signer and message verification alongside canonical work ID. Versioned alias lookup accepts historical references without rewriting proof or conferring current permissions. Rehearse grants whose original grantor is no longer in current allowlist. |
| Move guards | Default refuse a move crossing live dependencies or board-scoped gate evidence. Return precise blockers and retain all rows atomically; no partial unplace/re-place. An approved migration/rebinding protocol for evidence is a separate decision; aliasing alone cannot clear this guard. |
| Old task-client adapter | Read/write compatibility for task UUID, placement, title/metadata, status, launch grants and errors; replay and rollback, old/new API/daemon combinations, no blind retries. Preserve typed strict readers (A23). |
| Rollback | Retain old tables, mappings, projections, task edges, compatible readers and bootstrap allowances through S7; preflight a reversible snapshot/replay and version-skew matrix. Fail closed rather than roll back to a writer that discards new authority or produces split status. S8 has a separately announced irreversible boundary. |

## Bounded stages and gates

1. **Inventory and compatibility contract.** At implementation head, complete
   consumer matrix including filters, wire schemas, event/history, API and
   daemon caches; capture S4 reviewed head and capability evidence. Specify
   actor and holder representation/version negotiation, operation IDs,
   schema migrations with markers/change feeds and lock order. Deliver an
   old/new/rollback compatibility matrix and trace each consumer to a test.
   No occupancy authority switches in this stage.
2. **Additive readers and work identity.** Add schema, opaque IDs and aliases,
   read-only projections/backfills with collision report; deploy all readers
   before writers. Shadow compare task/item status, identity and permissions,
   with no duplicate side effects. Rehearse repeated startup, rollback,
   ambiguous aliases and unknown outcomes. Block activation on unexplained
   mismatch or unsupported old strict reader.
3. **Fenced occupancy and workspace bootstrap.** Add claim/replacement,
   idempotency and workspace reconciliation under one bounded capability;
   exercise concurrent hosts, same-UID resume, Fresh, old-main revival,
   standby outage, stale generations, crash points, 64 KiB rule composition,
   explicit model precedence and unauthorized actor fixtures. Advertise
   `occupancy_v1` only after the stage 4 consumer implementation and rollback
   pin satisfy the contract; enable per host only when its readers and API
   support it. Stage 3 alone does not advertise or activate it.
4. **Whole holder-consumer activation.** Implement matrix together behind a
   central gate. Backfill unknowns without inventing members; test roles,
   capacity, flags, pushes/acks, stewards, grants, gate editors, historical
   signatures and rollback. Per-host canary cannot create member-only state
   that an old writer erases. S6 bootstrap allows only named paths needed to
   manage newly hired taskless sessions; it is not general S7 enforcement.
5. **Launch, handover and convergence adapters.** Introduce item launch and
   durable handover with fault-injection receipts; task adapter, stable edges,
   evidence provenance and move guards. Prove old/new write coherence and
   rollback with real-shaped sanitized migration snapshots. No data retirement
   or automatic migration of scoped gate authority. Backtests/cloud/continuous
   and O5 lifecycle regressions are explicit acceptance tests.
6. **S5 shadow and Team sidebar.** Compute both current and proposed
   manager-chain session-control **and board-edit** decisions, record decision,
   reason, policy version, host/caller class and coverage; shadow has no
   side-effects. Ship Team→member→session view independently from cancelled
   #46 and demonstrate org move, refresh, vacancy and permission behavior.
   S5 discovery/ranking invented numbers remain separate measured gates; do
   not introduce new scope enforcement early.
7. **S7 enforcement gate.** Owner directed about a week of clean/expected
   shadow differences, not merely a clock. Require per-host and caller-class
   coverage, outage/restart and clock-discontinuity accounting, every difference
   classified with owner and evidence, and explicit unresolved exceptions. Run
   allow/deny and rollback rehearsals on compatible hosts; retain S6 bootstrap,
   old edges and capability floor. Central switch is auditable and reversible;
   halt on unexplained mismatch, missing coverage, stale cache or unsupported
   client. Five-minute cache refresh is not approved by elapsed time.
8. **S8 separate irreversible decision.** After every §16.1 gate, coverage,
   backup and migration rehearsal passes, publish the planned retirement and
   rollback boundary to affected users/clients before action. Obtain the
   separate cutover decision, retain evidence and old adapters until then;
   retirement of task binding/`agent_task_edges` is irreversible and must not
   be represented as rollback-safe. This plan does not authorize it.

For every stage: record exact source/head and capability matrix, schema and
operation migration checks, compatibility fixtures and rollback rehearsal;
review failures and unresolved data before promoting. A capability is a
promise of end-to-end behavior, not just a schema bit. Pin the minimum
rollback version before any writer uses new semantics; a partial rollout fails
closed rather than silently dropping identity, work, grant or attribution.

## Assumptions register and independent critique

Root register A1–A25, B1–B20 and C1–C23 is the complete baseline of carried
and proposed defaults; this section adds only material decisions introduced by
this plan. Owner rulings above are not presented for reapproval. Existing
invented timers/limits/ranking values (A15, B4/B17, C9/C11/C13/C15/C16/C17/
C19/C21) remain gated; none is adopted by this plan. B13's branch name is a
carried proposal, not evidence it is already deployed. C4's link-first mirror
is transitional under O6. C10's O5 supersession is mandatory. B1 needs
transactional writer fencing in addition to a uniqueness index; B3/B4 cannot
promise immediate local revocation with fail-static cache. B5 losing standby
is not automatically assigned. B6/B7 must distinguish signer, member and
acting session, including capacity. B9 should be narrowed to the exact
fresh/operator paths verified on S4, not asserted as universal shipped
behavior. B14 must distinguish acceptance from identity change and retain
unknown resources and original push recipients. The S6 table says pushes are
retargeted; this plan proposes explicit authorized reissue instead of silently
retargeting an accepted pending delivery, and flags that policy for review. A20's observation interval is
necessary direction, not evidence of complete coverage; C20 Team sidebar is a
separate delivery gate from #46. A23–A25 constrain S6's compatibility and
lifecycle dependency as well as S4: do not rely on added typed fields before
negotiation, attribution to alter Fresh ownership, or silent rule truncation.

| ID | New assumption / proposal; provenance | If wrong / cheapest evidence | Disposition |
|---|---|---|---|
| N1 | Opaque external work ID plus persistent unique aliases; INVENTED implementation choice from O6 and §16.1 | Clients cannot locate moved work or collisions select wrong work; inspect all API ID forms and rehearse linked/unlinked/collision migration on sanitized snapshot. Recommend opaque ID with ambiguity refusal. | Gate before identity schema |
| N2 | Request ID plus actor and canonical payload hash with durable operation result; INVENTED API contract to meet idempotency | Duplicate launches/side effects or rejected legitimate retries; fault-inject lost replies and changed payload across old/new clients. Recommend scoped operation receipts. | Gate before launch/claim writers |
| N3 | Handover uses per-resource explicit dispositions, readiness exceptions and no silent push retarget; CONSEQUENCE of O5, B14 and delivery semantics, details proposed | Work or notices lost/stalled; sample real resource types, replay crash/outage with owners. Recommend explicit unknown state and reissue. | Gate before replacement activation |
| N4 | Refuse moves with live cross-board dependencies or scoped gate evidence by default; CONSEQUENCE §16.1, rebinding policy not ruled | Work may be temporarily immovable or historical evidence misinterpreted; inventory real move cases and demonstrate blocker explanation. Recommend refuse until independently approved migration. | Gate on move implementation |
| N5 | Model precedence explicit > member > work > compatible work rule > host; OWNER-RULED member-before-task, remaining ordering CARRIED/PROPOSED | Unexpected model/cost or incompatible engine; table-test actual launch paths and compare existing policy. Recommend preserve explicit choice and return resolution source. | Gate on resolution change |
| N6 | Current signer proof and present authorization are separately represented; CONSEQUENCE existing board-scoped grants plus member transition | Historical grant invalidated or fake authority minted; replay historical signed messages and allowlist changes. Recommend immutable issuance context and explicit unresolved backfill. | Gate before grant migration |
| N7 | Local fail-static cache retains revocation delay, unbounded during an outage; CARRIED policy, bound currently unmeasured | Revoked local control survives longer than expected; measure disconnect/reconnect and stale snapshots; recommend no claimed hard bound or invented refresh timer before evidence. | Gate before S7 |

Coordinator should route only these material implementation choices and any
new discrepancies after review; the listed Owner rulings and preserved behavior
are not reopened. This is a proposed design, not a mandate to fan out code.
