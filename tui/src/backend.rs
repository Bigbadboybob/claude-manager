//! Background threads between the UI and the planning API.
//!
//! Two threads share one [`TaskCache`]:
//!
//! * the **feed thread** ([`feed_loop`]) keeps the cache current through the
//!   incremental feed (`GET /tasks/changes`): one consistent snapshot, then
//!   long-polled change pages (idle ≈ one ~100-byte reply per 25 s instead of
//!   the pre-feed 5 MB full list every 5 s). It falls back to legacy full
//!   polling against an API that predates the feed and re-probes it later.
//! * the **command thread** ([`backend_loop`]) executes UI-initiated writes
//!   and re-emits the cached lists every [`HEARTBEAT`] so the app's
//!   time-gated sweeps keep their cadence without any network traffic.
//!
//! Consistency: a feed page is applied only if the cache still sits at the
//! cursor (and generation) the request was issued with; otherwise it is
//! discarded and the next poll resumes from the newer cursor. That rules out
//! stale overwrites across reconnects and forced snapshots.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::api::{
    is_unsupported, ApiClient, ChangeCursor, Task, TaskChangesPage, TaskCreateBody,
};
use crate::config::Config;

/// Cadence at which the cached lists are re-emitted to the UI (no network).
pub const HEARTBEAT: Duration = Duration::from_secs(5);
/// Server-side hold for an idle long poll; must stay under the client's 30 s
/// global request timeout (`ApiClient::new`).
const LONG_POLL_WAIT_SECS: f32 = 25.0;
/// Full-list cadence while the API lacks the feed (pre-feed behaviour).
const LEGACY_POLL: Duration = Duration::from_secs(5);
/// How long to stay in legacy mode before probing the feed again.
const LEGACY_REPROBE: Duration = Duration::from_secs(60);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Local mirror of the server's (non-archived) task list plus the feed
/// resume point.
#[derive(Default)]
pub struct TaskCache {
    tasks: HashMap<String, Task>,
    cursor: Option<ChangeCursor>,
    /// Bumped on every wholesale replacement; a page issued against an older
    /// generation is discarded on apply.
    generation: u64,
    /// True once any snapshot/list has been applied. Nothing is emitted
    /// before that: an empty `TasksUpdated` would make the app's workspace
    /// sweeps act on a vacuously empty task set.
    loaded: bool,
}

/// What the feed request was issued against (see `TaskCache::apply`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuePoint {
    cursor: Option<ChangeCursor>,
    generation: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// The page was applied; `changed` says whether any task differs.
    Applied { changed: bool },
    /// The cache moved since the request was issued; nothing was applied.
    Discarded,
}

fn status_rank(status: &str) -> u8 {
    match status {
        "blocked" => 0,
        "running" => 1,
        "backlog" => 2,
        "draft" => 3,
        "done" => 4,
        "archived" => 5,
        _ => 6,
    }
}

