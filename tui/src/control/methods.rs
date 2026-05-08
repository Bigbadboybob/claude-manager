//! Method handlers for the control socket. Each handler takes `&mut App`
//! plus the request params and returns a `serde_json::Value` (or an
//! `ErrorCode` + message). Dispatch lives in `App::dispatch_control`.
//!
//! Auth model (per AGENT_ORCHESTRATION.md):
//!   - Caller's workspace + (optional) task is looked up server-side
//!     from the request's `caller.session_uid`. The MCP server only
//!     ever sends the UID; task is derived here, never trusted.
//!   - Tasked caller (workflow- or planning-launched): can act on its
//!     own task. Phase 5 will extend to descendants once subtasks exist.
//!   - Taskless caller (`A-n`): can act on any session in its own
//!     workspace, but `task_id` parameters are rejected.
//!   - Tombstones are visible to the same scope rule (so an agent can
//!     read what its own sibling did before exiting).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent;
use crate::app::{App, SessionStatus, SessionTombstone, TerminalSession, Workspace};
use crate::control::protocol::ErrorCode;

/// Result of a method handler. `Err(ErrorCode, message)` produces an
/// error envelope; `Ok(Value)` produces a success envelope.
pub type MethodResult = Result<Value, (ErrorCode, String)>;

