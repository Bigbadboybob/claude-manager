//! Continuous Tasks — daemon-resident periodic-fire scheduler (Phase 3).
//!
//! ## Why this exists
//!
//! Phase 2 shipped the `trigger` funnel — a continuous task fires only when an
//! operator (or an agent fanning out) calls it. Phase 3 adds the autonomous
//! driver: a daemon thread that, every tick, fires `Periodic` tasks that have
//! come due, respawns dead supervised `Persistent` sessions, and reconciles the
//! `in_flight` spawn-window guards a crash/kill could leak.
//!
//! ## Structural twin of `workflow::poller`
//!
//! This is a deliberate twin of [`crate::workflow::poller::WorkflowPoller`]:
//! the same `{ state, shutdown, tick_micros, handle, panic_record }` lifecycle,
//! the same `new` / `start` (idempotent, `io::Result`) / `shutdown`
//! (signal-the-flag + join) shape, and the same `run_loop` that wraps each tick
//! in `catch_unwind` + a chunked, shutdown-aware sleep. [`ContinuousScheduler::tick_once`]
//! is the twin of `poll_once`: it mirrors the collect-under-lock / DROP-lock /
//! act-lock-free discipline.
//!
//! ## Lock discipline (the #1 risk)
//!
//! The scheduler NEVER holds `state.lock()` across a `methods::trigger` call or
//! a PTY write — `trigger` re-acquires the `DaemonState` mutex internally, and
//! the reaper's `on_exit` callback re-acquires it too, so holding it across a
//! fire would deadlock. Task state is read from DISK via
//! [`crate::continuous::task::load_all`] (no `DaemonState` lock); the only lock
//! the tick takes is a BRIEF, read-only session-liveness probe, dropped before
//! any fire.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::continuous::runlog::{ContinuousRunLog, RunLogLine};
use crate::continuous::task::{self, ContinuousTask, RunStatus, Schedule};
use crate::control::protocol::Caller;
use crate::state::DaemonState;
use crate::workflow::poller::PanicRecord;

/// Default tick interval (microseconds). Poller-class cadence — mirrors
/// [`crate::workflow::poller::DEFAULT_TICK_INTERVAL_MICROS`]. Seeded from
/// `[scheduler] tick_interval` in [`ContinuousScheduler::new`]; this is the
/// fallback when that configured value is `0`. Distinct from the poller's
/// identically-named const (different module path, no collision).
pub const DEFAULT_TICK_INTERVAL_MICROS: u64 = 250_000;

/// Lower bound the scheduler respects no matter what `tick_interval` is set to.
/// Mirrors the poller's floor; prevents a `tick_interval = 0`/tiny value from
/// busy-looping a CPU core.
const MIN_TICK_INTERVAL_MICROS: u64 = 1_000; // 1ms floor

/// Spawn-window grace: a freshly-armed `in_flight` whose session hasn't been
/// inserted into the registry yet is a HEALTHY mid-spawn fire, not a leak.
/// Restart reconciliation only orphans a guard older than this — long enough to
/// outlast any plausible spawn (claude-trust + the two-phase `start_session`),
/// short enough that a guard genuinely leaked across a daemon restart clears
/// promptly. Without it, the 250 ms tick would race a concurrent manual fire's
/// spawn window and falsely orphan a healthy fire.
const RECONCILE_GRACE_SECS: u64 = 60;

/// Exponential-backoff ceiling for a persistently-failing task (seconds).
const BACKOFF_CAP_SECS: u64 = 3600; // 1 hour

/// Outcome of a single in-process fire, distilled from `methods::trigger`'s
/// return into the three cases the ADVANCE phase branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FireOutcome {
    /// `Ok({fired:true})` — the fire happened. Reset `consecutive_failures`.
    Fired,
    /// `Ok({fired:false, reason:busy|paused|duplicate_fire_token})` — a benign
    /// skip (the `in_flight` guard / `paused` flag already prevents
    /// re-selection). Do NOT bump `consecutive_failures`.
    Skipped,
    /// `Err((code,msg))` — a hard failure. Bump `consecutive_failures` and back
    /// the next fire off exponentially.
    Failed,
}

/// Daemon-side continuous-task scheduler. Structural twin of
/// [`crate::workflow::poller::WorkflowPoller`]. Spawn via [`Self::start`], tear
/// down via [`Self::shutdown`]. Tests construct it and drive [`Self::tick_once`]
/// manually without the loop thread.
pub struct ContinuousScheduler {
    state: Arc<Mutex<DaemonState>>,
    shutdown: Arc<AtomicBool>,
    tick_micros: Arc<AtomicU64>,
    handle: Mutex<Option<JoinHandle<()>>>,
    /// Latest panic from `run_loop`'s `catch_unwind` branch + a running count.
    /// `None` until the first panic. Mirrors the poller's field.
    panic_record: Arc<Mutex<Option<PanicRecord>>>,
    /// Test-only: substitute the in-process fire so unit tests can drive the
    /// due-check / supervision / advance phases without spawning a real agent.
    /// Records the task_ids fired and returns a canned [`FireOutcome`].
    /// Production never installs this — `fire_task` calls `methods::trigger`.
    #[cfg(test)]
    fire_spy: Mutex<Option<FireSpy>>,
}

