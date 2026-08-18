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

use std::collections::HashMap;
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

/// Operator-caller token the periodic scheduler fires under. It is the auth
/// identity AND the provenance label on the run record. The persistent-fire
/// auto-`/compact` cadence keys off it: ONLY a fire from this caller may turn
/// into a `/compact` (see `methods::trigger`). Every other fire path — a manual
/// `continuous.run_now`, the MCP `trigger` fan-out, a `resolve_stuck` refire —
/// uses a different token and therefore always runs the prompt, never compacts.
pub const SCHEDULER_CALLER_TOKEN: &str = "continuous-scheduler";

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

/// Consecutive fire-launch failures after which the scheduler auto-pauses a
/// task (circuit breaker) and emits an alert, rather than churning + backing
/// off forever. The operator un-pauses after fixing the cause.
const CONSECUTIVE_FAILURE_LIMIT: u32 = 5;

/// Consumer (Phase 4) inter-fire spacing (seconds). After a Consumer fire (or a
/// run-active skip) the task is not re-evaluated for this long — a deep queue
/// drains at ≤1 batch/min instead of hot-looping every 250 ms tick, while
/// stragglers still beat the (much larger) `window_secs`. Also the Consumer
/// failure-backoff BASE: `backoff_secs(CONSUMER_REFIRE_SPACING_SECS, n)` grows
/// 2 min → 4 min → … capped at [`BACKOFF_CAP_SECS`].
const CONSUMER_REFIRE_SPACING_SECS: u64 = 60;

/// How long a fetched queue depth stays fresh (seconds). The Consumer due-check
/// runs every tick (250 ms) but polls the planning API's `GET /queues/{q}` at
/// most this often per queue — the depth cache in
/// [`ContinuousScheduler::consumer_depth_snapshot`] serves the interim ticks. A
/// fetch FAILURE also caches (as depth 0) so an API outage produces one error
/// log per queue per TTL, not four per second.
const QUEUE_DEPTH_CACHE_TTL_SECS: u64 = 30;

/// How often (seconds) the auth/wedge pass re-reads a given task's transcript
/// tail. The pass runs every tick (250 ms) but tail reads are file I/O +
/// JSON parsing; per-task throttling keeps the steady-state cost at one small
/// read per task per interval. Detection latency is dominated by the wedge
/// grace / alert cooldowns, so 30 s adds nothing observable.
const TAIL_PROBE_INTERVAL_SECS: u64 = 30;

/// Auth-expiry alert cooldown (seconds) — per task. A persistent PERIODIC
/// task keeps re-firing into an auth-dead session (its due-gate has no
/// run-active block), minting a fresh 401 record each period; a
/// record-identity latch would therefore re-alert every period. Cooldown
/// instead: one push per task per window while the condition persists, and a
/// healthy turn clears the latch so a NEW episode alerts immediately.
const AUTH_REALERT_SECS: u64 = 6 * 3600;

/// How often (seconds) the credentials preflight re-checks
/// `~/.claude/.credentials.json`.
const CREDS_CHECK_INTERVAL_SECS: u64 = 60;

/// Post-compact settle spacing (seconds). A compact-only fire delivers
/// `/compact`, whose summarization turn is async and can outlast a few
/// minutes; a prompt pasted into the PTY mid-summarization is DROPPED by the
/// agent TUI (the documented run_now/compact collision). So after a
/// [`FireOutcome::FiredCompact`] the next fire is scheduled no sooner than
/// this — it floors the per-schedule spacing (`max(every, this)`), which
/// matters for Consumers whose [`CONSUMER_REFIRE_SPACING_SECS`] (60s) is well
/// under a `/compact` turn. One stretched inter-fire gap every `compact_every`
/// runs is noise next to a dropped batch prompt (which would strand the
/// batch's items as consumed-but-unprocessed).
const COMPACT_SETTLE_SPACING_SECS: u64 = 600;