/// Map the internal `session_type` ("claude" / "codex" / "bash") to the
/// public MCP wire form. The internal name predates the agent
/// orchestration surface; "claude-code" is what `start_session` accepts
/// and what the MCP tool docs advertise.
fn public_session_type(internal: &str) -> &str {
    match internal {
        "claude" => "claude-code",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Auth helpers
// ---------------------------------------------------------------------------

/// Locate a live session by uid. Returns (workspace_index, session_index).
pub fn find_live_session(workspaces: &[Workspace], uid: &str) -> Option<(usize, usize)> {
    for (wi, ws) in workspaces.iter().enumerate() {
        for (si, ts) in ws.sessions.iter().enumerate() {
            if ts.uid == uid {
                return Some((wi, si));
            }
        }
    }
    None
}

/// Locate a tombstone by uid. Returns (workspace_index, tombstone_index).
pub fn find_tombstone(workspaces: &[Workspace], uid: &str) -> Option<(usize, usize)> {
    for (wi, ws) in workspaces.iter().enumerate() {
        for (ti, t) in ws.tombstones.iter().enumerate() {
            if t.uid == uid {
                return Some((wi, ti));
            }
        }
    }
    None
}

/// Where the target session lives — used for both auth checks and the
/// resolver's `state` field.
pub enum TargetLocation {
    Live { wi: usize, si: usize },
    Exited { wi: usize, ti: usize },
}

/// Locate either a live session or a tombstone for `uid`.
pub fn locate_target(workspaces: &[Workspace], uid: &str) -> Option<TargetLocation> {
    if let Some((wi, si)) = find_live_session(workspaces, uid) {
        return Some(TargetLocation::Live { wi, si });
    }
    if let Some((wi, ti)) = find_tombstone(workspaces, uid) {
        return Some(TargetLocation::Exited { wi, ti });
    }
    None
}

/// Caller context derived from `session_uid`. Carries enough state to
/// scope reads and mutations.
#[derive(Debug)]
pub struct CallerCtx {
    pub workspace_index: Option<usize>,
    pub task_id: Option<String>,
    /// True when the caller is a tombstoned (already-closed) session.
    /// Read methods can still serve such callers — that's the whole
    /// point of Phase 2b retention. Mutating methods MUST refuse them
    /// (otherwise a request that arrives in the moment a session is
    /// closing — or any stale UID known to a peer — could keep mutating
    /// state for the full 30-day tombstone window).
    pub is_tombstone: bool,
}

/// Look up the caller, allowing tombstoned identities. Used by read
/// methods (`resolve_authorized_session`, `list_sessions`).
pub fn caller_ctx_or_tombstone(workspaces: &[Workspace], caller_uid: &str) -> Option<CallerCtx> {
    if let Some((wi, si)) = find_live_session(workspaces, caller_uid) {
        return Some(CallerCtx {
            workspace_index: Some(wi),
            task_id: workspaces[wi].sessions[si].task_id.clone(),
            is_tombstone: false,
        });
    }
    if let Some((wi, ti)) = find_tombstone(workspaces, caller_uid) {
        return Some(CallerCtx {
            workspace_index: Some(wi),
            task_id: workspaces[wi].tombstones[ti].task_id.clone(),
            is_tombstone: true,
        });
    }
    None
}

/// Look up the caller; refuses tombstoned identities. Used by mutating
/// methods. Returns `Err(Unauthorized, "caller_exited")` for a known-but-
/// closed caller, distinguishing it from an unknown UID (`NotFound`).
pub fn live_caller_ctx(
    workspaces: &[Workspace],
    caller_uid: &str,
) -> Result<CallerCtx, (ErrorCode, String)> {
    if let Some((wi, si)) = find_live_session(workspaces, caller_uid) {
        return Ok(CallerCtx {
            workspace_index: Some(wi),
            task_id: workspaces[wi].sessions[si].task_id.clone(),
            is_tombstone: false,
        });
    }
    if find_tombstone(workspaces, caller_uid).is_some() {
        return Err((
            ErrorCode::Unauthorized,
            "caller_exited: this session has been closed".into(),
        ));
    }
    Err((
        ErrorCode::NotFound,
        "caller session not found in any workspace".into(),
    ))
}

/// Check whether `caller` may read or mutate the target session/tombstone
/// in `target_wi`. Tasked callers: same task_id (Phase 5 adds descendants).
/// Taskless callers: same workspace.
pub fn caller_authorized_for(
    caller: &CallerCtx,
    target_wi: usize,
    target_task_id: Option<&str>,
) -> bool {
    let Some(caller_wi) = caller.workspace_index else {
        return false;
    };
    match &caller.task_id {
        Some(task_id) => {
            // Tasked caller — only their own task.
            target_task_id == Some(task_id.as_str()) && caller_wi == target_wi
        }
        None => {
            // Taskless caller — same workspace, regardless of target's task.
            caller_wi == target_wi
        }
    }
}

// ---------------------------------------------------------------------------
// resolve_authorized_session
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ResolveParams {
    session_uid: String,
}

#[derive(Serialize)]
struct ResolveResult {
    state: &'static str,
    engine: &'static str,
    transcript_path: Option<String>,
    generation: u64,
    /// True iff the session is live, its PTY has been quiet long enough
    /// to flip into `SessionStatus::Idle`, AND no input is pending in
    /// the queue. Always false for `state: "exited"` (a dead session
    /// isn't idle, it's gone).
    idle: bool,
}

/// Public-facing `state` value for a session. Mirrors what the resolver
/// returns and what `list_sessions` reports — same meaning, same wire
/// strings, no surprises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStateKind {
    /// Live session, transcript bound, ready for `send_input` /
    /// `read_session_output`.
    Ready,
    /// Live session, transcript not yet detected (fresh spawn or
    /// just-cleared, before the detector picks up the new file).
    Pending,
    /// PTY child has exited. The session row may still be in
    /// `ws.sessions` momentarily (between exit and the next close
    /// path) — that's a valid intermediate state, treated identically
    /// to a tombstoned session for orchestration purposes.
    Exited,
}

impl RuntimeStateKind {
    pub fn as_wire(self) -> &'static str {
        match self {
            RuntimeStateKind::Ready => "ready",
            RuntimeStateKind::Pending => "pending",
            RuntimeStateKind::Exited => "exited",
        }
    }
}

