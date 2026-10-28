# Process-bound provenance for S4: proposed contract

Status: **specification for independent review**, not an implemented or
approved protocol. Baseline is `origin/main` `0c2fcd8d5e84279e16c1572ad3e1a6cf6998a63d`.
O1, O4–O6 and the signer/attribution/authorization separation in
`doc/org-model.md` remain in force. The local S5 observation and spool contracts
and S6 occupancy plan are planning inputs, not deployed guarantees. Rejected S4
head `ad069d9` is evidence, not a base or implementation to activate. This
document proposes a narrow evidence contract; it does not design an S6 registry,
grant member authority, change legacy operations, or authorize rollout.

## Present evidence and trust boundary

| Present source at baseline | What it establishes and its limit |
|---|---|
| `daemon/src/control/protocol.rs:Request` and `CallerSession`; `control/operator.rs` | A session caller supplies a UID on the same-user 0600 control socket. Operator validation is a separate path. A UID in an envelope, environment or `/proc` alone does not authenticate the actual originating agent process. Same OS UID is a trust boundary, not isolation against a malicious local peer. |
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
scheduler actions have distinct validated origin classes, not a fabricated
agent signer. A missing authenticated channel must never be upgraded from a
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

Every record distinguishes initiating actor `A` (session, validated operator,
or system), executing session `P`, trusted issuer/custodian, `M` if known,
operation and object, and the actual authorization decision and policy version.
For a delegated or asynchronous effect, retain `A` and the causal operation;
the executor is not silently rewritten as the author. A holder target's claim
is derived for that target, never copied from the signer. Current member
status is distinct from an event-time claim; old records keep their original
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
   method, operation ID, payload digest, `P`, `A`, and `M` or unknown. The API
   verifies issuer entitlement, binding/custody, signature or equivalent
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
a documented global order or fail promptly and release; no external I/O while
holding them. Heartbeat transition takes an exclusive unit only after releasing
registry/cache locks, then rechecks exact binding/version; no lock upgrade.
Capture, release for I/O, and revalidate before effect where an exclusive unit
cannot be held. Timeouts, permit failure and asynchronous effects stay explicit
unknown rather than silently authorizing. This is a proposed synchronization
invariant, not a claim that the rejected gate implementation satisfies it.

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
with each central write where supported. A same-key, same-digest replay returns
its original receipt; a changed payload conflicts. For external effects, retain
durable intent before effect, then an outcome/receipt; crash or lost response
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

Before any implementation is advertised as verified: prove channel binding
against forged UID, stolen/replayed/expired credential within the stated
threat model; actual child PID/start and incarnation through spawn, fork,
revive, rebind, restart and holder rehello; caller and target replacement and
promotion races; queued-writer nested spawn/input with timeout and lock-order
checks; workflow and Fresh origin/executor/run distinctions; remote issuer,
forward/handback replay, revocation and unavailable-source cases; crash seams
before/after local and central effects with exact-receipt reconciliation; and
old/new API, old daemon and rollback holder-preservation cases on isolated
fixtures. Check central transaction fencing separately before any S6 authority
use. Show unknown rather than inferred attribution in every missing, ambiguous
and stale case, preserve rule/brief boundaries and existing O5 lifecycle, and
obtain independent review of actual implementation and release compatibility.
These are future verification gates, not tests performed for this spec.