/// Outcome of a single in-process fire, distilled from `methods::trigger`'s
/// return into the cases the ADVANCE phase branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FireOutcome {
    /// `Ok({fired:true})` — the fire happened. Reset `consecutive_failures`.
    Fired,
    /// `Ok({fired:true, compact:true})` — a persistent compact-only fire: the
    /// PTY got `/compact` instead of the prompt, and the run was recorded
    /// already-Done at fire time (nothing will `report_done` it). Advances like
    /// [`Fired`] but floors the spacing at [`COMPACT_SETTLE_SPACING_SECS`] so
    /// the NEXT fire's paste cannot land mid-summarization and be dropped.
    FiredCompact,
    /// `Ok({fired:false, reason:busy|paused|duplicate_fire_token})` — a benign
    /// skip (the `in_flight` guard / `paused` flag already prevents
    /// re-selection). Do NOT bump `consecutive_failures`. Leaves `next_fire_at`
    /// UNADVANCED — the task is re-evaluated next due tick.
    Skipped,
    /// Phase 3b DUE-SKIP-ACTIVE: a FRESH task whose prior run is still `Running`
    /// was NOT re-fired (no overlapping fresh runs). UNLIKE [`Skipped`], this
    /// ADVANCES `next_fire_at` catch-up-once (so the periodic schedule keeps
    /// moving without hot-re-evaluating every tick), but — like `Skipped` — does
    /// NOT bump `consecutive_failures` (the skip is benign, not a failure).
    SkipActive,
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
    /// Persistent-stall one-shot latch: `task_id -> the last_fired_at we already
    /// alerted on`. Keeps `persistent_stall_pass` from re-alerting every tick for
    /// the same wedged fire episode; a NEW fire (different `last_fired_at`)
    /// re-arms. In-memory only — a daemon restart re-surfaces a still-stalled
    /// orchestrator exactly once, which is the desired behavior.
    stall_alerts: Mutex<HashMap<String, u64>>,
    /// Consumer depth cache: `queue -> (fetched_at, pending_depth)`. Serves the
    /// due-check between API polls ([`QUEUE_DEPTH_CACHE_TTL_SECS`]). In-memory
    /// only — a restart just re-polls.
    queue_depths: Mutex<HashMap<String, (u64, u64)>>,
    /// Auth/wedge transcript-probe throttle: `task_id -> last probe ts`
    /// ([`TAIL_PROBE_INTERVAL_SECS`]). In-memory only.
    probe_at: Mutex<HashMap<String, u64>>,
    /// Auth-expiry alert cooldown latch: `task_id -> last alert ts`
    /// ([`AUTH_REALERT_SECS`]); cleared when the task's transcript shows a
    /// healthy (non-auth-error) tail again. In-memory only — a restart
    /// re-alerts a still-dead session once, which is the desired behavior.
    auth_alerts: Mutex<HashMap<String, u64>>,
    /// Wedge-escalation one-shot latch: `task_id -> run seq already escalated`
    /// (past the wedge-close limit the run is deliberately LEFT `Running`, so
    /// without this the pass would re-escalate the same run every probe).
    wedge_escalations: Mutex<HashMap<String, u64>>,
    /// Credentials-preflight throttle + cooldown: `(last check ts, last alert
    /// ts)`. The alert half is a per-[`AUTH_REALERT_SECS`] cooldown, cleared
    /// when the file is healthy again.
    creds_state: Mutex<(u64, u64)>,
    /// Test-only: canned per-queue depths so scheduler tests drive the Consumer
    /// due-check without an HTTP server. `Some(map)` short-circuits
    /// `consumer_depth_snapshot` entirely (missing queues read as depth 0).
    #[cfg(test)]
    depth_override: Mutex<Option<HashMap<String, u64>>>,
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
            stall_alerts: Mutex::new(HashMap::new()),
            queue_depths: Mutex::new(HashMap::new()),
            probe_at: Mutex::new(HashMap::new()),
            auth_alerts: Mutex::new(HashMap::new()),
            wedge_escalations: Mutex::new(HashMap::new()),
            creds_state: Mutex::new((0, 0)),
            #[cfg(test)]
            depth_override: Mutex::new(None),
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
        // H3: drain mode also parks the tick — firing a continuous task
        // during a pre-restart drain would mint work the restart kills.
        // Schedules naturally catch up post-restart (due-check on load).
        {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if !s.config.scheduler.enabled || s.draining {
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

        // (c) SUPERVISION — dead-persistent respawn. A supervised Persistent
        // task whose pinned session died is respawned via the same `trigger`
        // funnel (its persistent executor handles dead->respawn). The fresh-hang
        // watchdog (Phase 3b) is a SEPARATE pass below — disjoint by `run_mode`
        // (this one is Persistent-only; the watchdog is Fresh-only).
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
                // Advance the schedule for THIS fire too — the due loop below
                // skips `acted` tasks, so without this a due supervised task
                // fires here AND again next tick (a double-fire). No-ops for
                // non-Periodic schedules.
                self.advance_after_fire(&tk.task_id, FireOutcome::Fired, now);
            }
        }

        // (c.2) WATCHDOG (Phase 3b) — fresh-hang detection over the SAME loaded
        // snapshot. Disjoint from the Persistent supervision above.
        self.watchdog_pass(&tasks, now);

        // (c.3) PERSISTENT-STALL — the Persistent analogue of the Fresh
        // watchdog: an ALIVE orchestrator that received a fire but produced no
        // transcript growth within the stall budget is wedged. Surfaces only
        // (no auto-act); off unless `persistent_max_stall_secs` is configured.
        self.persistent_stall_pass(&tasks, now);

        // (c.3b) AUTH-EXPIRY + CONSUMER-WEDGE — transcript-tail ground truth
        // for the two failure shapes the 2026-08-03 incident proved invisible:
        // an auth-dead session (every fire guaranteed dead → push-alert, run
        // deliberately left Running) and a Consumer run whose session ended
        // its turn without report_done (auto-close so the due-gate refires,
        // bounded by the wedge-close limit). A closed run is picked up by the
        // NEXT tick's disk load — this tick's `tasks` snapshot predates it.
        self.auth_wedge_pass(&tasks, now);

        // (c.3c) CREDENTIALS PREFLIGHT — a truncated/invalid
        // ~/.claude/.credentials.json makes every claude fire dead before any
        // transcript says so; push-alert as soon as the file itself is broken.
        self.credentials_preflight(&tasks, now);

        // (c.4) CONSUMER DEPTH SNAPSHOT (Phase 4) — poll the planning API's
        // queue stats for the queues of eligible Consumer tasks, throttled by
        // the per-queue depth cache. Pure data for the due check below.
        let depths = self.consumer_depth_snapshot(&tasks, now);

        // (d) DUE CHECK — pure read of the disk-loaded tasks (+ the depth
        // snapshot for Consumer). Cron / OnDemand are skipped.
        let due = collect_due(&tasks, now, &depths);

        // (e) FIRE + ADVANCE — no DaemonState lock held across the fire.
        for task_id in due {
            if acted.contains(&task_id) {
                continue;
            }
            // DUE-SKIP-ACTIVE (Phase 3b): a task whose prior run is still
            // Running passes collect_due's `in_flight.is_none()` filter (trigger
            // clears in_flight on return) but must NOT be re-fired. For FRESH
            // that would pile up overlapping fresh sessions; for a Consumer it
            // would paste a second batch into a mid-run orchestrator (both run
            // modes — a Consumer's prompt must end in `report_done`, same
            // contract as periodic-fresh). Skip the fire but still ADVANCE
            // next_fire_at. Periodic Persistent tasks are exempt (one live
            // session; re-delivery to the same PTY is the intended behavior).
            if let Some(tk) = tasks.iter().find(|t| t.task_id == task_id) {
                if run_active_blocks_fire(tk) {
                    self.advance_after_fire(&task_id, FireOutcome::SkipActive, now);
                    continue;
                }
            }
            let fire_token = mint_fire_token();
            let outcome = self.fire_task(&task_id, &fire_token);
            self.advance_after_fire(&task_id, outcome, now);
        }
    }

    /// Phase (c.4): per-queue pending depths for the Consumer due-check.
    /// Only queues of ELIGIBLE Consumer tasks (enabled, un-paused, past their
    /// `next_fire_at` gate) are polled; each queue at most once per
    /// [`QUEUE_DEPTH_CACHE_TTL_SECS`] (fresh cache entries serve the interim
    /// ticks). A fetch failure logs once and caches depth 0 for the TTL, so an
    /// API outage cannot hot-loop error logs — and cannot fire the task (a
    /// depth-0 Consumer is never due). No DaemonState lock is held across the
    /// HTTP call (creds are cloned out under a brief lock).
    fn consumer_depth_snapshot(
        &self,
        tasks: &[ContinuousTask],
        now: u64,
    ) -> HashMap<String, u64> {
        let mut wanted: Vec<String> = Vec::new();
        for t in tasks {
            if !(t.enabled && !t.paused && t.next_fire_at <= now) {
                continue;
            }
            if let Schedule::Consumer { queue, .. } = &t.schedule {
                if !wanted.contains(queue) {
                    wanted.push(queue.clone());
                }
            }
        }
        if wanted.is_empty() {
            return HashMap::new();
        }
        #[cfg(test)]
        {
            let g = self.depth_override.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(overrides) = g.as_ref() {
                return wanted
                    .iter()
                    .map(|q| (q.clone(), overrides.get(q).copied().unwrap_or(0)))
                    .collect();
            }
        }
        let (api_url, api_token) = {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (s.config.api_url.clone(), s.config.api_token.clone())
        };
        let mut out = HashMap::new();
        for q in wanted {
            // Serve from cache when fresh.
            {
                let cache = self.queue_depths.lock().unwrap_or_else(|p| p.into_inner());
                if let Some((fetched_at, depth)) = cache.get(&q) {
                    if now.saturating_sub(*fetched_at) < QUEUE_DEPTH_CACHE_TTL_SECS {
                        out.insert(q.clone(), *depth);
                        continue;
                    }
                }
            }
            // Fetch (10s ureq timeout bounds a hung API; the failure path
            // caches 0 so the stall repeats at most once per TTL per queue).
            let depth = match crate::continuous::queue::QueueClient::from_overrides(
                Some(&api_url),
                Some(&api_token),
            )
            .and_then(|c| c.stats(&q))
            {
                Ok(stats) => stats.pending,
                Err(e) => {
                    eprintln!(
                        "cm-daemon: continuous scheduler failed to poll depth for \
                         queue '{}': {}",
                        q,
                        e.to_method_err().1,
                    );
                    0
                }
            };
            self.queue_depths
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(q.clone(), (now, depth));
            out.insert(q, depth);
        }
        out
    }

    /// Test-only: canned per-queue depths for the Consumer due-check.
    #[cfg(test)]
    fn set_depth_override(&self, depths: HashMap<String, u64>) {
        *self.depth_override.lock().unwrap_or_else(|p| p.into_inner()) = Some(depths);
    }

    /// Phase (c.2) WATCHDOG — FRESH-hang detection + investigator dispatch
    /// (Phase 3b, DESIGN_CONTINUOUS_TASKS.md §11). DISJOINT from the
    /// dead-persistent supervision pass: that one requires
    /// `RunMode::Persistent` + `supervise`; this one requires `RunMode::Fresh`,
    /// so no task is matched by both.
    ///
    /// A fresh run is STUCK when its session is ALIVE but has overrun its
    /// `max_runtime_secs` budget without finishing (no `report_done`, no clean
    /// exit). A DEAD-but-`Running` session is NOT the watchdog's job — that is
    /// `handle_session_exit` (clean exit → Done) or `reconcile_orphans` (leaked
    /// `in_flight` → Orphaned); the `session_is_dead(stuck_uid) == false` guard
    /// deliberately excludes it. `max_runtime_secs == None` ⇒ watchdog OFF for
    /// that task; `last_run.session_uid == None` ⇒ not a candidate.
    ///
    /// On STUCK, three mutually-exclusive branches:
    ///   1. a LIVE investigator is already on this run (`investigator_uid` set
    ///      AND alive) → do nothing this tick.
    ///   2. under the investigation cap (`investigation_count <
    ///      max_investigations`) → SNAPSHOT the evidence + SPAWN a fresh
    ///      investigator (`spawn_investigator` itself records `investigator_uid`
    ///      / bumps `investigation_count` / appends the runs.jsonl `"stuck"`
    ///      line).
    ///   3. cap reached → AUTO-ESCALATE (`escalate_stuck`: kill + `last_run` →
    ///      Stuck + clear investigator + runs.jsonl `"escalated"`).
    ///
    /// LOCK DISCIPLINE: the only locks taken are the brief, read-only
    /// `session_is_dead` / `session_transcript_path` / config probes, each
    /// dropped BEFORE the snapshot fs-copies / `spawn_investigator` /
    /// `escalate_stuck` (all of which re-acquire the DaemonState lock
    /// internally — holding it across them would deadlock).
    fn watchdog_pass(&self, tasks: &[ContinuousTask], now: u64) {
        let (max_investigations, investigator_budget) = {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (
                s.config.scheduler.max_investigations,
                s.config.scheduler.default_investigator_runtime_secs,
            )
        };

        for tk in tasks {
            // FRESH-only + enabled + NOT paused. A task paused mid-run keeps
            // its live run, but the operator paused it deliberately — the
            // watchdog must not investigate / escalate-kill a paused task's run.
            if tk.run_mode != task::RunMode::Fresh || !tk.enabled || tk.paused {
                continue;
            }
            // Must have an ACTIVE run.
            let Some(run) = tk.last_run.as_ref() else {
                continue;
            };
            if run.status != RunStatus::Running {
                continue;
            }
            // Watchdog OFF when no runtime budget is configured.
            let Some(max_runtime) = tk.max_runtime_secs else {
                continue;
            };
            // No session to watch ⇒ not a candidate.
            let Some(stuck_uid) = run.session_uid.as_deref() else {
                continue;
            };
            // Within budget ⇒ healthy.
            let elapsed = now.saturating_sub(run.started_at);
            if elapsed <= max_runtime as u64 {
                continue;
            }
            // A DEAD session past its budget is handle_session_exit /
            // reconcile_orphans territory — the watchdog only acts on an
            // ALIVE-but-overrunning run.
            if self.session_is_dead(stuck_uid) {
                continue;
            }
            let seq = run.seq;

            // Branch 1: a LIVE investigator is already on this run → wait it out,
            // UNLESS it has itself blown its own runtime budget. A wedged
            // investigator (never calls resolve_stuck, never exits) would
            // otherwise pin this run forever — Branch 1 would `continue` every
            // tick. Past its budget, ABANDON it (kill + clear binding); next tick
            // the count check re-investigates or auto-escalates. A `None`
            // started_at (pre-3b-fix state.json) reads as elapsed 0 = within
            // budget, never a false abandon. (A CRASHED investigator self-heals:
            // session_is_dead == true falls through to the count check below.)
            if let Some(inv_uid) = tk.investigator_uid.as_deref() {
                if !self.session_is_dead(inv_uid) {
                    let inv_started = tk.investigator_started_at.unwrap_or(now);
                    if now.saturating_sub(inv_started) <= investigator_budget as u64 {
                        continue;
                    }
                    crate::control::methods::abandon_timed_out_investigator(
                        &self.state,
                        tk,
                        seq,
                        inv_uid,
                    );
                    continue;
                }
            }

            if tk.investigation_count < max_investigations {
                // Branch 2: SNAPSHOT the evidence (transcript_path resolved under
                // a brief lock, then DROP before the fs-copies) + SPAWN a fresh
                // investigator. `spawn_investigator` records investigator_uid /
                // investigation_count + the runs.jsonl "stuck" line itself.
                let transcript = self.session_transcript_path(stuck_uid);
                let snapshot_dir = crate::control::methods::snapshot_stuck_run(
                    tk,
                    seq,
                    transcript.as_deref(),
                    elapsed,
                    "max_runtime_exceeded",
                );
                if let Err((code, msg)) = crate::control::methods::spawn_investigator(
                    &self.state,
                    tk,
                    seq,
                    &snapshot_dir,
                ) {
                    eprintln!(
                        "cm-daemon: continuous watchdog failed to spawn investigator \
                         for task={} seq={}: {:?}: {}",
                        tk.task_id, seq, code, msg,
                    );
                }
            } else {
                // Branch 3: cap reached → AUTO-ESCALATE (kill + last_run → Stuck +
                // runs.jsonl "escalated"). The kill rides kill_session semantics
                // (leave-in-registry → the reaper broadcasts Exited), so no lock
                // is held across it.
                if let Err((code, msg)) = crate::control::methods::escalate_stuck(
                    &self.state,
                    tk,
                    seq,
                    "max_investigations",
                ) {
                    eprintln!(
                        "cm-daemon: continuous watchdog auto-escalate failed for \
                         task={} seq={}: {:?}: {}",
                        tk.task_id, seq, code, msg,
                    );
                }
            }
        }
    }

    /// (c.3) Persistent-orchestrator stall detection — the Persistent analogue
    /// of the Fresh `watchdog_pass`. A persistent orchestrator's session is
    /// long-lived (it never exits between fires), so the Fresh "elapsed since
    /// `started_at` > `max_runtime`" signal is meaningless. The stall signal is
    /// instead: a fire was DELIVERED (`last_fired_at`) but the session's
    /// transcript has NOT grown since (`mtime < last_fired_at`) after the budget
    /// — the orchestrator received the prompt (or an interactive modal swallowed
    /// it) and never produced output. `last_activity_at` can't be used: the
    /// fire's OWN delivery write bumps it, so it can't tell "received-then-
    /// wedged" from "worked-then-idle"; transcript mtime (wall-clock, advances
    /// only on real assistant/tool turns) is the discriminating signal.
    ///
    /// SURFACE-ONLY: a daemon-log alert + a `"stalled"` runs.jsonl line, ONE per
    /// fire episode via the in-memory `stall_alerts` latch. NO auto-kill / auto-
    /// investigate — the dominant cause (a shared-account rate-limit modal, see
    /// `reference_continuous_triage_setup`) would also block any investigator we
    /// spawned, and auto-killing a possibly-healthy orchestrator is too
    /// aggressive for a detector. Recovery stays the operator's call (the
    /// documented `kill_session` → supervisor-respawn + re-fire path).
    ///
    /// OFF unless `[scheduler] persistent_max_stall_secs` is set (opt-in — a
    /// generously large budget avoids false positives on legitimately long,
    /// transcript-silent tool calls). Known limitation: if a modal appears
    /// AFTER the fire's user-turn was already written (mid-turn rate-limit), the
    /// transcript mtime is past `last_fired_at` and this misses it — a safe
    /// direction (a miss, not a false alarm).
    fn persistent_stall_pass(&self, tasks: &[ContinuousTask], now: u64) {
        let (budget, notify_cmd) = {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (
                s.config.scheduler.persistent_max_stall_secs,
                s.config.notify_command.clone(),
            )
        };
        let Some(budget) = budget else {
            return; // OFF (opt-in)
        };
        if budget == 0 {
            return;
        }

        for tk in tasks {
            // Persistent + enabled + NOT paused. A paused task's live session is
            // intentionally idle (operator paused it) — not stalled.
            if tk.run_mode != task::RunMode::Persistent || !tk.enabled || tk.paused {
                continue;
            }
            // Never-fired (just created) is not a stall.
            if tk.last_fired_at == 0 {
                continue;
            }
            // Only a fire older than the budget can be judged stalled.
            if now.saturating_sub(tk.last_fired_at) <= budget as u64 {
                continue;
            }
            // The live orchestrator session. A DEAD session is supervision's job
            // (dead-persistent respawn), NOT the stall detector's.
            let Some(uid) = tk.current_session_uid.as_deref() else {
                continue;
            };
            if self.session_is_dead(uid) {
                continue;
            }
            // Ground truth: has the transcript grown since the fire? A path we
            // can't resolve/stat (detector hasn't bound one yet) is "can't judge"
            // → skip, conservative.
            let Some(path) = self.session_transcript_path(uid) else {
                continue;
            };
            let Some(mtime) = mtime_unix(&path) else {
                continue;
            };
            if mtime >= tk.last_fired_at {
                // Grew since the fire ⇒ healthy. Clear any stale latch so a
                // LATER wedge on a future fire re-alerts.
                self.stall_alerts
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&tk.task_id);
                continue;
            }

            // STALLED. One alert per fire episode (keyed on last_fired_at).
            {
                let mut latch =
                    self.stall_alerts.lock().unwrap_or_else(|p| p.into_inner());
                if latch.get(&tk.task_id) == Some(&tk.last_fired_at) {
                    continue; // already surfaced this episode
                }
                latch.insert(tk.task_id.clone(), tk.last_fired_at);
            }

            let stalled_secs = now.saturating_sub(tk.last_fired_at);
            crate::notify::notify_operator(
                notify_cmd.as_deref(),
                "persistent-stall",
                &format!(
                    "continuous task {} STALLED — fired {}s ago (session {}) but \
                     its transcript has not grown since; the orchestrator may be parked at \
                     an interactive modal. Surfacing only; recovery is the operator's call \
                     (kill the session → supervisor respawns + re-fires).",
                    tk.task_id, stalled_secs, uid,
                ),
            );
            if let Err(e) = ContinuousRunLog::append(&RunLogLine {
                seq: tk.last_run.as_ref().map(|r| r.seq).unwrap_or(tk.run_count as u64),
                ts: now as f64,
                task_id: tk.task_id.clone(),
                event: "stalled".to_string(),
                fire_token: tk.last_run.as_ref().map(|r| r.fire_token.clone()),
                session_uid: Some(uid.to_string()),
                run_mode: Some("persistent".to_string()),
                trigger_source: Some(SCHEDULER_CALLER_TOKEN.to_string()),
                status: Some("running".to_string()),
                detail: Some(
                    format!(
                        "no transcript growth for {}s after fire (budget {}s)",
                        stalled_secs, budget
                    )
                    .into(),
                ),
            }) {
                eprintln!(
                    "cm-daemon: failed to append \"stalled\" audit line for {}: {}",
                    tk.task_id, e,
                );
            }
        }
    }

    /// (c.3b) Auth-expiry + consumer-wedge detection over transcript tails.
    /// Both detectors ride ONE throttled tail probe per task
    /// ([`TAIL_PROBE_INTERVAL_SECS`]) of the ACTIVE run's session transcript.
    ///
    /// Why not the Stop hook: an auth-error turn never runs Stop hooks
    /// (verified in the 2026-08-03 incident transcript — healthy turns log a
    /// `stop_hook_summary`, auth turns don't), so `session.turn_ended` is
    /// blind exactly when it matters. Why not `persistent_stall_pass`: the
    /// fire's own prompt delivery + the synthetic 401 record grow the
    /// transcript PAST `last_fired_at`, so the mtime-vs-fire stall signal
    /// reads "healthy" on an auth-dead session.
    ///
    /// Per task with a `Running` run on a LIVE claude session:
    ///
    ///   1. **Auth expiry** (any schedule / run mode): the newest assistant
    ///      record is a synthetic `authentication_failed` message → runlog
    ///      `"auth_expired"` + operator push alert (per-task
    ///      [`AUTH_REALERT_SECS`] cooldown; healthy tail clears it). The run
    ///      is deliberately LEFT `Running`: every subsequent fire is
    ///      guaranteed dead, and for a Consumer the run-active due-gate is
    ///      the only thing preventing fires from claiming+acking queue items
    ///      into a dead session. Recovery is the operator's `/login` +
    ///      `continuous.force_done` (the alert says exactly that).
    ///   2. **Consumer wedge** (Consumer schedule, both run modes): the tail
    ///      shows a COMPLETED turn (or a delivered-but-unanswered prompt) and
    ///      NOTHING has happened for `[scheduler] consumer_wedge_grace_secs`
    ///      — the agent finished without `report_done` and the due-gate would
    ///      skip every future fire forever (the 3.5-day incident shape).
    ///      Under `[scheduler] wedge_close_limit` consecutive closes:
    ///      auto-close (`Running → Failed` + runlog `"wedge_closed"` + alert);
    ///      the due-gate then refires naturally next tick. At the limit:
    ///      escalate once per run (runlog `"wedge_escalated"` + alert) and
    ///      leave the run `Running` so a chronically-broken consumer stops
    ///      burning queue items on close→refire churn.
    ///
    /// A `MidTurn` tail (newest record is a healthy assistant record, e.g. a
    /// long blocking tool call) is never touched. LOCK DISCIPLINE: brief
    /// probes only; `task::modify` / notify spawns run lock-free.
    fn auth_wedge_pass(&self, tasks: &[ContinuousTask], now: u64) {
        use crate::continuous::probe::{self, TailShape};

        let (grace, wedge_limit, notify_cmd) = {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (
                s.config.scheduler.consumer_wedge_grace_secs,
                s.config.scheduler.wedge_close_limit,
                s.config.notify_command.clone(),
            )
        };

        for tk in tasks {
            // Claude-only: the tail shapes (turn_duration records, the
            // authentication_failed marker) are claude-code transcript vocab.
            if tk.engine != task::Engine::Claude || !tk.enabled || tk.paused {
                continue;
            }
            let Some(run) = tk.last_run.as_ref() else {
                continue;
            };
            if run.status != RunStatus::Running {
                continue;
            }
            let Some(uid) = run.session_uid.as_deref() else {
                continue;
            };
            // A DEAD session's run is handle_session_exit / reconcile_orphans
            // territory.
            if self.session_is_dead(uid) {
                continue;
            }
            // Per-task probe throttle.
            {
                let mut probes = self.probe_at.lock().unwrap_or_else(|p| p.into_inner());
                let last = probes.get(&tk.task_id).copied().unwrap_or(0);
                if now.saturating_sub(last) < TAIL_PROBE_INTERVAL_SECS {
                    continue;
                }
                probes.insert(tk.task_id.clone(), now);
            }
            // No transcript bound yet / unreadable → can't judge, skip.
            let Some(path) = self.session_transcript_path(uid) else {
                continue;
            };
            let Some(mtime) = mtime_unix(&path) else {
                continue;
            };
            let Some(tail) = probe::probe_transcript_tail(&path) else {
                continue;
            };

            // --- 1. auth expiry (any schedule) ---
            if let Some(banner) = tail.auth_error.as_deref() {
                self.surface_auth_expiry(tk, run.seq, uid, banner, notify_cmd.as_deref(), now);
                // NEVER fall through to the wedge close: the blocked due-gate
                // is protecting queue items from dead fires.
                continue;
            }
            self.auth_alerts
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&tk.task_id);

            // --- 2. consumer wedge ---
            if grace == 0 || !matches!(tk.schedule, Schedule::Consumer { .. }) {
                continue;
            }
            // Quiet window: nothing since the newest of (transcript growth,
            // fire delivery, run start). `last_fired_at` guards the paste-
            // in-flight window right after a fire whose prompt hasn't landed
            // in the transcript yet.
            let quiet_since = mtime.max(run.started_at).max(tk.last_fired_at);
            if now.saturating_sub(quiet_since) < grace {
                continue;
            }
            match tail.shape {
                TailShape::MidTurn => continue, // long tool call — healthy
                TailShape::TurnComplete | TailShape::AwaitingResponse => {}
            }
            let quiet_secs = now.saturating_sub(quiet_since);
            if tk.consecutive_wedge_closes >= wedge_limit {
                self.escalate_wedged_run(
                    tk,
                    run.seq,
                    uid,
                    quiet_secs,
                    wedge_limit,
                    notify_cmd.as_deref(),
                    now,
                );
            } else {
                self.close_wedged_run(tk, run.seq, uid, quiet_secs, wedge_limit, notify_cmd.as_deref(), now);
            }
        }
    }

    /// Auth-expiry surfacing: runlog `"auth_expired"` + push alert, at most
    /// once per [`AUTH_REALERT_SECS`] per task while the condition persists.
    fn surface_auth_expiry(
        &self,
        tk: &ContinuousTask,
        seq: u64,
        uid: &str,
        banner: &str,
        notify_cmd: Option<&str>,
        now: u64,
    ) {
        {
            let mut latch = self.auth_alerts.lock().unwrap_or_else(|p| p.into_inner());
            let last = latch.get(&tk.task_id).copied().unwrap_or(0);
            if now.saturating_sub(last) < AUTH_REALERT_SECS {
                return;
            }
            latch.insert(tk.task_id.clone(), now);
        }
        crate::notify::notify_operator(
            notify_cmd,
            "auth-expired",
            &format!(
                "claude auth EXPIRED: continuous task '{}' (session {}) ended its turn \
                 with \"{}\". Every scheduled fire is dead until /login is re-run on the \
                 daemon host. Run seq {} is left Running to block further fires; after \
                 re-login, close it with continuous.force_done(task_id=\"{}\", seq={}).",
                tk.task_id, uid, banner, seq, tk.task_id, seq,
            ),
        );
        if let Err(e) = ContinuousRunLog::append(&RunLogLine {
            seq,
            ts: now as f64,
            task_id: tk.task_id.clone(),
            event: "auth_expired".to_string(),
            fire_token: tk.last_run.as_ref().map(|r| r.fire_token.clone()),
            session_uid: Some(uid.to_string()),
            run_mode: None,
            trigger_source: Some(SCHEDULER_CALLER_TOKEN.to_string()),
            status: Some("running".to_string()),
            detail: Some(banner.to_string().into()),
        }) {
            eprintln!(
                "cm-daemon: failed to append \"auth_expired\" audit line for {}: {}",
                tk.task_id, e,
            );
        }
    }

    /// Wedge auto-close: `Running → Failed` (guarded on seq/uid/status so a
    /// concurrent `report_done` / new fire wins), bump the consecutive-close
    /// counter, audit + alert. The due-gate refires naturally next tick.
    fn close_wedged_run(
        &self,
        tk: &ContinuousTask,
        seq: u64,
        uid: &str,
        quiet_secs: u64,
        wedge_limit: u32,
        notify_cmd: Option<&str>,
        now: u64,
    ) {
        let mut closed = false;
        let mut closes = tk.consecutive_wedge_closes;
        let _ = task::modify(&tk.task_id, |t| {
            if let Some(run) = t.last_run.as_mut() {
                if run.seq == seq
                    && run.status == RunStatus::Running
                    && run.session_uid.as_deref() == Some(uid)
                {
                    run.status = RunStatus::Failed;
                    run.finished_at = Some(now);
                    t.consecutive_wedge_closes = t.consecutive_wedge_closes.saturating_add(1);
                    closes = t.consecutive_wedge_closes;
                    closed = true;
                }
            }
        });
        if !closed {
            return; // raced with report_done / a newer fire — nothing wedged
        }
        crate::notify::notify_operator(
            notify_cmd,
            "consumer-wedge",
            &format!(
                "continuous task '{}' run seq {} WEDGED: session {} ended its turn \
                 without report_done and produced nothing for {}s. Auto-closed \
                 (Running → Failed); the due-gate will refire. (consecutive close \
                 {}/{} — at the limit the scheduler escalates instead)",
                tk.task_id, seq, uid, quiet_secs, closes, wedge_limit,
            ),
        );
        if let Err(e) = ContinuousRunLog::append(&RunLogLine {
            seq,
            ts: now as f64,
            task_id: tk.task_id.clone(),
            event: "wedge_closed".to_string(),
            fire_token: tk.last_run.as_ref().map(|r| r.fire_token.clone()),
            session_uid: Some(uid.to_string()),
            run_mode: None,
            trigger_source: Some(SCHEDULER_CALLER_TOKEN.to_string()),
            status: Some("failed".to_string()),
            detail: Some(
                format!(
                    "turn ended without report_done; quiet {}s (grace elapsed); \
                     consecutive close {}/{}",
                    quiet_secs, closes, wedge_limit,
                )
                .into(),
            ),
        }) {
            eprintln!(
                "cm-daemon: failed to append \"wedge_closed\" audit line for {}: {}",
                tk.task_id, e,
            );
        }
    }

    /// Wedge escalation (close limit exhausted): one-shot per run seq. The run
    /// is LEFT `Running` — with a chronically-wedging consumer, every further
    /// close→refire claims and acks real queue items into a broken
    /// orchestrator, so blocking fires is the protective choice.
    fn escalate_wedged_run(
        &self,
        tk: &ContinuousTask,
        seq: u64,
        uid: &str,
        quiet_secs: u64,
        wedge_limit: u32,
        notify_cmd: Option<&str>,
        now: u64,
    ) {
        {
            let mut latch = self
                .wedge_escalations
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if latch.get(&tk.task_id) == Some(&seq) {
                return; // this run already escalated
            }
            latch.insert(tk.task_id.clone(), seq);
        }
        crate::notify::notify_operator(
            notify_cmd,
            "consumer-wedge",
            &format!(
                "continuous task '{}' run seq {} WEDGED AGAIN (turn ended without \
                 report_done, quiet {}s) and the auto-close limit ({}) is exhausted — \
                 {} consecutive runs wedged. Leaving the run Running so fires stay \
                 blocked (each refire would burn queue items). Investigate the \
                 orchestrator (session {}); once fixed, recover with \
                 continuous.force_done(task_id=\"{}\", seq={}).",
                tk.task_id,
                seq,
                quiet_secs,
                wedge_limit,
                tk.consecutive_wedge_closes,
                uid,
                tk.task_id,
                seq,
            ),
        );
        if let Err(e) = ContinuousRunLog::append(&RunLogLine {
            seq,
            ts: now as f64,
            task_id: tk.task_id.clone(),
            event: "wedge_escalated".to_string(),
            fire_token: tk.last_run.as_ref().map(|r| r.fire_token.clone()),
            session_uid: Some(uid.to_string()),
            run_mode: None,
            trigger_source: Some(SCHEDULER_CALLER_TOKEN.to_string()),
            status: Some("running".to_string()),
            detail: Some(
                format!(
                    "wedge-close limit {} exhausted after {} consecutive wedged runs; \
                     run left Running to block fires",
                    wedge_limit, tk.consecutive_wedge_closes,
                )
                .into(),
            ),
        }) {
            eprintln!(
                "cm-daemon: failed to append \"wedge_escalated\" audit line for {}: {}",
                tk.task_id, e,
            );
        }
    }

    /// (c.3c) Credentials preflight: when any enabled claude-engine continuous
    /// task exists, structurally validate `~/.claude/.credentials.json` every
    /// [`CREDS_CHECK_INTERVAL_SECS`]. A file that EXISTS but is truncated /
    /// unparseable / token-less (the 2026-08-03 incident began exactly there —
    /// 243 bytes of a ~500-byte file) means every subsequent claude fire is
    /// dead; push-alert without waiting for a session to prove it. Missing
    /// file = healthy (keychain / API-key setups). Alert cooldown
    /// [`AUTH_REALERT_SECS`]; a healthy re-check clears it. Returns whether an
    /// alert fired this call (tests observe it; the tick ignores it).
    fn credentials_preflight(&self, tasks: &[ContinuousTask], now: u64) -> bool {
        if !tasks
            .iter()
            .any(|t| t.enabled && !t.paused && t.engine == task::Engine::Claude)
        {
            return false;
        }
        {
            let mut st = self.creds_state.lock().unwrap_or_else(|p| p.into_inner());
            if now.saturating_sub(st.0) < CREDS_CHECK_INTERVAL_SECS {
                return false;
            }
            st.0 = now;
        }
        let Some(home) = std::env::var_os("HOME") else {
            return false;
        };
        let path = std::path::PathBuf::from(home).join(".claude/.credentials.json");
        let problem = crate::continuous::probe::credentials_file_problem(&path);
        {
            let mut st = self.creds_state.lock().unwrap_or_else(|p| p.into_inner());
            match problem.as_deref() {
                None => {
                    st.1 = 0; // healthy — re-arm the alert
                    return false;
                }
                Some(_) => {
                    if now.saturating_sub(st.1) < AUTH_REALERT_SECS {
                        return false;
                    }
                    st.1 = now;
                }
            }
        }
        let reason = problem.unwrap_or_default();
        let notify_cmd = {
            let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
            s.config.notify_command.clone()
        };
        crate::notify::notify_operator(
            notify_cmd.as_deref(),
            "auth-expired",
            &format!(
                "claude credentials file {} is INVALID ({}) — every continuous \
                 claude fire on this host will fail with auth errors until \
                 /login is re-run.",
                path.display(),
                reason,
            ),
        );
        true
    }

    /// Phase (a): orphan any task whose `in_flight` spawn-window guard outlived
    /// its spawn window AND whose guarded session is dead (registry-absent or
    /// kernel-exited). Marks `last_run.status = Orphaned`, clears `in_flight`,
    /// and appends an `"orphaned"` runs.jsonl line. This closes the Phase-2
    /// in_flight-leak residual (a guard left set on an unclean fire path); an
    /// overdue on-disk guard from before a daemon restart clears here too.
    fn reconcile_orphans(&self, now: u64) {
        for tk in task::load_all() {
            // A run can be stranded `Running` with no live reaper to finish it
            // when the daemon restarts / crashes / is SIGKILLed mid-run (systemd
            // kills the whole cgroup, so the child dies without its clean-exit
            // hook). Two shapes:
            //   (1) `in_flight` is still armed — the fire died during/just after
            //       spawn, before the record step cleared the guard; OR
            //   (2) the record step already cleared `in_flight`, but `last_run`
            //       is still `Running` against a now-dead session. This is the
            //       common restart-mid-run case — and the one that silently
            //       WEDGES a fresh periodic task forever: `fresh_run_active`
            //       stays true, every due tick takes SkipActive, and the task
            //       never fires again until a manual run_now.
            // Resolve (session_uid, started_at, seq, fire_token) from whichever
            // guard is present, then orphan the run + clear `in_flight`.
            let stranded: Option<(String, u64, u64, String)> =
                if let Some(inflight) = tk.in_flight.as_ref() {
                    let seq = tk
                        .last_run
                        .as_ref()
                        .map(|r| r.seq)
                        .unwrap_or(tk.run_count as u64);
                    Some((
                        inflight.session_uid.clone(),
                        inflight.started_at,
                        seq,
                        inflight.fire_token.clone(),
                    ))
                } else {
                    match tk.last_run.as_ref() {
                        Some(lr)
                            if lr.status == RunStatus::Running && lr.session_uid.is_some() =>
                        {
                            Some((
                                lr.session_uid.clone().unwrap(),
                                lr.started_at,
                                lr.seq,
                                lr.fire_token.clone(),
                            ))
                        }
                        _ => None,
                    }
                };
            let Some((session_uid, started_at, seq, fire_token)) = stranded else {
                continue;
            };
            // Spawn-window grace: a just-armed guard / just-started run whose
            // session isn't in the registry yet is a healthy mid-spawn fire.
            if now.saturating_sub(started_at) < RECONCILE_GRACE_SECS {
                continue;
            }
            if !self.session_is_dead(&session_uid) {
                continue;
            }
            let _ = task::modify(&tk.task_id, |t| {
                if let Some(lr) = t.last_run.as_mut() {
                    lr.status = RunStatus::Orphaned;
                    // A reconciled orphan IS terminal — stamp finished_at so the
                    // run record and audit aren't left open forever.
                    if lr.finished_at.is_none() {
                        lr.finished_at = Some(now);
                    }
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
                trigger_source: Some(SCHEDULER_CALLER_TOKEN.to_string()),
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

    /// Phase (e) advance: record the fire and recompute `next_fire_at`.
    /// CATCH-UP-ONCE — `next_fire_at = now + spacing` recomputed from `now`,
    /// NEVER accumulated from `last_fired_at` (backfilling missed slots after
    /// downtime would fire a burst). On a hard failure, bump
    /// `consecutive_failures` and back the next fire off exponentially (capped);
    /// a successful fire resets the counter; a benign skip leaves `next_fire_at`
    /// to retry without bumping failures.
    ///
    /// `spacing` per schedule: Periodic uses `every_secs` (the cadence).
    /// Consumer (Phase 4) uses [`CONSUMER_REFIRE_SPACING_SECS`] — for a Consumer
    /// `next_fire_at` is a NOT-BEFORE gate, not a cadence: due-ness comes from
    /// queue depth / window in `collect_due`, and the small spacing just rate-
    /// limits re-evaluation (drain a deep queue at ≤1 batch/min; recheck a
    /// run-active skip in a minute; back a failure off from a 60s base).
    fn advance_after_fire(&self, task_id: &str, outcome: FireOutcome, now: u64) {
        let mut auto_paused: Option<(u64, u32)> = None; // (run seq, failure count)
        let _ = task::modify(task_id, |t| {
            let every = match &t.schedule {
                Schedule::Periodic { every_secs } => (*every_secs).max(1),
                Schedule::Consumer { .. } => CONSUMER_REFIRE_SPACING_SECS,
                // Cron / OnDemand can't reach here (collect_due filters them);
                // be defensive and leave scheduling untouched.
                _ => return,
            };
            t.last_fired_at = now;
            match outcome {
                FireOutcome::Fired => {
                    t.consecutive_failures = 0;
                    t.next_fire_at = now.saturating_add(every);
                }
                FireOutcome::FiredCompact => {
                    // A compact-only fire: like Fired, but the next fire must
                    // clear the async `/compact` summarization turn — a prompt
                    // pasted mid-summarization is dropped. Floor the spacing.
                    t.consecutive_failures = 0;
                    t.next_fire_at =
                        now.saturating_add(every.max(COMPACT_SETTLE_SPACING_SECS));
                }
                FireOutcome::Skipped => {
                    // Benign skip (busy/paused/duplicate): the in_flight guard /
                    // paused flag already blocks re-selection; leave
                    // next_fire_at to retry next due tick, and do NOT bump
                    // failures (the task is healthy).
                }
                FireOutcome::SkipActive => {
                    // DUE-SKIP-ACTIVE (Phase 3b): a fresh run is still Running,
                    // so the due loop declined to overlap a second fresh run.
                    // ADVANCE catch-up-once (like Fired) so the schedule keeps
                    // moving — otherwise this still-Running task would re-qualify
                    // as due every 250 ms tick. Do NOT bump (or reset) failures:
                    // the skip is benign, not a fire and not a failure.
                    t.next_fire_at = now.saturating_add(every);
                }
                FireOutcome::Failed => {
                    t.consecutive_failures = t.consecutive_failures.saturating_add(1);
                    t.next_fire_at =
                        now.saturating_add(backoff_secs(every, t.consecutive_failures));
                    // Circuit breaker: after too many consecutive fire failures,
                    // stop churning — pause the task and alert the operator, who
                    // un-pauses after fixing the cause.
                    if t.consecutive_failures >= CONSECUTIVE_FAILURE_LIMIT && !t.paused {
                        t.paused = true;
                        let seq =
                            t.last_run.as_ref().map(|r| r.seq).unwrap_or(t.run_count as u64);
                        auto_paused = Some((seq, t.consecutive_failures));
                    }
                }
            }
        });
        if let Some((seq, fails)) = auto_paused {
            let notify_cmd = {
                let s = self.state.lock().unwrap_or_else(|p| p.into_inner());
                s.config.notify_command.clone()
            };
            crate::notify::notify_operator(
                notify_cmd.as_deref(),
                "circuit-breaker",
                &format!(
                    "continuous task {} AUTO-PAUSED (circuit breaker) after {} \
                     consecutive fire failures — un-pause it after fixing the cause",
                    task_id, fails,
                ),
            );
            if let Err(e) = ContinuousRunLog::append(&RunLogLine {
                seq,
                ts: now as f64,
                task_id: task_id.to_string(),
                event: "auto_paused".to_string(),
                fire_token: None,
                session_uid: None,
                run_mode: None,
                trigger_source: Some(SCHEDULER_CALLER_TOKEN.to_string()),
                status: Some("paused".to_string()),
                detail: Some(
                    format!("circuit breaker: {} consecutive fire failures", fails).into(),
                ),
            }) {
                eprintln!(
                    "cm-daemon: failed to append \"auto_paused\" audit line for {}: {}",
                    task_id, e,
                );
            }
        }
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
        let caller = Caller::operator(SCHEDULER_CALLER_TOKEN);
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
                    if v.get("compact").and_then(|c| c.as_bool()).unwrap_or(false) {
                        FireOutcome::FiredCompact
                    } else {
                        FireOutcome::Fired
                    }
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

    /// Brief, read-only lookup of a session's resolved transcript path — used by
    /// the watchdog to thread the stuck run's transcript into
    /// [`crate::control::methods::snapshot_stuck_run`] WITHOUT holding the lock
    /// across the fs-copies (same collect-then-act discipline as
    /// [`Self::session_is_dead`]; the lock drops on return). `None` when the
    /// session is registry-absent or the transcript detector hasn't bound a path
    /// yet (a trust-dialog hang may never produce one — the snapshot then skips
    /// the transcript artifact, best-effort).
    fn session_transcript_path(&self, uid: &str) -> Option<std::path::PathBuf> {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state
            .sessions
            .get(uid)
            .and_then(|s| s.transcript_path.clone())
            .map(std::path::PathBuf::from)
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

/// Phase 3b DUE-SKIP-ACTIVE predicate: a FRESH task whose most recent run is
/// still `Running`. The periodic due loop declines to re-fire such a task (no
/// overlapping fresh runs) but still advances `next_fire_at` catch-up-once.
/// Persistent tasks are EXEMPT — they reuse one live session, so re-delivering
/// the prompt to the same PTY is the intended persistent behavior, not a skip.
fn fresh_run_active(tk: &ContinuousTask) -> bool {
    tk.run_mode == task::RunMode::Fresh
        && tk
            .last_run
            .as_ref()
            .map_or(false, |r| r.status == RunStatus::Running)
}

/// Due-loop skip predicate, generalizing [`fresh_run_active`] for Phase 4:
/// a CONSUMER task with a `Running` last run blocks in BOTH run modes — firing
/// would claim a fresh batch and paste it into a mid-run orchestrator. The
/// Consumer contract is therefore periodic-fresh-shaped: the prompt MUST end in
/// `report_done` (or the session must exit) or the queue never drains again —
/// recoverable via `continuous.run_now`, same as the documented periodic-fresh
/// footgun, or via the operator break-glass `continuous.force_done`. Periodic
/// tasks keep the Phase-3b fresh-only semantics. `pub(crate)` so the trigger
/// compact-boundary regression tests can assert the gate directly.
pub(crate) fn run_active_blocks_fire(tk: &ContinuousTask) -> bool {
    if matches!(tk.schedule, Schedule::Consumer { .. }) {
        return tk
            .last_run
            .as_ref()
            .map_or(false, |r| r.status == RunStatus::Running);
    }
    fresh_run_active(tk)
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
        trigger_source: Some(SCHEDULER_CALLER_TOKEN.to_string()),
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
/// Common gate: `enabled && !paused && in_flight.is_none() && next_fire_at <=
/// now`. Then per schedule:
///   - `Periodic` — due (the gate IS the cadence).
///   - `Consumer` (Phase 4) — due iff the queue has pending items AND (depth ≥
///     `depth_threshold` (burst arm; a 0 threshold disables it) OR the oldest
///     wait exceeds `window_secs` — approximated as time since the last fire; a
///     never-fired task (`last_fired_at == 0`) satisfies the window arm
///     outright, so the first item ever enqueued fires without a full window
///     wait). `queue_depths` is the (c.4) snapshot; a queue missing from it
///     reads as depth 0 (not due).
///   - `Cron` / `OnDemand` — never due here.
/// Pure read — no DaemonState lock, no I/O.
fn collect_due(
    tasks: &[ContinuousTask],
    now: u64,
    queue_depths: &HashMap<String, u64>,
) -> Vec<String> {
    tasks
        .iter()
        .filter(|t| {
            t.enabled && !t.paused && t.in_flight.is_none() && t.next_fire_at <= now
        })
        .filter(|t| match &t.schedule {
            Schedule::Periodic { .. } => true,
            Schedule::Consumer {
                queue,
                window_secs,
                depth_threshold,
                ..
            } => {
                let depth = queue_depths.get(queue).copied().unwrap_or(0);
                depth > 0
                    && ((*depth_threshold > 0 && depth >= *depth_threshold as u64)
                        || t.last_fired_at == 0
                        || now.saturating_sub(t.last_fired_at) >= *window_secs)
            }
            _ => false,
        })
        .map(|t| t.task_id.clone())
        .collect()
}

/// A file's mtime as unix seconds, or `None` if it can't be stat'd (missing /
/// permission / pre-epoch). Same wall-clock as [`task::now_unix`], so it
/// compares directly against a task's `last_fired_at`. Used by
/// [`ContinuousScheduler::persistent_stall_pass`] to read transcript growth.
fn mtime_unix(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
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

    #[test]
    fn reconcile_orphans_recovers_dead_but_running_run_with_no_in_flight() {
        // The restart-mid-run wedge (both HIGH findings): the record step
        // already cleared in_flight, but last_run is still Running against a
        // session that died with the daemon. Without reconciliation this stays
        // Running forever -> fresh_run_active true -> every due tick SkipActives
        // -> the task never fires again.
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            let now = 10_000u64;
            let mut t = periodic_task("wedge", 100, now);
            t.in_flight = None; // record step cleared it; the run outlived it
            t.last_run = Some(RunRecord {
                seq: 1,
                fire_token: "ft-wedge".into(),
                started_at: now - 1000, // past RECONCILE_GRACE_SECS
                finished_at: None,
                session_uid: Some("ts-dead-uid".into()), // absent from registry => dead
                status: RunStatus::Running,
                trigger_source: "operator".into(),
            });
            task::save(&t).expect("save");

            assert!(fresh_run_active(&t), "precondition: wedged Running run");
            sched.reconcile_orphans(now);

            let reloaded = task::load_one("wedge").expect("load");
            let lr = reloaded.last_run.as_ref().unwrap();
            assert_eq!(lr.status, RunStatus::Orphaned, "dead-but-Running run orphaned");
            assert_eq!(lr.finished_at, Some(now), "orphan stamps finished_at");
            assert!(!fresh_run_active(&reloaded), "schedule is un-wedged");
            let runs =
                std::fs::read_to_string(task::runs_log_path("wedge")).expect("runs.jsonl");
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
        // Empty depth snapshot: the Consumer task reads depth 0 -> not due.
        assert_eq!(
            collect_due(&tasks, now, &HashMap::new()),
            vec!["due".to_string()],
        );
    }

    // ----- (d) due check: Consumer (Phase 4) -----

    fn consumer_task(id: &str, queue: &str, window_secs: u64, depth_threshold: u32) -> ContinuousTask {
        let mut t = ContinuousTask::new(
            id.into(),
            id.into(),
            format!("ws-{}", id),
            "/tmp/r".into(),
            Engine::Claude,
            RunMode::Persistent,
            Schedule::Consumer {
                queue: queue.into(),
                batch_max: 10,
                window_secs,
                depth_threshold,
            },
            "drain the batch".into(),
        );
        t.next_fire_at = 0;
        t
    }

    /// Consumer due arms: depth ≥ threshold fires regardless of window; below
    /// threshold the window (measured from last_fired_at) decides; an empty
    /// queue never fires; a 0 threshold disables the burst arm.
    #[test]
    fn collect_due_consumer_depth_and_window_arms() {
        let now = 10_000u64;
        let depths: HashMap<String, u64> =
            [("q".to_string(), 2u64)].into_iter().collect();

        // Burst arm: threshold 2 met by depth 2 — due even mid-window.
        let mut burst = consumer_task("burst", "q", 100_000, 2);
        burst.last_fired_at = now - 10;
        // Below threshold (3 > 2) + mid-window — not due.
        let mut waiting = consumer_task("waiting", "q", 100_000, 3);
        waiting.last_fired_at = now - 10;
        // Below threshold but window elapsed — due (straggler drain).
        let mut straggler = consumer_task("straggler", "q", 50, 3);
        straggler.last_fired_at = now - 60;
        // Empty queue — never due, even with the window long elapsed.
        let mut empty = consumer_task("empty", "empty-q", 50, 1);
        empty.last_fired_at = now - 60;
        // Never fired (last_fired_at 0) + any depth — window arm satisfied
        // immediately (first item fires without waiting a full window).
        let first = consumer_task("first", "q", 100_000, 99);
        // 0 threshold disables the burst arm; window still works.
        let mut zero_thresh = consumer_task("zero-thresh", "q", 100_000, 0);
        zero_thresh.last_fired_at = now - 10;
        // next_fire_at gate (spacing/backoff) blocks even a deep queue.
        let mut gated = consumer_task("gated", "q", 50, 1);
        gated.last_fired_at = now - 60;
        gated.next_fire_at = now + 30;

        let tasks = vec![burst, waiting, straggler, empty, first, zero_thresh, gated];
        let due = collect_due(&tasks, now, &depths);
        assert_eq!(
            due,
            vec!["burst".to_string(), "straggler".to_string(), "first".to_string()],
        );
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

    #[test]
    fn circuit_breaker_auto_pauses_at_consecutive_failure_limit() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            task::save(&periodic_task("cb", 100, 0)).expect("save");
            let now = 1_000u64;
            // Below the limit: keeps retrying with backoff, NOT paused.
            for _ in 0..(CONSECUTIVE_FAILURE_LIMIT - 1) {
                sched.advance_after_fire("cb", FireOutcome::Failed, now);
            }
            let before = task::load_one("cb").unwrap();
            assert!(!before.paused, "not paused below the limit");
            assert_eq!(before.consecutive_failures, CONSECUTIVE_FAILURE_LIMIT - 1);
            // The failure that reaches the limit trips the breaker.
            sched.advance_after_fire("cb", FireOutcome::Failed, now);
            let tripped = task::load_one("cb").unwrap();
            assert!(tripped.paused, "auto-paused at the failure limit");
            assert_eq!(tripped.consecutive_failures, CONSECUTIVE_FAILURE_LIMIT);
            let runs = std::fs::read_to_string(task::runs_log_path("cb")).expect("runs.jsonl");
            assert!(runs.contains("\"auto_paused\""), "auto_paused audit line: {}", runs);
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

    /// A compact-only fire (`FiredCompact`) advances like a success but floors
    /// the spacing at [`COMPACT_SETTLE_SPACING_SECS`]: the next fire must not
    /// paste a prompt into the PTY while `/compact`'s async summarization turn
    /// is still running (the paste would be dropped — for a Consumer that
    /// strands the batch as consumed-but-unprocessed). A cadence LONGER than
    /// the floor keeps its own spacing.
    #[test]
    fn fired_compact_floors_next_fire_at_settle_spacing() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            let now = 1_000u64;

            // Consumer spacing (60s) is well under the settle floor → floored.
            let mut c = consumer_task("compact-consumer", "q", 50, 1);
            c.consecutive_failures = 3;
            task::save(&c).expect("save");
            sched.advance_after_fire("compact-consumer", FireOutcome::FiredCompact, now);
            let r = task::load_one("compact-consumer").unwrap();
            assert_eq!(
                r.next_fire_at,
                now + COMPACT_SETTLE_SPACING_SECS,
                "consumer spacing floored to the compact settle window",
            );
            assert_eq!(r.consecutive_failures, 0, "a compact fire is a success");
            assert_eq!(r.last_fired_at, now);

            // A Periodic cadence longer than the floor keeps its own spacing.
            let every = COMPACT_SETTLE_SPACING_SECS + 100;
            task::save(&periodic_task("compact-periodic", every, 0)).expect("save");
            sched.advance_after_fire("compact-periodic", FireOutcome::FiredCompact, now);
            let p = task::load_one("compact-periodic").unwrap();
            assert_eq!(p.next_fire_at, now + every, "longer cadence unfloored");
        });
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

    /// A Consumer task fires through `tick_once` when its queue is deep enough
    /// (depth override stands in for the API poll) and advances by the small
    /// Consumer spacing, not a periodic cadence.
    #[test]
    fn tick_once_fires_due_consumer_task_and_advances_by_spacing() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            sched.arm_fire_spy(FireOutcome::Fired);
            sched.set_depth_override(
                [("proposals".to_string(), 5u64)].into_iter().collect(),
            );
            task::save(&consumer_task("consumer-fire", "proposals", 21_600, 3))
                .expect("save");

            let t0 = task::now_unix();
            sched.tick_once();
            let t1 = task::now_unix();

            assert_eq!(sched.fire_spy_calls(), vec!["consumer-fire".to_string()]);
            let r = task::load_one("consumer-fire").unwrap();
            assert!(
                r.next_fire_at >= t0 + CONSUMER_REFIRE_SPACING_SECS
                    && r.next_fire_at <= t1 + CONSUMER_REFIRE_SPACING_SECS,
                "advanced by the Consumer spacing (not the window): {}",
                r.next_fire_at,
            );
            assert_eq!(r.consecutive_failures, 0);
        });
    }

    /// A Consumer task with an empty queue never fires, whatever the window.
    #[test]
    fn tick_once_skips_consumer_with_empty_queue() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            sched.arm_fire_spy(FireOutcome::Fired);
            sched.set_depth_override(HashMap::new()); // all queues read depth 0
            let mut t = consumer_task("consumer-empty", "proposals", 60, 1);
            t.last_fired_at = 1; // window long elapsed
            task::save(&t).expect("save");

            sched.tick_once();

            assert!(sched.fire_spy_calls().is_empty(), "empty queue never fires");
        });
    }

    /// A Consumer whose last run is still Running is NOT re-fired — even in
    /// Persistent run_mode (unlike Periodic-Persistent) — but its not-before
    /// gate advances so the due check doesn't hot-loop. The batch waits for
    /// `report_done`.
    #[test]
    fn tick_once_consumer_run_active_skips_both_run_modes() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            sched.arm_fire_spy(FireOutcome::Fired);
            sched.set_depth_override(
                [("proposals".to_string(), 50u64)].into_iter().collect(),
            );
            let mut t = consumer_task("consumer-active", "proposals", 60, 1);
            assert_eq!(t.run_mode, RunMode::Persistent, "the harder case");
            t.last_run = Some(RunRecord {
                seq: 3,
                fire_token: "ft-live".into(),
                started_at: task::now_unix(),
                finished_at: None,
                session_uid: Some("ts-live".into()),
                status: RunStatus::Running,
                trigger_source: "operator".into(),
            });
            task::save(&t).expect("save");

            let t0 = task::now_unix();
            sched.tick_once();

            assert!(sched.fire_spy_calls().is_empty(), "run-active Consumer must not re-fire");
            let r = task::load_one("consumer-active").unwrap();
            assert!(
                r.next_fire_at >= t0 + CONSUMER_REFIRE_SPACING_SECS,
                "SkipActive advances the not-before gate: {}",
                r.next_fire_at,
            );
            assert_eq!(r.consecutive_failures, 0, "benign skip, not a failure");
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

    // ----- (c.2) watchdog (Phase 3b: fresh-hang detection) -----

    /// Insert a LIVE `/bin/sleep` DaemonSession under `uid` — the registry entry
    /// the watchdog's liveness probe (`session_is_dead`) + the auto-escalate kill
    /// path read (no real claude). `DaemonSession::spawn` arms its reaper with
    /// `on_exit=None`, so a killed session is LEFT in the registry (the
    /// kill_session-semantics assertion relies on that). The session's
    /// `DaemonSession` drops with `state` at end of test, SIGKILLing the child.
    fn insert_live_session(state: &Arc<Mutex<DaemonState>>, uid: &str) {
        let mut sp = crate::session::SpawnParams::new(uid, "worker", "/bin/sleep");
        sp.args = vec!["60".to_string()];
        sp.session_type = "claude-code".to_string();
        let ds = crate::session::DaemonSession::spawn(sp).expect("spawn /bin/sleep");
        state.lock().unwrap().sessions.insert(uid.to_string(), ds);
    }

    /// A FRESH `OnDemand` task with a `Running` `last_run` pinned to `stuck_uid`
    /// and a tiny `max_runtime_secs`. `started_at = 1` makes the elapsed against
    /// the real `now_unix()` enormous, so the run is unconditionally past budget.
    /// `OnDemand` keeps it out of `collect_due`, isolating the watchdog from the
    /// due loop.
    fn fresh_running_task(id: &str, stuck_uid: &str, max_runtime_secs: u32) -> ContinuousTask {
        let mut t = ContinuousTask::new(
            id.into(),
            id.into(),
            format!("ws-{}", id),
            "/tmp/repo".into(),
            Engine::Claude,
            RunMode::Fresh,
            Schedule::OnDemand,
            "go".into(),
        );
        t.max_runtime_secs = Some(max_runtime_secs);
        t.last_run = Some(RunRecord {
            seq: 1,
            fire_token: "ft-stuck-1".into(),
            started_at: 1,
            finished_at: None,
            session_uid: Some(stuck_uid.into()),
            status: RunStatus::Running,
            trigger_source: "operator".into(),
        });
        t.run_count = 1;
        t
    }

    /// A PERSISTENT orchestrator whose session is ALIVE but whose transcript has
    /// not grown since the last fire (fired > budget ago) is STALLED: the pass
    /// SURFACES it (a `"stalled"` runs.jsonl line) exactly once per fire episode
    /// and does NOT mutate the run/task (surface-only — no pause, no spawn). A
    /// second pass in the same episode does not re-alert (the in-memory latch).
    #[test]
    fn persistent_stall_pass_surfaces_wedged_orchestrator_once() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            state.lock().unwrap().config.scheduler.persistent_max_stall_secs = Some(100);
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let uid = "ts-persistent-stall-0";
            insert_live_session(&state, uid);

            // A transcript file whose mtime is the ground-truth "last output".
            let home = std::env::var("HOME").expect("HOME");
            let transcript = std::path::Path::new(&home).join("stall-transcript.jsonl");
            std::fs::write(&transcript, b"{}\n").expect("write transcript");
            state
                .lock()
                .unwrap()
                .sessions
                .get_mut(uid)
                .unwrap()
                .transcript_path = Some(transcript.to_string_lossy().into_owned());
            let mtime = mtime_unix(&transcript).expect("mtime");

            // A persistent orchestrator fired 1000s AFTER that last write, so the
            // transcript has NOT grown since the fire.
            let fired_at = mtime + 1000;
            let mut t = ContinuousTask::new(
                "pstall".into(),
                "pstall".into(),
                "ws-pstall".into(),
                "/tmp/repo".into(),
                Engine::Claude,
                RunMode::Persistent,
                Schedule::Periodic { every_secs: 3600 },
                "go".into(),
            );
            t.last_fired_at = fired_at;
            t.current_session_uid = Some(uid.into());
            t.run_count = 5;
            t.last_run = Some(RunRecord {
                seq: 5,
                fire_token: "ft-p-5".into(),
                started_at: fired_at,
                finished_at: None,
                session_uid: Some(uid.into()),
                status: RunStatus::Running,
                trigger_source: "scheduler".into(),
            });
            task::save(&t).expect("save");

            // now = past the budget after the fire.
            let now = fired_at + 100 + 1;
            let tasks = task::load_all();
            sched.persistent_stall_pass(&tasks, now);

            let runs =
                std::fs::read_to_string(task::runs_log_path("pstall")).expect("runs.jsonl");
            assert_eq!(runs.matches("\"stalled\"").count(), 1, "one stalled line: {}", runs);

            // Surface-only: the run/task are untouched.
            let reloaded = task::load_one("pstall").expect("load");
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Running,
                "surface-only: run stays Running",
            );
            assert!(!reloaded.paused, "surface-only: not auto-paused");

            // Idempotent within the episode: a second pass does NOT re-alert.
            sched.persistent_stall_pass(&tasks, now + 1);
            let runs2 =
                std::fs::read_to_string(task::runs_log_path("pstall")).expect("runs.jsonl");
            assert_eq!(runs2.matches("\"stalled\"").count(), 1, "no re-alert: {}", runs2);
        });
    }

    /// A fresh run ALIVE past its `max_runtime_secs` (and under the investigation
    /// cap) is STUCK: the watchdog snapshots the evidence and spawns a fresh
    /// investigator, binding `investigator_uid` + bumping `investigation_count`.
    /// The spawn boundary is spied so no real claude launches; the stuck run
    /// stays `Running` (the investigator decides its fate).
    #[test]
    fn watchdog_spawns_investigator_when_alive_past_max_runtime() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let stuck_uid = "ts-stuck-aaaa-0";
            insert_live_session(&state, stuck_uid);
            task::save(&fresh_running_task("wd-spawn", stuck_uid, 1)).expect("save");

            crate::control::methods::arm_continuous_spawn_spy_for_test();
            sched.tick_once();

            // Exactly one investigator spawn reached the (spied) boundary, tagged.
            let captured = crate::control::methods::take_continuous_spawn_spy_for_test();
            assert_eq!(captured.len(), 1, "exactly one investigator spawn");
            assert_eq!(captured[0]["label"], serde_json::json!("investigator"));
            assert_eq!(captured[0]["continuous_task_id"], serde_json::json!("wd-spawn"));
            assert_eq!(captured[0]["session_type"], serde_json::json!("claude-code"));

            let reloaded = task::load_one("wd-spawn").unwrap();
            assert_eq!(reloaded.investigation_count, 1, "one investigation recorded");
            assert!(reloaded.investigator_uid.is_some(), "investigator bound");
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Running,
                "the stuck run stays Running — the investigator decides",
            );

            // The snapshot landed (metadata.json at minimum — no transcript bound
            // on the /bin/sleep session, so that artifact is skipped best-effort).
            let meta = task::task_dir("wd-spawn")
                .join("stuck")
                .join("1")
                .join("metadata.json");
            assert!(meta.exists(), "snapshot metadata.json written: {}", meta.display());

            // spawn_investigator appended a "stuck" audit line.
            let runs = std::fs::read_to_string(task::runs_log_path("wd-spawn")).unwrap();
            assert!(runs.contains("\"stuck\""), "stuck audit line: {}", runs);
        });
    }

    /// While a LIVE investigator is already on the stuck run, the watchdog does
    /// NOTHING this tick — no second investigator, no escalation, counters
    /// untouched.
    #[test]
    fn watchdog_skips_when_live_investigator_present() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let stuck_uid = "ts-stuck-bbbb-0";
            let inv_uid = "ts-inv-bbbb-0";
            insert_live_session(&state, stuck_uid);
            insert_live_session(&state, inv_uid); // investigator is ALIVE
            let mut t = fresh_running_task("wd-skip", stuck_uid, 1);
            t.investigator_uid = Some(inv_uid.to_string());
            t.investigation_count = 1;
            task::save(&t).expect("save");

            crate::control::methods::arm_continuous_spawn_spy_for_test();
            sched.tick_once();

            let captured = crate::control::methods::take_continuous_spawn_spy_for_test();
            assert!(captured.is_empty(), "no new investigator while one is alive");

            let reloaded = task::load_one("wd-skip").unwrap();
            assert_eq!(reloaded.investigation_count, 1, "count unchanged");
            assert_eq!(reloaded.investigator_uid.as_deref(), Some(inv_uid));
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Running,
                "stuck run untouched while the investigator works",
            );
            let runs =
                std::fs::read_to_string(task::runs_log_path("wd-skip")).unwrap_or_default();
            assert!(!runs.contains("\"escalated\""), "must not escalate: {}", runs);
        });
    }

    /// At the investigation cap (`investigation_count == max_investigations`,
    /// default 2) the watchdog AUTO-ESCALATES: it kills the stuck session via
    /// kill_session semantics (left in registry, operator-kill flag set), flips
    /// `last_run` to Stuck, clears the investigator, and writes an "escalated"
    /// audit line — and spawns NO further investigator.
    #[test]
    fn watchdog_auto_escalates_when_investigation_cap_reached() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let stuck_uid = "ts-stuck-cccc-0";
            insert_live_session(&state, stuck_uid);
            // default max_investigations == 2; count == 2 => cap reached.
            let mut t = fresh_running_task("wd-cap", stuck_uid, 1);
            t.investigation_count = 2;
            task::save(&t).expect("save");

            crate::control::methods::arm_continuous_spawn_spy_for_test();
            sched.tick_once();

            let captured = crate::control::methods::take_continuous_spawn_spy_for_test();
            assert!(captured.is_empty(), "cap reached => no investigator spawn");

            let reloaded = task::load_one("wd-cap").unwrap();
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Stuck,
                "auto-escalate flips last_run to Stuck",
            );
            assert!(
                reloaded.investigator_uid.is_none(),
                "investigator cleared on escalate",
            );

            // kill_session semantics: the stuck session is killed but LEFT in the
            // registry with the operator-kill flag set (so the reaper broadcasts
            // ManifestDiff::Exited — never a bare sessions.remove()).
            {
                let s = state.lock().unwrap();
                let stuck = s
                    .sessions
                    .get(stuck_uid)
                    .expect("stuck session left in registry");
                assert!(
                    stuck.last_exit.operator_kill_requested(),
                    "escalate killed via kill_session semantics",
                );
            }

            let runs = std::fs::read_to_string(task::runs_log_path("wd-cap")).unwrap();
            assert!(runs.contains("\"escalated\""), "escalated audit line: {}", runs);
        });
    }

    /// A LIVE investigator that has blown its OWN runtime budget
    /// (`default_investigator_runtime_secs`, default 600s) is ABANDONED: killed
    /// via kill_session semantics, its binding cleared, an "investigator_timeout"
    /// audit written — and NO new investigator spawned this tick (next tick the
    /// count check re-investigates or escalates). Without this a wedged-but-alive
    /// investigator would pin the stuck run forever.
    #[test]
    fn watchdog_abandons_timed_out_investigator() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let stuck_uid = "ts-stuck-dddd-0";
            let inv_uid = "ts-inv-dddd-0";
            insert_live_session(&state, stuck_uid);
            insert_live_session(&state, inv_uid); // investigator ALIVE but wedged
            let mut t = fresh_running_task("wd-invtimeout", stuck_uid, 1);
            t.investigator_uid = Some(inv_uid.to_string());
            t.investigation_count = 1;
            // Spawned well past the default 600s investigator budget => over budget.
            t.investigator_started_at = Some(task::now_unix().saturating_sub(700));
            task::save(&t).expect("save");

            crate::control::methods::arm_continuous_spawn_spy_for_test();
            sched.tick_once();

            // No NEW investigator this tick (abandon -> continue; re-eval next tick).
            let captured = crate::control::methods::take_continuous_spawn_spy_for_test();
            assert!(captured.is_empty(), "abandon spawns no new investigator this tick");

            let reloaded = task::load_one("wd-invtimeout").unwrap();
            assert!(reloaded.investigator_uid.is_none(), "wedged investigator binding cleared");
            assert!(reloaded.investigator_started_at.is_none(), "investigator clock cleared");
            assert_eq!(reloaded.investigation_count, 1, "count NOT bumped by abandon");
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Running,
                "the stuck WORKER run stays Running — only the investigator is abandoned",
            );

            // The INVESTIGATOR (not the worker) was killed via kill_session
            // semantics: left in the registry with the operator-kill flag set.
            {
                let s = state.lock().unwrap();
                let inv = s.sessions.get(inv_uid).expect("investigator left in registry");
                assert!(
                    inv.last_exit.operator_kill_requested(),
                    "investigator killed via kill_session semantics",
                );
                let worker = s.sessions.get(stuck_uid).expect("worker still present");
                assert!(
                    !worker.last_exit.operator_kill_requested(),
                    "the worker is NOT killed by abandon",
                );
            }

            let runs = std::fs::read_to_string(task::runs_log_path("wd-invtimeout")).unwrap();
            assert!(runs.contains("\"investigator_timeout\""), "timeout audit: {}", runs);
            assert!(!runs.contains("\"escalated\""), "abandon is not an escalation: {}", runs);
        });
    }

    /// `fresh_run_active` is FRESH-only: a Persistent task with a Running
    /// `last_run` is EXEMPT from DUE-SKIP-ACTIVE (it reuses one live session, so
    /// re-delivery is the intended persistent behavior).
    #[test]
    fn fresh_run_active_is_fresh_only() {
        let mut fresh = fresh_running_task("fra-fresh", "ts-x", 60);
        assert!(fresh_run_active(&fresh), "fresh + Running => active");
        fresh.run_mode = RunMode::Persistent;
        assert!(!fresh_run_active(&fresh), "persistent is exempt");
        fresh.run_mode = RunMode::Fresh;
        fresh.last_run.as_mut().unwrap().status = RunStatus::Done;
        assert!(!fresh_run_active(&fresh), "a finished run is not active");
    }

    // ----- (e) DUE-SKIP-ACTIVE (Phase 3b) -----

    /// A FRESH Periodic task that is DUE but whose prior run is still `Running`
    /// is NOT re-fired (no overlapping fresh runs) — yet `next_fire_at` STILL
    /// advances catch-up-once and `consecutive_failures` is NOT bumped.
    /// (`max_runtime_secs=None` keeps the watchdog OFF so only DUE-SKIP-ACTIVE
    /// is exercised.)
    #[test]
    fn due_check_skips_active_fresh_run_but_advances() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            sched.arm_fire_spy(FireOutcome::Fired); // would record a fire if one happened
            // A genuinely-ACTIVE run has a LIVE session in the registry. Without
            // one, reconcile_orphans now (correctly) treats the run as a dead
            // stranded run and recovers it — so register a live session to
            // isolate DUE-SKIP-ACTIVE from restart-reconciliation.
            insert_live_session(&state, "ts-active-0");
            let mut t = periodic_task("skip-active", 100, 1); // due (next_fire_at=1)
            t.last_run = Some(RunRecord {
                seq: 1,
                fire_token: "ft-active-1".into(),
                started_at: 1,
                finished_at: None,
                session_uid: Some("ts-active-0".into()),
                status: RunStatus::Running,
                trigger_source: "operator".into(),
            });
            t.run_count = 1;
            task::save(&t).expect("save");

            let t0 = task::now_unix();
            sched.tick_once();
            let t1 = task::now_unix();

            assert!(
                sched.fire_spy_calls().is_empty(),
                "an active fresh run must not be re-fired",
            );
            let r = task::load_one("skip-active").unwrap();
            assert!(
                r.next_fire_at >= t0 + 100 && r.next_fire_at <= t1 + 100,
                "next_fire_at advanced catch-up-once: {}",
                r.next_fire_at,
            );
            assert!(r.next_fire_at > t1, "advanced into the future, not a re-fire");
            assert_eq!(
                r.consecutive_failures, 0,
                "skip-active is benign — does not bump failures",
            );
            assert_eq!(
                r.last_run.as_ref().unwrap().status,
                RunStatus::Running,
                "the active run is left untouched",
            );
        });
    }

    /// `advance_after_fire(SkipActive)` advances `next_fire_at` catch-up-once
    /// (like Fired) but — unlike Fired — does NOT reset `consecutive_failures`,
    /// and — unlike Skipped — DOES advance the clock.
    #[test]
    fn advance_skip_active_advances_without_touching_failures() {
        let _tmp = with_temp_home(|| {
            let sched = scheduler();
            let mut t = periodic_task("sa-advance", 100, 7);
            t.consecutive_failures = 3; // pre-existing streak must be preserved
            task::save(&t).expect("save");

            sched.advance_after_fire("sa-advance", FireOutcome::SkipActive, 1000);

            let r = task::load_one("sa-advance").unwrap();
            assert_eq!(r.next_fire_at, 1100, "advanced now + every");
            assert_eq!(r.last_fired_at, 1000);
            assert_eq!(r.consecutive_failures, 3, "neither bumped nor reset");
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

    // ----- (c.3b) auth-expiry + consumer-wedge pass -----

    // Transcript fixture lines — the shapes `continuous::probe` classifies
    // (mirrors of the real 2026-08-03 incident records).
    const T_USER: &str = r#"{"type":"user","message":{"role":"user","content":"go"}}"#;
    const T_ASSISTANT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done."}]}}"#;
    const T_TOOL_USE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#;
    const T_TURN_END: &str = r#"{"type":"system","subtype":"turn_duration","durationMs":10}"#;
    const T_AUTH_401: &str = r#"{"type":"assistant","error":"authentication_failed","isApiErrorMessage":true,"message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"Login expired · Please run /login"}]}}"#;

    /// Live session + transcript with the given lines; binds the transcript to
    /// the session and returns the file's real mtime (unix secs) — test `now`
    /// values are derived from it, mirroring the stall-pass test.
    fn bind_transcript(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        name: &str,
        lines: &[&str],
    ) -> u64 {
        insert_live_session(state, uid);
        let home = std::env::var("HOME").expect("HOME");
        let path = std::path::Path::new(&home).join(name);
        std::fs::write(&path, lines.join("\n") + "\n").expect("write transcript");
        state
            .lock()
            .unwrap()
            .sessions
            .get_mut(uid)
            .unwrap()
            .transcript_path = Some(path.to_string_lossy().into_owned());
        mtime_unix(&path).expect("mtime")
    }

    /// A Consumer task (given run mode) with a `Running` run pinned to `uid`,
    /// with run start / last fire stamped at `at`.
    fn running_consumer(
        id: &str,
        uid: &str,
        run_mode: RunMode,
        seq: u64,
        at: u64,
    ) -> ContinuousTask {
        let mut t = consumer_task(id, "q", 3600, 1);
        t.run_mode = run_mode;
        t.current_session_uid = Some(uid.into());
        t.last_fired_at = at;
        t.run_count = seq as u32;
        t.last_run = Some(RunRecord {
            seq,
            fire_token: format!("ft-{}-{}", id, seq),
            started_at: at,
            finished_at: None,
            session_uid: Some(uid.into()),
            status: RunStatus::Running,
            trigger_source: "operator".into(),
        });
        t
    }

    /// The incident shape: a persistent Consumer whose session ended its turn
    /// with the synthetic 401 record. The pass surfaces `auth_expired` ONCE
    /// per cooldown window (re-alerting after it elapses) and deliberately
    /// leaves the run Running — the blocked due-gate is what keeps future
    /// fires from claiming+acking queue items into a dead session.
    #[test]
    fn auth_wedge_pass_surfaces_auth_expiry_and_leaves_run_running() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let uid = "ts-auth-dead-0";
            let mtime = bind_transcript(
                &state,
                uid,
                "auth.jsonl",
                &[T_USER, T_AUTH_401, T_TURN_END],
            );
            // Quiet WAY past the wedge grace: proves auth wins over the close.
            let now = mtime + 10_000;
            let t = running_consumer("authdead", uid, RunMode::Persistent, 5, mtime);
            task::save(&t).expect("save");

            sched.auth_wedge_pass(&task::load_all(), now);

            let runs = std::fs::read_to_string(task::runs_log_path("authdead")).unwrap();
            assert_eq!(runs.matches("\"auth_expired\"").count(), 1, "{}", runs);
            assert!(runs.contains("Login expired"), "banner in detail: {}", runs);
            let reloaded = task::load_one("authdead").unwrap();
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Running,
                "auth-dead run stays Running (due-gate protects the queue)",
            );
            assert_eq!(reloaded.consecutive_wedge_closes, 0, "not a wedge close");

            // Within the cooldown (past the probe throttle): no re-alert.
            sched.auth_wedge_pass(&task::load_all(), now + TAIL_PROBE_INTERVAL_SECS + 1);
            let runs2 = std::fs::read_to_string(task::runs_log_path("authdead")).unwrap();
            assert_eq!(runs2.matches("\"auth_expired\"").count(), 1, "{}", runs2);

            // Past the cooldown: one re-alert (the condition persists).
            sched.auth_wedge_pass(&task::load_all(), now + AUTH_REALERT_SECS + 1);
            let runs3 = std::fs::read_to_string(task::runs_log_path("authdead")).unwrap();
            assert_eq!(runs3.matches("\"auth_expired\"").count(), 2, "{}", runs3);
        });
    }

    /// The 3.5-day wedge shape: a Consumer run still `Running` while its live
    /// session's transcript ends in a COMPLETED healthy turn and nothing has
    /// happened past the grace. Auto-closed `Running → Failed` (due-gate
    /// unblocked), counter bumped, `"wedge_closed"` audited.
    #[test]
    fn auth_wedge_pass_closes_wedged_consumer_run() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let grace = {
                let s = state.lock().unwrap();
                s.config.scheduler.consumer_wedge_grace_secs
            };
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let uid = "ts-wedged-0";
            let mtime = bind_transcript(
                &state,
                uid,
                "wedge.jsonl",
                &[T_USER, T_ASSISTANT, T_TURN_END],
            );
            let now = mtime + grace + 1;
            let t = running_consumer("wedged", uid, RunMode::Persistent, 7, mtime);
            task::save(&t).expect("save");

            sched.auth_wedge_pass(&task::load_all(), now);

            let reloaded = task::load_one("wedged").unwrap();
            let lr = reloaded.last_run.as_ref().unwrap();
            assert_eq!(lr.status, RunStatus::Failed, "wedged run auto-closed");
            assert_eq!(lr.finished_at, Some(now));
            assert_eq!(reloaded.consecutive_wedge_closes, 1);
            let runs = std::fs::read_to_string(task::runs_log_path("wedged")).unwrap();
            assert_eq!(runs.matches("\"wedge_closed\"").count(), 1, "{}", runs);
            // The due-gate is unblocked again.
            assert!(!run_active_blocks_fire(&reloaded), "consumer can refire");
        });
    }

    /// Within the grace, mid-turn tails (long tool call), and non-Consumer
    /// schedules are never closed.
    #[test]
    fn auth_wedge_pass_skips_healthy_and_non_consumer_shapes() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let grace = {
                let s = state.lock().unwrap();
                s.config.scheduler.consumer_wedge_grace_secs
            };
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));

            // (a) turn-complete but WITHIN the grace.
            let m1 = bind_transcript(
                &state,
                "ts-w-recent",
                "recent.jsonl",
                &[T_USER, T_ASSISTANT, T_TURN_END],
            );
            task::save(&running_consumer("recent", "ts-w-recent", RunMode::Persistent, 1, m1))
                .unwrap();
            // (b) MID-TURN (trailing tool_use) long past the grace.
            let m2 = bind_transcript(
                &state,
                "ts-w-midturn",
                "midturn.jsonl",
                &[T_USER, T_TOOL_USE],
            );
            task::save(&running_consumer(
                "midturn",
                "ts-w-midturn",
                RunMode::Fresh,
                1,
                m2,
            ))
            .unwrap();
            // (c) PERIODIC persistent (non-Consumer), turn-complete + stale —
            // a Running run here is normal persistent bookkeeping, never a
            // wedge.
            let m3 = bind_transcript(
                &state,
                "ts-w-periodic",
                "periodic.jsonl",
                &[T_USER, T_ASSISTANT, T_TURN_END],
            );
            let mut periodic = periodic_task("periodic", 3600, 0);
            periodic.run_mode = RunMode::Persistent;
            periodic.current_session_uid = Some("ts-w-periodic".into());
            periodic.last_fired_at = m3;
            periodic.last_run = Some(RunRecord {
                seq: 1,
                fire_token: "ft-p".into(),
                started_at: m3,
                finished_at: None,
                session_uid: Some("ts-w-periodic".into()),
                status: RunStatus::Running,
                trigger_source: "operator".into(),
            });
            task::save(&periodic).unwrap();

            // One pass, stale for everyone by mtime — but (a)'s quiet window
            // is refreshed via last_fired_at (a fire just delivered, its
            // paste not yet in the transcript), exercising the grace guard.
            let now = m1.max(m2).max(m3) + grace + 1;
            let _ = task::modify("recent", |t| t.last_fired_at = now - 10);
            sched.auth_wedge_pass(&task::load_all(), now);

            for id in ["recent", "midturn", "periodic"] {
                let t = task::load_one(id).unwrap();
                assert_eq!(
                    t.last_run.as_ref().unwrap().status,
                    RunStatus::Running,
                    "{} must not be closed",
                    id,
                );
                assert_eq!(t.consecutive_wedge_closes, 0, "{}", id);
                let runs = std::fs::read_to_string(task::runs_log_path(id)).unwrap_or_default();
                assert!(
                    !runs.contains("wedge_closed"),
                    "{} must not wedge-close: {}",
                    id,
                    runs,
                );
            }
        });
    }

    /// At the close limit the pass escalates ONCE per run (audit + alert),
    /// leaving the run Running so a chronically-wedging consumer stops
    /// burning queue items via close→refire churn.
    #[test]
    fn auth_wedge_pass_escalates_at_close_limit() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let (grace, limit) = {
                let s = state.lock().unwrap();
                (
                    s.config.scheduler.consumer_wedge_grace_secs,
                    s.config.scheduler.wedge_close_limit,
                )
            };
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let uid = "ts-chronic-0";
            let mtime = bind_transcript(
                &state,
                uid,
                "chronic.jsonl",
                &[T_USER, T_ASSISTANT, T_TURN_END],
            );
            let now = mtime + grace + 1;
            let mut t = running_consumer("chronic", uid, RunMode::Persistent, 9, mtime);
            t.consecutive_wedge_closes = limit; // streak already exhausted
            task::save(&t).expect("save");

            sched.auth_wedge_pass(&task::load_all(), now);

            let reloaded = task::load_one("chronic").unwrap();
            assert_eq!(
                reloaded.last_run.as_ref().unwrap().status,
                RunStatus::Running,
                "escalated run is deliberately left Running",
            );
            let runs = std::fs::read_to_string(task::runs_log_path("chronic")).unwrap();
            assert_eq!(runs.matches("\"wedge_escalated\"").count(), 1, "{}", runs);

            // Idempotent for the same run seq.
            sched.auth_wedge_pass(&task::load_all(), now + TAIL_PROBE_INTERVAL_SECS + 1);
            let runs2 = std::fs::read_to_string(task::runs_log_path("chronic")).unwrap();
            assert_eq!(runs2.matches("\"wedge_escalated\"").count(), 1, "{}", runs2);
        });
    }

    // ----- (c.3c) credentials preflight -----

    /// Exists-but-broken credentials alert once per episode (cooldown), gated
    /// on having an enabled claude continuous task; a healthy file (or a
    /// missing one) never alerts and re-arms the latch.
    #[test]
    fn credentials_preflight_alerts_once_per_bad_episode() {
        let _tmp = with_temp_home(|| {
            let state = Arc::new(Mutex::new(DaemonState::default()));
            let sched = Arc::new(ContinuousScheduler::new(Arc::clone(&state)));
            let home = std::env::var("HOME").unwrap();
            let dir = std::path::Path::new(&home).join(".claude");
            std::fs::create_dir_all(&dir).unwrap();
            let creds = dir.join(".credentials.json");
            let tasks = vec![periodic_task("ct", 60, 0)]; // enabled claude task
            let mut now = 100_000u64;

            // Missing file: healthy.
            assert!(!sched.credentials_preflight(&tasks, now));

            // Truncated file: one alert...
            now += CREDS_CHECK_INTERVAL_SECS + 1;
            std::fs::write(&creds, r#"{"claudeAiOauth":{"accessToken":"sk-an"#).unwrap();
            assert!(sched.credentials_preflight(&tasks, now));
            // ...throttled within the check interval...
            assert!(!sched.credentials_preflight(&tasks, now + 1));
            // ...and cooldown-latched past it.
            now += CREDS_CHECK_INTERVAL_SECS + 1;
            assert!(!sched.credentials_preflight(&tasks, now));

            // Healthy again: no alert, latch re-armed.
            now += CREDS_CHECK_INTERVAL_SECS + 1;
            std::fs::write(&creds, r#"{"claudeAiOauth":{"accessToken":"sk-ant-ok"}}"#)
                .unwrap();
            assert!(!sched.credentials_preflight(&tasks, now));

            // Broken AGAIN (new episode): immediate re-alert, no 6h wait.
            now += CREDS_CHECK_INTERVAL_SECS + 1;
            std::fs::write(&creds, b"").unwrap();
            assert!(sched.credentials_preflight(&tasks, now));

            // No claude tasks → never alerts even with a broken file.
            now += CREDS_CHECK_INTERVAL_SECS + 1;
            assert!(!sched.credentials_preflight(&[], now));
        });
    }
}
