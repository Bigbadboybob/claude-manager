//! Slice 12b: `HostId` newtype. Lives in the daemon crate so
//! `cm_daemon::manifest::ManifestEntry` can carry the field as a
//! first-class type without a layering inversion (the manifest is
//! daemon-owned, the host-config is TUI-owned). The TUI's
//! `hosts::HostId` re-exports this type.
//!
//! No logic — pure newtype. The Phase 3 host-abstraction belongs
//! in the TUI (`tui::hosts`); the daemon just needs a typed
//! identifier for the manifest schema.
//!
//! ## Why on the daemon side?
//!
//! Pre-12b the manifest entry was host-agnostic — every session
//! was implicitly local. Phase 3 (per
//! `daemon/NOTES.md` slice 12b) adds a `host_id` field. Two
//! options for where the type lives:
//!
//! 1. **Plain `String` on the daemon side, typed at the TUI
//!    boundary.** No daemon-crate change beyond a field; TUI
//!    converts string ↔ HostId at load. Lost the typed wrapper
//!    on the daemon side but the daemon doesn't actually USE
//!    host_id for any logic (it just stores it in the manifest).
//! 2. **`HostId` newtype on the daemon side, re-exported by the
//!    TUI.** Typed safety preserved across the layer. The newtype
//!    is a 1-line struct with no deps.
//!
//! The slice plan spec says `host_id: HostId` literally; chose (2)
//! to match. The daemon crate doesn't grow much surface — this
//! module is ~20 lines of code excluding doc and tests.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Stable identifier for a host. `String` newtype with
/// `#[serde(transparent)]` so it serializes as a bare string in
/// TOML / JSON. Names are case-sensitive (no normalization), match
/// the value of the `name = "..."` field in `~/.cm/hosts.toml`.
///
/// The reserved value `""` is rejected at TUI-side validation
/// (`tui::hosts::HostsConfig::validate`) — `HostId(String::new())`
/// is what an unset / defaulted-by-mistake field would carry, and
/// silently accepting it would mask bugs. The daemon crate itself
/// doesn't validate the value; it's only ever read+written through
/// the manifest.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HostId(pub String);

impl HostId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The "local" host. Used as the default for pre-12 manifest
    /// entries (`#[serde(default = "...")]` on `ManifestEntry::host_id`),
    /// for the TUI's synthesized-default hosts.toml, and as the
    /// fallback whenever a host-aware site needs a known-safe
    /// identifier.
    ///
    /// Returning the value rather than a `const &'static str` lets
    /// callers write `HostId::local()` without `.to_string()` clutter
    /// and matches the rest of the constructor surface.
    pub fn local() -> Self {
        Self("local".to_string())
    }
}

/// `local` is the default host — lets `ManifestWorkspace` (which gained a
/// `host_id` field, DESIGN_REMOVE_GLOBAL_HOST.md) keep deriving `Default`, and
/// any `HostId::default()` reads as the local machine.
impl Default for HostId {
    fn default() -> Self {
        Self::local()
    }
}

impl fmt::Display for HostId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_id_serde_transparent() {
        // The `#[serde(transparent)]` attr means HostId
        // serializes as a bare string, not an object. Critical
        // for the wire shape on disk (`host_id = "local"` in TOML
        // / `"host_id": "local"` in JSON).
        let id = HostId::new("manager");
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, r#""manager""#);
        let back: HostId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, HostId::new("manager"));
    }

    #[test]
    fn host_id_local_constant() {
        assert_eq!(HostId::local(), HostId::new("local"));
        assert_eq!(HostId::local().as_str(), "local");
    }

    #[test]
    fn host_id_display_is_inner_string() {
        // `HostId(String)` should `Display` as the inner string
        // verbatim. Surface this so log lines and error messages
        // don't render `HostId("manager")` to the operator.
        assert_eq!(format!("{}", HostId::new("manager")), "manager");
    }
}
