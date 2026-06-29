//! Off-thread remote-attach worker.
//!
//! The deferred-remote-reattach drain used to call
//! `app::try_attach_via_daemon_with_deps` ON THE MAIN THREAD — a synchronous
//! ~1-2s round-trip over the (possibly slow/flaky) tunnel. When a burst of
//! remote sessions surfaced on first connect (a continuous orchestrator + its
//! just-spawned agents), the drain attached them and froze the UI for seconds.
//!
//! This worker runs the attach on its OWN thread. The main loop dispatches an
//! [`AttachRequest`] (non-blocking) and later drains the ready [`AttachResult`]
//! via the result channel, binding the `Session` into its slot with zero
//! blocking I/O on the main thread.

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::host_pool::HostPool;
use crate::session::Session;

/// What the worker needs to attach a single remote session.
pub struct AttachRequest {
    pub ws_id: String,
    pub entry: cm_daemon::manifest::ManifestEntry,
    pub worktree: PathBuf,
    pub cols: u16,
    pub rows: u16,
    /// Reconnect attempt count, echoed back so the result handler can apply the
    /// retry/cap policy without re-deriving it.
    pub attempts: u32,
}

/// The outcome of an [`AttachRequest`]. `session` is `None` when the attach
/// failed (session gone / host unreachable) — the main loop then re-queues for
/// retry (or, past the cap, marks the slot exited).
pub struct AttachResult {
    pub ws_id: String,
    pub entry: cm_daemon::manifest::ManifestEntry,
    pub attempts: u32,
    pub session: Option<Session>,
}

pub struct AttachWorker {
    cmd_tx: mpsc::Sender<AttachRequest>,
    /// Drained by the main loop (`App::drain_attach_results`).
    pub result_rx: mpsc::Receiver<AttachResult>,
    _thread: JoinHandle<()>,
}

impl AttachWorker {
    pub fn spawn(host_pool: Arc<HostPool>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AttachRequest>();
        let (result_tx, result_rx) = mpsc::channel::<AttachResult>();
        let thread = std::thread::Builder::new()
            .name("cm-tui-attach-worker".into())
            .spawn(move || {
                while let Ok(req) = cmd_rx.recv() {
                    // Blocking is fine HERE — off the main thread. The attach
                    // RPCs route through the entry's own host socket.
                    let session = crate::app::try_attach_via_daemon_with_deps(
                        &host_pool,
                        &req.entry.uid,
                        &req.ws_id,
                        &req.worktree,
                        &req.entry.session_type,
                        &req.entry.label,
                        req.cols,
                        req.rows,
                        req.entry.task_id.as_deref(),
                        req.entry.workflow_run_id.as_deref(),
                        req.entry.workflow_role.as_deref(),
                        &req.entry.host_id,
                        // Transcript binding survived on the remote daemon —
                        // don't push a wrong-for-remote local path over it.
                        None,
                    )
                    .ok();
                    if result_tx
                        .send(AttachResult {
                            ws_id: req.ws_id,
                            entry: req.entry,
                            attempts: req.attempts,
                            session,
                        })
                        .is_err()
                    {
                        return; // main loop dropped the receiver — shut down.
                    }
                }
            })
            .expect("spawn attach-worker thread");
        AttachWorker {
            cmd_tx,
            result_rx,
            _thread: thread,
        }
    }

    /// Dispatch an attach to the worker. Best-effort: returns false if the
    /// worker is gone (the caller keeps the entry queued for retry).
    pub fn request(&self, req: AttachRequest) -> bool {
        self.cmd_tx.send(req).is_ok()
    }
}
