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

/// Why an attach attempt failed, so the main loop can pick a recovery policy
/// without re-deriving it (the classification needs the raw `anyhow::Error`,
/// which doesn't cross the channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachFailureKind {
    /// The attach reached the daemon and it reported the session doesn't exist
    /// (`ErrorCode::NotFound`). The daemon-side session is genuinely gone —
    /// count this toward the give-up budget and eventually mark the slot exited.
    SessionGone,
    /// Anything else: tunnel down / respawning, connect refused, RPC I/O
    /// timeout, or a non-NotFound daemon code. The daemon session is (almost
    /// certainly) still alive; keep the slot reconnecting and retry WITHOUT
    /// burning the give-up budget, so a transient transport outage never
    /// tears a live session down.
    TransportDown,
}

/// The outcome of an [`AttachRequest`]. `session` is `Some` on success. On
/// failure it's `None` and [`Self::failure`] carries the reason so the main
/// loop can retry-forever (transport) vs give-up-after-cap (session gone).
pub struct AttachResult {
    pub ws_id: String,
    pub entry: cm_daemon::manifest::ManifestEntry,
    pub attempts: u32,
    pub session: Option<Session>,
    /// `None` on success; `Some(kind)` classifies a failed attach.
    pub failure: Option<AttachFailureKind>,
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
                    let outcome = crate::app::try_attach_via_daemon_with_deps(
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
                    );
                    let (session, failure) = match outcome {
                        Ok(s) => (Some(s), None),
                        Err(e) => {
                            let kind = if crate::client_session::attach_failure_is_session_gone(&e) {
                                AttachFailureKind::SessionGone
                            } else {
                                AttachFailureKind::TransportDown
                            };
                            (None, Some(kind))
                        }
                    };
                    if result_tx
                        .send(AttachResult {
                            ws_id: req.ws_id,
                            entry: req.entry,
                            attempts: req.attempts,
                            session,
                            failure,
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
