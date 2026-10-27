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
2. Each occupancy transition locks the member row and validates its expected
   generation and current incarnation. **Only replacement closes a main row**;
   vacancy and retirement do not. Replacement atomically closes the previous
   main with a reason, increments generation, opens its successor with
   predecessor, and records a durable result. Initial creation continues to
   allocate generation 1. The unique open-main constraint is a second defense,
   not the fence itself. Compare generation **inside the same transaction** as
   each central privileged effect; a read-before-write check is insufficient.
   Missing, stale or mismatched authority fails closed with a stable error;
   never retry a rejected claim as a different member. Inventory all central
   operations requiring fencing before advertising `occupancy_v1`.
3. Use the transition matrix below, with no new timer or automatic closure.
   Transcript generation is not occupancy generation. Claim races yield one
   winner; a losing candidate remains unbound/standby until identity is
   resolved, then may become assistant, but is not silently assigned work.
   Generation is monotonic and cannot be reset by rollback or a stale host.
4. Offline launch may create an unbound standby, but it cannot acquire a
   member grant, execute a disputed assignment, or write the member home
   before a successful online claim. It can report its own local failure and
   wait. A local manager-snapshot cache is not an occupancy authority. Central
   writes fence immediately; after S7, local controls use the specified
   fail-static snapshot, so revocations can have a stale-cache window. Before
   S7 the existing control rule remains authoritative except the named
   bootstrap rules. Measure the window; do not promise instantaneous
   distributed fencing.
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
   chain; validate cross-org and cycles transactionally. Preserve §4.2:
   Owner A-s adds a fresh agent as a new member under Owner; A-F follows the
   source member as an assistant unless Owner chooses hire/new member. A fresh
   top-level operator agent with no spawner uses the durable fresh-operator
   marker and becomes an Owner-managed member; restore/resume retains its
   existing identity and is not a hire. Workflow and continuous rows and
   shells are excluded from A-s auto-placement, and continuous workers retain
   their separate lifecycle. Exercise actual shipped marker and path behavior
   on the S4 head. Root B9's grouping of A-s with exclusions is erroneous;
   this correction preserves the rule and does not reopen it.
7. For model resolution propose explicit launch choice > member default >
   task/work default > compatible work-rule model > host default, preserving
   the actual staffing-policy selection contract. Resolve engine and model as a
   compatible pair; report source and reject incompatible explicit choices
   instead of silently replacing them. Existing child/fork/continuous policies
   and explicit choices require fixtures before adopting this ordering.

### Occupancy transition matrix

| Event | Main row and generation | Status, handover and authority |
|---|---|---|
| Initial hire | Insert initial main, generation 1, atomically with member/claim; no predecessor. | Authorized hire establishes current main; no pre-existing assignment is implied. |
| Same UID resume, revive or model/effort change | Preserve the same open row and generation; record a change event. | A revive of the still-current vacant main clears vacancy; transcript changes do not claim a new occupancy. |
| Replace / fill vacant member | Close old main with reason; increment generation and open new main with predecessor in one CAS transaction. | Fence the old generation centrally; start durable handover. The incoming current main can acknowledge or release handover entries as described below. |
| Fresh continuous fire | When Fresh replaces the prior main, use the same replacement CAS and new generation. | Preserve scheduler/run-token authority for the exact run's `report_done` and current disposable worker lifecycle; an old run token does not authorize arbitrary member writes. Persistent continuous behavior remains as shipped. |
| Vacancy | Keep the open main row and generation. | Mark vacant according to the separately gated policy; route/flag per §4.3. Same UID revival clears vacancy; no new timer is introduced here. |
| Retirement / reinstatement | Keep sessions, main row and generation; neither operation closes occupancy. | Retirement hands holds to manager, moves reports and removes team leads per §4.3; reinstatement restores pre-retire status and still-vacant team leads, without moving reports back. Apply existing authority checks; no automatic session kill. |
| Revive a main closed by replacement | The closed row stays closed and cannot regain main authority; current generation remains unchanged. | Revived old main becomes an assistant with a notice under §4.3; no automatic work assignment or inherited approval rights. |

