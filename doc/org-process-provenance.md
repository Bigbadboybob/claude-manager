# Process-bound provenance for S4: proposed contract

Status: **reviewed proposal with binding CM engineering decisions below**.
The proposal through “Proposed decisions and minimum future verification” was
independently reviewed at `597b97be` on 2026-10-28. The final **Decided** section
supersedes that proposal for S4-min, including its credentials, admission gates,
new receipts and remote-work requirements. No runtime implementation is claimed.
Baseline is `origin/main` `0c2fcd8d5e84279e16c1572ad3e1a6cf6998a63d`.
O1, O4–O6 and the signer/attribution/authorization separation in
`doc/org-model.md` remain in force. The local S5 observation and spool contracts
and S6 occupancy plan are planning inputs, not deployed guarantees. Rejected S4
head `ad069d9` is evidence, not a base or implementation to activate. This
document proposes a narrow evidence contract; it does not design an S6 registry,
grant member authority, change legacy operations, or authorize rollout.

## Present evidence and trust boundary

| Present source at baseline | What it establishes and its limit |
|---|---|
| `daemon/src/control/protocol.rs:Request` and `CallerSession`; `control/operator.rs` | A session caller supplies a UID on the same-user 0600 control socket. `validate_operator` accepts any Operator-shaped caller when the token is unset; only the separate strong gate requires a configured, matching token. A UID in an envelope, environment or `/proc` alone does not authenticate the actual originating agent process. Same OS UID is a trust boundary, not isolation against a malicious local peer. |
| `daemon/src/session.rs:DaemonSession`, `holder_mode.rs`, `holder-proto/src/holder_manifest.rs`; `adopt.rs:proc_starttime` | Registry, holder incarnation and process start information can help identify a live child. Holder incarnation has a lineage high-water mark; transcript generation, process start, holder incarnation, brain epoch and S6 occupancy generation are distinct. None alone proves who sent an RPC. |
| `assistants.rs:stamp_spawn`, `member_for_spawn`, `fork_org`, `backfill`; `control/methods.rs:start_session` | O5 supplies existing local lifecycle stamps and assistant/worker behavior. Main-cache, persisted stamp, ancestor/task fallback and workspace are not independently process-bound evidence. Preserve their current lifecycle semantics; do not retroactively certify them. |
| `items/org.rs:session_facts`, `items/heartbeat.rs`, `dispatch/org_db.py` | Heartbeats carry assistant facts; central `org_session_facts` coalesces omitted member/role with old values. Omission cannot clear a historical stamp or prove it is current. A cache, timestamp, checksum or heartbeat alone is not this contract's proof. |
| `api/auth.py:verify_token`, `api/items_models.py:Actor/Holder` | The shared API bearer and caller-provided actor fields do not authenticate a daemon, process or member. Existing strict models reject unknown fields; direct API/TUI-originated writes remain unverified unless an independently trusted origin is established. |
| `control/methods.rs:start_workflow`, `compose_continuous_spawn_params`, `trigger`; `continuous/scheduler.rs` | Workflow role bindings and Fresh run/fire tokens serve their existing workflow and exact-run purposes. A token or task relationship is not proof of a member or general authority. |

`pid` below means OS PID where qualified; a CM participant PID
`agent:<daemon>:<uid>` is an address, not an OS process identity. Neither a
session UID, task, workspace, native transcript UUID, nor a recycled OS PID is
proof of the others. The proposed contract assumes cooperating trusted host
daemons/holders and host OS; a compromised daemon, stolen issuer credential,
or malicious same-UID process outside the admitted channel is not contained.
If that stronger threat model is required, host isolation and credential
protection must be designed separately. Do not label bearer possession, process
ancestry or a signed daemon assertion as cryptographic proof that a particular
model produced specific words.

## Proposed evidence unit

For an eligible write, a trusted local adapter at the daemon boundary captures
an immutable **execution binding** `P`: issuer/host identity and boot lineage,
daemon identity, session UID, holder lineage and incarnation where applicable,
child OS PID plus kernel start identity (boot-scoped), transcript generation,
and a daemon-minted non-reused binding epoch. Missing required fields are
unknown, not zero. A pidfd or equivalent live kernel check supports liveness;
an uncheckable binding is unknown. Holderless/adopted paths need an equally
strong, reviewed binding or remain unknown. Transcript generation is context,
not a process generation or an occupancy generation. No ID is trusted merely
because the request supplies it.

