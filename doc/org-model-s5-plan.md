# S5 plan: profiles, discovery and observable scope shadow

Status: **design for coordinator review**; not code, a cutover or a release
request. Source baseline `origin/main` `0c2fcd8`, `doc/org-model.md` §§6,
8, 10–13, root assumptions register `033e63d`, and the accepted S6 plan
(root local integration `fdab6ba`; source design head `4aef822`). S5 runs in
parallel with S4 #47 and S6. Org owns S4 and its sole CPUs 12–13 / 10 GB test
driver. This design uses no driver, build, test, live DB or session control.
The existing Owner program Go covers the direction; engineering review and
evidence gates below do not create a new Owner approval requirement.

## Existing behavior and boundaries

* `sql/028_org_core.sql` stores `members.profile` JSONB, `revision`, standing
  rules, `org_events` and `org_changes`; `dispatch/org_db.py:_shape_member`
  exposes title, mandate, focus note/pin and returns `focus: None`. The S2
  snapshot/member/team reads exist. There is no general `profile_set` writer,
  profile event history, S5 evidence index, `who` or `owner_of` at this head.
  `org(view="member"|"team"|"me")` and legacy aliases already exist in
  `mcp_server/server.py`. Preserve their read contract and old PID clients.
* Team charter, rules, lead and status already have Owner-only edits through
  `api/org.py:TeamPatchBody` and `dispatch/org_db.py:team_update`, revision
  and request replay. S5 must not silently widen that existing endpoint while
  S4 delivers rules. `members.profile` accepts older plain and wrapped
  values (`_profile_value`); migrate additively without resetting either.
* `tui/src/app/team_view.rs` already implements A-v Status → Task → Team,
  team/member/session rows, folds, Owner/unassigned fallback and persisted
  selection; `orgchart_teams.rs` shares `session_attachments`, which prefers
  member stamps then spawner, task-parent and workspace fallbacks. These are
  **display inference only**, never authority or authorship. `org_model.rs`
  polls visible views and uses versioned snapshots; cached attachments key on
  org version and session/task links. The Team view falls back to Task when
  no usable org snapshot exists. It is not a new sidebar to rebuild and is
  distinct from cancelled #46. The reported move-to-sidebar gap needs an
  end-to-end move → org change/version → refreshed snapshot → attachment and
  Team row check; source alone does not establish its cause. Current default
  is Status when no stored view is present (`app.rs`); `persist.rs` stores
  Status, Task or Team. Preserve the saved choice and do not silently change
  the default. Task remains available; the O6/S8 direction to make
  Task a filter/default transition is a separately reviewed S8 delivery, not
  this first S5 slice. Distinguish A-v Team from F6 chart in reproduction.
* Existing session **read** expansion for report members via
  `daemon/src/items/org.rs:read_authorized_target` and
  `control/methods.rs:resolve_authorized_session` is shipped and performs
  identity revalidation. Do not mistake it for the S7 control rule or remove
  it. For actual controls `control/auth.rs:check_session_caller` checks self,
  global, same/descendant task or taskless same workspace; missing task tree
  can yield `TaskTreeNotYetSynced`. `items/rpc.rs:authorize_edit` admits
  Owner/global, task board, a holder of any open item on that board, or an
  explicitly joined board member. API board/item rules and gate-specific
  checks add their own restrictions. Capture the actual complete decision,
  including earlier denials and endpoint-specific restrictions; do not assume
  every board operation has the same check. These are baseline facts at
  `0c2fcd8`, to re-inventory after S4 integration.
* S4 attribution is not authority: its compatible reader/storage, negotiated
  writer and rollback-pin work is being reviewed independently. A23–A25 apply:
  strict `Actor`/`Holder` wire models forbid unknown fields; old readers and
  old rollback binaries must preserve calls, partial writes and rule delivery.
  Never trust caller-supplied member identity as signer, infer an unknown
  member, change continuous/Fresh lifecycle, or silently truncate rules.
  `org_stamp_v1` must mean complete, reviewed attribution, not just a field.

