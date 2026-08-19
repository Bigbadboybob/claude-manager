//! Re-export shim: the sealed re-exec manifest now lives in the
//! `cm-holder-proto` crate (DESIGN_HOLDER_BRAIN_SPLIT phase 1 — the
//! manifest is shared language between binary generations: the
//! monolith image, the future cm-holder image, and every brain build,
//! so it moved to the crate that owns the holder/brain compatibility
//! surface and its additive-only discipline).
//!
//! This shim keeps every existing path compiling unchanged —
//! `crate::reexec_manifest::{ReexecManifest, SessionRecord, …}` in
//! `reexec.rs` and `cm_daemon::reexec_manifest::*` in the
//! `reexec_manifest_roles` / `verify_handoff` integration suites all
//! resolve through it. New split-side code should depend on
//! `cm_holder_proto::reexec_manifest` directly; daemon-internal code
//! may keep using this path indefinitely.

pub use cm_holder_proto::reexec_manifest::*;