**Proposed request admission:** introduce a versioned, per-binding authenticated
session channel/credential issued through trusted spawn or verified adoption;
the receiver resolves it to `P`, checks the live binding and never accepts a
caller-selected UID/member as proof. The existing proxy must present that
credential on each request, with method, canonical payload digest, audience,
operation ID and a replay-resistant sequence or nonce. Bind the credential to
the session channel and invalidate it on binding replacement. Credentials are
secret, not logged or placed in evidence. A valid credential proves use of the
authorized session channel within the stated same-user trust boundary; it does
not identify which descendant composed the request or resist a same-UID
credential thief. Existing UID-only calls continue under existing behavior,
but have `unverified_origin` for this evidence contract. Operator and internal
scheduler actions have separate origin classes, not a fabricated agent signer.
An Operator-shaped request accepted by the compatibility path with token
validation disabled is `unverified_origin`. A configured, matching token may
establish the operator channel within the stated same-user threat boundary;
an invalid token is not validated. Classification is independent of whether
the existing endpoint allows or denies the operation, and does not change that
authorization. A missing authenticated channel must never be upgraded from a
matching UID or an authenticated daemon-to-API connection.

A separate **member claim** `M` says member ID, role (`main`, `assistant`,
`worker`), source and source revision, as-of point, and the precise `P` to
which the trusted issuer binds it. A main requires authoritative current
incarnation/occupancy evidence, checked against its version; an assistant or
worker requires an explicitly established, auditable binding from a trusted
spawn/fork/assignment transition and its source evidence. A reference to the
old O5 stamp may be retained as *legacy*, not promoted to verified `M`. Role is
an attribution claim, not a grant. If the authority source is absent, stale,
contradictory, incomplete or cannot associate its record with `P`, `M` is
`unknown(reason)`; `no_member` is known only from a complete authoritative
negative. A pending placement is unknown, not unbound. An unresolved source
must not fall through to the caller, ancestor, task or workspace to create a
known claim. Separately preserve legacy lifecycle stamps and protections.

Every record has separate evidence slots: **initiator** `A` origin and its own
binding and member claim or unknown; **executor** session binding and its own
member claim or unknown; and **target**, when applicable, its own binding and
member claim or unknown. Each claim has its own source, revision and as-of;
each binding identifies the subject it describes. Operator/system initiators
have their own validated or unknown origin and do not acquire an invented
session or member. Record a causal edge and operation ID from initiator to
executor and any target, plus trusted issuer/custodian, operation and object,
and the actual authorization decision and policy version.
For a delegated or asynchronous effect, retain `A` and the causal operation;
the executor is not silently rewritten as the author. Never fill one slot's
member, role or binding from another slot merely because a causal edge exists:
an explicitly established inheritance is a separate evidence-bearing
transition. A holder target's claim is derived for that target, never copied
from the signer. Current member status is distinct from an event-time claim;
old records keep their original
evidence, including unknown, and a correction is a linked, auditable event.
Do not reveal credentials, raw prompts or private transcripts in evidence.

## Issuance, forwarding and write ordering

1. At admission, authenticate the channel, resolve the live `P`, and capture
   actor, target and versioned member-source facts under the existing local
   synchronization. Allocate an operation ID. The trusted adapter, not the
   client, constructs the attribution envelope and canonical request digest.
   Do not make network or disk calls while holding the global daemon mutex.
2. Before a local irreversible effect, revalidate the exact binding, required
   claim/version, applicable existing authorization and target under their
   effect synchronization. If relevant state changed, no verified attribution
   is emitted; hold/refuse only where the existing operation or an explicitly
   enabled new contract requires it. Existing PID-based behavior and its
   errors remain unchanged during evidence-only adoption.
