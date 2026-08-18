//! Restart coordinator + quiescence barrier — the prepare/commit/abort
//! skeleton for the in-place re-exec restart.
//!
//! DESIGN_SEAMLESS_RESTART phase 2d (review findings R2, R10). This slice
//! is one of the "ordinary, individually-testable daemon changes" that
//! land BEFORE any exec/handoff code: there is deliberately **no
//! `daemon.restart` RPC, no FD manifest, and no exec anywhere near this
//! module**. What it provides:
//!
//!   1. An **in-flight mutation counter** (`enter_mutation` /
//!      [`MutationGuard`]) that `dispatch_request` holds around every
//!      mutating RPC body. The existing drain flag
//!      (`DaemonState::draining`, H3) only refuses NEW spawn-shaped work
//!      at the front door; it cannot stop work that already passed the
//!      check — a spawn mid-flight (child forked, not yet in the
//!      registry) when a future exec fires would miss the FD manifest
//!      and be orphaned alive (R2). The counter is what lets a restart
//!      wait for that in-flight work to finish.
//!   2. A **single restart latch** ([`begin`] → [`QuiesceGuard`]): at
//!      most one restart attempt is in flight; a second `begin` while
//!      one is active refuses with [`RestartBusy`]
//!      (`restart_in_progress`). `begin` closes the front door
//!      (`draining = true`) and marks the attempt
//!      (`DaemonState::restarting = true`, surfaced on `daemon.health`).
//!   3. [`QuiesceGuard::wait_quiesced`] — block, WITHOUT holding the
//!      state mutex (in-flight mutations need it to finish), until the
//!      mutation counter reaches zero and every registered safe-point
//!      subsystem has acknowledged its pause. Bounded by a timeout: a
//!      barrier that can't be reached (wedged writer, stuck delivery)
//!      must abort the restart, not hang the daemon
//!      (DESIGN_SEAMLESS_RESTART "Failure classes" → `restart_busy`).
//!   4. **Abort restores everything.** [`QuiesceGuard::abort`] / `Drop`
//!      un-drains (to the pre-`begin` value, so an operator's manual
//!      `daemon.drain` is preserved), clears `restarting`, and resumes
//!      every parked subsystem — after an abort the daemon is
//!      indistinguishable from before `begin()`.
//!   5. A **safe-point registration surface**
//!      ([`RestartCoordinator::register_subsystem`] →
//!      [`SafePointHandle`]) that later phase-2 slices plug their
//!      long-running loops into. Nothing registers yet; the protocol is
//!      implemented (and tested) so the plugs are drop-in:
//!        - **Delivery writers / memory-cap watchers (R10, R12)**: park
//!          between writes / at a no-signal point.
//!
//!      The **reapers (2a, R4)** and **PTY readers (2b, R3)** do NOT
//!      register here: they quiesce through their own exclusive gates
//!      (`crate::reap_gate::freeze()` and the reader gate's twin),
//!      which the restart sequence takes AFTER `wait_quiesced`
//!      succeeds. A gate freeze is stronger than handle-parking — it
//!      also excludes a holder already mid-consumption/mid-push, with
//!      no per-thread registration — and it carries its own
//!      lock-ordering rule: never take a freeze while holding the
//!      state mutex (a reaper holds its permit across `on_exit`,
//!      which takes that lock; see `crate::reap_gate`).
//!
//! ## What is deliberately OUT of scope here
//!
//! **PTY input flowing over attach streams** is not gated by the
//! mutation counter: the counter covers only the `dispatch_request`
//! body, and attach connections leave dispatch before their long-lived
//! stream loop starts (see the guard-release note in
//! `control/dispatch.rs`). Raw PTY writes arriving over an established
//! attach stream are handled by the writer safe points of a later slice
//! (R10) — they will register through the same [`SafePointHandle`]
//! surface above.
//!
//! ## How this composes with `daemon.drain` (H3)
//!
//! Drain remains the operator's manual seam, unchanged. `begin` sets the
//! same `draining` flag directly (the refuse gate keys off the flag, not
//! the RPC) but does NOT reuse `methods::daemon_drain`'s inbox-notice
//! path: the drain notice tells agents "this session will be killed and
//! resumed", which is exactly wrong for a seamless restart. Messaging
//! for the restart path is a later slice's decision.
//!
//! The barrier's guarantee, precisely: after `begin` (draining set under
//! the state lock) and a successful `wait_quiesced` (counter observed
//! zero), every mutating RPC that entered dispatch before the drain flip
//! has fully returned, and every one that entered after will observe
//! `draining == true` when its body takes the state lock — so
//! spawn-shaped work has either completed or will be refused. Mutations
//! that drain does NOT gate (`send_input`, workflow transitions, …) can
//! still start after the counter's zero-crossing; fencing those is what
//! the writer safe points and the commit step of later slices are for.
//! This slice's counter is the R2 fix (no half-spawned children), not
//! the whole freeze.
//!
//! ## Lock ordering
//!
//! Four locks appear in this module's paths. The global order is:
//!
//!   `DaemonState` mutex → `counts` → `subsystems` (registry) →
//!   per-subsystem `flags`
//!
//! - `dispatch_request` holds the state mutex briefly while taking
//!   `counts` (guard acquisition).
//! - `wait_quiesced` holds `counts` while reading `subsystems` +
//!   `flags`; it NEVER touches the state mutex.
//! - A subsystem's pause-ack sets its own `flags`, releases, then takes
//!   `counts` (empty critical section) before notifying — the classic
//!   no-lost-wakeup bracket against a waiter that just checked the
//!   condition and is about to sleep.
//! - `begin`/abort take the state mutex WITHOUT holding any module lock.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::state::DaemonState;

