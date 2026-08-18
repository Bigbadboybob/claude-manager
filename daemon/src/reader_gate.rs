//! Process-wide PTY-reader gate. Restart hardening /
//! DESIGN_SEAMLESS_RESTART phase 2b (review finding R3).
//!
//! ## Why this exists
//!
//! Every session runs a reader thread continuously draining the PTY
//! master into the process-memory fanout ring. Pre-2b, that thread
//! parked in a blocking `read()`: at a future re-exec handoff, up to
//! one read-buffer of bytes (8 KiB today) could sit on the reader's
//! stack between `read()` and `fanout.push()` — already removed from
//! the kernel's PTY buffer, about to vanish with the thread when the
//! old image is replaced. The phase-1 OS proof deliberately had no
//! reader, so its "PTY bytes wait in the kernel across the swap"
//! result holds ONLY if every reader is quiesced at a post-`push`
//! boundary first (the design's Quiesce step, "PTY readers" bullet).
//!
//! The fix is the structural twin of [`crate::reap_gate`], applied
//! to the byte path instead of the exit-status path: readers wait
//! for readability via non-consuming `poll(2)` on the master fd (see
//! `session.rs::poll_master_until_read_ready`) and perform the
//! `read()`→`fanout.push()` pair as one indivisible unit while
//! holding this gate's shared **read permit**. The future restart
//! coordinator (phase 2d) takes the exclusive write side
//! ([`freeze`]) for the handoff window.
//!
//! ## The contract
//!
//! - A reader may poll for readability at any time, gate or no gate
//!   — readiness detection is non-consuming and needs no permit.
//! - A reader may CONSUME bytes from the PTY (`read()`) only while
//!   holding a [`read_permit`], and must hold that same permit until
//!   the bytes are pushed into the fanout ring (or, on EOF/error,
//!   until `fanout.close()` has run). Releasing the permit between
//!   `read` and `push` would reopen R3's loss window.
//! - The permit must NEVER be held while parked in poll/read-waiting
//!   — only around the bounded read+push unit. A reader that held
//!   its permit across a poll wait would block [`freeze`] for as
//!   long as its child stays quiet, i.e. indefinitely.
//! - [`freeze`] therefore guarantees: while the returned guard
//!   lives, **zero bytes are in flight on any reader stack**. Every
//!   byte the children have produced is either already pushed to a
//!   fanout ring (persisted implicitly via transcripts; the ring
//!   itself is expendable) or still in the kernel's PTY buffer —
//!   and the kernel buffer survives an `exec` of the daemon image.
//! - Read permits are shared: any number of readers can consume
//!   concurrently in normal operation. The gate costs one
//!   uncontended rwlock acquisition per read+push — noise next to
//!   the `read(2)` syscall itself.
//!
//! ## Buffer-bound consequence (freezes must be SHORT)
//!
//! While a freeze is held, children keep writing into their PTYs but
//! nobody drains them. A kernel PTY buffer holds on the order of
//! 64 KiB; a chatty child fills it and then **blocks in `write(2)`**
//! until the post-freeze reader resumes draining. That is a stall,
//! never a loss — but it means the coordinator must hold the freeze
//! only for the bounded handoff window and the readers must resume
//! promptly on release (they do: each is parked in a finite-timeout
//! poll or blocked right at `read_permit()`, and the rwlock wakes
//! them on thaw).
//!
//! ## Lock ordering (important for the phase-2d coordinator)
//!
//! A reader holds its permit across `fanout.push()`, which takes the
//! fanout's internal mutex and then stamps the shared
//! `last_activity_at` cell. Therefore [`freeze`] must NEVER be
//! called while holding a fanout's inner lock or an activity cell —
//! a reader blocked on either would hold its read permit and
//! deadlock the freeze. (Both locks are short-scoped internals of
//! `session.rs`; no RPC handler holds them across calls, so in
//! practice this only constrains the coordinator itself.) As with
//! the reap gate, a bounded-wait/try variant belongs to the
//! phase-2d quiesce barrier, not here.
//!
//! ## Scope
//!
//! This gate covers the PTY **output** path (reader threads) only.
//! Writer paths (`send_input` etc.) are a separate quiesce concern —
//! R10, phase 2d — and the exit-status path has its own gate
//! ([`crate::reap_gate`]). The two gates are independent by design:
//! freezing bytes must not stall reaping, and vice versa.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The gate. `()` payload — the lock IS the data. `const`-
/// constructible, so plain `static` storage with no lazy-init
/// machinery.
static GATE: RwLock<()> = RwLock::new(());