3. For a central write, propose a mutually authenticated, enrolled daemon
   issuer and a versioned integrity-protected envelope bound to destination,
   method, operation ID, payload digest and the separate initiator, executor
   and target bindings and claims or unknown. The API verifies issuer
   entitlement, binding/custody, signature or equivalent
   authenticated channel, request binding, source version and replay before
   accepting it as *issuer-attested as-of evidence*. The present shared bearer
   cannot do that. Verification failures produce unknown or refusal on the new
   contract; never downgrade a failed verified write into a legacy retry.
   A central member/occupancy-dependent privileged effect needs a current
   authoritative generation check in the same transaction as the effect,
   per the proposed S6 plan. This spec supplies no such authority and does not
   claim a distributed atomic transaction between daemon and API.
4. A remote forwarding daemon retains the authenticated origin envelope and
   records itself as forwarder, not as original actor. The destination verifies
   issuer, audience, digest, nonce and source binding; a handback links the
   original operation and authenticated receiver receipt. Connection identity,
   a matching body, shared bearer, or a peer's assertion about another host is
   insufficient. If origin, binding, freshness, receipt or target ownership
   cannot be verified, retain unknown; do not infer a remote member. Cross-host
   revocation cannot be instantaneous without a central fence at effect.

**Lock rule:** a single explicit per-UID admission unit owns any necessary
local lifecycle/identity permit; pass an unforgeable in-process guard/context
to nested spawn or input helpers rather than reacquiring its `RwLock`. Never
block on a nested read behind a queued writer. Acquire multiple UID permits in
a documented global order or fail promptly and release. Do not hold global
daemon, registry or cache locks across network, disk, subprocess or PTY I/O.
Heartbeat transition takes an exclusive unit only after releasing those locks,
then rechecks exact binding/version; no lock upgrade.

The per-UID unit may cover a specifically bounded, reviewed **local effect**:
for example, revalidate the exact live target binding and input state, then
write the accepted PTY body and Enter as one unit before releasing it. The
target cannot be rebound or retired through cooperating CM paths during that
unit. Define actual write/timeout bounds and partial-body, failed Enter and
restart behavior before implementation; a partial or uncertain effect remains
unknown and is not replayed automatically. Spawn needs its own reviewed
bounded local admission/effect boundary, including actual child binding and
failure cleanup; it cannot rely solely on a pre-spawn check.

Network, arbitrary or unbounded I/O is not held inside the per-UID unit. If an
effect must cross that boundary, release the unit, retain durable operation
intent and captured binding/claim versions, perform the effect with a recipient
that checks the applicable expected generation at its own effect boundary,
then reacquire and revalidate before any dependent local effect. If the
recipient cannot enforce that fence, a post-call check cannot undo a possibly
committed effect: classify the outcome and attribution as unknown, preserve
evidence and reconcile explicitly. A timeout, changed binding, permit failure
or ambiguous remote receipt never implies no effect. The local unit cannot
serialize independent external native clients. These are proposed invariants,
not a claim that the rejected gate implementation satisfies them; the rules
snapshot remains as-of, not distributed current-at-write.

## Lifecycle matrix

| Transition | Proposed attribution and custody behavior |
|---|---|
| Local spawn | Record the authenticated initiating binding and spawn intent before the effect; bind the actual spawned child `P` and resulting member/role only after both are verifiable. Ordinary children inherit the verified member as assistants; continuous-member children remain workers under existing lifecycle. Failed/ambiguous spawn preserves intent and unknown outcome, never creates a second child blindly. |
| Fork | Record initiator separately from source. Bind a new child process; derive membership only from verified source at the fork boundary, not from the initiator if the source is unknown. The fork does not inherit source authority. Existing O5 behavior remains until a reviewed adapter can establish this evidence. |
| Workflow | Record initiating actor, workflow run/role and actual bound or fresh participant separately. An already bound participant uses its own verified `P/M`; a fresh participant requires an explicit verified assignment and its own process binding. Unknown creator/participant is unknown, not a member inferred from workspace or task. Workflow lifecycle/role permissions stay unchanged. |
| Fresh / persistent continuous | Record scheduler as issuer, fire/run identity and executor; bind each Fresh process independently and use the existing run token only for its scoped run semantics. Persistent execution revalidates its process binding at each admission. A trusted continuous-member assignment may describe a worker; it is not main occupancy proof. No automatic disposal or authority change follows attribution. |
| Revive, A-R, rotation, handoff, rebind | UID reuse, PID reuse and transcript changes cannot reuse an old `P`: establish a new binding epoch and child PID/start, and independently establish `M`. Link predecessor as history; a native UUID is not proof of a live process. A-R's `claude_recovery.rs:Identity` includes UID, workspace, transcript generation, holder incarnation and PID/start controls for its own recovery, not a general attribution proof. No automatic native stop or recovery effect is introduced here. |
| Brain restart/adoption | A surviving process may retain a binding only when verified holder custody and exact child/start identity survive and are revalidated; otherwise issue a new binding or unknown. Preserve durable evidence, but do not treat a restored manifest, prior cache or last-known membership as current proof. Unclean/missing custody makes the interval unknown. |
| Holder upgrade / rehello | `holder_mode.rs` renegotiates holder hello/rehello; manifest high-water prevents incarnation reuse within its holder lineage. Proposed evidence must verify same authenticated lineage and exact live child/incarnation across upgrade; capability renegotiation alone does not prove them. Changed or unverifiable lineage yields unknown/new binding. |
| Promotion, role change, replacement | Keep `P` distinct from `M`; record old and new claim revisions and the exact effective boundary. Serialize local admissions or revalidate at effect, then obtain authoritative transition result. An ambiguous central response is unresolved, not a new role or a failed effect. Same UID revival cannot resurrect a superseded main. S6 occupancy generation and replacement policy remain S6's responsibility. |

