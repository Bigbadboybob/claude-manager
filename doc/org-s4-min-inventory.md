# S4-min source inventory (working assessment)

Baseline: `a2029aba68ddbf7b75534e67eae9ae4c811ce5f7`. The **Decided** section
and assumptions register in `org-process-provenance.md` control S4-min; the
earlier stronger security design does not. This inventory records source facts
and missing evidence, not a claim that S4-min is implemented or released.

## Central source and positive-path obstacle

`sql/028_org_core.sql` defines `incarnations` with member, role, participant
PID, daemon ID, UID, occupancy generation and open/ended status. At this
baseline the only `INSERT INTO incarnations` is in
`dispatch/org_db.py:_create_member`, and it inserts a `main` row. Ordinary
local child spawn in `daemon/src/control/methods.rs` calls
`assistants::stamp_spawn` and does not create an open central assistant or
worker incarnation. `member_for_pid` falls back to `org_session_facts` for
assistants/workers; those heartbeat facts coalesce old member and role values
and are legacy O5 evidence, not the required central open-incarnation proof.
Do not substitute the parent's incarnation, a task, an ancestry link, a
workspace or a stored O5 stamp for the child's missing central record.

`dispatch/org_db.py:version` is the actual feed version: `max(org_changes.seq)`.
`snapshot` reads this and member/open-main data in one repeatable-read
transaction. `_shape_member` includes `main.pid`, `daemon_id`, `session_uid`,
`occupancy_generation` and `since`; `_MEMBER_SELECT` only joins an unended
`main` incarnation. `org_changes` records member and incarnation changes.
The snapshot is a candidate source for an exact open main at an as-of version,
not a current-at-effect guarantee or a process-bound proof by itself.

The daemon's `items/org.rs:MemberCache` stores heartbeat `by_uid` summaries and
an observation time; `heartbeat_extras` returns neither the org version nor
open-incarnation fields. `org.snapshot` is an on-demand RPC, not an existing
daemon-maintained freshness snapshot. No daemon latest-seen org version for
this cache was found. S4-min adds a separate in-memory snapshot refreshed by
the existing heartbeat and a monotonic latest-observed feed version; it does
not replace the lifecycle cache. A version must not be invented from local time
or member revision; a verified claim needs a coherent source snapshot and
explicit tracking of newer observed org changes. The independently observed
frontier is only what this daemon has seen, not global freshness.

`org_db.py:promote` creates an open main for a promoted assistant. It returns a
member plus a top-level version fetched on another connection *after* the
member write; concurrent changes can make that version unrelated to the
returned member. `daemon/src/items/org.rs:promote` currently calls `note_main`
without a version-bound process association. A locally spawned then promoted
main could be a positive subject only after a coherent versioned central
snapshot and a separately established matching local spawn/process origin are
available. This does not establish a positive for ordinary local assistants.
CM ruling `e3634390` accepts this narrower positive: only a locally spawned,
subsequently open main may be verified from a coherent snapshot and local
origin binding. It does not add assistant incarnations. Assistants and workers
retain their spawn-time O5 member and role as `o5-spawn-stamp`, explicitly
unverified and as-of spawn; this is separate from current main evidence. A
missing/stale main snapshot cannot establish a verified claim. Do not call an
all-unknown implementation complete.

## Producer and consumer boundaries

| Boundary at baseline | Necessary compatibility / attribution constraint |
|---|---|
| Local `mcp_start_session` → `start_session` → `assistants::stamp_spawn` | Compute only from an already obtained snapshot before existing recovery/retirement gates; no new gate, wait, disk or network. Preserve O5 stamp and lifecycle as legacy. New source facts must bind the actual subject, not its parent; workflow, Fresh, TUI and unsupported restore/adopt paths remain unknown. |
| `items/rpc.rs:Ctx::capture`, `actor`, `item_set`, `item_resolve`, `resolve_holder` | Actor and each target are different subjects. `api/items_models.py` uses strict `Actor`/`Holder`; unsupported or old API must see exactly old wire. Capability knowledge must already exist, never be discovered through an extra call in the write path. No change to authorization. |
| `dispatch/items_rules.py` and `items_db.py:_load_items/_apply` | Holder edits load, then delete/reinsert rows. New nullable holder metadata must survive an old-client replacement, reorder and role change for retained identity; removed and new unknown holders remain removed/unknown. No arbitrary `via` is a grant. |
| `messaging/rpc.rs` → `messaging/store.rs:request/send` | The existing request digest hashes caller params before publication. Adding a variable snapshot to those params changes retry identity. Any daemon-only evidence must preserve the old request digest and receipt behavior. Forwarded/remote origin is unknown, and must not inherit a local sender claim. |
| `items/heartbeat.rs` → `org_db.py:heartbeat_extras` | Existing heartbeat is best-effort and cannot block ordinary operations. Any additive central snapshot or support announcement must be coherent and independently versioned, compatible with older peers, and not create a new operation-path network call or timer. |

## Verification after explicit driver transfer

Use isolated positive and negative fixtures for the real source/version,
locally supported subject, stale/newer observed version, missing/contradictory
record and excluded paths; separately test independent actor/target claims.
Exercise a contended snapshot and queued heartbeat writer while ordinary
spawn/item/chat behavior remains unchanged and completes. Verify existing O5
lifecycle, strict old API wire, capability absence and old/new holder
replacement on isolated storage. Review the whole diff for no provenance gate,
new authority, backfill, timer, rules/notice change or cross-host assertion.
The #56 driver reservation has not been transferred; no test/build is performed
by this inventory.

S4-min writing is off unless the daemon starts with `CM_S4_MIN_WRITE=1`.
Rollback requires stopping these writes on every participating daemon before
rolling the API back; the last capability snapshot is as-of and cannot protect
an uncoordinated API downgrade before its next refresh. No data migration back
is needed. An older API can delete and reinsert holder rows without preserving
their current metadata, even after new daemon writes stop. The new API also
records holder evidence in additive item-event fields, so historical evidence
survives that old writer; current holder attribution is unknown after such a
rollback unless preservation is independently established. This is a release
constraint, not authorization to deploy. Snapshot
refresh uses the existing heartbeat thread and adds no timer; process-origin
evidence is intentionally lost on brain restart/adoption.