// ============================================================
// Mutation counter + safe-point registry
// ============================================================

/// In-flight mutation counts. Behind one mutex so a waiter can block on
/// the paired condvar. `entered_total` is a cumulative probe: it lets a
/// test (or a future diagnostic) prove a guard WAS taken around a
/// dispatch that has already returned, which the transient `in_flight`
/// count cannot show.
#[derive(Default)]
struct MutationCounts {
    in_flight: u64,
    entered_total: u64,
}

/// Registered safe-point subsystems. `pause_requested` is carried on
/// the registry (not only per-handle) so a subsystem that registers
/// MID-quiesce — e.g. a workflow fresh-context respawn's reader, which
/// drain deliberately does not gate — inherits the in-effect pause
/// request instead of running through the barrier unfenced.
struct SubsystemRegistry {
    pause_requested: bool,
    entries: Vec<Weak<SafePointShared>>,
}

/// The coordinator: mutation counter + safe-point registry + the
/// condvar a quiesce waiter blocks on. One per daemon process, owned by
/// `DaemonState::restart_coordinator` (Arc'd so `dispatch_request` can
/// take a guard without holding the state mutex for the method body's
/// duration, and so subsystem handles can outlive any one lock scope).
pub struct RestartCoordinator {
    counts: Mutex<MutationCounts>,
    /// Notified when `in_flight` drops to zero and when a subsystem
    /// acknowledges a pause — the two condition edges `wait_quiesced`
    /// sleeps on.
    quiesce_cv: Condvar,
    subsystems: Mutex<SubsystemRegistry>,
}

impl Default for RestartCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl RestartCoordinator {
    pub fn new() -> Self {
        Self {
            counts: Mutex::new(MutationCounts::default()),
            quiesce_cv: Condvar::new(),
            subsystems: Mutex::new(SubsystemRegistry {
                pause_requested: false,
                entries: Vec::new(),
            }),
        }
    }

    /// Take the in-flight mutation guard. Called by `dispatch_request`
    /// for every method not on the read-only allowlist / coordinator
    /// exemption list (see `control/dispatch.rs`), BEFORE the method
    /// body runs. Associated-fn shape (not a method) because the guard
    /// must hold its own `Arc` to the coordinator and `self: &Arc<Self>`
    /// receivers aren't stable.
    pub fn enter_mutation(this: &Arc<Self>) -> MutationGuard {
        {
            let mut c = this.counts.lock().unwrap_or_else(|p| p.into_inner());
            c.in_flight += 1;
            c.entered_total += 1;
        }
        MutationGuard {
            coordinator: Arc::clone(this),
        }
    }

