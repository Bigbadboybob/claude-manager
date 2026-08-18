//! Process-wide PTY-writer gate. Restart hardening /
//! DESIGN_SEAMLESS_RESTART phase 4h (review finding R10, audit gap 6).
//!
//! ## Why this exists
//!
//! The restart barrier's other two halves cover the exit-status path
//! ([`crate::reap_gate`]) and the PTY **output** path
//! ([`crate::reader_gate`]). The PTY **input** path was the last
//! unfenced writer surface (audit gap 6): the coordinator's
//! `wait_quiesced` covers mutating RPC *bodies* via the in-flight
//! mutation counter, but every daemon-side byte that reaches a
//! session's PTY is written OUTSIDE that cover —
//!
//!   - the detached prompt-delivery threads
//!     (`control::methods::spawn_agent_prompt_delivery` /
//!     `spawn_persistent_prompt_delivery`: `start_session`
//!     auto-prompts, agent `send_input`, continuous fires, `/compact`
//!     fires, stuck-run investigator prompts) outlive their dispatch
//!     body by design;
//!   - attach-stream Input frames are written by `handle_connection`'s
//!     long-lived stream loop, which runs AFTER `dispatch_request`
//!     returned its streaming outcome (the P2d scope note);
//!   - the workflow poller's activation drainer
//!     (`workflow::finalize::advance_finalization` + the fresh-reset
//!     `/clear` writes) runs on the poller tick thread.
//!
//! At a re-exec, a thread mid-`write_all` is torn mid-byte-sequence,
//! and a delivery that wrote its prompt body but not its deferred
//! Enter leaves the prompt frozen unsubmitted in the composer —
//! corrupted terminal input no rehydrate can repair (R10). The
//! design's decision was quiescence over journaling: PTY writes are
//! not idempotent, so they must be *fenced*, not replayed.
//!
//! ## The unit model
//!
//! Two permit shapes, matching the two write shapes in the daemon:
//!
//! - [`write_permit`] — one **single-write unit**: exactly one
//!   `write_all`+`flush`. Acquired *inside*
//!   `session::InputHandle::write_and_stamp`, so every production PTY
//!   write is covered structurally — a future input path cannot
//!   forget the gate any more than it can forget the activity stamp.
//!   This is the whole unit for attach-stream input, the bash
//!   `send_input` path, and each phase-write of the workflow
//!   activation drainer (whose *cross*-write recovery is the durable
//!   `PendingActivation.phase` record — see Scope below).
//! - [`unit_permit`] — one **multi-write delivery unit**: the whole
//!   body → gap → Enter sequence of `deliver_agent_body`. Held across
//!   the unit so a freeze can never land between the body and its
//!   Enter. Inside a held unit, nested [`write_permit`] calls are
//!   no-ops (tracked per-thread), so the funnel's internal
//!   acquisition cannot self-deadlock against a pending freeze.
//!
//! A delivery unit is seconds long (the Enter gap alone is 1.5s), so
//! unlike the twins' microsecond consume/push units, a blocking
//! freeze against a leisurely unit would stall the restart. The gate
//! therefore adds a **pause request** ([`request_pause`]): delivery
//! units check [`pause_requested`] at each step boundary and finish
//! PROMPTLY — after the body, prefer completing the Enter on a short
//! floor over parking mid-prompt; after the Enter, skip the
//! best-effort verify windows. The documented trade: **a quiesce may
//! wait up to one in-progress delivery step (~3s worst case), never
//! tear a unit.**
//!
//! ## The freeze protocol (what [`perform_reexec`] does)
//!
//! 1. `wait_quiesced` first: in-flight mutating RPCs (which include
//!    the synchronous bash-input write) drain to zero.
//! 2. [`request_pause`]: new units park at the door (see below), and
//!    in-flight delivery units wrap up promptly at their next step
//!    boundary.
//! 3. [`freeze`]: poll `try_write` until every in-flight permit is
//!    released, bounded by a caller timeout — a wedged `write_all`
//!    (a child that stopped draining its stdin fills the ~64KiB
//!    kernel input buffer and blocks the writer indefinitely) must
//!    ABORT the restart with `restart_busy`, never hang it. The
//!    freeze takes `&PauseGuard` by parameter: polling `try_write`
//!    does not queue as a writer, so WITHOUT the door a steady stream
//!    of fresh permits could starve the poll forever.
//!
//! While the freeze lives, **no thread is between the first and last
//! byte of any delivery unit**: every unit either fully landed or
//! never started. [`write_permit`]/[`unit_permit`] callers arriving
//! under the freeze park (door or read-acquisition) and either
//! proceed after an abort or die with the old image at exec —
//! never-started, never torn. Whether a never-started delivery is
//! retried is its caller's contract: workflow activations re-drive
//! from the durable `PendingActivation.phase` record (the poller
//! re-drives them post-swap), while plain `send_input` auto-prompts
//! and continuous fires lean on their existing monitor/notify
//! timeout handling — the gate deliberately does NOT invent a
//! redelivery journal (PTY writes are not idempotent).
//!
//! ## Lock ordering / deadlock-freedom
//!
//! - A permit holder may take: the session's writer mutex, activity
//!   cells, fanout subscriptions (bounded `recv_timeout`), and — on
//!   the poller path — workflow-run flocks *around but never across*
//!   its writes. A permit holder must NEVER acquire the reap gate,
//!   the reader gate, or (transitively) block on the `DaemonState`
//!   mutex while the restart sequence could hold it: the established
//!   `InputHandle` contract (clone the handle out under the state
//!   lock, DROP the lock, then write) is now load-bearing for restart
//!   deadlock-freedom, not just for RPC latency.
//! - The three gates' holder sets are pairwise disjoint (reapers take
//!   only reap permits, readers only reader permits, writers only
//!   writer permits; `handle_session_exit` — the reaper's `on_exit`
//!   body — performs no PTY writes), so the freeze acquisition order
//!   among them is arbitrary and safe. `perform_reexec` takes the
//!   writer freeze FIRST because it is the slow one (up to one
//!   delivery step), keeping the reader-freeze hold — whose duration
//!   bounds how long chatty children can block on full PTY output
//!   buffers — as short as ever.
//! - The door mutex and the pause counter are leaf locks: never held
//!   while acquiring `GATE`, the state mutex, or a writer mutex.
//!
//! ## Scope
//!
//! Covers the PTY **input** byte path only. Exempt by design:
//! - `DaemonSession::resize` — TIOCSWINSZ is an atomic ioctl, not a
//!   tearable byte stream.
//! - `DaemonSession::send_input` — the documented test-only
//!   convenience (all production writes route through `InputHandle`).
//! - manifest.watch / attach-stream *outbound* frames — daemon→client
//!   socket writes, not PTY input; connections drop at exec by
//!   contract.
//! - The workflow drainer's *between*-write windows (body persisted
//!   as `BodySent`, Enter deadline pending): covered by the durable
//!   phase record, not by holding a permit across poller ticks —
//!   pinned by `finalize::tests::restart_at_body_sent_resumes_without_duplicate_body`
//!   and `activation_cut_before_first_byte_redrives_body_from_persisted_phase`.