#[cfg(test)]
struct FireSpy {
    calls: Vec<String>,
    outcome: FireOutcome,
}

impl ContinuousScheduler {
    /// Construct without starting the loop thread. Seeds `tick_micros` from
    /// `[scheduler] tick_interval` (micros), clamped to the 1 ms floor, falling
    /// back to [`DEFAULT_TICK_INTERVAL_MICROS`] when the configured value is
    /// `0`. Mirrors `WorkflowPoller::new` but honors the config (the poller
    /// hardcodes its default) so lib.rs stays a plain `new()` + `start()`.
    pub fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        let configured = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            s.config.scheduler.tick_interval
        };
        let seed = if configured == 0 {
            DEFAULT_TICK_INTERVAL_MICROS
        } else {
            configured.max(MIN_TICK_INTERVAL_MICROS)
        };
        ContinuousScheduler {
            state,
            shutdown: Arc::new(AtomicBool::new(false)),
            tick_micros: Arc::new(AtomicU64::new(seed)),
            handle: Mutex::new(None),
            panic_record: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            fire_spy: Mutex::new(None),
        }
    }

    /// Read the most recent `tick_once` panic, if any. `None` until `run_loop`
    /// has caught at least one panic. Mirror of `WorkflowPoller::panic_record`.
    pub fn panic_record(&self) -> Option<PanicRecord> {
        self.panic_record.lock().unwrap().clone()
    }

    /// Spawn the loop thread. Idempotent: a second call is a no-op. Returns
    /// `io::Result` so a transient thread-spawn failure surfaces to the caller
    /// in `lib.rs::run()` (which treats it as FATAL — the scheduler is the only
    /// periodic-fire driver). Verbatim shape of `WorkflowPoller::start`.
    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("cm-continuous-scheduler".into())
            .spawn(move || me.run_loop())?;
        *guard = Some(handle);
        Ok(())
    }

    /// Signal the loop to exit and join. Safe from any thread; safe (no-op) if
    /// never started (handle is `None`). Verbatim shape of
    /// `WorkflowPoller::shutdown`.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let handle = self.handle.lock().unwrap().take();
        if let Some(h) = handle {
            // Ignore join errors — a panicked tick was already caught by
            // `catch_unwind` inside the loop, so a panic at the join boundary
            // would be a bug to surface but not abort shutdown over.
            let _ = h.join();
        }
    }

    /// Test-only setter. Production uses the config-seeded value. Tests drop it
    /// to ~10 ms so the shutdown-latency invariant runs fast. Mirror of
    /// `WorkflowPoller::set_tick_interval_for_test`.
    #[cfg(test)]
    pub fn set_tick_interval_for_test(&self, micros: u64) {
        let clamped = micros.max(MIN_TICK_INTERVAL_MICROS);
        self.tick_micros.store(clamped, Ordering::SeqCst);
    }

    /// Test-only: install a fire spy so `tick_once` records due/supervision
    /// fires and returns `outcome` instead of calling `methods::trigger`.
    #[cfg(test)]
    fn arm_fire_spy(&self, outcome: FireOutcome) {
        *self.fire_spy.lock().unwrap_or_else(|p| p.into_inner()) = Some(FireSpy {
            calls: Vec::new(),
            outcome,
        });
    }

    /// Test-only: the task_ids the fire spy recorded this run, in order.
    #[cfg(test)]
    fn fire_spy_calls(&self) -> Vec<String> {
        self.fire_spy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|s| s.calls.clone())
            .unwrap_or_default()
    }

    /// One iteration of the loop. Public so tests drive it deterministically
    /// without the loop thread (mirrors `poll_once`). Wrapping in `catch_unwind`
    /// is `run_loop`'s job, not here.
    ///
    /// Phases (DESIGN_CONTINUOUS_TASKS.md §7), mirroring `poll_once`'s
    /// collect-under-lock / DROP / act-lock-free discipline:
    ///   (a) restart reconciliation — clear a leaked `in_flight` guard.
    ///   (b) load tasks from disk (authority).
    ///   (c) supervision — respawn a dead supervised `Persistent` session.
    ///   (d) due check — snapshot the due `Periodic` set.
    ///   (e) fire + advance — fire each due task, advance `next_fire_at`
    ///       catch-up-once (or back off on failure).
    pub fn tick_once(&self) {
        // Master on/off (read under a brief lock). lib.rs still constructs +
        // starts the thread when disabled; the tick is just a no-op.
        {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if !s.config.scheduler.enabled {
                return;
            }
        }

        let now = task::now_unix();

        // (a) RESTART RECONCILIATION — closes the Phase-2 in_flight-leak
        // residual. Runs BEFORE the load below so (c)/(d) see post-reconcile
        // state.
        self.reconcile_orphans(now);

        // (b) LOAD — disk is authority (no DaemonState lock). Phase 3 is
        // Periodic-only + small N, so a plain Vec filter of the due set is
        // behaviorally identical to the design's BinaryHeap due-index (that
        // heap is an optimization deferred until load volume warrants it).
        let tasks = task::load_all();

        // Tasks already fired this tick (supervision), so the due check below
        // doesn't double-fire a Persistent+Periodic task.
        let mut acted: HashSet<String> = HashSet::new();

        // (c) SUPERVISION — dead-persistent respawn ONLY (NO fresh-hang / Stuck
        // / investigator; that is Phase 3b). A supervised Persistent task whose
        // pinned session died is respawned via the same `trigger` funnel (its
        // persistent executor handles dead->respawn).
        for tk in &tasks {
            if !should_supervise(tk) {
                continue;
            }
            let Some(uid) = tk.current_session_uid.as_deref() else {
                continue;
            };
            if !self.session_is_dead(uid) {
                continue;
            }
            let fire_token = mint_fire_token();
            if self.fire_task(&tk.task_id, &fire_token) == FireOutcome::Fired {
                acted.insert(tk.task_id.clone());
                append_supervision_audit(tk, &fire_token, now);
            }
        }

        // (d) DUE CHECK — pure read of the disk-loaded tasks. Consumer / Cron /
        // OnDemand are skipped (Periodic-only in Phase 3).
        let due = collect_due(&tasks, now);

        // (e) FIRE + ADVANCE — no DaemonState lock held across the fire.
        for task_id in due {
            if acted.contains(&task_id) {
                continue;
            }
            let fire_token = mint_fire_token();
            let outcome = self.fire_task(&task_id, &fire_token);
            self.advance_after_fire(&task_id, outcome, now);
        }
    }

    /// Phase (a): orphan any task whose `in_flight` spawn-window guard outlived
    /// its spawn window AND whose guarded session is dead (registry-absent or
    /// kernel-exited). Marks `last_run.status = Orphaned`, clears `in_flight`,
    /// and appends an `"orphaned"` runs.jsonl line. This closes the Phase-2
    /// in_flight-leak residual (a guard left set on an unclean fire path); an
    /// overdue on-disk guard from before a daemon restart clears here too.
    fn reconcile_orphans(&self, now: u64) {
        for tk in task::load_all() {
            let Some(inflight) = tk.in_flight.as_ref() else {
                continue;
            };
            // Spawn-window grace: a just-armed guard whose session isn't in the
            // registry yet is a healthy mid-spawn fire, not a leak.
            if now.saturating_sub(inflight.started_at) < RECONCILE_GRACE_SECS {
                continue;
            }
            if !self.session_is_dead(&inflight.session_uid) {
                continue;
            }
            let fire_token = inflight.fire_token.clone();
            let session_uid = inflight.session_uid.clone();
            let seq = tk
                .last_run
                .as_ref()
                .map(|r| r.seq)
                .unwrap_or(tk.run_count as u64);
            let _ = task::modify(&tk.task_id, |t| {
                if let Some(lr) = t.last_run.as_mut() {
                    lr.status = RunStatus::Orphaned;
                }
                t.in_flight = None;
            });
            if let Err(e) = ContinuousRunLog::append(&RunLogLine {
                seq,
                ts: now as f64,
                task_id: tk.task_id.clone(),
                event: "orphaned".to_string(),
                fire_token: Some(fire_token),
                session_uid: Some(session_uid),
                run_mode: None,
                trigger_source: Some("continuous-scheduler".to_string()),
                status: Some("orphaned".to_string()),
                detail: None,
            }) {
                eprintln!(
                    "cm-daemon: continuous scheduler failed to append \"orphaned\" \
                     audit line for task={}: {}",
                    tk.task_id, e,
                );
            }
        }
    }

    /// Phase (e) advance: record the periodic fire and recompute `next_fire_at`.
    /// CATCH-UP-ONCE — `next_fire_at = now + every_secs` recomputed from `now`,
    /// NEVER accumulated from `last_fired_at` (backfilling missed slots after
    /// downtime would fire a burst). On a hard failure, bump
    /// `consecutive_failures` and back the next fire off exponentially (capped);
    /// a successful fire resets the counter; a benign skip leaves `next_fire_at`
    /// to retry without bumping failures.
    fn advance_after_fire(&self, task_id: &str, outcome: FireOutcome, now: u64) {
        let _ = task::modify(task_id, |t| {
            let every = match &t.schedule {
                Schedule::Periodic { every_secs } => (*every_secs).max(1),
                // Non-periodic can't reach here (collect_due filters Periodic);
                // be defensive and leave scheduling untouched.
                _ => return,
            };
            t.last_fired_at = now;
            match outcome {
                FireOutcome::Fired => {
                    t.consecutive_failures = 0;
                    t.next_fire_at = now.saturating_add(every);
                }
                FireOutcome::Skipped => {
                    // Benign skip (busy/paused/duplicate): the in_flight guard /
                    // paused flag already blocks re-selection; leave
                    // next_fire_at to retry next due tick, and do NOT bump
                    // failures (the task is healthy).
                }
                FireOutcome::Failed => {
                    t.consecutive_failures = t.consecutive_failures.saturating_add(1);
                    t.next_fire_at =
                        now.saturating_add(backoff_secs(every, t.consecutive_failures));
                }
            }
        });
    }

    /// Fire a task in-process via `methods::trigger` with an internal Operator
    /// caller + the minted idempotency token, distilling the return into a
    /// [`FireOutcome`]. Mirrors `WorkflowPoller::fire_static_transition`'s
    /// `Caller::operator(...) + methods::*` pattern. NO DaemonState lock is held
    /// here (trigger re-acquires it internally).
    fn fire_task(&self, task_id: &str, fire_token: &str) -> FireOutcome {
        #[cfg(test)]
        {
            let mut g = self.fire_spy.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(spy) = g.as_mut() {
                spy.calls.push(task_id.to_string());
                return spy.outcome;
            }
        }
        let caller = Caller::operator("continuous-scheduler");
        let params = serde_json::json!({ "task_id": task_id, "fire_token": fire_token });
        // Per-fire panic isolation. `trigger` is written to return `Err`, never
        // panic — but an unexpected panic for ONE task's config must not unwind
        // the whole due-loop: that would starve every other due task this tick
        // and, because `advance_after_fire` never runs, leave `next_fire_at`
        // unadvanced so the panicking task re-fires every tick (a scheduler-wide
        // hot-loop). Catch it here and treat it as a hard failure so the task
        // backs off and the loop continues. `run_loop`'s catch_unwind is the
        // outer backstop; a poisoned `state` mutex is recovered via `into_inner`
        // on the next lock (as everywhere in this file).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::control::methods::trigger(&self.state, &caller, &params)
        }));
        match result {
            Ok(Ok(v)) => {
                if v.get("fired").and_then(|f| f.as_bool()).unwrap_or(false) {
                    FireOutcome::Fired
                } else {
                    // busy / paused / duplicate_fire_token — a benign skip.
                    FireOutcome::Skipped
                }
            }
            Ok(Err((code, msg))) => {
                eprintln!(
                    "cm-daemon: continuous scheduler fire failed for task={}: {:?}: {}",
                    task_id, code, msg,
                );
                FireOutcome::Failed
            }
            Err(panic) => {
                eprintln!(
                    "cm-daemon: continuous scheduler fire PANICKED for task={}: {}",
                    task_id,
                    panic_payload_to_string(&panic),
                );
                FireOutcome::Failed
            }
        }
    }

    /// Brief, read-only session-liveness probe — the ONLY DaemonState lock the
    /// tick takes. A uid is DEAD iff it is registry-absent (the reaper's
    /// `on_exit` removed it) OR its `last_exit` shows a kernel-recorded exit
    /// (`kernel_set` flips before the registry removal, so there is no liveness
    /// gap). Prefers the immutable `kernel_set()` read over `try_wait` (which
    /// needs `&mut`). The lock drops on return — before any fire.
    fn session_is_dead(&self, uid: &str) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .sessions
            .get(uid)
            .map_or(true, |s| s.last_exit.kernel_set())
    }

    /// The loop body, run on the spawned thread. Wraps each `tick_once` in
    /// `catch_unwind` so a panic in one tick doesn't kill the daemon, then
    /// sleeps the remainder of the tick interval in shutdown-aware chunks.
    /// Verbatim shape of `WorkflowPoller::run_loop`.
    fn run_loop(self: Arc<Self>) {
        while !self.shutdown.load(Ordering::SeqCst) {
            let tick_start = Instant::now();

            // Panic safety: a panic in `tick_once` shouldn't crash the daemon.
            // `AssertUnwindSafe` is justified because the closure borrows
            // `&self` immutably and lock guards drop on unwind.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.tick_once()
            }));

            if let Err(panic) = result {
                let msg = panic_payload_to_string(&panic);
                eprintln!(
                    "cm-daemon: continuous scheduler panicked in tick_once: {} \
                     (continuing — daemon stays up; next tick will retry)",
                    msg,
                );
                let mut rec = self.panic_record.lock().unwrap();
                let count = rec.as_ref().map(|r| r.count + 1).unwrap_or(1);
                *rec = Some(PanicRecord { message: msg, count });
            }

            // Sleep the remainder of the tick interval in small chunks so
            // shutdown latency is bounded by ~MIN_TICK_INTERVAL_MICROS rather
            // than the full tick. The shutdown flag is re-checked at the top of
            // the loop, so worst-case shutdown latency is `2 × tick_interval`.
            let tick_micros = self.tick_micros.load(Ordering::SeqCst);
            let target = Duration::from_micros(tick_micros);
            let elapsed = tick_start.elapsed();
            if elapsed < target {
                let remaining = target - elapsed;
                let chunk = Duration::from_micros(MIN_TICK_INTERVAL_MICROS);
                let mut left = remaining;
                while left > Duration::ZERO {
                    if self.shutdown.load(Ordering::SeqCst) {
                        return;
                    }
                    let sleep_for = if left < chunk { left } else { chunk };
                    std::thread::sleep(sleep_for);
                    left = left.saturating_sub(sleep_for);
                }
            }
        }
    }
}

