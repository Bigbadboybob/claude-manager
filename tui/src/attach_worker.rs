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
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::host_pool::HostPool;
use crate::session::Session;

/// What the worker needs to attach a single remote session.
pub const ATTACH_WORKERS: usize = 4;
pub const ATTACHES_PER_HOST: usize = 2;

pub struct AttachRequest {
    pub request_id: u64,
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
    pub request_id: u64,
    pub ws_id: String,
    pub entry: cm_daemon::manifest::ManifestEntry,
    pub attempts: u32,
    pub session: Option<Session>,
    /// `None` on success; `Some(kind)` classifies a failed attach.
    pub failure: Option<AttachFailureKind>,
    /// The host tunnel-generation captured at attach time (`Some` on success,
    /// `None` on failure). The main loop records it per-uid so the
    /// stale-generation watchdog can tell when this stream's tunnel was later
    /// replaced. Captured on the worker thread right after the attach so it
    /// reflects the generation the stream was actually dialed under.
    pub tunnel_generation: Option<u64>,
}

pub struct AttachWorker {
    cmd_tx: mpsc::SyncSender<AttachRequest>,
    /// Drained by the main loop (`App::drain_attach_results`).
    pub result_rx: mpsc::Receiver<AttachResult>,
    _threads: Vec<JoinHandle<()>>,
}

impl AttachWorker {
    pub fn spawn(host_pool: Arc<HostPool>) -> Self {
        // A rendezvous channel has no hidden FIFO. Undispatched work stays in
        // App where it can be reprioritized when the user selects a pane.
        let (cmd_tx, cmd_rx) = mpsc::sync_channel::<AttachRequest>(0);
        let cmd_rx = Arc::new(Mutex::new(cmd_rx));
        let (result_tx, result_rx) = mpsc::channel::<AttachResult>();
        let mut threads = Vec::new();
        for index in 0..ATTACH_WORKERS {
            let cmd_rx = Arc::clone(&cmd_rx);
            let result_tx = result_tx.clone();
            let host_pool = Arc::clone(&host_pool);
            let thread = std::thread::Builder::new()
                .name(format!("cm-tui-attach-{index}"))
                .spawn(move || {
                    loop {
                        // Release the receiver mutex BEFORE doing network I/O.
                        let request = cmd_rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
                        let Ok(req) = request else { return };
                        // Blocking is fine HERE — off the main thread. The attach
                        // RPCs route through the entry's own host socket.
                        let generation = host_pool
                            .for_host(&req.entry.host_id)
                            .map(|_| host_pool.tunnel_generation(&req.entry.host_id));
                        let outcome =
                            generation
                                .map_err(anyhow::Error::from)
                                .and_then(|generation| {
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
                                    )?;
                                    if host_pool.tunnel_generation(&req.entry.host_id) != generation
                                    {
                                        anyhow::bail!("tunnel changed during attach");
                                    }
                                    Ok((session, generation))
                                });
                        let (session, failure, tunnel_generation) = match outcome {
                            Ok((s, generation)) => (Some(s), None, Some(generation)),
                            Err(e) => {
                                let kind =
                                    if crate::client_session::attach_failure_is_session_gone(&e) {
                                        AttachFailureKind::SessionGone
                                    } else {
                                        AttachFailureKind::TransportDown
                                    };
                                (None, Some(kind), None)
                            }
                        };
                        if result_tx
                            .send(AttachResult {
                                request_id: req.request_id,
                                ws_id: req.ws_id,
                                entry: req.entry,
                                attempts: req.attempts,
                                session,
                                failure,
                                tunnel_generation,
                            })
                            .is_err()
                        {
                            return; // main loop dropped the receiver — shut down.
                        }
                    }
                })
                .expect("spawn attach-worker thread");
            threads.push(thread);
        }
        AttachWorker {
            cmd_tx,
            result_rx,
            _threads: threads,
        }
    }

    /// Dispatch an attach to the worker. Best-effort: returns false if the
    /// workers are busy or gone (App keeps and reprioritizes the request).
    pub fn request(&self, req: AttachRequest) -> bool {
        self.cmd_tx.try_send(req).is_ok()
    }
    #[cfg(test)]
    pub(crate) fn for_test() -> (
        Self,
        mpsc::Receiver<AttachRequest>,
        mpsc::Sender<AttachResult>,
    ) {
        let (cmd_tx, cmd_rx) = mpsc::sync_channel(ATTACH_WORKERS);
        let (result_tx, result_rx) = mpsc::channel();
        (
            Self {
                cmd_tx,
                result_rx,
                _threads: Vec::new(),
            },
            cmd_rx,
            result_tx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{HostConfig, HostTransport, HostsConfig};
    use cm_daemon::control::{
        protocol::{ErrorCode, Response},
        wire,
    };
    use cm_daemon::host_id::HostId;
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    #[test]
    fn stalled_host_does_not_block_attach_to_another_host() {
        let dir = tempfile::tempdir().unwrap();
        let slow_path = dir.path().join("slow.sock");
        let fast_path = dir.path().join("fast.sock");
        let slow = UnixListener::bind(&slow_path).unwrap();
        let fast = UnixListener::bind(&fast_path).unwrap();
        let (connected_tx, connected_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let slow_thread = std::thread::spawn(move || {
            let (mut socket, _) = slow.accept().unwrap();
            let _ = wire::read_request(&mut socket).unwrap();
            connected_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        let fast_thread = std::thread::spawn(move || {
            let (mut socket, _) = fast.accept().unwrap();
            let req = wire::read_request(&mut socket).unwrap().unwrap();
            wire::write_response(
                &mut socket,
                &Response::err(req.id, ErrorCode::NotFound, "test session absent"),
            )
            .unwrap();
        });
        let hosts = HostsConfig {
            hosts: vec![
                HostConfig {
                    id: HostId::local(),
                    default: true,
                    transport: HostTransport::Unix { socket: slow_path },
                    operator_token: Some("test".into()),
                    operator_token_file: None,
                },
                HostConfig {
                    id: HostId::new("fast"),
                    default: false,
                    transport: HostTransport::Unix { socket: fast_path },
                    operator_token: Some("test".into()),
                    operator_token_file: None,
                },
            ],
        };
        let worker = AttachWorker::spawn(Arc::new(HostPool::from_config(&hosts).unwrap()));
        let request = |id, host| {
            AttachRequest {
            request_id: id, ws_id: "test".into(), worktree: dir.path().to_path_buf(), cols: 80, rows: 24, attempts: 0,
            entry: serde_json::from_value(serde_json::json!({
                "uid": format!("test-{id}"), "label": "test", "session_type": "bash", "host_id": host,
            })).unwrap(),
        }
        };
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.request(request(1, "local")) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        connected_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        while !worker.request(request(2, "fast")) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        let result = worker.result_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        slow_thread.join().unwrap();
        fast_thread.join().unwrap();
        let result = result.expect("healthy host was held behind a stalled host");
        assert_eq!(result.request_id, 2);
        assert_eq!(result.failure, Some(AttachFailureKind::SessionGone));
    }
}
