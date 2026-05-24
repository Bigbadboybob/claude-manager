//! Sub-2b-2 (10d-mcp-surface-2b-2): blocking HTTP client for the
//! planning API. Mirrors the surface that
//! `mcp_server/cli/planning_client.py::PlanningClient.propose_task`
//! wraps today, so the daemon can serve `propose_task` MCP calls
//! authoritatively (no TUI dependency, no Python re-implementation
//! divergence).
//!
//! ## Why this exists daemon-side
//!
//! Pre-sub-2b-2 the Python MCP `propose_task` tool talked directly
//! to the planning API via the `cli` package's `PlanningClient`.
//! That worked, but it left two cross-cutting concerns un-addressed:
//!
//!   1. **Auth uniformity.** Sub-2a's `check_session_caller` and
//!      the task-subtree machinery only ran for tools that routed
//!      through the daemon. `propose_task` bypassed it — so an
//!      agent could propose a task to ANY project, regardless of
//!      its own task scope.
//!   2. **Architectural coherence.** Phase 1's goal is
//!      "daemon owns the orchestration runtime." With the Python
//!      tool talking directly to the API for `propose_task`, the
//!      daemon would never see those calls — couldn't log them,
//!      couldn't enforce policy, couldn't survive future API
//!      surface changes uniformly.
//!
//! The daemon now owns the API talk for `propose_task`. The
//! Python tool routes through `control_client.call("propose_task", …)`
//! when `CM_DAEMON_SOCKET` is set (the same dispatch convention
//! sub-2a established for `send_input` / `kill_session` / etc.),
//! and falls back to the direct PlanningClient path otherwise
//! (development without the daemon, the test suite, etc.).
//!
//! ## Why ureq (not reqwest)
//!
//! ureq is a blocking client and the daemon's dispatcher is
//! blocking too. reqwest would have required pulling in a Tokio
//! runtime AND rewriting the dispatcher to bridge sync ↔ async
//! around each HTTP call — pure churn for one method. The TUI
//! already uses `ureq = "3"` in `tui/src/api.rs`; the daemon
//! adopting the same dep + version keeps the two crates aligned
//! and avoids two HTTP stacks in the final binary tree.
//!
//! ## Env vars (matches the existing PlanningClient contract)
//!
//!   - `CM_API_URL`: required, base URL of the planning API.
//!     Injected by the TUI's MCP config builder when it spawns
//!     agents; explicit set required for ad-hoc daemon invocations
//!     (e.g. tests, agents not spawned through the TUI).
//!   - `CM_API_TOKEN`: required, bearer token for API auth.
//!     Same injection path.
//!
//! Both env vars are read PER CALL (not cached) so the daemon
//! picks up rotated tokens without a restart. The cost is one
//! env read per `propose_task` — negligible.

use serde::Serialize;
use std::time::Duration;

use crate::control::protocol::ErrorCode;

/// `propose_task` request body. Mirrors the planning API's
/// `POST /tasks` schema — same field set the existing
/// `cli/planning_client.py::propose_task` builds today
/// (`source: "claude"`, draft status, repo_branch defaults to
/// `main`). Sub-2b-2 keeps wire-shape parity so callers can A/B
/// the two paths during the migration without observable
/// behavior change.
#[derive(Serialize)]
struct ProposeTaskBody<'a> {
    repo_url: &'a str,
    repo_branch: &'a str,
    name: &'a str,
    project: &'a str,
    description: &'a str,
    /// Worker activation prompt. Falls back to `name` when the
    /// MCP caller didn't supply one (matches the Python tool's
    /// `prompt or name` shape).
    prompt: &'a str,
    source: &'static str,
    is_cloud: bool,
    priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    difficulty: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    depends: Option<&'a [String]>,
}

/// Parameters the daemon's `methods::propose_task` collects from
/// the MCP request and hands to this client.
pub struct ProposeTaskRequest<'a> {
    pub project: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub prompt: &'a str,
    pub repo_url: &'a str,
    pub difficulty: Option<i32>,
    pub depends: Option<&'a [String]>,
}

