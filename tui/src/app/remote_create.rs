//! Remote A-n: the host creates and provisions the checkout before it replies,
//! which can outlast an ordinary control-RPC budget. Run the create and attach
//! off the input thread, recover a lost reply by the original UID, and never
//! leave behind a taskless session the viewer cannot adopt.
use super::*;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Checkout setup (fetch, worktree, provisioning) on a large repository.
const REMOTE_CREATE_TIMEOUT: Duration = Duration::from_secs(150);

#[derive(Clone)]
pub(super) struct RemoteCreateSpec {
    pub host: cm_daemon::host_id::HostId,
    pub repo_url: String,
    pub label: String,
    pub slug: String,
    pub engine: LaunchEngine,
    pub start_branch: Option<String>,
    pub idle_timeout_secs: u16,
    pub seed_from: Option<String>,
    pub in_place: bool,
    pub uid: String,
    pub workspace_id: String,
    /// Captured at request time; the cursor may move while the host works.
    pub section: Option<String>,
}

pub(super) struct PreparedRemoteCreate {
    pub session: Session,
    pub worktree_path: PathBuf,
    pub main_repo_path: Option<PathBuf>,
    pub resume_id: Option<String>,
}

pub(super) struct RemoteCreateFlight {
    pub spec: RemoteCreateSpec,
    result: Receiver<anyhow::Result<PreparedRemoteCreate>>,
}

/// Create, recover a lost reply by UID, then attach. Any failure after the
/// host may have spawned the session kills that UID: an unbound session is
/// never adopted by the viewer, so it would otherwise run unseen.
fn run_remote_create(
    pool: &crate::host_pool::HostPool,
    socket: &Path,
    job: &RemoteCreateSpec,
    cols: u16,
    rows: u16,
    timeout: Duration,
) -> anyhow::Result<PreparedRemoteCreate> {
    let token = pool.operator_token_for(&job.host);
    let session_type = job.engine.as_session_type();
    let wire_engine = match job.engine {
        LaunchEngine::Claude => "claude-code",
        LaunchEngine::Codex => "codex",
    };
    let discard = |error: anyhow::Error| -> anyhow::Error {
        match crate::client_session::rpc_kill_session(socket, &token, &job.uid) {
            Ok(()) => error,
            // A genuine refusal created nothing; only report a real leak risk.
            Err(e) if e.downcast_ref::<crate::client_session::DaemonRpcError>().is_some() => error,
            Err(e) => error.context(format!(
                "session {} may still be running on {} (cleanup failed: {e})",
                job.uid, job.host
            )),
        }
    };
    let created = crate::client_session::rpc_create_session_with_timeout(
        socket,
        &token,
        &job.uid,
        &job.workspace_id,
        session_type,
        wire_engine,
        &job.repo_url,
        job.start_branch.as_deref(),
        &job.slug,
        None,
        cols,
        rows,
        job.in_place,
        job.seed_from.as_deref(),
        timeout,
    );
    let (worktree_path, main_repo_path, resume_id) = match created {
        Ok(r) => (
            PathBuf::from(r.worktree_path),
            r.main_repo_path.map(PathBuf::from),
            r.resume_id,
        ),
        // The daemon answered with a refusal: nothing was spawned.
        Err(e) if e.downcast_ref::<crate::client_session::DaemonRpcError>().is_some() => {
            return Err(e);
        }
        // A lost reply does not imply a failed spawn.
        Err(e) => {
            let found = crate::client_session::rpc_list_daemon_sessions(socket, &token)
                .ok()
                .and_then(|rows| rows.into_iter().find(|s| s.session_uid == job.uid))
                .and_then(|s| s.worktree_path);
            match found {
                Some(path) => (PathBuf::from(path), None, None),
                None => return Err(discard(e)),
            }
        }
    };
    let session = try_attach_via_daemon_with_deps(
        pool,
        &job.uid,
        &job.workspace_id,
        &worktree_path,
        session_type,
        session_type,
        cols,
        rows,
        None,
        None,
        None,
        &job.host,
        None,
    )
    .map_err(|e| discard(e.context("attach")))?;
    Ok(PreparedRemoteCreate {
        session,
        worktree_path,
        main_repo_path,
        resume_id,
    })
}

impl App {
    pub(super) fn start_remote_create(&mut self, spec: RemoteCreateSpec) {
        self.start_remote_create_with_timeout(spec, REMOTE_CREATE_TIMEOUT);
    }

