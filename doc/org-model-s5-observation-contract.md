# S5 observation integration contract

Status: **source-grounded proposal for review**, no hooks, schema activation,
collectors, live mutations, builds or session controls. Source snapshot:
`a02b0e79c5171e65c3baf74ebe1e2f0276eeb79e` (accepted #51 candidate on
`0c2fcd8`); accepted S5 plan root local `a1e2a53`, accepted S6 plan root local
`fdab6ba`. Line references below are to `a02b0e79`; S4 #47 is changing some
of these files. Re-inventory on the actual integration head. Root's local
integration is not evidence of main or production deployment.

## Contract and authoritative boundary

A **logical attempt** is one authenticated caller's operation on one target;
a batched operation has a parent request plus one child attempt per target.
Its actual authorization is the existing effective chain of checks, including
operator gates, daemon scope and any API-specific gates. Record the actual
final authorization separately from the execution outcome. `allow` plus a
failed PTY write, failed transaction, retry or an accepted asynchronous prompt
is not successful delivery; accepted is not processed. Validation/target
missing, API unavailability, cancellation or early lifecycle conflict are
`not_evaluated` / `unknown`, not invented allow/deny. No raw error text,
prompt, transcript, token, profile or private message is recorded.

Candidate `s5-candidate-v2` (`dispatch/shadow_policy.py`) is advisory only and
compares with the **supplied final actual result**; it does not calculate the
old policy. Endpoint readiness is a separately observed fact. A partial chain,
stale identity or missing component yields `unknown`; missing observation is
not agreement. If a handler checked a permitted scope and then refused a
lifecycle/target precondition, preserve both facts: the scope subdecision can
be known, while the effective endpoint result is `not_evaluated`/`unknown`
unless its actual outcome is explicitly classified. Never turn an HTTP 409,
422, timeout, post-authorization execution failure, or crash into an
authorization deny. Emit stable reason codes, not free-text errors.

## Source inventory: session control and adjacent scope

The coverage registry is an explicit manifest keyed by source head, method,
caller/target class and stage. The following list is the baseline search
inventory, not a promise that a single hook sees them all. Line anchors are
entry points and checks; implementers must inspect all branches at the frozen
integration head and include every registered path or mark it uncovered.