## Profiles: additive write contract

Propose `profile_set(member?, changes, expected_revision, request_id)` through
MCP → daemon trusted actor → API → `dispatch/org_db.py`, with API-compatible
version negotiation. Absent member means authenticated caller's member, never
an arbitrary supplied stamp. Owner is the operator actor; a member's active
main is established from authoritative binding and manager chain. Assistants,
workers, shells, stale or unknown bindings cannot impersonate the member.
Check authorization and expected revision **inside the same transaction and
lock** as the update, history event, feed change and request receipt. A request
ID scoped to actor/method and request hash returns the original result on
identical retry; changed payload rejects; uncertain response is resolved by
receipt lookup. CAS conflict returns current revision with no partial field
writes. Existing `org_requests` stores actor/method/request and response but
not a payload hash; the proposed hash mismatch guarantee requires an additive
compatible receipt design rather than assuming current replay enforces it. Preserve omitted versus explicit clear (including mixed permitted and
forbidden fields: reject atomically); no newly invented field-length rejection
or silent truncation of existing values.

| Field / action | Proposed target rule from §8.2 | Notes / safe initial behavior |
|---|---|---|
| `title`, `mandate` | Owner or active main on manager chain | Preserve existing title/mandate and source metadata; no self-edit by default. |
| `focus` | Derived, not writable | Show its evidence and freshness/unknown; do not overwrite with an asserted value. |
| `focus_note`, `ask_me_about`, `availability_note` | Member's own authenticated main; Owner/manager only where specified | Self-note expiry and numeric limits remain gated, no silent expiry in first slice. Tags without supporting evidence display unverified. |
| `focus_pinned`, `on_call`, `deputy` | Manager chain or Owner; self may clear `on_call` only | Keep manager pin distinct from derived focus; resolve deputy as member, not authority transfer. Record on-call window explicitly if provided. |
| `not_my_area` | Self main or manager chain or Owner | Redirect is a contact hint, not a grant. |
| Team `charter`, `standing_rules` | Spec target team lead/Owner, but existing `team_update` is Owner-only | Preserve existing shipped Owner-only path in the initial slice; design and review any lead-writer extension separately with S4. Do not modify S4's delivery path in parallel. |

Persist authored `{value, set_by, via, set_at}` and immutable field-level
history containing actual signer PID, trusted member attribution if known,
authorization basis, old/new values or safe references, revision, request and
time. History access follows its own visibility check; no shadow record needs
profile text. A legacy plain value remains readable, has unknown provenance
until an explicit new edit, and is never restamped from current membership.
CAS protects concurrent profile/team writes; no direct changes to manager,
team, `standing_rules`, titles on another object, or org authority via
`profile_set`. Do not infer assistant approval from attribution. Update
`org_changes`/snapshot version in the same transaction and verify old
`_profile_value` / TUI consumers still read both representations.

## Evidence and discovery contracts

Four separately labelled facts are returned: **worked on**, **reviewed**,
**owns now**, **may decide**. A discovery match is not a grant. `who(topic,
intent=explain|owner|approve, scope, available, limit)` filters intent first;
`owner_of(path|component|repo)` distinguishes current owner-role holds/jobs and
linked paths from historical authors. For `approve`, return applicable
content-bound existing gate grants and, when the separate rulings system exists,
verified Owner rulings. An `approver` holder is a decision contact with
`authority_unverified`, not proof of approval power. An unavailable authority
may be shown with its deputy but is not replaced by an unqualified expert;
current authority checks and historical signer proof remain separate. Owner is
returned only under the documented Owner-authority exception (§9.5); this does
not make discovery itself grant permission. No new grants registry is created.

