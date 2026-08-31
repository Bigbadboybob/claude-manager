use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Task as returned by the API. `serde` ignores unknown fields by default, so
/// dropping fields here just means we stop deserializing them — the API
/// schema can keep returning them.
#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub id: String,
    pub created_at: String,
    pub repo_url: String,
    pub repo_branch: String,
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub status: String,
    pub worker_vm: Option<String>,
    pub worker_zone: Option<String>,
    pub blocked_at: Option<String>,
    pub session_id: Option<String>,
    pub wip_branch: Option<String>,
    // Planning fields
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub difficulty: Option<i32>,
    #[serde(default)]
    pub depends: Option<Vec<String>>,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub is_cloud: bool,
    /// Task kind: "oneshot" (default), "continuous", or "backtest".
    /// Backtest rows carry `metadata.backtest` (run_key, label, config,
    /// branch) and `metadata.vm` (project, zone) and run the pipeline in
    /// a root-owned tmux named "backtest" on the worker VM — the planning
    /// panel's `A-w` watch action keys off this to attach read-only.
    #[serde(default = "default_kind")]
    pub kind: String,
    /// FK to another task. Null for top-level tasks. Phase 5 subtask field.
    #[serde(default)]
    pub parent_task_id: Option<String>,
    /// "inherit" (default) or "branch". Only meaningful when
    /// `parent_task_id` is set. Phase 5 subtask field.
    #[serde(default = "default_worktree_mode")]
    pub worktree_mode: String,
    /// Free-form JSONB bag. Skills attach structured context here (e.g.
    /// `metadata.resume.design_doc_path` for the design-doc bundle) so
    /// the schema doesn't churn for every new shape. None = no bag.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// Provenance stamped on agent-filed backtests at `metadata.filer`.
///
/// Every field is optional so the TUI remains compatible with older rows and
/// partially-known callers. New submitters currently write schema version 1.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilerMetadata {
    pub schema_version: Option<u64>,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub workspace_id: Option<String>,
    pub worktree_path: Option<String>,
    pub machine: Option<String>,
    pub continuous_task_id: Option<String>,
    pub workflow_run_id: Option<String>,
    pub workflow_role: Option<String>,
    pub managed_by_session_id: Option<String>,
    pub submitted_via: Option<String>,
}