/// Shared consumption permit held by a reader thread around one
/// `read()`→`fanout.push()` unit (or the terminal
/// `read()`→`fanout.close()`). Dropping it releases the gate.
pub type ReaderPermit = RwLockReadGuard<'static, ()>;

/// Exclusive freeze held by the restart coordinator for the handoff
/// window. While it lives, no reader can pull bytes off a PTY and
/// none are in flight on a reader stack — unpushed bytes live only
/// in kernel PTY buffers, which survive exec. Dropping it thaws the
/// readers.
pub type ReaderFreeze = RwLockWriteGuard<'static, ()>;

/// Acquire a shared read permit. Blocks while a [`freeze`] is held
/// (that block is the entire point — a reader that saw readability
/// parks here until the handoff window closes, and the bytes wait
/// in the kernel's PTY buffer).
///
/// Poisoning is ignored (`into_inner`): the payload is `()`, so a
/// panicking holder leaves nothing inconsistent to protect, and a
/// reader refusing to drain over a poisoned gate would wedge every
/// chatty child at a full PTY buffer forever — strictly worse than
/// proceeding.
pub fn read_permit() -> ReaderPermit {
    GATE.read().unwrap_or_else(|p| p.into_inner())
}

/// Acquire the exclusive freeze. Blocks until every in-flight read
/// permit is released — i.e. until every reader currently mid-
/// read+push has finished pushing. Bounded in practice by the
/// longest single read+push (one ≤8 KiB `read(2)` plus a ring
/// append and non-blocking subscriber sends); the caller (the
/// phase-2d quiesce barrier) is responsible for wrapping this in
/// its own timeout/abort discipline, for keeping the hold SHORT
/// (see the buffer-bound consequence in the module docs), and for
/// the lock-ordering rule above.
pub fn freeze() -> ReaderFreeze {
    GATE.write().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Read permits are shared: two concurrent holders coexist
    /// without blocking each other (readers must never serialize
    /// against each other in normal operation).
    #[test]
    fn read_permits_are_concurrent() {
        let a = read_permit();
        // If read permits excluded each other this would deadlock;
        // acquire on another thread with a timeout-join to turn a
        // regression into a failure instead of a hang.
        let t = std::thread::spawn(|| {
            let _b = read_permit();
        });
        // Generous: suite runs under load.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !t.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            t.is_finished(),
            "second read_permit() blocked while first was held — permits must be shared"
        );
        t.join().unwrap();
        drop(a);
    }

    /// The freeze excludes read permits: a permit requested under a
    /// live freeze is not granted until the freeze drops.
    ///
    /// NOTE ON TIMING: the freeze is held only ~250ms here because
    /// the gate is process-global — a long hold in a unit test
    /// would stall every concurrently-running session test's reader
    /// in this same test binary. The full behavioral proof (fanout
    /// stops growing across multiple reader poll periods, then the
    /// complete byte sequence arrives with no gaps) lives in
    /// `daemon/tests/reader_gate_freeze.rs`, which runs in its own
    /// process.
    #[test]
    fn freeze_blocks_read_permits_until_dropped() {
        let frozen = freeze();

        let acquired = Arc::new(AtomicBool::new(false));
        let acquired_clone = Arc::clone(&acquired);
        let t = std::thread::spawn(move || {
            let _p = read_permit();
            acquired_clone.store(true, Ordering::SeqCst);
        });

        // While frozen: the permit must not be granted. 250ms is
        // plenty for the thread to have reached the read() call;
        // if the OS is slow to even schedule it, "not acquired"
        // still holds, so this direction cannot flake under load.
        std::thread::sleep(Duration::from_millis(250));
        assert!(
            !acquired.load(Ordering::SeqCst),
            "read_permit() was granted while freeze() was held"
        );

        drop(frozen);

        // After release: granted promptly (generous bound — load).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !acquired.load(Ordering::SeqCst)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            acquired.load(Ordering::SeqCst),
            "read_permit() still blocked after freeze was dropped"
        );
        t.join().unwrap();
    }
}
