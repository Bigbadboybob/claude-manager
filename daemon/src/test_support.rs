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

/// Requested allocation totals on this test thread only (not RSS, live bytes,
/// allocator capacity or allocations on spawned threads). System remains the
/// allocator; disabled outside an explicitly measured fixture closure.
pub(crate) mod allocations {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
    };

    #[derive(Clone, Copy, Debug, Default)]
    pub struct Counts {
        pub calls: u64,
        pub bytes: u64,
    }
    thread_local! { static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) }; }

    struct Counting;
    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    fn record(bytes: usize) {
        let _ = COUNTS.try_with(|slot| {
            if let Some(mut counts) = slot.get() {
                counts.calls += 1;
                counts.bytes += bytes as u64;
                slot.set(Some(counts));
            }
        });
    }
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = System.alloc(layout);
            if !p.is_null() {
                record(layout.size());
            }
            p
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let p = System.alloc_zeroed(layout);
            if !p.is_null() {
                record(layout.size());
            }
            p
        }
        unsafe fn realloc(&self, p: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            let p = System.realloc(p, layout, size);
            if !p.is_null() {
                record(size);
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
            System.dealloc(p, layout);
        }
    }

    pub fn measure<T>(run: impl FnOnce() -> T) -> (T, Counts) {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                COUNTS.with(|s| s.set(None));
            }
        }
        COUNTS.with(|s| {
            assert!(s.get().is_none(), "nested allocation measurement");
            s.set(Some(Counts::default()));
        });
        let _reset = Reset;
        let result = run();
        (result, COUNTS.with(|s| s.get().unwrap()))
    }
}