use std::cell::Cell;
use std::fmt;
use std::sync::{Condvar, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

/// The gate. `()` payload — the lock IS the data. `const`-
/// constructible, so plain `static` storage with no lazy-init
/// machinery (the twins' idiom).
static GATE: RwLock<()> = RwLock::new(());

/// The door: parks NEW units while a pause is in effect so the
/// freeze's `try_write` poll cannot be starved by fresh permits.
/// `usize` because tests (and, defensively, overlapping restart
/// attempts) may nest pause requests.
static DOOR: Mutex<usize> = Mutex::new(0);
static DOOR_CV: Condvar = Condvar::new();

thread_local! {
    /// Depth of unit permits held by THIS thread. Nested
    /// [`write_permit`] calls inside a held unit are no-ops — the
    /// unit's own read guard already covers them, and a second
    /// `GATE.read()` on the same thread could deadlock against a
    /// queued writer on a write-preferring rwlock.
    static UNIT_DEPTH: Cell<u32> = const { Cell::new(0) };
}

fn door_paused() -> bool {
    *DOOR.lock().unwrap_or_else(|p| p.into_inner()) > 0
}

/// Block while a pause is in effect. Leaf lock: released before any
/// other acquisition. A thread parked here has written NOTHING of its
/// unit — on abort it proceeds and delivers; at exec it dies with the
/// old image (never-started, the caller's retry contract applies).
fn await_door() {
    let mut n = DOOR.lock().unwrap_or_else(|p| p.into_inner());
    while *n > 0 {
        n = DOOR_CV.wait(n).unwrap_or_else(|p| p.into_inner());
    }
}

fn read_gate() -> RwLockReadGuard<'static, ()> {
    // Poisoning ignored (`into_inner`), same rationale as the twins:
    // the payload is `()`, so a panicking holder leaves nothing
    // inconsistent, and refusing to write over a poisoned gate would
    // wedge every prompt delivery forever — strictly worse.
    GATE.read().unwrap_or_else(|p| p.into_inner())
}

