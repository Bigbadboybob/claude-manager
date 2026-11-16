//! Background planning-parent refresher for headless hosts.
//!
//! The descendant-scope checks (`auth::task_in_scope`) walk
//! `DaemonState.task_tree`, which the TUI's `task.update_tree` push fills
//! with every task's parent edge. A host no TUI has pushed to lately (a
//! headless cm-manager after a daemon restart) has no edges for tasks it
//! never saw pushed, so an orchestrator was refused on its own done
//! subtask. This loop reads `parent_task_id` from the planning API for
//! the tasks this daemon's sessions and agent-minted edges reference,
//! when `task_tree` doesn't know them, and keeps the readings in
//! `DaemonState.planning_task_parents`.
//!
//! Off the dispatch path by design: RPCs never make HTTP calls for scope.
//! Bounded per pass (lookups, hops, wall time, per-request timeout), never
//! holds the state lock across a request, and backs off failed lookups.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::state::{DaemonState, PlanningParent};

/// How often a pass runs.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
/// A reading younger than this is reused without asking again.
pub const REFETCH_AFTER: Duration = Duration::from_secs(10 * 60);
/// Planning lookups per pass; the rest wait for the next pass.
pub const MAX_LOOKUPS_PER_PASS: usize = 64;
/// Parent hops followed from one referenced task.
pub const MAX_HOPS: usize = 32;
/// Wall-time budget for one pass's lookups.
pub const PASS_BUDGET: Duration = Duration::from_secs(20);
/// Timeout of one lookup.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);
/// First retry delay after a failed lookup; doubles per failure.
pub const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
pub const FAILURE_BACKOFF_MAX: Duration = Duration::from_secs(30 * 60);

/// What one pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassStats {
    pub lookups: usize,
    pub failures: usize,
}

/// Tasks this daemon's scope checks can be asked about: the tasks of
/// live and exited sessions, and both ends of agent-minted edges.
fn seed_task_ids(state: &DaemonState) -> Vec<String> {
    let mut seen = HashSet::new();
    state
        .sessions
        .values()
        .filter_map(|s| s.task_id.clone())
        .chain(state.recently_exited.iter().filter_map(|t| t.task_id.clone()))
        .chain(state.agent_task_edges.iter().flat_map(|(c, p)| [c.clone(), p.clone()]))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

fn backoff(failures: u32) -> Duration {
    FAILURE_BACKOFF
        .saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(FAILURE_BACKOFF_MAX)
}

/// One pass: walk each seed task's parent chain, using `task_tree` where
/// it knows a task and the planning API where it doesn't, then prune
/// readings and failures no seed reaches any more.
pub fn refresh_once(state_arc: &Arc<Mutex<DaemonState>>) -> PassStats {
    let started = Instant::now();
    let (api_url, api_token, seeds) = {
        let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        (st.config.api_url.clone(), st.config.api_token.clone(), seed_task_ids(&st))
    };
    let mut stats = PassStats::default();
    let mut reached: HashSet<String> = HashSet::new();
    let mut api_unconfigured = false;
    for seed in seeds {
        let mut cur = seed;
        for _ in 0..MAX_HOPS {
            // A task another seed's walk already reached has had its chain
            // walked; a repeat inside this walk is a cycle.
            if !reached.insert(cur.clone()) {
                break;
            }
            let (pushed, cached, retry_at) = {
                let st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                (
                    st.task_tree.get(&cur).cloned(),
                    st.planning_task_parents.get(&cur).cloned(),
                    st.planning_parent_failures.get(&cur).map(|(at, _)| *at),
                )
            };
            let parent = if let Some(parent) = pushed {
                parent
            } else if let Some(c) = cached.as_ref().filter(|c| c.fetched_at.elapsed() < REFETCH_AFTER) {
                c.parent.clone()
            } else if api_unconfigured
                || retry_at.is_some_and(|at| Instant::now() < at)
                || stats.lookups >= MAX_LOOKUPS_PER_PASS
                || started.elapsed() > PASS_BUDGET
            {
                // Keep a stale reading over none; the next pass refreshes it.
                match cached {
                    Some(c) => c.parent,
                    None => break,
                }
            } else {
                stats.lookups += 1;
                match crate::planning_client::fetch_task_parent(
                    &cur,
                    Some(api_url.as_str()),
                    Some(api_token.as_str()),
                    LOOKUP_TIMEOUT,
                ) {
                    Ok(row) => {
                        let parent = row.flatten();
                        let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                        st.planning_parent_failures.remove(&cur);
                        st.planning_task_parents.insert(
                            cur.clone(),
                            PlanningParent { parent: parent.clone(), fetched_at: Instant::now() },
                        );
                        parent
                    }
                    Err(crate::planning_client::PlanningClientError::MissingConfig(_)) => {
                        stats.lookups -= 1;
                        api_unconfigured = true;
                        match cached {
                            Some(c) => c.parent,
                            None => break,
                        }
                    }
                    Err(error) => {
                        stats.failures += 1;
                        let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
                        let entry = st
                            .planning_parent_failures
                            .entry(cur.clone())
                            .or_insert((Instant::now(), 0));
                        entry.1 += 1;
                        entry.0 = Instant::now() + backoff(entry.1);
                        if entry.1 == 1 {
                            eprintln!(
                                "cm-daemon: task parent refresh could not read task {cur}: {error:?} (backing off)"
                            );
                        }
                        drop(st);
                        match cached {
                            Some(c) => c.parent,
                            None => break,
                        }
                    }
                }
            };
            match parent {
                Some(parent) => cur = parent,
                None => break,
            }
        }
    }
    let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
    st.planning_task_parents.retain(|task, _| reached.contains(task));
    st.planning_parent_failures.retain(|task, _| reached.contains(task));
    stats
}

pub struct TaskParentRefresher {
    state: Arc<Mutex<DaemonState>>,
    shutdown: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl TaskParentRefresher {
    pub fn new(state: Arc<Mutex<DaemonState>>) -> Self {
        Self { state, shutdown: Arc::new(AtomicBool::new(false)), handle: Mutex::new(None) }
    }

    pub fn start(self: &Arc<Self>) -> std::io::Result<()> {
        let mut guard = self.handle.lock().unwrap();
        if guard.is_some() {
            return Ok(());
        }
        let me = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("cm-task-parents".into())
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
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| refresh_once(&self.state)));
            if result.is_err() {
                eprintln!("cm-daemon: task parent refresh panicked (continuing — next pass retries)");
            }
            while !self.shutdown.load(Ordering::SeqCst) && started.elapsed() < REFRESH_INTERVAL {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), FAILURE_BACKOFF);
        assert_eq!(backoff(2), FAILURE_BACKOFF * 2);
        assert_eq!(backoff(40), FAILURE_BACKOFF_MAX);
    }
}
