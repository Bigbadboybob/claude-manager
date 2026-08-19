//! Crate-local test helpers for `cm-holder-proto`. Compiled only in
//! test builds via `#[cfg(test)] mod test_support;` in `lib.rs`.
//!
//! This is the proto-crate sibling of `daemon/src/test_support.rs`
//! (and `tui/src/test_support.rs`): each crate's test binary is its
//! own process, so each carries its own env mutex — no cross-crate
//! coordination is needed or possible. It exists because relocating
//! `reexec_manifest` here (DESIGN_HOLDER_BRAIN_SPLIT phase 1, review
//! finding C15) moved a unit test that serializes access to the
//! `CM_REEXEC_MANIFEST_FD` environment variable; the import path
//! `crate::test_support::env_lock` is kept identical so the moved
//! test's body is untouched.

use std::sync::{Mutex, MutexGuard};

/// One mutex for every test in this crate that touches process-global
/// state (environment variables). Recovers from a panicked test by
/// ignoring poisoning so the rest of the suite still runs.
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}