/// Phase (c) predicate: a supervision candidate is an enabled, un-paused,
/// supervised `Persistent` task with no in-flight fire. (Liveness of its pinned
/// session is probed separately by the caller.) A task with no
/// `current_session_uid` has never fired — supervision restarts a session that
/// died, it does not bootstrap one, so the caller also requires a uid.
fn should_supervise(tk: &ContinuousTask) -> bool {
    tk.run_mode == task::RunMode::Persistent
        && tk.supervise
        && tk.enabled
        && !tk.paused
        && tk.in_flight.is_none()
}

/// Append a `"supervised_restart"` audit line after a supervised respawn.
fn append_supervision_audit(tk: &ContinuousTask, fire_token: &str, now: u64) {
    if let Err(e) = ContinuousRunLog::append(&RunLogLine {
        seq: tk.run_count as u64,
        ts: now as f64,
        task_id: tk.task_id.clone(),
        event: "supervised_restart".to_string(),
        fire_token: Some(fire_token.to_string()),
        session_uid: tk.current_session_uid.clone(),
        run_mode: Some("persistent".to_string()),
        trigger_source: Some("continuous-scheduler".to_string()),
        status: None,
        detail: None,
    }) {
        eprintln!(
            "cm-daemon: continuous scheduler failed to append \
             \"supervised_restart\" audit line for task={}: {}",
            tk.task_id, e,
        );
    }
}

