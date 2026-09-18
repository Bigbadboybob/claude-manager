//! Close the workers a finished task still owns.
//!
//! THE BUG THIS EXISTS FOR (2026-09-18). The daemon has always had
//! `sweep_task_sessions`: kill every session bound to a task, so its checkout
//! can be removed. It was wired to exactly ONE caller, the `mark_subtask_done`
//! MCP tool. Every other route a task takes to `done` — the planning-API PATCH
//! `/triage-review` uses, the TUI's A-d, a cloud status flip — skipped it, so
//! the task's workers stayed alive at their prompt forever.
//!
//! That is not merely untidy. The reaper refuses any checkout holding a live
//! session (`live_session`), and it evaluates liveness BEFORE it computes age,
//! so a surviving worker means the seven-day inactivity clock never starts at
//! all. "Keep the worktree, reap it if I don't come back" silently became
//! "never reap". Measured on cm-manager the morning this landed: 65 live
//! sessions, every one idle at its prompt, 56 sitting in a `cm-sub` checkout
//! and 47 of those already past `report_done`, some since August. Every
//! operator attempt to reap a finished task hit `live_session` and had to fall
//! back to "keep".
//!
//! The fix is to hook the sweep to the GENERAL condition (a task is terminal)
//! instead of to one manual entry point. This pass polls the planning rows of
//! the tasks that currently own a live session, and sweeps the terminal ones.
//!
//! WHAT IT DELIBERATELY DOES NOT DO:
//! * It does not close a worker whose task is still open, even one that has
//!   called `report_done`. A worker awaiting review stays visible on the board;
//!   that is policy (`doc/continuous-review-routing.md`), and the review round
//!   is exactly the window in which someone may send it more work.
//! * It does not delete anything. Closing a session leaves the checkout, the
//!   branch, the files and the transcript in place, so `A-R` still revives the
//!   conversation. It only lets the reaper's clock start.
//! * It never touches a continuous orchestrator or a workflow participant:
//!   the scheduler and the workflow engine own those lifecycles.
//!
//! COST. It reads ONE planning row per task that owns a live session, on a
//! five-minute cadence — never `GET /tasks`. A periodic whole-board poll is the
//! precise shape that cost ~80 GiB/day before 2026-09-10 (CLAUDE.md), and the
//! sweep only ever needs the handful of rows that own a session.
use crate::state::DaemonState;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How often the pass runs. Deliberately slow: nothing here is urgent, the
/// work it unblocks is measured in days, and the cadence bounds the planning-API
/// reads (one per task owning a live session, per pass).
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Upper bound on planning rows read in ONE pass, so a host with a pathological
/// number of bound sessions cannot turn the sweep into a whole-board poll by
/// another name. The remainder is picked up on the next pass.
pub const MAX_LOOKUPS_PER_PASS: usize = 64;

/// Statuses that mean the task is finished and its workers have no reason to
/// hold their checkout. Matches `TERMINAL_TASK_STATUSES` in
/// `scripts/worktree_reaper.py`, which is what actually decides reapability —
/// the two lists disagreeing would make the sweep close sessions the reaper
/// still refuses to act on, or vice versa.
pub const TERMINAL_TASK_STATUSES: [&str; 2] = ["done", "archived"];

pub fn is_terminal(status: &str) -> bool {
    TERMINAL_TASK_STATUSES.contains(&status.trim().to_ascii_lowercase().as_str())
}

/// Task ids that own at least one live session this pass may act on. Applies
/// the cheap, purely local half of the filter so the pass never spends a
/// planning read on a task whose sessions it could not sweep anyway.
pub fn candidate_task_ids(state_arc: &Arc<Mutex<DaemonState>>) -> Vec<String> {
    let state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    let ids: BTreeSet<String> = state
        .sessions
        .values()
        .filter(|s| s.continuous_task_id.is_none() && s.workflow_run_id.is_none())
        .filter_map(|s| s.task_id.clone())
        .collect();
    ids.into_iter().take(MAX_LOOKUPS_PER_PASS).collect()
}

pub struct TerminalTaskSweeper {
    state: Arc<Mutex<DaemonState>>,
    shutdown: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TerminalTaskSweeper {
    pub fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        Self {
            state,
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
        }
    }