## Persistence, compatibility and boundaries

Persist immutable evidence and an operation/payload-bound receipt atomically
with each central write where supported. The replay scope binds issuer and
authenticated principal, original binding epoch, audience, method and operation
ID; the stored canonical digest also binds the complete effect-bearing request
and evidence. Admission of a new operation consumes its nonce/sequence and
reserves the scoped key atomically with the outcome or durable unknown intent.
An authenticated exact retry with the same scope and digest retrieves the
original pending/outcome/receipt without re-executing or rebinding; changed
payload, principal or binding conflicts. A different issuer cannot claim the
same operation's receipt by presenting its ID. Unknown, expired or missing
replay state cannot authorize fresh execution of that operation.

After the original process/channel is gone, an independently authenticated and
authorized reconciliation principal may inspect that exact historical key and
digest and resolve its recorded outcome; it is not the original signer and
cannot use the old credential to initiate a new effect. Require a bounded,
durable replay/receipt retention floor covering retries, crash recovery and
reconciliation before activation. Exhaustion or uncertain retention yields
explicit unknown/refusal rather than pretending a key is unused. For external
effects, retain durable intent before effect, then an outcome/receipt; crash or lost response
is unknown until independently reconciled by exact operation, source and
binding. Never automatically replay an uncertain input or write under a new
identity, retire receipts to manufacture a safe queue, or convert a failed
lookup into a negative. A checksum is damage detection, not issuer custody.
Link corrections to prior immutable facts and preserve holes, source versions,
revocations and coverage. Retention/recovery of legacy lifecycle information
must not depend on whether its current attribution is known.

Use additive versioned readers/storage first, then explicit capability and
issuer negotiation. Old strict APIs get the old request with no new fields;
old clients retain old behavior and unknown provenance. New API/old daemon
holder-list replacement, reorder and role changes must preserve existing
member/role/via data for the same retained holder under the database lock;
removed and new unknown holders remain removed/unknown. No arbitrary client
`via` is authority. A rollback floor must preserve evidence, holder metadata,
operation-bound receipts and all writers that can replace those rows; otherwise
do not enable writers. A stale/unsupported capability is unknown, not success.
Do not backfill historical records from current membership or assume an old
API receipt binds a request payload. Keep the API's coalesced lifecycle facts
out of verified-current S4 evidence until an independently reviewed,
source-bound uncertainty transition exists.

Standing rules are separate from process proof: a known pending placement or
known member without an actual validated rules snapshot is unresolved. The
last successfully validated snapshot is an **as-of** boundary, not a
distributed current-at-write guarantee. Preserve the original work on refusal,
avoid uncertain automatic replay, and do not paste an asynchronous brief into
a changed composer or later turn. Full rule delivery and notice cutover remain
deferred. S5 observations retain explicit unknown and existing authorization
outcomes; S6 cannot consume a claim as occupancy authority merely because it
is stored or forwarded.

## Proposed decisions and minimum future verification