/// Unified runtime view used by every Phase 3 surface that reports on
/// a session's readiness/idle. Three call sites previously computed
/// these signals independently and disagreed:
///   - `list_sessions.idle` ignored `pending_prompt`/`pending_clear`.
///   - `resolve_authorized_session` ignored `ts.session.exited`.
///   - `compute_idle` (the original helper) didn't account for `exited`.
/// An agent polling `list_sessions` to decide "is the sibling done?"
/// would see one answer; `read_session_output` would see another;
/// `send_input` would reject for a third. Routing every surface
/// through this helper makes the answers structurally consistent.
pub fn session_runtime_state(ts: &TerminalSession) -> RuntimeState {
    if ts.session.exited {
        // Dead PTY — not "ready", not "pending", and definitely not
        // idle (in the agent-orchestration sense of "waiting for
        // input"). Path resolution still works via `ts.transcript_id`
        // until the close path tombstones the row.
        return RuntimeState {
            kind: RuntimeStateKind::Exited,
            idle: false,
        };
    }
    let kind = if ts.transcript_id.is_some() {
        RuntimeStateKind::Ready
    } else {
        RuntimeStateKind::Pending
    };
    // Idle definition (matches the existing UI dot heuristic):
    //   - `SessionStatus::Idle` — burst detection has decided no PTY
    //     output for ~`idle_timeout_secs` (default ~2s).
    //   - No `pending_prompt` / `pending_clear` queued — input that
    //     hasn't been delivered yet counts as in-flight work.
    let idle = matches!(ts.status, SessionStatus::Idle)
        && ts.pending_prompt.is_none()
        && ts.pending_clear.is_none();
    RuntimeState { kind, idle }
}

#[derive(Debug, Clone, Copy)]
pub struct RuntimeState {
    pub kind: RuntimeStateKind,
    pub idle: bool,
}

/// Resolve a session's transcript path for direct file IO by the MCP
/// server. Returns:
///   - `state="ready"` + path when the session is live with a bound
///     transcript file.
///   - `state="pending"` + path=null when live but transcript not yet
///     detected (fresh spawn / just-cleared).
///   - `state="exited"` + path when the session is gone but its last
///     transcript still exists on disk.
pub fn resolve_authorized_session(app: &App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: ResolveParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;

    // Read method — tombstoned callers are still allowed to read their
    // own scope (that's the point of Phase 2b retention).
    let caller = caller_ctx_or_tombstone(&app.workspaces, caller_uid)
        .ok_or((ErrorCode::NotFound, "caller session not found".into()))?;
    let target = locate_target(&app.workspaces, &p.session_uid)
        .ok_or((ErrorCode::NotFound, format!("session_uid not found")))?;

    match target {
        TargetLocation::Live { wi, si } => {
            let ts = &app.workspaces[wi].sessions[si];
            if !caller_authorized_for(&caller, wi, ts.task_id.as_deref()) {
                return Err((
                    ErrorCode::Unauthorized,
                    "session is outside caller's scope".into(),
                ));
            }
            let engine = engine_str(&ts.session_type);
            let runtime = session_runtime_state(ts);
            // Pending → no path yet. Ready or exited-but-still-live →
            // resolve via the live ts so reads work even in the brief
            // window before the close path tombstones the row.
            let path = match runtime.kind {
                RuntimeStateKind::Pending => None,
                RuntimeStateKind::Ready | RuntimeStateKind::Exited => {
                    resolve_live_path(&app.workspaces[wi], ts)
                }
            };
            Ok(json!(ResolveResult {
                state: runtime.kind.as_wire(),
                engine,
                transcript_path: path,
                generation: ts.generation,
                idle: runtime.idle,
            }))
        }
        TargetLocation::Exited { wi, ti } => {
            let tomb = &app.workspaces[wi].tombstones[ti];
            if !caller_authorized_for(&caller, wi, tomb.task_id.as_deref()) {
                return Err((
                    ErrorCode::Unauthorized,
                    "session is outside caller's scope".into(),
                ));
            }
            let engine = engine_str(&tomb.session_type);
            let path = resolve_exited_path(&app.workspaces[wi], tomb);
            Ok(json!(ResolveResult {
                state: "exited",
                engine,
                transcript_path: path,
                generation: tomb.generation,
                // Exited sessions are by definition not "idle in the
                // ready-for-input sense" — there's no PTY to write to.
                idle: false,
            }))
        }
    }
}

fn engine_str(session_type: &str) -> &'static str {
    match session_type {
        "codex" => "codex",
        _ => "claude-code",
    }
}

