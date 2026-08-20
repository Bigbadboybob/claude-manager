//! Cloud-backtest sidebar group: the data model behind the collapsible
//! `backtests` section at the bottom of the Sessions-view sidebar.
//!
//! Cloud backtest tasks (`kind="backtest"`, `is_cloud=true`) run entirely on
//! ephemeral GCP worker VMs — there is never a local session to attach to, so
//! they must NOT be modelled as workspaces (an empty top-level header per run
//! crowded the sidebar; a perf-bench fleet is K×N of them). Instead
//! `reconcile_tasks` feeds every fetched backtest task through
//! [`update_backtest_rows`] and the sidebar renders the surviving rows as one
//! group: fleets (runs sharing a label stem) fold into a single row, terminal
//! runs linger for a short grace period (longer for failures — they carry a
//! signal a human may act on) and then leave the group. Results stay readable
//! via `get_backtest_result` / GCS regardless — nothing here owns data.

use std::time::{Duration, Instant};

use crate::api::Task;

/// How long a `done` run stays in the group after the TUI first observes the
/// terminal status. Short: success needs no action, and the row's residual
/// value (the result pointer) is one `get_backtest_result` away.
pub(crate) const BACKTEST_DONE_GRACE: Duration = Duration::from_secs(120);
/// How long a `blocked` (failed / reaped) run stays. Longer — failures carry
/// a signal. Anchored on the task's `blocked_at` when parseable (absolute, so
/// it survives TUI restarts), else on first observation. Mirrors the dispatch
/// daemon's failed-VM TTL (`CM_BACKTEST_BLOCKED_VM_TTL_SECS`).
pub(crate) const BACKTEST_FAILED_GRACE: Duration = Duration::from_secs(1800);

/// Lifecycle of one backtest run as the TUI models it. `Failed` covers both
/// a pipeline failure and a runtime-limit reap — the task row doesn't
/// distinguish them (the artifact does, and that's read by id).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BacktestState {
    Queued,
    Running { vm: Option<String> },
    Done,
    Failed,
}

/// One backtest task, reduced to what the sidebar shows. Derived from the
/// planning API's task rows each refresh — never persisted.
#[derive(Clone, Debug)]
pub(crate) struct BacktestRow {
    pub task_id: String,
    /// Display label: `metadata.backtest.label`, else the task name minus
    /// its `backtest: ` prefix / ` @ <branch>` suffix, else the short id.
    pub label: String,
    pub branch: String,
    pub state: BacktestState,
    /// Keys the GCS results prefix (`backtests/<run_key>/`).
    pub run_key: Option<String>,
    /// `metadata.backtest.launched_at` (unix ms) — runtime anchor.
    pub launched_ms: Option<u64>,
    /// `blocked_at` (unix ms) — absolute grace anchor for failures.
    pub blocked_ms: Option<u64>,
    /// When THIS TUI first saw the run in a terminal state. Grace anchor for
    /// `done` (the API row carries no completion timestamp) and the fallback
    /// for `blocked` rows with an unparseable `blocked_at`.
    pub first_seen_terminal: Option<Instant>,
    /// When THIS TUI first saw the run `running` — runtime fallback when
    /// `launched_at` is missing.
    pub running_since: Option<Instant>,
}

/// How the group renders a set of rows: singletons stay flat; runs sharing a
/// label stem (the label minus its last `-<segment>`) collapse into a fleet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BacktestGroupItem {
    Single(usize),
    Fleet { stem: String, members: Vec<usize> },
}

/// The label stem a fleet groups under: everything before the last `-`.
/// `None` when there's no `-` or the stem would be empty (`"-x"`).
pub(crate) fn fleet_stem(label: &str) -> Option<&str> {
    match label.rfind('-') {
        Some(i) if i > 0 => Some(&label[..i]),
        _ => None,
    }
}