## Work, launch and workspace contract

O6 requires an externally usable canonical work ID, not an assumption that a
board-local number or today's internal `items.id` is universal. Consumer
inventory favors an opaque stable identifier (avoids conflating UUID task IDs,
internal BIGSERIAL item IDs and board-local numbers); the representation is an
engineering choice gated by client/DB compatibility evidence, not a new Owner
policy question. Migration uses a **many historical aliases to one work**
equivalence relation: one task UUID and all item link rows with that task ID,
including archived links, represent the same work. A board-native item with no
task link is its own work. An archived task with no live link still resolves to
its work and archived status; do not invent a live item or a second work row.
Repeatable backfill and startup maintain this mapping and its provenance.

`place_task` archives a prior board link and may reactivate it when returning;
`items_task_link` is unique only while `link_archived_at IS NULL`. For A→B→A,
A's historical number, B's number and the task UUID resolve to the same work;
the reactivated A link is current and B remains a historical board occurrence.
Expose historical references with their original board/item/receipt/grant keys
and meaning; use current authority for ordinary work state and preserve old
board-scoped evidence as historical, never rewrite it as newly issued on A.
The adapter must explicitly distinguish historical occurrence reads from
current-work operations: authorized legacy writes to an unambiguous identity
route to the single current status authority, or return the existing closed,
archived or scope error rather than mutate historical status. Conflict only
when data is genuinely inconsistent (e.g. two live links, incompatible source
metadata with no authoritative revision, alias collision), not merely because
multiple archived links exist. Quarantine such cases for reconciliation and
refuse unsafe writes until resolved; never choose a source silently. Preserve
original board/number aliases and disallow ambiguous reuse. Keep old APIs and
tables as adapters until the separately reviewed S8 retirement.

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
| Pushes, nudges, escalation and chat touches: `api/board_engine.py:_push`, `_push_orchestrator`, `_escalate`; `dispatch/items_db.py:_push_owner_transitions`, `heartbeat`, `chat_touch`; `daemon/src/items/heartbeat.rs`, `chat.rs` | Preserve §7.4 pending-push continuity with the state-aware retarget/receipt contract below. Audit original and new target and dispatch generation; never infer delivery or processing from `delivered_at` alone. Reconcile API/daemon receipts, fence stale-host dispatch and dedupe; distinguish automatic safe retarget from uncertain explicit disposition. |
| Board steward, orchestrator, membership and epic PoC: `dispatch/items_db.py:_stewards`, `_orchestrator`; `dispatch/board_members.py`; `dispatch/epics.py:_authorize`, `set_poc`; `api/board_engine.py:_orchestrator_row`, `_escalation_target` | Separate durable named office from current acting PID and board membership/guest roles; preserve board scope, explicit grants and signer. Resolve vacancy without manufacturing a substitute; preserve add/remove/auto-end semantics and test replacement and assistant cases. |
| Launch grants and task controls: `dispatch/placement.py:_may_grant`, `_orchestrator_pid`, `_move_launch_grants`; `daemon/src/items/grants.rs`, `state.rs`, `control/auth.rs`, `control/methods.rs:resolve_authorized_session`, task scope and spawn paths; `daemon/src/claude_recovery.rs` | Preserve task/PID grants and daemon cache until compatible work-ID adapters exist; identify grantor, recipient, scope, epoch and revocation. Prevent old cached grants becoming replacement authority; keep taskless bootstrap and existing workflow/run token scoped. |
| Receipts, gate editors and waiver grantors: `dispatch/gate_db.py`, `dispatch/gate_rules.py`, `api/items.py` | Immutable original board, item/work ID, labels, signer PID, source message and grant parameters remain verifiable. Current authorization is distinct from historical signer proof: replacing a PID allowlist with member IDs must not retrospectively invalidate or manufacture signatures. Backfill/audit explicit authorization with unresolved cases gated; no alias creates authority. |
| Placement and old planning clients: `dispatch/placement.py:place_task`, `_unplace_locked`, `mirror_gate`, `apply_task_side`, `translate_task_status`, `task_description_changed`; planning/task APIs, `mcp_server/server.py`, TUI planning/board | One status writer under existing task-change advisory then board/task/team row lock order; reads/projections may mirror, never independently decide. Map old task edits, statuses and errors through the adapter; test old/new simultaneous writers and rollback, preserve text/metadata until their migration is explicitly complete. |
| Views and discovery: `tui/src/app/board.rs`, `orgchart*.rs`, planning/on-boards, APIs and MCP docs | Expose member owner, via/acting PID, vacancy and provenance without granting rights or leaking restricted evidence. Team→member→session sidebar must reflect org moves and refresh; preserve task and board clients during transition. |
| Messaging: `daemon/src/messaging/store.rs`, `rpc.rs`, `store/mentions.rs`; identity replication, DM/channel/follow/watch and cursors | Resolve `@handle` and member-addressed DM to current main when the API is reachable; during outage use the replicated last-known-main mapping (§4.4). That address conveys no occupancy or write authority. Ordinary PID DMs and transcript access remain private. Reapply only authorized memberships/follows; dedupe delivery and retain offline unresolved state without treating an address as an authorization claim. Member mail remains out of scope. |
| Lifecycle and evidence: `daemon/src/assistants.rs`, continuous scheduler/report_done run-token, `spawn_setup.rs`, `worktree.rs`, teardown, audit/history and backtest/cloud paths | Preserve durable assistants and distinct disposable workers, Fresh exact-run completion, workspace cleanup guards and immutable original authorship. Test lifecycle independently of attribution or work completion; no automatic assistant closure. |