/// Is a restart pause in effect? Delivery units poll this at each
/// step boundary (post-body, post-gap, each verify window) and finish
/// promptly when set — see the module docs' unit model.
pub fn pause_requested() -> bool {
    door_paused()
}

/// RAII pause request. While any guard lives, [`pause_requested`] is
/// true and NEW units park at the door; in-flight units are asked (by
/// their own step-boundary polls) to wrap up promptly. Dropping the
/// last guard reopens the door and wakes every parked unit — the
/// abort path's "indistinguishable from before" restore.
pub struct PauseGuard {
    _private: (),
}

/// Request the pause. Taken by `perform_reexec` AFTER `wait_quiesced`
/// succeeds (taking it before would deadlock-shape the barrier: an
/// in-flight mutating dispatch parked at the door could never return,
/// so the counter could never reach zero) and held across the freeze
/// and the exec attempt.
pub fn request_pause() -> PauseGuard {
    let mut n = DOOR.lock().unwrap_or_else(|p| p.into_inner());
    *n += 1;
    PauseGuard { _private: () }
}

impl Drop for PauseGuard {
    fn drop(&mut self) {
        let mut n = DOOR.lock().unwrap_or_else(|p| p.into_inner());
        *n = n.saturating_sub(1);
        if *n == 0 {
            DOOR_CV.notify_all();
        }
    }
}

/// Shared permit for one SINGLE-WRITE unit (one `write_all`+`flush`).
/// Acquired inside `InputHandle::write_and_stamp`; a no-op when the
/// calling thread already holds a [`unit_permit`] (see `UNIT_DEPTH`).
pub struct WritePermit {
    _guard: Option<RwLockReadGuard<'static, ()>>,
}

/// Acquire a single-write permit. Parks at the door while a pause is
/// requested, then blocks while a [`freeze`] is held (that block is
/// the point — the write waits out the handoff window, then either
/// proceeds post-abort or dies with the old image at exec).
pub fn write_permit() -> WritePermit {
    if UNIT_DEPTH.with(|d| d.get()) > 0 {
        return WritePermit { _guard: None };
    }
    await_door();
    WritePermit {
        _guard: Some(read_gate()),
    }
}

/// Shared permit for one MULTI-WRITE delivery unit (body → gap →
/// Enter). Held by the delivery thread across the whole unit; nested
/// [`write_permit`] acquisitions on the same thread become no-ops.
pub struct WriterUnitPermit {
    _guard: Option<RwLockReadGuard<'static, ()>>,
}

/// Acquire a delivery-unit permit. Same door/freeze semantics as
/// [`write_permit`]; nested acquisition on a thread already inside a
/// unit is a no-op (the outer unit's guard covers it).
///
/// The holder's obligation: poll [`pause_requested`] at every step
/// boundary and finish the unit promptly under a pause — never park
/// between the first and last byte (see `deliver_agent_body`).
pub fn unit_permit() -> WriterUnitPermit {
    if UNIT_DEPTH.with(|d| d.get()) > 0 {
        UNIT_DEPTH.with(|d| d.set(d.get() + 1));
        return WriterUnitPermit { _guard: None };
    }
    await_door();
    let guard = read_gate();
    UNIT_DEPTH.with(|d| d.set(d.get() + 1));
    WriterUnitPermit {
        _guard: Some(guard),
    }
}

