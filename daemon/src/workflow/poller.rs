//! 10d-2c-2-2-a: daemon-resident workflow `on_idle` poller (skeleton).
//!
//! ## Why this exists
//!
//! Pre-2c-2-2, the static-`on_idle` polling lived in
//! `tui/src/workflow/controller.rs::tick_local`. That meant
//! daemon-spawned workflow participants needed the TUI to be running
//! and the controller to be ticking before a transition could fire on
//! a quiet agent turn. The dynamic path (`workflow_transition` MCP
//! call) was daemon-routed in 2c-1, but the static path (where the
//! agent goes idle without calling a transition tool) still required
//! TUI residency.
//!
//! 2c-2-2 moves the static-path driver into the daemon. The TUI's
//! poller stays in place — both pollers consult a per-run ownership
//! gate (`daemon_owns_run`) to ensure exactly one fires per tick.
//!
//! ## Sub-split status
//!
//! **2c-2-2-a (this file's initial state)**: skeleton only.
//! - Poller thread spawns and ticks at the configured interval.
//! - `poll_once` walks active runs and produces a `Vec<Decision>`,
//!   but **every decision is currently `Skip`** because the
//!   ownership gate (`daemon_owns_run`) returns `false`
//!   unconditionally. Behavior is unchanged from pre-2c-2-2.
//! - Shutdown wiring + panic safety + lock-contention pattern are
//!   all in place and tested.
//!
//! **2c-2-2-b (next slice)**: flip the gate. `daemon_owns_run` returns
//! true when the active role's session is in `DaemonState.sessions`.
//! Decisions fire. The TUI controller gets the same gate (2c-2-3
//! bundle).
//!
//! **2c-2-2-c (final slice)**: cross-path interaction tests
//! (`workflow_transition` racing the static poller, etc.).
//!
//! ## Ownership boundaries
//!
//! - `state.workflow_runs`, `events.jsonl`: written only via
//!   `try_modify` + `WorkflowEventsWriter` (the 2c-1 plumbing). The
//!   poller is a writer.
//! - `state.workflow_definitions`: read-only here. Written by
//!   `workflow_update_definitions` (2c-2-1).
//! - `state.sessions`: read-only for the ownership gate.
//!
//! ## Lock-contention pattern
//!
//! The poller MUST NOT hold `state.lock()` across transcript I/O.
//! The flow is:
//!
//! 1. Acquire `state.lock()` briefly to collect a `Vec<TickSnapshot>`
//!    (pure read, no I/O).
//! 2. Drop the lock.
//! 3. For each snapshot, do the transcript / events I/O without the
//!    lock.
//! 4. For each fire decision, re-acquire via `try_modify` whose
//!    closure validates that the snapshot is still applicable
//!    (active_role unchanged, etc.) and aborts otherwise.
//!
//! This is enforced by code structure: `collect_snapshots` returns
//! `Vec<TickSnapshot>` only, no guard.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Wall-clock milliseconds since the Unix epoch. Used by the delivery drainer
/// for the persisted deferred-Enter gap deadline (`enter_fire_at_ms`).
fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

use crate::control::protocol::Caller;
use crate::state::DaemonState;
use crate::workflow::run::WorkflowRun;
use crate::workflow::toml_schema::{Engine, Workflow};

/// Default tick interval (microseconds). 250ms is a balance between
/// responsiveness (a quiet agent's turn-complete fires the next
/// transition within a quarter-second) and the work the poller does
/// per tick (transcript reads on every active run). Tests override
/// this via `set_tick_interval_for_test`.
pub const DEFAULT_TICK_INTERVAL_MICROS: u64 = 250_000;

/// Lower bound the poller respects no matter what value the
/// configurable interval is set to. Prevents a malformed
/// `set_tick_interval_for_test(0)` from busy-looping a CPU core.
const MIN_TICK_INTERVAL_MICROS: u64 = 1_000; // 1ms floor

/// One per-run decision emitted by `poll_once`. 2c-2-2-b makes
/// `ActivateStatic` a fire path: `poll_once` calls
/// `workflow_transition` internally for each, reusing the same
/// handler MCP callers use (battle-tested across 2c-1's 15 reviewer
/// rounds). Skip carries a typed reason for log + test visibility.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Daemon-owned active role had a completed turn since baseline;
    /// a static `on_idle` transition fires. Carries the rendered
    /// activation prompt — built daemon-side via
    /// [`DaemonWorkflowResolver`] so the TUI tail can deliver it
    /// even when the TUI was offline at fire time.
    ActivateStatic {
        run_id: String,
        from_role: String,
        to_role: String,
        rendered_prompt: String,
    },
    /// Run was inspected but no fire — gate didn't favor daemon
    /// ownership, the agent isn't idle, or some precondition isn't
    /// met yet. Carries `reason` for log/test visibility.
    Skip { run_id: String, reason: SkipReason },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Active role's session isn't daemon-owned. The TUI's
    /// controller fires for this run. See [`daemon_owns_run`].
    TuiOwns,
    /// Run is paused. Pollers skip paused runs until resumed.
    Paused,
    /// Run has no active role (transient state during start/stop).
    NoActiveRole,
    /// Active role's session has no transcript id yet (spawn in
    /// flight). Subsequent tick retries.
    NoTranscriptId,
    /// Active role's workspace has no worktree_path yet (sub-2b-3
    /// async detector window).
    NoWorktreePath,
    /// Workflow definition isn't loaded yet (TUI hasn't pushed
    /// `workflow_update_definitions` yet, or the workflow name on
    /// the run doesn't match any pushed definition). R10.
    NoWorkflowDefinition,
    /// Workflow definition has no `on_idle` transition from the
    /// active role.
    NoOnIdleTransition,
    /// Gate ran, idle predicate ran, but the agent isn't idle since
    /// baseline. Most common steady-state skip.
    NotIdle,
    /// Active role's session wasn't found in either
    /// `state.sessions` or `state.tui_sessions` (the gate
    /// couldn't determine ownership). Self-resolves once
    /// `tui_sessions` push catches up (R11).
    SessionNotFound,
    /// 10d-2c-2-2-b F1: no history entry for the currently-active
    /// role yet. Happens in the 10d-2c-1 round-1 gap window —
    /// daemon's `workflow_transition` handler advances
    /// `active_role` under flock, but the TUI tail's history-entry
    /// append happens later when it consumes the event. The
    /// daemon-poller can see the post-mutation state.json before
    /// the TUI gets there. Using `role_baselines` (launch-time)
    /// in this window would be a STALE baseline for any role on
    /// its 2nd+ activation — false-positive idle fires. Skip with
    /// this reason; next tick (after TUI appends) the gate has a
    /// real `active_assistant_start_count` to compare against.
    ///
    /// Parity note: TUI's path uses
    /// `WorkflowRun::active_assistant_start_count().unwrap_or(0)`
    /// — the `unwrap_or(0)` is BENIGN there because the TUI tail
    /// performs the history append in the same `modify` closure
    /// that consumes the daemon event, so TUI never observes the
    /// gap. Daemon-poller does observe the gap, hence the skip.
    /// This divergence is intentional + commented at the call
    /// site in `evaluate_snapshot`.
    NoHistoryEntry,
}

/// What `collect_snapshots` returns under the lock. Pure data — no
/// borrows back into `DaemonState`. The poller body iterates these
/// AFTER dropping the state mutex so transcript I/O doesn't block
/// dispatch threads. See the `lock-contention pattern` docs above.
///
/// 2c-2-2-b: enough state to evaluate the gate + render the
/// activation prompt without re-acquiring the lock. Anything that
/// would require a re-read (e.g. checking `state.sessions` for the
/// gate, or workflow_definitions for the template) is captured here.
#[derive(Debug, Clone)]
struct TickSnapshot {
    run_id: String,
    workflow_name: String,
    active_role: Option<String>,
    paused: bool,
    /// Cloned `WorkflowRun` for the resolver to read (role_baselines,
    /// role_plans, goal, history). Cheap clone — `WorkflowRun` is
    /// small and the poller fires at most one transition per run
    /// per tick.
    run: WorkflowRun,
    /// Worktree path for the run's workspace, if known.
    /// `state.workspaces.get(&run.task_key).worktree_path`. None
    /// during sub-2b-3's async-detector window (R8 from prior
    /// proposal); skip with `NoWorktreePath`.
    worktree_path: Option<PathBuf>,
    /// Per-role session_type, used to derive the engine for
    /// transcript reads. Built by walking `state.sessions` +
    /// `state.tui_sessions` looking for entries whose
    /// `workflow_run_id` + `workflow_role` match this run.
    role_session_types: BTreeMap<String, String>,
    /// True iff the active role's session lives in `state.sessions`
    /// (daemon-spawned). Otherwise the TUI's poller owns this run.
    /// 2c-2-2-b's gate. Captured here so the apply phase doesn't
    /// need to re-walk session maps under the lock.
    daemon_owns: bool,
    /// True iff we found ANY session (daemon or TUI) for the active
    /// role. Distinguishes "TUI owns" from "no session bound yet"
    /// (R11 — TUI snapshot push lag).
    active_session_found: bool,
    /// Snapshot of the workflow definition, if loaded. Cloned
    /// because the apply phase may run under a fresh state lock
    /// in [`fire_static_transition`] and we don't want to re-lookup
    /// (R12: definition could be replaced mid-tick).
    workflow: Option<Workflow>,
}

/// Record of the most recent `poll_once` panic. Read by the
/// `panic_visible_after_poll_once_panic` test to assert visibility
/// deterministically without fighting libtest's stderr capture. Also
/// useful for a future daemon health endpoint — "has the poller ever
/// panicked, and what did it say?" Stderr emission is preserved
/// (line in `run_loop`) for live operability.
#[derive(Debug, Clone)]
pub struct PanicRecord {
    pub message: String,
    pub count: u64,
}

/// Daemon-side workflow poller. Spawn via `start`, signal teardown
/// via `shutdown`, join via `join`. Designed so tests can construct
/// and tick manually (`poll_once`) without starting the loop thread.
pub struct WorkflowPoller {
    state: Arc<Mutex<DaemonState>>,
    shutdown: Arc<AtomicBool>,
    tick_micros: Arc<AtomicU64>,
    handle: Mutex<Option<JoinHandle<()>>>,
    /// Latest panic from `run_loop`'s `catch_unwind` branch, plus
    /// total panic count. Populated alongside the stderr log.
    /// `None` until the first panic; tests assert on `Some(...)`.
    panic_record: Arc<Mutex<Option<PanicRecord>>>,
    /// Test-only: skip the apply phase so `poll_once` returns
    /// decisions without actually invoking `workflow_transition`.
    /// Lets gate-output tests (T1, T2 daemon-side, R10) run
    /// without the full flock + events.jsonl + state.json
    /// machinery. Production always reads this as `false`.
    disable_apply_for_test: AtomicBool,
    /// 10d-2c-2-2-b F2 test seam (pulled forward from 2c-2-2-c):
    /// runs between snapshot collection and the apply phase.
    /// Tests inject a closure that mutates `DaemonState` to
    /// simulate a concurrent role swap. The apply phase's
    /// `expected_from` check inside `workflow_transition` then
    /// aborts with Conflict.
    ///
    /// `Mutex<Option<Box<dyn ...>>>` instead of an OnceCell so
    /// tests can set, run, set-different, run again. Production
    /// never sets this.
    pre_apply_hook:
        Mutex<Option<Box<dyn Fn(&mut DaemonState) + Send + Sync>>>,
    /// Phase 3: per-session terminal-mode/quiet trackers (Phase 1
    /// `PtyModeTracker`), lazily attached to each owned session's
    /// `PtyByteFanout` the first time the delivery drainer finalizes against
    /// it. Keyed by session uid.
    finalize_trackers:
        Mutex<std::collections::HashMap<String, Arc<Mutex<crate::workflow::pty_tracker::PtyModeTracker>>>>,
    /// Phase 3: deferred-Enter gap (ms) the delivery drainer uses between a
    /// body write and its Enter. Production = `DEFAULT_ENTER_GAP_MS`; tests set
    /// 0 for instant delivery.
    finalize_gap_ms: AtomicU64,
    /// Phase 3: PTY-quiet window (ms) the delivery drainer requires before
    /// writing a body. Production = 2000; tests set 0 to bypass the gate.
    finalize_quiet_ms: AtomicU64,
}

