//! Background push fanout to host daemons.
//!
//! Pre-this-module the per-host daemon RPC fanout
//! (`App::push_state_to_daemon` and its three helpers) ran on the
//! main thread. With local-only setups that was sub-ms per call;
//! with SSH-tunneled remote hosts it added ~500ms-1s of RTT per
//! push, so every backend-tick reconcile (every 5s) and every
//! state mutation blocked keystroke handling for that long.
//!
//! `PushWorker` owns a dedicated thread that drains an mpsc
//! channel of pending pushes, coalesces bursts (latest of each
//! kind wins), de-dupes against the last-successfully-pushed
//! payload per (host, kind), and does the RPC fanout. Main
//! thread fires owned snapshots and returns immediately.
//!
//! Reachability cache calls (`mark_push_success` /
//! `mark_push_failure`) stay where they were — just moved to the
//! worker thread. The 5s `rpc_round_trip` read/write timeout
//! still applies as a safety net so a dead remote doesn't make
//! the worker queue back up forever.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use cm_daemon::host_id::HostId;
use cm_daemon::workflow::toml_schema::Workflow;

use crate::host_pool::HostPool;

/// Owned version of `client_session::TuiSessionSnapshotPush`
/// (which is borrowed). Built on the main thread and sent to the
/// worker.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TuiSessionRow {
    pub uid: String,
    pub task_id: Option<String>,
    pub label: Option<String>,
    pub session_type: Option<String>,
    pub hidden: bool,
    pub workflow_run_id: Option<String>,
    pub workflow_role: Option<String>,
    pub global_perms: bool,
    /// 5d: workspace this session lives in + that workspace's checkout,
    /// so the daemon's `list_sessions` can report the same
    /// `workspace_id` / `worktree_path` for TUI-owned rows that it
    /// already reports for daemon-owned ones.
    pub workspace_id: Option<String>,
    pub worktree_path: Option<String>,
}

enum PushCommand {
    TaskTree {
        tasks: Vec<(String, Option<String>, Option<String>)>,
        workspaces: Vec<(String, Option<String>)>,
        hosts: Vec<HostId>,
    },
    TuiSessions {
        per_host: HashMap<HostId, Vec<TuiSessionRow>>,
    },
    WorkflowDefs {
        workflows: HashMap<String, Workflow>,
        hosts: Vec<HostId>,
    },
    /// Seamless-restart audit gap 3: drop every cached payload
    /// hash for `host` so the NEXT push of each kind re-sends
    /// unconditionally. Fired by the App when that host's
    /// `manifest.watch` stream (re)establishes — the daemon
    /// behind the socket may be a NEW process (re-exec) whose
    /// TUI-pushed state (`task_tree` / `tui_sessions` /
    /// `workflow_definitions`) was born empty; without this the
    /// de-dupe would suppress the re-prime forever on an idle
    /// TUI (identical payload → identical hash → skip).
    InvalidateHost { host: HostId },
    Shutdown,
}

pub struct PushWorker {
    cmd_tx: mpsc::Sender<PushCommand>,
    thread: Option<JoinHandle<()>>,
}

impl PushWorker {
    pub fn spawn(host_pool: Arc<HostPool>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("cm-tui-push-worker".to_string())
            .spawn(move || worker_loop(host_pool, cmd_rx))
            .expect("spawn push worker thread");
        PushWorker {
            cmd_tx,
            thread: Some(thread),
        }
    }

    pub fn push_task_tree(
        &self,
        tasks: Vec<(String, Option<String>, Option<String>)>,
        workspaces: Vec<(String, Option<String>)>,
        hosts: Vec<HostId>,
    ) {
        let _ = self.cmd_tx.send(PushCommand::TaskTree {
            tasks,
            workspaces,
            hosts,
        });
    }

    pub fn push_tui_sessions(
        &self,
        per_host: HashMap<HostId, Vec<TuiSessionRow>>,
    ) {
        let _ = self.cmd_tx.send(PushCommand::TuiSessions { per_host });
    }

