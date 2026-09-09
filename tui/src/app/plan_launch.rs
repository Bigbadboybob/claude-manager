//! Remote planning launches: checkout setup and attachment must not block input.
use super::*;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone)]
pub(super) struct PlanLaunchSpec {
    pub project: String,
    pub slug: String,
    pub prompt: String,
    pub task_id: String,
    pub parent_task_id: Option<String>,
    pub in_place: bool,
    pub engine: String,
    pub host: cm_daemon::host_id::HostId,
    pub repo_url: String,
    pub uid: String,
    pub workspace_id: String,
    pub start_branch: Option<String>,
}

pub(super) struct PreparedPlanLaunch {
    pub main_repo: Option<PathBuf>,
    pub worktree_path: PathBuf,
    pub session: Session,
    pub pending: Option<Vec<String>>,
    pub remote_branch: Option<String>,
}

pub(super) struct PlanLaunchFlight {
    pub spec: PlanLaunchSpec,
    result: Receiver<anyhow::Result<PreparedPlanLaunch>>,
}

impl App {
    pub(super) fn start_remote_plan_launch(&mut self, spec: PlanLaunchSpec) {
        if self
            .plan_launches
            .iter()
            .any(|f| f.spec.task_id == spec.task_id)
        {
            self.set_status_msg("This task is already being launched");
            return;
        }
        if let Some((wi, si)) = self.workspaces.iter().enumerate().find_map(|(wi, w)| {
            w.sessions
                .iter()
                .position(|s| {
                    !s.session.exited && s.task_id.as_deref() == Some(spec.task_id.as_str())
                })
                .map(|si| (wi, si))
        }) {
            self.cursor = Cursor::Session(wi, si);
            self.view_mode = ViewMode::Sessions;
            self.set_status_msg("Task already has a session");
            return;
        }
        let Some(socket) = self.host_pool.live_socket_path(&spec.host) else {
            self.set_status_msg(&format!("Host `{}` is unavailable", spec.host));
            return;
        };
        let pool = std::sync::Arc::clone(&self.host_pool);
        let (cols, rows) = self.last_term_size;
        let job = spec.clone();
        let (tx, result) = mpsc::channel();
        let spawn = std::thread::Builder::new()
            .name("cm-plan-launch".into())
            .spawn(move || {
                let run = || -> anyhow::Result<PreparedPlanLaunch> {
                    let token = pool.operator_token_for(&job.host);
                    let existing = crate::client_session::rpc_list_daemon_sessions(&socket, &token)?;
                    anyhow::ensure!(
                        !existing.iter().any(|s| s.task_id.as_deref() == Some(&job.task_id)),
                        "This task already has a session on {}; waiting for the viewer to recover it",
                        job.host,
                    );
                    let created = crate::client_session::rpc_create_session_with_timeout(
                        &socket,
                        &token,
                        &job.uid,
                        &job.workspace_id,
                        &job.slug,
                        if job.engine == "claude" {
                            "claude-code"
                        } else {
                            &job.engine
                        },
                        &job.repo_url,
                        job.start_branch.as_deref(),
                        &job.slug,
                        Some(&job.task_id),
                        cols,
                        rows,
                        job.in_place,
                        None,
                        Duration::from_secs(150),
                    );
                    // A lost reply does not imply a failed spawn. Recover by the
                    // original UID; never repeat the create or kill a live worker.
                    let (path, main_repo, branch) = match created {
                        Ok(r) => (
                            PathBuf::from(r.worktree_path),
                            r.main_repo_path.map(PathBuf::from),
                            r.branch,
                        ),
                        Err(error) => {
                            let found =
                                crate::client_session::rpc_list_daemon_sessions(&socket, &token)
                                    .ok()
                                    .and_then(|rows| {
                                        rows.into_iter().find(|s| s.session_uid == job.uid)
                                    });
                            let Some(path) = found.and_then(|s| s.worktree_path) else {
                                return Err(error);
                            };
                            (PathBuf::from(path), None, None)
                        }
                    };
                    let session = try_attach_via_daemon_with_deps(
                        &pool,
                        &job.uid,
                        &job.workspace_id,
                        &path,
                        &job.engine,
                        &job.slug,
                        cols,
                        rows,
                        Some(&job.task_id),
                        None,
                        None,
                        &job.host,
                        None,
                    )?;
                    Ok(PreparedPlanLaunch {
                        main_repo,
                        worktree_path: path,
                        session,
                        pending: None,
                        remote_branch: branch,
                    })
                };
                let _ = tx.send(run());
            });
        match spawn {
            Ok(_) => {
                self.set_status_msg(&format!("Launching {} on {}…", spec.slug, spec.host));
                self.plan_launches.push(PlanLaunchFlight { spec, result });
            }
            Err(e) => self.set_status_msg(&format!("Could not start launch worker: {e}")),
        }
    }