impl WorkflowPoller {
    /// Construct without starting the loop thread. Tests use this
    /// to drive `poll_once` manually.
    pub fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        WorkflowPoller {
            state,
            shutdown: Arc::new(AtomicBool::new(false)),
            tick_micros: Arc::new(AtomicU64::new(DEFAULT_TICK_INTERVAL_MICROS)),
            handle: Mutex::new(None),
            panic_record: Arc::new(Mutex::new(None)),
            disable_apply_for_test: AtomicBool::new(false),
            pre_apply_hook: Mutex::new(None),
            finalize_trackers: Mutex::new(std::collections::HashMap::new()),
            finalize_gap_ms: AtomicU64::new(crate::workflow::finalize::DEFAULT_ENTER_GAP_MS),
            finalize_quiet_ms: AtomicU64::new(2_000),
        }
    }

    /// Test-only: set the delivery drainer's deferred-Enter gap and PTY-quiet
    /// window (both in ms). Tests pass `(0, 0)` for instant, gate-free delivery.
    pub fn set_finalize_timing_for_test(&self, gap_ms: u64, quiet_ms: u64) {
        self.finalize_gap_ms.store(gap_ms, Ordering::SeqCst);
        self.finalize_quiet_ms.store(quiet_ms, Ordering::SeqCst);
    }

    /// Test-only setter. Skips the apply phase so callers can
    /// assert on the produced `Decision` vec without invoking the
    /// `workflow_transition` handler (which would touch
    /// events.jsonl, state.json, per-run flocks, etc.). The full
    /// fire pipeline is exercised by the existing
    /// `workflow_transition_*` test suite + 2c-2-2-c's race tests.
    pub fn set_disable_apply_for_test(&self, disable: bool) {
        self.disable_apply_for_test.store(disable, Ordering::SeqCst);
    }

    /// 10d-2c-2-2-b F2 test seam: install a hook that runs between
    /// the snapshot-collect phase and the apply phase. Tests use
    /// this to inject a stale-snapshot race (e.g., flip
    /// `active_role` between snapshot and apply) and assert
    /// `workflow_transition`'s `expected_from` check catches it.
    /// Production never sets this.
    pub fn set_pre_apply_hook_for_test<F>(&self, hook: F)
    where
        F: Fn(&mut DaemonState) + Send + Sync + 'static,
    {
        *self.pre_apply_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// Read the most recent `poll_once` panic, if any. `None` until
    /// `run_loop` has caught at least one panic. Includes a running
    /// count so callers can distinguish "panicked once" from
    /// "panicking continuously."
    pub fn panic_record(&self) -> Option<PanicRecord> {
        self.panic_record.lock().unwrap().clone()
    }

    /// Spawn the loop thread. Idempotent: a second call is a no-op
    /// (the existing thread keeps running). Returns `io::Result` so
    /// a transient thread-spawn failure (FD limit, memory pressure,
    /// `RLIMIT_NPROC` hit) doesn't crash daemon startup. The caller
    /// in `lib.rs::run()` logs and continues without polling — the
    /// daemon stays up, and TUI-driven static `on_idle` still works
    /// for opt-in-off sessions.
    ///
    /// Matches the `spawn_watcher` pattern from
    /// `daemon/src/session_watch.rs` (slice 10d-memory-cap-relocation),
    /// minus the test injection seam — the surface area here is one
    /// `Builder::spawn` call and a future slice can add a
    /// `WatcherSpawnFn`-style seam if a real failure path needs to
    /// be tested. The contract test below pins the `io::Result<()>`
    /// return type so a regression to `.expect()` gets caught.
    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("cm-workflow-poller".into())
            .spawn(move || me.run_loop())?;
        *guard = Some(handle);
        Ok(())
    }

    /// Signal the loop to exit and join. Safe to call from any
    /// thread, including from inside a `Drop` impl on the daemon's
    /// top-level state. Bounded by `2 × tick_interval` worst case
    /// (one tick to notice the flag, plus the in-flight tick's
    /// remaining work).
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let handle = self.handle.lock().unwrap().take();
        if let Some(h) = handle {
            // Ignore join errors — a panicked poll iteration would
            // have already been caught by `catch_unwind` inside the
            // loop, so a panic at the join boundary would be a bug
            // we want to surface but not abort daemon shutdown over.
            let _ = h.join();
        }
    }

    /// Test-only setter. Production code uses
    /// `DEFAULT_TICK_INTERVAL_MICROS`. Tests drop it to ~10ms so the
    /// shutdown-latency invariant runs in <100ms instead of seconds.
    pub fn set_tick_interval_for_test(&self, micros: u64) {
        let clamped = micros.max(MIN_TICK_INTERVAL_MICROS);
        self.tick_micros.store(clamped, Ordering::SeqCst);
    }

    /// One iteration of the poll loop. Public so tests can drive
    /// the poller deterministically without time.
    ///
    /// Wrapping in `catch_unwind` is done by `run_loop`, not here —
    /// `poll_once` itself is allowed to panic in tests that want to
    /// assert "this would have panicked"; `run_loop` is what guards
    /// the daemon against the panic.
    ///
    /// Phases:
    /// 1. Collect snapshots under the lock (pure read).
    /// 2. Evaluate each snapshot lock-free (transcript I/O,
    ///    prompt rendering).
    /// 3. Apply `Decision::ActivateStatic` decisions via internal
    ///    `workflow_transition` calls — the same handler MCP
    ///    callers use.
    pub fn poll_once(&self) -> Vec<Decision> {
        // Phase 1: collect snapshots under the lock. Pure read.
        let snapshots = {
            let s = self.state.lock().unwrap();
            collect_snapshots(&s)
        }; // lock dropped here — transcript I/O happens lock-free

        // Phase 2: evaluate each snapshot lock-free.
        let decisions: Vec<Decision> = snapshots
            .iter()
            .map(|snap| self.evaluate_snapshot(snap))
            .collect();

        // 10d-2c-2-2-b F2 test seam: invoke pre_apply_hook (if
        // installed) between evaluate and apply. Tests use this
        // to simulate a concurrent state mutation so the
        // `expected_from` precondition in `workflow_transition`
        // fires. Production never installs this hook.
        {
            let hook_guard = self.pre_apply_hook.lock().unwrap();
            if let Some(hook) = hook_guard.as_ref() {
                let mut state = self.state.lock().unwrap();
                hook(&mut state);
            }
        }

        // Phase 3: apply each ActivateStatic via the existing
        // `workflow_transition` handler. The handler does its own
        // flock + try_modify + event-write; if the snapshot is
        // stale (e.g. active_role flipped between snapshot and
        // here), the handler's auth/precondition check fails and
        // returns Conflict — we log and move on.
        //
        // Apply errors don't promote into the returned `decisions`
        // vector (the test invariants only assert which decisions
        // were PRODUCED, not which applied). 2c-2-2-c's race tests
        // assert the apply outcome separately via state inspection.
        if !self.disable_apply_for_test.load(Ordering::SeqCst) {
            for d in &decisions {
                if let Decision::ActivateStatic {
                    run_id,
                    from_role,
                    to_role,
                    rendered_prompt,
                } = d
                {
                    if let Err(e) = self.fire_static_transition(
                        run_id,
                        from_role,
                        to_role,
                        rendered_prompt,
                    ) {
                        eprintln!(
                            "cm-daemon: workflow poller failed to fire static \
                             transition for run={} from={} to={}: {}",
                            run_id, from_role, to_role, e,
                        );
                    }
                }
            }
            // Phase 3: complete in-flight hand-offs. Keyed off run state
            // (`pending_activation`), gated on `daemon_owns_run`, idempotent —
            // re-runs a half-finalized activation after a crash and never
            // double-applies. Runs AFTER the fire loop so a just-fired
            // transition's Queued record gets finalized starting this tick.
            self.drain_finalizations();
        }

        decisions
    }

    /// Advance every daemon-owned run's `pending_activation` to completion (or
    /// until it blocks on PTY-quiet / the Enter gap / sid rebind). For each, it
    /// resolves the target role's owned session, attaches a `PtyModeTracker` to
    /// its `PtyByteFanout` (lazily, cached by uid), and drives
    /// [`finalize::advance_finalization`] — porting `submit_prompt`'s body/Enter
    /// separation and engine-correct Enter encoding into the daemon.
    fn drain_finalizations(&self) {
        struct Work {
            run_id: String,
            uid: String,
            worktree: std::path::PathBuf,
            workflow: Workflow,
            role_engines: BTreeMap<String, Engine>,
        }
        // Collect work + resolve session/worktree under the lock (pure read).
        let work: Vec<Work> = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            crate::workflow::run::load_all()
                .into_iter()
                // Running AND not paused — NOT `is_active()`, which also
                // matches Paused. A paused run (e.g. the user typed into the
                // participant session) must NOT get /clear/body/Enter shoved
                // into its PTY: that fights the user's input. Mirrors the
                // poller gate's `snap.paused` skip and the TUI's
                // `if paused { continue; }` short-circuit before delivery.
                .filter(|run| {
                    matches!(run.status, crate::workflow::run::RunStatus::Running)
                        && !run.paused
                        && run.pending_activation.is_some()
                })
                .filter(|run| daemon_owns_run(&state, run))
                .filter_map(|run| {
                    let pa = run.pending_activation.as_ref()?;
                    let uid = run
                        .role_sessions
                        .get(&pa.target_role)?
                        .daemon_session_uid
                        .clone()?;
                    if !state.sessions.contains_key(&uid) {
                        return None;
                    }
                    let workflow = state.workflow_definitions.get(&run.workflow_name).cloned()?;
                    // Worktree via the active (== target) role's bound session.
                    let ws_id = run
                        .active_role
                        .as_deref()
                        .and_then(|active| resolve_role_session_context(&state, &run, active))
                        .map(|c| c.workspace_id);
                    let key = ws_id.as_deref().unwrap_or(run.task_key.as_str());
                    let worktree = state
                        .workspaces
                        .get(key)
                        .and_then(|ws| ws.worktree_path.clone())?;
                    let mut role_engines = BTreeMap::new();
                    for role in workflow.roles.keys() {
                        let st = resolve_role_session_type(&state, &run, role)
                            .unwrap_or_else(|| "claude-code".to_string());
                        role_engines.insert(role.clone(), engine_for_session_type(&st));
                    }
                    Some(Work {
                        run_id: run.run_id.clone(),
                        uid,
                        worktree,
                        workflow,
                        role_engines,
                    })
                })
                .collect()
        };

        let gap_ms = self.finalize_gap_ms.load(Ordering::SeqCst);
        let quiet_ms = self.finalize_quiet_ms.load(Ordering::SeqCst);

        for w in work {
            // Clone the input handle + fanout out of state, drop the state lock
            // before any PTY write (the lock contract from `DaemonSession`).
            let handle_fanout = {
                let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
                state
                    .sessions
                    .get(&w.uid)
                    .map(|s| (s.input_handle(), Arc::clone(&s.fanout)))
            };
            let Some((handle, fanout)) = handle_fanout else {
                continue;
            };
            let tracker = self.tracker_for(&w.uid, &fanout);

            // Advance until the run blocks or completes this tick.
            loop {
                let ctx = crate::workflow::finalize::FinalizeCtx {
                    run_id: &w.run_id,
                    workflow: &w.workflow,
                    worktree: &w.worktree,
                    role_engines: w.role_engines.clone(),
                    now_ms: now_unix_ms(),
                    now_instant: Instant::now(),
                    gap_ms,
                    quiet_window: Duration::from_millis(quiet_ms),
                };
                let guard = tracker.lock().unwrap_or_else(|p| p.into_inner());
                let step = crate::workflow::finalize::advance_finalization(
                    &ctx,
                    &guard,
                    |b| handle.write_and_stamp(b),
                );
                drop(guard);
                match step {
                    Ok(crate::workflow::finalize::FinalizeStep::Advanced(_)) => continue,
                    Ok(_) => break,
                    Err(e) => {
                        eprintln!(
                            "cm-daemon: finalize drainer error for run {}: {}",
                            w.run_id, e
                        );
                        break;
                    }
                }
            }
        }
    }

    /// Get-or-create the per-session `PtyModeTracker`, attaching it to the
    /// session's `PtyByteFanout` on first use so it observes the live terminal
    /// mode + output-quiet timing.
    fn tracker_for(
        &self,
        uid: &str,
        fanout: &Arc<crate::session::PtyByteFanout>,
    ) -> Arc<Mutex<crate::workflow::pty_tracker::PtyModeTracker>> {
        let mut map = self.finalize_trackers.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(t) = map.get(uid) {
            return Arc::clone(t);
        }
        let t = crate::workflow::pty_tracker::spawn_for_fanout(fanout, 80, 24);
        map.insert(uid.to_string(), Arc::clone(&t));
        t
    }

    /// 2c-2-2-b real evaluate. The hot path:
    /// 1. `active_role` must be set.
    /// 2. Not paused.
    /// 3. Daemon owns the active role's session (per
    ///    `TickSnapshot.daemon_owns`).
    /// 4. Workflow definition loaded + has on_idle from active.
    /// 5. Worktree path known.
    /// 6. `assistant_turn_completed_since(engine, wt, sid,
    ///    baseline)` returns true.
    /// 7. Activation prompt renders to non-empty string.
    fn evaluate_snapshot(&self, snap: &TickSnapshot) -> Decision {
        if snap.paused {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::Paused,
            };
        }
        let Some(active) = snap.active_role.as_deref() else {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NoActiveRole,
            };
        };
        if !snap.active_session_found {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::SessionNotFound,
            };
        }
        if !snap.daemon_owns {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::TuiOwns,
            };
        }
        let Some(wf) = snap.workflow.as_ref() else {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NoWorkflowDefinition,
            };
        };
        let Some(transition) = wf.static_transition_on_idle(active) else {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NoOnIdleTransition,
            };
        };
        let Some(worktree) = snap.worktree_path.as_deref() else {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NoWorktreePath,
            };
        };
        let Some(binding) = snap.run.role_sessions.get(active) else {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NoTranscriptId,
            };
        };
        let Some(sid) = binding.current_session_id.as_deref() else {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NoTranscriptId,
            };
        };
        let session_type = snap
            .role_session_types
            .get(active)
            .map(|s| s.as_str())
            .unwrap_or("claude-code");
        let engine = engine_for_session_type(session_type);
        // F1: read baseline from the most recent history entry
        // for the active role — matches TUI's
        // `tui/src/workflow/controller.rs:1087` use of
        // `active_assistant_start_count()`. Pre-fix the daemon
        // read `role_baselines[active].assistant_count` (the
        // LAUNCH-time floor); after the 2nd+ activation that's
        // stale and the gate fires every tick.
        //
        // None means the 10d-2c-1 round-1 gap: state.json has
        // active_role advanced but TUI hasn't yet appended the
        // history entry for it. Skip with NoHistoryEntry; the
        // next tick (after TUI catches up) proceeds normally.
        let Some(baseline) = snap.run.active_assistant_start_count() else {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NoHistoryEntry,
            };
        };
        let fire = crate::workflow::transcript::assistant_turn_completed_since(
            &engine, worktree, sid, baseline,
        );
        if !fire {
            return Decision::Skip {
                run_id: snap.run_id.clone(),
                reason: SkipReason::NotIdle,
            };
        }

        // Render activation prompt. Same shape as the TUI's
        // `WorkflowResolver` path; uses
        // `subsequent_activation_prompt` when the target role has
        // prior activations in `run.history` (matches
        // `tui/src/workflow/controller.rs:1687-1695`).
        let to_role = transition.to.clone();
        let target_role_spec = match wf.roles.get(&to_role) {
            Some(r) => r,
            None => {
                return Decision::Skip {
                    run_id: snap.run_id.clone(),
                    reason: SkipReason::NoOnIdleTransition,
                };
            }
        };
        let prior_activations =
            snap.run.history.iter().filter(|h| h.role == to_role).count();
        let template = if prior_activations > 0 {
            target_role_spec
                .subsequent_activation_prompt
                .as_deref()
                .or(target_role_spec.activation_prompt.as_deref())
        } else {
            target_role_spec.activation_prompt.as_deref()
        };
        // F4: do NOT skip on missing-template or empty-rendered
        // prompt. TUI's path `tui/src/workflow/controller.rs:2089`
        // fires the transition either way: it just skips the
        // PTY write when rendered is empty (and the TUI tail's
        // `template_source = if !supplied_prompt.is_empty()
        // { ... } else { default_template.unwrap_or_default() }`
        // falls back to local template rendering when
        // `args.prompt` is empty). Mirror: produce
        // ActivateStatic with whatever the rendered string ends
        // up as. Pre-fix daemon-owned promptless workflows
        // would WEDGE — gate sees idle, refuses to fire, every
        // tick.
        let mut role_engines = BTreeMap::new();
        for role in wf.roles.keys() {
            let st = snap
                .role_session_types
                .get(role)
                .map(|s| s.as_str())
                .unwrap_or("claude-code");
            role_engines.insert(role.clone(), engine_for_session_type(st));
        }
        let resolver = DaemonWorkflowResolver {
            run: &snap.run,
            worktree_path: snap.worktree_path.as_deref(),
            role_engines,
        };
        let rendered = template
            .map(|t| crate::workflow::template::render(t, &resolver))
            .unwrap_or_default();

        Decision::ActivateStatic {
            run_id: snap.run_id.clone(),
            from_role: active.to_string(),
            to_role,
            rendered_prompt: rendered,
        }
    }

    /// Fire a static `on_idle` transition by invoking the existing
    /// `workflow_transition` handler internally. Reuses all of
    /// 2c-1's flock + try_modify + rollback + event-write
    /// machinery — no new RMW path.
    ///
    /// `role` on the call's params is the OUTGOING `from_role`
    /// (matches "what an MCP caller in that role would write" per
    /// the round-2 review and adjustment 2 of 2c-2-2-b's plan).
    /// Observability shouldn't lie: `event.source = "daemon"`
    /// distinguishes daemon-routed events; `event.role` carries the
    /// active role whose idle triggered the fire.
    fn fire_static_transition(
        &self,
        run_id: &str,
        from_role: &str,
        to_role: &str,
        // Phase 3: the poller no longer pre-renders the activation prompt into
        // the transition. It records the RAW source on `pending_activation`
        // (empty for static fires) and the finalization drainer renders ONCE,
        // at finalization, from the pre-reset/pre-append snapshot. The Decision
        // still carries the poll-time render for observability/tests, but it is
        // NOT threaded into the hand-off — hence the leading underscore.
        _rendered_prompt: &str,
    ) -> Result<(), String> {
        // 10d-2c-2-2-b F2 + F3:
        // - `expected_from`: snapshot's active_role. If state.json
        //   changed between snapshot and apply (concurrent MCP
        //   `workflow_transition`, dynamic role swap), the
        //   handler aborts with Conflict; no double-mutation.
        // - `trigger`: "static_idle" so the TUI tail's history
        //   append uses `TriggerKind::StaticIdle{from_role}`
        //   instead of `TriggerKind::McpTransition` (which would
        //   otherwise be the default for daemon-source events).
        //
        // Also: pre-existing `role` field carries the active role
        // for observability (matches "what an MCP caller in
        // that role would write"). `event.source = "daemon"`
        // already distinguishes daemon-routed events from MCP-
        // routed; the `trigger` field is the additional
        // discriminator for poller-vs-MCP within daemon source.
        let params = serde_json::json!({
            "run_id": run_id,
            "to": to_role,
            "role": from_role,
            // Empty: static fires carry no prompt. Finalization falls back to
            // the target role's (subsequent_)activation_prompt and renders it.
            "prompt": "",
            "expected_from": from_role,
            "trigger": "static_idle",
        });
        let caller = Caller::operator("daemon-poller");
        match crate::control::methods::workflow_transition(
            &self.state,
            &caller,
            &params,
        ) {
            Ok(_) => Ok(()),
            Err((code, msg)) => Err(format!("{:?}: {}", code, msg)),
        }
    }

    /// The actual loop body, run on the spawned thread. Wraps each
    /// `poll_once` in `catch_unwind` so a panic in one tick doesn't
    /// kill the daemon. After a panic we log + sleep + continue.
    fn run_loop(self: Arc<Self>) {
        while !self.shutdown.load(Ordering::SeqCst) {
            let tick_start = Instant::now();

            // Panic safety: a panic in `poll_once` shouldn't crash
            // the daemon. `catch_unwind` requires `UnwindSafe`; the
            // closure borrows `&self` immutably and `poll_once`'s
            // body doesn't share mutable state across the panic
            // boundary (lock guards drop on unwind). The
            // `AssertUnwindSafe` is justified by the above — see
            // `panic_in_poll_once_does_not_crash_daemon` test.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.poll_once()
            }));

            if let Err(panic) = result {
                // Best-effort run_id extraction. The panic payload
                // is opaque; in practice it's a `&str` or `String`
                // from a panic! / unwrap. Visibility goes two ways:
                //   (1) stderr eprintln for live operability — what
                //       an operator tail-f'ing the daemon sees.
                //   (2) `panic_record` field for programmatic
                //       inspection (tests, future health endpoint).
                let msg = panic_payload_to_string(&panic);
                eprintln!(
                    "cm-daemon: workflow poller panicked in poll_once: {} \
                     (continuing — daemon stays up; next tick will retry)",
                    msg,
                );
                let mut rec = self.panic_record.lock().unwrap();
                let count = rec.as_ref().map(|r| r.count + 1).unwrap_or(1);
                *rec = Some(PanicRecord {
                    message: msg,
                    count,
                });
            }

            // Sleep the remainder of the tick interval. If the tick
            // ran long, sleep the minimum so we don't burn CPU on
            // back-to-back ticks. The shutdown flag is re-checked
            // at the top of the loop, so worst-case shutdown
            // latency is `2 × tick_interval` (one tick to notice +
            // one tick in flight).
            let tick_micros = self.tick_micros.load(Ordering::SeqCst);
            let target = Duration::from_micros(tick_micros);
            let elapsed = tick_start.elapsed();
            if elapsed < target {
                let remaining = target - elapsed;
                // Sleep in small chunks so shutdown latency is
                // bounded by ~MIN_TICK_INTERVAL_MICROS rather than
                // the full tick. This matters for the
                // `shutdown_latency_bounded` test under a 250ms
                // production interval — a single `sleep(target)`
                // would otherwise make T5 require a faster tick to
                // pass.
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

/// 10d-2c-2-2-b reviewer-fix: build per-run snapshots from
/// **on-disk state.json files**, not from `state.workflow_runs`.
///
/// The cache stays populated only via daemon's own
/// `workflow_transition` / `workflow_done` handlers (the round-1
/// design: handlers re-load disk under flock on every entry).
/// TUI-side writes — workflow launch, deferred history-entry
/// append (10d-2c-1 round-1 Option A), `sync_role_session_ids`,
/// pause/stop/resume — go straight to disk and don't touch the
/// daemon's cache. Pre-fix the poller saw:
///   1. **Launch invisibility**: TUI-launched runs not in cache →
///      `state.workflow_runs.values()` empty → no decisions →
///      daemon-owned static fires never happen.
///   2. **History-append staleness**: TUI's deferred history
///      append lands on disk but daemon's cache stays stale →
///      poller returns `Skip{NoHistoryEntry}` forever even after
///      disk has the entry.
///
/// Both make the daemon poller functionally inert for actual
/// workflows. Fix: read disk on every `poll_once` via `load_all()`
/// (which the TUI's controller already uses for the same reason).
/// The cost is 1 readdir + N `state.json` reads per tick (~250ms
/// production interval, typically 1-5 active runs); negligible.
///
/// Lock contention: `load_all`'s deserialize takes its own
/// per-file flocks via the file-system layer (state.json.lock).
/// If a handler-side `modify` is mid-flight, the disk read sees
/// the pre-modify or post-modify state but never torn — flock
/// guarantees atomic visibility.
///
/// The `state` parameter is still needed for the read-side gate
/// inputs (`state.sessions`, `state.workspaces`,
/// `state.workflow_definitions`, `state.tui_sessions`) — those
/// ARE properly daemon-owned. Only `workflow_runs` was the
/// non-authoritative cache.
fn collect_snapshots(state: &DaemonState) -> Vec<TickSnapshot> {
    let runs = crate::workflow::run::load_all();
    runs
        .iter()
        .filter(|run| run.is_active())
        .map(|run| {
            // Per-role session_type, derived by walking BOTH
            // Round-5 review-round-5 F1: per-role session_type
            // resolution via the shared `resolve_role_session_context`
            // helper's three-tier fallback (uid → daemon-tag →
            // TUI-tag). Pre-r5-r5 this walked sessions for tag
            // matches only — daemon-owned sessions without
            // `set_workflow_context` tags would have no entry
            // here, the engine would default to ClaudeCode, and a
            // Codex transcript would never be read correctly. The
            // helper aligns this signal with `daemon_owns_run`.
            let mut role_session_types: BTreeMap<String, String> =
                BTreeMap::new();
            for role in run.role_sessions.keys() {
                if let Some(st) = resolve_role_session_type(state, run, role) {
                    role_session_types.insert(role.clone(), st);
                }
            }

            let active = run.active_role.as_deref();
            // Round-5 F1: `active_session_found` covers BOTH the
            // tag-based path (role_session_types is built from
            // workflow_run_id/workflow_role match — for the
            // engine-derivation read path) AND the durable
            // uid-binding path (the new ownership signal).
            // Pre-r5 this only checked tags, so a daemon
            // session without tags but with a proper
            // `daemon_session_uid` binding would skip with
            // SessionNotFound and never fire.
            let active_session_found = active
                .map(|r| {
                    role_session_types.contains_key(r)
                        || run
                            .role_sessions
                            .get(r)
                            .and_then(|b| b.daemon_session_uid.as_deref())
                            .map(|uid| state.sessions.contains_key(uid))
                            .unwrap_or(false)
                })
                .unwrap_or(false);
            let daemon_owns = daemon_owns_run(state, run);

            // Worktree resolution via session tags, NOT
            // `run.task_key`. The session bound to this run +
            // active role carries the authoritative
            // `workspace_id` — `run.task_key` is the
            // launch-time workspace_id and can drift (the
            // session might rebind to a different workspace
            // via TUI flows). TUI's `fire_transition` uses
            // the same approach:
            // `locate_workflow_session` → `workspaces[ti].worktree_path`
            // at `tui/src/workflow/controller.rs:1709, 1724`.
            //
            // Round-5 F1 + review-round-5 F1: workspace
            // resolution via the shared helper's three-tier
            // fallback (uid → daemon-tag → TUI-tag). Round-3 F3
            // introduced session-tag-based resolution; round-5
            // review extends to the uid-first path. Falls back
            // to `run.task_key` only when no session is bound at
            // all (the pre-spawn / pre-snapshot window).
            let session_workspace_id: Option<String> = run
                .active_role
                .as_deref()
                .and_then(|active| resolve_role_session_context(state, run, active))
                .map(|ctx| ctx.workspace_id);
            let workspace_lookup_key = session_workspace_id
                .as_deref()
                .unwrap_or(run.task_key.as_str());
            let worktree_path = state
                .workspaces
                .get(workspace_lookup_key)
                .and_then(|ws| ws.worktree_path.clone());

            let workflow = state.workflow_definitions.get(&run.workflow_name).cloned();

            TickSnapshot {
                run_id: run.run_id.clone(),
                workflow_name: run.workflow_name.clone(),
                active_role: run.active_role.clone(),
                paused: run.paused,
                run: run.clone(),
                worktree_path,
                role_session_types,
                daemon_owns,
                active_session_found,
                workflow,
            }
        })
        .collect()
}

/// 2c-2-2-b ownership gate (round-5 F1: uid-based, durable signal).
///
/// Daemon owns a run iff `run.role_sessions[active].daemon_session_uid`
/// is set AND `state.sessions` contains that uid.
///
/// **Why uid-based over tag-based**: pre-r5 the gate walked
/// `state.sessions` for a session tagged with `workflow_run_id` +
/// `workflow_role`. Those tags are populated by the best-effort
/// `session.set_workflow_context` RPC — if that push fails or
/// hasn't landed yet, the daemon session lacks tags and the
/// gate would say "TUI owns" while the TUI's own gate (checking
/// `TerminalSession.daemon_session_uid`) said "daemon owns".
/// Result: workflow stalled, neither poller fires.
///
/// Post-r5: the gate uses
/// `run.role_sessions[active].daemon_session_uid`, which the TUI
/// populates at every `current_session_id` write site, then
/// state.json membership check. Both signals are durable
/// (workflow record on disk, daemon registry in memory). The
/// `set_workflow_context` tags become defense-in-depth for
/// `lookup_session_any` (auth path), not load-bearing for
/// ownership.
///
/// TUI's equivalent gate (`controller.rs:1121` skip when
/// `session.daemon_session_uid.is_some()`) checks the SAME
/// session-uid signal, just from a different angle. The
/// two-poller agreement invariant rests on this signal
/// alignment.
pub fn daemon_owns_run(state: &DaemonState, run: &WorkflowRun) -> bool {
    let Some(active) = run.active_role.as_deref() else {
        return false;
    };
    let Some(binding) = run.role_sessions.get(active) else {
        return false;
    };
    let Some(uid) = binding.daemon_session_uid.as_deref() else {
        return false;
    };
    state.sessions.contains_key(uid)
}

/// Round-5 (review round 5) shared helper: resolve the
/// authoritative session context (session_type + workspace_id) for
/// a workflow run's role. Three-tier fallback aligned with the
/// round-5 F1 ownership signal:
///   1. Uid-based: `run.role_sessions[role].daemon_session_uid` →
///      `state.sessions.get(uid)`. Durable, matches what
///      `daemon_owns_run` uses.
///   2. Daemon tag-based: walk `state.sessions` for a session
///      whose `workflow_run_id` + `workflow_role` match.
///      Defense-in-depth for sessions whose
///      `session.set_workflow_context` push landed.
///   3. TUI tag-based: walk `state.tui_sessions` for a tag match;
///      derive workspace_id from `task_id` via `state.bindings`
///      (TUI snapshots don't carry workspace_id directly).
///
/// Returns None if no session can be resolved at all. Used by:
///   - `collect_snapshots` for per-role engine derivation +
///     worktree resolution (pre-r5 used tags only → untagged
///     daemon-owned sessions defaulted to ClaudeCode + wrong
///     worktree).
///   - `crate::control::methods::capture_outgoing_last_message`
///     for the closing history entry's `last_message` capture
///     (same pre-r5 bug class).
pub fn resolve_role_session_context(
    state: &DaemonState,
    run: &WorkflowRun,
    role: &str,
) -> Option<RoleSessionContext> {
    // Tier 1: uid-based.
    if let Some(uid) = run
        .role_sessions
        .get(role)
        .and_then(|b| b.daemon_session_uid.as_deref())
    {
        if let Some(s) = state.sessions.get(uid) {
            return Some(RoleSessionContext {
                session_type: s.session_type.clone(),
                workspace_id: s.workspace_id.clone(),
            });
        }
    }
    // Tier 2: daemon tag-based.
    if let Some(s) = state.sessions.values().find(|s| {
        s.workflow_run_id.as_deref() == Some(&run.run_id)
            && s.workflow_role.as_deref() == Some(role)
    }) {
        return Some(RoleSessionContext {
            session_type: s.session_type.clone(),
            workspace_id: s.workspace_id.clone(),
        });
    }
    // Tier 3: TUI tag-based with task_id → workspace derivation.
    if let Some(ts) = state.tui_sessions.values().find(|s| {
        s.workflow_run_id.as_deref() == Some(&run.run_id)
            && s.workflow_role.as_deref() == Some(role)
    }) {
        let session_type = ts
            .session_type
            .clone()
            .unwrap_or_else(|| "claude-code".to_string());
        // `TuiSessionSnapshot` doesn't carry workspace_id; derive
        // via task_id binding.
        let workspace_id = ts
            .task_id
            .as_deref()
            .and_then(|tid| state.bindings.get(tid).cloned());
        if let Some(ws_id) = workspace_id {
            return Some(RoleSessionContext {
                session_type,
                workspace_id: ws_id,
            });
        }
    }
    None
}

/// Output of [`resolve_role_session_context`]. Carries the two
/// pieces of state most callers need: session_type (for engine
/// derivation via `engine_for_session_type`) and workspace_id
/// (for worktree lookup via `state.workspaces`).
#[derive(Debug, Clone)]
pub struct RoleSessionContext {
    pub session_type: String,
    pub workspace_id: String,
}

/// Type-only variant: resolves just `session_type` via the same
/// three-tier fallback as [`resolve_role_session_context`] but
/// does NOT require workspace_id to be derivable. Used by
/// `collect_snapshots` for engine derivation, which doesn't need
/// the workspace — that path is handled separately. Separating
/// the two lets a TUI session without task_id binding still
/// surface its engine to the resolver (workspace-id derivation
/// would fail for it).
pub fn resolve_role_session_type(
    state: &DaemonState,
    run: &WorkflowRun,
    role: &str,
) -> Option<String> {
    if let Some(uid) = run
        .role_sessions
        .get(role)
        .and_then(|b| b.daemon_session_uid.as_deref())
    {
        if let Some(s) = state.sessions.get(uid) {
            return Some(s.session_type.clone());
        }
    }
    if let Some(s) = state.sessions.values().find(|s| {
        s.workflow_run_id.as_deref() == Some(&run.run_id)
            && s.workflow_role.as_deref() == Some(role)
    }) {
        return Some(s.session_type.clone());
    }
    if let Some(ts) = state.tui_sessions.values().find(|s| {
        s.workflow_run_id.as_deref() == Some(&run.run_id)
            && s.workflow_role.as_deref() == Some(role)
    }) {
        return Some(
            ts.session_type
                .clone()
                .unwrap_or_else(|| "claude-code".to_string()),
        );
    }
    None
}

/// Map a `TerminalSession.session_type` string to an `Engine`.
/// Mirrors `tui/src/app.rs::engine_for_session_type` exactly — the
/// parity test pins this. Default is ClaudeCode for any unknown
/// value (matches the TUI's permissive default).
fn engine_for_session_type(session_type: &str) -> Engine {
    match session_type {
        "codex" => Engine::Codex,
        _ => Engine::ClaudeCode,
    }
}

/// Daemon-side equivalent of `tui/src/workflow/controller.rs::WorkflowResolver`.
/// Implements [`crate::workflow::template::RoleResolver`] for the
/// activation-prompt template engine.
///
/// **Single-source contract**: this resolver's outputs must equal
/// the TUI's resolver outputs for the same logical inputs (workflow
/// definition, run, role, session uid, worktree). Pinned by the
/// `daemon_and_tui_resolvers_produce_identical_output` parity test
/// (lives on the TUI side, in `tui/src/workflow/controller.rs`'s
/// test module, since that's the only place where both impls are
/// importable in the same crate).
///
/// The differences from the TUI resolver are READ-PATH-ONLY:
/// - TUI reads from `App.workspaces[ti].sessions[si].transcript_id`
///   + `App.workspaces[ti].worktree_path`.
/// - Daemon reads from `DaemonState.workspaces[task_key].worktree_path`
///   + the snapshot's `role_session_types` map.
///
/// Both end up calling the same `crate::workflow::transcript::*`
/// functions, so the actual transcript-read output is byte-for-byte
/// identical given identical inputs.
pub struct DaemonWorkflowResolver<'a> {
    pub run: &'a WorkflowRun,
    pub worktree_path: Option<&'a Path>,
    pub role_engines: BTreeMap<String, Engine>,
}