    pub fn push_workflow_defs(
        &self,
        workflows: HashMap<String, Workflow>,
        hosts: Vec<HostId>,
    ) {
        let _ = self.cmd_tx.send(PushCommand::WorkflowDefs {
            workflows,
            hosts,
        });
    }

    /// Seamless-restart audit gap 3: invalidate the de-dupe cache
    /// for one host. The next push of EVERY kind to that host
    /// re-sends even when the payload hash matches the last
    /// successful push. Call when there's reason to believe the
    /// daemon behind the host's socket no longer holds the pushed
    /// state (today's one caller: the App's `manifest.watch`
    /// stream-established handler — a daemon re-exec kills the
    /// stream, so a reconnect marks a possibly-fresh daemon).
    ///
    /// mpsc ordering guarantees the invalidation is processed
    /// before any push queued after it from the same thread, and
    /// [`execute`] applies invalidations before the pushes of the
    /// same drained batch — so `invalidate_host` followed by a
    /// re-push never loses the race against coalescing.
    pub fn invalidate_host(&self, host: HostId) {
        let _ = self.cmd_tx.send(PushCommand::InvalidateHost { host });
    }

    pub fn shutdown(&mut self) {
        let _ = self.cmd_tx.send(PushCommand::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for PushWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Coalescing buffer: only the most-recent command of each kind
/// survives a drain. Bursts of state mutations collapse into a
/// single fanout pass.
#[derive(Default)]
struct Pending {
    task_tree: Option<(
        Vec<(String, Option<String>, Option<String>)>,
        Vec<(String, Option<String>)>,
        Vec<HostId>,
    )>,
    tui_sessions: Option<HashMap<HostId, Vec<TuiSessionRow>>>,
    workflow_defs: Option<(HashMap<String, Workflow>, Vec<HostId>)>,
    /// Hosts whose hash cache must be dropped BEFORE this batch's
    /// pushes execute. Accumulated (deduped), never
    /// latest-wins-coalesced — two reconnects in one drain window
    /// still clear both hosts.
    invalidate_hosts: Vec<HostId>,
    shutdown: bool,
}

fn apply(pending: &mut Pending, cmd: PushCommand) {
    match cmd {
        PushCommand::TaskTree {
            tasks,
            workspaces,
            hosts,
        } => {
            pending.task_tree = Some((tasks, workspaces, hosts));
        }
        PushCommand::TuiSessions { per_host } => {
            pending.tui_sessions = Some(per_host);
        }
        PushCommand::WorkflowDefs { workflows, hosts } => {
            pending.workflow_defs = Some((workflows, hosts));
        }
        PushCommand::InvalidateHost { host } => {
            if !pending.invalidate_hosts.contains(&host) {
                pending.invalidate_hosts.push(host);
            }
        }
        PushCommand::Shutdown => {
            pending.shutdown = true;
        }
    }
}

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
enum PushKind {
    TaskTree,
    TuiSessions,
    WorkflowDefs,
}

fn worker_loop(
    host_pool: Arc<HostPool>,
    cmd_rx: mpsc::Receiver<PushCommand>,
) {
    // (host, kind) → hash of the last payload we successfully
    // pushed. Worker-thread-local; the worker is single-threaded
    // so no Mutex needed.
    let mut last_hashes: HashMap<(HostId, PushKind), u64> = HashMap::new();

    loop {
        let first = match cmd_rx.recv() {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut pending = Pending::default();
        apply(&mut pending, first);
        // Drain anything else already queued and keep only the
        // latest of each kind — the daemon only cares about the
        // most recent snapshot, intermediates are wasted work.
        while let Ok(cmd) = cmd_rx.try_recv() {
            apply(&mut pending, cmd);
        }
        if pending.shutdown {
            return;
        }
        execute(&host_pool, &mut last_hashes, pending);
    }
}

fn execute(
    host_pool: &HostPool,
    last_hashes: &mut HashMap<(HostId, PushKind), u64>,
    pending: Pending,
) {
    // Invalidations first: a reconnect signal queued alongside (or
    // before) this batch's pushes must clear the cache before the
    // hash checks run, so the re-prime push in the same batch
    // actually fires.
    for host in &pending.invalidate_hosts {
        last_hashes.retain(|(h, _kind), _| h != host);
    }
    if let Some((tasks, workspaces, hosts)) = pending.task_tree {
        for host_id in &hosts {
            push_task_tree(host_pool, last_hashes, host_id, &tasks, &workspaces);
        }
    }
    if let Some(per_host) = pending.tui_sessions {
        for (host_id, sessions) in &per_host {
            push_tui_sessions(host_pool, last_hashes, host_id, sessions);
        }
    }
    if let Some((workflows, hosts)) = pending.workflow_defs {
        for host_id in &hosts {
            push_workflow_defs(host_pool, last_hashes, host_id, &workflows);
        }
    }
}

/// Hash a payload by serializing it to JSON bytes and hashing
/// those. Works uniformly for any `Serialize` shape including
/// the HashMap-of-workflows payload. Within a single TUI run the
/// iteration order of an unchanged HashMap is stable, so a
/// no-change repush hashes identically.
fn hash_payload<T: serde::Serialize>(value: &T) -> u64 {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    hasher.finish()
}

fn push_task_tree(
    host_pool: &HostPool,
    last_hashes: &mut HashMap<(HostId, PushKind), u64>,
    host_id: &HostId,
    tasks: &[(String, Option<String>, Option<String>)],
    workspaces: &[(String, Option<String>)],
) {
    if host_pool.should_skip_for_push(host_id, Instant::now()) {
        return;
    }
    let hash = hash_payload(&(tasks, workspaces));
    let key = (host_id.clone(), PushKind::TaskTree);
    if last_hashes.get(&key) == Some(&hash) {
        return;
    }
    let daemon_socket = match host_pool.for_host(host_id) {
        Ok(h) => match h.socket_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "cm-tui: task.update_tree to host {} skipped: no socket path",
                    host_id.as_str(),
                );
                return;
            }
        },
        Err(e) => {
            host_pool.mark_push_failure(host_id, Instant::now());
            eprintln!(
                "cm-tui: task.update_tree to host {} skipped: {}",
                host_id.as_str(),
                e,
            );
            return;
        }
    };
    match crate::client_session::rpc_task_update_tree(
        &daemon_socket,
        &host_pool.operator_token_for(host_id),
        tasks,
        workspaces,
    ) {
        Ok(()) => {
            host_pool.mark_push_success(host_id);
            last_hashes.insert(key, hash);
        }
        Err(e) => {
            host_pool.mark_push_failure(host_id, Instant::now());
            eprintln!(
                "cm-tui: task.update_tree to host {} failed: {}",
                host_id.as_str(),
                e,
            );
        }
    }
}

fn push_tui_sessions(
    host_pool: &HostPool,
    last_hashes: &mut HashMap<(HostId, PushKind), u64>,
    host_id: &HostId,
    sessions: &[TuiSessionRow],
) {
    if host_pool.should_skip_for_push(host_id, Instant::now()) {
        return;
    }
    let hash = hash_payload(&sessions);
    let key = (host_id.clone(), PushKind::TuiSessions);
    if last_hashes.get(&key) == Some(&hash) {
        return;
    }
    let daemon_socket = match host_pool.for_host(host_id) {
        Ok(h) => match h.socket_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "cm-tui: tui.update_sessions_snapshot to host {} skipped: no socket path",
                    host_id.as_str(),
                );
                return;
            }
        },
        Err(e) => {
            host_pool.mark_push_failure(host_id, Instant::now());
            eprintln!(
                "cm-tui: tui.update_sessions_snapshot to host {} skipped: {}",
                host_id.as_str(),
                e,
            );
            return;
        }
    };
    let borrowed: Vec<crate::client_session::TuiSessionSnapshotPush<'_>> = sessions
        .iter()
        .map(|s| crate::client_session::TuiSessionSnapshotPush {
            uid: s.uid.as_str(),
            task_id: s.task_id.as_deref(),
            label: s.label.as_deref(),
            session_type: s.session_type.as_deref(),
            hidden: s.hidden,
            workflow_run_id: s.workflow_run_id.as_deref(),
            workflow_role: s.workflow_role.as_deref(),
            global_perms: s.global_perms,
            workspace_id: s.workspace_id.as_deref(),
            worktree_path: s.worktree_path.as_deref(),
        })
        .collect();
    match crate::client_session::rpc_tui_update_sessions_snapshot(
        &daemon_socket,
        &host_pool.operator_token_for(host_id),
        &borrowed,
    ) {
        Ok(()) => {
            host_pool.mark_push_success(host_id);
            last_hashes.insert(key, hash);
        }
        Err(e) => {
            host_pool.mark_push_failure(host_id, Instant::now());
            eprintln!(
                "cm-tui: tui.update_sessions_snapshot to host {} failed: {}",
                host_id.as_str(),
                e,
            );
        }
    }
}