    /// Current number of mutating dispatches in flight. Diagnostic /
    /// test probe; the barrier itself waits via the condvar.
    pub fn mutations_in_flight(&self) -> u64 {
        self.counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .in_flight
    }

    /// Cumulative count of mutation guards ever taken. See
    /// [`MutationCounts::entered_total`].
    pub fn mutations_entered_total(&self) -> u64 {
        self.counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entered_total
    }

    /// Register a long-running subsystem (reaper, PTY reader, delivery
    /// writer, memory-cap watcher) with the quiescence barrier. The
    /// returned handle is held by the subsystem's own thread, which
    /// polls [`SafePointHandle::pause_requested`] (or just calls
    /// [`SafePointHandle::park_if_requested`]) at each of its safe
    /// points. Dropping the handle unregisters (the registry holds
    /// `Weak` refs and prunes dead ones), so a subsystem that exits
    /// mid-quiesce can never wedge the barrier.
    ///
    /// If a pause request is already in effect (registration raced an
    /// active quiesce), the new handle is born with the request set —
    /// see [`SubsystemRegistry::pause_requested`].
    ///
    /// NOTE: nothing registers yet in this slice. The intended callers
    /// are the delivery writers and memory-cap watchers (R10, R12).
    /// The reapers (2a) and PTY readers (2b) deliberately do NOT
    /// register — they quiesce via their own exclusive gate freezes,
    /// taken by the restart sequence after `wait_quiesced`; see the
    /// module doc and `crate::reap_gate`.
    pub fn register_subsystem(this: &Arc<Self>, name: &str) -> SafePointHandle {
        let mut reg = this.subsystems.lock().unwrap_or_else(|p| p.into_inner());
        let shared = Arc::new(SafePointShared {
            name: name.to_string(),
            flags: Mutex::new(SafePointFlags {
                pause_requested: reg.pause_requested,
                paused: false,
            }),
            resume_cv: Condvar::new(),
            coordinator: Arc::downgrade(this),
        });
        reg.entries.push(Arc::downgrade(&shared));
        SafePointHandle { shared }
    }

    /// Ask every registered subsystem to pause at its next safe point.
    fn request_pause_all(&self) {
        let mut reg = self.subsystems.lock().unwrap_or_else(|p| p.into_inner());
        reg.pause_requested = true;
        reg.entries.retain(|w| {
            if let Some(shared) = w.upgrade() {
                shared
                    .flags
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .pause_requested = true;
                true
            } else {
                false
            }
        });
    }

    /// Clear the pause request and wake every parked subsystem. Abort
    /// path; a future commit path never resumes (the new image restarts
    /// subsystems from scratch).
    fn resume_all(&self) {
        let mut reg = self.subsystems.lock().unwrap_or_else(|p| p.into_inner());
        reg.pause_requested = false;
        reg.entries.retain(|w| {
            if let Some(shared) = w.upgrade() {
                shared
                    .flags
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .pause_requested = false;
                shared.resume_cv.notify_all();
                true
            } else {
                false
            }
        });
    }

    /// Names of registered subsystems that have not yet acknowledged
    /// their pause. Prunes dead registrations. Empty = the subsystem
    /// half of the barrier is quiesced (vacuously true while nothing
    /// registers).
    fn unpaused_subsystems(&self) -> Vec<String> {
        let mut reg = self.subsystems.lock().unwrap_or_else(|p| p.into_inner());
        let mut unpaused = Vec::new();
        reg.entries.retain(|w| {
            if let Some(shared) = w.upgrade() {
                let flags = shared.flags.lock().unwrap_or_else(|p| p.into_inner());
                if !flags.paused {
                    unpaused.push(shared.name.clone());
                }
                true
            } else {
                false
            }
        });
        unpaused
    }