| Path / source anchor | Actual gate and observation classification |
|---|---|
| `daemon/src/control/dispatch.rs:95`, `:401`, `:1874`, `control/board_token.rs:17`, `:53`, `protocol.rs:14`, `:47`, `operator.rs:95` | Request/Caller, delegated board-token allowlist and operator-token validation precede operations; the delegated token allows only its listed messaging/attention methods, not general board edits, and must remain a separate ingress class; `Caller::Session` and `Caller::Operator` are distinct. Never store `token_id` or credential. Invalid/forged operator is an ingress denial, not an org candidate. Control RPC request `id` is caller-supplied and cannot alone identify a unique attempt. |
| `daemon/src/control/auth.rs:148`, `:196`, `:245`, `:279` | `task_in_scope`, live and exited checks: caller/target registry, self, global, tasked same/descendant task, taskless same workspace; unsynced task tree may be retryable. These are components, not final operations. Global is current local behavior; candidate same-org restriction is separate. No extra traversal under the mutex. |
| `control/methods.rs:1990` / `:2047` (`send_input`), `:2253` / `:2280` (`kill_session`) | Scope and target lookup under same state lock; retain this atomicity. Pre-scope recovery/retirement gates, dead PTY, queued asynchronous prompt, identity revalidation, signal/write failure are distinct from the scope result. Operator bypass is a named component. |
| `control/send_key.rs:51`, `:99`; dispatch routes `control/dispatch.rs:3529`, `:3542`, `:3607` | `send_key` checks each target and reports per-target outcome; one parent and per-target children, no double count of `send_one` plus the RPC. Preserve exit/not-found/unauthorized vs write result. `send_input`/kill have separate handler dispatch. |
| `control/dispatch.rs:2130`, `:2202`, `:2273` (`rotate`, `authorize_target`, `handoff`) | Operator, self, task scope or **existing** API member-manager authority fallback; target shell/continuous/workflow refusal is an independent endpoint precondition. `now`/`force` operator-only and status reads differ. Do not pretend the current rule is only the old task tree or apply candidate control to status reads. |
| Operator lifecycle operations: `control/dispatch.rs:2312`, `:2337`, `:2384` and following recovery handlers; `control/methods.rs:2203`, `:3941`, `:4736` | Handoff cancellation/bulk, revive and recovery confirmations have distinct operator gates; session resize, global-permission changes and daemon drain have their own operator-only boundaries. Enumerate actual route and per-target result rather than borrowing generic session scope. Do not log credential material or reinterpret operator bypass as a session-main fact. |
| `control/methods.rs:18573`, `:18633`, `:18684`, `:18729` (`session_fork`) | Source live/exited scope and target task-scope are distinct components; both must pass, plus task/workspace/validation preconditions. An accepted fork/launch outcome is separate from permission. |
| `control/methods.rs:9762`–`:9912`, `:17807`, `:17915` | Spawn and subtask creation use their own task/launch/setup checks; `spawn_setup.rs:40`, `:57` checks creator/operator/global before step-specific board/team calls. Capture each explicitly applicable scope decision, not a synthetic target-session decision before a target exists; record spawned child/step outcomes separately. |
| `control/methods.rs:2530` / `:2553`, `:2658`–`:3020`, `:3135`–`:3218`; `items/org.rs:303` | `read_session_output`, list and `resolve_authorized_session` are **read** scope, with tombstones and existing member-report read widening and revalidation. Inventory and tag separately; do not fold into the control denominator or grant a write because a read passed. |
| `control/methods.rs:3324`, `:3414` / `:3430`, `:3650`, `:3812` | Agent-report self-only, turn-ended shared scope, transcript rebind and workflow context have method-specific caller/lifecycle gates. Identify which are actual user-initiated control versus self-report/internal before including; separate categories and preconditions, not an assumed blanket permission. |
| `control/methods.rs:4950` / `:5039`, `:5813`, `:5912` / `:5957`, `:6019`, `:6101`–`:6168`, `:6669`, `:7281`, `:7550`; `control/dispatch.rs:1874` | Workflow start/read/list/stop have task or run/participant-specific checks; transition/done/reject validate caller and forged operators. Workflow role is not an ordinary durable assistant by assumption. Record control-like requests separately and mark candidate `unsupported` until exact target semantics exist; do not broaden workflow rights. |
| `control/methods.rs:13256` / `:13283`, `:13817`, `:13940`, `:14251`; `continuous/*` | Trigger/enqueue/report/resolve use scheduler and run-token/participant rules. Tag continuous member, worker, fire/run token identity (opaque fingerprint, never secret) and origin; no claimed member-main equivalence or teardown change. Calls without a meaningful target-session rule are `not_applicable`, not agreement. |
| `control/methods.rs:19348` / `:19387`, `:19482` / `:19510`, `:19547` / `:19637`, `:19689` / `:19728`, `:21184` / `:21209`; `mcp_server/server.py:1853` | Subtask list/status/update/reparent, `task.scope_check` and done have task-tree checks; task IDs are not member/session IDs. Record as task-scope components/coverage, not fabricated session candidates. Multi-ID scope checks have one child result per ID; MCP reparent precheck is not the final API/daemon mutation. |
| `daemon/src/sidebar.rs:97`, `:140`, `:193`, `:281`; `attach.rs:228`, `:366`; `control/methods.rs:2203` | Sidebar assignments use caller/task scope and their own rules; attach uses caller-bound ticket/stream checks, resize is operator-only. Explicitly separate or mark out-of-scope for initial policy candidates; absence must be visible in coverage, not silently counted as compared. |