    fn start_remote_create_with_timeout(&mut self, spec: RemoteCreateSpec, timeout: Duration) {
        if self
            .remote_creates
            .iter()
            .any(|f| f.spec.host == spec.host && f.spec.slug == spec.slug)
        {
            self.set_status_msg(&format!("{} is already being created", spec.label));
            return;
        }
        // An unconfigured host fails now; only tunnel warm-up can block.
        if let Err(e) = self.host_pool.for_host(&spec.host) {
            self.set_status_msg(&format!(
                "Remote host `{}` not reachable: {e}",
                spec.host.as_str()
            ));
            return;
        }
        let pool = std::sync::Arc::clone(&self.host_pool);
        let (cols, rows) = self.last_term_size;
        let job = spec.clone();
        let (tx, result) = mpsc::channel();
        let spawn = std::thread::Builder::new()
            .name("cm-remote-create".into())
            .spawn(move || {
                // Warming a cold tunnel blocks, so resolve the socket here.
                let result = match pool.for_host(&job.host).ok().and_then(|h| h.socket_path()) {
                    Some(socket) => run_remote_create(&pool, &socket, &job, cols, rows, timeout),
                    None => Err(anyhow::anyhow!("host not reachable (no live socket)")),
                };
                let _ = tx.send(result);
            });
        match spawn {
            Ok(_) => {
                self.set_status_msg(&format!(
                    "Creating {} on `{}`…",
                    spec.label,
                    spec.host.as_str()
                ));
                self.remote_creates.push(RemoteCreateFlight { spec, result });
            }
            Err(e) => self.set_status_msg(&format!("Could not start create worker: {e}")),
        }
    }