fn push_workflow_defs(
    host_pool: &HostPool,
    last_hashes: &mut HashMap<(HostId, PushKind), u64>,
    host_id: &HostId,
    workflows: &HashMap<String, Workflow>,
) {
    if host_pool.should_skip_for_push(host_id, Instant::now()) {
        return;
    }
    let hash = hash_payload(workflows);
    let key = (host_id.clone(), PushKind::WorkflowDefs);
    if last_hashes.get(&key) == Some(&hash) {
        return;
    }
    let daemon_socket = match host_pool.for_host(host_id) {
        Ok(h) => match h.socket_path() {
            Some(p) => p,
            None => {
                eprintln!(
                    "cm-tui: workflow.update_definitions to host {} skipped: no socket path",
                    host_id.as_str(),
                );
                return;
            }
        },
        Err(e) => {
            host_pool.mark_push_failure(host_id, Instant::now());
            eprintln!(
                "cm-tui: workflow.update_definitions to host {} skipped: {}",
                host_id.as_str(),
                e,
            );
            return;
        }
    };
    match crate::client_session::rpc_workflow_update_definitions(
        &daemon_socket,
        &host_pool.operator_token_for(host_id),
        workflows,
    ) {
        Ok(()) => {
            host_pool.mark_push_success(host_id);
            last_hashes.insert(key, hash);
        }
        Err(e) => {
            host_pool.mark_push_failure(host_id, Instant::now());
            eprintln!(
                "cm-tui: workflow.update_definitions to host {} failed: {}",
                host_id.as_str(),
                e,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    use cm_daemon::state::DaemonState;

    /// In-process daemon on a tempdir socket running the REAL
    /// dispatcher (mirrors `client_session::tests::
    /// start_test_daemon`, minus the workspace seeding the push
    /// RPCs don't need). Push RPCs are plain request/response —
    /// stream outcomes are unreachable here and dropped.
    fn start_push_test_daemon() -> (
        std::path::PathBuf,
        Arc<Mutex<DaemonState>>,
        Arc<AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().to_path_buf();
        // Keep the tempdir alive for the test binary's lifetime
        // (same idiom as start_test_daemon).
        std::mem::forget(dir);
        let socket_path = dir_path.join("push-test-daemon.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind test socket");
        listener.set_nonblocking(true).expect("nonblocking listener");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let state_for_thread = state.clone();
        let stop_for_thread = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let state = state_for_thread.clone();
                        std::thread::spawn(move || {
                            let _ = stream.set_read_timeout(Some(
                                Duration::from_secs(2),
                            ));
                            let _ = stream.set_write_timeout(Some(
                                Duration::from_secs(2),
                            ));
                            let req = match cm_daemon::control::wire::read_request(
                                &mut stream,
                            ) {
                                Ok(Some(r)) => r,
                                _ => return,
                            };
                            if let cm_daemon::control::dispatch::DispatchOutcome::Done(
                                resp,
                            ) = cm_daemon::control::dispatch::dispatch_request(
                                &state, &req,
                            ) {
                                let _ = cm_daemon::control::wire::write_response(
                                    &mut stream,
                                    &resp,
                                );
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        (socket_path, state, stop, handle)
    }

    fn stop_push_test_daemon(
        socket_path: &std::path::Path,
        stop: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    ) {
        stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(socket_path);
        let _ = handle.join();
    }

    fn local_pool_for(socket: &std::path::Path) -> Arc<HostPool> {
        let hosts = crate::hosts::HostsConfig {
            hosts: vec![crate::hosts::HostConfig {
                id: HostId::local(),
                transport: crate::hosts::HostTransport::Unix {
                    socket: socket.to_path_buf(),
                },
                default: true,
                operator_token: None,
                operator_token_file: None,
            }],
        };
        Arc::new(HostPool::from_config(&hosts).expect("pool"))
    }

    fn sample_tasks() -> Vec<(String, Option<String>, Option<String>)> {
        vec![("task-reprime".to_string(), None, None)]
    }

    fn wait_for<F: Fn() -> bool>(cond: F, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for: {}",
                what,
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Gap 3 (a): `invalidate_host` clears the payload-hash cache,
    /// so the NEXT push of a byte-identical payload actually fires
    /// its RPC. The daemon-side clear between the two pushes plays
    /// the re-exec'd daemon whose `task_tree` was born empty —
    /// pre-fix the second push was de-dupe-skipped and the state
    /// stayed empty forever.
    #[test]
    fn invalidate_host_forces_resend_of_identical_payload() {
        let (socket, state, stop, handle) = start_push_test_daemon();
        let pool = local_pool_for(&socket);
        let mut worker = PushWorker::spawn(pool);

        worker.push_task_tree(sample_tasks(), vec![], vec![HostId::local()]);
        wait_for(
            || state.lock().unwrap().task_tree.contains_key("task-reprime"),
            "first push to land in DaemonState.task_tree",
        );

        // The "daemon re-exec": same socket, state born empty.
        state.lock().unwrap().task_tree.clear();

        worker.invalidate_host(HostId::local());
        worker.push_task_tree(sample_tasks(), vec![], vec![HostId::local()]);
        wait_for(
            || state.lock().unwrap().task_tree.contains_key("task-reprime"),
            "identical re-push to land after invalidate_host \
             (hash cache must have been cleared)",
        );

        worker.shutdown();
        stop_push_test_daemon(&socket, stop, handle);
    }

    /// Gap 3 (b) regression pin: WITHOUT an invalidation, an
    /// identical re-push stays de-duped (the cache still works —
    /// the fix must not turn every push into a re-send).
    /// Determinism: a tui_sessions push queued AFTER the deduped
    /// task-tree push proves the worker drained past it — the
    /// worker is single-threaded and `execute` handles task_tree
    /// before tui_sessions within a batch, so once the session row
    /// is visible daemon-side, the task-tree skip already happened.
    #[test]
    fn identical_push_without_invalidation_stays_deduped() {
        let (socket, state, stop, handle) = start_push_test_daemon();
        let pool = local_pool_for(&socket);
        let mut worker = PushWorker::spawn(pool);

        worker.push_task_tree(sample_tasks(), vec![], vec![HostId::local()]);
        wait_for(
            || state.lock().unwrap().task_tree.contains_key("task-reprime"),
            "first push to land in DaemonState.task_tree",
        );

        // Clear daemon-side so a (wrong) re-send would be visible.
        state.lock().unwrap().task_tree.clear();

        // Identical payload, no invalidation → must be skipped.
        worker.push_task_tree(sample_tasks(), vec![], vec![HostId::local()]);

        // Sequencing probe: a later push of another kind.
        let mut per_host = HashMap::new();
        per_host.insert(
            HostId::local(),
            vec![TuiSessionRow {
                uid: "ts-dedupe-probe".to_string(),
                task_id: None,
                label: Some("probe".to_string()),
                session_type: Some("bash".to_string()),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
                global_perms: false,
                workspace_id: None,
                worktree_path: None,
            }],
        );
        worker.push_tui_sessions(per_host);
        wait_for(
            || {
                state
                    .lock()
                    .unwrap()
                    .tui_sessions
                    .contains_key("ts-dedupe-probe")
            },
            "probe tui_sessions push to land (worker drained past \
             the deduped task-tree push)",
        );
        assert!(
            state.lock().unwrap().task_tree.is_empty(),
            "identical task-tree re-push must have been de-duped \
             (no invalidation was requested) — de-dupe regression",
        );

        worker.shutdown();
        stop_push_test_daemon(&socket, stop, handle);
    }
}