    /// No-lost-wakeup notify: take the waiter's mutex (empty critical
    /// section) between the caller's condition mutation and the notify,
    /// so a waiter that just evaluated the condition false is
    /// guaranteed to be inside `wait_timeout` (and thus woken) rather
    /// than between check and sleep (and thus missed).
    fn notify_progress(&self) {
        drop(self.counts.lock().unwrap_or_else(|p| p.into_inner()));
        self.quiesce_cv.notify_all();
    }
}

/// RAII guard for one in-flight mutating dispatch. Drop decrements the
/// counter and, on the zero-crossing, wakes any quiesce waiter.
///
/// Held as a local in `dispatch_request`, so it releases when dispatch
/// RETURNS — including the streaming outcomes (`attach.open`,
/// `manifest.watch`, `events.subscribe`), whose long-lived stream loops
/// run in `handle_connection` AFTER dispatch returned. Without that
/// property a single attached TUI would pin the counter above zero
/// forever.
pub struct MutationGuard {
    coordinator: Arc<RestartCoordinator>,
}

impl Drop for MutationGuard {
    fn drop(&mut self) {
        let hit_zero = {
            let mut c = self
                .coordinator
                .counts
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            c.in_flight = c.in_flight.saturating_sub(1);
            c.in_flight == 0
        };
        if hit_zero {
            self.coordinator.quiesce_cv.notify_all();
        }
    }
}

// ============================================================
// Safe-point handles (subsystem side)
// ============================================================

#[derive(Default)]
struct SafePointFlags {
    /// Set by the coordinator (`request_pause_all`), cleared by
    /// `resume_all`.
    pause_requested: bool,
    /// Set by the subsystem when it parks at a safe point; the
    /// coordinator's quiesced condition requires it on every
    /// registered handle.
    paused: bool,
}

struct SafePointShared {
    name: String,
    flags: Mutex<SafePointFlags>,
    /// Wakes a parked subsystem on resume.
    resume_cv: Condvar,
    /// Back-ref for the pause-ack notify. `Weak`: a leaked handle must
    /// not keep the coordinator (and through it the notify machinery)
    /// alive past state teardown; if the coordinator is gone, nobody is
    /// waiting and the notify is skipped.
    coordinator: Weak<RestartCoordinator>,
}

/// Subsystem-side handle to one registered safe point. Held by the
/// subsystem's thread; see [`RestartCoordinator::register_subsystem`]
/// for the registration story and the intended first callers.
pub struct SafePointHandle {
    shared: Arc<SafePointShared>,
}

impl SafePointHandle {
    pub fn name(&self) -> &str {
        &self.shared.name
    }

    /// Is a pause currently requested? Cheap check for subsystem loops
    /// that want to branch before reaching their park call.
    pub fn pause_requested(&self) -> bool {
        self.shared
            .flags
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .pause_requested
    }

    /// The safe-point primitive: if a pause is requested, acknowledge
    /// it (mark `paused`, wake the quiesce waiter) and BLOCK until the
    /// coordinator resumes. Returns `true` if it parked. Call this at
    /// every point where the subsystem holds no half-done work — e.g.
    /// the reaper between `waitpid`-status consumptions, a PTY reader
    /// after a completed `fanout.push`.
    pub fn park_if_requested(&self) -> bool {
        {
            let flags = self.shared.flags.lock().unwrap_or_else(|p| p.into_inner());
            if !flags.pause_requested {
                return false;
            }
        }
        self.park_until_resumed();
        true
    }

    /// Unconditional park: acknowledge the pause and block until
    /// resumed. If no pause is in effect this returns immediately (the
    /// ack is still made visible and retracted, which is harmless).
    pub fn park_until_resumed(&self) {
        {
            let mut flags = self.shared.flags.lock().unwrap_or_else(|p| p.into_inner());
            flags.paused = true;
        }
        // Ack notify AFTER releasing our flags lock; `notify_progress`
        // takes the waiter's mutex to close the check-then-sleep race.
        // (Lock order: never hold `flags` while taking `counts`.)
        if let Some(coordinator) = self.shared.coordinator.upgrade() {
            coordinator.notify_progress();
        }
        let mut flags = self.shared.flags.lock().unwrap_or_else(|p| p.into_inner());
        while flags.pause_requested {
            flags = self
                .shared
                .resume_cv
                .wait(flags)
                .unwrap_or_else(|p| p.into_inner());
        }
        flags.paused = false;
    }
}

