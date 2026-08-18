//! Startup orphan sweep (wedge campaign F3) — close runs the previous daemon
//! instance left `Running`.
//!
//! ## Why the per-tick reconciler can't do this
//!
//! A daemon restart (deploy) or box reboot kills every hosted PTY child, and
//! the exit is never observed — `handle_session_exit`'s run-closing hook needs
//! the reaper thread that died with the old daemon. The stranded run then
//! dodges BOTH standing reconcilers:
//!
//!   * `scheduler::reconcile_orphans` only closes a run whose session is DEAD
//!     — but the S5 restore (`restore_sessions`) resumes a supervised
//!     persistent orchestrator at its SAME uid within seconds of startup, so
//!     by the first tick the session is alive again. Alive, at its prompt,
//!     and never re-driven: nothing will ever `report_done` the old run.
//!     (Incidents #6 / #9 / #17 of the 2026-08-18 wedge characterization —
//!     three restart-orphan clusters in 14 days, each needing a manual
//!     `force_done` sweep the runbook prescribed as a standing step.)
//!   * `auth_wedge_pass`'s auto-close was Consumer-scoped (F2 widens it), and
//!     even widened it waits out the full wedge grace.
//!
//! So: at startup, BEFORE `restore_sessions` resumes anything, every task
//! whose `last_run` is `Running` (or whose `in_flight` spawn guard is armed)
//! is either **closed as a restart orphan** — the daemon just started, so no
//! session process it owns is mid-run — or **re-adopted** when the run's
//! session process demonstrably survived (kernel-visible via its per-session
//! `--mcp-config …/mcp/<uid>/…` argv marker; a survivor can still complete
//! its run over the fresh daemon's socket, so closing it would be wrong).
//!
//! Closing is attributed on the same runs.jsonl path every other closer uses:
//! event `"restart_orphaned"`, status `"orphaned"`, trigger_source
//! `"startup-sweep"` — so the external reaper / guard_watch / forensics see
//! an attributed close, never an unexplained one.

use crate::continuous::runlog::{ContinuousRunLog, RunLogLine};
use crate::continuous::task::{self, RunStatus};

/// Provenance stamped on every sweep-written runs.jsonl line.
pub const STARTUP_SWEEP_TOKEN: &str = "startup-sweep";

/// Run the sweep against the live `/proc` prober. Called once from
/// `lib.rs::run()` at daemon startup, before `restore_sessions`.
pub fn startup_orphan_sweep() {
    startup_orphan_sweep_with(session_process_alive)
}

