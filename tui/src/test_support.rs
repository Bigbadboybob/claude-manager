//! Crate-wide test helpers. Compiled only in test builds (gated by
//! `#[cfg(test)] mod test_support;` in `main.rs`).
//!
//! This module exists primarily to host the **single** crate-wide HOME
//! mutex. Cargo runs tests in parallel across modules, so a per-module
//! `static LOCK` doesn't actually serialize HOME-mutating tests in
//! different modules — they end up with independent mutexes and race
//! each other. Every test that touches `std::env::set_var("HOME", ...)`
//! must grab `home_lock()`.

use std::sync::{Mutex, MutexGuard};

/// One mutex for the entire crate. Acquire before any HOME mutation.
/// Use `unwrap_or_else(|p| p.into_inner())` to recover from poisoned
/// state in tests that panic — the next test should still be able to
/// run cleanly.
pub(crate) fn home_lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}