/// Phase (d) due predicate: the set of task_ids that should fire this tick.
/// `enabled && !paused && in_flight.is_none() && Schedule::Periodic &&
/// next_fire_at <= now`. Consumer / Cron / OnDemand are skipped (Phase 4+).
/// Pure read — no DaemonState lock.
fn collect_due(tasks: &[ContinuousTask], now: u64) -> Vec<String> {
    tasks
        .iter()
        .filter(|t| {
            t.enabled
                && !t.paused
                && t.in_flight.is_none()
                && matches!(t.schedule, Schedule::Periodic { .. })
                && t.next_fire_at <= now
        })
        .map(|t| t.task_id.clone())
        .collect()
}

/// Exponential, capped backoff (seconds) after `consecutive_failures` failed
/// fires: `every_secs * 2^failures`, clamped to [`BACKOFF_CAP_SECS`]. Keeps a
/// persistently-failing task from hot-looping.
fn backoff_secs(every_secs: u64, consecutive_failures: u32) -> u64 {
    let base = every_secs.max(1);
    let shift = consecutive_failures.min(16);
    let mult = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    base.saturating_mul(mult).min(BACKOFF_CAP_SECS)
}

/// Mint a fresh idempotency token for a scheduler-driven fire. A FRESH token
/// every tick keeps `trigger`'s duplicate-fire-token guard from ever tripping
/// for a periodic fire (idempotency keys are for caller-supplied retries);
/// overlapping fires are blocked by the `in_flight` guard instead. Mirrors
/// `methods::new_fire_token`'s scheme (private there, so re-minted here).
fn mint_fire_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ft_sched_{:x}-{:x}", nanos, n)
}

