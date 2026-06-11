//! Workflow observation glue for the TUI.
//!
//! After the daemon-side orchestration relocation the TUI drives no workflow
//! logic — it only OBSERVES runs (via `workflow_watch` / `manifest.watch`) and
//! renders them. This module is the home for that observation-side glue,
//! extracted out of the `app.rs` god object. For now it holds the per-run tick
//! logger; the larger `events.subscribe` → run-state-mirror plumbing in
//! `App::tick_workflows` is the next thing to move here (it's still coupled to
//! `App` state).

use crate::workflow;

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