The following are proposals for coordinator/independent review, not new Owner
rulings: (1) same-user/cooperating-host threat boundary and per-binding channel
credential; (2) enrolled authenticated daemon issuer and evidence transport,
including key provisioning, revocation and replay retention; (3) exact
authoritative member-binding source, revision and freshness requirements per
role; (4) per-UID admission guard discipline and failure behavior; (5) receipt,
correction and preserving rollback floor; (6) which evidence-only paths can
ship first while others remain unknown. No arbitrary freshness TTL is decided
here; without a proven source/version and a reviewed bound, claim unknown.
S6's proposed five-minute refresh is not an S4 decision. Owner's existing O5
assistant durability and existing scope rules are not reopened.

Before any implementation is advertised as verified: exercise configured and
matching, configured and invalid, and unconfigured compatibility Operator
paths, distinguishing the existing endpoint decision from origin evidence;
prove forged UIDs and wrong, expired or revoked credentials, and a replayed
nonce presented as a new operation, cannot establish a new binding or effect;
an authenticated exact retry follows the receipt rule above. Verify that known
compromise/revocation prevents later
verified claims subject to the documented remote window. Undetected theft of a
valid credential is outside this threat model and cannot establish actual
process/model authorship. Verify actual child PID/start and incarnation through
spawn, fork,
revive, rebind, restart and holder rehello; caller and target replacement and
promotion races; queued-writer nested spawn/input with timeout and lock-order
checks; workflow and Fresh origin/executor/run distinctions, including an
initiator from member A, executor from member B and a remote target/handback
with independently unknown or known claims, plus scheduler/operator cases with
no fabricated executor member; remote issuer,
forward/handback replay, revocation and unavailable-source cases; crash seams
before/after local and central effects with exact-receipt reconciliation,
concurrent duplicate calls, lost reply, restart, cross-issuer collision and
expired/unknown retention; and
old/new API, old daemon and rollback holder-preservation cases on isolated
fixtures. Check central transaction fencing separately before any S6 authority
use. Show unknown rather than inferred attribution in every missing, ambiguous
and stale case, preserve rule/brief boundaries and existing O5 lifecycle, and
obtain independent review of actual implementation and release compatibility.
These are future verification gates, not tests performed for this spec.

## Decided (CM 2026-10-28)

Authority: Swarm-Coord's engineering decision
`8c9334f8-112f-457a-be52-95fe1328d987`, created
`2026-10-28T15:43:16.588Z`. CM explicitly owns these engineering choices under
Owner's altitude rule; they are not requests for another Owner approval. This
section is the implementation contract for **S4-min**. The stronger historical
proposal above is context, not additional requirements to implement now.

1. **Purpose and trust.** S4 is attribution for display and routing, not a
   security mechanism. Assume one owner, cooperating trusted hosts and the
   same OS user, with no adversary. The existing operator token, SSH tunnel and
   0600 socket are the trust boundary. Add no per-binding credentials, key
   provisioning or signing. A `verified` attribution means a trusted daemon's
   supported as-of claim under this model; it does not authenticate model
   authorship, resist a malicious same-user process, grant authority, or prove
   that membership remained current when a later effect happened. Legacy
   lifecycle and endpoint authorization behavior remain unchanged.
2. **Issuer and transport.** The daemon issues the attribution on the existing
   daemon-to-API path, using the API token plus daemon ID. Use existing
   request-ID/receipt idempotency. Add no replay or retention store, new nonce
   protocol, new signing path, or historical reconciliation service.
3. **Member source and freshness.** Use the central org record—member plus
   open incarnation—as recorded at spawn, adoption or promotion, cached by the
   daemon with its org version. If the version is unknown or older than the
   latest org event the daemon has seen, record `unknown`. There is no TTL.
   Missing, contradictory or unsupported subject/source evidence is unknown;
   do not derive a verified member from an ancestor, workspace, task, legacy
   O5 stamp or caller-supplied member. The actual source and version fields
   must be identified and covered by the implementation's proof; an invented
   local revision is not a substitute for the central org version.