impl Drop for WriterUnitPermit {
    fn drop(&mut self) {
        UNIT_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Exclusive freeze held by the restart coordinator for the handoff
/// window. While it lives, no thread is between the first and last
/// byte of any PTY-input unit. Dropping it thaws the writers (abort
/// path; the commit path is the exec, which never returns).
#[derive(Debug)]
pub struct WriterFreeze {
    _guard: RwLockWriteGuard<'static, ()>,
}

/// [`freeze`] failure: some writer unit did not finish within the
/// timeout — most plausibly a `write_all` wedged against a full PTY
/// input buffer (child stopped draining stdin). Renders into the
/// design's `restart_busy` abort class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterFreezeTimedOut {
    pub waited: Duration,
}

impl fmt::Display for WriterFreezeTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "restart_busy: PTY writer gate not quiesced after {:?} — a \
             prompt-delivery or input write is still in flight (possibly \
             wedged against a full PTY input buffer); the restart aborts \
             rather than tear a delivery unit (R10)",
            self.waited,
        )
    }
}

/// Poll interval for the freeze's `try_write` loop. Coarse enough to
/// be free, fine enough that an uncontended freeze lands promptly.
const FREEZE_POLL: Duration = Duration::from_millis(5);

/// The gate + door are process-global; tests (here AND in other
/// modules — the delivery-shape tests in `control::methods`, the
/// `perform_reexec` wiring test in `reexec`) that hold a pause or a
/// freeze, or assert their ABSENCE, serialize on this so they can't
/// race each other. Holds are kept short so concurrently-running
/// session tests in this binary are only briefly delayed — the
/// twins' idiom.
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