/// Best-effort decode of a `catch_unwind` payload. Copied verbatim from
/// `workflow::poller::panic_payload_to_string` (private there, not importable).
fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<panic payload not a string>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuous::task::{Engine, InFlight, RunMode, RunRecord};

    /// Serialize HOME env mutation across the whole crate (cargo runs tests
    /// from different modules in parallel). Mirror of the helper in
    /// `continuous::task` / `continuous::runlog` tests.
    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        f();
        if let Some(o) = orig {
            unsafe {
                std::env::set_var("HOME", o);
            }
        }
        tmp
    }

    fn scheduler() -> Arc<ContinuousScheduler> {
        Arc::new(ContinuousScheduler::new(Arc::new(Mutex::new(
            DaemonState::default(),
        ))))
    }

    fn periodic_task(id: &str, every_secs: u64, next_fire_at: u64) -> ContinuousTask {
        let mut t = ContinuousTask::new(
            id.to_string(),
            id.to_string(),
            format!("ws-{}", id),
            "/tmp/repo".to_string(),
            Engine::Claude,
            RunMode::Fresh,
            Schedule::Periodic { every_secs },
            "go".to_string(),
        );
        t.next_fire_at = next_fire_at;
        t
    }

    // ----- (a) restart reconciliation -----

    /// A leaked `in_flight` guard (armed long ago, session never in the
    /// registry) is reconciled: `last_run.status` → Orphaned, guard cleared, an
    /// `"orphaned"` audit line appended. Closes the Phase-2 in_flight residual.
    #[test]
    fn reconcile_orphans_marks_leaked_in_flight_and_clears_guard() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            let now = 10_000u64;
            let mut t = periodic_task("recon", 100, now);
            t.in_flight = Some(InFlight {
                fire_token: "ft-leak".into(),
                session_uid: "ts-dead-uid".into(),
                started_at: now - 1000, // past the grace window
            });
            t.last_run = Some(RunRecord {
                seq: 1,
                fire_token: "ft-leak".into(),
                started_at: now - 1000,
                finished_at: None,
                session_uid: Some("ts-dead-uid".into()),
                status: RunStatus::Running,
                trigger_source: "operator".into(),
            });
            task::save(&t).expect("save");

            sched.reconcile_orphans(now);

            let reloaded = task::load_one("recon").expect("load");
            assert!(reloaded.in_flight.is_none(), "leaked guard cleared");
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Orphaned,
            );
            let runs =
                std::fs::read_to_string(task::runs_log_path("recon")).expect("runs.jsonl");
            assert!(runs.contains("\"orphaned\""), "orphan audit line: {}", runs);
        });
    }

    /// A freshly-armed guard (within the spawn-window grace) is a healthy
    /// mid-spawn fire — reconciliation must NOT orphan it.
    #[test]
    fn reconcile_respects_spawn_window_grace() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            let now = 10_000u64;
            let mut t = periodic_task("grace", 100, now);
            t.in_flight = Some(InFlight {
                fire_token: "ft-fresh".into(),
                session_uid: "ts-spawning".into(),
                started_at: now - 5, // < RECONCILE_GRACE_SECS
            });
            task::save(&t).expect("save");

            sched.reconcile_orphans(now);

            let reloaded = task::load_one("grace").expect("load");
            assert!(reloaded.in_flight.is_some(), "mid-spawn guard preserved");
        });
    }

    // ----- (d) due check -----

    /// The due predicate selects EXACTLY the eligible Periodic tasks: due, not
    /// future / paused / disabled / in-flight / non-Periodic.
    #[test]
    fn collect_due_selects_only_eligible_periodic_tasks() {
        let now = 1000u64;
        let mk = |id: &str,
                  sched: Schedule,
                  next: u64,
                  enabled: bool,
                  paused: bool,
                  inflight: bool| {
            let mut t = ContinuousTask::new(
                id.into(),
                id.into(),
                format!("ws-{}", id),
                "/tmp/r".into(),
                Engine::Claude,
                RunMode::Fresh,
                sched,
                "go".into(),
            );
            t.next_fire_at = next;
            t.enabled = enabled;
            t.paused = paused;
            if inflight {
                t.in_flight = Some(InFlight {
                    fire_token: "f".into(),
                    session_uid: "u".into(),
                    started_at: now,
                });
            }
            t
        };
        let tasks = vec![
            mk("due", Schedule::Periodic { every_secs: 60 }, 1000, true, false, false),
            mk("future", Schedule::Periodic { every_secs: 60 }, 2000, true, false, false),
            mk("paused", Schedule::Periodic { every_secs: 60 }, 0, true, true, false),
            mk("disabled", Schedule::Periodic { every_secs: 60 }, 0, false, false, false),
            mk("busy", Schedule::Periodic { every_secs: 60 }, 0, true, false, true),
            mk("ondemand", Schedule::OnDemand, 0, true, false, false),
            mk(
                "consumer",
                Schedule::Consumer {
                    queue: "q".into(),
                    batch_max: 1,
                    window_secs: 1,
                    depth_threshold: 1,
                },
                0,
                true,
                false,
                false,
            ),
        ];
        assert_eq!(collect_due(&tasks, now), vec!["due".to_string()]);
    }

    // ----- (e) advance: catch-up-once + backoff -----

    /// CATCH-UP-ONCE: after a long downtime (next_fire_at way in the past), a
    /// fire advances next_fire_at to a SINGLE slot from `now` — never `overdue +
    /// every` (which would re-fire immediately = a backfill storm).
    #[test]
    fn advance_catch_up_once_recomputes_next_from_now() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            let mut t = periodic_task("catchup", 100, 0); // overdue since epoch
            t.last_fired_at = 0;
            task::save(&t).expect("save");

            let now = 5_000u64;
            sched.advance_after_fire("catchup", FireOutcome::Fired, now);

            let r = task::load_one("catchup").unwrap();
            assert_eq!(r.next_fire_at, now + 100, "single slot from now");
            assert!(r.next_fire_at > now, "next fire is in the future, not a re-fire");
            assert_eq!(r.last_fired_at, now);
            assert_eq!(r.consecutive_failures, 0);
        });
    }

    /// A failed fire backs the next fire off exponentially and bumps the
    /// counter; a subsequent success resets both.
    #[test]
    fn backoff_grows_then_resets_on_success() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            task::save(&periodic_task("backoff", 100, 0)).expect("save");
            let now = 1_000u64;

            sched.advance_after_fire("backoff", FireOutcome::Failed, now);
            let a = task::load_one("backoff").unwrap();
            assert_eq!(a.consecutive_failures, 1);
            let delta1 = a.next_fire_at - now;
            assert!(delta1 > 0, "backed off into the future");

            sched.advance_after_fire("backoff", FireOutcome::Failed, now);
            let b = task::load_one("backoff").unwrap();
            assert_eq!(b.consecutive_failures, 2);
            let delta2 = b.next_fire_at - now;
            assert!(delta2 > delta1, "backoff grows: {} !> {}", delta2, delta1);

            sched.advance_after_fire("backoff", FireOutcome::Fired, now);
            let c = task::load_one("backoff").unwrap();
            assert_eq!(c.consecutive_failures, 0, "reset on success");
            assert_eq!(c.next_fire_at, now + 100, "back to one period");
        });
    }

    /// `backoff_secs` grows exponentially and clamps to the cap.
    #[test]
    fn backoff_secs_caps() {
        assert_eq!(backoff_secs(100, 1), 200);
        assert_eq!(backoff_secs(100, 2), 400);
        assert_eq!(backoff_secs(100, 5), 3200);
        assert_eq!(backoff_secs(100, 6), BACKOFF_CAP_SECS); // 6400 -> capped
        assert_eq!(backoff_secs(100, 30), BACKOFF_CAP_SECS); // no overflow panic
    }

    /// A benign skip (busy/paused/duplicate) advances last_fired_at but neither
    /// bumps failures nor moves next_fire_at — the task retries next due tick.
    #[test]
    fn skipped_fire_does_not_bump_failures_or_advance() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            task::save(&periodic_task("skip", 100, 7)).expect("save");
            sched.advance_after_fire("skip", FireOutcome::Skipped, 1000);
            let r = task::load_one("skip").unwrap();
            assert_eq!(r.consecutive_failures, 0);
            assert_eq!(r.last_fired_at, 1000);
            assert_eq!(r.next_fire_at, 7, "next_fire_at left to retry");
        });
    }

    // ----- full tick_once integration (fire spied) -----

    /// A due Periodic task fires exactly once through `tick_once` and advances
    /// catch-up-once. The fire spy stands in for `methods::trigger` so no real
    /// agent spawns.
    #[test]
    fn tick_once_fires_due_periodic_task_and_advances() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            sched.arm_fire_spy(FireOutcome::Fired);
            task::save(&periodic_task("ticked", 3600, 1)).expect("save"); // overdue

            let t0 = task::now_unix();
            sched.tick_once();
            let t1 = task::now_unix();

            assert_eq!(sched.fire_spy_calls(), vec!["ticked".to_string()]);
            let r = task::load_one("ticked").unwrap();
            assert!(
                r.next_fire_at >= t0 + 3600 && r.next_fire_at <= t1 + 3600,
                "advanced catch-up-once from now: {}",
                r.next_fire_at,
            );
            assert!(r.next_fire_at > t1, "next fire is in the future (no immediate re-fire)");
        });
    }

    /// A disabled scheduler's `tick_once` is a total no-op — no fire, no advance.
    #[test]
    fn tick_once_noop_when_scheduler_disabled() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            state.lock().unwrap().config.scheduler.enabled = false;
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            sched.arm_fire_spy(FireOutcome::Fired);
            task::save(&periodic_task("disabled-tick", 3600, 1)).expect("save");

            sched.tick_once();

            assert!(sched.fire_spy_calls().is_empty(), "disabled scheduler fires nothing");
            let r = task::load_one("disabled-tick").unwrap();
            assert_eq!(r.next_fire_at, 1, "next_fire_at untouched when disabled");
        });
    }

    // ----- (c) supervision -----

    /// A supervised Persistent task whose pinned session is dead
    /// (registry-absent) is respawned via the fire funnel + gets a
    /// `"supervised_restart"` audit line.
    #[test]
    fn supervision_respawns_dead_persistent_session() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            sched.arm_fire_spy(FireOutcome::Fired);
            let mut t = ContinuousTask::new(
                "sup".into(),
                "sup".into(),
                "ws-sup".into(),
                "/tmp/r".into(),
                Engine::Claude,
                RunMode::Persistent,
                Schedule::OnDemand, // not Periodic — only supervision can fire it
                "go".into(),
            );
            t.supervise = true;
            t.current_session_uid = Some("ts-dead".into()); // absent from registry => dead
            task::save(&t).expect("save");

            sched.tick_once();

            assert_eq!(sched.fire_spy_calls(), vec!["sup".to_string()]);
            let runs = std::fs::read_to_string(task::runs_log_path("sup")).unwrap();
            assert!(
                runs.contains("\"supervised_restart\""),
                "supervised_restart audit line: {}",
                runs,
            );
        });
    }

    /// A supervised Persistent task that never fired (no current_session_uid) is
    /// NOT bootstrapped by supervision — it waits for an explicit fire.
    #[test]
    fn supervision_skips_task_with_no_session() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            sched.arm_fire_spy(FireOutcome::Fired);
            let mut t = ContinuousTask::new(
                "sup-none".into(),
                "sup-none".into(),
                "ws".into(),
                "/tmp/r".into(),
                Engine::Claude,
                RunMode::Persistent,
                Schedule::OnDemand,
                "go".into(),
            );
            t.supervise = true; // but current_session_uid stays None
            task::save(&t).expect("save");

            sched.tick_once();

            assert!(sched.fire_spy_calls().is_empty(), "no session to restart");
        });
    }

    // ----- panic safety (twin of the poller's hand-rolled test) -----

    /// Mirrors `WorkflowPoller::panic_in_poll_once_is_captured_into_panic_record`:
    /// drive the `catch_unwind` + `panic_record` machinery inline (same code
    /// path shape) so the assertions are deterministic without fighting
    /// libtest's stderr capture.
    #[test]
    fn panic_in_tick_once_is_captured_into_panic_record() {
        let sched = ContinuousScheduler::new(Arc::new(Mutex::new(DaemonState::default())));
        assert!(sched.panic_record().is_none(), "no panics before any tick");
        for (i, msg) in ["first synthetic panic", "second synthetic panic"]
            .iter()
            .enumerate()
        {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| panic!("{}", *msg)));
            if let Err(panic) = result {
                let extracted = panic_payload_to_string(&panic);
                let mut rec = sched.panic_record.lock().unwrap();
                let count = rec.as_ref().map(|r| r.count + 1).unwrap_or(1);
                *rec = Some(PanicRecord {
                    message: extracted,
                    count,
                });
            }
            let rec = sched.panic_record().expect("record set after panic");
            assert_eq!(rec.count, (i + 1) as u64);
            assert_eq!(rec.message, *msg);
        }
    }

    /// `panic_payload_to_string` recovers the message from a `&'static str`
    /// panic and from a `String` panic.
    #[test]
    fn panic_payload_extracts_static_str_and_string() {
        let s = std::panic::catch_unwind(|| panic!("static str panic")).unwrap_err();
        assert_eq!(panic_payload_to_string(&s), "static str panic");

        let owned = "owned string panic".to_string();
        let p = std::panic::catch_unwind(move || panic!("{}", owned)).unwrap_err();
        assert_eq!(panic_payload_to_string(&p), "owned string panic");
    }

    // ----- lifecycle -----

    /// Shutdown latency is bounded by the (small, test) tick interval — pins the
    /// chunked-sleep shutdown check. Wrapped in a temp HOME so the loop thread's
    /// `tick_once` reads an EMPTY continuous-tasks dir (no real fires).
    #[test]
    fn shutdown_latency_bounded_by_tick_interval() {
        let _tmp = with_temp_home(|| {
            let sched = Arc::new(ContinuousScheduler::new(Arc::new(Mutex::new(
                DaemonState::default(),
            ))));
            sched.set_tick_interval_for_test(10_000); // 10ms
            sched.start().expect("spawn scheduler thread under test load");
            let t0 = Instant::now();
            sched.shutdown(); // joins the thread before returning
            assert!(
                t0.elapsed() < Duration::from_millis(200),
                "shutdown should be prompt, took {:?}",
                t0.elapsed(),
            );
        });
    }

    /// `start` is idempotent — a second call doesn't spawn a second thread.
    #[test]
    fn start_is_idempotent() {
        let _tmp = with_temp_home(|| {
            let sched = Arc::new(ContinuousScheduler::new(Arc::new(Mutex::new(
                DaemonState::default(),
            ))));
            sched.set_tick_interval_for_test(10_000);
            sched.start().expect("first start");
            sched.start().expect("second start is a no-op");
            sched.shutdown();
        });
    }
}
