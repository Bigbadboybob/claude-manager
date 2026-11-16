//! The org-model preservation bridge (doc/org-model.md §12, stage S0b).
//!
//! Every persisted or transported session record (the durable manifest and
//! its tombstones, the re-exec [`SessionRecord`](crate::reexec_manifest::SessionRecord),
//! resume identities, the TUI manifest) carries one NESTED, OPAQUE `org`
//! object, and every path that rebuilds a record copies it unchanged. It is
//! deliberately a JSON map, not a typed struct: a bridge-capable binary must
//! round-trip keys a later stage adds (`member_id`, `occupancy_id`, …)
//! without knowing them, so a rollback onto it loses nothing. It is never
//! serde-flattened into its record, so nothing in it can collide with an
//! existing top-level field such as the transcript `generation`.
//!
//! Keys (all optional; S0b itself only ever writes `workspace_kind`):
//! - `member_id`, `occupancy_id`, `acting_as`: org identity (S2+).
//! - `occupancy_generation`: the member's monotonic occupancy counter
//!   (S6 fencing). NOT the transcript `generation`, which counts
//!   transcript rebinds and is reused across revives (see the
//!   `SessionRecord::generation` doc); the two never share a value.
//! - `workspace_kind`: `home` / `stream` / `ephemeral`; absent = unclassified.
//! - `binding_epoch`: the central binding version; when set, local
//!   reconstruction never re-infers the task↔workspace relation it covers.

use serde_json::{Map, Value};

/// The opaque `org` object. `None` on a record means "never set", which
/// serializes as an absent key, so pre-feature records stay byte-identical.
pub type OrgObject = Map<String, Value>;

/// Capability names (doc/org-model.md §12). Each is advertised separately by
/// the brain and by the holder; a host has one only when both (and the
/// rollback pin) do.
pub const ORG_PRESERVE_V1: &str = "org_preserve_v1";
/// Reserved: terminal sweep honours `terminal_via=item` and workspace kinds,
/// and local `home` marks are applied (S3).
pub const LIFECYCLE_GUARD_V1: &str = "lifecycle_guard_v1";
/// Reserved: daemons stamp `member_id` / `acting_as` on writes (S4).
pub const ORG_STAMP_V1: &str = "org_stamp_v1";
/// Reserved: occupancy rows and generations, standby, fencing (S6).
pub const OCCUPANCY_V1: &str = "occupancy_v1";

/// What this build of the shared record code preserves. The holder reports
/// this list in its hello; the brain intersects it with its own.
pub fn preserved_capabilities() -> Vec<String> {
    vec![ORG_PRESERVE_V1.to_string()]
}

pub const WORKSPACE_KIND: &str = "workspace_kind";
pub const KIND_EPHEMERAL: &str = "ephemeral";
pub const BINDING_EPOCH: &str = "binding_epoch";
pub const OCCUPANCY_GENERATION: &str = "occupancy_generation";

/// `org.workspace_kind`, when set.
pub fn workspace_kind(org: Option<&OrgObject>) -> Option<&str> {
    org?.get(WORKSPACE_KIND)?.as_str()
}

/// `org.binding_epoch`, when set to a positive integer.
pub fn binding_epoch(org: Option<&OrgObject>) -> Option<u64> {
    org?.get(BINDING_EPOCH)?.as_u64().filter(|e| *e > 0)
}

/// Set one key, creating the object if needed.
pub fn set(org: &mut Option<OrgObject>, key: &str, value: Value) {
    org.get_or_insert_with(Map::new).insert(key.to_string(), value);
}