Actor classes to cross every applicable site: validated Owner/operator,
global/non-global session, self/other, taskless/tasked, main, durable
assistant, unbound/unknown/stale, shell, continuous worker, workflow
participant, missing/exited target, remote/unreachable host, same/cross-org.
The same session can change class over time. `managed_by_uid`, task parent,
workspace and display attachment do not establish member or management rights.
Role/class unknown is a valid explicit state, not a deny. In this baseline
`authorize_target` already has a narrow manager-chain fallback; S5 must
observe that actual behavior as shipped rather than erase it.

## Source inventory: board and API

`mcp_server/server.py:335`, `:388`, `:477`, `:512`, `:581`, `:593`, `:647`,
`:696`, `:753`, `:790` are client surfaces, not independent authoritative
checks. Trace RPC method and operation, including inner setup calls, through
the daemon and central implementation; do not infer identity from a client
argument. `daemon/src/items/rpc.rs:30`, `:113` snapshots `Ctx`; `:205` builds
the baseline actor from actual caller. `:350` resolves board edit using
operator/global, implicit task board, open PID holder, or explicit board join.
The `Ctx` mutex is released before planning API I/O. Correlate immutable
snapshot/revalidation state; do not hold it across I/O.

| Operation and daemon site | API and central decision sites; capture scope |
|---|---|
| `item.create`, `item.set`, `item.resolve`: `rpc.rs:472`, `:504`, `:570` (each calls `:350`) | `api/items.py:95`, `:102`, `:129` → `items_db.py:471`, `:505`, `:536`, `:655`, `:388`; `items_rules.py:633`, `:672`, `:852`, `:1118` add status/Owner/holder/reopen constraints. Bulk `each` yields per-item outcomes; count parent and children separately, with operation denominator defined per child. API call may fail before or after scope; record exact phase. |
| `board.settings`: `rpc.rs:972` | `api/items.py:83` → `items_db.py:667`, `:721`, `:729`, `:734`: generic board edit plus current orchestrator for governed settings, Owner-only gate-editor/grantor allowlists, and gate-editor/orchestrator for default class. `BoardOperation.SETTINGS` must carry exact `SettingsKind` for **each** field; mixed/unclassified patch is unknown until all relevant restrictions are evaluated. A generic allow is not effective authorization. |
| `item.place`/`item.unplace`: `rpc.rs:1001`, `:1016`, `:1045` | `require_task_scope` is an additional task gate, then board edit, then `api/items.py:225`, `:233` → `placement.py:285`, `:381`, `:438` transactional placement/move guards. A taskless/global/Owner result cannot be inferred from board scope alone. |
| `group.create`/`group.set`: `rpc.rs:1065`, `:1078` | `api/items.py:239`, `:247` → `placement.py:562`, `:587`, `:620`; shape/promote/demote epic is Owner/orchestrator-only in baseline while ordinary heading edits follow different checks. Record exact subtype, not generic board-edit success. |
| `board.join`, leave/remove: `rpc.rs:1103`, `:1121` | `api/items.py:256`, `:265` → `board_members.py:74`, `:93`; leave self differs from removal, which checks self/Owner/orchestrator/steward/adder with current edit. A helper `.is_ok()` used for `editor` is a component, not a second final request. Auto-end on heartbeat (`board_members.py:132`) is system work, separate coverage. |
| `epic.set_poc`, `staff`, close-plan/apply: `rpc.rs:1135` | `api/items.py:281`, `:288`, `:295`, `:301` → `epics.py:59`, `:124`, `:219`, `:253`, `:371`. Set PoC allows Owner, current board orchestrator, steward, and exact current epic PoC/main manager chain (#28); close uses Owner/orchestrator with its own plan/revision checks. Staffing primary calls org move; guest is a different path. Do not treat `authorize_edit` as if this dispatcher calls it. |
| Gate report/waive/profile: `items/gate.rs:92`, `:109`, `:476`, `:511` | `api/items.py:145`, `:159`, `:166`, `:181` → `gate_db.py:274`, `:499`, `:525`, `:562`, `:623`, `:650`; report explicitly uses daemon board edit; grant/profile have their own grantor/message/content/editor checks and may bypass that generic check. Rechecking a message while reading receipts (`gate.rs:431`) can PATCH a grant: tag this system/read-derived effect and don't treat it as a user-issued waiver. Approver contact is not a grant. |
| Chat touch and internal effects | `api/items.py:114`, `:121` → `items_db.py:578`, `:589`, `:599` and `items_rules.py:1050`: per-item holder/steward and ambiguity rules; `placement.py:186` launch-grant transfer; `epics.py:219` staffing; `gate_db.py` refresh. System/daemon-origin operations have no session actor unless proven and must not be assigned one. |
| Legacy task writes: `control/methods.rs:19482`, `:19547`; `api/main.py:935`, `:945` | Placed-task status and description PATCH can enter `placement.py:457`, `:549` through the legacy adapter; task scope, board/status mirror and side effects are separate components. A direct legacy API write may have no verified daemon correlation; do not pretend it traversed `items/rpc.rs:350`. |
| Direct API / TUI paths | `api/items.py:40` router uses `api/auth.py:7` shared bearer. It trusts supplied actor fields by design; possession of that bearer or a claimed daemon ID does **not** prove a particular actor/host. Direct API, TUI/operator, retry and internal calls must have an explicit origin class. If no authenticated daemon-side correlation exists, store `unverified_origin`, observe central result separately and leave end-to-end candidate/coverage unknown. Do not join on actor/body/time alone. `api/items.py:90` delete has no actor in this API: record as a separate bearer-auth operation, not an attributed member decision. |

A `gate_profile` read, a board read or an internal background refresh is not a
user board-edit attempt merely because it shares dispatch code. Conversely,
read paths that mutate (receipt recheck) are explicitly marked. Separate
per-item/per-field components and effective operation: a parent patch that
fails any required check is not allowed merely because another component
allowed. Code inventories at integration must also include direct API clients,
MCP/TUI paths, malformed input and early returns; an unhooked branch remains
uncovered until evidence proves coverage. API `Actor`/`Holder` strict models
at `api/items_models.py:17`–`:42` reject unknown fields at this baseline.

## Identity, snapshots and temporal provenance

A bounded **attempt envelope** contains only typed scalar IDs and reason
codes. Every field is `known(value, source, version)` or `unknown(reason)`;
absence is not the Owner, false, fresh, or same org. Record:

* origin class (`daemon_session`, `validated_operator`, `direct_api`,
  `system`, `unknown`), daemon ID, server/host ID, boot lineage + brain epoch
  (`capabilities.rs:75`–`:107`), locally allocated attempt sequence and
  per-target child ordinal; method/site enum and policy version. Existing RPC
  `Request.id` may be retained as a bounded diagnostic reference, not the
  dedupe key or proof of identity. Never retain operator token identifiers.
* caller participant PID and UID when authenticated, engine/kind, own/global
  flag as verified at decision time, member ID/role **only** from S4's trusted
  provenance, and provenance source/version. Target host/UID/PID, liveness,
  holder-incarnation/PID-start binding where available; task ID and task-tree
  snapshot pushed/not-pushed/version or stable content digest, workspace ID
  and binding epoch. An absent task ID is an explicit taskless fact only if
  the registry lookup was complete. Transcript `generation`, holder
  incarnation, occupancy generation, binding epoch and brain epoch are
  different domains; never compare or substitute them.
* relevant org ID, actor/target membership and acting role, membership/manager
  snapshot version and freshness, board ID, item ID/board-number alias,
  exact epic/team and PoC chain, role/delegation, field category and gate
  grant/editor fact source. Record enum knowledge for `YES`/`NO`/`UNKNOWN`
  and whether chain is complete, acyclic and stable; missing unbound/old
  writers, absent freshness proof, invalid/cyclic graph or unverified cross-host
  origin produces unknown. `no member` is not a trusted negative unless
  explicitly established by the authoritative source.
* Capture **at** each existing decision under its existing synchronization:
  the values already used in that lock/transaction and identity stamps, with
  a compact fingerprint for later verification. If data is resolved after
  lock release (API or org fallback), attach its source/version as a separate
  component; do not reconstruct past facts from today's member, the TUI's
  attachment heuristic, co-tenancy or PID alone. A changed actor/target
  generation between capture and effective check yields a race/unknown. Old
  API/daemon or absent S4 fields are recorded as legacy/unknown, never
  retroactively stamped with a new member. Capture status must not alter
  existing TOCTOU protections.

## Nonblocking capture, linkage and Python placement

The Rust control thread constructs a pre-sized minimal envelope from facts it
already owns and a unique attempt ID, then does a bounded nonblocking enqueue
(`try_send`-style) outside its critical section where possible. No disk, socket,
HTTP, Python interpreter, subprocess, new lock cycle, full graph traversal or
waiting for observer acknowledgment in an authorization critical section.
The authorized method returns exactly its existing result whether the queue,
serializer, disk or central API is down. The candidate evaluator never sends
an operation or participates in its admission. Event serialization and I/O
run in an independently supervised background worker, disabled until the
contract and compatibility gates are reviewed. The bounded queue and failure
accounting below are prerequisites, not permission to silently drop.

`dispatch/shadow_policy.py` is Python and cannot run in Rust in-process as
written. Recommend **asynchronous central Python evaluation** of immutable
captured facts after durable receipt: the API-side Python worker imports the
accepted evaluator, and writes candidate result/version linked to the event;
board observations can use the same evaluator asynchronously. No Python/HTTP
call in Rust auth, and no re-query of current state to fabricate missing
historical facts. If the facts were not captured or are mismatched, evaluate as
unknown. A future Rust native evaluator is an alternative only with shared
contract fixtures, versioned parity and its own review; no unreviewed duplicate
policy. Python job failure/lag leaves `candidate_pending` or `unknown`, not
agreement. Replay is deterministic for the stored facts and policy version;
retain the evaluator version or its content digest for audit.

Each capture producer allocates an unguessable boot/producer UUID once, plus
monotonic per-producer sequence; the stable key is `(daemon_id, boot_uuid,
seq, child_ordinal, component_kind)`. It is distinct from caller-controlled
RPC ID, local process PID, restart counter and transcript generation. A
parent/child relation links batch targets and board-field checks. Central
retries use the same event ID and payload digest; identical replays are
idempotent, same ID/different payload is a conflict/quarantine, never a second
observation. API-generated direct attempts use their own trusted process
lineage and ID and cannot claim to be daemon-origin solely from a header.
An optional correlation header must be negotiated outside strict request
bodies and authenticated/bound by a future compatible transport; until then it
is **untrusted hint only**, so API and daemon records stay unjoined/unknown.
Never attach labels or match by time/PID/body as proof. This is a present
coverage limitation: old/direct calls cannot yield an authenticated joined
end-to-end result from existing wires. The safe initial choice is separately
recorded components with `unverified_origin` / `unknown`; a later versioned,
authenticated correlation transport can improve coverage without changing
authorization. Neither a caller-supplied ID nor a shared bearer solves it.

The authoritative path remains fully functional with old daemons, direct API
clients and rollbacks. Parent spans have per-component states
`pending | allow | deny | error | not_evaluated | unknown`; a final joins only
when all required components are observed with verified correlation. A daemon
may finish a response after an API write whose outcome is unknown; do not
reissue or invent final status. Define denominators as `(logical attempt,
target)` exactly once; raw component events and delivery retries have separate
counts. Atomic/mixed batch partial results remain individually visible. A
startup manifest lists all expected sites and coverage; uninstrumented sites,
unknown origins and pending final join are counted separately, not successes.

## Durability, access, feed and bounded operation

Proposal, subject to implementation review and capacity verification:

1. **Producer epochs and loss:** before enabling observation on a boot, an
   independent startup worker durably records a producer-open marker and its
   source/host/boot ID. If that fails, observe-disabled/unknown is advertised;
   authorization proceeds. Allocate the sequence even for failed queue
   admission. Per-site monotonic attempted/admitted/overflow/oversize counts
   and a reserved loss marker are preallocated, updated without blocking the
   decision. The worker periodically durably checkpoints counters and
   sequence ranges, with a clean-close marker only after the queue and durable
   acknowledgments drain. A crash before a checkpoint cannot promise an exact
   lost count: the entire interval since last durable checkpoint through an
   unclean boot is **coverage unknown**, even if no gap record was saved. A
   reboot without any persisted open marker is also unknown. Never silently
   reset a loss counter or infer a complete zero.
2. **Local spool:** bounded framed records (length, checksum, payload) and
   append-only segment/index, fsync/group-commit off the auth path. Recover by
   verifying frames, truncate only a damaged tail while recording a gap, and
   quarantine middle corruption; no automatic overwrite of unacknowledged or
   review-held evidence. Persist durable acknowledged range and replay the
   same IDs after crash/timeout. A failed write, full filesystem or lost
   checkpoint yields explicit loss/unknown; the caller's operation is
   unaffected. Control files must be reserved and bounded too; if even a gap
   marker cannot persist, the last known durable interval/host is unknown.
3. **Central schema (proposed, not created):** `shadow_attempts` stores the
   ID, bounded facts/provenance and actual/endpoint phase, payload digest,
   created/received time and retention hold; unique producer/sequence/child/
   component key. `shadow_results` stores evaluator version/digest, decision,
   reason, completeness and evaluation timestamp under unique `(event_id,
   evaluator_version)`; `shadow_intervals` stores producer open/close,
   checkpoints, gap ranges, counts/overflow and review classification; a
   restricted `shadow_changes` feed carries event/interval IDs, epoch, cursor
   and `pruned_through`, with `expired` forcing resnapshot. Retain original
   event and append versioned corrections; never overwrite a prior result.
   Insert event and feed row in one transaction, same-key/different-digest
   rejected; acknowledge only after commit. A central crash between commit and
   ack causes safe replay. Define commit ordering explicitly, e.g. the
   advisory-locked task-feed pattern `sql/016_task_changes.sql:1`–`:55`,
   without importing its lock into auth or board write critical sections;
   notify is a hint, not the source of truth. Rollback preserves unread spool,
   receipt and rows, and old writers neither read nor erase them.
4. **Access:** no payload, prompt, raw paths, tokens, DM content or profile
   text. IDs and permission patterns are sensitive: restrictive local
   file/directory modes, encrypted transport under existing authenticated
   service boundary, separate restricted operator/coordinator review endpoint;
   not ordinary `org.snapshot`, public board history or caller-visible errors.
   Central read/retention authorization is explicit and audited; the baseline
   shared API bearer is insufficient as a per-user authorization mechanism.
   A future restricted reader needs a separately verified operator/reviewer
   identity and must not expose records merely because a client can call the
   existing general API. No new access is granted by possession of an ID. Untrusted clients may
   not assert authenticated origin. Rate/error logs contain only bounded
   codes and aggregate counts. Key rotation/backup is part of sink review.
5. **Clocks/epochs:** persist boot ID, process lineage/brain epoch,
   monotonic timestamp and wall timestamp at capture plus central receipt;
   record restart, missing/changed boot markers and wall-clock discontinuity.
   Monotonic order is only valid within its boot. No assumption that sequence
   number equals cross-host time or that wall-clock duration implies full
   coverage. Retention/review intervals crossing an unknown epoch are flagged.
6. **Feed and counters:** restrict and version feed/query readers; preserve
   cursor epoch and pruning boundary, dedupe and explicit expired-cursor
   resnapshot. Counters distinguish attempts, actual allow/deny/unknown,
   candidate allow/deny/unknown, disagree, not-applicable, missing components,
   queue overflow, spool loss, pending evaluation, writer errors and host/site/
   class coverage. Reconciliation uses counters plus producer intervals, not
   only observed event count. Missing, overflow and unknown intervals block a
   clean S7 coverage claim; do not sample or silently classify as agreement.

### Concrete bounds and evidence behind them

At read-only inspection, `/home/lucas` reports 1,081,101,176,832 bytes total,
415,756,951,552 bytes available; `/` reports 10,432,540,672 bytes available
(`df -B1`, 2026-10-28). The retained #51 receipt/log directory is 35,788 bytes;
its compiler log is about 33 KiB and pytest logs 69/132 bytes. These are
**test-log sizes, not observation volume or event-rate measurements**. The
central database's actual volume, free space, WAL/index overhead, number of
hosts, decision rates, burst distribution and compressed record size are
unknown. Filesystem free space on this host does not establish PostgreSQL
capacity or permission to consume it.

Recommend conservative **initial engineering ceilings**, not measured usage or
Owner policy: at most **4,096 bytes per encoded event** including bounded typed
provenance (no free text); reject/mark oversize as coverage loss, never truncate
an ID into another identity. Frame overhead budget 16 bytes. Fixed memory ring
**256 slots / at most 1 MiB of payload** plus preallocated counters and fixed
metadata; a full ring drops no authoritative request and marks coverage loss.
Per producer on disk: **64 MiB total**, partitioned into **56 MiB event
segments + 8 MiB reserved** for checkpoints, loss/epoch markers and index;
apply bounds to temporary files and filesystem allocation as well. Central:
**1 GiB global physical quota** for the observation table, indexes and feed,
plus explicitly measured WAL/backup headroom, with **128 MiB per producer**
and a reserved bounded control/interval area inside those caps. Quotas use
actual allocated bytes, not just JSON length. Producer IDs cannot be rotated
to evade per-host and global limits. No storage activation until the actual
central mount and safe free-space reserve are verified. These are design
ceilings for a later canary; sizing may be changed after the probe, with reason.

At the maximum 4,096-byte payload + 16-byte frame, the 56 MiB segment budget
holds at most **14,280 records**, before any filesystem overhead; that is only
**85 records/hour** if evenly spread over 168 hours. This is a conservative
capacity calculation, **not an observed rate** and not a promise that a week
fits. Component events and batch children consume separate records; even one
busy host can exceed it. Reserve/retention counts cannot claim coverage if
limits are reached. A 1 MiB ring holds at most 256 full-sized records; a burst
or blocked worker can fill it sooner than spool capacity.

Retention rule: keep every event, interval, gap and pending/classified S7
review evidence through the Owner-directed roughly one-week observation and
until its review is explicitly complete; no implicit deletion at seven days.
A preexisting review/legal hold wins. Only after review and acknowledged
consumer cursors may an operator-approved retention policy prune; publish the
pruned boundary and force old cursors to resnapshot. If quota/headroom is
exhausted before then, stop accepting observer records, retain existing data,
mark the interval and affected host **coverage incomplete**, expose loss and
request capacity/review; do not evict held data, block user work or call S7
clean. Epoch-wide unknown after unclean crash can outlive the quota interval.
No invented automatic timer or discard policy is authorized by this document.

Smallest later measurement: one reserved, finite, isolated **fixture-based**
probe, not a scheduled/live collector: serialize synthetic worst-case bounded
records and replay representative existing sanitized authorization fixtures
under a private labelled target; measure encoded bytes, enqueue throughput,
quota/overflow and fsync cost with the future component implementation. Then,
a separately coordinated opt-in bounded counter-only canary on each relevant
host can measure actual eligible decisions/bursts and DB physical/WAL growth
without storing content. Its duration, resource reservation and reviewed
privacy/retention protocol must be explicit; until then rates remain unknown.
No probe, canary or collector is authorized or run here.

## S4 and rollout prerequisites

* Freeze the actually reviewed S4 head and wire contract. `0c2fcd8` has strict
  `Actor`/`Holder`; the in-progress S4 lane has an uncommitted repair on
  `4f53b8d` and is **not** a shipped or stable interface. The observation
  integration must use the final reviewed identity/role stamps and explicit
  old/new wire capability negotiation, reader-before-writer ordering, actual
  rollback pin and `org_stamp_v1` completeness gate. Do not assume S4 owns
  occupancy fencing, changes control permissions or has complete provenance.
* Coordinate only the captured fact adapter/interface with org after its
  active repairs: exact actor/holder semantics for main, assistant, worker,
  Owner, unbound/legacy, snapshot freshness/version, and how old API/daemon
  refusal/uncertain commit is surfaced. No actor-provided member field, extra
  strict wire field, capability claim or generated correlation proof until
  both sides support it. Separate observation correlation from S4 attribution;
  a capability or shared token alone does not authenticate an event origin.
* Add restricted readers/storage/schema first, then offline capture behind a
  default-off feature and compatible per-host support; test old daemon/new API,
  new daemon/old strict API, old rollback and absent S4 against explicit
  unknown. Enable only after quotas, gaps and monitoring work. Roll back to
  previous authoritative behavior with producer disabled and bootstrap rules
  retained; preserve spool and persisted review evidence. Neither rollout nor
  rollback changes authorization, work assignments, prompts or lifecycle.
* S7 readiness requires source/site/host/caller/unknown coverage, no
  unexplained gaps, reviewed expected differences, outage behavior and
  rollback rehearsal as well as the Owner-directed observation interval.
  A Python worker crash, backend timeout, queue overflow or clock jump must
  reduce coverage, not permit S7 on a clean-looking week.

## First implementation slice and acceptance

**One minimal slice:** build a default-off, in-memory capture envelope and
bounded queue/producer interval library with **no hooks in production paths**,
no database, network or collector. Put it in a new
`daemon/src/shadow_observation.rs` with minimal module declaration in
`daemon/src/lib.rs`; add isolated Rust unit/fixture tests in that module or
`daemon/tests/shadow_observation.rs`. Reuse `dispatch/shadow_policy.py` only as
an offline fixture oracle: a small checked-in, content-free shared JSON corpus
under `tests/fixtures/s5-shadow/` and Python fixture tests can verify immutable
facts/unknown/version parity without shipping duplicate authorization logic.
The Rust component stores typed snapshots and emits no candidate decision,
starts no worker unless explicitly driven by tests, and is not yet complete
coverage. For this slice use an injectable in-memory checkpoint model to
simulate durable open/close and crash gaps; it does not claim real disk
durability before the later spool implementation. Source inventory and schema contract remain documentation; actual
hooking, restricted durable spool, transport/API/schema/feed and live
activation are separate future reviewed slices. Module/file names are proposed
boundaries to avoid S4's `items/rpc.rs`, `items/org.rs`, `control/methods.rs`,
`api/items_models.py`, `api/org.py`, `dispatch/org_db.py` and S4 migrations;
rebase/coordinate before touching those later.

Acceptance: frozen exact-head tests validate per-boot IDs and per-target batch
keys, immutable captured fields, endpoint/actual/outcome separation, policy
version and Python fixture roundtrip; queue byte/slot limits including an
oversize and full queue; monotonic attempted/admitted/loss counts and explicit
unknown on simulated crash with no clean checkpoint; no deadlock or blocking
callback when queue is full; independent original decision/error unchanged;
no live path imports or caller-controlled origin proof. Include Owner/global,
taskless/main/assistant/unbound/shell/continuous/workflow and cross-org,
missing target and incomplete/cyclic chain; separate close-epic vs set-PoC,
settings field restrictions, batch partial results and legacy/strict wire.
Test in a private labelled target with a separately reserved non-overlapping
CPU/memory driver and exact-head receipts; no driver is granted by this doc.
The later durable slice must additionally test crash before/after frame/fsync,
partial/corrupt tail, disk full including reserve, reexec/reboot, lost marker,
remote ack uncertainty, duplicate/different digest, central crash before/after
commit, ACL denial, expired cursor, held retention, WAL quota/headroom and
wall-clock jumps. Neither unit tests nor one week's elapsed time prove the
full production inventory until every site and host is reconciled.

Engineering decisions in this contract are ceilings, schema/transport and
queue implementation choices; they require evidence and independent review,
not a new Owner policy ruling. A genuine policy question arises only if later
evidence demands choosing between losing observation coverage and changing
user authorization/availability, retaining data beyond an approved hold, or a
new S7 scope rule; route a concrete choice rather than smuggling it into
engineering defaults. Until then the existing program authorization stands.