| Source / producer at baseline | What can be established; ingestion rule |
|---|---|
| Items: `sql/017_items.sql:item_events`, `dispatch/items_rules.py:BoardTx.apply`, `items_db.py:_apply`; S3 holder roles | The creation snapshot records holder PIDs and owner PID; subsequent events store only changed `prev/new` fields. Reconstruct owner at close only from complete ordered creation + owner-change history and same-transaction semantics; current `item_holders`, `closed_by`, or clicker is not proof of the historical owner. Snapshots do not preserve reviewer roles, so holder PID history alone cannot prove a reviewer; require separately preserved event-time role evidence or mark unknown. Same-operation reassignment/close needs an explicit contract/fixture. Missing history/legacy co-owners => unknown/unverified, no manufactured attribution. Correct reopen/drop and handoff by append-only correction. |
| Receipts/grants: `sql/023_item_receipts.sql`, `dispatch/gate_db.py`, `gate_rules.py` | Receipt records poster PID, board/item, gate/head and verdict; it does not automatically establish reviewer or grantor. Grant carries immutable `grantor_pid`, message proof and binding, plus revocation. Attribute only the supported source role; retain issuance board/key and distinguish historical signed evidence from current grant applicability. |
| Epic `groups`, close plan/events: `dispatch/epics.py`, `sql/029_org_s3_groups_placement.sql` | Existing team/PoC and close decisions are available, but current lead is not automatically historical PoC. Use event-time evidence only when a complete event identifies it; unknown otherwise. |
| Commits: `scripts/worktree_lineage.py:install_trailer_hook`, `commit_trailers`, `trailers` | Hook chains `prepare-commit-msg`, appends `CM-Session` and optionally `CM-Member` from environment, skips merge commits, preserves existing trailers; installation/coverage is not universal and trailers are assertions requiring provenance validation. Ingest authored commit SHA + repo, not merger/lander or a shared checkout; distinguish author identity from event-time verified member. Missing/conflicting/forged trailers => unknown, not a member. No branch/workspace co-tenancy inference; optional backfill is a separately labelled, lower-confidence future path. |
| `report_done`: daemon session state and `state.rs` tombstones | Reason and time exist on local live/retained exited session; not a durable, central, complete S5 feed. An explicit versioned daemon→API forwarder with stable event ID, replay/ack and coverage is future work; no fabricated historical archive, no private transcript ingestion. Reason text is untrusted. |
| Current work: board holder/role, team lead/PoC, declared item links/job | Query live state as of labelled snapshot; this is a current claim, not history or approval. Parse path safely and filter visibility first. `OWNER.md`/VM scanner is later, not implied by an item link. |
| Owner rulings, member mail, message/mention evidence | Rulings are external prerequisite; member mail, OWNER.md/VM scanner and mention ingestion are later. Never count private DMs, even if the caller is a DM participant, in this discovery index. |

Each evidence record has immutable per-subject identity `(source kind,
immutable source key, attribution role, credited subject)`; §14's proposed
`UNIQUE(source_key, role, member_id)` captures the member-backed case when
source keys are namespaced. One source can credit multiple reviewers or
contributors in the same role, so they must remain separate. A verified
member ID is a subject only when the event-time record supports it. Preserve
a legacy PID as a typed PID subject with unknown member provenance; if even a
stable subject reference is missing, retain an explicit unresolved source
occurrence rather than collapsing it into a shared null identity or inventing
someone. A later binding does not retroactively relabel an old PID/unknown
record: any evidence-backed attribution correction is a new auditable event,
linked to the exact prior record and any newly credited subject, with source
and justification. Never silently infer membership from a current binding.

Carry source revision, event time, actor/signer, trusted attribution when
present, confidence/provenance, visibility scope and correction/supersession
links. Deduplicate identical per-subject replay, while distinct subjects and
roles remain distinct. Corrections target the exact immutable record, append
idempotently and preserve the prior history. A cursor/watermark is per source
and records holes, reset, replay, ingestion failure and earliest coverage; old
sources and old clients without S4 fields remain `unknown`. Rebuild from
source plus corrections must yield the same records and attribution, including
legacy PID and unresolved occurrences; preserve original board/item receipt
and grant keys through S6's many-alias migration.