    /// One pass. Returns the uids it closed. Never panics on a planning-API
    /// failure: an unreachable or unconfigured API simply means this host does
    /// not know which tasks are terminal, and the honest response to that is to
    /// close nothing and retry next pass.
    pub fn tick_once(&self) -> Vec<String> {
        let (api_url, api_token) = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            (state.config.api_url.clone(), state.config.api_token.clone())
        };
        let mut closed = Vec::new();
        for task_id in candidate_task_ids(&self.state) {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let status = match crate::planning_client::fetch_task_status(
                &task_id,
                Some(api_url.as_str()),
                Some(api_token.as_str()),
            ) {
                Ok(Some(status)) => status,
                // A deleted planning row is not evidence that the work is
                // finished; leave the session alone and let the reaper's
                // unowned-checkout policy handle the checkout.
                Ok(None) => continue,
                Err(error) => {
                    eprintln!(
                        "cm-daemon: terminal-task sweep could not read task {task_id}: {error:?}"
                    );
                    continue;
                }
            };
            if !is_terminal(&status) {
                continue;
            }
            let swept = crate::control::methods::sweep_terminal_task_sessions(
                &self.state,
                &task_id,
                "terminal-task-sweep",
            );
            if !swept.is_empty() {
                eprintln!(
                    "cm-daemon: task {task_id} is {status}; closed {} finished worker session(s): {}",
                    swept.len(),
                    swept.join(", "),
                );
                closed.extend(swept);
            }
        }
        closed
    }

    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("cm-terminal-sweep".into())
            .spawn(move || me.run_loop())?;
        *guard = Some(handle);
        Ok(())
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let handle = self.handle.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    fn run_loop(self: Arc<Self>) {
        while !self.shutdown.load(Ordering::SeqCst) {
            let started = Instant::now();
            // A panic in one pass must not take the daemon down; the next pass
            // retries. Same posture as the scheduler's loop.
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.tick_once()));
            if let Err(panic) = result {
                let msg = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string());
                eprintln!(
                    "cm-daemon: terminal-task sweep panicked: {msg} (continuing — next pass retries)"
                );
            }
            // Sleep the remainder in small chunks so shutdown latency stays
            // short instead of a five-minute join.
            while !self.shutdown.load(Ordering::SeqCst) && started.elapsed() < SWEEP_INTERVAL {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_statuses_match_the_reaper_exactly() {
        // The reaper is the component that actually refuses to remove a
        // checkout; if these lists drift, the sweep closes sessions the reaper
        // still protects (or leaves ones it would have reaped).
        let reaper = include_str!("../../scripts/worktree_reaper.py");
        let line = reaper
            .lines()
            .find(|l| l.starts_with("TERMINAL_TASK_STATUSES"))
            .expect("reaper declares TERMINAL_TASK_STATUSES");
        for status in TERMINAL_TASK_STATUSES {
            assert!(
                line.contains(&format!("\"{status}\"")),
                "reaper's terminal set is missing {status}: {line}"
            );
        }
        assert_eq!(
            line.matches('"').count() / 2,
            TERMINAL_TASK_STATUSES.len(),
            "reaper terminal set has entries the sweep does not know: {line}"
        );
    }

    #[test]
    fn is_terminal_accepts_the_apis_casing_and_padding() {
        assert!(is_terminal("done"));
        assert!(is_terminal("Archived"));
        assert!(is_terminal(" done "));
        assert!(!is_terminal("running"));
        assert!(!is_terminal("blocked"));
        // `blocked` is the owner-review park: emphatically NOT terminal, or the
        // sweep would close the very workers a review round is waiting on.
        assert!(!is_terminal("draft"));
        assert!(!is_terminal(""));
    }

    /// Fixture teardown: the spawned `/bin/sleep` children must not outlive
    /// the test.
    fn kill_fixtures(state: &Arc<Mutex<DaemonState>>) {
        let mut guard = state.lock().unwrap();
        for session in guard.sessions.values_mut() {
            let _ = session.kill();
        }
        guard.sessions.clear();
    }

    /// Sessions can only be built by actually spawning one (there is no
    /// `Default`), so the fixtures ride a cheap `/bin/sleep` exactly as the
    /// scheduler's own tests do.
    fn session(task_id: &str) -> crate::session::DaemonSession {
        let mut sp = crate::session::SpawnParams::new("ts-fixture", "worker", "/bin/sleep");
        sp.args = vec!["60".to_string()];
        sp.session_type = "claude-code".to_string();
        let mut ds = crate::session::DaemonSession::spawn(sp).expect("spawn /bin/sleep");
        ds.task_id = Some(task_id.to_string());
        ds
    }

    #[test]
    fn candidates_exclude_continuous_and_workflow_sessions_and_dedupe_by_task() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        {
            let mut guard = state.lock().unwrap();
            guard.sessions.insert("ts-plain".into(), session("task-plain"));
            // Two sessions on ONE task must cost one planning read, not two.
            guard.sessions.insert("ts-plain-2".into(), session("task-plain"));

            let mut orchestrator = session("task-continuous");
            orchestrator.continuous_task_id = Some("bug-triage".into());
            guard.sessions.insert("ts-orch".into(), orchestrator);

            let mut participant = session("task-workflow");
            participant.workflow_run_id = Some("run-1".into());
            guard.sessions.insert("ts-wf".into(), participant);
        }
        assert_eq!(candidate_task_ids(&state), vec!["task-plain".to_string()]);
        kill_fixtures(&state);
    }

    #[test]
    fn a_host_that_cannot_read_planning_closes_nothing() {
        // `api_url` empty and no env fallback: the fetch errors. "Cannot tell"
        // must never be read as "terminal" — that would kill live workers on a
        // host whose planning API is merely down.
        let _guard = crate::planning_client::test_env_lock();
        let state = Arc::new(Mutex::new(DaemonState::default()));
        {
            let mut guard = state.lock().unwrap();
            guard.sessions.insert("ts-unknown".into(), session("task-unknown"));
        }
        let sweeper = TerminalTaskSweeper::new(Arc::clone(&state));
        assert!(sweeper.tick_once().is_empty());
        assert!(state.lock().unwrap().sessions.contains_key("ts-unknown"));
        kill_fixtures(&state);
    }
}