// ============================================================
// The restart lifecycle latch
// ============================================================

/// `begin` refusal: a restart attempt is already in flight. Renders as
/// the wire-facing `restart_in_progress` the design doc names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartBusy;

impl fmt::Display for RestartBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "restart_in_progress: a restart attempt is already in flight")
    }
}

/// `wait_quiesced` failure: the barrier was not reached within the
/// timeout. Carries what was still holding it so the abort can report
/// inline (`restart_busy`, DESIGN_SEAMLESS_RESTART "Failure classes")
/// instead of leaving the operator to guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuiesceTimedOut {
    /// Mutating dispatches still in flight at the deadline.
    pub mutations_in_flight: u64,
    /// Registered subsystems that never acknowledged their pause.
    pub unpaused_subsystems: Vec<String>,
}

impl fmt::Display for QuiesceTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "restart_busy: quiescence not reached ({} mutating RPC(s) in flight; \
             unpaused subsystems: [{}])",
            self.mutations_in_flight,
            self.unpaused_subsystems.join(", "),
        )
    }
}

/// Begin a restart attempt: take the single-restart latch, close the
/// front door, and hand back the guard that owns the attempt.
///
/// Under one state-lock hold: refuse with [`RestartBusy`] if
/// `restarting` is already set, else set `restarting = true` and
/// `draining = true` (snapshotting the prior drain value so an abort
/// can restore an operator-initiated drain exactly). Then request a
/// pause from every registered safe-point subsystem.
///
/// The returned [`QuiesceGuard`] aborts on Drop — callers that decide
/// not to proceed (quiesce timeout, preflight failure in later slices)
/// just drop it and the daemon is exactly as before `begin`.
pub fn begin(state: &Arc<Mutex<DaemonState>>) -> Result<QuiesceGuard, RestartBusy> {
    let (coordinator, prior_draining) = {
        let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
        if st.restarting {
            return Err(RestartBusy);
        }
        let prior = st.draining;
        st.restarting = true;
        st.draining = true;
        (Arc::clone(&st.restart_coordinator), prior)
    };
    // Outside the state lock: subsystem flags have their own ordering
    // (see the module doc) and must never nest inside the state mutex
    // in a path a subsystem's own state-lock use could invert.
    coordinator.request_pause_all();
    Ok(QuiesceGuard {
        state: Arc::clone(state),
        coordinator,
        prior_draining,
    })
}

/// Owner of one in-flight restart attempt. See [`begin`].
pub struct QuiesceGuard {
    state: Arc<Mutex<DaemonState>>,
    coordinator: Arc<RestartCoordinator>,
    /// `DaemonState::draining` as it was before `begin` flipped it, so
    /// abort restores an operator's manual drain rather than silently
    /// cancelling it.
    prior_draining: bool,
}