## Handover and pending delivery

Propose a durable operation keyed by member and old/new generation, with
`replacing → reconciling → ready` and explicit failure/retry state. Atomic
replacement fences central writes before new main may take member authority.
Snapshot enumerates holds and role/delegation, pending pushes/acks, reviews,
monitors, running jobs, workspace paths/dirty state and unresolved discovery;
record source, timestamp, disposition, and individual evidence. Unknown or
unreachable resources stay **unknown**, not clean or accepted.

The new current main, authenticated and fenced at the replacement generation,
may read the checklist, acknowledge/accept or release its individual entries,
add notes and request reconciliation while the operation is `reconciling`.
These narrow actions must remain available before overall `ready`, so readiness
cannot deadlock its own acknowledgements. Each unresolved item remains marked
`handover` and routes to the manager until that item is accepted or released;
normal execution/ownership rights for that item do not arise from the mere
replacement. Record each disposition atomically and idempotently. Identity
change is not acknowledgement or a transfer of private DMs, transcripts or
credentials. Monitor and job transfer, if supported, requires the owning
subsystem's explicit capability and run identity; otherwise surface it for
individual disposition. Ready requires all required dispositions or a
recorded authorized exception, not an empty inventory caused by an outage.

§7.4/§13 require pending pushes to follow the replacement. Preserve that
continuity while distinguishing dispatch states. At this baseline,
`dispatch/items_db.py:heartbeat` increments `attempts` in the transaction
before returning a push; daemon `items/heartbeat.rs:deliver` accepts
`notifications::publish` enqueue and acks on a later beat. `delivered_at IS
NULL` therefore does **not** prove never offered or submitted; nonnull may
represent acknowledgement, enqueue or a dropped outcome, and does **not**
prove engine delivery or processing. A lost heartbeat response, daemon crash,
late ack or delayed stale-host execution makes the outcome uncertain.

* Atomically fence old-generation dispatch and retarget a push only when durable
  API/daemon state proves it was never offered or submitted. Preserve original
  target and event ID in audit, record new target and generation; delivery to
  the new main proceeds automatically under the replacement fence.