    pub(crate) fn drain_plan_launches(&mut self) {
        let mut i = 0;
        while i < self.plan_launches.len() {
            let result = match self.plan_launches[i].result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Err(anyhow::anyhow!("launch worker disconnected"))
                }
            };
            let flight = self.plan_launches.remove(i);
            match result {
                Ok(prepared) => self.finish_plan_launch(flight.spec, prepared),
                Err(e) => {
                    self.set_status_msg(&format!(
                        "Launch {}: {e:#}. Any created worker will be recovered automatically.",
                        flight.spec.slug
                    ));
                }
            }
            self.needs_redraw = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{HostConfig, HostTransport, HostsConfig};
    use cm_daemon::control::{protocol::Response, wire};
    use std::os::unix::net::UnixListener;

    #[test]
    fn lost_create_reply_recovers_original_uid_off_thread_without_recreate_or_kill() {
        let _guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("launch.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            // Reject attach after proving the lost response was recovered.
            // No real daemon, agent or checkout is used by this test.
            for (i, method) in [
                "list_sessions",
                "create_session",
                "list_sessions",
                "attach.direct",
            ]
            .iter()
            .enumerate()
            {
                let (mut stream, _) = listener.accept().unwrap();
                let req = wire::read_request(&mut stream).unwrap().unwrap();
                assert_eq!(&req.method, method);
                if i == 1 {
                    assert_eq!(req.params["uid"], "launch-uid");
                    assert_eq!(req.params["task_id"], "launch-task");
                    std::thread::sleep(Duration::from_millis(250));
                    continue; // Lost response, although the daemon created it.
                }
                if i == 3 {
                    assert_eq!(req.params["uid"], "launch-uid");
                    break; // Attach transport failure must never kill the worker.
                }
                let result = if i == 0 {
                    serde_json::json!([])
                } else {
                    serde_json::json!([{"session_uid":"launch-uid", "type":"codex",
                        "task_id":"launch-task", "workspace_id":"launch-ws",
                        "worktree_path":"/tmp/remote-checkout"}])
                };
                wire::write_response(
                    &mut stream,
                    &Response {
                        id: req.id,
                        ok: true,
                        result: Some(result),
                        error: None,
                    },
                )
                .unwrap();
            }
        });
        let host = cm_daemon::host_id::HostId::new("sessions");
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
        let spec = PlanLaunchSpec {
            project: "repo".into(),
            slug: "task".into(),
            prompt: "draft".into(),
            task_id: "launch-task".into(),
            parent_task_id: None,
            in_place: false,
            engine: "codex".into(),
            host,
            repo_url: "https://example.org/repo".into(),
            uid: "launch-uid".into(),
            workspace_id: "launch-ws".into(),
            start_branch: None,
        };
        let started = Instant::now();
        app.start_remote_plan_launch(spec.clone());
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "launch must return before the slow create reply"
        );
        app.start_remote_plan_launch(spec);
        assert_eq!(
            app.plan_launches.len(),
            1,
            "repeated launch must not spawn a second worker"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app.plan_launches.is_empty() && Instant::now() < deadline {
            app.drain_plan_launches();
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(app.plan_launches.is_empty());
        server.join().unwrap();
    }
}