impl<'a> DaemonWorkflowResolver<'a> {
    fn lookup(&self, role: &str) -> Option<(Engine, &'a Path, &'a str)> {
        let engine = self.role_engines.get(role).cloned()?;
        let binding = self.run.role_sessions.get(role)?;
        let sid = binding.current_session_id.as_deref()?;
        let worktree = self.worktree_path?;
        Some((engine, worktree, sid))
    }
}

impl<'a> crate::workflow::template::RoleResolver for DaemonWorkflowResolver<'a> {
    fn user_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let offset = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.user_count)
            .unwrap_or(0);
        crate::workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            crate::workflow::transcript::MessageKind::User,
        )
        .into_iter()
        .skip(offset)
        .collect()
    }

    fn assistant_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let offset = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.assistant_count)
            .unwrap_or(0);
        crate::workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            crate::workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .skip(offset)
        .collect()
    }

    /// `{{ roles.<role>.this_turn }}` — everything the role has said since its
    /// most recent activation. Mirrors the TUI's `WorkflowResolver`
    /// (`tui/src/workflow/controller.rs:298`): slice `list_messages` from the
    /// role's latest history entry's `text_messages_at_start` (the text-bearing
    /// count snapshotted at activation, NOT the `count_messages` turn count).
    /// Without this override the default impl falls back to `assistant_messages`
    /// (sliced by `role_baselines`), which is the LAUNCH offset, not the
    /// per-activation offset — so the manager's prompt would surface the whole
    /// run instead of just this round.
    fn assistant_since_activation(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let offset = self
            .run
            .history
            .iter()
            .rev()
            .find(|h| h.role == role)
            .map(|h| h.text_messages_at_start)
            .unwrap_or(0);
        crate::workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            crate::workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .skip(offset)
        .collect()
    }

    fn prior_user_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let baseline = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.user_count)
            .unwrap_or(0);
        crate::workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            crate::workflow::transcript::MessageKind::User,
        )
        .into_iter()
        .take(baseline)
        .collect()
    }

    fn prior_assistant_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let baseline = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.assistant_count)
            .unwrap_or(0);
        crate::workflow::transcript::list_messages(
            &engine,
            wt,
            sid,
            crate::workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .take(baseline)
        .collect()
    }

    fn latest_plan(&self, role: &str) -> Option<String> {
        if let Some(plan) = self.run.role_plans.get(role) {
            if !plan.is_empty() {
                return Some(plan.clone());
            }
        }
        let (engine, wt, sid) = self.lookup(role)?;
        crate::workflow::transcript::latest_plan(&engine, wt, sid)
    }

    fn goal(&self) -> Option<String> {
        self.run.goal.clone()
    }

    /// `{{ rejected_findings }}` — the manager's dismissed findings, surfaced to
    /// the reviewer so it stops re-raising them. Mirrors the TUI's
    /// `WorkflowResolver` (`tui/src/workflow/controller.rs:380`). Without this
    /// override the trait default returns empty, so the daemon-rendered reviewer
    /// prompt would drop the `{{ rejected_findings }}` section entirely.
    fn rejected_findings(&self) -> Vec<String> {
        self.run
            .rejected_findings
            .iter()
            .map(|r| r.text.clone())
            .collect()
    }
}