impl QuiesceGuard {
    /// Block until the barrier is quiesced: zero in-flight mutating
    /// dispatches AND every registered subsystem paused — or the
    /// timeout expires ([`QuiesceTimedOut`]).
    ///
    /// Runs entirely WITHOUT the state mutex: in-flight mutations need
    /// it to finish, so a waiter holding it would deadlock the barrier
    /// against the very work it is waiting out. (The condvar wait
    /// releases the internal `counts` mutex while sleeping, so guard
    /// acquisition never blocks on the waiter either.)
    ///
    /// On timeout the guard is NOT consumed — the caller decides;
    /// dropping it aborts (the expected response, per the design doc's
    /// `restart_busy` failure class).
    pub fn wait_quiesced(&self, timeout: Duration) -> Result<(), QuiesceTimedOut> {
        let deadline = Instant::now() + timeout;
        let coordinator = &self.coordinator;
        let mut counts = coordinator
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        loop {
            // Lock order inside the loop: counts (held) → subsystems →
            // per-subsystem flags. Consistent with every other path;
            // see the module doc.
            let unpaused = coordinator.unpaused_subsystems();
            if counts.in_flight == 0 && unpaused.is_empty() {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(QuiesceTimedOut {
                    mutations_in_flight: counts.in_flight,
                    unpaused_subsystems: unpaused,
                });
            }
            let (guard, _timed_out) = coordinator
                .quiesce_cv
                .wait_timeout(counts, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            counts = guard;
            // Spurious wakeups and partial progress both just re-check.
        }
    }

    /// Abort the attempt explicitly. Identical to dropping the guard;
    /// named so call sites read as intent rather than as a scope quirk.
    pub fn abort(self) {
        // Drop impl does the restore.
    }
}

impl Drop for QuiesceGuard {
    fn drop(&mut self) {
        // Resume first (no state lock held), then restore the flags.
        // After this, the daemon is indistinguishable from before
        // `begin()`: subsystems running, drain back to its prior value,
        // `restarting` clear.
        self.coordinator.resume_all();
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.restarting = false;
        st.draining = self.prior_draining;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn make_state() -> Arc<Mutex<DaemonState>> {
        Arc::new(Mutex::new(DaemonState::new()))
    }

    /// Deliverable (b): `wait_quiesced` blocks while a mutation guard
    /// is held and completes once it drops. Proven in two steps to
    /// avoid an upper-bound timing assertion (flaky under load): a
    /// short-timeout wait against a held guard times out; a generous
    /// wait completes after a helper thread drops the guard.
    #[test]
    fn wait_quiesced_blocks_on_held_mutation_guard_and_completes_on_drop() {
        let state = make_state();
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);
        let quiesce = begin(&state).expect("begin");

        let mutation = RestartCoordinator::enter_mutation(&coordinator);
        let err = quiesce
            .wait_quiesced(Duration::from_millis(50))
            .expect_err("must time out while a mutation is in flight");
        assert_eq!(err.mutations_in_flight, 1);
        assert!(err.unpaused_subsystems.is_empty());

        let dropper = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(mutation);
        });
        quiesce
            .wait_quiesced(Duration::from_secs(10))
            .expect("quiesces once the guard drops");
        dropper.join().expect("dropper thread");
    }

    /// Deliverable (c): timeout → `QuiesceTimedOut`, and the abort
    /// (guard drop) restores draining + restarting exactly.
    #[test]
    fn timeout_then_abort_restores_daemon_state() {
        let state = make_state();
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);
        let _mutation = RestartCoordinator::enter_mutation(&coordinator);

        let quiesce = begin(&state).expect("begin");
        {
            let st = state.lock().unwrap();
            assert!(st.restarting, "begin sets restarting");
            assert!(st.draining, "begin closes the front door");
        }
        let err = quiesce
            .wait_quiesced(Duration::from_millis(30))
            .expect_err("held mutation guard must force a timeout");
        assert_eq!(err.mutations_in_flight, 1);

        quiesce.abort();
        let st = state.lock().unwrap();
        assert!(!st.restarting, "abort clears restarting");
        assert!(
            !st.draining,
            "abort restores the pre-begin drain value (false here)"
        );
    }

    /// An operator-initiated drain that predates `begin` must SURVIVE
    /// the abort — restoring `draining=false` would silently cancel the
    /// operator's manual seam.
    #[test]
    fn abort_preserves_pre_existing_operator_drain() {
        let state = make_state();
        state.lock().unwrap().draining = true;

        let quiesce = begin(&state).expect("begin");
        drop(quiesce);

        let st = state.lock().unwrap();
        assert!(!st.restarting);
        assert!(
            st.draining,
            "operator drain from before begin() must still be in effect"
        );
    }

    /// Deliverable (d): the single-restart latch. A second `begin`
    /// while one attempt is active refuses with `RestartBusy`; after
    /// the first aborts, `begin` works again.
    #[test]
    fn second_begin_while_active_is_busy() {
        let state = make_state();
        let first = begin(&state).expect("first begin");
        let second = begin(&state);
        assert_eq!(second.err(), Some(RestartBusy));

        drop(first);
        let third = begin(&state).expect("latch released on abort");
        drop(third);
    }

    /// Safe-point protocol end to end: request → subsystem ack (park)
    /// → quiesced → abort resumes the parked thread.
    #[test]
    fn subsystem_park_ack_and_resume_on_abort() {
        let state = make_state();
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);
        let handle =
            RestartCoordinator::register_subsystem(&coordinator, "test-reaper");
        assert!(!handle.pause_requested(), "no pause before begin");

        let quiesce = begin(&state).expect("begin");
        assert!(handle.pause_requested(), "begin requests the pause");

        // Simulated subsystem loop: reach the safe point, park.
        let worker = thread::spawn(move || {
            assert!(handle.park_if_requested(), "must park under a request");
            // Returns only after resume; hand the handle back out so
            // the test can assert post-resume state.
            handle
        });

        quiesce
            .wait_quiesced(Duration::from_secs(10))
            .expect("quiesced once the subsystem acks");

        quiesce.abort();
        let handle = worker.join().expect("worker resumed after abort");
        assert!(
            !handle.pause_requested(),
            "abort clears the pause request"
        );
        assert!(
            !handle.park_if_requested(),
            "no pause in effect after abort — park is a no-op"
        );
    }

    /// A registered subsystem that never acks holds the barrier: the
    /// timeout error names it.
    #[test]
    fn unacked_subsystem_times_out_with_its_name() {
        let state = make_state();
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);
        let _handle =
            RestartCoordinator::register_subsystem(&coordinator, "wedged-writer");

        let quiesce = begin(&state).expect("begin");
        let err = quiesce
            .wait_quiesced(Duration::from_millis(30))
            .expect_err("un-acked subsystem must hold the barrier");
        assert_eq!(err.mutations_in_flight, 0);
        assert_eq!(err.unpaused_subsystems, vec!["wedged-writer".to_string()]);
        // Display carries the diagnosis inline (the design doc's
        // report-inline requirement for restart_busy).
        let msg = err.to_string();
        assert!(msg.contains("restart_busy"), "{}", msg);
        assert!(msg.contains("wedged-writer"), "{}", msg);
    }

    /// A dropped handle (subsystem thread exited) is pruned, not
    /// counted as unpaused — an exiting reader must never wedge a
    /// restart.
    #[test]
    fn dropped_handle_is_pruned_from_the_barrier() {
        let state = make_state();
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);
        let handle =
            RestartCoordinator::register_subsystem(&coordinator, "short-lived");
        drop(handle);

        let quiesce = begin(&state).expect("begin");
        quiesce
            .wait_quiesced(Duration::from_millis(200))
            .expect("dead registration must not hold the barrier");
    }

    /// A subsystem registering MID-quiesce (e.g. a workflow respawn's
    /// reader — drain doesn't gate those) inherits the in-effect pause
    /// request instead of running unfenced.
    #[test]
    fn registration_mid_quiesce_inherits_pause_request() {
        let state = make_state();
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);
        let quiesce = begin(&state).expect("begin");

        let handle =
            RestartCoordinator::register_subsystem(&coordinator, "late-reader");
        assert!(
            handle.pause_requested(),
            "handle born mid-quiesce must see the pause request"
        );
        drop(quiesce);
        assert!(!handle.pause_requested(), "abort resumes late registrants too");
    }

    /// `daemon.health` surfaces the attempt (deliverable 3): restarting
    /// rides beside draining, and both clear on abort.
    #[test]
    fn daemon_health_reports_restarting_beside_draining() {
        let state = make_state();
        let before = crate::control::methods::daemon_health(&state).expect("health");
        assert_eq!(before["restarting"], serde_json::json!(false));
        assert_eq!(before["draining"], serde_json::json!(false));

        let quiesce = begin(&state).expect("begin");
        let during = crate::control::methods::daemon_health(&state).expect("health");
        assert_eq!(during["restarting"], serde_json::json!(true));
        assert_eq!(during["draining"], serde_json::json!(true));

        drop(quiesce);
        let after = crate::control::methods::daemon_health(&state).expect("health");
        assert_eq!(after["restarting"], serde_json::json!(false));
        assert_eq!(after["draining"], serde_json::json!(false));
    }
}