**Privacy before computation:** authenticate the caller; derive readable
boards/public channels and its joined channels using the authoritative current
read policy, and filter each source **before** candidate generation, counts,
score, ranking, snippets, result size or cache. Never count inaccessible
sources or DMs, including in aggregate, timing-derived candidate totals or
shared precomputed scores. Owner follows the documented exception, while DMs
remain excluded from this index. Use per-caller/visibility-version keyed caches
or recompute; revalidate on membership revocation and fail closed for unknown
visibility. For offline/incomplete ACL or source coverage return explicit
partial/unknown coverage, not a fabricated complete zero or stale privileged
result. Do not include private source IDs, titles, counts or score changes in
errors or logs visible to an unauthorized caller. References point to existing
read tools and each is checked under the caller's current scope.

Wrap every agent-authored excerpt, item/profile title, reason, path-derived
text and reference title in explicit `untrusted_text` output with fixed
consumer guidance (§8.6); validate structured IDs separately. No imperative
blocklist and no execution of text, runbook paths or profile instructions.
Return actor, evidence role, verification status, applicable scope and as-of
coverage, with no inferred authority from scores or `via` attribution. Old org
reads remain wire-compatible; new read APIs are additive/versioned.

## Shadow authority: observe both decisions, never enforce

The actual old policy remains authoritative, including existing independent
read expansion and endpoint-specific checks; proposed policy is computed only
from the same captured facts and does not send, mutate, grant, alter errors,
slow an operation into failure or influence the result. Treat `unknown` as a
third result when inputs, scope, identity, snapshot, policy version or source
coverage are insufficient; it is neither allow nor deny. No substitution of a
TUI's display attachment for trusted identity. Compare actual and candidate
using the same actor/target/operation snapshot, preserve request and generation
identities, and explicitly label racing/changing identities as indeterminate.

| Decision family and source sites to inventory on implementation head | Actual result vs proposed observation |
|---|---|
| Session control: `daemon/src/control/auth.rs:check_session_caller`, `caller_reaches_target`, `control/methods.rs` send/kill and other target resolution, `control/send_key.rs`, `control/dispatch.rs` task scope and session-control methods | Record each effective authorization path, including operator bypass, self, global, tasked subtree, taskless same-workspace, missing task tree and target missing/exited. Candidate §6.3: main's own assistants + reports' sessions, assistant only its own spawns, Owner; team membership itself grants no control; preserve cross-org boundary. Read-only report-member access is separately tagged and not counted as control. |
| Board edit: `daemon/src/items/rpc.rs:authorize_edit` and all callers, `dispatch/items_rules.py:BoardTx`, `dispatch/items_db.py` settings/steward checks, `dispatch/board_members.py`, `dispatch/epics.py`, `dispatch/gate_db.py`, `api/items.py` | Capture daemon entry decision and actual central/end-specific outcome separately, never double-count a single request as two independent operations. Candidate §6.4: board team members, holder on its own item, delegated assistant on delegated work, team lead and steward/orchestrator where appropriate; settings, PoC and gate authorization retain distinct checks. Current broad holder/joined-member behavior remains authoritative until S7. |

Inventory **all** entry points, including Owner/operator, global-perms,
taskless and tasked callers, main, assistant, unbound/legacy/unknown, shell,
continuous worker, workflow participant, target missing/exited, cross-org,
manager/guest/board-member, and stale/replaced bindings. Actor/target role and
org inputs must come from reviewed S4/authoritative provenance where known;
otherwise candidate is `unknown`. Preserve existing shell and continuous
lifecycle and global-perms behavior; compare proposed cross-org semantics as a
shadow difference, never enforce it here. S6's only pre-S7 enforcement changes
are the named and logged `hirer_controls_hire` and
`main_controls_own_assistants`; distinguish them by policy/version when they
exist, and retain them on S7 rollback.