fn resolve_live_path(ws: &Workspace, ts: &TerminalSession) -> Option<String> {
    let sid = ts.transcript_id.as_deref()?;
    let wt = ws.worktree_path.as_deref()?;
    let agent = agent::agent_for(&ts.session_type);
    let ctx = agent::AgentCtx {
        ts,
        worktree_path: wt,
    };
    agent.transcript_path(ctx).map(|p| p.to_string_lossy().to_string()).or_else(|| {
        // transcript_path returns None if ts.transcript_id is None;
        // we already guarded for that. Defensive None.
        let _ = sid;
        None
    })
}

fn resolve_exited_path(_ws: &Workspace, tomb: &SessionTombstone) -> Option<String> {
    let sid = tomb.last_transcript_id.as_deref()?;
    // Use the tombstone's own worktree snapshot — survives subsequent
    // mutations of the live workspace (e.g. `push_active` clearing
    // `worktree_path` when uploading to cloud). Falls back to None for
    // the codex branch which doesn't need it.
    match tomb.session_type.as_str() {
        "codex" => {
            // Walk ~/.codex/sessions/YYYY/MM/DD for a file with matching id.
            // Re-implemented inline because the agent module's
            // `codex_transcript_path` is private.
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
            let sessions = home.join(".codex/sessions");
            find_codex_file(&sessions, sid).map(|p| p.to_string_lossy().to_string())
        }
        _ => {
            // Claude: encode the snapshotted worktree path same as
            // ClaudeCodeAgent does.
            let wt = tomb.worktree_path.as_deref()?;
            let home = std::env::var_os("HOME").map(std::path::PathBuf::from)?;
            let path_str = wt.to_str()?;
            let encoded = path_str.replace('/', "-").replace('.', "-");
            Some(
                home.join(format!(".claude/projects/{}", encoded))
                    .join(format!("{}.jsonl", sid))
                    .to_string_lossy()
                    .to_string(),
            )
        }
    }
}

