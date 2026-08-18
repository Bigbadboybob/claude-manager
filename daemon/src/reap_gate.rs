//! Process-wide reap gate. Restart hardening /
//! DESIGN_SEAMLESS_RESTART phase 2a (review finding R4).
//!
//! ## Why this exists
//!
//! Pre-2a, each session's reaper thread blocked in `waitpid` and
//! consumed the child's exit status the instant the child died.
//! During a future re-exec handoff window (the seamless-restart
//! design's Quiesce step), a status consumed microseconds before
//! the `exec` is lost forever in the gap between `waitpid` and
//! persistence — the old image consumed the kernel's one copy of
//! the status, then ceased to exist before writing it anywhere, and
//! no reaper in the new image can reconstruct it (the child is no
//! longer a zombie; the kernel has forgotten it).
//!
//! The fix is to split *detecting* exit-readiness from *consuming*
//! the status. Reapers wait for readiness via non-consuming
//! `poll(2)` on the session's pidfd (see
//! `session.rs::poll_pidfd_until_exit_ready`), and only consume —
//! `waitid(P_PIDFD, …, WEXITED)`, or the `waitpid` fallback — while
//! holding this gate's shared **read permit**. The future restart
//! coordinator (phase 2d) takes the exclusive write side
//! ([`freeze`]) for the handoff window: with the freeze held, zero
//! consumptions can happen, so a child dying mid-swap simply stays
//! a zombie that the new image reaps *with full status* after
//! commit.
//!
//! ## The contract
//!
//! - A reaper may detect exit-readiness at any time, gate or no
//!   gate — readiness detection is non-consuming and needs no
//!   permit.
//! - A reaper may CONSUME a wait status (`waitid`/`waitpid`) only
//!   while holding a [`read_permit`], and must hold that same
//!   permit until the consumed status has been handed to its
//!   downstream consumers (exit channel, `LastExitProbe`, the
//!   `on_exit` registry callback). R4's loss window is
//!   *consumed-but-not-yet-persisted*; releasing the permit between
//!   `waitid` and the downstream writes would reopen it.
//! - [`freeze`] therefore guarantees: while the returned guard
//!   lives, no exit status is consumed AND no previously-consumed
//!   status is still in flight on a reaper stack. Children that die
//!   under a freeze park as kernel zombies — fully reconstructible
//!   later.
//! - Read permits are shared: any number of reapers can consume
//!   concurrently in normal operation. The gate costs one
//!   uncontended rwlock acquisition per session exit — noise.
//!
//! ## Lock ordering (important for the phase-2d coordinator)
//!
//! A reaper holds its read permit ACROSS the `on_exit` callback,
//! which takes the daemon state lock (registry removal, tombstone
//! persistence). Therefore [`freeze`] must NEVER be called while
//! holding the daemon state lock or anything else an `on_exit`
//! callback can acquire — a reaper blocked on that lock would hold
//! its read permit forever and deadlock the freeze. The restart
//! coordinator's quiesce barrier must take the freeze from a
//! lock-free context, with its own bounded wait (the design's
//! `restart_busy` abort); a bounded-wait/try variant belongs to
//! that phase, not here.
//!
//! ## Scope
//!
//! Exempt by design: the pre-arm spawn-failure cleanup paths
//! (`PendingSession::Drop`, `arm_reaper`'s thread-spawn error path)
//! stay on direct kill-then-`waitpid`. They only run while a spawn
//! RPC is in flight, and the restart barrier's in-flight-mutation
//! counter (phase 2d) excludes such windows from handoffs — see the
//! comments at those call sites.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

/// The gate. `()` payload — the lock IS the data. `const`-
/// constructible, so plain `static` storage with no lazy-init
/// machinery.
static GATE: RwLock<()> = RwLock::new(());

/// Shared consumption permit held by a reaper around wait-status
/// consumption *and* the downstream persistence of the consumed
/// status. Dropping it releases the gate.
pub type ReapPermit = RwLockReadGuard<'static, ()>;

/// Exclusive freeze held by the restart coordinator for the
/// handoff window. While it lives, no reaper can consume an exit
/// status and none is in flight. Dropping it thaws the reapers.
pub type ReapFreeze = RwLockWriteGuard<'static, ()>;

/// Acquire a shared read permit. Blocks while a [`freeze`] is held
/// (that block is the entire point — a reaper that detected
/// exit-readiness parks here until the handoff window closes, and
/// its child waits as a zombie).
///
/// Poisoning is ignored (`into_inner`): the payload is `()`, so a
/// panicking holder leaves nothing inconsistent to protect, and a
/// reaper refusing to reap over a poisoned gate would leak zombies
/// forever — strictly worse than proceeding.
pub fn read_permit() -> ReapPermit {
    GATE.read().unwrap_or_else(|p| p.into_inner())
}

/// Acquire the exclusive freeze. Blocks until every in-flight read
/// permit is released — i.e. until every reaper currently mid-
/// consumption has finished persisting its status. Bounded in
/// practice by the longest single consume-and-persist sequence;
/// the caller (the phase-2d quiesce barrier) is responsible for
/// wrapping this in its own timeout/abort discipline and for the
/// lock-ordering rule in the module docs.
pub fn freeze() -> ReapFreeze {
    GATE.write().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    /// Read permits are shared: two concurrent holders coexist
    /// without blocking each other (reapers must never serialize
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
    /// would stall every concurrently-running session test's reaper
    /// in this same test binary. The full behavioral proof (no exit
    /// delivered on a session's exit channel across multiple reaper
    /// poll periods) lives in `daemon/tests/reap_gate_freeze.rs`,
    /// which runs in its own process.
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