4. **No provenance gate.** Never block or delay a write or spawn for
   provenance. Compute the claim from an already obtained snapshot, outside
   all locks and before acquiring an existing operation gate. Any contention
   or timeout yields unknown and leaves the existing operation path alone.
   Snapshot acquisition itself must be nonblocking; release any successful
   short try-lock before computing the claim. Add no new admission permit,
   retirement gate, lock upgrade, nested acquisition or wait for a heartbeat,
   member lookup, network call or disk operation. Do not convert uncertainty
   into a provenance-induced refusal or automatic retry. The claim is as-of
   its snapshot, with no new claim of effect-time fencing.
5. **Storage and rollback.** Use additive nullable columns or JSON fields;
   old readers ignore the additions. Rollback means stop writing attribution,
   with no migration back. Validate the actual reader/writer compatibility:
   existing strict API schemas require compatible parsing and old endpoints
   must receive the old request shape. Preserve existing write semantics and
   historical attribution without adding a new receipt system or activating
   the rejected broad S4 migration. No production rollback or backfill is
   implied by this docs change.
6. **First implementation slice.** Cover daemon-originated writes for locally
   spawned sessions: spawn, item set/resolve and chat send, plus distinguish
   existing O5 stamps as `legacy` from supported `verified` claims. Everything
   else—remote origin/forwarding/handback, workflows, Fresh, TUI/API-direct and
   unsupported restored/adopted paths—remains unknown for S4-min. Recording a
   central member source during adoption does not by itself admit adoption
   to the supported first slice. These paths remain S6 follow-ups. Use one
   existing implementation lane and a deadlock-focused independent review;
   no cross-host implementation work. S6 waits on accepted S4-min.

S4-min retains separate initiating actor, executor and target meanings where
the existing operation exposes them; never copy one subject's member into
another subject's attribution. It does not add a general causal-event system.
Known pending placement remains unknown rather than falsely unbound, and old
records are not certified from current membership. Scope and authorization
checks continue to use their existing sources. Standing-rule delivery and the
rejected S4 gate/notice changes remain outside this slice.

### Assumptions register for S4-min

This register applies the assumption audit to the reviewed proposal after
CM's six decisions. `CARRIED (CM)` means an explicit delegated engineering
decision from the receipt above, not an invented Owner ruling. `CONSEQUENCE`
records an observable limit of those choices. Unselected proposal mechanisms
are overridden, not implementation prerequisites. No new numeric defaults
or Owner decisions are introduced here.