fn find_codex_file(dir: &std::path::Path, transcript_id: &str) -> Option<std::path::PathBuf> {
    use std::fs;
    use std::io::BufRead;
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(hit) = find_codex_file(&path, transcript_id) {
                return Some(hit);
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            let mut reader = std::io::BufReader::new(file);
            let mut buf = String::new();
            if reader.read_line(&mut buf).is_err() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(buf.trim()) else {
                continue;
            };
            if v.pointer("/payload/id").and_then(|v| v.as_str()) == Some(transcript_id) {
                return Some(path);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// list_sessions
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ListSessionsParams {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    include_exited: bool,
}

pub fn list_sessions(app: &App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: ListSessionsParams = if params.is_null() {
        ListSessionsParams::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?
    };
    // Read method — tombstoned callers can still list (their own
    // workspace, including their own tombstone if include_exited).
    let caller = caller_ctx_or_tombstone(&app.workspaces, caller_uid)
        .ok_or((ErrorCode::NotFound, "caller session not found".into()))?;
    let Some(caller_wi) = caller.workspace_index else {
        return Err((
            ErrorCode::NotFound,
            "caller session not found in any workspace".into(),
        ));
    };

    // Resolve scope: explicit task_id (must be authorized), else default
    // (caller's task if any, else caller's workspace).
    let scope_task: Option<String> = match (p.task_id.as_deref(), caller.task_id.as_deref()) {
        (Some(req), None) => {
            // Taskless caller passing a task_id: unauthorized.
            return Err((
                ErrorCode::Unauthorized,
                format!("taskless caller cannot scope to task {}", req),
            ));
        }
        (Some(req), Some(own)) if req != own => {
            // Phase 5 will extend to descendants. Today: own task only.
            return Err((
                ErrorCode::Unauthorized,
                "scope to a different task is not yet allowed".into(),
            ));
        }
        (Some(_), Some(own)) => Some(own.to_string()),
        (None, own) => own.map(str::to_string),
    };

    let mut out: Vec<Value> = Vec::new();
    let ws = &app.workspaces[caller_wi];
    for ts in &ws.sessions {
        if !session_in_scope(ts.task_id.as_deref(), scope_task.as_deref()) {
            continue;
        }
        // Both `state` and `idle` flow from the same helper as the
        // resolver — guarantees `list_sessions` and
        // `resolve_authorized_session` agree on every signal. Without
        // this, an agent polling `list_sessions` to decide "is the
        // sibling done?" gets a different answer than
        // `read_session_output`.
        let runtime = session_runtime_state(ts);
        out.push(json!({
            "session_uid": ts.uid,
            "label": ts.label,
            // Normalize internal `session_type` to the public wire form.
            // Internally "claude" is used for compactness; the MCP
            // surface uses "claude-code" (matching `start_session`'s
            // accepted values). Without this normalization a
            // list→copy-type→start_session round-trip fails because
            // start_session rejects "claude".
            "type": public_session_type(&ts.session_type),
            "state": runtime.kind.as_wire(),
            "idle": runtime.idle,
            "managed_by_uid": ts.managed_by_uid,
        }));
    }
    if p.include_exited {
        for tomb in &ws.tombstones {
            if !tombstone_in_scope(tomb.task_id.as_deref(), scope_task.as_deref()) {
                continue;
            }
            out.push(json!({
                "session_uid": tomb.uid,
                "label": tomb.label,
                "type": public_session_type(&tomb.session_type),
                "state": "exited",
                "idle": Value::Null,
                "managed_by_uid": tomb.managed_by_uid,
            }));
        }
    }
    Ok(Value::Array(out))
}

fn session_in_scope(session_task: Option<&str>, scope_task: Option<&str>) -> bool {
    match scope_task {
        Some(t) => session_task == Some(t),
        None => true, // taskless scope = whole workspace
    }
}

fn tombstone_in_scope(tomb_task: Option<&str>, scope_task: Option<&str>) -> bool {
    session_in_scope(tomb_task, scope_task)
}

// ---------------------------------------------------------------------------
// send_input
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SendInputParams {
    session_uid: String,
    text: String,
    #[serde(default = "default_submit")]
    submit: bool,
}
fn default_submit() -> bool {
    true
}

pub fn send_input(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: SendInputParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;

    // Mutation — caller must be live, not tombstoned.
    let caller = live_caller_ctx(&app.workspaces, caller_uid)?;
    let (wi, si) = find_live_session(&app.workspaces, &p.session_uid)
        .ok_or((ErrorCode::NotFound, "session_uid not found".into()))?;
    let target_task = app.workspaces[wi].sessions[si].task_id.clone();
    if !caller_authorized_for(&caller, wi, target_task.as_deref()) {
        return Err((
            ErrorCode::Unauthorized,
            "session is outside caller's scope".into(),
        ));
    }
    if !p.submit {
        // Phase 1: only `submit=true` is wired through Agent. Without
        // submit, we'd need a typing path (no Enter). Punt for v1.
        return Err((
            ErrorCode::InvalidParams,
            "submit=false is not yet supported".into(),
        ));
    }
    // Reject writes into a dead PTY. Routed through the same
    // `session_runtime_state` helper as `list_sessions` and
    // `resolve_authorized_session` so every Phase 3 surface agrees on
    // when a session is "exited". Workflow fresh-respawn paths replace
    // `ts.session` with a live Session BEFORE the activation prompt is
    // delivered, so a legitimate respawn isn't gated by this.
    if session_runtime_state(&app.workspaces[wi].sessions[si]).kind
        == RuntimeStateKind::Exited
    {
        return Err((
            ErrorCode::Conflict,
            "session_not_writable: PTY has exited".into(),
        ));
    }
    let session_type = app.workspaces[wi].sessions[si].session_type.clone();
    let agent = agent::agent_for(&session_type);
    let wt = app.workspaces[wi]
        .worktree_path
        .clone()
        .unwrap_or_default();
    let ts = &mut app.workspaces[wi].sessions[si];
    let ctx = agent::AgentCtxMut {
        ts,
        worktree_path: &wt,
    };
    agent
        .submit_prompt(ctx, &p.text)
        .map_err(|e| (ErrorCode::Internal, format!("submit_prompt: {}", e)))?;
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// kill_session
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct KillParams {
    session_uid: String,
}

pub fn kill_session(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: KillParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    // Mutation — caller must be live, not tombstoned.
    let caller = live_caller_ctx(&app.workspaces, caller_uid)?;
    let (wi, si) = find_live_session(&app.workspaces, &p.session_uid)
        .ok_or((ErrorCode::NotFound, "session_uid not found".into()))?;
    let target_task = app.workspaces[wi].sessions[si].task_id.clone();
    if !caller_authorized_for(&caller, wi, target_task.as_deref()) {
        return Err((
            ErrorCode::Unauthorized,
            "session is outside caller's scope".into(),
        ));
    }
    // Tombstone-then-drop. Mark the PTY exited so the drainer cleans up.
    {
        let ws = &mut app.workspaces[wi];
        App::tombstone_session_pub(ws, si);
        if let Some(ts) = ws.sessions.get_mut(si) {
            ts.session.exited = true;
        }
        ws.sessions.remove(si);
    }
    // Persist immediately so a TUI crash between this kill and the next
    // unrelated save can't lose the tombstone (which would let the
    // killed session restore as live on next boot, breaking Phase 2b).
    app.save_session_manifest();
    Ok(json!({"ok": true}))
}

// ---------------------------------------------------------------------------
// start_session
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StartSessionParams {
    #[serde(default)]
    task_id: Option<String>,
    #[serde(rename = "type")]
    type_: String,
    label: String,
    #[serde(default)]
    prompt: Option<String>,
}

pub fn start_session(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: StartSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    if !matches!(p.type_.as_str(), "claude-code" | "codex") {
        return Err((
            ErrorCode::InvalidParams,
            format!("type must be claude-code or codex, got {}", p.type_),
        ));
    }
    // Mutation — caller must be live.
    let caller = live_caller_ctx(&app.workspaces, caller_uid)?;
    let Some(caller_wi) = caller.workspace_index else {
        return Err((ErrorCode::NotFound, "caller session not found".into()));
    };
    // task_id resolution: same rules as list_sessions.
    let task_id_for_new: Option<String> = match (p.task_id.as_deref(), caller.task_id.as_deref()) {
        (Some(req), None) => {
            return Err((
                ErrorCode::Unauthorized,
                format!("taskless caller cannot bind to task {}", req),
            ));
        }
        (Some(req), Some(own)) if req != own => {
            return Err((
                ErrorCode::Unauthorized,
                "binding to a different task is not yet allowed".into(),
            ));
        }
        (Some(_), Some(own)) => Some(own.to_string()),
        (None, own) => own.map(str::to_string),
    };

    let new_uid = app.spawn_managed_session(
        caller_wi,
        caller_uid,
        &p.type_,
        &p.label,
        task_id_for_new,
        p.prompt.as_deref(),
    )
    .map_err(|e| (ErrorCode::Internal, format!("spawn: {}", e)))?;
    Ok(json!({"session_uid": new_uid}))
}

#[cfg(test)]
mod tests {
    //! Auth-helper tests. These target `live_caller_ctx` /
    //! `caller_ctx_or_tombstone` directly because they're the choke
    //! point that decides whether a tombstoned UID can mutate state.
    //! End-to-end tests of the dispatch flow would need a full `App`,
    //! which spawns background threads; the helpers cover the
    //! correctness boundary without that overhead.

    use super::*;
    use crate::app::{SessionStatus, TerminalSession};
    use std::collections::HashMap;
    use std::time::Instant;

    fn dummy_session() -> crate::session::Session {
        crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new())
            .expect("dummy session")
    }

    fn live_ts(uid: &str, task_id: Option<&str>) -> TerminalSession {
        TerminalSession {
            uid: uid.into(),
            label: "test".into(),
            session_type: "claude".into(),
            session: dummy_session(),
            status: SessionStatus::Idle,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            task_id: task_id.map(str::to_string),
            last_delivery: None,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
        }
    }

    fn workspace_with(sessions: Vec<TerminalSession>, tombstones: Vec<SessionTombstone>) -> Workspace {
        Workspace {
            id: "ws-1".into(),
            name: "ws".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(std::path::PathBuf::from("/tmp/ws")),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            sessions,
            tombstones,
        }
    }

    fn make_tombstone(uid: &str) -> SessionTombstone {
        SessionTombstone {
            uid: uid.into(),
            managed_by_uid: None,
            label: "test".into(),
            session_type: "claude".into(),
            task_id: None,
            last_transcript_id: Some("transcript-x".into()),
            worktree_path: Some(std::path::PathBuf::from("/tmp/ws")),
            generation: 0,
            exited_at: 0.0,
        }
    }

    #[test]
    fn live_caller_resolves_for_live_session() {
        let ws = workspace_with(vec![live_ts("caller-uid", None)], vec![]);
        let workspaces = vec![ws];
        let ctx = live_caller_ctx(&workspaces, "caller-uid").expect("live caller resolves");
        assert!(!ctx.is_tombstone);
        assert_eq!(ctx.workspace_index, Some(0));
    }

    #[test]
    fn live_caller_rejects_tombstoned_with_unauthorized() {
        // Critical regression: a tombstoned UID must NOT authorize a
        // mutation. The error must be `Unauthorized` (caller exited),
        // distinct from `NotFound` (caller never existed) so the agent
        // can tell the two apart.
        let ws = workspace_with(vec![], vec![make_tombstone("dead-uid")]);
        let workspaces = vec![ws];
        let err = live_caller_ctx(&workspaces, "dead-uid").expect_err("must reject");
        assert_eq!(err.0, ErrorCode::Unauthorized);
        assert!(
            err.1.contains("caller_exited"),
            "message should signal exit, got {:?}",
            err.1
        );
    }

    #[test]
    fn live_caller_rejects_unknown_with_not_found() {
        let workspaces: Vec<Workspace> = vec![];
        let err = live_caller_ctx(&workspaces, "ghost").expect_err("must reject");
        assert_eq!(err.0, ErrorCode::NotFound);
    }

    #[test]
    fn read_caller_resolves_for_tombstone() {
        // Reads (`resolve_authorized_session`, `list_sessions`) must
        // still work for tombstoned callers — that's the whole point
        // of Phase 2b retention.
        let ws = workspace_with(vec![], vec![make_tombstone("dead-uid")]);
        let workspaces = vec![ws];
        let ctx = caller_ctx_or_tombstone(&workspaces, "dead-uid")
            .expect("tombstoned caller still allowed for reads");
        assert!(ctx.is_tombstone);
        assert_eq!(ctx.workspace_index, Some(0));
    }

    #[test]
    fn read_caller_returns_none_for_unknown() {
        let workspaces: Vec<Workspace> = vec![];
        assert!(caller_ctx_or_tombstone(&workspaces, "ghost").is_none());
    }

    // ----- Runtime-state helper consistency (#2) -----
    //
    // The unified `session_runtime_state` helper is the single source
    // of truth for `list_sessions.{state, idle}`,
    // `resolve_authorized_session.{state, idle}`, and `send_input`'s
    // exited-rejection. These tests pin its contract for every
    // interesting input combination — the three call sites all read
    // from this output, so structural agreement is automatic.

    #[test]
    fn runtime_state_live_idle_no_pending() {
        // The "agent is done, waiting for the next instruction" case.
        // live_ts builds with status: Idle, no pending — should report
        // ready + idle=true.
        let mut ts = live_ts("u", None);
        ts.transcript_id = Some("sid".into());
        let r = session_runtime_state(&ts);
        assert_eq!(r.kind, RuntimeStateKind::Ready);
        assert!(r.idle);
    }

    #[test]
    fn runtime_state_live_busy() {
        // The "agent is running, don't bother it" case.
        let mut ts = live_ts("u", None);
        ts.transcript_id = Some("sid".into());
        ts.status = SessionStatus::Running;
        let r = session_runtime_state(&ts);
        assert_eq!(r.kind, RuntimeStateKind::Ready);
        assert!(!r.idle, "Running status must not report idle");
    }

    #[test]
    fn runtime_state_live_with_pending_input() {
        // The "agent looks idle but we just queued a prompt" case.
        // Without this fix, the brief window between queue and
        // delivery would falsely report idle=true and an agent could
        // double-send.
        let mut ts = live_ts("u", None);
        ts.transcript_id = Some("sid".into());
        ts.pending_prompt = Some(crate::app::PendingWrite::wait_for_quiet(
            "hi".into(),
            true,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(60),
        ));
        let r = session_runtime_state(&ts);
        assert_eq!(r.kind, RuntimeStateKind::Ready);
        assert!(!r.idle, "queued input must keep idle false");
    }

    #[test]
    fn runtime_state_pending_with_pending_clear() {
        // `/clear` queued but not yet delivered — the new transcript
        // ID isn't bound either. Both pending_clear AND missing
        // transcript_id make this not idle.
        let mut ts = live_ts("u", None);
        ts.transcript_id = None;
        ts.pending_clear = Some(crate::app::PendingWrite::wait_for_quiet(
            "/clear".into(),
            true,
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(2),
            std::time::Duration::from_secs(60),
        ));
        let r = session_runtime_state(&ts);
        assert_eq!(r.kind, RuntimeStateKind::Pending);
        assert!(!r.idle);
    }

    #[test]
    fn runtime_state_pending_no_transcript_yet() {
        // Fresh-spawn: PTY is up but the detector hasn't bound a
        // transcript file yet. Idle in the SessionStatus sense, but
        // not bound → state is "pending".
        let mut ts = live_ts("u", None);
        ts.transcript_id = None;
        let r = session_runtime_state(&ts);
        assert_eq!(r.kind, RuntimeStateKind::Pending);
    }

    #[test]
    fn runtime_state_exited_row_still_present() {
        // The window between PTY exit and the close path tombstoning
        // the row. Pre-fix, `resolve_authorized_session` would say
        // "ready" (it ignored `session.exited`), `list_sessions` would
        // say "exited", and `send_input` would reject with `Conflict`.
        // Now all three agree: exited.
        let mut ts = live_ts("u", None);
        ts.transcript_id = Some("sid".into());
        ts.session.exited = true;
        let r = session_runtime_state(&ts);
        assert_eq!(r.kind, RuntimeStateKind::Exited);
        assert!(!r.idle, "exited sessions are never idle");
    }

    #[test]
    fn runtime_state_wire_strings_match_expected() {
        assert_eq!(RuntimeStateKind::Ready.as_wire(), "ready");
        assert_eq!(RuntimeStateKind::Pending.as_wire(), "pending");
        assert_eq!(RuntimeStateKind::Exited.as_wire(), "exited");
    }

    // ----- Session-type wire normalization (#3) -----

    #[test]
    fn public_session_type_normalizes_claude() {
        assert_eq!(public_session_type("claude"), "claude-code");
    }

    #[test]
    fn public_session_type_passes_through_codex_and_bash() {
        assert_eq!(public_session_type("codex"), "codex");
        assert_eq!(public_session_type("bash"), "bash");
    }

    #[test]
    fn list_to_start_round_trip_compatible() {
        // Regression: list_sessions used to emit "claude" but
        // start_session only accepts "claude-code". A copy-the-type
        // round-trip would fail. Now list_sessions normalizes via
        // `public_session_type`, and the value it emits is exactly
        // what start_session's enum check accepts.
        let listed_type = public_session_type("claude"); // what list_sessions returns
        // Mirror start_session's accepted-types check:
        assert!(matches!(listed_type, "claude-code" | "codex"));
    }

    #[test]
    fn kill_session_refuses_tombstoned_caller() {
        // End-to-end-ish: the `kill_session` handler routes through
        // `live_caller_ctx`. A tombstoned caller trying to kill a live
        // sibling must get `Unauthorized` with `caller_exited`,
        // independent of whether the target is in scope.
        //
        // We can't build a full App here, but `live_caller_ctx` is the
        // single gate every mutating handler uses — proving its
        // contract proves the gate.
        let ws = workspace_with(
            vec![live_ts("target-uid", None)],
            vec![make_tombstone("dead-caller")],
        );
        let workspaces = vec![ws];
        let err = live_caller_ctx(&workspaces, "dead-caller").expect_err("must reject");
        assert_eq!(err.0, ErrorCode::Unauthorized);
    }
}
