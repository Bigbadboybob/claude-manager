# S4 attribution and standing-rule inventory (work in progress)

S4 records attribution; it does not change authorization, holder keys or
occupancy. PID remains the signer and existing authority checks remain in
force. An actor stamp describes the signer; a holder stamp describes the
holder, which may be a different session. Unknown and legacy identities stay
unknown, and a shell is never attributed as an agent. Continuous workers
remain workers. This document tracks the review surface; it does not assert
that an unchecked path has been implemented.

## Producers and durable consumers

* The common `control::methods::start_session` funnel serves MCP spawn,
  operator create/add, workflow participants, continuous fires, restore and
  revive. Fork supplies source-derived org separately. Resume/reexec,
  manifest/tombstone, holder adoption and rollback use the existing opaque
  `org` preservation bridge. Review the provenance of inputs to the common
  funnel separately from the authenticated operator RPC: MCP constructs its
  own parameters; restore and fork require their original trusted identity.
* Heartbeat emits `session_states` and `org_session_facts`; the latter already
  carries assistant/worker attribution, while main bindings come from the API.
  Continuous task mapping is keyed by daemon and continuous ID and is not an
  occupancy claim. A Fresh fire cannot be assumed to own a new main row.
  Known workflow-creator or continuous mappings may supply attribution and
  rules independently of lifecycle membership; they do not grant main
  authority or make a workflow role a durable assistant. No inference from
  arbitrary workspace co-tenancy; unresolved old provenance remains unknown.
* Item and org calls use `items::rpc::Ctx::actor`; translated task writes use
  `session_actor`. Actor fields flow through strict API `Actor` into item and
  org mutations, gate receipts/grants and placement. `item_events`, receipt
  poster, grantor, and `org_events.actor` are evidence consumers. Items'
  creator/closer, flag resolution and group changes must be traced as well.
* Holder references use `resolve_holder`, `holder_value` and the `Holder`
  API model, then `items_rules._holder` and the `item_holders` database
  roundtrip. Explicit target selection is not the same as signer identity;
  unknown remote targets must not inherit the caller's stamp. Board membership
  and launch grants are separate PID-bearing writes.
* Chat's local replicated immutable event `actor` and identity/name records
  come from the authenticated messaging context and store event machinery;
  authenticated peer calls carry the captured attribution separately from
  agent parameters. Scheduler-owned channel joins and naming capture their
  session graph under the daemon lock; deferred joins recheck the current
  binding and session graph before forwarding. System notices stay system
  events. Chat-derived item touches use the actual sent event's signer, so a
  later reuse of its UID cannot retag that event. Receipt, request-dedup, push-recipient, session-routing,
  manager and grant references include structural PIDs: distinguish these
  from attribution evidence, preserving exact PID semantics and current keys.
* Legacy HTTP and TUI writers, system-generated events and Owner calls have no
  daemon-established agent identity. They retain compatible unstamped records;
  member attribution must never be guessed from a PID or checkout.
* Explicit board joins retain the adding actor's member and role next to the
  unchanged `added_by` PID. The joined PID remains a structural board-membership
  key; `set_poc` joins use the same actor-evidence path.

## Rules and compatibility

The primary-team and member rule lists are separate labelled sources. Item
briefs additionally need the work's owning team when it differs from the
member's primary team; the same source is included once. No conflict
precedence is implied. Existing lists and source values must roundtrip in full;
runbook paths are references and are not executed. Continuous standing
instructions remain an independent work contract.

Initial deliveries include MCP/fork prompt delivery, continuous Fresh and
investigator prompts, workflow activation finalization, and operator/TUI
launches. Promptless starts and resume/respawn still need first-ping delivery.
The TUI planning launch queues its own prompt on the attached PTY, outside
`spawn_initial_prompt_delivery`. Its new-workspace (local and remote) and
existing-workspace paths ask the owning daemon to prepare the brief off the UI
thread before the pending write; preparation failure leaves it unsent. A fresh
operator session can be seated only after an asynchronous heartbeat, so its
member identity/rules are unknown before that reply. An old daemon falls back
to its legacy brief with a visible warning. This version-skew behavior is not
evidence that the complete rollout gate has been met.
On older APIs without the owning-team endpoint, an explicit item team can be
read directly but inherited epic/board team rules cannot be resolved; this is
also a version-skew source gap, not an empty-team assertion, and the brief
labels it unresolved. An MCP task launch whose task lookup fails labels the
work-team source unresolved; TUI preparation reports the API failure and
leaves the brief unsent.
The existing 64 KiB agent-input limit must not be bypassed or silently truncate
rules. For an oversized list, retain a complete immutable content-addressed
snapshot and deliver an explicit digest and retrievable reference; validate
the fully composed prompt before effects when possible. Main change
notices require durable retry and dedup across restart, including a rule set
that changes away and later changes back.

`Actor` and `Holder` currently reject unknown fields. The API needs additive
typed readers and storage before daemons send a new field. A new daemon must
negotiate explicit support with an old API and must not silently repeat a
possibly committed write; new API/old daemon and rollback remain compatible.
Capability `org_stamp_v1` must remain absent until the complete producer and
consumer matrix, preservation bridge, running holder and rollback pin are
verified. No holder upgrade is authorized by this work.
The new API preserves existing holder attribution on a rewrite even when the
writer is an old daemon. Rolling back the API binary to pre-S4 code can still
discard holder attribution when that code deletes and recreates holder rows;
the unadvertised capability is not a rollback guarantee. A rollout must pin a
preserving API/holder floor and verify it independently before advertising.

Attribution-only workflow and continuous parents carry their evidence through
spawns and forks in separate keys; this does not promote the child to a durable
assistant, claim its workspace, or confer main authority. Existing holder
`via_member_id` is retained during unrelated holder rewrites.