/// Testable core: `alive(uid)` answers "does a kernel process for this
/// session uid still exist right now?".
pub fn startup_orphan_sweep_with(alive: impl Fn(&str) -> bool) {
    let now = task::now_unix();
    let mut orphaned = 0usize;
    let mut readopted = 0usize;
    for tk in task::load_all() {
        // Resolve the stranded run from whichever guard is present — the same
        // two shapes `reconcile_orphans` handles: an armed `in_flight` (died
        // during the spawn window) or a `Running` `last_run` (died mid-run).
        let stranded: Option<(Option<String>, u64, String)> =
            if let Some(inflight) = tk.in_flight.as_ref() {
                let seq = tk
                    .last_run
                    .as_ref()
                    .map(|r| r.seq)
                    .unwrap_or(tk.run_count as u64);
                Some((
                    Some(inflight.session_uid.clone()),
                    seq,
                    inflight.fire_token.clone(),
                ))
            } else {
                match tk.last_run.as_ref() {
                    Some(lr) if lr.status == RunStatus::Running => {
                        Some((lr.session_uid.clone(), lr.seq, lr.fire_token.clone()))
                    }
                    _ => None,
                }
            };
        let Some((session_uid, seq, fire_token)) = stranded else {
            continue;
        };

        // Re-adopt: the session's process survived the restart (e.g. the old
        // daemon was SIGKILLed without its cgroup, and the agent kept
        // running). It can still `report_done` over the new daemon's socket —
        // leave the run open, but say so on the audit trail.
        if let Some(uid) = session_uid.as_deref() {
            if alive(uid) {
                readopted += 1;
                append_line(
                    &tk.task_id,
                    seq,
                    now,
                    "readopted",
                    Some(fire_token),
                    session_uid.clone(),
                    "running",
                    format!(
                        "daemon restart: session process for {} survived — run \
                         left Running (re-adopted)",
                        uid,
                    ),
                );
                continue;
            }
        }

        // Orphan: the daemon just started and the run's session process is
        // gone, so nothing can ever finish this run. Close it (guarded on
        // seq/status so a concurrent writer wins) and clear the spawn guard.
        let mut closed = false;
        let _ = task::modify(&tk.task_id, |t| {
            if let Some(run) = t.last_run.as_mut() {
                if run.seq == seq && run.status == RunStatus::Running {
                    run.status = RunStatus::Orphaned;
                    if run.finished_at.is_none() {
                        run.finished_at = Some(now);
                    }
                    closed = true;
                }
            }
            t.in_flight = None;
        });
        if !closed && tk.in_flight.is_none() {
            continue; // nothing actually stranded (raced a writer)
        }
        orphaned += 1;
        append_line(
            &tk.task_id,
            seq,
            now,
            "restart_orphaned",
            Some(fire_token),
            session_uid,
            "orphaned",
            "daemon start: run was Running from a previous daemon instance \
             with no live session process — closed as restart orphan (the \
             schedule refires naturally)"
                .to_string(),
        );
    }
    if orphaned + readopted > 0 {
        eprintln!(
            "cm-daemon: startup orphan sweep closed {} stranded run(s) as \
             restart orphans, re-adopted {} with surviving processes",
            orphaned, readopted,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn append_line(
    task_id: &str,
    seq: u64,
    now: u64,
    event: &str,
    fire_token: Option<String>,
    session_uid: Option<String>,
    status: &str,
    detail: String,
) {
    if let Err(e) = ContinuousRunLog::append(&RunLogLine {
        seq,
        ts: now as f64,
        task_id: task_id.to_string(),
        event: event.to_string(),
        fire_token,
        session_uid,
        run_mode: None,
        trigger_source: Some(STARTUP_SWEEP_TOKEN.to_string()),
        status: Some(status.to_string()),
        detail: Some(detail.into()),
    }) {
        eprintln!(
            "cm-daemon: startup sweep failed to append \"{}\" audit line for {}: {}",
            event, task_id, e,
        );
    }
}

/// Kernel-level liveness probe for a session uid: scan `/proc/<pid>/cmdline`
/// for the per-session MCP-config marker (`…/mcp/<uid>/…`) every
/// claude/codex spawn carries in argv. Best-effort by design: bash sessions
/// carry no marker and read as dead — for a startup sweep that is the safe
/// default (a bash continuous run that lost its daemon can't `report_done`
/// anyway, since it never could).
fn session_process_alive(uid: &str) -> bool {
    if uid.is_empty() {
        return false;
    }
    let marker = format!("/mcp/{}/", uid);
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let cmdline = String::from_utf8_lossy(&raw);
        if cmdline.contains(&marker) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuous::task::{ContinuousTask, Engine, InFlight, RunMode, RunRecord, Schedule};

    fn with_temp_home<F: FnOnce()>(f: F) {
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
    }

    fn task_with_running_run(id: &str, uid: &str) -> ContinuousTask {
        let mut tk = ContinuousTask::new(
            id.into(),
            "label".into(),
            "ws".into(),
            "/tmp/wt".into(),
            Engine::Claude,
            RunMode::Persistent,
            Schedule::Periodic { every_secs: 3600 },
            "go".into(),
        );
        tk.current_session_uid = Some(uid.into());
        tk.last_run = Some(RunRecord {
            seq: 7,
            fire_token: "ft_x".into(),
            started_at: 1000,
            finished_at: None,
            session_uid: Some(uid.into()),
            status: RunStatus::Running,
            trigger_source: "continuous-scheduler".into(),
        });
        tk
    }

    fn read_runlog(id: &str) -> Vec<serde_json::Value> {
        let path = task::runs_log_path(id);
        std::fs::read_to_string(path)
            .map(|s| {
                s.lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A run left `Running` by the previous daemon instance whose session
    /// process is gone is closed `Orphaned` with a `restart_orphaned` audit
    /// line attributed to the startup sweep.
    #[test]
    fn sweep_closes_stranded_run_as_restart_orphan() {
        with_temp_home(|| {
            task::save(&task_with_running_run("t-orphan", "ts-aa-1")).unwrap();

            startup_orphan_sweep_with(|_| false);

            let tk = task::load_one("t-orphan").unwrap();
            let run = tk.last_run.unwrap();
            assert_eq!(run.status, RunStatus::Orphaned);
            assert!(run.finished_at.is_some(), "close stamps finished_at");
            assert!(tk.in_flight.is_none());

            let log = read_runlog("t-orphan");
            assert_eq!(log.len(), 1);
            assert_eq!(log[0]["event"], "restart_orphaned");
            assert_eq!(log[0]["status"], "orphaned");
            assert_eq!(log[0]["trigger_source"], STARTUP_SWEEP_TOKEN);
            assert_eq!(log[0]["seq"], 7);
            assert_eq!(log[0]["session_uid"], "ts-aa-1");
        });
    }

    /// A surviving session process re-adopts: the run stays `Running` and the
    /// audit trail records the decision.
    #[test]
    fn sweep_readopts_when_session_process_survives() {
        with_temp_home(|| {
            task::save(&task_with_running_run("t-live", "ts-bb-2")).unwrap();

            startup_orphan_sweep_with(|uid| uid == "ts-bb-2");

            let tk = task::load_one("t-live").unwrap();
            assert_eq!(tk.last_run.unwrap().status, RunStatus::Running);

            let log = read_runlog("t-live");
            assert_eq!(log.len(), 1);
            assert_eq!(log[0]["event"], "readopted");
            assert_eq!(log[0]["status"], "running");
        });
    }

    /// An armed `in_flight` guard with no live process is cleared and its run
    /// closed — the fire died inside its spawn window when the daemon did.
    #[test]
    fn sweep_clears_leaked_in_flight_guard() {
        with_temp_home(|| {
            let mut tk = task_with_running_run("t-inflight", "ts-cc-3");
            tk.in_flight = Some(InFlight {
                fire_token: "ft_spawn".into(),
                session_uid: "ts-cc-3".into(),
                started_at: 999,
            });
            task::save(&tk).unwrap();

            startup_orphan_sweep_with(|_| false);

            let tk = task::load_one("t-inflight").unwrap();
            assert!(tk.in_flight.is_none(), "spawn guard cleared");
            assert_eq!(tk.last_run.unwrap().status, RunStatus::Orphaned);
            let log = read_runlog("t-inflight");
            assert_eq!(log[0]["event"], "restart_orphaned");
        });
    }

    /// Terminal runs are untouched — the sweep only sees `Running`.
    #[test]
    fn sweep_ignores_closed_runs() {
        with_temp_home(|| {
            let mut tk = task_with_running_run("t-done", "ts-dd-4");
            tk.last_run.as_mut().unwrap().status = RunStatus::Done;
            task::save(&tk).unwrap();

            startup_orphan_sweep_with(|_| false);

            let tk = task::load_one("t-done").unwrap();
            assert_eq!(tk.last_run.unwrap().status, RunStatus::Done);
            assert!(read_runlog("t-done").is_empty(), "no audit line");
        });
    }
}