impl FilerMetadata {
    pub fn from_metadata(metadata: &Option<serde_json::Value>) -> Option<Self> {
        let filer = metadata.as_ref()?.get("filer")?.as_object()?;
        let text = |key: &str| {
            filer
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Some(Self {
            schema_version: filer.get("schema_version").and_then(|v| v.as_u64()),
            agent: text("agent"),
            session_id: text("session_id"),
            task_id: text("task_id"),
            workspace_id: text("workspace_id"),
            worktree_path: text("worktree_path"),
            machine: text("machine"),
            continuous_task_id: text("continuous_task_id"),
            workflow_run_id: text("workflow_run_id"),
            workflow_role: text("workflow_role"),
            managed_by_session_id: text("managed_by_session_id"),
            submitted_via: text("submitted_via"),
        })
    }

    /// Consistent label/value rows shared by planning detail and A-i peek.
    pub fn display_fields(&self) -> Vec<(&'static str, String)> {
        let mut fields = Vec::new();
        fields.push((
            "Filed by",
            self.agent.clone().unwrap_or_else(|| "unknown".into()),
        ));
        for (label, value) in [
            ("Filer session", &self.session_id),
            ("Filer task", &self.task_id),
            ("Workspace", &self.workspace_id),
            ("Machine", &self.machine),
            ("Worktree", &self.worktree_path),
            ("Continuous task", &self.continuous_task_id),
        ] {
            if let Some(value) = value {
                fields.push((label, value.clone()));
            }
        }
        if let Some(run) = &self.workflow_run_id {
            let value = match &self.workflow_role {
                Some(role) => format!("{} ({})", run, role),
                None => run.clone(),
            };
            fields.push(("Workflow", value));
        } else if let Some(role) = &self.workflow_role {
            fields.push(("Workflow role", role.clone()));
        }
        for (label, value) in [
            ("Managed by", &self.managed_by_session_id),
            ("Filed via", &self.submitted_via),
        ] {
            if let Some(value) = value {
                fields.push((label, value.clone()));
            }
        }
        fields
    }
}

fn default_worktree_mode() -> String {
    "inherit".to_string()
}

fn default_kind() -> String {
    "oneshot".to_string()
}

fn default_source() -> String {
    "user".to_string()
}

/// Body for creating a task.
#[derive(Serialize)]
pub struct TaskCreateBody {
    pub repo_url: String,
    pub repo_branch: String,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    // Planning fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_cloud: Option<bool>,
    // Subtask fields (Phase 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_mode: Option<String>,
    /// `wip_branch` at create time. Inherit-mode subtasks need this
    /// to persist their parent's branch through reconcile (without
    /// it the API row's `wip_branch` is NULL and the next reconcile
    /// blanks the local copy, so a branch-mode grandchild would
    /// fall back to "main" as its start ref).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wip_branch: Option<String>,
}

/// Blocking HTTP client for the Claude Manager API.
pub struct ApiClient {
    base_url: String,
    token: String,
    agent: ureq::Agent,
}

impl ApiClient {
    pub fn new(config: &Config) -> Self {
        // 30s global timeout (was 5s). The planning fetch runs on the backend
        // thread (`backend::do_refresh`), NOT the main loop, so a generous
        // timeout never freezes the UI — it just lets a slow fetch over a flaky
        // WAN link to a remote API host complete instead of erroring with
        // `json: timeout: global`. The backend re-fetches every 5s, so a
        // genuinely-dead host still surfaces a stale list rather than hanging
        // the app. Matches the CLI client's 30s.
        //
        // max_idle_age 2s (default 15s): uvicorn's default keep-alive idle
        // timeout is 5s — the same as the backend poll interval — so with the
        // default pool age every poll reused a connection that was exactly at
        // the server's kill threshold. When the server close crossed the WAN
        // in flight with the next request, the request went into a dying
        // socket and surfaced as `io: Peer disconnected` (ureq has no retry).
        // Capping reuse at 2s means a pooled connection is only reused while
        // the server is still guaranteed to hold it open; the 5s-idle poll
        // opens a fresh connection instead, while back-to-back requests
        // within one tick (tasks + plan-tasks) still share one.
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(std::time::Duration::from_secs(30)))
                .max_idle_age(std::time::Duration::from_secs(2))
                .build(),
        );
        ApiClient {
            base_url: config.api_url.trim_end_matches('/').to_string(),
            token: config.api_token.clone(),
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub fn list_tasks(&self, status: Option<&str>) -> anyhow::Result<Vec<Task>> {
        let url = match status {
            Some(s) => format!("{}?status={}", self.url("/tasks"), s),
            None => self.url("/tasks"),
        };
        let fetch = || -> anyhow::Result<Vec<Task>> {
            let body = self
                .agent
                .get(&url)
                .header("Authorization", &self.auth_header())
                .call()?
                .body_mut()
                .read_json::<Vec<Task>>()?;
            Ok(body)
        };
        // This is the recurring background poll: a single failure flips the
        // status bar to "API: ..." and the ● indicator to disconnected until
        // the next 5s tick. Retry once — but only for the fast-fail
        // interrupted-transport class (server closed a keep-alive connection
        // mid-flight, WAN blip) where a fresh connection is likely to
        // succeed immediately. Timeouts and connect failures are excluded so
        // a dead network can't double the 30s worst-case block on the
        // backend thread.
        match fetch() {
            Err(e) if is_interrupted_transport(&e) => fetch(),
            other => other,
        }
    }

    pub fn get_task(&self, task_id: &str) -> anyhow::Result<Task> {
        let body = self
            .agent
            .get(&self.url(&format!("/tasks/{}", task_id)))
            .header("Authorization", &self.auth_header())
            .call()?
            .body_mut()
            .read_json::<Task>()?;
        Ok(body)
    }

    pub fn create_task(&self, body: &TaskCreateBody) -> anyhow::Result<Task> {
        let resp = self
            .agent
            .post(&self.url("/tasks"))
            .header("Authorization", &self.auth_header())
            .send_json(body)?
            .body_mut()
            .read_json::<Task>()?;
        Ok(resp)
    }

    pub fn update_task(
        &self,
        task_id: &str,
        fields: &HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Task> {
        let resp = self
            .agent
            .patch(&self.url(&format!("/tasks/{}", task_id)))
            .header("Authorization", &self.auth_header())
            .send_json(fields)?
            .body_mut()
            .read_json::<Task>()?;
        Ok(resp)
    }

    pub fn delete_task(&self, task_id: &str) -> anyhow::Result<()> {
        self.agent
            .delete(&self.url(&format!("/tasks/{}", task_id)))
            .header("Authorization", &self.auth_header())
            .call()?;
        Ok(())
    }
}

/// True when the error is a transport interruption that fails fast —
/// the peer closed the connection under the request (stale keep-alive
/// reuse, server restart, WAN blip). These are the errors where one
/// immediate retry on a fresh connection is safe for an idempotent GET.
/// Deliberately excludes `ureq::Error::Timeout` and connect-phase
/// failures: retrying those can block for another full timeout when the
/// network is genuinely down.
fn is_interrupted_transport(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<ureq::Error>() {
        Some(ureq::Error::Io(io)) => matches!(
            io.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn filer_metadata_parses_and_formats_all_v1_identifiers() {
        let metadata = Some(serde_json::json!({
            "filer": {
                "schema_version": 1,
                "agent": "codex",
                "session_id": "session-1",
                "task_id": "task-1",
                "workspace_id": "workspace-1",
                "worktree_path": "/repo/worktree",
                "machine": "host-1",
                "continuous_task_id": "continuous-1",
                "workflow_run_id": "workflow-1",
                "workflow_role": "worker",
                "managed_by_session_id": "manager-1",
                "submitted_via": "mcp.submit_backtest"
            }
        }));
        let filer = FilerMetadata::from_metadata(&metadata).expect("filer");
        assert_eq!(filer.schema_version, Some(1));
        assert_eq!(filer.agent.as_deref(), Some("codex"));
        let fields = filer.display_fields();
        assert!(fields.contains(&("Filed by", "codex".into())));
        assert!(fields.contains(&("Filer session", "session-1".into())));
        assert!(fields.contains(&("Filer task", "task-1".into())));
        assert!(fields.contains(&("Workspace", "workspace-1".into())));
        assert!(fields.contains(&("Machine", "host-1".into())));
        assert!(fields.contains(&("Worktree", "/repo/worktree".into())));
        assert!(fields.contains(&("Continuous task", "continuous-1".into())));
        assert!(fields.contains(&("Workflow", "workflow-1 (worker)".into())));
        assert!(fields.contains(&("Managed by", "manager-1".into())));
        assert!(fields.contains(&("Filed via", "mcp.submit_backtest".into())));
    }

    #[test]
    fn filer_metadata_is_absent_on_legacy_tasks() {
        assert!(FilerMetadata::from_metadata(&None).is_none());
        assert!(FilerMetadata::from_metadata(&Some(serde_json::json!({
            "backtest": {"label": "legacy"}
        })))
        .is_none());
    }

    /// Per-connection behavior for the stub API server.
    enum Stub {
        /// Read the request, then close without sending anything —
        /// exactly what the client sees when uvicorn kills a keep-alive
        /// connection under an in-flight request.
        CloseNoResponse,
        /// Read the request, respond with the given status and body.
        Respond(u16, &'static str),
    }

    /// One accepted connection per `Stub` entry; the listener is dropped
    /// afterwards so any surplus connection attempt fails with
    /// ConnectionRefused instead of hanging the test. Returns the port
    /// and a counter of accepted connections.
    fn spawn_stub_api(behaviors: Vec<Stub>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let port = listener.local_addr().unwrap().port();
        let conns = Arc::new(AtomicUsize::new(0));
        let conns_srv = conns.clone();
        std::thread::spawn(move || {
            for behavior in behaviors {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                conns_srv.fetch_add(1, Ordering::SeqCst);
                // Drain the request head so the client's write completes
                // before we act (a GET fits in one segment).
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                match behavior {
                    Stub::CloseNoResponse => drop(stream),
                    Stub::Respond(status, body) => {
                        let resp = format!(
                            "HTTP/1.1 {} X\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                            status,
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes());
                    }
                }
            }
        });
        (port, conns)
    }

    fn client_for(port: u16) -> ApiClient {
        let config = Config {
            api_url: format!("http://127.0.0.1:{}", port),
            api_token: "test-token".into(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        };
        ApiClient::new(&config)
    }

    /// The mid-flight keep-alive close (`io: Peer disconnected`) recovers
    /// via one retry on a fresh connection instead of surfacing as a
    /// spurious "disconnected" flash in the status bar.
    #[test]
    fn list_tasks_retries_once_on_interrupted_transport() {
        let (port, conns) =
            spawn_stub_api(vec![Stub::CloseNoResponse, Stub::Respond(200, "[]")]);
        let client = client_for(port);
        let tasks = client.list_tasks(None).expect("retry must recover");
        assert!(tasks.is_empty());
        assert_eq!(conns.load(Ordering::SeqCst), 2, "expected exactly one retry");
    }

    /// A persistently-broken transport still fails after the single
    /// retry — the retry must not loop.
    #[test]
    fn list_tasks_gives_up_after_one_retry() {
        let (port, conns) = spawn_stub_api(vec![
            Stub::CloseNoResponse,
            Stub::CloseNoResponse,
            // Never reached unless the client (wrongly) retries twice.
            Stub::Respond(200, "[]"),
        ]);
        let client = client_for(port);
        let err = client.list_tasks(None).expect_err("must fail");
        assert!(is_interrupted_transport(&err), "unexpected error: {err}");
        assert_eq!(conns.load(Ordering::SeqCst), 2, "expected exactly one retry");
    }

    /// HTTP-level errors (4xx/5xx parsed from a live connection) are not
    /// transport interruptions and must not be retried.
    #[test]
    fn list_tasks_does_not_retry_http_status_errors() {
        let (port, conns) = spawn_stub_api(vec![
            Stub::Respond(500, "{}"),
            // Never reached unless the client (wrongly) retries.
            Stub::Respond(200, "[]"),
        ]);
        let client = client_for(port);
        let err = client.list_tasks(None).expect_err("must fail");
        assert!(!is_interrupted_transport(&err), "unexpected error: {err}");
        assert_eq!(conns.load(Ordering::SeqCst), 1, "status errors must not retry");
    }
}
