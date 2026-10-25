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
//! * It never closes a session Owner has used in the last [`OWNER_GRACE`]:
//!   typed into, shown in the TUI (`session.viewed`), revived or resumed.
//!   2026-10-12: a worker whose task was marked done the second it called
//!   `report_done` was closed four minutes later while Owner was still using
//!   it, which read as a random kill; the revive then had to reopen the task
//!   to keep the sweep from closing it again. A revive now counts as use.
//! * It waits [`DONE_DELAY`] after first seeing a task terminal, so a task
//!   marked done by mistake can be reopened before its workers close.
//!
//! COST. It reads ONE planning row per task that owns a live session, on a
//! five-minute cadence — never `GET /tasks`. A periodic whole-board poll is the
//! precise shape that cost ~80 GiB/day before 2026-09-10 (CLAUDE.md), and the
//! sweep only ever needs the handful of rows that own a session.
use crate::state::DaemonState;
use std::collections::{BTreeSet, HashMap};
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

/// A session Owner used this recently is never swept.
pub const OWNER_GRACE: Duration = Duration::from_secs(2 * 3600);

/// How long a task must have been seen terminal before its workers close.
pub const DONE_DELAY: Duration = Duration::from_secs(15 * 60);

/// The owner-activity rule's inputs: the window, and the moment this brain
/// started. Owner-view stamps live in memory only, so every session counts as
/// used at brain start: a deploy delays sweeps by one window instead of
/// forgetting a session Owner was looking at.
#[derive(Clone, Copy, Debug)]
pub struct OwnerGrace {
    pub window: Duration,
    pub floor: Instant,
}

/// True when Owner typed into `sess`, showed it in the TUI, or revived or
/// resumed it within `grace.window` (or the brain started within it).
pub fn owner_recently_used(
    state: &DaemonState,
    uid: &str,
    sess: &crate::session::DaemonSession,
    grace: OwnerGrace,
) -> bool {
    let typed = *sess
        .last_operator_input_at
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    [typed, state.owner_seen.get(uid).copied(), Some(grace.floor)]
        .into_iter()
        .flatten()
        .any(|at| at.elapsed() < grace.window)
}

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
    grace: OwnerGrace,
    done_delay: Duration,
    /// When each candidate task was first seen terminal by this brain.
    terminal_since: Mutex<HashMap<String, Instant>>,
}

impl TerminalTaskSweeper {
    pub fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        Self::with_timing(state, OWNER_GRACE, DONE_DELAY)
    }

    pub fn with_timing(
        state: Arc<Mutex<DaemonState>>,
        owner_grace: Duration,
        done_delay: Duration,
    ) -> Self {
        Self {
            state,
            shutdown: Arc::new(AtomicBool::new(false)),
            handle: Mutex::new(None),
            grace: OwnerGrace { window: owner_grace, floor: Instant::now() },
            done_delay,
            terminal_since: Mutex::new(HashMap::new()),
        }
    }

    /// Whether a task seen with `status` now has waited out the done delay.
    /// Records the first terminal sighting and forgets a reopened task.
    fn due(&self, task_id: &str, status: &str) -> bool {
        let mut since = self.terminal_since.lock().unwrap_or_else(|p| p.into_inner());
        if !is_terminal(status) {
            since.remove(task_id);
            return false;
        }
        since
            .entry(task_id.to_string())
            .or_insert_with(Instant::now)
            .elapsed()
            >= self.done_delay
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
        let candidates = candidate_task_ids(&self.state);
        {
            // Forget tasks that no longer own a sweepable session.
            let keep: BTreeSet<&String> = candidates.iter().collect();
            self.terminal_since
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|task, _| keep.contains(task));
        }
        for task_id in candidates {
            if self.shutdown.load(Ordering::SeqCst) {
                break;
            }
            let (status, via_item) = match crate::planning_client::fetch_task_terminal(
                &task_id,
                Some(api_url.as_str()),
                Some(api_token.as_str()),
            ) {
                Ok(Some(row)) => {
                    let via_item = row.terminal_via.as_deref()
                        == Some(crate::lifecycle_guard::TERMINAL_VIA_ITEM);
                    (row.status, via_item)
                }
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
            if !self.due(&task_id, &status) {
                continue;
            }
            // A board-originated close (the status mirror) only unassigns:
            // the guard lets the sweep close ephemeral sessions alone
            // (doc/org-model.md §7.1).
            let swept = crate::control::methods::sweep_terminal_task_sessions(
                &self.state,
                &task_id,
                "terminal-task-sweep",
                Some(self.grace),
                via_item,
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
    fn a_task_waits_out_the_done_delay_and_reopening_resets_it() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let slow = TerminalTaskSweeper::with_timing(
            Arc::clone(&state),
            OWNER_GRACE,
            Duration::from_secs(3600),
        );
        assert!(!slow.due("t", "done"), "first sighting starts the clock");
        assert!(!slow.due("t", "done"), "still inside the delay");
        assert!(!slow.due("t", "running"), "reopened");
        assert!(!slow.terminal_since.lock().unwrap().contains_key("t"));

        let instant = TerminalTaskSweeper::with_timing(Arc::clone(&state), OWNER_GRACE, Duration::ZERO);
        assert!(instant.due("t", "archived"));
        assert!(!instant.due("t", "blocked"));
    }

    #[test]
    fn the_defaults_match_the_spec() {
        // Owner: never close what Owner used in the last two hours; give a
        // just-marked-done task a quarter hour to be reopened.
        assert_eq!(OWNER_GRACE, Duration::from_secs(7200));
        assert_eq!(DONE_DELAY, Duration::from_secs(900));
        let sweeper = TerminalTaskSweeper::new(Arc::new(Mutex::new(DaemonState::default())));
        assert_eq!(sweeper.grace.window, OWNER_GRACE);
        assert_eq!(sweeper.done_delay, DONE_DELAY);
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
