//! Crate-wide test helpers for `cm-daemon`. Compiled only in test
//! builds via `#[cfg(test)] mod test_support;` in `lib.rs`.
//!
//! ## Why this exists
//!
//! Cargo runs each crate's test binary as a single process, and within
//! that process tests run in parallel threads by default. Anything that
//! reads or writes *process-global* state — environment variables,
//! umask, the CWD — has to be serialized or you get flaky cross-thread
//! interference.
//!
//! `tui/src/test_support.rs` is the matching helper on the TUI side
//! (different process, different mutex, no need to coordinate across
//! crates). When code relocates from the TUI to the daemon, the test
//! call shape changes from `crate::test_support::home_lock()` to
//! `crate::test_support::env_lock()`.

use std::sync::{Mutex, MutexGuard};

/// One mutex for every test in this crate that touches process-global
/// state — `HOME` and other env vars, `umask`, and (future) CWD. A
/// single mutex avoids the "two-mutex stomp" where a test in one module
/// flips `HOME` while a test in another module also flips it.
///
/// Recovers from a panicked test by ignoring poisoning so the rest of
/// the suite still runs.
pub(crate) fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}