/// Concrete error variants. The dispatcher maps these onto
/// `ErrorCode`; the typed shape lets the method body distinguish
/// "config missing" (Internal — daemon misconfigured) from
/// "API rejected the request" (forwarded as the API's status code
/// + body so the agent sees what the planning queue rejected) from
/// "transport failure" (Internal — network/HTTP-level).
#[derive(Debug)]
pub enum PlanningClientError {
    /// `CM_API_URL` or `CM_API_TOKEN` is missing/empty. The daemon
    /// was not configured with planning-API credentials. Returned
    /// as `ErrorCode::Internal` because it's a daemon-side
    /// misconfiguration; the agent can't fix it.
    MissingConfig(&'static str),
    /// HTTP transport-level failure (connect refused, timeout,
    /// TLS error). Returned as `ErrorCode::Internal` — the agent
    /// might retry, but the daemon can't tell whether the API
    /// is up.
    Transport(String),
    /// API returned a non-2xx response. The status code + body
    /// are surfaced so the agent can see the rejection reason
    /// (e.g. "project not found" from a 404, validation errors
    /// from a 400). Mapped to `ErrorCode::InvalidParams` for
    /// 4xx and `ErrorCode::Internal` for 5xx.
    ApiError { status: u16, body: String },
}

impl PlanningClientError {
    pub fn to_method_err(&self) -> (ErrorCode, String) {
        match self {
            PlanningClientError::MissingConfig(var) => (
                ErrorCode::Internal,
                format!(
                    "daemon is missing planning API config — env var '{}' \
                     is unset; daemon was launched without CM_API_URL/\
                     CM_API_TOKEN being inherited",
                    var,
                ),
            ),
            PlanningClientError::Transport(msg) => (
                ErrorCode::Internal,
                format!("planning API transport failure: {}", msg),
            ),
            PlanningClientError::ApiError { status, body } => {
                let code = if (400..500).contains(status) {
                    ErrorCode::InvalidParams
                } else {
                    ErrorCode::Internal
                };
                (
                    code,
                    format!("planning API returned {}: {}", status, body),
                )
            }
        }
    }
}

/// Read `CM_API_URL` from env. Returned reference is owned (the
/// caller may pass `&String` to ureq), trailing slashes
/// normalized to mirror the TUI's `ApiClient` shape.
fn read_api_url() -> Result<String, PlanningClientError> {
    let url = std::env::var("CM_API_URL")
        .map_err(|_| PlanningClientError::MissingConfig("CM_API_URL"))?;
    let trimmed = url.trim().to_string();
    if trimmed.is_empty() {
        return Err(PlanningClientError::MissingConfig("CM_API_URL"));
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

fn read_api_token() -> Result<String, PlanningClientError> {
    let token = std::env::var("CM_API_TOKEN")
        .map_err(|_| PlanningClientError::MissingConfig("CM_API_TOKEN"))?;
    let trimmed = token.trim().to_string();
    if trimmed.is_empty() {
        return Err(PlanningClientError::MissingConfig("CM_API_TOKEN"));
    }
    Ok(trimmed)
}

/// Build a fresh `ureq::Agent` per call. ureq agents own a
/// connection pool; the daemon's expected RPC rate for
/// `propose_task` is low enough that per-call construction is
/// fine (and avoids a long-lived state field that would need
/// to be threaded through `DaemonState`). 10s timeout matches
/// the TUI's `ApiClient`.
fn build_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build(),
    )
}

/// POST to `<CM_API_URL>/tasks` with the propose-task body.
/// Returns the API's JSON response on success (the new task
/// row, including the assigned `id`).
///
/// This is the single API call sub-2b-2 ships. Future sub-slices
/// can grow this client with `list_projects`, `list_tasks`, etc.
/// as the daemon takes over more of what the Python
/// `PlanningClient` does today.
pub fn propose_task(req: &ProposeTaskRequest<'_>) -> Result<serde_json::Value, PlanningClientError> {
    let api_url = read_api_url()?;
    let api_token = read_api_token()?;
    let endpoint = format!("{}/tasks", api_url);
    let body = ProposeTaskBody {
        repo_url: req.repo_url,
        repo_branch: "main",
        name: req.name,
        project: req.project,
        description: req.description,
        // Match the Python tool's `prompt or name` fallback so
        // the worker agent gets a non-empty activation prompt
        // even when the MCP caller passed prompt="".
        prompt: if req.prompt.is_empty() {
            req.name
        } else {
            req.prompt
        },
        source: "claude",
        is_cloud: false,
        priority: 0,
        difficulty: req.difficulty,
        depends: req.depends,
    };
    let agent = build_agent();
    let auth = format!("Bearer {}", api_token);
    let mut response = match agent
        .post(&endpoint)
        .header("Authorization", &auth)
        .send_json(&body)
    {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(status)) => {
            // ureq v3 surfaces non-2xx as `StatusCode(u16)`.
            // We don't have the body here in this arm (no
            // response handle); a follow-up if we want
            // structured API error JSON would use
            // `http_status_as_error: false` on the agent
            // builder. For now, surface the status alone.
            return Err(PlanningClientError::ApiError {
                status,
                body: String::new(),
            });
        }
        Err(e) => return Err(PlanningClientError::Transport(e.to_string())),
    };
    response
        .body_mut()
        .read_json::<serde_json::Value>()
        .map_err(|e| PlanningClientError::Transport(format!("decode response: {}", e)))
}

