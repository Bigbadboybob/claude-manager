//! `cm-holder-proto` — the holder/brain compatibility surface.
//!
//! DESIGN_HOLDER_BRAIN_SPLIT phase 1 (§ Crate boundaries): the types
//! and framing that must stay legible across BINARY GENERATIONS — the
//! monolith image, the (future) cm-holder image, and every brain
//! build. Everything here carries the additive-only discipline from
//! the design's "Version-skew testing" section:
//!
//! - unknown JSON fields are ignored on both sides —
//!   `deny_unknown_fields` is banned in this crate (guard test in the
//!   skew suite);
//! - shape changes bump the relevant schema/proto version constant,
//!   and a version bump is accompanied by a skew-matrix entry;
//! - nothing in this crate may depend on daemon internals — the
//!   dependency arrow points daemon → proto, never back.
//!
//! Phase 1 relocates exactly one module: the sealed re-exec manifest
//! ([`reexec_manifest`]), verbatim from `daemon/src/reexec_manifest.rs`
//! (the daemon keeps a `pub use` shim at its old path so every
//! `cm_daemon::reexec_manifest::*` consumer — `reexec.rs`, the
//! `reexec_manifest_roles` / `verify_handoff` integration suites —
//! compiles unchanged). The manifest is shared language because the
//! migration writes it in one image and reads it in another, and the
//! rollback ladder can cross a schema boundary (the design's
//! `rollback_schema_version` contract). Later phases add the holder
//! channel here: frame envelope (`req_id`, length cap), verbs,
//! SCM_RIGHTS send/recv helpers, and the version negotiation
//! constants.

#![cfg(target_os = "linux")]

pub mod channel;
pub mod reexec_manifest;

#[cfg(test)]
mod test_support;