| # | Assumption / value | Provenance | Scope | Blast radius and cheapest verification | Status |
|---|---|---|---|---|---|
| 1 | Single-owner, trusted cooperating hosts, same-user, no adversary | CARRIED (CM 1) | Trust | Attribution cannot establish hostile-process identity; assert this meaning in docs and output tests | approved by CM |
| 2 | Existing token/SSH/0600 socket; no per-binding credentials or signing | CARRIED (CM 1) | Transport | Possession or daemon assertion is not model authorship; exclude credential/issuer infrastructure from diff | approved by CM; overrides proposal |
| 3 | Attribution for display/routing never grants authority | CARRIED (CM 1; existing org-model separation) | Consumers | Reusing a label in permissions would change control; review consumers and unchanged authorization fixtures | approved |
| 4 | Daemon issuer, existing API token plus daemon ID | CARRIED (CM 2) | Writes | Direct/client-provided fields could be mislabelled; fixture supported daemon path versus unknown direct path | approved by CM |
| 5 | Existing request-ID receipts only; no new replay/retention store | CARRIED (CM 2) | Idempotency | New retention semantics could alter writes; keep existing duplicate behavior and audit storage diff | approved by CM; overrides proposal |
| 6 | Central member plus open incarnation cached at spawn/adopt/promote with org version | CARRIED (CM 3) | Member source | Wrong subject/version would produce a plausible wrong label; identify actual fields and fixture one positive supported local case | implementation evidence gate |
| 7 | Unknown version or version older than latest observed org event yields unknown; no TTL | CARRIED (CM 3) | Freshness | Missing events can leave an as-of claim older than central state; test seen-version invalidation and disclose observation boundary | approved by CM |
| 8 | Snapshot claims do not prove globally current or effect-time membership | CONSEQUENCE (3, 4) | Meaning | A membership change after snapshot can precede the effect; race fixture must preserve operation and as-of label | accepted scope limit |
| 9 | Nonblocking snapshot; computation outside locks before existing gates | CARRIED (CM 4) | Locking | A hidden acquisition can reproduce spawn/heartbeat deadlock; contention/queued-writer fixtures plus lock-path review | implementation evidence gate |
| 10 | Unknown on contention/timeout; no provenance refusal, wait or retry | CARRIED (CM 4) | Failure behavior | Unavailable attribution must not prevent work; test contended/missing source alongside ordinary operation success | approved by CM |
| 11 | No new lifecycle permit, credentials, receipt reconciliation or distributed fence | CONSEQUENCE (1, 2, 4) | Exclusions | Importing the historical proposal reopens the rejected broad migration; inspect complete diff | overrides proposal |
| 12 | Additive nullable storage; rollback stops writes, no migration back | CARRIED (CM 5) | Persistence | An old parser or whole-row rewrite can lose fields or reject writes; targeted new/old schema and writer fixtures | implementation evidence gate |
| 13 | Old strict endpoints receive old wire shape; no mutation retry after ambiguous response | CARRIED (existing API behavior); CONSEQUENCE (5) | Compatibility | Unknown JSON fields can reject live operations; capability/unsupported fixture before new-field use | implementation evidence gate |
| 14 | Local spawn, item set/resolve and chat send only | CARRIED (CM 6) | Coverage | Uncovered origin must not appear verified; explicit supported-path inventory and positive/negative fixtures | approved by CM |
| 15 | Remote/handback/workflow/Fresh/TUI/API-direct/unsupported adoption remain unknown | CARRIED (CM 6); CONSEQUENCE (coverage) | Exclusions | Reusing a cached O5 stamp could falsely certify excluded paths; origin-matrix fixtures | approved by CM |
| 16 | Legacy O5 stamps retained and labelled legacy; no lifecycle change | CARRIED (CM 6; existing O5) | Lifecycle | Dropping stamps or changing teardown alters assistants/workers; existing lifecycle controls remain | approved |
| 17 | Initiator, executor and target claims cannot fill one another | CARRIED (reviewed spec; existing subject meanings) | Subjects | Cross-member writes can gain the wrong label; fixture distinct subjects and independently unknown claims | approved scope invariant |
| 18 | No retroactive certification or changes to rules/notice delivery | CONSEQUENCE (first slice); CARRIED (deferred S4) | History/rules | Old records or unsent work could be silently changed; no backfill/delivery code in diff | excluded |
| 19 | One implementation lane, independent deadlock review; S6 waits | CARRIED (CM 6) | Sequencing | Premature S6 use could treat incomplete coverage as authority; release receipt names actual accepted coverage | approved by CM |

### Source-feasibility ruling (CM 2026-10-28)

CM receipt `e3634390-a12b-4ce1-86c9-a544efbe8b2c`, created
`2026-10-28T15:52:26.290Z`, resolves the source feasibility gate above:

- A main-only positive path satisfies S4-min. Use an asynchronous producer of
  the existing, consistent versioned `/org/snapshot`, outside operation locks,
  together with an exact local origin binding. Missing or stale version is
  unknown. Add no assistant-specific central incarnation or broader central
  data contract.
- Assistants and workers retain their existing O5 spawn-time member and role
  with the distinct provenance label `o5-spawn-stamp`: as-of spawn and
  unverified. Do not erase that legacy attribution to bare unknown. Existing
  `member_id` and `acting_as` role meanings remain intact; the provenance label
  is separate. Readers distinguish it from a supported verified main claim;
  neither label claims current-at-effect membership.
- Daemon-internal chat metadata must preserve the existing request digest.
  Item capability negotiation is permitted, and old endpoints keep their old
  request shape. The actual preserving-reader/writer checks still apply.

Assumptions 6, 14–16 are thus narrowed by explicit CM ruling: the supported
positive central-source path is local mains; O5 legacy attribution for
assistants/workers remains visible and unverified. No unsupported origin is
promoted to verified. The diagnostic work in #56 keeps the shared test/build
driver priority; S4-min source work may proceed in its one existing lane.

The implementation evidence gates are engineering checks, not requests for
Owner permission. If an existing source cannot support a positive local claim
without new protocol or cross-host work, report that to CM and keep the claim
unknown; do not broaden the slice or claim S4-min complete with no supported
positive path.