/// Process-wide lock around env vars CM_API_URL / CM_API_TOKEN.
/// Sub-2b-2 test helper: this module's unit tests AND
/// dispatch.rs's propose_task tests mutate these vars; both
/// must serialize against each other.
///
/// `#[cfg(test)]` + `pub(crate)` so dispatch.rs tests can
/// import it. Production builds don't compile this in.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        super::test_env_lock()
    }

    /// Spawn a thread-local HTTP listener on localhost that
    /// records the inbound request (headers + body) and replies
    /// with a canned response. Returns (port, captured_request,
    /// stop_signal). Hand-rolled because `wiremock` would pull
    /// in tokio; the daemon's HTTP needs are too small for it.
    ///
    /// `canned_response_status` and `canned_response_body` shape
    /// the reply so per-test cases can exercise success + various
    /// error codes from one harness.
    fn spawn_stub_api(
        canned_response_status: u16,
        canned_response_body: &'static str,
    ) -> (u16, Arc<Mutex<CapturedRequest>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().unwrap().port();
        let captured = Arc::new(Mutex::new(CapturedRequest::default()));
        let captured_clone = Arc::clone(&captured);
        thread::spawn(move || {
            // Accept exactly one connection (each test only
            // makes one HTTP request); thread exits after.
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let mut accumulated = Vec::new();
                // Read until we see the body's content-length
                // bytes after the \r\n\r\n header terminator.
                // ureq sends Content-Length, so this is bounded.
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            accumulated.extend_from_slice(&buf[..n]);
                            if let Some(header_end) =
                                find_header_end(&accumulated)
                            {
                                let header_str = std::str::from_utf8(
                                    &accumulated[..header_end],
                                )
                                .unwrap_or("");
                                let content_length = header_str
                                    .lines()
                                    .find_map(|l| {
                                        let l = l.to_ascii_lowercase();
                                        l.strip_prefix("content-length:")
                                            .map(|v| {
                                                v.trim().parse::<usize>().ok()
                                            })
                                            .flatten()
                                    })
                                    .unwrap_or(0);
                                let body_received =
                                    accumulated.len() - (header_end + 4);
                                if body_received >= content_length {
                                    break;
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Parse captured request.
                let mut cap = captured_clone.lock().unwrap();
                cap.raw = String::from_utf8_lossy(&accumulated).into_owned();
                drop(cap);
                // Reply.
                let body_bytes = canned_response_body.as_bytes();
                let response = format!(
                    "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                    canned_response_status,
                    body_bytes.len(),
                    canned_response_body,
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        (port, captured)
    }

    fn find_header_end(buf: &[u8]) -> Option<usize> {
        buf.windows(4).position(|w| w == b"\r\n\r\n")
    }

    #[derive(Default)]
    struct CapturedRequest {
        raw: String,
    }

    impl CapturedRequest {
        fn body_json(&self) -> serde_json::Value {
            let body = self
                .raw
                .split_once("\r\n\r\n")
                .map(|(_, b)| b)
                .unwrap_or("");
            serde_json::from_str(body).expect("body is JSON")
        }
        fn auth_header(&self) -> Option<String> {
            self.raw.lines().find_map(|l| {
                // Lowercase only the prefix for case-insensitive
                // header-name matching; preserve the value (the
                // Bearer token's case-sensitive parts) verbatim.
                let (name, value) = l.split_once(':')?;
                if name.trim().eq_ignore_ascii_case("authorization") {
                    Some(value.trim().to_string())
                } else {
                    None
                }
            })
        }
        fn method_and_path(&self) -> (String, String) {
            let first = self.raw.lines().next().unwrap_or("");
            let parts: Vec<&str> = first.split_whitespace().collect();
            (
                parts.first().copied().unwrap_or("").to_string(),
                parts.get(1).copied().unwrap_or("").to_string(),
            )
        }
    }

    /// Helper: deterministic unused port (atomic counter into
    /// the 5-digit private range). NOT used today (`spawn_stub_api`
    /// uses port 0 + asks the kernel), but kept for future
    /// refactors that need a fixed port.
    #[allow(dead_code)]
    fn next_port() -> u16 {
        static N: AtomicU16 = AtomicU16::new(40_000);
        N.fetch_add(1, Ordering::Relaxed)
    }

    fn set_env(url: &str, token: &str) {
        // SAFETY: env mutation is process-wide; the env_lock at
        // each test's top serializes against parallel tests.
        unsafe {
            std::env::set_var("CM_API_URL", url);
            std::env::set_var("CM_API_TOKEN", token);
        }
    }

    fn clear_env() {
        unsafe {
            std::env::remove_var("CM_API_URL");
            std::env::remove_var("CM_API_TOKEN");
        }
    }

    fn make_req<'a>(project: &'a str, name: &'a str, repo_url: &'a str) -> ProposeTaskRequest<'a> {
        ProposeTaskRequest {
            project,
            name,
            description: "desc",
            prompt: "prompt",
            repo_url,
            difficulty: None,
            depends: None,
        }
    }

    #[test]
    fn propose_task_sends_correct_wire_shape() {
        let _g = env_lock();
        let (port, captured) = spawn_stub_api(
            200,
            r#"{"id":"task-123","name":"hello","project":"proj"}"#,
        );
        set_env(&format!("http://127.0.0.1:{}", port), "token-abc");
        let req = make_req("proj", "hello", "git@example.com:o/r.git");
        let resp = propose_task(&req).expect("propose ok");
        assert_eq!(resp["id"], "task-123");
        // Wire-shape pins.
        let cap = captured.lock().unwrap();
        let (method, path) = cap.method_and_path();
        assert_eq!(method, "POST");
        assert_eq!(path, "/tasks");
        assert_eq!(cap.auth_header().as_deref(), Some("Bearer token-abc"));
        let body = cap.body_json();
        assert_eq!(body["repo_url"], "git@example.com:o/r.git");
        assert_eq!(body["repo_branch"], "main");
        assert_eq!(body["name"], "hello");
        assert_eq!(body["project"], "proj");
        assert_eq!(body["description"], "desc");
        assert_eq!(body["prompt"], "prompt");
        assert_eq!(body["source"], "claude");
        assert_eq!(body["is_cloud"], false);
        assert_eq!(body["priority"], 0);
        assert!(body.get("difficulty").is_none() || body["difficulty"].is_null());
        clear_env();
    }

    /// Empty `prompt` falls back to `name` — mirrors the
    /// Python tool's `prompt or name` shape at
    /// `cli/planning_client.py:64`.
    #[test]
    fn propose_task_empty_prompt_falls_back_to_name() {
        let _g = env_lock();
        let (port, captured) = spawn_stub_api(
            200,
            r#"{"id":"task-fallback"}"#,
        );
        set_env(&format!("http://127.0.0.1:{}", port), "tok");
        let req = ProposeTaskRequest {
            project: "p",
            name: "the-name",
            description: "",
            prompt: "",
            repo_url: "git@x.com:a/b.git",
            difficulty: None,
            depends: None,
        };
        let _ = propose_task(&req).expect("ok");
        let cap = captured.lock().unwrap();
        assert_eq!(
            cap.body_json()["prompt"], "the-name",
            "empty prompt must fall back to name",
        );
        clear_env();
    }

    /// API returns 4xx → `PlanningClientError::ApiError` →
    /// maps to `ErrorCode::InvalidParams` so the agent sees the
    /// rejection as a parameter error (planning queue rejected
    /// e.g. unknown project).
    #[test]
    fn propose_task_4xx_maps_to_invalid_params() {
        let _g = env_lock();
        let (port, _) = spawn_stub_api(404, r#"{"detail":"project not found"}"#);
        set_env(&format!("http://127.0.0.1:{}", port), "tok");
        let req = make_req("ghost", "n", "u");
        let err = propose_task(&req).expect_err("must reject");
        match err {
            PlanningClientError::ApiError { status, .. } => assert_eq!(status, 404),
            other => panic!("expected ApiError, got {:?}", other),
        }
        let (code, _) = err.to_method_err();
        assert_eq!(code, ErrorCode::InvalidParams);
        clear_env();
    }

    /// API returns 5xx → `Internal`. Agent retry might help; the
    /// daemon isn't lying about a 500.
    #[test]
    fn propose_task_5xx_maps_to_internal() {
        let _g = env_lock();
        let (port, _) = spawn_stub_api(503, r#"{"error":"unavailable"}"#);
        set_env(&format!("http://127.0.0.1:{}", port), "tok");
        let req = make_req("p", "n", "u");
        let err = propose_task(&req).expect_err("must reject");
        let (code, _) = err.to_method_err();
        assert_eq!(code, ErrorCode::Internal);
        clear_env();
    }

    /// Missing CM_API_URL → MissingConfig → Internal with a
    /// message that names the missing var so the operator
    /// knows what to set.
    #[test]
    fn missing_api_url_is_internal_with_diagnostic() {
        let _g = env_lock();
        clear_env();
        unsafe { std::env::set_var("CM_API_TOKEN", "tok") };
        let req = make_req("p", "n", "u");
        let err = propose_task(&req).expect_err("must reject");
        let (code, msg) = err.to_method_err();
        assert_eq!(code, ErrorCode::Internal);
        assert!(
            msg.contains("CM_API_URL"),
            "diagnostic must name the missing var: {}",
            msg,
        );
        clear_env();
    }

    /// Missing CM_API_TOKEN → same shape, different var.
    #[test]
    fn missing_api_token_is_internal_with_diagnostic() {
        let _g = env_lock();
        clear_env();
        unsafe { std::env::set_var("CM_API_URL", "http://x") };
        let req = make_req("p", "n", "u");
        let err = propose_task(&req).expect_err("must reject");
        let (code, msg) = err.to_method_err();
        assert_eq!(code, ErrorCode::Internal);
        assert!(msg.contains("CM_API_TOKEN"), "msg: {}", msg);
        clear_env();
    }
}