Proposed additive event shape: stable operation/correlation ID; host and
source site; timestamp plus monotonic ordering/boot epoch; caller class and
trusted actor/target IDs when available; org/snapshot and policy versions;
actual allow/deny/error/unknown and reason code; proposed allow/deny/unknown
and reason code; outcome category, source coverage and optional bounded
non-sensitive input references. No body, transcript, private message, full
profile or raw prompt. Record the actual decision independently of actual
execution outcome. For two-level board checks link component records and one
final result. Log **all** eligible observed decisions and all disagreements;
no arbitrary silent sampling. Expose aggregate denominators and counts by
host, site, caller class, old/candidate result and unknown reason, plus missed
coverage, queue overflow, errors, reboot/clock discontinuity, policy/version
and source freshness. An unknown or missing record is not an agreement.
Expected differences are classified with reason, named accountable reviewer,
expiry/closure state and evidence; never auto-whitelist by elapsed time.

Use an additive restricted-access observation store and change feed, separate
from authorization and ordinary public org snapshots. Ingestion failure must
not change the authorized operation: record a durable gap/overflow marker and
increment loss counters; if neither can be persisted, expose the host as
coverage unknown on next contact. Replay/dedupe on stable IDs, retain event and
counter watermarks, bounded batching/queue/storage and explicit gaps across
restart. Choose concrete byte quotas and retention window from observed volume,
existing host capacity and S7 observation requirements before activation;
provide operator-visible usage and overflow status and a retention plan that
keeps pending S7 evidence through review. Do not invent a silent eviction timer
or a fixed quota and call it approved. A local file/central store choice must
include access controls, durable transfer/reconciliation, clock-jump and
rollback fixtures. `sql/016_task_changes.sql` provides a concrete pattern:
commit-ordered advisory-lock sequencing, epoch and `pruned_through`, explicit
expired-cursor resnapshot, and `NOTIFY` only as a wake hint;
`dispatch/db.py:list_task_changes` pages from the durable log. Reuse these
semantics only after reviewing lock order and authorization of the new feed;
existing org/task feeds do not already store authorization observations.

About one week of clean/expected shadow differences is Owner-directed, but
S7 readiness also requires complete per-host/site/caller-class coverage,
identity/freshness gaps and outages explicitly accounted, explained differences,
no hidden overflow and a rollback rehearsal. During outages the approved S7
fail-static snapshot permits existing local relationships, rejects new ones;
last-known-main messaging addresses remain valid without granting authority.
An outage can make revocation delay unbounded, and a permitted stale snapshot
is not automatically a mismatch. Proposed five-minute refresh remains an
unruled timer, not a shadow readiness criterion.

## Slices, file boundaries and gates

1. **First implementable slice: dual-decision observation foundation.** After
   coordinator review, implement pure candidate evaluators and opt-in capture
   for both session control and board-edit decisions, with actual decisions
   unchanged, explicit unknowns, per-site coverage/gap counters and a bounded
   storage contract. Use named new modules for policy/evaluation and
   observation; minimize hooks into `daemon/src/control/auth.rs`,
   `control/methods.rs`/`dispatch.rs`, `items/rpc.rs` and central board rules.
   The smallest independently reviewable patch is pure evaluator modules plus
   table-driven fixtures, with no production integration or schema changes;
   then add opt-in capture and durable sink after their bounds are agreed.
   No profile writer, discovery ranking, scope enforcement, new timers or
   automatic lifecycle change in this slice. Integrate only after S4's exact
   schema/version contract and overlapping files are agreed with org. Unknown
   identity is a first-class result, so observation can run before universal
   stamping; it cannot be used as authorization evidence. If durable sink
   bounds are not established, ship evaluator/fixture and coverage design behind
   an inactive flag; do not silently discard observations and claim coverage.
