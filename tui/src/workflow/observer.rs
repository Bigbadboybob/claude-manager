//! Workflow observation glue for the TUI.
//!
//! After the daemon-side orchestration relocation the TUI drives no workflow
//! logic — it only OBSERVES runs (via `workflow_watch` / `manifest.watch`) and
//! renders them. This module is the home for that observation-side glue,
//! extracted out of the `app.rs` god object. For now it holds the per-run tick
//! logger; the larger `events.subscribe` → run-state-mirror plumbing in
//! `App::tick_workflows` is the next thing to move here (it's still coupled to
//! `App` state).

use std::sync::mpsc::Receiver;

use crate::app::Workspace;
use crate::workflow::{self, WorkflowRun};
use crate::workflow_watch::WorkflowWatchEvent;

/// Untag every session pointing at `run_id` (clear `workflow_run_id` +
/// `workflow_role`, unhide so they behave like standalone sessions), then drop
/// the run from the in-memory mirror. Pure over the two collections — no `App`.
/// Used by the stop-cleanup path and [`drop_inactive_runs_from_in_mem`].
pub(crate) fn drop_run_from_in_mem(
    workflow_runs: &mut Vec<WorkflowRun>,
    workspaces: &mut Vec<Workspace>,
    run_id: &str,
) {
    for ws in workspaces.iter_mut() {
        for ts in &mut ws.sessions {
            if ts.workflow_run_id.as_deref() == Some(run_id) {
                ts.workflow_run_id = None;
                ts.workflow_role = None;
                ts.hidden = false;
            }
        }
    }
    workflow_runs.retain(|r| r.run_id != run_id);
}

/// For each tracked run, peek `state.json` on disk; if it's no longer active
/// (Detached / Done) or the run file is gone, drop the run from the mirror and
/// untag its sessions. Returns the count dropped so the caller can decide
/// whether to persist the manifest.
///
/// The TUI is a pure observer: the daemon owns run state and broadcasts a fresh
/// snapshot on every change (adopted by [`RunMirror::apply_snapshot`]), so this
/// reconcile only needs to GC runs that have reached a terminal/absent on-disk
/// state out of the in-memory view.
pub(crate) fn drop_inactive_runs_from_in_mem(
    workflow_runs: &mut Vec<WorkflowRun>,
    workspaces: &mut Vec<Workspace>,
) -> usize {
    let tracked: Vec<String> = workflow_runs.iter().map(|r| r.run_id.clone()).collect();
    let mut dropped = 0usize;
    for run_id in &tracked {
        // `load_one` returns `None` if the run file is missing or unreadable.
        // Treat missing as "not active" — a tracked run whose file is gone is
        // also stale.
        let still_active = workflow::run::load_one(run_id)
            .map(|r| r.is_active())
            .unwrap_or(false);
        if !still_active {
            drop_run_from_in_mem(workflow_runs, workspaces, run_id);
            dropped += 1;
        }
    }
    dropped
}

/// Drain all currently-queued `workflow_watch` events from the consumer-thread
/// channel without blocking, in arrival order. Empty when no channel is wired
/// (`rx` is `None`) or nothing is queued. Both `Empty` and `Disconnected` end
/// the drain (a dead consumer thread simply yields no more events).
pub(crate) fn drain_watch_channel(
    rx: Option<&Receiver<WorkflowWatchEvent>>,
) -> Vec<WorkflowWatchEvent> {
    let mut events = Vec::new();
    if let Some(rx) = rx {
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
    }
    events
}

/// The observer-side mirror of daemon-broadcast workflow run state. The daemon
/// poller is authoritative (Phase 4 §E — the TUI drives nothing); this just
/// adopts the snapshots it sends. Borrows the narrow slice of `App` the mirror
/// touches (`workflow_runs` + the redraw flag) so the logic lives out of the
/// `app.rs` god object.
pub(crate) struct RunMirror<'a> {
    pub runs: &'a mut Vec<WorkflowRun>,
    pub needs_redraw: &'a mut bool,
}

impl RunMirror<'_> {
    /// Adopt a `Snapshot` frame: the daemon's `WorkflowRun` is authoritative,
    /// so UPDATE an existing run in place (or insert when absent) — NOT a
    /// conservative no-overwrite merge. With the local controller gone the TUI
    /// holds no live diffs of its own, so refusing to overwrite would freeze a
    /// run at its creation snapshot (active_role/history/terminal status never
    /// advancing without a reconnect). The daemon broadcasts a fresh snapshot
    /// on every change, so adopting it wholesale is how progress renders.
    pub(crate) fn apply_snapshot(&mut self, run: WorkflowRun) {
        match self.runs.iter_mut().find(|r| r.run_id == run.run_id) {
            Some(slot) => *slot = run,
            None => self.runs.push(run),
        }
        *self.needs_redraw = true;
    }

    /// Apply a drained batch of watch events. A `Snapshot` updates the mirror;
    /// an `Event` is purely a redraw nudge and is NOT buffered — run STATE
    /// arrives authoritatively via `Snapshot` frames
    /// (`daemon::broadcast_changed_snapshots`), so a buffer would have no
    /// reader (the local controller that used to drain it is gone).
    pub(crate) fn apply_events(&mut self, events: Vec<WorkflowWatchEvent>) {
        for ev in events {
            match ev {
                WorkflowWatchEvent::Snapshot(run) => self.apply_snapshot(run),
                WorkflowWatchEvent::Event(_event) => *self.needs_redraw = true,
            }
        }
    }
}

/// Per-file cap on `~/.cm/workflow-runs/<run-id>/tick.log`. When [`log_tick`]
/// is about to write and the file is at or over this size, it truncates and
/// starts fresh (with a marker line). Generous because these logs are useful
/// debugging artifacts; this exists only to bound runaway growth from
/// pathologically chatty runs.
const TICK_LOG_MAX_BYTES: u64 = 500 * 1024 * 1024;

/// Append a debug line to a workflow run's `tick.log`, rate-limited to one
/// identical `(run_id, msg)` per second so a hot observation loop can't flood
/// the file. Best-effort: any I/O error is swallowed (this is a debugging aid,
/// not a correctness path). Truncates at [`TICK_LOG_MAX_BYTES`].
pub(crate) fn log_tick(run_id: &str, msg: &str) {
    use std::io::Write as _;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Rate-limit: remember the last (run_id, msg) logged and when. Skip if we
    // logged the same thing within the last second.
    static LAST: std::sync::OnceLock<Mutex<Option<(String, String, u64)>>> =
        std::sync::OnceLock::new();
    let lock = LAST.get_or_init(|| Mutex::new(None));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    {
        let mut guard = match lock.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some((last_run, last_msg, last_ts)) = guard.as_ref() {
            if last_run == run_id && last_msg == msg && now.saturating_sub(*last_ts) < 1 {
                return;
            }
        }
        *guard = Some((run_id.to_string(), msg.to_string(), now));
    }

    let path = workflow::run::run_dir(run_id).join("tick.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let truncate = std::fs::metadata(&path)
        .map(|m| m.len() >= TICK_LOG_MAX_BYTES)
        .unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if truncate {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    if let Ok(mut f) = opts.open(&path) {
        if truncate {
            let _ = writeln!(
                f,
                "{} (log truncated: cap {} bytes hit)",
                now, TICK_LOG_MAX_BYTES
            );
        }
        let _ = writeln!(f, "{} {}", now, msg);
    }
}