/// Reconcile the sidebar's backtest rows against a fresh API task list.
///
/// Pure with respect to time: callers pass `now` (grace clocks) and `now_ms`
/// (unix wall clock, for the absolute `blocked_at` anchor) so tests can drive
/// both. Rows keep their observation stamps across refreshes; rows whose task
/// vanished from the fetch (archived / deleted) drop immediately; terminal
/// rows drop once their grace expires.
pub(crate) fn update_backtest_rows(
    rows: &mut Vec<BacktestRow>,
    tasks: &[Task],
    now: Instant,
    now_ms: u64,
) {
    let mut next: Vec<BacktestRow> = Vec::new();
    for task in tasks {
        if task.kind != "backtest" || !task.is_cloud {
            continue;
        }
        let state = match task.status.as_str() {
            "backlog" => BacktestState::Queued,
            "running" => BacktestState::Running {
                vm: task.worker_vm.clone().filter(|v| !v.is_empty()),
            },
            "done" => BacktestState::Done,
            "blocked" => BacktestState::Failed,
            // draft (not yet submitted), archived (already swept server-side,
            // and excluded from the default fetch anyway), anything unknown.
            _ => continue,
        };

        let bt = task
            .metadata
            .as_ref()
            .and_then(|m| m.get("backtest"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let meta_str = |key: &str| -> Option<String> {
            bt.get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };

        let label = meta_str("label")
            .or_else(|| task.name.as_deref().map(strip_backtest_name))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| task.id.chars().take(8).collect());
        let branch = meta_str("branch").unwrap_or_else(|| task.repo_branch.clone());
        let launched_ms = meta_str("launched_at")
            .as_deref()
            .and_then(cm_daemon::workflow::history::iso8601_to_ms);
        let blocked_ms = task
            .blocked_at
            .as_deref()
            .and_then(cm_daemon::workflow::history::iso8601_to_ms);

        let prev = rows.iter().find(|r| r.task_id == task.id);
        let terminal = matches!(state, BacktestState::Done | BacktestState::Failed);
        let first_seen_terminal = match prev.and_then(|p| p.first_seen_terminal) {
            Some(t) if terminal => Some(t),
            _ if terminal => Some(now),
            _ => None,
        };
        let running_since = match prev.and_then(|p| p.running_since) {
            Some(t) => Some(t),
            None if matches!(state, BacktestState::Running { .. }) => Some(now),
            None => None,
        };

        let row = BacktestRow {
            task_id: task.id.clone(),
            label,
            branch,
            state,
            run_key: meta_str("run_key"),
            launched_ms,
            blocked_ms,
            first_seen_terminal,
            running_since,
        };
        if !grace_expired(&row, now, now_ms) {
            next.push(row);
        }
    }
    *rows = next;
}

/// Terminal-row linger policy (task eb71a9c6, absorbed here): a `done` row
/// leaves [`BACKTEST_DONE_GRACE`] after first observed terminal; a failed row
/// leaves [`BACKTEST_FAILED_GRACE`] after `blocked_at` (absolute) or first
/// observation when `blocked_at` didn't parse.
fn grace_expired(row: &BacktestRow, now: Instant, now_ms: u64) -> bool {
    match row.state {
        BacktestState::Queued | BacktestState::Running { .. } => false,
        BacktestState::Done => row
            .first_seen_terminal
            .is_some_and(|t| now.duration_since(t) > BACKTEST_DONE_GRACE),
        BacktestState::Failed => match row.blocked_ms {
            Some(b) => now_ms.saturating_sub(b)
                > BACKTEST_FAILED_GRACE.as_millis() as u64,
            None => row
                .first_seen_terminal
                .is_some_and(|t| now.duration_since(t) > BACKTEST_FAILED_GRACE),
        },
    }
}

/// `"backtest: <label> @ <branch>"` → `<label>` (the shape submit_backtest
/// mints). Tolerates names that never had the prefix/suffix.
fn strip_backtest_name(name: &str) -> String {
    let name = name.strip_prefix("backtest: ").unwrap_or(name);
    match name.rfind(" @ ") {
        Some(i) => name[..i].to_string(),
        None => name.to_string(),
    }
}

/// Partition rows into fleets (≥2 rows sharing a label stem) and singletons,
/// in first-appearance order. A perf-bench round submitting K runs labelled
/// `perf-bench-hist-k3-<arm>` reads as ONE fleet row with K children instead
/// of K top-level rows.
pub(crate) fn group_backtest_rows(rows: &[BacktestRow]) -> Vec<BacktestGroupItem> {
    use std::collections::HashMap;
    let mut stem_counts: HashMap<&str, usize> = HashMap::new();
    for row in rows {
        if let Some(stem) = fleet_stem(&row.label) {
            *stem_counts.entry(stem).or_insert(0) += 1;
        }
    }
    let mut out: Vec<BacktestGroupItem> = Vec::new();
    let mut emitted_stems: Vec<String> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        match fleet_stem(&row.label) {
            Some(stem) if stem_counts[stem] >= 2 => {
                if emitted_stems.iter().any(|s| s == stem) {
                    continue; // fleet already emitted at its first member
                }
                emitted_stems.push(stem.to_string());
                let members = rows
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| fleet_stem(&r.label) == Some(stem))
                    .map(|(j, _)| j)
                    .collect();
                out.push(BacktestGroupItem::Fleet {
                    stem: stem.to_string(),
                    members,
                });
            }
            _ => out.push(BacktestGroupItem::Single(i)),
        }
    }
    out
}

