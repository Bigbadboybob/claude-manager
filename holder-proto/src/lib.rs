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

#[cfg(test)]
mod guard_tests {
    //! Source-scan guards for the crate's protocol law (the skew
    //! suite's compile-time arm).

    /// The additive-only discipline: `deny_unknown_fields` is BANNED
    /// on the CHANNEL — an old binary must parse a newer peer's
    /// frames, ignoring fields it doesn't know. The sealed re-exec
    /// manifest is deliberately exempt: it is schema-version-GATED
    /// (`UnsupportedSchemaVersion` refuses a shape it doesn't speak)
    /// and its strictness is part of its validation posture (R8) —
    /// cross-version handoffs go through `rollback_schema_version`,
    /// never through silent field-dropping.
    #[test]
    fn no_deny_unknown_fields_on_the_channel() {
        // Needle assembled at runtime so this test's own source
        // can't self-match.
        let needle = ["deny_", "unknown_fields"].concat();
        for (name, src) in [
            ("channel.rs", include_str!("channel.rs")),
            ("lib.rs", include_str!("lib.rs")),
        ] {
            let hits = src
                .lines()
                .filter(|l| l.contains(&needle) && l.contains("#["))
                .count();
            assert_eq!(hits, 0, "{name} carries a {needle} attribute");
        }
    }

    /// The version-literal cross-check (§ Version-skew testing): a
    /// PROTO_VERSION bump must be a DELIBERATE act accompanied by a
    /// skew-matrix update — this pin fails on any bump until the
    /// bumper (a) re-runs `scripts/holder-skew-matrix` against the
    /// current baseline, (b) records the compat decision in
    /// DESIGN_HOLDER_BRAIN_SPLIT.md's § Version-skew testing, and
    /// (c) updates this literal. Never "fix the test" alone.
    #[test]
    fn proto_version_bump_requires_a_matrix_update() {
        assert_eq!(crate::channel::PROTO_VERSION_MIN, 1);
        assert_eq!(crate::channel::PROTO_VERSION_MAX, 1);
    }

    /// S10: every SCM_RIGHTS receive passes MSG_CMSG_CLOEXEC — a
    /// received fd must never be inheritable by a concurrently
    /// forked child. One recvmsg call site exists; it must carry the
    /// flag on the same call.
    #[test]
    fn recvmsg_always_passes_cmsg_cloexec() {
        let src = include_str!("channel.rs");
        let recv_sites: Vec<&str> = src
            .lines()
            .filter(|l| l.contains("libc::recvmsg("))
            .collect();
        assert!(!recv_sites.is_empty(), "expected a recvmsg call site");
        for site in recv_sites {
            assert!(
                site.contains("MSG_CMSG_CLOEXEC"),
                "recvmsg without MSG_CMSG_CLOEXEC: {site}"
            );
        }
    }
}