* Reconcile offered/submitted/accepted, dropped, observed and unknown states
  using durable API dispatch receipts and daemon notification receipts, with
  explicit correlation, idempotency and a stale-host dispatch fence. A host
  holding an earlier response must revalidate at the actual dispatch boundary;
  replacement/dispatch races and offline hosts require fail-closed or a durable
  coordination protocol, not an assumption that a database lock reaches a
  disconnected daemon. Do not blindly replay an already accepted/submitted or
  uncertain action. Preserve its recorded outcome and target; unknown remains
  unknown until evidence or an authorized explicit disposition resolves it.
* A new handover summary may describe prior uncertain delivery to the new main
  without replaying the original action. Automatic movement is for provably
  unsent pushes; explicit disposition covers unresolved delivery and any
  authorized new action, with a new auditable event and deduplication. Retain
  enough evidence to distinguish enqueue, engine submission, observation and
  processing where supported; do not claim more than a receipt proves.

Exercise crash points before/after offer, response loss, enqueue, ack and
replacement, delayed old-host dispatch, reconnect, duplicate retries and
missing receipts. This is an engineering reconciliation of the approved
continuity rule; escalate only a real behavior tradeoff demonstrated by the
receipt evidence, not a blanket new Owner policy decision.

## O6 convergence prerequisites and safe moves

| §16.1 prerequisite | Stage contract and evidence before retirement |
|---|---|
| One canonical work ID everywhere | Backfill and API expose stable ID; all task, board, grant, receipt, launch, control, UI and event paths roundtrip it. Alias map handles many historical links, A→B→A, no live link, board-native items, real collisions and former board numbers. Inventory and prove no consumer still requires a session or board-local identifier as identity. |
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
   with no duplicate side effects. Rehearse many historical links per task,
   A→B→A reactivation, archived tasks, board-native items and evidence keys.
   Rehearse repeated startup, rollback, ambiguous aliases and unknown outcomes.
   Block activation on unexplained mismatch or unsupported old strict reader.
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
   that an old writer erases. S6 bootstrap allows exactly
   `hirer_controls_hire` and `main_controls_own_assistants`, feature-gated and
   logged as §6.3 specifies; it is not general S7 enforcement.
5. **Launch, handover and convergence adapters.** Introduce item launch and
   durable handover with fault-injection receipts; task adapter, stable edges,
   evidence provenance and move guards. Prove old/new write coherence and
   rollback with real-shaped sanitized migration snapshots. No data retirement
   or automatic migration of scoped gate authority. Backtests/cloud/continuous
   and O5 lifecycle regressions are explicit acceptance tests.

**Parallel dependency lane — S5 profiles, discovery, shadow and Team sidebar.**
Build profiles and discovery under their own gates while S6 stages 1–5 proceed;
neither lane must wait for the other to finish before starting. Compute both
current and proposed manager-chain session-control **and board-edit** decisions,
record decision, reason, policy version, host/caller class and coverage;
shadow has no side effects. Ship Team→member→session view independently from
cancelled #46 and demonstrate org move, refresh, vacancy and permission
behavior. S5 discovery/ranking invented numbers remain separate measured
gates; do not introduce new scope enforcement early.

**S7 enforcement gate — after the applicable S5 and S6 dependencies.** Owner
directed about a week of clean/expected shadow differences, not merely a clock.
Require per-host and caller-class coverage, outage/restart and
clock-discontinuity accounting, every difference classified with owner and
evidence, and explicit unresolved exceptions. Run allow/deny and rollback
rehearsals on compatible hosts; retain S6 bootstrap, old edges and capability
floor. Central switch is auditable and reversible; halt on unexplained
mismatch, missing coverage, a cache that violates the documented fail-static
contract, or unsupported client. During API outage the last authorized manager
snapshot continues existing local relations; new relationships and central
changes remain refused, and the local revocation delay is unbounded until
refresh. A permitted stale snapshot or last-known-main address is not itself an
unexplained mismatch. Five-minute refresh is not approved by elapsed time.