/// Best-effort decode of a `catch_unwind` payload. Most panics carry
/// a `&'static str` or `String`; anything else falls back to a
/// generic marker so the log line is at least useful for "where" if
/// not "why."
fn panic_payload_to_string(
    payload: &Box<dyn std::any::Any + Send + 'static>,
) -> String {
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
    use crate::workflow::run::{
        MessageBaseline, RoleBinding, WorkflowRun,
    };
    use std::collections::BTreeMap;

    /// Build a `DaemonState` with one active workflow run AND
    /// persist that run to disk (`~/.cm/workflow-runs/<id>/state.json`).
    /// The disk save is required because the reviewer-fix
    /// switched `collect_snapshots` to read disk via
    /// `workflow::run::load_all()` instead of the
    /// `state.workflow_runs` cache. Tests that build runs only
    /// in-memory would see empty snapshots otherwise.
    ///
    /// CALLER REQUIREMENT: must hold `crate::test_support::env_lock()`
    /// and have `HOME` pointing at a tempdir BEFORE calling this —
    /// the disk save lands under `$HOME/.cm/workflow-runs/`.
    /// Round-5 F1 test helper: after spawning a daemon session,
    /// patch the run's `role_sessions[role].daemon_session_uid`
    /// so the new uid-based ownership gate sees it. Mirrors what
    /// `tui/src/workflow/controller.rs::launch_workflow` does in
    /// production (co-writes the uid alongside `current_session_id`).
    fn bind_daemon_uid_to_role(run_id: &str, role: &str, uid: &str) {
        crate::workflow::run::modify(run_id, |r| {
            if let Some(b) = r.role_sessions.get_mut(role) {
                b.daemon_session_uid = Some(uid.to_string());
            }
        })
        .expect("bind daemon_session_uid for test");
    }

    fn make_state_with_one_active_run(run_id: &str) -> Arc<Mutex<DaemonState>> {
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: Some("sid-worker".to_string()),
                daemon_session_uid: None,
            },
        );
        let mut baselines = BTreeMap::new();
        baselines.insert(
            "worker".to_string(),
            MessageBaseline {
                user_count: 0,
                assistant_count: 0,
            },
        );
        let run = WorkflowRun::new(
            run_id.to_string(),
            "feedback".to_string(),
            "/tmp/wf-poller-test".to_string(),
            roles,
            "worker".to_string(),
            baselines,
            None,
            BTreeMap::new(),
            0,
        );
        // Persist to disk so `load_all()` in `collect_snapshots`
        // surfaces it. Pre-reviewer-fix the cache alone was
        // enough — that was the bug.
        crate::workflow::run::save(&run).expect("save run to disk");
        let mut state = DaemonState::default();
        state.workflow_runs.insert(run_id.to_string(), run);
        Arc::new(Mutex::new(state))
    }

    /// R11 — A run whose active role's session isn't visible in
    /// either `state.sessions` (daemon-spawned) OR
    /// `state.tui_sessions` (TUI-only snapshot) gets
    /// `SessionNotFound`. This is the "TUI snapshot push lag"
    /// surface from the 2c-2-2-b proposal. Self-resolves on next
    /// snapshot push.
    #[test]
    fn poll_once_returns_session_not_found_when_no_snapshot_registered() {
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());
        let state = make_state_with_one_active_run("r1");
        let poller = WorkflowPoller::new(state);
        let decisions = poller.poll_once();
        assert_eq!(decisions.len(), 1);
        assert!(matches!(
            &decisions[0],
            Decision::Skip { run_id, reason: SkipReason::SessionNotFound }
                if run_id == "r1"
        ), "expected Skip{{SessionNotFound}}, got {:?}", decisions[0]);
    }

    /// T2 daemon-side companion: a run whose active role is bound
    /// to a TUI-only session (visible only via
    /// `state.tui_sessions`, not in `state.sessions`) gets
    /// `Skip{TuiOwns}`. The TUI controller fires for this run; the
    /// daemon poller stays out.
    #[test]
    fn poll_once_returns_tui_owns_when_active_role_session_is_tui_only() {
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());
        let state = make_state_with_one_active_run("r1");
        {
            let mut s = state.lock().unwrap();
            s.tui_sessions.insert(
                "tui-uid-1".to_string(),
                crate::state::TuiSessionSnapshot {
                    uid: "tui-uid-1".to_string(),
                    task_id: None,
                    label: Some("worker".to_string()),
                    session_type: Some("claude-code".to_string()),
                    hidden: false,
                    workflow_run_id: Some("r1".to_string()),
                    workflow_role: Some("worker".to_string()),
                },
            );
            s.tui_sessions_pushed = true;
        }
        let poller = WorkflowPoller::new(state);
        let decisions = poller.poll_once();
        assert_eq!(decisions.len(), 1);
        assert!(matches!(
            &decisions[0],
            Decision::Skip { run_id, reason: SkipReason::TuiOwns }
                if run_id == "r1"
        ), "expected Skip{{TuiOwns}}, got {:?}", decisions[0]);
    }

    #[test]
    fn poll_once_returns_empty_when_no_active_runs() {
        // Round-5 F2: wrap in env_lock + tempdir HOME — the
        // reviewer-fix in round-3 switched `collect_snapshots`
        // to `workflow::run::load_all()` which reads
        // `$HOME/.cm/workflow-runs/`. Pre-isolation this test
        // saw the dev's real workflow-runs dir when run
        // standalone and would assert non-empty.
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let state = Arc::new(Mutex::new(DaemonState::default()));
        let poller = WorkflowPoller::new(state);
        let decisions = poller.poll_once();
        assert!(decisions.is_empty());
    }

    /// T5: shutdown latency bounded.
    /// Start the loop with a 10ms tick, signal shutdown
    /// immediately, assert the thread joins within a generous
    /// upper bound (200ms — covers OS scheduler jitter while still
    /// catching a "shutdown flag never checked" regression).
    #[test]
    fn shutdown_latency_bounded_by_tick_interval() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let poller = Arc::new(WorkflowPoller::new(state));
        poller.set_tick_interval_for_test(10_000); // 10ms
        poller.start().expect("spawn poller thread under test load");
        let t0 = Instant::now();
        poller.shutdown();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "shutdown took {:?} — flag should be checked at sleep-chunk granularity \
             (~1ms), not full tick boundary; if this regresses, the run_loop's \
             chunked-sleep got removed",
            elapsed,
        );
    }

    /// T4 part A: `panic_payload_to_string` recovers the message
    /// from a `&'static str` panic and from a `String` panic. The
    /// production `run_loop` uses this to populate `panic_record`
    /// and the stderr line.
    #[test]
    fn panic_payload_extracts_static_str_and_string() {
        let s_payload = std::panic::catch_unwind(|| {
            panic!("static str panic");
        })
        .unwrap_err();
        assert_eq!(panic_payload_to_string(&s_payload), "static str panic");

        let owned = "owned string panic".to_string();
        let str_payload = std::panic::catch_unwind(move || {
            panic!("{}", owned);
        })
        .unwrap_err();
        assert_eq!(
            panic_payload_to_string(&str_payload),
            "owned string panic"
        );
    }

    /// T4 part B: a panic inside the `run_loop` thread doesn't
    /// kill the daemon AND is captured into `panic_record` for
    /// programmatic visibility. We can't easily inject a panic
    /// into the *real* poll loop without a test hook, so this
    /// test exercises the `catch_unwind` + `panic_record`
    /// machinery directly via a hand-rolled equivalent of the
    /// `run_loop` panic branch — same code path, same shape, run
    /// inline so the assertions are deterministic.
    ///
    /// Stderr emission is verified by the standalone
    /// `panic_log_line_format` test below — separating
    /// "deterministic state record" from "stderr format" avoids
    /// fighting libtest's stderr capture.
    #[test]
    fn panic_in_poll_once_is_captured_into_panic_record() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let poller = WorkflowPoller::new(state);
        assert!(
            poller.panic_record().is_none(),
            "no panics before any poll_once",
        );

        // Simulate two panics, asserting the count climbs and the
        // most recent message wins (matches `run_loop`'s
        // overwrite-with-newest semantics).
        for (i, msg) in ["first synthetic panic", "second synthetic panic"]
            .iter()
            .enumerate()
        {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || panic!("{}", *msg),
            ));
            if let Err(panic) = result {
                let extracted = panic_payload_to_string(&panic);
                let mut rec = poller.panic_record.lock().unwrap();
                let count = rec.as_ref().map(|r| r.count + 1).unwrap_or(1);
                *rec = Some(PanicRecord {
                    message: extracted,
                    count,
                });
            }
            let rec = poller.panic_record().expect("record set after panic");
            assert_eq!(rec.count, (i + 1) as u64);
            assert_eq!(rec.message, *msg);
        }

        // Daemon-survival check: poller is still usable after two
        // captured panics. `poll_once` returns normally.
        let _decisions = poller.poll_once();
    }

    /// T4 part C (visibility): the stderr line format includes
    /// the panic message and the "daemon stays up" reassurance.
    /// Asserted on the string `run_loop` would emit, not by
    /// capturing stderr — that fight-with-libtest path was the
    /// original T4 shape and produced a flaky test. The format
    /// IS exercised at runtime by `run_loop` itself; this test
    /// pins that the format string includes both required
    /// substrings so a future eprintln refactor that drops the
    /// "daemon stays up" reassurance gets caught.
    #[test]
    fn panic_log_line_format_includes_message_and_survival_reassurance() {
        let msg = "test panic with run_id=r-panic";
        let line = format!(
            "cm-daemon: workflow poller panicked in poll_once: {} \
             (continuing — daemon stays up; next tick will retry)",
            msg,
        );
        assert!(line.contains("workflow poller panicked"));
        assert!(line.contains("r-panic"));
        assert!(line.contains("daemon stays up"));
    }

    /// Lock-contention pattern: a long-running RPC handler
    /// holding `state.lock()` doesn't block `poll_once`'s
    /// transcript I/O. The pattern is: collect snapshots under
    /// the lock (sub-millisecond), drop, do I/O lock-free.
    ///
    /// We simulate by spawning a thread that grabs `state.lock()`
    /// and sleeps 100ms while holding it, then asserts the
    /// poller's `poll_once` blocks ONLY on snapshot collection
    /// (which waits for the lock) but the I/O phase would have
    /// completed already if the lock-drop is correct. The pattern
    /// the test verifies: `poll_once` releases the lock before
    /// returning.
    #[test]
    fn poll_once_releases_state_lock_before_returning() {
        let state = make_state_with_one_active_run("r-contention");
        let poller = WorkflowPoller::new(Arc::clone(&state));

        // First, run a baseline `poll_once` so the snapshot phase
        // is warm; this proves the assertion below isn't a
        // first-call artifact.
        let _ = poller.poll_once();

        // Hold the state lock from another thread while
        // `poll_once` runs. If `poll_once` correctly drops the
        // lock before returning, a concurrent attempt to lock
        // (after `poll_once` returns) will succeed without
        // contention.
        let state_clone = Arc::clone(&state);
        let lock_holder = std::thread::spawn(move || {
            let guard = state_clone.lock().unwrap();
            std::thread::sleep(Duration::from_millis(50));
            drop(guard);
        });
        // Give the holder a moment to actually grab the lock.
        std::thread::sleep(Duration::from_millis(10));

        // poll_once should block waiting for the lock (snapshot
        // phase), then return promptly after holder releases.
        let t0 = Instant::now();
        let _decisions = poller.poll_once();
        let elapsed = t0.elapsed();

        lock_holder.join().unwrap();

        // Lock holder slept 50ms; we waited 10ms before calling
        // poll_once, so the lock should release ~40ms after our
        // call. We give a generous upper bound (200ms) — the
        // narrow assertion is "this completed, didn't deadlock."
        assert!(
            elapsed < Duration::from_millis(200),
            "poll_once took {:?} — should release lock immediately \
             after snapshot phase; if this regresses, the I/O \
             phase is running inside the lock guard",
            elapsed,
        );

        // After poll_once returns, the state lock must be free —
        // we should be able to acquire it without blocking. This
        // is the actual lock-drop invariant.
        let t1 = Instant::now();
        let _g = state.lock().unwrap();
        let lock_acquire = t1.elapsed();
        assert!(
            lock_acquire < Duration::from_millis(5),
            "state lock should be free after poll_once returns; \
             acquiring took {:?}",
            lock_acquire,
        );
    }

    /// T1 — Daemon-owned active role fires `ActivateStatic` with a
    /// rendered prompt. Uses `disable_apply_for_test(true)` so the
    /// test only asserts the GATE output; the full handler is
    /// exercised by the existing `workflow_transition_*` suite.
    ///
    /// Setup mirrors the production hot path:
    /// - state.sessions has a daemon-tagged session for "worker"
    ///   bound to run_id "r1" (so `daemon_owns_run` returns true).
    /// - state.workspaces has a worktree path matching run.task_key.
    /// - state.workflow_definitions has a "feedback" workflow with
    ///   on_idle worker→reviewer + an activation_prompt template.
    /// - A claude-shape JSONL with one complete assistant turn so
    ///   `assistant_turn_completed_since` returns true.
    #[test]
    fn poll_once_fires_activate_static_when_daemon_owns_idle_worker() {
        use crate::session::DaemonSession;
        // Hold env_lock for the whole test so HOME mutation is
        // serialized against other HOME-touching tests. See
        // `with_home_lock` doc.
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());
        let home = _tmp_home.path();

        // Worktree with a claude transcript for "worker" — one
        // complete assistant turn (end_turn). count_messages
        // returns 1; baseline is 0; gate fires.
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r1");
        {
            let mut s = state.lock().unwrap();
            // Workspace with worktree_path pointing at our tempdir.
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            // Workflow definition: worker→reviewer on idle, with a
            // simple activation prompt template.
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("worker start".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            roles.insert(
                "reviewer".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some(
                        "review {{ roles.worker.last_message }}".to_string(),
                    ),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );
            // Daemon-tagged session for "worker" → gate returns
            // true. Spawn via /bin/sleep with workflow context.
            let mut sp = crate::session::SpawnParams::new(
                "ts-worker",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.session_type = "claude-code".to_string();
            sp.workflow_run_id = Some("r1".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn /bin/sleep");
            s.sessions.insert("ts-worker".to_string(), ds);
        }
        // Round-5 F1: bind the daemon uid into the run's role
        // binding so the new uid-based gate fires.
        bind_daemon_uid_to_role("r1", "worker", "ts-worker");

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);
        let decisions = poller.poll_once();
        assert_eq!(decisions.len(), 1, "expected one decision for r1");
        match &decisions[0] {
            Decision::ActivateStatic {
                run_id,
                from_role,
                to_role,
                rendered_prompt,
            } => {
                assert_eq!(run_id, "r1");
                assert_eq!(from_role, "worker");
                assert_eq!(to_role, "reviewer");
                // Template rendered with worker's last assistant
                // message ("done"). Pin substrings so a future
                // template-engine refactor catches divergence.
                assert!(
                    rendered_prompt.starts_with("review "),
                    "rendered prompt should start with 'review ': {:?}",
                    rendered_prompt,
                );
                assert!(
                    rendered_prompt.contains("done"),
                    "rendered prompt should contain worker's last message \
                     'done': {:?}",
                    rendered_prompt,
                );
            }
            other => panic!("expected ActivateStatic, got {:?}", other),
        }
    }

    /// Phase 3 headless end-to-end (doc/daemon-side-workflow-orchestration.md):
    /// a daemon-owned worker goes idle, `poll_once` fires worker->reviewer, and
    /// the delivery drainer (same `poll_once`) resets the FRESH reviewer,
    /// appends its history entry with both counts 0, delivers the rendered
    /// prompt, and — after the reviewer produces a turn — rebinds the new
    /// transcript. No TUI process anywhere.
    #[test]
    fn poll_once_finalizes_fresh_reviewer_headlessly_end_to_end() {
        use crate::session::{DaemonSession, SpawnParams};
        use crate::workflow::toml_schema::{Context, Role, Transition, TriggerOn, Workflow};

        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        std::env::set_var("HOME", _tmp_home.path());
        let home = _tmp_home.path();

        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = home.join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        // Worker idle with one complete assistant turn -> gate fires.
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"implemented the thing"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("rE2E");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);

            let mut roles = BTreeMap::new();
            roles.insert("worker".to_string(), Role {
                engine: Engine::ClaudeCode, context: Context::Persistent,
                activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false,
            });
            roles.insert("reviewer".to_string(), Role {
                engine: Engine::ClaudeCode, context: Context::Fresh,
                activation_prompt: Some("Review this: {{ roles.worker.last_message }}".to_string()),
                subsequent_activation_prompt: None, needs_mcp: false,
            });
            // Manager exists so the reviewer HAS an on_idle transition — that
            // way the reviewer's inert gate reports NoTranscriptId (sid unbound),
            // not NoOnIdleTransition. Manager itself is never reached in this test.
            roles.insert("manager".to_string(), Role {
                engine: Engine::ClaudeCode, context: Context::Persistent,
                activation_prompt: Some("decide".to_string()), subsequent_activation_prompt: None, needs_mcp: false,
            });
            s.workflow_definitions.insert("feedback".to_string(), Workflow {
                name: "feedback".to_string(), description: String::new(), roles,
                role_order: vec!["worker".to_string(), "reviewer".to_string(), "manager".to_string()],
                transitions: vec![
                    Transition { from: "worker".to_string(), on: TriggerOn::Idle, to: "reviewer".to_string() },
                    Transition { from: "reviewer".to_string(), on: TriggerOn::Idle, to: "manager".to_string() },
                ],
            });

            for (uid, role) in [("ts-worker", "worker"), ("ts-reviewer", "reviewer")] {
                let mut sp = SpawnParams::new(uid, role, "/bin/sleep");
                sp.args = vec!["60".to_string()];
                sp.workspace_id = "/tmp/wf-poller-test".to_string();
                sp.session_type = "claude-code".to_string();
                sp.workflow_run_id = Some("rE2E".to_string());
                sp.workflow_role = Some(role.to_string());
                let ds = DaemonSession::spawn(sp).expect("spawn /bin/sleep");
                s.sessions.insert(uid.to_string(), ds);
            }
        }
        bind_daemon_uid_to_role("rE2E", "worker", "ts-worker");
        // Add the reviewer role binding (fresh, bound to its daemon session).
        crate::workflow::run::modify("rE2E", |r| {
            r.role_sessions.insert("reviewer".to_string(), RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: Some("sid-reviewer-old".to_string()),
                daemon_session_uid: Some("ts-reviewer".to_string()),
            });
        })
        .unwrap();

        let poller = WorkflowPoller::new(state);
        poller.set_finalize_timing_for_test(0, 0); // instant, gate-free delivery

        // Tick 1: fire worker->reviewer, then finalize up to rebind-pending.
        poller.poll_once();

        let run = crate::workflow::run::load_one("rE2E").unwrap();
        assert_eq!(run.active_role.as_deref(), Some("reviewer"), "transition fired");
        let pa = run.pending_activation.as_ref().expect("finalizing");
        assert_eq!(
            pa.phase,
            crate::workflow::run::ActivationPhase::RebindPending,
            "delivered, awaiting rebind"
        );
        // Fresh reviewer history entry: both counts 0 (post-/clear, NOT worker's).
        let entry = run.history.iter().rev().find(|h| h.role == "reviewer").unwrap();
        assert_eq!(entry.assistant_count_at_start, 0);
        assert_eq!(entry.text_messages_at_start, 0);
        assert!(entry.session_id.is_none(), "session_id pending pre-rebind");
        // current_session_id cleared by the fresh reset; gate inert.
        assert!(run.role_sessions["reviewer"].current_session_id.is_none());
        // The gate stays inert via NoTranscriptId (current_session_id unbound),
        // NOT NoHistoryEntry — the history entry WAS appended this hand-off.
        let decisions = poller.poll_once();
        let reason = decisions.iter().find_map(|d| match d {
            Decision::Skip { run_id, reason } if run_id == "rE2E" => Some(reason),
            _ => None,
        });
        assert!(
            matches!(reason, Some(SkipReason::NoTranscriptId)),
            "gate inert via NoTranscriptId, never NoHistoryEntry; got {:?}",
            reason
        );

        // The reviewer produces its turn (writes a NEW transcript).
        std::fs::write(
            proj.join("sid-reviewer-new.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"found a bug"}]}}"##,
        )
        .unwrap();

        // Tick 2: discovery rebinds, patches the entry, finalization completes.
        poller.poll_once();

        let run = crate::workflow::run::load_one("rE2E").unwrap();
        assert!(run.pending_activation.is_none(), "finalization done");
        assert_eq!(
            run.role_sessions["reviewer"].current_session_id.as_deref(),
            Some("sid-reviewer-new"),
            "rebound to the reviewer's NEW transcript"
        );
        let entry = run.history.iter().rev().find(|h| h.role == "reviewer").unwrap();
        assert_eq!(entry.session_id.as_deref(), Some("sid-reviewer-new"), "entry patched");
        // The reviewer's assistant turn count incremented past its (0) baseline.
        assert!(
            crate::workflow::transcript::assistant_turn_completed_since(
                &Engine::ClaudeCode, &wt, "sid-reviewer-new", 0,
            ),
            "reviewer advanced a turn after receiving the prompt"
        );
    }

    /// Phase 3 (reviewer High #2): the delivery drainer must NOT drive a
    /// PAUSED run. `is_active()` returns true for Paused, but a paused run
    /// (e.g. the user typed into the participant session) must not get
    /// /clear/body/Enter shoved into its PTY. Asserts the pending_activation is
    /// untouched while paused, then advances once unpaused.
    #[test]
    fn drain_finalizations_skips_paused_runs() {
        use crate::session::{DaemonSession, SpawnParams};
        use crate::workflow::run::{ActivationPhase, PendingActivation, TriggerKind};
        use crate::workflow::toml_schema::{Context, Role, Transition, TriggerOn, Workflow};

        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        std::env::set_var("HOME", _tmp_home.path());
        let home = _tmp_home.path();
        let wt = home.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = home.join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("sid-rev.jsonl"), "").unwrap();

        let state = make_state_with_one_active_run("rPause");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            let mut roles = BTreeMap::new();
            roles.insert("worker".to_string(), Role {
                engine: Engine::ClaudeCode, context: Context::Persistent,
                activation_prompt: None, subsequent_activation_prompt: None, needs_mcp: false,
            });
            roles.insert("reviewer".to_string(), Role {
                engine: Engine::ClaudeCode, context: Context::Fresh,
                activation_prompt: Some("review".to_string()),
                subsequent_activation_prompt: None, needs_mcp: false,
            });
            s.workflow_definitions.insert("feedback".to_string(), Workflow {
                name: "feedback".to_string(), description: String::new(), roles,
                role_order: vec!["worker".to_string(), "reviewer".to_string()],
                transitions: vec![Transition {
                    from: "worker".to_string(), on: TriggerOn::Idle, to: "reviewer".to_string(),
                }],
            });
            let mut sp = SpawnParams::new("ts-rev", "reviewer", "/bin/sleep");
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.session_type = "claude-code".to_string();
            sp.workflow_run_id = Some("rPause".to_string());
            sp.workflow_role = Some("reviewer".to_string());
            s.sessions.insert("ts-rev".to_string(), DaemonSession::spawn(sp).expect("spawn"));
        }
        // Run mid-hand-off: active=reviewer, pending_activation Queued, PAUSED.
        crate::workflow::run::modify("rPause", |r| {
            r.active_role = Some("reviewer".to_string());
            r.role_sessions.insert("reviewer".to_string(), RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: Some("sid-rev".to_string()),
                daemon_session_uid: Some("ts-rev".to_string()),
            });
            r.pending_activation = Some(PendingActivation {
                activation_id: 2, target_role: "reviewer".to_string(), iteration: 2,
                trigger: TriggerKind::StaticIdle { from_role: "worker".to_string() },
                raw_prompt: String::new(), needs_fresh_reset: true,
                phase: ActivationPhase::Queued, rendered_prompt: None,
                pre_clear_snapshot: None, enter_fire_at_ms: None,
            });
            r.set_paused(true);
        })
        .unwrap();

        let poller = WorkflowPoller::new(state);
        poller.set_finalize_timing_for_test(0, 0);

        // Paused: drain must NOT advance the activation.
        poller.poll_once();
        let run = crate::workflow::run::load_one("rPause").unwrap();
        assert_eq!(
            run.pending_activation.as_ref().unwrap().phase,
            ActivationPhase::Queued,
            "drain must not touch a paused run"
        );
        assert_eq!(
            run.role_sessions["reviewer"].current_session_id.as_deref(),
            Some("sid-rev"),
            "fresh reset must NOT have run while paused (sid intact)"
        );

        // Unpause: the next tick finalizes.
        crate::workflow::run::modify("rPause", |r| r.set_paused(false)).unwrap();
        poller.poll_once();
        let run = crate::workflow::run::load_one("rPause").unwrap();
        assert_ne!(
            run.pending_activation.as_ref().map(|p| p.phase.clone()),
            Some(ActivationPhase::Queued),
            "once unpaused, the drainer advances the activation"
        );
    }

    /// Reviewer-fix: `poll_once` reads runs from **disk**
    /// (`workflow::run::load_all`), NOT from
    /// `state.workflow_runs`. Without this, TUI-launched runs
    /// (which write disk + TUI's in-memory `App.workflow_runs`
    /// but never touch daemon's cache) would be invisible to
    /// the poller.
    ///
    /// Test: write a state.json directly to disk WITHOUT
    /// populating `state.workflow_runs`. Assert `poll_once`
    /// sees the run.
    #[test]
    fn poll_once_reads_runs_from_disk_not_cache() {
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        // Build a run and persist to disk via the same path TUI
        // uses (`workflow::run::save`). Critically: do NOT
        // insert anything into `state.workflow_runs`.
        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: Some("sid-disk".to_string()),
                daemon_session_uid: None,
            },
        );
        let run = WorkflowRun::new(
            "r-disk-only".to_string(),
            "feedback".to_string(),
            "/tmp/wf-poller-test".to_string(),
            role_sessions,
            "worker".to_string(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );
        crate::workflow::run::save(&run).expect("save run to disk");

        // DaemonState is empty (no workflow_runs entry).
        let state = Arc::new(Mutex::new(DaemonState::default()));
        assert!(
            state.lock().unwrap().workflow_runs.is_empty(),
            "test precondition: state.workflow_runs cache is empty",
        );

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);
        let decisions = poller.poll_once();
        // Filter to only OUR run — under workspace parallelism
        // some other test's tempdir might have leaked runs into
        // the env_lock-shared HOME (though env_lock serializes,
        // the methods.rs `with_temp_home` pattern releases the
        // lock before the caller's `_tmp` drops, so cross-test
        // tempdir-residue races are theoretically possible).
        // The contract we want to pin is "the poller sees runs
        // on disk that aren't in the cache" — checking our
        // specific run_id is in the decision list is sufficient.
        let saw_our_run = decisions.iter().any(|d| match d {
            Decision::Skip { run_id, .. }
            | Decision::ActivateStatic { run_id, .. } => run_id == "r-disk-only",
        });
        assert!(
            saw_our_run,
            "poller must see the disk-only run; pre-fix it would \
             have seen empty cache and returned no decisions for \
             this run_id. Got decisions: {:?}",
            decisions,
        );
    }

    /// Companion to the disk-read test: after the TUI's deferred
    /// history-entry append lands on disk (via `run::modify`),
    /// the next `poll_once` observes it. Pre-fix the daemon's
    /// cache would have stayed stale forever (it's only
    /// refreshed by daemon's own handlers).
    ///
    /// Setup: run with `active_role = "reviewer"` but no
    /// history entry for reviewer (the round-1 gap window).
    /// First poll → Skip{NoHistoryEntry}. TUI appends a
    /// history entry via `run::modify`. Second poll → no
    /// longer NoHistoryEntry.
    #[test]
    fn poll_once_observes_disk_history_after_tui_appends() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        // Worktree + transcript so worktree-skip doesn't fire.
        let wt = _tmp_home.path().join("wt-disk-hist");
        std::fs::create_dir_all(&wt).unwrap();

        // Build run with active_role=reviewer, no reviewer history.
        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: Some("sid-w".to_string()),
                daemon_session_uid: None,
            },
        );
        role_sessions.insert(
            "reviewer".to_string(),
            RoleBinding {
                session_label: "reviewer".to_string(),
                current_session_id: Some("sid-r".to_string()),
                daemon_session_uid: None,
            },
        );
        let mut run = WorkflowRun::new(
            "r-disk-hist".to_string(),
            "feedback".to_string(),
            "/tmp/wf-poller-disk-hist".to_string(),
            role_sessions,
            "worker".to_string(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );
        // Manually flip active_role to "reviewer" without
        // appending a history entry → gap window.
        run.active_role = Some("reviewer".to_string());
        crate::workflow::run::save(&run).expect("seed save");

        let state = Arc::new(Mutex::new(DaemonState::default()));
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-disk-hist".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces
                .insert("/tmp/wf-poller-disk-hist".to_string(), ws);
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("w".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            roles.insert(
                "reviewer".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("r".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "reviewer".to_string(),
                        on: TriggerOn::Idle,
                        to: "worker".to_string(),
                    }],
                },
            );
            // Daemon session so the gate says "daemon owns."
            let mut sp = crate::session::SpawnParams::new(
                "ts-d-r",
                "reviewer",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-disk-hist".to_string();
            sp.workflow_run_id = Some("r-disk-hist".to_string());
            sp.workflow_role = Some("reviewer".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-d-r".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-disk-hist", "reviewer", "ts-d-r");

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);

        // First poll: NoHistoryEntry.
        let decisions = poller.poll_once();
        assert!(
            matches!(
                &decisions[0],
                Decision::Skip {
                    reason: SkipReason::NoHistoryEntry,
                    ..
                }
            ),
            "first poll should skip with NoHistoryEntry; got {:?}",
            decisions[0],
        );

        // TUI's deferred history append: write a reviewer entry
        // via `run::modify` (the actual path the TUI tail uses).
        crate::workflow::run::modify("r-disk-hist", |r| {
            r.append_history_entry_for_event_target_role(
                "reviewer",
                2,
                crate::workflow::run::TriggerKind::StaticIdle {
                    from_role: "worker".to_string(),
                },
                0,
                0,
            );
        })
        .expect("append history entry");

        // Second poll: NoHistoryEntry no longer; proceeds to
        // gate check (which will skip with NotIdle since no
        // transcript turns).
        let decisions2 = poller.poll_once();
        assert!(
            !matches!(
                &decisions2[0],
                Decision::Skip {
                    reason: SkipReason::NoHistoryEntry,
                    ..
                }
            ),
            "after TUI appends history on disk, poll must observe \
             it; pre-fix the cache would have stayed stale and \
             this would still be NoHistoryEntry. Got {:?}",
            decisions2[0],
        );
    }

    /// F1 — Daemon uses `active_assistant_start_count()` for the
    /// idle baseline, NOT `role_baselines[active].assistant_count`.
    /// Two sub-cases:
    ///   (a) Run with NO history entry for the active role yet
    ///       (10d-2c-1 round-1 gap window — daemon advanced
    ///       `active_role` but TUI hasn't appended history yet).
    ///       Skip with `NoHistoryEntry`.
    ///   (b) Run with a history entry for the active role whose
    ///       `assistant_count_at_start` reflects current
    ///       activation. Gate uses that as baseline.
    ///
    /// Both pinned in one test: build a run with NO history entry
    /// for "reviewer" (active_role set to reviewer, history only
    /// contains the initial worker entry from `WorkflowRun::new`).
    /// Assert SkipReason::NoHistoryEntry.
    #[test]
    fn poll_once_skips_no_history_entry_in_round1_gap_window() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        // Worktree + transcript so evaluate doesn't skip on
        // NoWorktreePath. We need to get past every earlier
        // skip check to reach the baseline lookup.
        let wt = _tmp_home.path().join("wt-f1");
        std::fs::create_dir_all(&wt).unwrap();

        let state = make_state_with_one_active_run("r-f1-gap");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            // Register a workflow definition so evaluate gets
            // past the NoWorkflowDefinition skip.
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("worker".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            roles.insert(
                "reviewer".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("review".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            // reviewer → worker on_idle so the active role
            // (reviewer in this test) has an on_idle transition.
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "reviewer".to_string(),
                        on: TriggerOn::Idle,
                        to: "worker".to_string(),
                    }],
                },
            );

            // Advance active_role to "reviewer" but DON'T append
            // a history entry for reviewer — this is the gap
            // window where daemon-poller would see stale baseline
            // if it used `role_baselines`. Mutate cache AND
            // re-save to disk (reviewer-fix: poller reads disk,
            // not cache).
            let run = s.workflow_runs.get_mut("r-f1-gap").unwrap();
            run.role_sessions.insert(
                "reviewer".to_string(),
                RoleBinding {
                    session_label: "reviewer".to_string(),
                    current_session_id: Some("sid-reviewer".to_string()),
                    daemon_session_uid: None,
                },
            );
            run.active_role = Some("reviewer".to_string());
            // Verify the gap: history only has the initial
            // worker entry, none for reviewer.
            assert!(
                run.active_assistant_start_count().is_none(),
                "test setup precondition: active_assistant_start_count \
                 should be None in the gap window (active_role=reviewer, \
                 history only has worker entry)",
            );
            // Re-save so the disk-read in collect_snapshots sees
            // the mutated state.
            crate::workflow::run::save(run).expect("re-save mutated run");

            // Daemon session for reviewer so the ownership gate
            // says "daemon owns."
            let mut sp = crate::session::SpawnParams::new(
                "ts-daemon-reviewer",
                "reviewer",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-f1-gap".to_string());
            sp.workflow_role = Some("reviewer".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-daemon-reviewer".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-f1-gap", "reviewer", "ts-daemon-reviewer");

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);
        let decisions = poller.poll_once();
        assert_eq!(decisions.len(), 1);
        assert!(
            matches!(
                &decisions[0],
                Decision::Skip {
                    run_id,
                    reason: SkipReason::NoHistoryEntry
                } if run_id == "r-f1-gap"
            ),
            "expected Skip{{NoHistoryEntry}} in the round-1 gap \
             window; got {:?}",
            decisions[0],
        );
    }

    /// R10 — Workflow definition not yet pushed (TUI hasn't called
    /// `workflow_update_definitions` yet). Daemon poller produces
    /// `Skip{NoWorkflowDefinition}`. Self-resolves on next push.
    #[test]
    fn poll_once_returns_no_workflow_definition_when_unloaded() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let state = make_state_with_one_active_run("r-no-def");
        {
            let mut s = state.lock().unwrap();
            let mut sp = crate::session::SpawnParams::new(
                "ts-worker-no-def",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-no-def".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-worker-no-def".to_string(), ds);
            // Deliberately DO NOT insert anything into
            // `workflow_definitions` — that's the R10 surface.
        }
        bind_daemon_uid_to_role("r-no-def", "worker", "ts-worker-no-def");

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);
        let decisions = poller.poll_once();
        assert_eq!(decisions.len(), 1);
        assert!(
            matches!(
                &decisions[0],
                Decision::Skip {
                    run_id,
                    reason: SkipReason::NoWorkflowDefinition
                } if run_id == "r-no-def"
            ),
            "expected Skip{{NoWorkflowDefinition}}, got {:?}",
            decisions[0],
        );
    }

    /// Daemon-side resolver smoke + structural sanity. The
    /// cross-crate parity test (daemon resolver vs TUI resolver
    /// producing identical output for the same workflow_def +
    /// run + session_id + worktree) is asserted from the TUI side
    /// in `tui/src/workflow/controller.rs::tests` (added in this
    /// same slice) — that's the only crate where both impls are
    /// importable at once. This test pins the daemon side reads
    /// what the gate needs.
    #[test]
    fn daemon_resolver_reads_baseline_and_renders_template() {
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());
        let wt = _tmp_home.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-w.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"latest worker turn"}]}}"##,
        )
        .unwrap();

        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: Some("sid-w".to_string()),
                daemon_session_uid: None,
            },
        );
        let mut baselines = BTreeMap::new();
        baselines.insert(
            "worker".to_string(),
            MessageBaseline {
                user_count: 0,
                assistant_count: 0,
            },
        );
        let run = WorkflowRun::new(
            "r-resolver".to_string(),
            "feedback".to_string(),
            "/tmp/wf-poller-resolver".to_string(),
            role_sessions,
            "worker".to_string(),
            baselines,
            None,
            BTreeMap::new(),
            0,
        );
        let mut engines = BTreeMap::new();
        engines.insert("worker".to_string(), Engine::ClaudeCode);
        let resolver = DaemonWorkflowResolver {
            run: &run,
            worktree_path: Some(wt.as_path()),
            role_engines: engines,
        };

        // assistant_messages skips the baseline (assistant_count=0
        // → no skip) and surfaces the one post-baseline turn.
        let am: Vec<String> = <DaemonWorkflowResolver as crate::workflow::template::RoleResolver>::assistant_messages(&resolver, "worker");
        assert_eq!(am.len(), 1, "one post-baseline turn expected");
        assert!(am[0].contains("latest worker turn"));

        // Template rendering: {{ roles.worker.last_message }}
        // resolves to the assistant turn.
        let rendered = crate::workflow::template::render(
            "review: {{ roles.worker.last_message }}",
            &resolver,
        );
        assert!(
            rendered.contains("latest worker turn"),
            "template should embed the worker's last message: {:?}",
            rendered,
        );
    }

    /// API contract: `start()` returns `io::Result<()>` so a
    /// transient thread-spawn failure doesn't crash daemon startup.
    /// The reviewer flagged that `.expect("spawn workflow poller
    /// thread")` would defeat the "no behavior change" property of
    /// 2c-2-2-a — pre-existing `spawn_watcher` (slice 10d-memory-
    /// cap-relocation) is the model for the API shape.
    ///
    /// We exercise the success path here. A failure-injection seam
    /// (matching `WatcherSpawnFn`) was deferred — the production
    /// surface is one `Builder::spawn` call; a future slice can
    /// add the seam if a real failure path needs coverage.
    #[test]
    fn start_returns_io_result_and_idempotent() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let poller = Arc::new(WorkflowPoller::new(state));
        // Slow tick so the thread doesn't accumulate work during
        // the test.
        poller.set_tick_interval_for_test(60_000); // 60ms
        // First start succeeds and spawns the thread.
        let r1: std::io::Result<()> = poller.start();
        assert!(r1.is_ok(), "first start should succeed: {:?}", r1);
        // Idempotent: second call is a no-op success — no second
        // thread spawned, the existing one keeps running. This is
        // what `lib.rs::run()`'s "if let Err" branch relies on for
        // graceful startup retries (none currently, but the
        // contract is set).
        let r2: std::io::Result<()> = poller.start();
        assert!(r2.is_ok(), "second start should be idempotent: {:?}", r2);
        poller.shutdown();
    }

    /// F2 — Stale-snapshot apply rejected by `expected_from`.
    /// Poller snapshots `active_role = "worker"`. Pre-apply hook
    /// flips state.json's `active_role` to "reviewer". When the
    /// poller invokes `workflow_transition` with
    /// `expected_from: "worker"`, the handler's precondition
    /// check inside `try_modify` sees `active_role == "reviewer"`
    /// and aborts with Conflict. No state mutation. No event.
    #[test]
    fn poll_once_stale_snapshot_rejected_via_expected_from() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        // Build the same scenario as T1 — daemon-owned idle
        // worker → reviewer with template + transcript — but
        // inject a pre-apply hook that flips active_role to
        // "reviewer" between snapshot and apply.
        let wt = _tmp_home.path().join("wt-f2");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r-f2-stale");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("worker prompt".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            roles.insert(
                "reviewer".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("review".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );
            // Add reviewer binding for the post-flip state.
            s.workflow_runs
                .get_mut("r-f2-stale")
                .unwrap()
                .role_sessions
                .insert(
                    "reviewer".to_string(),
                    RoleBinding {
                        session_label: "reviewer".to_string(),
                        current_session_id: Some("sid-reviewer".to_string()),
                        daemon_session_uid: None,
                    },
                );

            let mut sp = crate::session::SpawnParams::new(
                "ts-worker-f2",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-f2-stale".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-worker-f2".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-f2-stale", "worker", "ts-worker-f2");

        let poller = WorkflowPoller::new(Arc::clone(&state));
        // Install hook: flip active_role between snapshot and
        // apply ON DISK (reviewer-fix made the disk
        // authoritative; the handler's try_modify reads disk
        // under flock). The poller snapshots active_role =
        // "worker"; hook flips disk to "reviewer"; apply phase
        // calls workflow_transition with
        // `expected_from = "worker"`, handler's check sees
        // disk's active_role = "reviewer" → Conflict.
        poller.set_pre_apply_hook_for_test(|_state: &mut DaemonState| {
            crate::workflow::run::modify("r-f2-stale", |r| {
                r.active_role = Some("reviewer".to_string());
            })
            .expect("flip active_role on disk for race simulation");
        });
        // Apply is ENABLED for this test — we want the handler
        // to run and reject.
        let pre_disk = crate::workflow::run::load_one("r-f2-stale")
            .expect("disk load before");
        let pre_iteration = pre_disk.iteration;
        let _ = poller.poll_once();
        let post_disk = crate::workflow::run::load_one("r-f2-stale")
            .expect("disk load after");
        // The hook flipped active_role on disk; the handler's
        // expected_from check (expected=worker, actual=reviewer)
        // returned Conflict; mutation rejected. Active_role
        // stays at the hook's value; iteration unchanged.
        assert_eq!(
            post_disk.active_role.as_deref(),
            Some("reviewer"),
            "active_role reflects the hook's flip (no further \
             advance by the rejected apply)",
        );
        assert_eq!(
            post_disk.iteration, pre_iteration,
            "iteration must not advance: handler rejected with \
             Conflict before mutation",
        );
    }

    /// 10d-2c-2-2-c T3 extension — `active_role` flipped to `None`
    /// mid-tick (e.g. workflow_done landed between snapshot and
    /// apply). Daemon's `expected_from` check sees active_role
    /// is None, but expected is "worker" → mismatch → Conflict.
    /// No event written, no state mutation by the poller.
    #[test]
    fn poll_once_stale_active_role_none_rejected_by_expected_from() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let wt = _tmp_home.path().join("wt-t3-none");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r-t3-none");
        crate::workflow::run::modify("r-t3-none", |r| {
            r.role_sessions.insert(
                "reviewer".to_string(),
                RoleBinding {
                    session_label: "reviewer".to_string(),
                    current_session_id: Some("sid-r".to_string()),
                    daemon_session_uid: None,
                },
            );
        })
        .expect("add reviewer binding");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let make_role = |p: &str| Role {
                engine: Engine::ClaudeCode,
                context: Context::Persistent,
                activation_prompt: Some(p.to_string()),
                subsequent_activation_prompt: None,
                needs_mcp: false,
            };
            let mut roles = BTreeMap::new();
            roles.insert("worker".to_string(), make_role("w"));
            roles.insert("reviewer".to_string(), make_role("r"));
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec!["worker".to_string(), "reviewer".to_string()],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );

            let mut sp = crate::session::SpawnParams::new(
                "ts-w-t3-none",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-t3-none".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-w-t3-none".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-t3-none", "worker", "ts-w-t3-none");

        let poller = WorkflowPoller::new(Arc::clone(&state));
        // Hook: simulate workflow_done landing — clear
        // active_role on disk between snapshot and apply.
        poller.set_pre_apply_hook_for_test(|_s: &mut DaemonState| {
            crate::workflow::run::modify("r-t3-none", |r| {
                r.active_role = None;
            })
            .expect("clear active_role");
        });

        let _ = poller.poll_once();
        let post = crate::workflow::run::load_one("r-t3-none")
            .expect("post load");
        assert!(
            post.active_role.is_none(),
            "active_role stays None (hook's value); apply was \
             rejected pre-mutation. Got: {:?}",
            post.active_role,
        );
        // Iteration unchanged from seed (1).
        assert_eq!(
            post.iteration, 1,
            "iteration must not advance — apply rejected by \
             expected_from mismatch",
        );
        // No event written.
        let (events, _) =
            crate::workflow::events::read_new("r-t3-none", 0);
        assert!(
            events.is_empty(),
            "no event written — apply rejected. Got: {:?}",
            events,
        );
    }

    /// 10d-2c-2-2-c T3 extension — `active_role` flipped to a
    /// DIFFERENT role (concurrent dynamic transition mid-tick).
    /// Same Conflict shape as the None case. Belt-and-suspenders
    /// with the existing F2 stale test, which uses "reviewer";
    /// this test pins the more general invariant for any
    /// not-equal-to-expected value.
    #[test]
    fn poll_once_stale_active_role_other_role_rejected() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let wt = _tmp_home.path().join("wt-t3-other");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r-t3-other");
        crate::workflow::run::modify("r-t3-other", |r| {
            r.role_sessions.insert(
                "reviewer".to_string(),
                RoleBinding {
                    session_label: "reviewer".to_string(),
                    current_session_id: Some("sid-r".to_string()),
                    daemon_session_uid: None,
                },
            );
            r.role_sessions.insert(
                "manager".to_string(),
                RoleBinding {
                    session_label: "manager".to_string(),
                    current_session_id: Some("sid-m".to_string()),
                    daemon_session_uid: None,
                },
            );
        })
        .expect("add bindings");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let make_role = |p: &str| Role {
                engine: Engine::ClaudeCode,
                context: Context::Persistent,
                activation_prompt: Some(p.to_string()),
                subsequent_activation_prompt: None,
                needs_mcp: false,
            };
            let mut roles = BTreeMap::new();
            roles.insert("worker".to_string(), make_role("w"));
            roles.insert("reviewer".to_string(), make_role("r"));
            roles.insert("manager".to_string(), make_role("m"));
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                        "manager".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );

            let mut sp = crate::session::SpawnParams::new(
                "ts-w-t3-other",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-t3-other".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-w-t3-other".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-t3-other", "worker", "ts-w-t3-other");

        let poller = WorkflowPoller::new(Arc::clone(&state));
        // Hook: simulate a concurrent dynamic transition that
        // flips active_role to "manager" (a different role than
        // expected="worker" AND different from target="reviewer").
        poller.set_pre_apply_hook_for_test(|_s: &mut DaemonState| {
            crate::workflow::run::modify("r-t3-other", |r| {
                r.active_role = Some("manager".to_string());
            })
            .expect("flip active_role to manager");
        });

        let _ = poller.poll_once();
        let post = crate::workflow::run::load_one("r-t3-other")
            .expect("post load");
        assert_eq!(
            post.active_role.as_deref(),
            Some("manager"),
            "active_role stays at hook's value (manager); apply \
             rejected by expected_from='worker' mismatch.",
        );
        // Iteration unchanged from seed.
        assert_eq!(
            post.iteration, 1,
            "iteration must not advance — handler returned Conflict",
        );
    }

    /// 10d-2c-2-2-c R12 — Workflow definition replaced mid-poll
    /// (between snapshot and apply). The poller's snapshot
    /// computed a `Decision::ActivateStatic` based on the OLD
    /// definition; before apply, the TUI re-pushes a new
    /// definition with a DIFFERENT `on_idle` target. The apply
    /// path calls `workflow_transition` with the target_role
    /// from the snapshot-time decision — that target is passed
    /// as an explicit param, NOT re-derived from the definition
    /// at apply time. Result: the snapshot-time decision
    /// commits; new definition takes effect next tick.
    ///
    /// This is the "definition reload doesn't retroactively
    /// invalidate in-flight decisions" invariant. The
    /// alternative (re-validate target against new definition)
    /// would mean a definition push could fail in-flight fires
    /// silently, which is worse.
    ///
    /// Companion to F2's stale-snapshot test (which DID reject
    /// because the run's `active_role` itself changed, not just
    /// the workflow definition).
    #[test]
    fn poll_once_workflow_definition_replaced_mid_apply_commits_snapshot_decision() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        // Setup mirrors T1's: idle worker → reviewer.
        let wt = _tmp_home.path().join("wt-r12");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r-r12-defswap");
        // Add reviewer + manager bindings to the on-disk run so
        // the handler's target-role validation passes regardless
        // of which definition is active. (Cache-only mutation
        // here would be wiped by the handler's `try_modify`
        // reload from disk.)
        crate::workflow::run::modify("r-r12-defswap", |r| {
            r.role_sessions.insert(
                "reviewer".to_string(),
                RoleBinding {
                    session_label: "reviewer".to_string(),
                    current_session_id: Some("sid-reviewer".to_string()),
                    daemon_session_uid: None,
                },
            );
            r.role_sessions.insert(
                "manager".to_string(),
                RoleBinding {
                    session_label: "manager".to_string(),
                    current_session_id: Some("sid-manager".to_string()),
                    daemon_session_uid: None,
                },
            );
        })
        .expect("add reviewer + manager role bindings");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let make_role = |prompt: &str| Role {
                engine: Engine::ClaudeCode,
                context: Context::Persistent,
                activation_prompt: Some(prompt.to_string()),
                subsequent_activation_prompt: None,
                needs_mcp: false,
            };
            let mut old_roles = BTreeMap::new();
            old_roles.insert("worker".to_string(), make_role("w"));
            old_roles.insert("reviewer".to_string(), make_role("r"));
            old_roles.insert("manager".to_string(), make_role("m"));
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles: old_roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                        "manager".to_string(),
                    ],
                    // OLD transition: worker → reviewer.
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );

            let mut sp = crate::session::SpawnParams::new(
                "ts-worker-r12",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-r12-defswap".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-worker-r12".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-r12-defswap", "worker", "ts-worker-r12");

        let poller = WorkflowPoller::new(Arc::clone(&state));
        // Pre-apply hook: swap definition to "worker → manager"
        // (different target than the snapshot-time computation
        // chose). Snapshot-time decision targeted "reviewer";
        // the apply path passes "reviewer" as a param.
        poller.set_pre_apply_hook_for_test(|s: &mut DaemonState| {
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let make_role = |prompt: &str| Role {
                engine: Engine::ClaudeCode,
                context: Context::Persistent,
                activation_prompt: Some(prompt.to_string()),
                subsequent_activation_prompt: None,
                needs_mcp: false,
            };
            let mut new_roles = BTreeMap::new();
            new_roles.insert("worker".to_string(), make_role("w"));
            new_roles.insert("reviewer".to_string(), make_role("r"));
            new_roles.insert("manager".to_string(), make_role("m"));
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles: new_roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                        "manager".to_string(),
                    ],
                    // NEW transition: worker → manager.
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "manager".to_string(),
                    }],
                },
            );
        });

        // Apply ENABLED: the handler runs and commits the
        // snapshot-time decision.
        let _ = poller.poll_once();
        let post = crate::workflow::run::load_one("r-r12-defswap")
            .expect("post-poll load");
        assert_eq!(
            post.active_role.as_deref(),
            Some("reviewer"),
            "snapshot-time decision (target=reviewer) must commit; \
             definition swap to worker→manager takes effect next \
             tick. Got active_role={:?}",
            post.active_role,
        );
        assert_eq!(
            post.iteration, 2,
            "iteration advances by 1 from snapshot-time commit",
        );
    }

    /// 10d-2c-2-2-c R12 outcome (b) — Identical definition
    /// re-push mid-apply (no actual change). Apply commits
    /// normally; the re-push is benign.
    #[test]
    fn poll_once_identical_definition_repush_mid_apply_commits_cleanly() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let wt = _tmp_home.path().join("wt-r12b");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r-r12b-noop");
        crate::workflow::run::modify("r-r12b-noop", |r| {
            r.role_sessions.insert(
                "reviewer".to_string(),
                RoleBinding {
                    session_label: "reviewer".to_string(),
                    current_session_id: Some("sid-r".to_string()),
                    daemon_session_uid: None,
                },
            );
        })
        .expect("add reviewer binding");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let make_role = |prompt: &str| Role {
                engine: Engine::ClaudeCode,
                context: Context::Persistent,
                activation_prompt: Some(prompt.to_string()),
                subsequent_activation_prompt: None,
                needs_mcp: false,
            };
            let mut roles = BTreeMap::new();
            roles.insert("worker".to_string(), make_role("w"));
            roles.insert("reviewer".to_string(), make_role("r"));
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec!["worker".to_string(), "reviewer".to_string()],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );

            let mut sp = crate::session::SpawnParams::new(
                "ts-worker-r12b",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-r12b-noop".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-worker-r12b".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-r12b-noop", "worker", "ts-worker-r12b");

        let poller = WorkflowPoller::new(Arc::clone(&state));
        // Hook: re-push the SAME definition. No-op semantically.
        poller.set_pre_apply_hook_for_test(|s: &mut DaemonState| {
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let make_role = |prompt: &str| Role {
                engine: Engine::ClaudeCode,
                context: Context::Persistent,
                activation_prompt: Some(prompt.to_string()),
                subsequent_activation_prompt: None,
                needs_mcp: false,
            };
            let mut roles = BTreeMap::new();
            roles.insert("worker".to_string(), make_role("w"));
            roles.insert("reviewer".to_string(), make_role("r"));
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec!["worker".to_string(), "reviewer".to_string()],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );
        });

        let _ = poller.poll_once();
        let post = crate::workflow::run::load_one("r-r12b-noop")
            .expect("post-poll load");
        assert_eq!(post.active_role.as_deref(), Some("reviewer"));
        assert_eq!(post.iteration, 2);
    }

    /// F4 — Empty rendered prompt does NOT skip the transition;
    /// daemon fires anyway with `args.prompt = ""`. The TUI tail's
    /// existing empty-prompt handling skips just the PTY write.
    /// Pre-fix daemon-owned promptless workflows would wedge.
    #[test]
    fn poll_once_fires_when_activation_prompt_is_absent() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let wt = _tmp_home.path().join("wt-f4");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r-f4-empty");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "/tmp/wf-poller-test".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("/tmp/wf-poller-test".to_string(), ws);
            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("ignored".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            // Reviewer has NO activation_prompt — daemon must
            // still fire the transition.
            roles.insert(
                "reviewer".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: None,
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );

            let mut sp = crate::session::SpawnParams::new(
                "ts-worker-f4",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "/tmp/wf-poller-test".to_string();
            sp.workflow_run_id = Some("r-f4-empty".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-worker-f4".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-f4-empty", "worker", "ts-worker-f4");

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);
        let decisions = poller.poll_once();
        assert_eq!(decisions.len(), 1);
        match &decisions[0] {
            Decision::ActivateStatic {
                run_id,
                from_role,
                to_role,
                rendered_prompt,
            } => {
                assert_eq!(run_id, "r-f4-empty");
                assert_eq!(from_role, "worker");
                assert_eq!(to_role, "reviewer");
                // Empty prompt — but transition STILL fires.
                assert_eq!(
                    rendered_prompt, "",
                    "rendered prompt is empty (no template) but \
                     the transition still fires; pre-fix this was \
                     a Skip{{NoActivationPrompt}}",
                );
            }
            other => panic!(
                "expected ActivateStatic with empty prompt, got {:?}",
                other,
            ),
        }
    }

    /// Review-round-5 F1 — `role_session_types` engine resolution
    /// uses the uid-first fallback, NOT tags only. A daemon-owned
    /// CODEX session without `set_workflow_context` tags must
    /// still surface as `session_type = "codex"`. Pre-fix it
    /// would fall through to default "claude-code" and the
    /// engine-derivation in `evaluate_snapshot` would read the
    /// wrong transcript format.
    #[test]
    fn resolve_role_session_type_uid_path_returns_correct_engine_without_tags() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: Some("sid-x".to_string()),
                daemon_session_uid: Some("ts-codex-untagged".to_string()),
            },
        );
        let run = WorkflowRun::new(
            "r-engine".to_string(),
            "feedback".to_string(),
            "ws-engine".to_string(),
            role_sessions,
            "worker".to_string(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );

        let mut state = DaemonState::default();
        let mut sp = crate::session::SpawnParams::new(
            "ts-codex-untagged",
            "worker",
            "/bin/sleep",
        );
        sp.args = vec!["60".to_string()];
        sp.workspace_id = "ws-engine".to_string();
        sp.session_type = "codex".to_string();
        // NB: workflow_run_id + workflow_role deliberately NOT set
        // — simulates `set_workflow_context` push that never
        // landed. Pre-r5-r5 the tag-only fallback would miss
        // this session entirely.
        let ds: DaemonSession =
            crate::session::DaemonSession::spawn(sp).expect("spawn");
        state.sessions.insert("ts-codex-untagged".to_string(), ds);

        let resolved = resolve_role_session_type(&state, &run, "worker");
        assert_eq!(
            resolved.as_deref(),
            Some("codex"),
            "uid-first fallback must surface codex even without \
             workflow_run_id/workflow_role tags. Got: {:?}",
            resolved,
        );
    }

    /// Round-5 F1 acceptance — gate fires for a daemon-attached
    /// session in `state.sessions` WITHOUT `workflow_run_id`/
    /// `workflow_role` tags, as long as the run's role binding
    /// has the matching `daemon_session_uid`. Pre-r5 the gate
    /// walked `state.sessions` for tags; if
    /// `session.set_workflow_context` push hadn't landed, daemon
    /// would skip with TuiOwns and the workflow would stall.
    /// Post-r5 the gate uses the durable on-disk binding.
    #[test]
    fn poll_once_gate_fires_without_workflow_tags_when_binding_has_uid() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let wt = _tmp_home.path().join("wt-r5");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        let state = make_state_with_one_active_run("r-r5-untagged");
        {
            let mut s = state.lock().unwrap();
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "ws-r5".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("ws-r5".to_string(), ws);

            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("w".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            roles.insert(
                "reviewer".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("r".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );

            // Daemon session UID is inserted, but workflow_run_id +
            // workflow_role are deliberately LEFT UNSET — this
            // simulates the failure mode where
            // `session.set_workflow_context` never landed.
            let mut sp = crate::session::SpawnParams::new(
                "ts-untagged",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "ws-r5".to_string();
            // NB: workflow_run_id and workflow_role NOT set.
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            assert!(
                ds.workflow_run_id.is_none(),
                "test precondition: daemon session has no tags",
            );
            s.sessions.insert("ts-untagged".to_string(), ds);
        }
        // Bind the daemon uid into the run's role binding — the
        // durable signal that pre-r5 was missing.
        bind_daemon_uid_to_role("r-r5-untagged", "worker", "ts-untagged");

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);
        let decisions = poller.poll_once();
        let our = decisions
            .iter()
            .find(|d| match d {
                Decision::Skip { run_id, .. } | Decision::ActivateStatic { run_id, .. } => {
                    run_id == "r-r5-untagged"
                }
            })
            .expect("our run produced a decision");
        assert!(
            matches!(our, Decision::ActivateStatic { .. }),
            "F1: gate must fire when binding.daemon_session_uid \
             matches a session in state.sessions, regardless of \
             whether the session has workflow_run_id/workflow_role \
             tags. Pre-r5 this would have been Skip{{TuiOwns}}. \
             Got {:?}",
            our,
        );
    }

    /// Round-4 F3 — worktree resolution via session tags, NOT
    /// `run.task_key`. When `run.task_key` drifts (e.g. workspace
    /// renamed at TUI level, or run was launched with a different
    /// workspace_id than the session currently uses), the
    /// session's `workspace_id` is authoritative.
    ///
    /// Setup: run.task_key = "ws-stale" (no entry in
    /// state.workspaces). Session for the active role points
    /// at "ws-real" (which has a worktree_path). The poller
    /// must use ws-real, not fall back to ws-stale.
    #[test]
    fn poll_once_resolves_worktree_via_session_workspace_id_not_task_key() {
        use crate::session::DaemonSession;
        let _guard = crate::test_support::env_lock();
        let _tmp_home = tempfile::tempdir().expect("tempdir");
        let _orig_home = std::env::var_os("HOME");
        std::env::set_var("HOME", _tmp_home.path());

        let wt = _tmp_home.path().join("wt-real");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_str = wt.to_str().unwrap();
        let encoded = wt_str.replace('/', "-").replace('.', "-");
        let proj = _tmp_home.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("sid-worker.jsonl"),
            r##"{"type":"assistant","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"done"}]}}"##,
        )
        .unwrap();

        // Build run with task_key = "ws-stale" — a workspace_id
        // that has NO entry in state.workspaces. Pre-F3 the
        // poller would have looked here and found nothing,
        // skipping with NoWorktreePath.
        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "worker".to_string(),
                current_session_id: Some("sid-worker".to_string()),
                daemon_session_uid: None,
            },
        );
        let run = WorkflowRun::new(
            "r-f3-drift".to_string(),
            "feedback".to_string(),
            "ws-stale".to_string(),
            role_sessions,
            "worker".to_string(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        );
        crate::workflow::run::save(&run).expect("save");

        let state = Arc::new(Mutex::new(DaemonState::default()));
        {
            let mut s = state.lock().unwrap();
            // workspace ENTRY is for "ws-real", NOT "ws-stale".
            let mut ws = crate::manifest::ManifestWorkspace::default();
            ws.id = "ws-real".to_string();
            ws.worktree_path = Some(wt.clone());
            s.workspaces.insert("ws-real".to_string(), ws);

            use crate::workflow::toml_schema::{
                Context, Role, Transition, TriggerOn, Workflow,
            };
            let mut roles = BTreeMap::new();
            roles.insert(
                "worker".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("w".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            roles.insert(
                "reviewer".to_string(),
                Role {
                    engine: Engine::ClaudeCode,
                    context: Context::Persistent,
                    activation_prompt: Some("r".to_string()),
                    subsequent_activation_prompt: None,
                    needs_mcp: false,
                },
            );
            s.workflow_definitions.insert(
                "feedback".to_string(),
                Workflow {
                    name: "feedback".to_string(),
                    description: String::new(),
                    roles,
                    role_order: vec![
                        "worker".to_string(),
                        "reviewer".to_string(),
                    ],
                    transitions: vec![Transition {
                        from: "worker".to_string(),
                        on: TriggerOn::Idle,
                        to: "reviewer".to_string(),
                    }],
                },
            );

            // Session's workspace_id is "ws-real" — this is the
            // authoritative tag the poller must use.
            let mut sp = crate::session::SpawnParams::new(
                "ts-w-f3",
                "worker",
                "/bin/sleep",
            );
            sp.args = vec!["60".to_string()];
            sp.workspace_id = "ws-real".to_string();
            sp.workflow_run_id = Some("r-f3-drift".to_string());
            sp.workflow_role = Some("worker".to_string());
            let ds: DaemonSession =
                crate::session::DaemonSession::spawn(sp).expect("spawn");
            s.sessions.insert("ts-w-f3".to_string(), ds);
        }
        bind_daemon_uid_to_role("r-f3-drift", "worker", "ts-w-f3");

        let poller = WorkflowPoller::new(state);
        poller.set_disable_apply_for_test(true);
        let decisions = poller.poll_once();
        // Find our run's decision.
        let our_decision = decisions
            .iter()
            .find(|d| match d {
                Decision::Skip { run_id, .. } | Decision::ActivateStatic { run_id, .. } => {
                    run_id == "r-f3-drift"
                }
            })
            .expect("our run produced a decision");
        // Should fire (worktree resolved via session tag). Pre-F3
        // would skip with NoWorktreePath because task_key
        // "ws-stale" isn't in state.workspaces.
        assert!(
            matches!(our_decision, Decision::ActivateStatic { .. }),
            "F3: worktree must resolve via session's workspace_id, \
             not the stale task_key. Got {:?}",
            our_decision,
        );
    }

    /// Tick-interval atomic: setter clamps to the floor; getter
    /// reads what was set. Just enough coverage so a future
    /// refactor of the floor doesn't silently drop the clamp.
    #[test]
    fn set_tick_interval_clamps_to_floor() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let poller = WorkflowPoller::new(state);
        poller.set_tick_interval_for_test(0);
        assert_eq!(
            poller.tick_micros.load(Ordering::SeqCst),
            MIN_TICK_INTERVAL_MICROS,
        );
        poller.set_tick_interval_for_test(10_000);
        assert_eq!(poller.tick_micros.load(Ordering::SeqCst), 10_000);
    }
}