/// Acquire the exclusive freeze, bounded by `timeout`.
///
/// Requires the caller's live [`PauseGuard`] by reference: the door
/// is what guarantees the `try_write` poll converges (new units park;
/// in-flight units — pause-aware by contract — finish within one
/// step window, ~3s worst case). Unlike the twins' blocking
/// `freeze()`, this one is bounded, because a writer unit is NOT
/// structurally bounded: a child that stopped reading its stdin
/// blocks `write_all` indefinitely, and the design's failure-class
/// contract demands `restart_busy` abort-not-hang.
pub fn freeze(
    _pause: &PauseGuard,
    timeout: Duration,
) -> Result<WriterFreeze, WriterFreezeTimedOut> {
    let start = Instant::now();
    loop {
        match GATE.try_write() {
            Ok(g) => return Ok(WriterFreeze { _guard: g }),
            Err(TryLockError::Poisoned(p)) => {
                return Ok(WriterFreeze {
                    _guard: p.into_inner(),
                })
            }
            Err(TryLockError::WouldBlock) => {}
        }
        let waited = start.elapsed();
        if waited >= timeout {
            return Err(WriterFreezeTimedOut { waited });
        }
        std::thread::sleep(FREEZE_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Permits are shared: concurrent units/writes never serialize
    /// against each other in normal operation.
    #[test]
    fn permits_are_concurrent() {
        let _s = serial();
        let a = unit_permit();
        let t = std::thread::spawn(|| {
            let _b = write_permit();
            let _c = unit_permit();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            t.is_finished(),
            "second permit blocked while first was held — permits must be shared"
        );
        t.join().unwrap();
        drop(a);
    }

    /// Deliverable: the freeze BLOCKS while a unit is in flight and
    /// is admitted once the unit finishes. (The permit guard is
    /// `!Send`, so the in-flight unit lives on its own thread and is
    /// released by flag.)
    #[test]
    fn freeze_waits_for_in_flight_unit_then_admits() {
        let _s = serial();
        let held = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let held_c = Arc::clone(&held);
        let release_c = Arc::clone(&release);
        let t = std::thread::spawn(move || {
            let _unit = unit_permit();
            held_c.store(true, Ordering::SeqCst);
            while !release_c.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            // `_unit` drops here — the unit finishes.
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !held.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(held.load(Ordering::SeqCst), "unit thread never started");

        let pause = request_pause();
        // With the unit held, a short-bounded freeze must time out.
        let err = freeze(&pause, Duration::from_millis(100))
            .expect_err("freeze must not be granted over an in-flight unit");
        assert!(err.to_string().contains("restart_busy"), "{}", err);

        // Unit finishes; a generous freeze then lands.
        release.store(true, Ordering::SeqCst);
        let frozen = freeze(&pause, Duration::from_secs(10))
            .expect("freeze admitted once the unit dropped");
        t.join().unwrap();
        drop(frozen);
        drop(pause);
    }

    /// Deliverable: the freeze EXCLUDES new units — a unit requested
    /// under a live freeze is not granted until the freeze drops.
    #[test]
    fn freeze_excludes_new_units_until_dropped() {
        let _s = serial();
        let pause = request_pause();
        let frozen = freeze(&pause, Duration::from_secs(5)).expect("uncontended freeze");

        let acquired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&acquired);
        let t = std::thread::spawn(move || {
            let _p = unit_permit();
            flag.store(true, Ordering::SeqCst);
        });

        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !acquired.load(Ordering::SeqCst),
            "unit_permit() granted while the freeze (and pause door) was held"
        );

        drop(frozen);
        drop(pause); // reopen the door — the abort path's restore
        let deadline = Instant::now() + Duration::from_secs(5);
        while !acquired.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            acquired.load(Ordering::SeqCst),
            "unit_permit() still blocked after freeze + pause dropped"
        );
        t.join().unwrap();
    }

    /// The pause door alone (no freeze yet — the acquisition window)
    /// parks new units, and dropping the pause releases them: the
    /// starvation guard for the freeze's try_write poll, and the
    /// abort path's restore.
    #[test]
    fn pause_parks_new_units_at_the_door() {
        let _s = serial();
        let pause = request_pause();
        assert!(pause_requested());

        let acquired = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&acquired);
        let t = std::thread::spawn(move || {
            let _p = write_permit();
            flag.store(true, Ordering::SeqCst);
        });
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            !acquired.load(Ordering::SeqCst),
            "write_permit() passed the door while a pause was requested"
        );

        drop(pause);
        assert!(!pause_requested());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !acquired.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(acquired.load(Ordering::SeqCst), "door never reopened");
        t.join().unwrap();
    }

    /// The reentrancy contract: a thread inside a held unit gets a
    /// NO-OP write_permit — even with a pause requested and a freeze
    /// pending, the nested acquisition must neither park at the door
    /// nor queue behind the pending writer (the self-deadlock this
    /// design exists to rule out: `deliver_agent_body` holds the unit
    /// and every one of its writes goes through `write_and_stamp`,
    /// which acquires internally).
    #[test]
    fn nested_write_permit_inside_unit_is_noop_under_pending_freeze() {
        let _s = serial();
        let unit = unit_permit();
        let pause = request_pause();

        // A freeze attempt is pending on another thread (it will time
        // out — the unit is held for the whole test).
        let freezer = std::thread::spawn(move || {
            let err = freeze(&pause, Duration::from_millis(400))
                .expect_err("unit held for the whole window");
            (pause, err)
        });

        // Nested acquisitions on the unit-holding thread must return
        // immediately despite the door being closed.
        let start = Instant::now();
        let inner_write = write_permit();
        let inner_unit = unit_permit();
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "nested permits must be no-ops inside a held unit"
        );
        drop(inner_unit);
        drop(inner_write);

        let (pause, _err) = freezer.join().unwrap();
        drop(pause);
        drop(unit);
    }

    /// Dropping the outer unit clears the thread-local depth: a LATER
    /// write_permit on the same thread takes a real guard again
    /// (i.e. the no-op path is scoped to the unit, not the thread's
    /// lifetime).
    #[test]
    fn unit_depth_unwinds_on_drop() {
        let _s = serial();
        {
            let _u = unit_permit();
            assert!(UNIT_DEPTH.with(|d| d.get()) == 1);
        }
        assert!(UNIT_DEPTH.with(|d| d.get()) == 0);
        // A real (non-noop) permit is taken now — provable via the
        // freeze being blocked by it.
        let w = write_permit();
        let pause = request_pause();
        let err = freeze(&pause, Duration::from_millis(80))
            .expect_err("post-unit write_permit must hold a real guard");
        assert!(err.waited >= Duration::from_millis(80));
        drop(w);
        drop(pause);
    }
}