impl TaskCache {
    pub fn issue_point(&self) -> IssuePoint {
        IssuePoint {
            cursor: self.cursor.clone(),
            generation: self.generation,
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn cursor(&self) -> Option<&ChangeCursor> {
        self.cursor.as_ref()
    }

    /// Wholesale replacement (a forced snapshot or a legacy full list).
    pub fn replace_all(&mut self, tasks: Vec<Task>, cursor: Option<ChangeCursor>) {
        self.tasks = tasks.into_iter().map(|t| (t.id.clone(), t)).collect();
        self.cursor = cursor;
        self.generation += 1;
        self.loaded = true;
    }

    /// Apply a feed page fetched against `issued`. Compare-and-swap: the
    /// page is dropped when the cache's cursor or generation moved since.
    pub fn apply(&mut self, issued: &IssuePoint, page: TaskChangesPage) -> ApplyOutcome {
        if self.issue_point() != *issued {
            return ApplyOutcome::Discarded;
        }
        let cursor = Some(ChangeCursor {
            epoch: page.epoch,
            seq: page.cursor,
        });
        if page.reset {
            self.replace_all(page.tasks.unwrap_or_default(), cursor);
            return ApplyOutcome::Applied { changed: true };
        }
        let mut changed = false;
        for change in page.changes {
            match (change.op.as_str(), change.task) {
                ("upsert", Some(task)) => {
                    self.tasks.insert(change.task_id, task);
                    changed = true;
                }
                _ => {
                    if self.tasks.remove(&change.task_id).is_some() {
                        changed = true;
                    }
                }
            }
        }
        self.cursor = cursor;
        ApplyOutcome::Applied { changed }
    }

    /// The task list in the API's list order (status rank, priority is not
    /// part of the client struct, so created_at breaks ties).
    pub fn materialize(&self) -> Vec<Task> {
        let mut out: Vec<Task> = self.tasks.values().cloned().collect();
        out.sort_by(|a, b| {
            status_rank(&a.status)
                .cmp(&status_rank(&b.status))
                .then_with(|| a.created_at.cmp(&b.created_at))
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }
}


/// Commands sent from the main thread to the background thread.
pub enum BackendCommand {
    Refresh,
    UpdateTask {
        id: String,
        fields: HashMap<String, serde_json::Value>,
    },
    DeleteTask {
        id: String,
    },
    /// Create a planning task in the DB.
    CreatePlanTask {
        project: String,
        repo_url: String,
        name: String,
        description: String,
        status: String,
        /// `Some(parent_id)` → posted as a subtask (parent_task_id flows
        /// to the API row). `None` → top-level task.
        parent_task_id: Option<String>,
        /// Worktree mode for subtasks; persisted on the API row so a
        /// future launch flow can branch correctly. Ignored when
        /// `parent_task_id` is None.
        worktree_mode: Option<String>,
    },
    /// Update a planning task in the DB.
    UpdatePlanTask {
        id: String,
        fields: HashMap<String, serde_json::Value>,
    },
    /// Delete a planning task from the DB.
    DeletePlanTask {
        id: String,
    },
    /// Fetch all planning tasks (tasks that have a project set).
    RefreshPlanTasks,
    Shutdown,
}

/// Events sent from the background thread to the main thread.
pub enum BackendEvent {
    TasksUpdated(Vec<Task>),
    ApiError(String),
    Connected,
    Disconnected,
    /// Progress message for multi-step operations.
    Progress(String),
    /// Planning tasks updated (all tasks with a project field).
    PlanTasksUpdated(Vec<Task>),
    /// A single planning task was updated — merge into local state.
    PlanTaskUpdated(Task),
    /// A planning task was created — return the full task.
    PlanTaskCreated(Task),
    /// A planning task was deleted.
    PlanTaskDeleted(String),
}

/// Handle to the background threads.
pub struct BackendHandle {
    pub cmd_tx: mpsc::Sender<BackendCommand>,
    pub event_rx: mpsc::Receiver<BackendEvent>,
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl BackendHandle {
    pub fn spawn(config: &Config) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
        let (event_tx, event_rx) = mpsc::channel::<BackendEvent>();

        let cache = Arc::new(Mutex::new(TaskCache::default()));
        let stop = Arc::new(AtomicBool::new(false));

        // Feed thread: owns the incremental subscription. It may sit in a
        // 25 s long poll, so it is detached — `shutdown` flags it and moves on.
        {
            let client = ApiClient::new(config);
            let event_tx = event_tx.clone();
            let cache = cache.clone();
            let stop = stop.clone();
            thread::spawn(move || feed_loop(client, event_tx, cache, stop));
        }

        let client = ApiClient::new(config);
        let gcp_project = config.gcp_project.clone();
        let gcp_zone = config.gcp_zone.clone();
        let stop_cmd = stop.clone();
        let thread = thread::spawn(move || {
            backend_loop(client, cmd_rx, event_tx, cache, stop_cmd, &gcp_project, &gcp_zone);
        });

        BackendHandle {
            cmd_tx,
            event_rx,
            thread: Some(thread),
            stop,
        }
    }

    /// Send a command to the backend, logging any SendError to stderr.
    /// `ctx` identifies the operation in the log line so a dropped
    /// command surfaces as more than a silent no-op.
    fn try_send_or_warn(&self, cmd: BackendCommand, ctx: &str) {
        if let Err(e) = self.cmd_tx.send(cmd) {
            eprintln!("backend send dropped ({}): {}", ctx, e);
        }
    }

    pub fn refresh(&self) {
        self.try_send_or_warn(BackendCommand::Refresh, "refresh");
    }

    pub fn update_task(&self, id: String, fields: HashMap<String, serde_json::Value>) {
        self.try_send_or_warn(BackendCommand::UpdateTask { id, fields }, "update_task");
    }

    pub fn delete_task(&self, id: String) {
        self.try_send_or_warn(BackendCommand::DeleteTask { id }, "delete_task");
    }

    pub fn create_plan_task(
        &self,
        project: String,
        repo_url: String,
        name: String,
        description: String,
        status: String,
        parent_task_id: Option<String>,
        worktree_mode: Option<String>,
    ) {
        self.try_send_or_warn(
            BackendCommand::CreatePlanTask {
                project,
                repo_url,
                name,
                description,
                status,
                parent_task_id,
                worktree_mode,
            },
            "create_plan_task",
        );
    }

    pub fn update_plan_task(&self, id: String, fields: HashMap<String, serde_json::Value>) {
        self.try_send_or_warn(
            BackendCommand::UpdatePlanTask { id, fields },
            "update_plan_task",
        );
    }

    pub fn delete_plan_task(&self, id: String) {
        self.try_send_or_warn(BackendCommand::DeletePlanTask { id }, "delete_plan_task");
    }

    pub fn refresh_plan_tasks(&self) {
        self.try_send_or_warn(BackendCommand::RefreshPlanTasks, "refresh_plan_tasks");
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // SendError here is expected if the receiver thread already
        // exited (clean shutdown race); skip the warn helper.
        let _ = self.cmd_tx.send(BackendCommand::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn backend_loop(
    client: ApiClient,
    cmd_rx: mpsc::Receiver<BackendCommand>,
    event_tx: mpsc::Sender<BackendEvent>,
    cache: Arc<Mutex<TaskCache>>,
    stop: Arc<AtomicBool>,
    _gcp_project: &str,
    _gcp_zone: &str,
) {
    // The feed thread performs the initial snapshot; this thread only
    // re-emits and executes writes. After a write, the feed delivers the
    // committed row (the trigger's NOTIFY releases the long poll at once);
    // only legacy mode (no feed) needs an explicit refetch.
    let after_write = |client: &ApiClient| {
        let legacy = lock(&cache).cursor().is_none();
        if legacy {
            do_legacy_refresh(client, &event_tx, &cache);
        }
    };

    loop {
        match cmd_rx.recv_timeout(HEARTBEAT) {
            Ok(BackendCommand::Shutdown) => break,
            Ok(BackendCommand::Refresh) => {
                do_snapshot(&client, &event_tx, &cache);
            }
            Ok(BackendCommand::UpdateTask { id, fields }) => {
                match client.update_task(&id, &fields) {
                    Ok(_) => after_write(&client),
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::DeleteTask { id }) => {
                match client.delete_task(&id) {
                    Ok(_) => after_write(&client),
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::CreatePlanTask {
                project,
                repo_url,
                name,
                description,
                status,
                parent_task_id,
                worktree_mode,
            }) => {
                do_create_plan_task(
                    &client,
                    &event_tx,
                    &project,
                    &repo_url,
                    &name,
                    &description,
                    &status,
                    parent_task_id.as_deref(),
                    worktree_mode.as_deref(),
                );
            }
            Ok(BackendCommand::UpdatePlanTask { id, fields }) => {
                match client.update_task(&id, &fields) {
                    Ok(task) => {
                        let _ = event_tx.send(BackendEvent::PlanTaskUpdated(task));
                        after_write(&client);
                    }
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::DeletePlanTask { id }) => {
                match client.delete_task(&id) {
                    Ok(_) => {
                        let _ = event_tx.send(BackendEvent::PlanTaskDeleted(id));
                        after_write(&client);
                    }
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::RefreshPlanTasks) => {
                // The feed keeps the cache current; answer from it unless
                // nothing has loaded yet (then a snapshot is the fastest way
                // to get anything on screen).
                if lock(&cache).is_loaded() {
                    emit_plan_tasks(&event_tx, &cache);
                } else {
                    do_snapshot(&client, &event_tx, &cache);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // Pre-feed, every 5 s tick re-fetched and re-emitted both
                // lists; the app's reconcile/sweep logic runs off those
                // events, so keep the cadence — from the cache, no network.
                if lock(&cache).is_loaded() {
                    emit_tasks(&event_tx, &cache);
                    emit_plan_tasks(&event_tx, &cache);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn lock(cache: &Arc<Mutex<TaskCache>>) -> std::sync::MutexGuard<'_, TaskCache> {
    cache.lock().unwrap_or_else(|p| p.into_inner())
}

fn emit_tasks(event_tx: &mpsc::Sender<BackendEvent>, cache: &Arc<Mutex<TaskCache>>) {
    let tasks = lock(cache).materialize();
    let _ = event_tx.send(BackendEvent::TasksUpdated(tasks));
}

fn emit_plan_tasks(event_tx: &mpsc::Sender<BackendEvent>, cache: &Arc<Mutex<TaskCache>>) {
    // Planning shows the tasks that have a project; the API has no
    // "has project" filter, so filter client-side (as before).
    let plan_tasks: Vec<Task> = lock(cache)
        .materialize()
        .into_iter()
        .filter(|t| t.project.is_some())
        .collect();
    let _ = event_tx.send(BackendEvent::PlanTasksUpdated(plan_tasks));
}

/// Forced full snapshot through the feed (falls back to the legacy list
/// against an old API). Used for the explicit refresh keys.
fn do_snapshot(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    cache: &Arc<Mutex<TaskCache>>,
) {
    match client.task_changes(None, 0.0) {
        Ok(page) => {
            let cursor = ChangeCursor {
                epoch: page.epoch,
                seq: page.cursor,
            };
            lock(cache).replace_all(page.tasks.unwrap_or_default(), Some(cursor));
            emit_tasks(event_tx, cache);
            emit_plan_tasks(event_tx, cache);
        }
        Err(e) if is_unsupported(&e) => {
            do_legacy_refresh(client, event_tx, cache);
        }
        Err(e) => {
            let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
        }
    }
}

/// Pre-feed behaviour: download the whole list and replace the cache.
fn do_legacy_refresh(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    cache: &Arc<Mutex<TaskCache>>,
) -> bool {
    match client.list_tasks(None) {
        Ok(tasks) => {
            lock(cache).replace_all(tasks, None);
            emit_tasks(event_tx, cache);
            emit_plan_tasks(event_tx, cache);
            true
        }
        Err(e) => {
            let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
            false
        }
    }
}

/// Sleep in short slices so shutdown is prompt.
fn sleep_unless_stopped(stop: &AtomicBool, total: Duration) {
    let deadline = Instant::now() + total;
    while !stop.load(Ordering::SeqCst) {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        thread::sleep(left.min(Duration::from_millis(250)));
    }
}

/// The incremental subscription: snapshot once, then long-poll change pages
/// and apply them under the compare-and-swap rule (`TaskCache::apply`).
fn feed_loop(
    client: ApiClient,
    event_tx: mpsc::Sender<BackendEvent>,
    cache: Arc<Mutex<TaskCache>>,
    stop: Arc<AtomicBool>,
) {
    let mut was_connected = false;
    let mut backoff = Duration::from_secs(1);
    let mut legacy_until: Option<Instant> = None;

    let set_connected = |connected: bool, was_connected: &mut bool| {
        if connected && !*was_connected {
            let _ = event_tx.send(BackendEvent::Connected);
        } else if !connected && *was_connected {
            let _ = event_tx.send(BackendEvent::Disconnected);
        }
        *was_connected = connected;
    };

    while !stop.load(Ordering::SeqCst) {
        if let Some(until) = legacy_until {
            if Instant::now() < until {
                let ok = do_legacy_refresh(&client, &event_tx, &cache);
                set_connected(ok, &mut was_connected);
                sleep_unless_stopped(&stop, LEGACY_POLL);
                continue;
            }
            legacy_until = None;
        }

        let issued = lock(&cache).issue_point();
        let wait = if issued.cursor.is_some() {
            LONG_POLL_WAIT_SECS
        } else {
            0.0
        };
        match client.task_changes(issued.cursor.as_ref(), wait) {
            Ok(page) => {
                backoff = Duration::from_secs(1);
                set_connected(true, &mut was_connected);
                let outcome = lock(&cache).apply(&issued, page);
                if outcome == (ApplyOutcome::Applied { changed: true }) {
                    emit_tasks(&event_tx, &cache);
                    emit_plan_tasks(&event_tx, &cache);
                }
                // A cut page (`more`) or a discarded one needs no special
                // casing: the next poll starts from the cache's cursor and
                // the server answers at once while changes are pending.
            }
            Err(e) if is_unsupported(&e) => {
                // Old API without the feed: behave exactly as before and
                // re-probe later so a rolling deploy picks the feed up.
                legacy_until = Some(Instant::now() + LEGACY_REPROBE);
            }
            Err(e) => {
                set_connected(false, &mut was_connected);
                let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                // The cursor is retained: the next successful poll catches up
                // from it (or resnapshots if the server no longer has it).
                sleep_unless_stopped(&stop, backoff);
                backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
            }
        }
    }
}

/// Get the Claude project path for a directory.
/// Claude encodes: '/' and '.' both become '-', leading dash kept.
/// Create a planning task, preserving parent binding and worktree intent.
fn do_create_plan_task(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    project: &str,
    repo_url: &str,
    name: &str,
    description: &str,
    status: &str,
    parent_task_id: Option<&str>,
    worktree_mode: Option<&str>,
) {
    let body = TaskCreateBody {
        repo_url: repo_url.to_string(),
        repo_branch: "main".to_string(),
        name: Some(name.to_string()),
        prompt: None,
        priority: 0,
        status: Some(status.to_string()),
        project: Some(project.to_string()),
        slug: None, // auto-generated by API
        description: Some(description.to_string()),
        difficulty: None,
        depends: None,
        source: None,
        is_cloud: Some(false),
        parent_task_id: parent_task_id.map(str::to_string),
        worktree_mode: worktree_mode.map(str::to_string),
        initiative_id: None,
        wip_branch: None,
        metadata: None,
    };

    match client.create_task(&body) {
        Ok(task) => {
            let _ = event_tx.send(BackendEvent::PlanTaskCreated(task));
        }
        Err(e) => {
            let _ = event_tx.send(BackendEvent::ApiError(format!(
                "Create plan task: {}",
                e
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_send_or_warn_does_not_panic_when_receiver_dropped() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
        let (_event_tx, event_rx) = mpsc::channel::<BackendEvent>();
        drop(cmd_rx);
        let handle = BackendHandle {
            cmd_tx,
            event_rx,
            thread: None,
            stop: Arc::new(AtomicBool::new(false)),
        };
        handle.refresh();
        handle.delete_task("nope".to_string());
        handle.refresh_plan_tasks();
    }

    fn task(id: &str, status: &str, created_at: &str) -> Task {
        serde_json::from_value(serde_json::json!({
            "id": id, "created_at": created_at, "repo_url": "r", "repo_branch": "main",
            "name": id, "prompt": null, "status": status, "worker_vm": null,
            "worker_zone": null, "blocked_at": null, "session_id": null,
            "wip_branch": null, "project": "p",
        }))
        .expect("task json")
    }

    fn page(epoch: &str, cursor: i64, changes: Vec<(&str, &str, Option<Task>)>) -> TaskChangesPage {
        TaskChangesPage {
            epoch: epoch.into(),
            cursor,
            reset: false,
            tasks: None,
            changes: changes
                .into_iter()
                .enumerate()
                .map(|(i, (id, op, task))| crate::api::TaskChange {
                    seq: cursor - (2 - i as i64),
                    task_id: id.into(),
                    op: op.into(),
                    task,
                })
                .collect(),
            more: false,
        }
    }

    fn snapshot(epoch: &str, cursor: i64, tasks: Vec<Task>) -> TaskChangesPage {
        TaskChangesPage {
            epoch: epoch.into(),
            cursor,
            reset: true,
            tasks: Some(tasks),
            changes: vec![],
            more: false,
        }
    }

    #[test]
    fn cache_applies_snapshot_then_changes_in_server_order() {
        let mut cache = TaskCache::default();
        assert!(!cache.is_loaded());
        let issued = cache.issue_point();
        let out = cache.apply(
            &issued,
            snapshot("e", 10, vec![task("a", "done", "2"), task("b", "running", "1")]),
        );
        assert_eq!(out, ApplyOutcome::Applied { changed: true });
        assert!(cache.is_loaded());
        assert_eq!(cache.cursor().unwrap().seq, 10);
        let ids: Vec<_> = cache.materialize().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["b", "a"], "running sorts before done");

        // Upsert + remove; cursor advances.
        let issued = cache.issue_point();
        let out = cache.apply(
            &issued,
            page("e", 12, vec![("a", "remove", None), ("c", "upsert", Some(task("c", "blocked", "3")))]),
        );
        assert_eq!(out, ApplyOutcome::Applied { changed: true });
        let ids: Vec<_> = cache.materialize().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["c", "b"]);
        assert_eq!(cache.cursor().unwrap().seq, 12);

        // Idle page: same cursor, nothing changed, nothing to emit.
        let issued = cache.issue_point();
        let out = cache.apply(&issued, page("e", 12, vec![]));
        assert_eq!(out, ApplyOutcome::Applied { changed: false });

        // Removing something we never had is not a change either.
        let issued = cache.issue_point();
        let out = cache.apply(&issued, page("e", 13, vec![("zzz", "remove", None)]));
        assert_eq!(out, ApplyOutcome::Applied { changed: false });
        assert_eq!(cache.cursor().unwrap().seq, 13);
    }

    /// A page issued before a forced snapshot (or any wholesale replacement)
    /// must not land on top of the newer contents.
    #[test]
    fn cache_discards_page_issued_against_older_state() {
        let mut cache = TaskCache::default();
        let issued0 = cache.issue_point();
        cache.apply(&issued0, snapshot("e", 10, vec![task("a", "backlog", "1")]));

        // Feed thread issues a poll at cursor 10 ...
        let issued = cache.issue_point();
        // ... meanwhile the command thread forces a fresh snapshot at 15.
        cache.replace_all(
            vec![task("a", "running", "1")],
            Some(ChangeCursor { epoch: "e".into(), seq: 15 }),
        );
        // The stale page (state as of seq 12) arrives: discarded.
        let out = cache.apply(&issued, page("e", 12, vec![("a", "upsert", Some(task("a", "blocked", "1")))]));
        assert_eq!(out, ApplyOutcome::Discarded);
        assert_eq!(cache.materialize()[0].status, "running");
        assert_eq!(cache.cursor().unwrap().seq, 15);

        // Same cursor but a different generation (legacy list replaced the
        // contents and dropped the cursor) is also discarded.
        let issued = cache.issue_point();
        cache.replace_all(vec![], None);
        let out = cache.apply(&issued, page("e", 16, vec![]));
        assert_eq!(out, ApplyOutcome::Discarded);
        assert!(cache.cursor().is_none());
    }

    /// Re-applying a page (a retried request after a lost reply) is a no-op
    /// on contents, and a server reset replaces everything.
    #[test]
    fn cache_reset_replaces_and_reapply_is_idempotent() {
        let mut cache = TaskCache::default();
        let issued = cache.issue_point();
        cache.apply(&issued, snapshot("e1", 5, vec![task("a", "backlog", "1"), task("b", "backlog", "2")]));
        let issued = cache.issue_point();
        let p = page("e1", 7, vec![("b", "upsert", Some(task("b", "done", "2")))]);
        assert_eq!(cache.apply(&issued, p.clone()), ApplyOutcome::Applied { changed: true });
        let before = cache.materialize().len();
        // A duplicate reply for the same request (issued at cursor 5) arrives
        // after the cache moved to 7: the CAS rejects it.
        assert_eq!(cache.apply(&issued, p), ApplyOutcome::Discarded);
        assert_eq!(cache.materialize().len(), before);
        assert_eq!(cache.cursor().unwrap().seq, 7);

        // Server lineage changed: the reset snapshot wins wholesale.
        let issued = cache.issue_point();
        let out = cache.apply(&issued, snapshot("e2", 1, vec![task("z", "backlog", "9")]));
        assert_eq!(out, ApplyOutcome::Applied { changed: true });
        let ids: Vec<_> = cache.materialize().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["z"]);
        assert_eq!(cache.cursor().unwrap(), &ChangeCursor { epoch: "e2".into(), seq: 1 });
    }
}
