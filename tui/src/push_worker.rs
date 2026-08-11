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
        crate::daemon_launch::operator_token(),
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
        crate::daemon_launch::operator_token(),
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
        crate::daemon_launch::operator_token(),
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