2. **Profiles.** Add minimal API model/route, daemon RPC, MCP tool,
   `dispatch/org_db.py` transaction + additive history/feed migration,
   documented wire contract and tests. Coordinate `api/org.py`,
   `api/items_models.py`, `daemon/src/items/org.rs`, `mcp_server/server.py`,
   `sql/` migrations and `org_db.py` with S4. Start with explicitly
   authorized non-expiring fields and preserved existing caps; stage
   team-lead writes, note expiry and invented limits separately. Update MCP
   version snapshot if tool signatures change. Existing Owner-only team edit
   and S4 standing-rule delivery keep their semantics.
3. **Evidence then discovery.** Build source-specific immutable ingestion and
   visibility check before `who`/`owner_of`; start with source roles whose
   event-time attribution and readable scope are provable. Source fixtures
   before ranking: complete/incomplete item history, same-transaction close,
   receipt vs grant, merge/author, reopens, invalid trailers, old unstamped
   sessions and per-caller ACL revocation. No central report_done, rulings,
   owner scanner or member mail claimed until their separate producer ships.
   Return labelled evidence and unverified contacts, partial coverage and
   deterministic stable ordering; scoring/timers are gated below.
4. **Team view audit and narrow repair.** Use existing `team_view.rs`,
   `org_model.rs`, `orgchart_teams.rs`, `nav.rs` and snapshot/feed integration.
   Capture current default/persisted selection, reproduce move in Team and
   Task separately; verify org write bumps version, visible/focused polling,
   background/unfocused behavior, full/unchanged replies, attachment cache
   invalidation, members changing team, workspace and main identity, folds,
   reconnect/offline and old-daemon fallback. Fix only a demonstrated gap;
   preserve old clients and the Status/Task/Team cycle. Do not conflate
   `parent_task_id` task display with actual manager or change S8 default here.

For each slice: additive schema first, readers before writers, explicit
capabilities/version negotiation, idempotent startup and change feed; strict
old API/new daemon and new API/old daemon fixtures; no blind retries after
uncertain commits. Roll back feature switch to existing authoritative policy
and compatible readers without losing profile/history/unknown provenance;
retain S6 bootstrap and task edges through S7. Record source revision and host
capability/rollback floor. Preserve auth behavior if telemetry is unavailable.

Acceptance fixtures for slice 1 must exercise both policy families and every
caller/target class above; owner/global/self/taskless/unknown, current vs
candidate allow/deny/unknown, full effective board path, cross-org,
workflow/continuous, S4 old/new skew, identity race, no effect/unchanged
return, durable replay, sink failure, overflow/gap, restart, clock jumps and
coverage counts. Slice 2 adds permission matrix, spoofed member, stale revision,
concurrent writes, duplicate/reused request ID, omitted/null, unknown legacy
values, event/feed atomicity and no unauthorized reads. Slice 3 adds ACL
filter-before-count/rank, inaccessible evidence/DM leakage, historical signer
vs live authority, correction/idempotency and untrusted text. Include one
source with multiple same-role subjects, repeated ingestion, distinct roles,
legacy PID, unresolved subject, later member binding and an explicit
per-record correction; assert no collapsed credits, retroactive relabel or
replay/rebuild divergence. Slice 4 checks
actual rendered Team rows before/after a move and cache/reconnect behavior.
Run targeted suites and relevant workspace checks only after implementation is
approved and resource lane assigned; no jobs are authorized by this plan. A
future driver needs an explicit non-overlapping CPU/memory reservation,
labelled isolated target, one driver, exact-head evidence and post-review
cleanup; do not assume org's CPUs 12–13 / 10 GB are available.

## Assumption audit and decisions

Root register `033e63d` A1–A25, B1–B22, C1–C23 remains authoritative for
provenance; accepted S6 plan corrects lifecycle, push and historical-link
semantics. These are not reopened. This table reconciles the S5-relevant draft
choices; none of the invented numbers is implicitly approved by a design
review. `gate` is engineering evidence unless a real user-facing policy
choice is identified and routed by coordinator.