**S8 separately reviewed and announced irreversible cutover.** After every
§16.1 gate, rollback readiness, coverage, backup and migration rehearsal pass,
publish the planned retirement and rollback boundary to affected users/clients
before action. Review and coordinate that cutover under the existing Owner
program Go; retain evidence and old adapters until then. Retirement of task
binding/`agent_task_edges` is irreversible and must not be represented as
rollback-safe. Seek further Owner direction only for an actual unresolved
policy choice, not as an invented mandatory new approval gate.

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
acting session, including capacity. B9 incorrectly groups A-s with exclusions:
§4.2 explicitly makes Owner A-s fresh agents new members, while A-F follows
source unless hired. Preserve
that ruling and verify the shipped marker and path details on S4. B14 must
distinguish acceptance from identity change and retain unknown resources;
pending pushes follow §7.4 via state-aware automatic retarget and receipt
reconciliation rather than a blanket manual-reissue rule. A20's observation
interval is necessary direction, not evidence of complete coverage; C20 Team sidebar is a
separate delivery gate from #46. A23–A25 constrain S6's compatibility and
lifecycle dependency as well as S4: do not rely on added typed fields before
negotiation, attribution to alter Fresh ownership, or silent rule truncation.

| ID | Assumption / proposal; provenance | If wrong / cheapest evidence | Classification |
|---|---|---|---|
| N1 | Opaque external work ID with many historical aliases; INVENTED representation implementing O6 and §16.1 | Moved work or history may resolve incorrectly; inventory consumers and rehearse A→B→A, archived/no-live, native, collision and evidence-key migration. Recommend opaque ID subject to engineering review. | Engineering evidence gate, not new Owner permission |
| N2 | Actor- and payload-bound durable operation receipts; INVENTED mechanics for idempotency | Lost replies duplicate launches/effects; fault-inject retries, hash mismatch and old/new clients. Recommend scoped receipts subject to engineering review. | Engineering evidence gate |
| N3 | Per-resource handover acknowledgement and state-aware pending push continuity; CONSEQUENCE of §7.4/§7.5 and delivery semantics | Lost, duplicated or stalled work; reconcile API/daemon receipts and stale-host races. Recommend automatic provably-unsent retarget and explicit unresolved dispositions. | Engineering evidence gate; route a policy tradeoff only if evidence exposes one |
| N4 | Refuse moves across live dependencies or scoped gate evidence; OWNER-RULED §16.1; rebinding mechanics unresolved | Work becomes immovable or history is misread; inventory cases and test blocker explanations. Recommend refusal until safe migration is designed. | Implement ruled guard; engineering evidence gate for rebinding, policy only if a genuine new tradeoff arises |
| N5 | Explicit > member > task > compatible work rule > host resolution; OWNER-RULED member-before-task, remaining order CARRIED/PROPOSED | Unexpected model/cost or engine incompatibility; table-test actual launch paths against shipped policy. Recommend preserve explicit choice and show source. | Engineering compatibility gate; escalate only a material behavior choice unsupported by existing ruling |
| N6 | Immutable signer proof separate from present authorization; CONSEQUENCE existing board-scoped grants and member transition | Historical grant invalidated or fake authority minted; replay signed messages and allowlist changes. Recommend immutable issuance context and explicit unresolved backfill. | Engineering evidence gate |
| N7 | Local fail-static cache retains revocation delay, unbounded during outage; CARRIED §6.3 | Revoked local control survives longer than expected; measure disconnect/reconnect and snapshots; preserve existing relations and refuse new ones. Recommend no invented hard bound or refresh timer. | Approved outage behavior; engineering coverage gate, not policy change |

Coordinator should route only genuinely unresolved policy choices after
engineering evidence; existing Owner rulings and preserved behavior are not
reopened. This plan proposes engineering contracts for review, not a mandate
to fan out code. The Owner's program Go already covers the staged direction;
S8 is separately reviewed and announced at its irreversible boundary.