    pub(crate) fn drain_remote_creates(&mut self) {
        let mut i = 0;
        while i < self.remote_creates.len() {
            let result = match self.remote_creates[i].result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!("create worker disconnected"))
                }
            };
            let flight = self.remote_creates.remove(i);
            match result {
                Ok(prepared) => self.finish_remote_create(flight.spec, prepared),
                Err(e) => self.set_status_msg(&format!(
                    "Remote create {} failed: {e:#}",
                    flight.spec.label
                )),
            }
            self.needs_redraw = true;
        }
    }

    fn finish_remote_create(&mut self, spec: RemoteCreateSpec, prepared: PreparedRemoteCreate) {
        let session_type = spec.engine.as_session_type();
        let ts = TerminalSession {
            color: None,
            uid: spec.uid,
            label: session_type.to_string(),
            session_type: session_type.to_string(),
            session: prepared.session,
            status: SessionStatus::Running,
            idle_since: None,
            last_write_at: None,
            transcript_id: if session_type == "claude" {
                prepared.resume_id
            } else {
                None
            },
            generation: 0,
            // Remote: the worktree lives on the daemon's filesystem, so the
            // TUI can't run local JSONL detection; the daemon broadcasts the
            // transcript binding instead.
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: spec.idle_timeout_secs,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            last_delivery: None,
            task_id: None,
            notify_on_idle: false,
            global_perms: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: spec.seed_from,
            preserved_last_exit: None,
            host_id: spec.host.clone(),
        };
        let ws = Workspace {
            color: None,
            pinned: false,
            id: spec.workspace_id,
            name: spec.label.clone(),
            is_closed: false,
            is_cloud: false,
            repo_url: Some(spec.repo_url),
            worktree_path: Some(prepared.worktree_path),
            // The main checkout lives on the remote host.
            main_repo_path: prepared.main_repo_path,
            worker_vm: None,
            worker_zone: None,
            host_id: spec.host.clone(),
            sessions: vec![ts],
            tombstones: Vec::new(),
        };
        let new_wi = self.workspaces.len();
        let new_ws_id = ws.id.clone();
        self.workspaces.push(ws);
        if let Some(sid) = spec.section {
            // Same A-n-inside-a-section inheritance as the local path.
            self.workspace_sections.insert(new_ws_id, sid);
        }
        self.cursor = Cursor::Session(new_wi, 0);
        self.save_session_manifest();
        self.set_status_msg(&format!("Workspace created on `{}`", spec.host.as_str()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{HostConfig, HostTransport, HostsConfig};
    use cm_daemon::control::{protocol::Response, wire};
    use std::os::unix::net::UnixListener;

    fn test_app(socket: PathBuf, host: &cm_daemon::host_id::HostId) -> App {
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.host_pool = std::sync::Arc::new(
            crate::host_pool::HostPool::from_config(&HostsConfig {
                hosts: vec![HostConfig {
                    id: host.clone(),
                    transport: HostTransport::Unix { socket },
                    default: true,
                    operator_token: Some("test".into()),
                    operator_token_file: None,
                }],
            })
            .unwrap(),
        );
        app
    }

    fn spec(host: cm_daemon::host_id::HostId) -> RemoteCreateSpec {
        RemoteCreateSpec {
            host,
            repo_url: "https://example.org/repo".into(),
            label: "data-discussion".into(),
            slug: "data-discussion".into(),
            engine: LaunchEngine::Claude,
            start_branch: None,
            idle_timeout_secs: 0,
            seed_from: None,
            in_place: false,
            uid: "create-uid".into(),
            workspace_id: "create-ws".into(),
            section: None,
        }
    }

    fn reply(stream: &mut std::os::unix::net::UnixStream, id: String, result: serde_json::Value) {
        wire::write_response(
            stream,
            &Response { id, ok: true, result: Some(result), error: None },
        )
        .unwrap();
    }

    fn drain(app: &mut App) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.remote_creates.is_empty() && Instant::now() < deadline {
            app.drain_remote_creates();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.remote_creates.is_empty());
    }

    /// The reported failure: the host spawned the session but replied after
    /// the viewer stopped waiting. The viewer must not block input, must not
    /// report a spurious failure path that repeats the create, and must find
    /// the session by its original UID.
    #[test]
    fn slow_create_reply_recovers_original_uid_off_thread() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("create.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let mut methods: Vec<String> = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().unwrap();
                let req = wire::read_request(&mut stream).unwrap().unwrap();
                methods.push(req.method.clone());
                match req.method.as_str() {
                    "create_session" => {
                        assert_eq!(req.params["uid"], "create-uid");
                        std::thread::sleep(Duration::from_millis(250));
                        // Reply lost: the viewer already timed out.
                    }
                    "list_sessions" => reply(
                        &mut stream,
                        req.id,
                        serde_json::json!([{"session_uid":"create-uid", "type":"claude-code",
                            "workspace_id":"create-ws", "worktree_path":"/tmp/remote-checkout"}]),
                    ),
                    "kill_session" => {
                        assert_eq!(req.params["session_uid"], "create-uid");
                        reply(&mut stream, req.id, serde_json::json!({"ok": true}));
                        break;
                    }
                    // Attach refused (dropped): the recovered session is discarded.
                    _ => assert_eq!(req.params["uid"], "create-uid"),
                }
            }
            methods
        });
        let host = cm_daemon::host_id::HostId::new("sessions");
        let mut app = test_app(socket, &host);
        let started = Instant::now();
        app.start_remote_create_with_timeout(spec(host.clone()), Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_millis(200), "A-n must not block input");
        app.start_remote_create_with_timeout(spec(host), Duration::from_millis(50));
        assert_eq!(app.remote_creates.len(), 1, "a repeated A-n must not create twice");
        drain(&mut app);
        let methods = server.join().unwrap();
        assert_eq!(&methods[..2], ["create_session", "list_sessions"]);
        assert_eq!(
            methods.iter().filter(|m| *m == "create_session").count(),
            1,
            "never repeat the create"
        );
        assert!(methods[2..methods.len() - 1].iter().all(|m| m.starts_with("attach")
            || m.starts_with("session.attach")), "{methods:?}");
        assert_eq!(methods.last().unwrap(), "kill_session", "an unattached taskless session must not leak");
    }

    /// A lost reply for a session the host never registered is discarded by
    /// UID, so a late spawn cannot become an invisible orphan.
    #[test]
    fn lost_create_reply_without_session_kills_the_uid() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("create.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let mut methods = Vec::new();
            for i in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let req = wire::read_request(&mut stream).unwrap().unwrap();
                methods.push(req.method.clone());
                match i {
                    0 => std::thread::sleep(Duration::from_millis(250)),
                    1 => reply(&mut stream, req.id, serde_json::json!([])),
                    _ => {
                        assert_eq!(req.params["session_uid"], "create-uid");
                        reply(&mut stream, req.id, serde_json::json!({"ok": true}));
                    }
                }
            }
            methods
        });
        let host = cm_daemon::host_id::HostId::new("sessions");
        let mut app = test_app(socket, &host);
        app.start_remote_create_with_timeout(spec(host), Duration::from_millis(50));
        drain(&mut app);
        assert_eq!(
            server.join().unwrap(),
            ["create_session", "list_sessions", "kill_session"]
        );
        assert!(app.workspaces.iter().all(|w| w.id != "create-ws"));
    }
}