| Register / provenance | Risk and recommendation; cheapest evidence | Disposition |
|---|---|---|
| C11: title 60, mandate 400, focus 120, tags 8, redirects 5, availability 80, note 7 days; INVENTED | Existing values may be rejected, truncated or disappear. First preserve full legacy values and existing validation, no new expiry/caps; inspect sanitized existing shapes and boundary fixtures, then propose explicit validation and user-visible expiry. | Engineering compatibility gate; material expiry/limits choice if evidence shows a behavioral change |
| C12: source + role + credited subject, owner vs clicker, author vs lander; CONSEQUENCE/CARRIED | Incorrect credit affects routing. Reconstruct only proven event-time roles; inspect actual `item_events`, closed/reopen specimens and hook coverage; unknown otherwise. | Engineering provenance gate |
| C13: weights 5/3/2/2/2, short-close half credit, team routing cap and unspecified branch cap; INVENTED | Ranking misroutes or rewards gaming; no numerical score or automatic routing in first discovery slice. Compare labelled, readable interview/source specimens and evaluate deterministic evidence categories first. | Engineering evaluation gate; adopt weights/caps only after evidence and policy review if material |
| C14: token overlap and no model on read path; CARRIED | Poor relevance or cost surprise. Recommend deterministic whole-word matching on eligible visible sources; compare small representative queries and measured latency. | Engineering quality gate, preserve no-model direction |
| C15: mandate drift 60d, nudge once/2wk; INVENTED | Spam or missed drift. No automatic drift nudge/timer initially; surface explicit evidence timestamps and unknowns. Assess real drift specimens first. | Engineering evidence gate; user-visible automation policy if introduced |
| C16: untrusted text, tag charset/length and reference title cap; CARRIED safety / INVENTED limits | Injection or broken legacy data. Label untrusted text from the first read, preserve existing values; compare actual fields and client parsers, reject unsafe structured IDs without truncating legacy prose. | Safety contract; numeric caps gated |
| C17: default limit 5, three refs, availability factors 1/.8/.3, unspecified load penalty; INVENTED; intent-before-rank CARRIED | Hidden results or inappropriate contact. Preserve intent and authority separation; propose paginated explicit caller limit under existing transport bounds, neutral deterministic ordering and no availability multiplier until measured. Measure representative questions, response sizes and coverage. | Engineering evidence gate; user-facing ranking/default choice if adopted |
| C18: caller-readable evidence only; never DMs; Owner exception; CARRIED | Invisible evidence leaks via count/rank/cache. Filter before all computation and revalidate; two-caller/private-channel/revocation fixtures. Owner exception does not make private DMs discovery evidence. | Required privacy constraint |
| C19: success window 30m, inbound cap 5 and duplicate threshold; INVENTED | Misleading routing metric or dropped work. Defer automatic `ask`/caps; measure linked replies only after a separately reviewed event contract; no metric window silently chosen. | Engineering evidence gate; policy decision if active cap/routing is later proposed |
| B4: five-minute snapshot refresh, fail-static outage; INVENTED timer / CARRIED policy | Revocation delay is unbounded during outage. Preserve fail-static behavior, show snapshot age and coverage; measure host propagation and outage recovery before selecting a timer. | Required outage behavior; timer evidence gate |
| B17: 24h vacancy, 14d suspend, 3d promotion hint; INVENTED | Unintended hidden members or noisy nags. No new lifecycle timer in S5; inspect existing distributions and ask for a policy ruling only if automation is proposed. | Later policy gate, outside first slice |

Engineering options N1–N3 (observation transport/retention bounds, profile
history representation, source-specific visibility-aware index) require
implementation review and measured sizing, not Owner permission merely because
an implementation choice exists. Genuine potential Owner choices are limited
to adopting new user-visible expiry, automatic routing/caps or lifecycle/drift
automation if the coordinator decides to pursue them after evidence. None
blocks the observation foundation under existing authority. A concrete
byte/retention bound and complete decision-site inventory remain prerequisites
for activating its durable sink, not a license for silent sample/drop.