/// Elapsed runtime of a run for display, preferring the daemon-stamped
/// `launched_at` (absolute), falling back to when this TUI first saw it
/// running. `None` for queued rows / no anchor.
pub(crate) fn runtime_secs(row: &BacktestRow, now: Instant, now_ms: u64) -> Option<u64> {
    if matches!(row.state, BacktestState::Queued) {
        return None;
    }
    // For terminal rows a "runtime so far" clock would keep counting past the
    // finish; without an end timestamp on the row, freeze at the terminal
    // observation instead of showing a still-growing number.
    let (end_now, end_ms) = match row.first_seen_terminal {
        Some(t) if matches!(row.state, BacktestState::Done | BacktestState::Failed) => {
            let ms_back = now.duration_since(t).as_millis() as u64;
            (t, now_ms.saturating_sub(ms_back))
        }
        _ => (now, now_ms),
    };
    match row.launched_ms {
        Some(l) => Some(end_ms.saturating_sub(l) / 1000),
        None => row
            .running_since
            .map(|s| end_now.duration_since(s).as_secs()),
    }
}

/// `95s` → `"1m35s"`, `4000s` → `"1h06m"` — compact enough for a sidebar row.
pub(crate) fn fmt_runtime(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, status: &str, label: &str) -> Task {
        let json = serde_json::json!({
            "id": id,
            "created_at": "2026-08-20T00:00:00Z",
            "repo_url": "git@github.com:x/predictionTrading.git",
            "repo_branch": "main",
            "name": format!("backtest: {} @ main", label),
            "prompt": null,
            "status": status,
            "worker_vm": null,
            "worker_zone": null,
            "blocked_at": null,
            "session_id": null,
            "wip_branch": null,
            "is_cloud": true,
            "kind": "backtest",
            "metadata": {"backtest": {"label": label, "run_key": format!("rk-{}", id)}},
        });
        serde_json::from_value(json).expect("task json")
    }

    fn update(rows: &mut Vec<BacktestRow>, tasks: &[Task], now: Instant, now_ms: u64) {
        update_backtest_rows(rows, tasks, now, now_ms);
    }

    #[test]
    fn non_backtest_and_non_cloud_tasks_are_ignored() {
        let mut plain = task("t1", "running", "x-1");
        plain.kind = "oneshot".into();
        let mut local_bt = task("t2", "running", "x-2");
        local_bt.is_cloud = false;
        let mut rows = Vec::new();
        update(&mut rows, &[plain, local_bt], Instant::now(), 0);
        assert!(rows.is_empty());
    }

    #[test]
    fn statuses_map_to_states_and_vm_rides_running() {
        let mut running = task("t1", "running", "a-1");
        running.worker_vm = Some("cm-bt-xyz".into());
        let tasks = vec![
            task("t0", "backlog", "a-0"),
            running,
            task("t2", "done", "a-2"),
            task("t3", "blocked", "a-3"),
            task("t4", "draft", "a-4"),
        ];
        let mut rows = Vec::new();
        update(&mut rows, &tasks, Instant::now(), 0);
        let states: Vec<_> = rows.iter().map(|r| r.state.clone()).collect();
        assert_eq!(
            states,
            vec![
                BacktestState::Queued,
                BacktestState::Running { vm: Some("cm-bt-xyz".into()) },
                BacktestState::Done,
                BacktestState::Failed,
            ]
        );
        assert_eq!(rows[1].run_key.as_deref(), Some("rk-t1"));
    }

    #[test]
    fn done_rows_expire_after_done_grace_only() {
        let t0 = Instant::now();
        let mut rows = Vec::new();
        let tasks = vec![task("t1", "done", "a-1")];
        update(&mut rows, &tasks, t0, 0);
        assert_eq!(rows.len(), 1, "fresh done row lingers");

        // Still inside the grace window.
        let mid = t0 + BACKTEST_DONE_GRACE / 2;
        update(&mut rows, &tasks, mid, 0);
        assert_eq!(rows.len(), 1);

        // Past it — and the anchor must be the FIRST observation, not the
        // latest refresh.
        let past = t0 + BACKTEST_DONE_GRACE + Duration::from_secs(1);
        update(&mut rows, &tasks, past, 0);
        assert!(rows.is_empty(), "done row leaves after grace");
    }

    #[test]
    fn failed_rows_use_blocked_at_and_longer_grace() {
        let mut t = task("t1", "blocked", "a-1");
        t.blocked_at = Some("2026-08-20T10:00:00Z".to_string());
        let blocked_ms = cm_daemon::workflow::history::iso8601_to_ms(
            "2026-08-20T10:00:00Z",
        )
        .unwrap();
        let now = Instant::now();
        let mut rows = Vec::new();

        // Done-grace past, failed-grace not: row stays (failures linger longer).
        let after_done_grace =
            blocked_ms + BACKTEST_DONE_GRACE.as_millis() as u64 + 1000;
        update(&mut rows, &[t.clone()], now, after_done_grace);
        assert_eq!(rows.len(), 1);

        // Past the failed grace (absolute anchor — no prior observation
        // needed, so a TUI (re)started late doesn't resurrect old failures).
        let past = blocked_ms + BACKTEST_FAILED_GRACE.as_millis() as u64 + 1000;
        let mut fresh = Vec::new();
        update(&mut fresh, &[t], now, past);
        assert!(fresh.is_empty());
    }

    #[test]
    fn vanished_tasks_drop_immediately() {
        let mut rows = Vec::new();
        update(&mut rows, &[task("t1", "running", "a-1")], Instant::now(), 0);
        assert_eq!(rows.len(), 1);
        update(&mut rows, &[], Instant::now(), 0);
        assert!(rows.is_empty(), "archived/deleted rows leave the group");
    }

    #[test]
    fn fleet_grouping_collects_shared_stems_and_leaves_singletons_flat() {
        let tasks = vec![
            task("t1", "running", "perf-bench-hist-k3-p1"),
            task("t2", "backlog", "solo-run"),
            task("t3", "backlog", "perf-bench-hist-k3-p2"),
            task("t4", "backlog", "perf-bench-hist-k3-p3"),
        ];
        let mut rows = Vec::new();
        update(&mut rows, &tasks, Instant::now(), 0);
        let groups = group_backtest_rows(&rows);
        assert_eq!(
            groups,
            vec![
                BacktestGroupItem::Fleet {
                    stem: "perf-bench-hist-k3".into(),
                    members: vec![0, 2, 3],
                },
                // "solo-run" HAS a dash, but no sibling shares the stem.
                BacktestGroupItem::Single(1),
            ]
        );
    }

    #[test]
    fn dashless_labels_never_fleet() {
        let tasks = vec![task("t1", "backlog", "alpha"), task("t2", "backlog", "beta")];
        let mut rows = Vec::new();
        update(&mut rows, &tasks, Instant::now(), 0);
        assert_eq!(
            group_backtest_rows(&rows),
            vec![BacktestGroupItem::Single(0), BacktestGroupItem::Single(1)]
        );
    }

    #[test]
    fn label_falls_back_to_name_then_short_id() {
        let mut named = task("aaaabbbb-cccc", "backlog", "x-1");
        named.metadata = None;
        let mut rows = Vec::new();
        update(&mut rows, &[named], Instant::now(), 0);
        assert_eq!(rows[0].label, "x-1", "derived from 'backtest: x-1 @ main'");

        let mut bare = task("aaaabbbb-cccc", "backlog", "x-1");
        bare.metadata = None;
        bare.name = None;
        let mut rows = Vec::new();
        update(&mut rows, &[bare], Instant::now(), 0);
        assert_eq!(rows[0].label, "aaaabbbb");
    }

    #[test]
    fn runtime_prefers_launched_at_and_freezes_at_terminal_observation() {
        let now_ms: u64 = 1_000_000_000;
        let t0 = Instant::now();
        let mut row = BacktestRow {
            task_id: "t".into(),
            label: "l".into(),
            branch: "main".into(),
            state: BacktestState::Running { vm: None },
            run_key: None,
            launched_ms: Some(now_ms - 90_000),
            blocked_ms: None,
            first_seen_terminal: None,
            running_since: None,
        };
        assert_eq!(runtime_secs(&row, t0, now_ms), Some(90));

        // Terminal 10s ago (observed then), refreshed now: clock frozen at 90s
        // even though the wall clock has moved on.
        row.state = BacktestState::Done;
        row.first_seen_terminal = Some(t0);
        assert_eq!(
            runtime_secs(&row, t0 + Duration::from_secs(10), now_ms + 10_000),
            Some(90)
        );

        row.launched_ms = None;
        row.running_since = Some(t0 - Duration::from_secs(30));
        row.state = BacktestState::Running { vm: None };
        row.first_seen_terminal = None;
        assert_eq!(runtime_secs(&row, t0, now_ms), Some(30));

        row.state = BacktestState::Queued;
        assert_eq!(runtime_secs(&row, t0, now_ms), None);
    }

    #[test]
    fn fmt_runtime_buckets() {
        assert_eq!(fmt_runtime(42), "42s");
        assert_eq!(fmt_runtime(95), "1m35s");
        assert_eq!(fmt_runtime(4000), "1h06m");
    }
}
