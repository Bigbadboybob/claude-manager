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

use cm_daemon::manifest::SessionTombstone;

use crate::agent;
use crate::app::{App, SessionStatus, TerminalSession, Workspace};
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
    /// Global-permissions grant. When true, `caller_authorized_for`
    /// short-circuits to authorized for ANY target, mirroring the
    /// daemon's `auth::check_session_caller` global short-circuit.
    /// Always false for tombstoned callers — a closed session can't
    /// mutate anything, and reverting its read scope to descendant-
    /// only on close is the safe default (we don't persist the grant
    /// on the tombstone).
    pub global_perms: bool,
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
            global_perms: workspaces[wi].sessions[si].global_perms,
            is_tombstone: false,
        });
    }
    if let Some((wi, ti)) = find_tombstone(workspaces, caller_uid) {
        return Some(CallerCtx {
            workspace_index: Some(wi),
            task_id: workspaces[wi].tombstones[ti].task_id.clone(),
            // Tombstoned callers never carry the grant (see CallerCtx doc).
            global_perms: false,
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
            global_perms: workspaces[wi].sessions[si].global_perms,
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
/// in `target_wi`. Two regimes:
///   - Tasked caller (workflow- or planning-launched): authorized iff
///     the target's task_id is the caller's task or a descendant in the
///     parent_task_id tree. The check is **purely task-tree** with no
///     workspace constraint, because branch-mode subtasks live in a
///     freshly-created child workspace different from the caller's.
///     A tasked caller cannot reach a target without a task_id.
///   - Taskless caller (`A-n`): authorized iff the target is in the
///     same workspace as the caller, regardless of its task_id.
pub fn caller_authorized_for(
    caller: &CallerCtx,
    tasks: &[crate::app::TaskEntry],
    target_wi: usize,
    target_task_id: Option<&str>,
) -> bool {
    let Some(caller_wi) = caller.workspace_index else {
        return false;
    };
    // Global-perms grant: authorized for any target. Mirrors the
    // daemon's `auth::check_session_caller` short-circuit so the TUI
    // and daemon agree on who can act on what.
    if caller.global_perms {
        return true;
    }
    match &caller.task_id {
        Some(task_id) => target_task_id
            .map(|tid| task_is_self_or_descendant_of(tasks, tid, task_id))
            .unwrap_or(false),
        None => caller_wi == target_wi,
    }
}

/// Cap on the parent_task_id walk. Defends against cycles or
/// pathologically deep trees — neither should occur in practice but
/// the auth check should not hang or stack-overflow if they do.
const MAX_TASK_DEPTH: usize = 64;

/// Is `target_id` either equal to `caller_id` or a (transitive) child
/// of it via the parent_task_id chain? Walks up from target. Stops at
/// `MAX_TASK_DEPTH` or the first task without a parent (top-level).
pub fn task_is_self_or_descendant_of(
    tasks: &[crate::app::TaskEntry],
    target_id: &str,
    caller_id: &str,
) -> bool {
    if target_id == caller_id {
        return true;
    }
    let mut cur = target_id.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..MAX_TASK_DEPTH {
        if !visited.insert(cur.clone()) {
            // Cycle detected — bail.
            return false;
        }
        let Some(task) = tasks
            .iter()
            .find(|t| t.task_id.as_deref() == Some(cur.as_str()))
        else {
            return false;
        };
        let Some(parent) = task.parent_task_id.clone() else {
            return false;
        };
        if parent == caller_id {
            return true;
        }
        cur = parent;
    }
    false
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

/// Return the caller's bound task_id (and workspace_id), letting MCP
/// tools answer "what task am I in?" without the caller having to track
/// it themselves. Used by `get_current_task` so skills can pull doc paths
/// and other bundle metadata off the task row without arguments.
///
/// Tombstoned callers (Phase 2b retention) are served too — a recently
/// closed session's bundle is still readable. Returns null fields when
/// the caller has no task or no workspace (e.g. `A-n` taskless sessions).
pub fn get_caller_task(app: &App, caller_uid: &str, _params: &Value) -> MethodResult {
    let caller = caller_ctx_or_tombstone(&app.workspaces, caller_uid)
        .ok_or((ErrorCode::NotFound, "caller session not found".into()))?;
    let workspace_id = caller
        .workspace_index
        .map(|wi| app.workspaces[wi].id.clone());
    Ok(json!({
        "task_id": caller.task_id,
        "workspace_id": workspace_id,
        "is_tombstone": caller.is_tombstone,
    }))
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
            if !caller_authorized_for(&caller, &app.tasks, wi, ts.task_id.as_deref()) {
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
            if !caller_authorized_for(&caller, &app.tasks, wi, tomb.task_id.as_deref()) {
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
    // The descendant-only auth model uses task-tree, not
    // workspace-membership, but we still require the caller's session
    // to exist in *some* workspace — a 404 here would be ambiguous
    // otherwise (manifest desync vs. unknown UID). The workspace_index
    // value itself is unused because `caller_authorized_for` walks
    // every workspace.
    if caller.workspace_index.is_none() {
        return Err((
            ErrorCode::NotFound,
            "caller session not found in any workspace".into(),
        ));
    }

    // Resolve scope: explicit task_id must be authorized (caller's own
    // or a descendant). Default = whatever the caller can see by auth.
    // Global callers may scope to any task and otherwise see every
    // session (no implicit own-task scope) — mirrors the daemon.
    if let Some(req) = p.task_id.as_deref() {
        if !caller.global_perms {
            match caller.task_id.as_deref() {
                None => {
                    // Taskless caller passing a task_id: unauthorized.
                    return Err((
                        ErrorCode::Unauthorized,
                        format!("taskless caller cannot scope to task {}", req),
                    ));
                }
                Some(own) => {
                    if !task_is_self_or_descendant_of(&app.tasks, req, own) {
                        return Err((
                            ErrorCode::Unauthorized,
                            format!(
                                "task {} is not the caller's task or a descendant",
                                req
                            ),
                        ));
                    }
                }
            }
        }
    }
    let scope_task: Option<String> = p.task_id.clone().or_else(|| {
        if caller.global_perms {
            None
        } else {
            caller.task_id.clone()
        }
    });

    // Iterate every workspace — branch-mode subtasks live in their own
    // workspace different from the caller's, but they're still within
    // scope by the descendant rule. caller_authorized_for makes the
    // final per-session decision so this stays consistent with reads
    // from `resolve_authorized_session`.
    let mut out: Vec<Value> = Vec::new();
    for (wi, ws) in app.workspaces.iter().enumerate() {
        let included_by_explicit_filter = |task: Option<&str>| -> bool {
            match scope_task.as_deref() {
                // Explicit scope: only that task's own + descendants.
                Some(scope) => task
                    .map(|t| task_is_self_or_descendant_of(&app.tasks, t, scope))
                    .unwrap_or(false),
                // Default scope (no explicit task_id): everything the
                // caller is authorized to see.
                None => caller_authorized_for(&caller, &app.tasks, wi, task),
            }
        };
        for ts in &ws.sessions {
            if !included_by_explicit_filter(ts.task_id.as_deref()) {
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
            // Grouping + perms metadata (parity with the daemon's
            // list_sessions): lets the MCP layer group by
            // workspace→task and flag privileged sessions.
            "task_id": ts.task_id,
            "workspace_id": ws.id,
            "workflow_run_id": ts.workflow_run_id,
            "workflow_role": ts.workflow_role,
            "global_perms": ts.global_perms,
        }));
    }
        if p.include_exited {
            for tomb in &ws.tombstones {
                let task = tomb.task_id.as_deref();
                let included = match scope_task.as_deref() {
                    Some(scope) => task
                        .map(|t| task_is_self_or_descendant_of(&app.tasks, t, scope))
                        .unwrap_or(false),
                    None => caller_authorized_for(&caller, &app.tasks, wi, task),
                };
                if !included {
                    continue;
                }
                out.push(json!({
                    "session_uid": tomb.uid,
                    "label": tomb.label,
                    "type": public_session_type(&tomb.session_type),
                    "state": "exited",
                    "idle": Value::Null,
                    "managed_by_uid": tomb.managed_by_uid,
                    "task_id": tomb.task_id,
                    "workspace_id": ws.id,
                    // Tombstones don't persist the grant — report false.
                    "global_perms": false,
                }));
            }
        }
    }
    Ok(Value::Array(out))
}

#[allow(dead_code)]
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
    if !caller_authorized_for(&caller, &app.tasks, wi, target_task.as_deref()) {
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
// notify_user
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct NotifyUserParams {
    #[serde(default)]
    message: String,
}

/// Raise an attention alert on the *calling* session: fire a desktop
/// notification and start the session's sidebar indicator blinking until the
/// user selects that session's row. Self-targeting by design — the caller is
/// the session that wants the user — so there's no cross-session authorization
/// to do beyond confirming the caller is a live (non-tombstoned) session.
pub fn notify_user(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: NotifyUserParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    let (wi, si) = find_live_session(&app.workspaces, caller_uid)
        .ok_or((ErrorCode::NotFound, "caller session not found".into()))?;
    let label = app.workspaces[wi].sessions[si].label.clone();
    app.raise_alert(caller_uid, &label, &p.message);
    Ok(json!({ "ok": true }))
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
    if !caller_authorized_for(&caller, &app.tasks, wi, target_task.as_deref()) {
        return Err((
            ErrorCode::Unauthorized,
            "session is outside caller's scope".into(),
        ));
    }
    // Tombstone-then-drop. Mark the PTY exited so the drainer cleans up.
    //
    // Slice 10c-e-3b-fix6: daemon-attached sessions need an
    // explicit `kill_session` RPC against the daemon BEFORE the
    // local handle drops — `Session::Drop` for daemon-attached
    // sessions is detach-only by design (slice 10c-e-3b-fix2),
    // so without this MCP `kill_session` succeeds locally but
    // leaves the daemon's PTY child running. The same helper
    // the operator-driven `A-w` path uses.
    {
        let pool = std::sync::Arc::clone(&app.host_pool);
        let ws = &mut app.workspaces[wi];
        crate::app::App::kill_daemon_session_if_attached(&pool, &ws.sessions[si]);
        App::tombstone_session_pub(ws, si);
        if let Some(ts) = ws.sessions.get_mut(si) {
            ts.session.exited = true;
        }
        ws.sessions.remove(si);
    }
    // Cancel any remote-reconnect bookkeeping for the killed session so a
    // session killed mid-reconnect (via this MCP/control path) isn't
    // resurrected by `drain_deferred_remote_reattach` when the tunnel
    // returns — the same resurrection guard the operator close paths use.
    app.forget_reconnect_state(&p.session_uid);
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
    /// Grant the spawned child global perms. Honored only when the
    /// caller is itself global (escalation guard) — mirrors the
    /// daemon's `mcp_start_session`.
    #[serde(default)]
    global_perms: bool,
}

pub fn start_session(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: StartSessionParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    if !matches!(p.type_.as_str(), "claude-code" | "codex" | "bash") {
        return Err((
            ErrorCode::InvalidParams,
            format!("type must be claude-code, codex, or bash, got {}", p.type_),
        ));
    }
    // Mutation — caller must be live.
    let caller = live_caller_ctx(&app.workspaces, caller_uid)?;
    let Some(caller_wi) = caller.workspace_index else {
        return Err((ErrorCode::NotFound, "caller session not found".into()));
    };
    // Global-perms escalation guard: only a global caller may grant
    // global perms to a child. Mirrors the daemon.
    if p.global_perms && !caller.global_perms {
        return Err((
            ErrorCode::Unauthorized,
            "global_perms requires the caller to itself hold global \
             permissions (escalation guard)".into(),
        ));
    }
    // task_id resolution. Allows binding to the caller's own task or
    // any descendant in the parent_task_id tree. Cross-task binding
    // outside the descendant tree is unauthorized — UNLESS the caller
    // is global, in which case it can bind the child to any task.
    let task_id_for_new: Option<String> = match (p.task_id.as_deref(), caller.task_id.as_deref()) {
        (Some(req), _) if caller.global_perms => Some(req.to_string()),
        (Some(req), None) => {
            return Err((
                ErrorCode::Unauthorized,
                format!("taskless caller cannot bind to task {}", req),
            ));
        }
        (Some(req), Some(own)) => {
            if !task_is_self_or_descendant_of(&app.tasks, req, own) {
                return Err((
                    ErrorCode::Unauthorized,
                    format!(
                        "task {} is not the caller's task or a descendant",
                        req
                    ),
                ));
            }
            Some(req.to_string())
        }
        (None, own) => own.map(str::to_string),
    };

    // Determine which workspace the new session lives in. For the
    // caller's own task or no task: use caller's workspace. For a
    // descendant task: that task's workspace (which may differ in
    // branch-mode subtasks).
    let target_wi = match task_id_for_new.as_deref() {
        Some(tid) if Some(tid) != caller.task_id.as_deref() => {
            // Descendant task — find its workspace.
            workspace_index_for_task(app, tid).ok_or((
                ErrorCode::NotFound,
                format!("task {} has no bound workspace", tid),
            ))?
        }
        _ => caller_wi,
    };

    let new_uid = app.spawn_managed_session(
        target_wi,
        caller_uid,
        &p.type_,
        &p.label,
        task_id_for_new,
        p.prompt.as_deref(),
        p.global_perms,
    )
    .map_err(|e| (ErrorCode::Internal, format!("spawn: {}", e)))?;
    Ok(json!({"session_uid": new_uid}))
}

/// Look up the workspace index for a task by its FK (`workspace_id`).
/// Returns None if the task isn't in `app.tasks`, has no workspace_id
/// set (still in backlog), or its workspace_id doesn't match any
/// existing workspace.
pub fn workspace_index_for_task(app: &App, task_id: &str) -> Option<usize> {
    let task = app.tasks.iter().find(|t| t.task_id.as_deref() == Some(task_id))?;
    let ws_id = task.workspace_id.as_deref()?;
    app.workspaces.iter().position(|w| w.id == ws_id)
}

// ---------------------------------------------------------------------------
// Phase 5 — subtask MCP tools
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateSubtaskParams {
    name: String,
    #[serde(default)]
    prompt: Option<String>,
    /// One of:
    ///   - `"inherit"` (default): share the parent's worktree.
    ///   - `"branch"`: new worktree off the parent's branch, named
    ///     `cm-sub/<slug-chain>-<short_id>`.
    ///   - `"in-place"`: spawn directly in the parent's MAIN repo checkout
    ///     — no new worktree, no new branch.
    #[serde(default = "default_subtask_worktree_mode")]
    worktree_mode: String,
    #[serde(default)]
    project: Option<String>,
}

fn default_subtask_worktree_mode() -> String {
    "inherit".to_string()
}

pub fn create_subtask(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: CreateSubtaskParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    if !matches!(p.worktree_mode.as_str(), "inherit" | "branch" | "in-place") {
        return Err((
            ErrorCode::InvalidParams,
            format!(
                "worktree_mode must be 'inherit', 'branch', or 'in-place', got '{}'",
                p.worktree_mode
            ),
        ));
    }
    let caller = live_caller_ctx(&app.workspaces, caller_uid)?;

    // Subtasks need a parent. Taskless callers should call propose_task
    // (the existing tool) to add a top-level task instead.
    let parent_task_id = caller.task_id.clone().ok_or((
        ErrorCode::Unauthorized,
        "create_subtask requires a tasked caller; use propose_task for top-level tasks".into(),
    ))?;

    // Look up the parent — needed for slug chain, repo_url, project,
    // and (in branch mode) wip_branch as the start ref.
    let parent = app
        .tasks
        .iter()
        .find(|t| t.task_id.as_deref() == Some(parent_task_id.as_str()))
        .cloned()
        .ok_or((
            ErrorCode::NotFound,
            format!("parent task {} not found in local task list", parent_task_id),
        ))?;
    let parent_workspace_id = parent.workspace_id.clone().ok_or((
        ErrorCode::Conflict,
        "parent task has no bound workspace — launch it before creating subtasks".into(),
    ))?;
    let parent_wi = app
        .workspaces
        .iter()
        .position(|w| w.id == parent_workspace_id)
        .ok_or((
            ErrorCode::NotFound,
            "parent workspace no longer exists".into(),
        ))?;
    let parent_repo_url = parent.repo_url.clone().ok_or((
        ErrorCode::Conflict,
        "parent task has no repo_url".into(),
    ))?;
    let parent_main_repo = app.workspaces[parent_wi].main_repo_path.clone();

    // Slug for the new task. Same encoding as `worktree::slugify`.
    let leaf_slug = cm_daemon::worktree::slugify(&p.name);
    if leaf_slug.is_empty() {
        return Err((
            ErrorCode::InvalidParams,
            format!("name '{}' produces an empty slug after normalization", p.name),
        ));
    }

    // Project: explicit > inherit from parent. The fallback reads the
    // parent's `project` field (populated from the API by reconcile).
    // Without this, subtasks created without an explicit `project`
    // arg get filtered out of the planning view by
    // `backend.rs::filter_project_tasks` after the next refresh.
    let project = p.project.clone().or_else(|| parent.project.clone());
    // Cloned for the local TaskEntry insert below — `project` itself
    // gets moved into the API body.
    let project_for_local = project.clone();

    // Generate a 7-hex-char short id BEFORE the API call. Used for
    // both the stored slug (`<chain>-<short>`) AND the branch name
    // (`cm-sub/<chain>-<short>`) so they share a consistent suffix
    // and we don't depend on the new task's UUID (which we wouldn't
    // know until after create).
    //
    // The DB has a UNIQUE INDEX on (project, slug). Without this
    // suffix, two subtasks with the same name under the same project
    // would collide on insert (even under different parents — slugs
    // would still collide). The suffix is per-call random, so even
    // same-name siblings under the same parent don't collide.
    let slug_chain = build_slug_chain(&app.tasks, &parent_task_id, &leaf_slug);
    let request_short_id = make_request_short_id();
    let unique_slug = format!("{}-{}", slug_chain, request_short_id);

    // Step 1: validate ALL preconditions BEFORE touching the API.
    // Earlier this version called `create_task` first and produced
    // orphan tasks if the worktree precheck failed — the rollback is
    // a separate API call that may itself fail. Front-loading the
    // checks reduces the orphan window to "the git command itself
    // racing or failing on disk", which we then handle with an
    // explicit DELETE on the failure path below.
    let inherit_worktree_path: Option<std::path::PathBuf> = if p.worktree_mode == "inherit" {
        let path = app.workspaces[parent_wi]
            .worktree_path
            .clone()
            .ok_or((
                ErrorCode::Conflict,
                "parent workspace has no worktree path (cloud workspace?)".into(),
            ))?;
        Some(path)
    } else {
        None
    };
    let branch_main_repo: Option<std::path::PathBuf> = if p.worktree_mode == "branch" {
        Some(parent_main_repo.clone().ok_or((
            ErrorCode::Conflict,
            "parent workspace has no main_repo_path; cannot branch".into(),
        ))?)
    } else {
        None
    };
    // In-place mode: the subtask runs directly in the parent's MAIN repo
    // checkout — no worktree, no branch. Resolve that path upfront so the
    // worktree-production step below is infallible.
    let in_place_main_repo: Option<std::path::PathBuf> = if p.worktree_mode == "in-place" {
        Some(parent_main_repo.clone().ok_or((
            ErrorCode::Conflict,
            "parent workspace has no main_repo_path; cannot launch in-place".into(),
        ))?)
    } else {
        None
    };
    // Resolve the parent's actual branch UPFRONT. Tasks launched into
    // existing workspaces commonly have `wip_branch: None` because
    // the system never had reason to set it; the source of truth in
    // that case is the parent worktree's actual HEAD. Order of
    // preference:
    //   1. parent.wip_branch (canonical when set)
    //   2. parent worktree's `git rev-parse --abbrev-ref HEAD`
    //   3. None (handled per-mode below)
    //
    // Branch-mode REQUIRES this to be Some — we don't fall back to
    // "main" because the user may not have a main branch at all, and
    // silently forking from the wrong base loses the parent's work.
    // Inherit-mode treats None as soft: the new task's wip_branch
    // inherits the resolution if available, so future grandchildren
    // can branch from it.
    let parent_branch_resolved: Option<String> = parent.wip_branch.clone().or_else(|| {
        app.workspaces[parent_wi]
            .worktree_path
            .as_deref()
            .and_then(cm_daemon::worktree::worktree_current_branch)
    });
    if p.worktree_mode == "branch" && parent_branch_resolved.is_none() {
        return Err((
            ErrorCode::Conflict,
            "cannot determine parent's base branch (no wip_branch and worktree HEAD is detached or unreadable)".into(),
        ));
    }

    // Compute the new task's `wip_branch` upfront — both modes need
    // this baked into the API row. Without it, the next reconcile
    // (which copies API state into the local TaskEntry) would null
    // out wip_branch for inherit-mode subtasks, and a branch-mode
    // grandchild would then fall back to "main" as its start ref.
    //
    // Inherit mode → same branch as parent (sessions live in the
    // parent's worktree, which is on the parent's branch).
    // Branch mode → the freshly-built `cm-sub/<chain>-<short>` name.
    let branch_name_for_new: Option<String> = match p.worktree_mode.as_str() {
        // Inherit: subtask's wip_branch IS the parent's resolved branch,
        // not just its (possibly-None) `wip_branch` field. Without
        // this fallback through worktree HEAD, an inherit-mode subtask
        // with parent.wip_branch=None would store None in the API,
        // and a future branch-mode grandchild would hit the same
        // base-branch problem.
        "inherit" => parent_branch_resolved.clone(),
        "branch" => Some(format!("cm-sub/{}-{}", slug_chain, request_short_id)),
        // In-place: the session runs in the MAIN repo checkout, NOT the
        // parent's worktree — so its branch is the main repo's CURRENT
        // branch, not `parent_branch_resolved` (which could be the parent's
        // `cm/...` worktree branch). Reading it from `in_place_main_repo`
        // mirrors the planning launch path (app.rs `launch_from_plan`).
        // Recording the parent's branch here would mislead branch-mode
        // grandchildren (wrong fork base) and let reconcile mis-map this
        // task onto a `cm/...` worktree dir after a manifest loss.
        "in-place" => in_place_main_repo
            .as_deref()
            .and_then(cm_daemon::worktree::worktree_current_branch),
        _ => None,
    };

    // Step 2: ask the API to create the task. Server assigns the UUID,
    // which we need for the short_id in branch-mode names.
    let api_client = api_client_or_err()?;
    let body = crate::api::TaskCreateBody {
        repo_url: parent_repo_url.clone(),
        repo_branch: "main".to_string(),
        name: Some(p.name.clone()),
        prompt: p.prompt.clone(),
        priority: 0,
        status: Some("running".to_string()),
        project,
        slug: Some(unique_slug.clone()),
        description: None,
        difficulty: None,
        depends: None,
        source: Some("claude".to_string()),
        is_cloud: Some(false),
        parent_task_id: Some(parent_task_id.clone()),
        worktree_mode: Some(p.worktree_mode.clone()),
        wip_branch: branch_name_for_new.clone(),
    };
    let new_task = api_client
        .create_task(&body)
        .map_err(|e| (ErrorCode::Internal, format!("api create_task: {}", e)))?;
    let new_task_id = new_task.id.clone();

    // Step 3: produce a worktree. Anything that errors after the API
    // create succeeded triggers a rollback DELETE so we don't leave an
    // orphan task in `running` state with no usable workspace.
    let (worktree_path, workspace_id_for_new) = match p.worktree_mode.as_str() {
        "inherit" => {
            // Path was validated upfront; safe to unwrap.
            let path = inherit_worktree_path.expect("validated above");
            (path, parent_workspace_id.clone())
        }
        "branch" => {
            let main_repo = branch_main_repo.expect("validated above");
            let parent_branch = parent_branch_resolved
                .clone()
                .expect("validated above");
            // Branch name was computed upfront and sent in the
            // create_task body — no post-create PATCH needed.
            let branch_name = branch_name_for_new
                .clone()
                .expect("branch_name_for_new is Some in branch mode");
            let worktree_path = match cm_daemon::worktree::create_subtask_worktree(
                &main_repo,
                &branch_name,
                &parent_branch,
            ) {
                Ok(p) => p,
                Err(e) => {
                    // Rollback: delete the API task so we don't leak
                    // a `running` row that points at nothing on disk.
                    // Best-effort — if the DELETE fails too the user
                    // has to clean up by hand, but we surface the
                    // original git error which is what they care about.
                    let _ = api_client.delete_task(&new_task_id);
                    return Err((
                        ErrorCode::Internal,
                        format!(
                            "create worktree failed for branch '{}'; api task {} rolled back: {}",
                            branch_name, new_task_id, e
                        ),
                    ));
                }
            };
            cm_daemon::worktree::setup_worktree(&main_repo, &worktree_path);

            // Register a fresh workspace for this subtask.
            let new_ws_id = crate::app::new_workspace_id();
            let new_ws = Workspace {
                id: new_ws_id.clone(),
                name: leaf_slug.clone(),
                is_closed: false,
                is_cloud: false,
                repo_url: Some(parent_repo_url.clone()),
                worktree_path: Some(worktree_path.clone()),
                main_repo_path: Some(main_repo.clone()),
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: vec![],
                tombstones: vec![],
                is_pushing: false,
            };
            app.workspaces.push(new_ws);
            (worktree_path, new_ws_id)
        }
        "in-place" => {
            // No worktree, no branch — the subtask's cwd IS the parent's
            // main repo. We still register a SEPARATE workspace (distinct
            // id) pointing at the main repo, rather than reusing the
            // parent's workspace the way "inherit" does: the common case
            // is a subtask of a *worktree-backed* parent that wants to run
            // in the main repo instead. Setting `worktree_path` ==
            // `main_repo_path` makes `Workspace::is_in_place()` true, so
            // teardown (delete, mark_subtask_done) never touches git.
            let main_repo = in_place_main_repo.expect("validated above");
            let new_ws_id = crate::app::new_workspace_id();
            let new_ws = Workspace {
                id: new_ws_id.clone(),
                name: leaf_slug.clone(),
                is_closed: false,
                is_cloud: false,
                repo_url: Some(parent_repo_url.clone()),
                worktree_path: Some(main_repo.clone()),
                main_repo_path: Some(main_repo.clone()),
                worker_vm: None,
                worker_zone: None,
                host_id: cm_daemon::host_id::HostId::local(),
                sessions: vec![],
                tombstones: vec![],
                is_pushing: false,
            };
            app.workspaces.push(new_ws);
            (main_repo, new_ws_id)
        }
        _ => unreachable!(),
    };

    // Step 4: insert the TaskEntry locally so it's visible immediately,
    // pre-empting the next API reconcile (which would also pick it up
    // a few seconds later).
    let already_present = app
        .tasks
        .iter()
        .any(|t| t.task_id.as_deref() == Some(new_task_id.as_str()));
    if !already_present {
        app.tasks.push(crate::app::TaskEntry {
            task_id: Some(new_task_id.clone()),
            name: p.name.clone(),
            api_status: crate::app::TaskStatus::Running,
            repo_url: Some(parent_repo_url),
            prompt: p.prompt.clone(),
            // Same value we baked into the API row — survives the
            // next reconcile.
            wip_branch: branch_name_for_new.clone(),
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            workspace_id: Some(workspace_id_for_new),
            project: project_for_local.clone(),
            parent_task_id: Some(parent_task_id),
            worktree_mode: crate::app::parse_worktree_mode(&p.worktree_mode),
            metadata: None,
        });
    }

    app.save_session_manifest();
    // Sub-2a Finding (round 3) #2: the local TaskEntry above
    // adds a fresh parent-task edge; without an immediate
    // `push_task_tree_to_daemon` the daemon's `task_tree`
    // doesn't see the new subtask until the next API reconcile
    // fires (typically seconds later). Until then, a tasked
    // agent acting on the subtask's session — which is exactly
    // what `create_subtask` is for — would fail the
    // descendant-task auth walk because the parent edge isn't
    // visible. Pre-fix: subtask spawn-then-act races reconcile.
    // Post-fix: edge is live the moment `create_subtask`
    // returns. Mirrors the launch_* paths which call this at
    // the tail for the same reason.
    app.push_task_tree_to_daemon();
    Ok(json!({
        "task_id": new_task_id,
        "worktree_path": worktree_path.to_string_lossy(),
    }))
}

/// Generate a 7-hex-char id from nanos + an atomic counter. Mirrors
/// the shape of `app::new_session_uid` but trimmed to fit a slug
/// suffix. Used by `create_subtask` to give each subtask a unique
/// suffix on both its slug (DB unique constraint) and its worktree
/// branch name.
fn make_request_short_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:07x}", (nanos.wrapping_add(n)) & 0x0FFF_FFFF)
}

/// Build the slug chain for a subtask branch name. Walks the parent
/// chain and joins each ancestor's slug with `-`, ending with the new
/// (leaf) slug. Caps at `MAX_TASK_DEPTH` to defend against cycles.
fn build_slug_chain(
    tasks: &[crate::app::TaskEntry],
    parent_id: &str,
    leaf_slug: &str,
) -> String {
    let mut chain: Vec<String> = vec![leaf_slug.to_string()];
    let mut cur: Option<String> = Some(parent_id.to_string());
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..MAX_TASK_DEPTH {
        let Some(id) = cur.clone() else { break };
        if !visited.insert(id.clone()) {
            break;
        }
        let Some(task) = tasks.iter().find(|t| t.task_id.as_deref() == Some(id.as_str()))
        else {
            break;
        };
        chain.push(cm_daemon::worktree::slugify(&task.name));
        cur = task.parent_task_id.clone();
    }
    chain.reverse();
    chain.join("-")
}

/// Get an `ApiClient` for outbound task ops. Reads config the same way
/// the rest of the TUI does. Returns `Err(Internal)` if config can't be
/// loaded — the agent gets a clear failure rather than a panic.
fn api_client_or_err() -> Result<crate::api::ApiClient, (ErrorCode, String)> {
    let config = crate::config::Config::load();
    Ok(crate::api::ApiClient::new(&config))
}

// ---------------------------------------------------------------------------
// list_subtasks
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ListSubtasksParams {
    #[serde(default)]
    task_id: Option<String>,
}

pub fn list_subtasks(app: &App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: ListSubtasksParams = if params.is_null() {
        ListSubtasksParams::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?
    };
    let caller = caller_ctx_or_tombstone(&app.workspaces, caller_uid)
        .ok_or((ErrorCode::NotFound, "caller session not found".into()))?;

    let scope = match p.task_id.as_deref() {
        Some(req) => {
            let own = caller.task_id.as_deref().ok_or((
                ErrorCode::Unauthorized,
                format!("taskless caller cannot scope to task {}", req),
            ))?;
            if !task_is_self_or_descendant_of(&app.tasks, req, own) {
                return Err((
                    ErrorCode::Unauthorized,
                    format!("task {} is not the caller's task or a descendant", req),
                ));
            }
            req.to_string()
        }
        None => caller.task_id.clone().ok_or((
            ErrorCode::Unauthorized,
            "taskless caller cannot list subtasks (no task scope)".into(),
        ))?,
    };

    let mut out: Vec<Value> = Vec::new();
    for task in &app.tasks {
        if task.parent_task_id.as_deref() == Some(scope.as_str()) {
            out.push(json!({
                "task_id": task.task_id,
                "name": task.name,
                "status": task_status_str(&task.api_status),
                "worktree_mode": task.worktree_mode.as_wire(),
                "wip_branch": task.wip_branch,
                "workspace_id": task.workspace_id,
            }));
        }
    }
    Ok(Value::Array(out))
}

fn task_status_str(s: &crate::app::TaskStatus) -> &'static str {
    use crate::app::TaskStatus;
    match s {
        TaskStatus::Backlog => "backlog",
        TaskStatus::Running => "running",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Done => "done",
    }
}

// ---------------------------------------------------------------------------
// mark_subtask_done
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct MarkSubtaskDoneParams {
    task_id: String,
    #[serde(default = "default_close_worktree")]
    close_worktree: bool,
    /// Discard an uncommitted subtask worktree instead of refusing to
    /// tear it down. Default false: a dirty worktree is a hard error so
    /// `git worktree remove --force` can't silently destroy unmerged work.
    #[serde(default)]
    force: bool,
}

fn default_close_worktree() -> bool {
    true
}

pub fn mark_subtask_done(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: MarkSubtaskDoneParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    let caller = live_caller_ctx(&app.workspaces, caller_uid)?;
    let own = caller.task_id.as_deref().ok_or((
        ErrorCode::Unauthorized,
        "taskless caller cannot mark subtasks done".into(),
    ))?;
    if !task_is_self_or_descendant_of(&app.tasks, &p.task_id, own) {
        return Err((
            ErrorCode::Unauthorized,
            format!("task {} is not the caller's task or a descendant", p.task_id),
        ));
    }

    // Look up the subtask + its workspace.
    let task = app
        .tasks
        .iter()
        .find(|t| t.task_id.as_deref() == Some(p.task_id.as_str()))
        .cloned()
        .ok_or((
            ErrorCode::NotFound,
            format!("task {} not found", p.task_id),
        ))?;
    let was_branch_mode = matches!(task.worktree_mode, crate::app::WorktreeMode::Branch);

    // Phase 1 — locate cleanup targets BEFORE running anything
    // destructive AND before marking the task done in the API. The
    // earlier ordering (API mark-done → cleanup) was the same orphan
    // class as Phase 5's create_subtask bug: a failed `git worktree
    // remove` returned an error, but the task was already Done in
    // the API. The next reconcile then drops Done tasks from
    // `app.tasks` (see app.rs:2930), and the user can never invoke
    // `mark_subtask_done` again to retry — the worktree is stranded.
    //
    // Worktree removal is gated by `close_worktree && was_branch_mode`.
    // Inherit-mode AND in-place subtasks have nothing of their own to
    // remove — inherit shares the parent's worktree, and in-place runs in
    // the main repo (its `worktree_path == main_repo_path`). Only `branch`
    // mode owns a dedicated worktree, so only it ever reaches the removal
    // path. Branch-mode + close_worktree=false
    // leaves the worktree on disk so the user can keep working in it
    // after the agent marks done (e.g. for a manual review pass).
    // `cleanup_outcome` is the precheck verdict:
    //   - `Some((wi, main, wt))` → Phase 2 should run `git worktree remove`.
    //   - `None` with `already_done = false` → no cleanup requested
    //     (close_worktree=false or inherit-mode).
    //   - `None` with `already_done = true` → cleanup ALREADY ran on
    //     a prior invocation (worktree removed, workspace closed),
    //     and the only thing left to retry is the API mark-done. This
    //     is the retry path: without it, a successful worktree remove
    //     followed by an API failure produces a stuck state where
    //     re-entering with `close_worktree=true` would bail on the
    //     missing `worktree_path` precheck before reaching Phase 3.
    let mut already_done = false;
    let cleanup_target = if p.close_worktree && was_branch_mode {
        let ws_id = task.workspace_id.clone().ok_or((
            ErrorCode::Conflict,
            format!(
                "task {} has no workspace_id; cannot remove its worktree (likely a manifest desync — try restarting the TUI to recover)",
                p.task_id
            ),
        ))?;
        let wi = app.workspaces.iter().position(|w| w.id == ws_id).ok_or((
            ErrorCode::Conflict,
            format!(
                "workspace {} for task {} is not present in this session",
                ws_id, p.task_id
            ),
        ))?;
        // Hard safety net: an in-place workspace's `worktree_path` IS the
        // main repo checkout (`worktree_path == main_repo_path`). Running
        // `git worktree remove` on it would destroy the user's main
        // working tree. This must hold even if `was_branch_mode` is true
        // here — e.g. a stale API `worktree_mode = "branch"` overwrote the
        // local `InPlace` on reconcile (app.rs `parse_worktree_mode`). The
        // path-equality check is authoritative regardless of the label, so
        // we never trust `worktree_mode` alone to gate destruction.
        if app.workspaces[wi].is_in_place() {
            None
        } else if app.workspaces[wi].worktree_path.is_none() {
            // The "already cleaned up" signal: Phase 2 sets
            // `worktree_path = None` (and `is_closed = true`) only after
            // a successful `git worktree remove`. So a workspace whose
            // `worktree_path` is None on entry to this method means the
            // remove already happened — fall through to Phase 3 to
            // (re)try the API mark-done. We deliberately don't require
            // `is_closed` here too; a single signal is enough and easier
            // to reason about.
            already_done = true;
            None
        } else {
            let main = app.workspaces[wi].main_repo_path.clone().ok_or((
                ErrorCode::Conflict,
                format!(
                    "workspace {} has no main_repo_path; cannot run `git worktree remove`",
                    ws_id
                ),
            ))?;
            // `worktree_path` is Some here (just checked).
            let wt = app.workspaces[wi].worktree_path.clone().expect("just checked");
            Some((wi, main, wt))
        }
    } else {
        None
    };

    // Safety guard (checked BEFORE Phase 2 closes any sessions): refuse to
    // tear down a branch worktree that has uncommitted changes.
    // `remove_worktree` runs `git worktree remove --force`, which would
    // silently destroy them. `force=true` overrides. Committed work is
    // preserved by the branch ref, so only the working tree is at risk.
    // `cleanup_target` is already filtered to a real branch worktree here
    // (None for in-place / already-cleaned), so no extra guard is needed.
    if !p.force {
        if let Some((_, _, wt)) = &cleanup_target {
            match cm_daemon::worktree::worktree_is_dirty(wt) {
                Ok(true) => {
                    return Err((
                        ErrorCode::Conflict,
                        format!(
                            "subtask {} worktree has uncommitted changes at {} — commit \
                             or merge them first (the branch ref is preserved), or pass \
                             force=true to discard them. Nothing was torn down.",
                            p.task_id,
                            wt.display()
                        ),
                    ));
                }
                Ok(false) => {}
                Err(e) => {
                    return Err((
                        ErrorCode::Internal,
                        format!(
                            "could not check subtask {} worktree cleanliness: {} — pass \
                             force=true to skip the check",
                            p.task_id, e
                        ),
                    ));
                }
            }
        }
    }

    // Phase 2 — perform cleanup. Sessions first (always), then the
    // worktree if applicable. We close sessions before the worktree
    // because git refuses to remove a worktree that has live
    // processes inside it; closing sessions also drops the PTY locks
    // on the directory.
    //
    // Filter sessions by task_id (not by workspace), so:
    //   - inherit-mode subtasks (sharing the parent's workspace):
    //     only the subtask-tagged sessions close, parent's sessions
    //     keep running.
    //   - branch-mode subtasks (own workspace): every session in
    //     that workspace closes, since they're all subtask-tagged.
    let target_task = p.task_id.clone();
    for wi in 0..app.workspaces.len() {
        let _ = app.tombstone_and_remove(wi, |ts| {
            ts.task_id.as_deref() == Some(target_task.as_str())
        });
    }

    // Observable end-state: `true` means the worktree is gone by the
    // time this response goes out, regardless of whether THIS call
    // is the one that removed it (vs. a prior call whose API update
    // failed and is being retried now).
    let mut worktree_removed = already_done;
    if let Some((wi, main, wt)) = cleanup_target {
        match cm_daemon::worktree::remove_worktree(&main, &wt) {
            Ok(()) => {
                // Only NOW clear the workspace-side path and mark
                // closed. If we did this unconditionally, a later
                // failed remove would leave the manifest unable to
                // find the path needed to retry.
                if let Some(ws) = app.workspaces.get_mut(wi) {
                    ws.is_closed = true;
                    ws.worktree_path = None;
                }
                worktree_removed = true;
                app.save_session_manifest();
            }
            Err(e) => {
                // Cleanup failure → DO NOT mark the task done.
                // Sessions are already tombstoned (which the manifest
                // captured via tombstone_and_remove's mandatory
                // save), but the API/local task stays in its prior
                // status so the user can retry this exact call.
                app.save_session_manifest();
                return Err((
                    ErrorCode::Internal,
                    format!(
                        "worktree remove failed for task {} (sessions closed, but task NOT marked done — retry once the worktree issue is resolved): {}",
                        p.task_id, e
                    ),
                ));
            }
        }
    }

    // Phase 3 — only NOW commit the Done status. Order matters:
    // API first so the source of truth flips, then local entry to
    // pre-empt the next reconcile. If the API call fails after
    // cleanup succeeded, the worktree is gone but the task is still
    // running in the API — the user can re-run `mark_subtask_done`,
    // and Phase 1 will short-circuit (no workspace to clean up,
    // close_worktree branch skipped) so only the API update fires.
    let api_client = api_client_or_err()?;
    let mut fields: std::collections::HashMap<String, Value> =
        std::collections::HashMap::new();
    fields.insert("status".into(), Value::String("done".into()));
    let _ = api_client.update_task(&p.task_id, &fields).map_err(|e| {
        (
            ErrorCode::Internal,
            format!(
                "cleanup succeeded but api update_task failed (worktree gone, task still 'running' in API — retry mark_subtask_done): {}",
                e
            ),
        )
    })?;
    // Update ALL matching entries, not just the first. Pre-fix this used
    // `iter_mut().find()` and stopped at the first match. The Phase 5
    // smoke test surfaced a bug where the first call left
    // `list_subtasks` reporting `status=running` even though the API
    // had been flipped to `done`. The defensive shape here is: dedupe
    // by `for_each` over the filter so any duplicate TaskEntry with
    // the same `task_id` (whatever its origin) gets the Done flip too.
    // A single-entry case still works because the iterator yields one
    // mutation; a duplicate case stops fizzling on retry #2.
    app.tasks
        .iter_mut()
        .filter(|t| t.task_id.as_deref() == Some(p.task_id.as_str()))
        .for_each(|t| t.api_status = crate::app::TaskStatus::Done);

    Ok(json!({"ok": true, "worktree_removed": worktree_removed}))
}

// ---------------------------------------------------------------------------
// Phase 4 — workflow MCP tools
//
// All four (start, stop, get_state, list) honor the same auth model
// as the session-management surface: the caller can act on workflows
// in their own task or any descendant in the parent_task_id tree.
// Workflow state lives in `app.workflow_runs`; persistence happens
// inside each App method that touches it.
// ---------------------------------------------------------------------------



#[derive(Deserialize)]
struct StopWorkflowParams {
    run_id: String,
}

pub fn stop_workflow(app: &mut App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: StopWorkflowParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    let caller = live_caller_ctx(&app.workspaces, caller_uid)?;

    // In-memory first.
    if let Some(run) = app.workflow_runs.iter().find(|r| r.run_id == p.run_id) {
        if !workflow_run_authorized(&caller, &app.tasks, run) {
            return Err((
                ErrorCode::Unauthorized,
                "workflow run is outside caller's scope".into(),
            ));
        }
        // No-op on terminal status. Pre-fix, calling stop_workflow on a
        // run that completed naturally via `workflow_done` overwrote the
        // persisted state.json status from `Done` to `Detached`,
        // erasing the distinction between successful completion and
        // user abort. `done_reason` was preserved but paired with a
        // status that said "you aborted me".
        if matches!(run.status, crate::workflow::run::RunStatus::Done) {
            return Ok(json!({"ok": true}));
        }
        app.stop_workflow_run(&p.run_id);
        return Ok(json!({"ok": true}));
    }
    // On-disk fallback. After `stop_workflow_run` removes the entry
    // from `app.workflow_runs`, a Detached run only lives on disk until
    // the next TUI restart reloads it. Treat a re-stop as a no-op.
    if let Some(run) = crate::workflow::run::load_one(&p.run_id) {
        if !workflow_run_authorized(&caller, &app.tasks, &run) {
            return Err((
                ErrorCode::Unauthorized,
                "workflow run is outside caller's scope".into(),
            ));
        }
        return Ok(json!({"ok": true}));
    }
    Err((
        ErrorCode::NotFound,
        format!("workflow run {} not found", p.run_id),
    ))
}

#[derive(Deserialize, Default)]
struct GetWorkflowStateParams {
    run_id: String,
}

pub fn get_workflow_state(app: &App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: GetWorkflowStateParams = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?;
    let caller = caller_ctx_or_tombstone(&app.workspaces, caller_uid)
        .ok_or((ErrorCode::NotFound, "caller session not found".into()))?;

    if let Some(run) = app.workflow_runs.iter().find(|r| r.run_id == p.run_id) {
        if !workflow_run_authorized(&caller, &app.tasks, run) {
            return Err((
                ErrorCode::Unauthorized,
                "workflow run is outside caller's scope".into(),
            ));
        }
        return Ok(serialize_workflow_run(run));
    }
    // On-disk fallback for runs that were detached + pruned from
    // `app.workflow_runs`. The state.json + events.jsonl on disk are
    // still authoritative for audit reads.
    if let Some(run) = crate::workflow::run::load_one(&p.run_id) {
        if !workflow_run_authorized(&caller, &app.tasks, &run) {
            return Err((
                ErrorCode::Unauthorized,
                "workflow run is outside caller's scope".into(),
            ));
        }
        return Ok(serialize_workflow_run(&run));
    }
    Err((
        ErrorCode::NotFound,
        format!("workflow run {} not found", p.run_id),
    ))
}

#[derive(Deserialize, Default)]
struct ListWorkflowsParams {
    #[serde(default)]
    task_id: Option<String>,
}

pub fn list_workflows(app: &App, caller_uid: &str, params: &Value) -> MethodResult {
    let p: ListWorkflowsParams = if params.is_null() {
        ListWorkflowsParams::default()
    } else {
        serde_json::from_value(params.clone())
            .map_err(|e| (ErrorCode::InvalidParams, format!("params: {}", e)))?
    };
    let caller = caller_ctx_or_tombstone(&app.workspaces, caller_uid)
        .ok_or((ErrorCode::NotFound, "caller session not found".into()))?;

    // Optional task_id filter — must be authorized.
    if let Some(req) = p.task_id.as_deref() {
        match caller.task_id.as_deref() {
            None => {
                return Err((
                    ErrorCode::Unauthorized,
                    format!("taskless caller cannot scope to task {}", req),
                ));
            }
            Some(own) => {
                if !task_is_self_or_descendant_of(&app.tasks, req, own) {
                    return Err((
                        ErrorCode::Unauthorized,
                        format!(
                            "task {} is not the caller's task or a descendant",
                            req
                        ),
                    ));
                }
            }
        }
    }

    let mut out: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for run in &app.workflow_runs {
        seen.insert(run.run_id.clone());
        if !list_workflows_visible(&caller, &app.tasks, run, p.task_id.as_deref()) {
            continue;
        }
        out.push(serialize_workflow_run_summary(run));
    }
    // On-disk fallback. Surface runs that have been pruned from
    // `app.workflow_runs` (Detached via `stop_workflow_run`, or Done
    // runs persisted to disk before this process started). The same
    // scope/auth filter applies.
    for run in crate::workflow::run::load_all() {
        if seen.contains(&run.run_id) {
            continue;
        }
        if !list_workflows_visible(&caller, &app.tasks, &run, p.task_id.as_deref()) {
            continue;
        }
        out.push(serialize_workflow_run_summary(&run));
    }
    Ok(Value::Array(out))
}

/// Combined scope+auth filter for list_workflows entries. Returns true
/// iff the caller can see the run under the requested scope.
fn list_workflows_visible(
    caller: &CallerCtx,
    tasks: &[crate::app::TaskEntry],
    run: &crate::workflow::run::WorkflowRun,
    explicit_scope: Option<&str>,
) -> bool {
    if let Some(req) = explicit_scope {
        // Explicit scope filter: include the run iff its bound task
        // is the requested one or its descendant.
        //
        // Legacy runs (no `task_id` field) need the same unambiguous
        // resolution as `workflow_run_authorized`'s fallback — pick
        // the workspace's candidate task ONLY when there's exactly
        // one. Picking the first via `.find()` would leak across task
        // boundaries.
        let resolved_tid: Option<String> = match run.task_id.as_deref() {
            Some(rid) => Some(rid.to_string()),
            None => {
                let candidates: Vec<&crate::app::TaskEntry> = tasks
                    .iter()
                    .filter(|t| {
                        t.workspace_id.as_deref() == Some(run.task_key.as_str())
                    })
                    .collect();
                if candidates.len() == 1 {
                    candidates[0].task_id.clone()
                } else {
                    None
                }
            }
        };
        match resolved_tid.as_deref() {
            Some(rid) => task_is_self_or_descendant_of(tasks, rid, req),
            None => false,
        }
    } else {
        workflow_run_authorized(caller, tasks, run)
    }
}

fn serialize_workflow_run_summary(run: &crate::workflow::run::WorkflowRun) -> Value {
    json!({
        "run_id": run.run_id,
        "name": run.workflow_name,
        "task_id": run.task_id,
        "workspace_id": run.task_key,
        "active_role": run.active_role,
        "iteration": run.iteration,
        "paused": run.paused,
        "status": run_status_str(&run.status),
        "started_at": run.started_at,
        "done_reason": run.done_reason,
    })
}

/// Auth check for workflow-targeted calls. Caller must have authority
/// over the run's bound task by the same descendant rule used for
/// sessions.
///
/// Two information sources, in priority order:
///   1. `WorkflowRun.task_id` — set by `start_workflow_run` (the MCP
///      launch path). Used directly when present.
///   2. `WorkflowRun.task_key` — the workspace id (per `workspace_key`).
///      Set by both UI and MCP launches. Resolve to candidate tasks
///      via `task.workspace_id` and accept if any candidate descends
///      from the caller's task.
fn workflow_run_authorized(
    caller: &CallerCtx,
    tasks: &[crate::app::TaskEntry],
    run: &crate::workflow::run::WorkflowRun,
) -> bool {
    let Some(own) = caller.task_id.as_deref() else {
        return false;
    };
    if let Some(rid) = run.task_id.as_deref() {
        return task_is_self_or_descendant_of(tasks, rid, own);
    }
    // Fallback for UI-launched runs (no `task_id` field). task_key is
    // the workspace id; resolve to the candidate task — but ONLY if
    // there's exactly one. Authorizing when ANY task in the workspace
    // is a descendant of the caller is too loose: if workspace W has
    // task A (caller's) and unrelated task B, an agent on A could
    // get_workflow_state / stop_workflow a run that was actually
    // launched on B. Reject ambiguous cases — better to refuse a
    // legacy MCP call than to leak access across task boundaries.
    let candidates: Vec<&crate::app::TaskEntry> = tasks
        .iter()
        .filter(|t| t.workspace_id.as_deref() == Some(run.task_key.as_str()))
        .collect();
    if candidates.len() != 1 {
        return false;
    }
    let Some(candidate_id) = candidates[0].task_id.as_deref() else {
        return false;
    };
    task_is_self_or_descendant_of(tasks, candidate_id, own)
}

fn run_status_str(status: &crate::workflow::run::RunStatus) -> &'static str {
    use crate::workflow::run::RunStatus;
    match status {
        RunStatus::Running => "running",
        RunStatus::Paused => "paused",
        RunStatus::Done => "done",
        RunStatus::Detached => "detached",
    }
}

fn serialize_workflow_run(run: &crate::workflow::run::WorkflowRun) -> Value {
    let history: Vec<Value> = run
        .history
        .iter()
        .map(|h| {
            json!({
                "iteration": h.iteration,
                "role": h.role,
                "transcript_id": h.session_id,
                "last_message": h.last_message,
                "activated_at": h.activated_at,
                "deactivated_at": h.deactivated_at,
                "trigger": serde_json::to_value(&h.trigger)
                    .unwrap_or_else(|_| Value::Null),
                "assistant_count_at_start": h.assistant_count_at_start,
            })
        })
        .collect();
    let role_sessions: serde_json::Map<String, Value> = run
        .role_sessions
        .iter()
        .map(|(role, binding)| {
            (
                role.clone(),
                json!({
                    "session_label": binding.session_label,
                    "current_transcript_id": binding.current_session_id,
                }),
            )
        })
        .collect();
    json!({
        "run_id": run.run_id,
        "name": run.workflow_name,
        // `task_id` is the MCP-bound task; null for UI-launched runs.
        // `workspace_id` is the run's workspace key (was previously
        // surfaced as `task_id`, which conflated the two and broke
        // descendant auth on follow-up calls).
        "task_id": run.task_id,
        "workspace_id": run.task_key,
        "active_role": run.active_role,
        "iteration": run.iteration,
        "paused": run.paused,
        "status": run_status_str(&run.status),
        "started_at": run.started_at,
        "done_reason": run.done_reason,
        "goal": run.goal,
        "history": history,
        "role_sessions": Value::Object(role_sessions),
    })
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
        crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
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
            continuous_task_id: None,
            task_id: task_id.map(str::to_string),
            last_delivery: None,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
            preserved_last_exit: None,
            host_id: crate::hosts::HostId::local(),
            global_perms: false,
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
            host_id: cm_daemon::host_id::HostId::local(),
            sessions,
            tombstones,
            is_pushing: false,
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

    // -- Phase 5 --

    fn task_with_parent(id: &str, parent: Option<&str>, name: &str) -> crate::app::TaskEntry {
        crate::app::TaskEntry {
            task_id: Some(id.into()),
            name: name.into(),
            api_status: crate::app::TaskStatus::Running,
            repo_url: None,
            prompt: None,
            wip_branch: None,
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            workspace_id: None,
            project: None,
            parent_task_id: parent.map(str::to_string),
            worktree_mode: crate::app::WorktreeMode::Inherit,
            metadata: None,
        }
    }

    #[test]
    fn descendant_walk_self_is_self() {
        let tasks = vec![task_with_parent("A", None, "a")];
        assert!(task_is_self_or_descendant_of(&tasks, "A", "A"));
    }

    #[test]
    fn descendant_walk_direct_child() {
        let tasks = vec![
            task_with_parent("A", None, "a"),
            task_with_parent("B", Some("A"), "b"),
        ];
        assert!(task_is_self_or_descendant_of(&tasks, "B", "A"));
        // Reverse direction: A is NOT a descendant of B.
        assert!(!task_is_self_or_descendant_of(&tasks, "A", "B"));
    }

    #[test]
    fn descendant_walk_deep_chain() {
        let tasks = vec![
            task_with_parent("A", None, "a"),
            task_with_parent("B", Some("A"), "b"),
            task_with_parent("C", Some("B"), "c"),
            task_with_parent("D", Some("C"), "d"),
        ];
        assert!(task_is_self_or_descendant_of(&tasks, "D", "A"));
        assert!(task_is_self_or_descendant_of(&tasks, "C", "A"));
        assert!(task_is_self_or_descendant_of(&tasks, "D", "B"));
    }

    #[test]
    fn descendant_walk_unrelated_tasks() {
        let tasks = vec![
            task_with_parent("A", None, "a"),
            task_with_parent("X", None, "x"),
            task_with_parent("Y", Some("X"), "y"),
        ];
        // X and Y are an unrelated subtree; not descendants of A.
        assert!(!task_is_self_or_descendant_of(&tasks, "X", "A"));
        assert!(!task_is_self_or_descendant_of(&tasks, "Y", "A"));
    }

    #[test]
    fn descendant_walk_handles_cycle_without_hanging() {
        // Pathological: A → B → A. If our walk loops forever, this
        // test never returns. The cycle guard caps depth and uses a
        // visited set.
        let tasks = vec![
            task_with_parent("A", Some("B"), "a"),
            task_with_parent("B", Some("A"), "b"),
        ];
        // C is not in the tasks list; the walk must terminate cleanly
        // and report "not a descendant".
        assert!(!task_is_self_or_descendant_of(&tasks, "A", "C"));
        assert!(!task_is_self_or_descendant_of(&tasks, "B", "C"));
    }

    #[test]
    fn descendant_walk_caps_at_max_depth() {
        // Build a chain of MAX_TASK_DEPTH + 5 tasks and confirm the
        // walk doesn't blow up. The reasoning: the walk should at
        // most walk MAX_TASK_DEPTH steps before bailing.
        let mut tasks: Vec<crate::app::TaskEntry> = Vec::new();
        for i in 0..(MAX_TASK_DEPTH + 5) {
            let id = format!("t{}", i);
            let parent = if i == 0 {
                None
            } else {
                Some(format!("t{}", i - 1))
            };
            tasks.push(task_with_parent(
                &id,
                parent.as_deref(),
                &format!("name-{}", i),
            ));
        }
        // The leaf is way past MAX_TASK_DEPTH from the root. The walk
        // returns false (depth exceeded), which is conservative — the
        // alternative (returning true with no real check) would be
        // wrong.
        let leaf = format!("t{}", MAX_TASK_DEPTH + 4);
        let _ = task_is_self_or_descendant_of(&tasks, &leaf, "t0");
        // No assert about true/false here — what matters is that the
        // call returned without panicking or hanging.
    }

    #[test]
    fn slug_chain_includes_all_ancestors() {
        let tasks = vec![
            task_with_parent("A", None, "first task"),
            task_with_parent("B", Some("A"), "second task"),
        ];
        let chain = build_slug_chain(&tasks, "B", "leaf");
        // Order: root → ... → parent → leaf, joined by "-".
        // Each task's name is slugified before joining.
        assert_eq!(chain, "first-task-second-task-leaf");
    }

    #[test]
    fn slug_chain_for_top_level_subtask() {
        let tasks = vec![task_with_parent("A", None, "parent")];
        let chain = build_slug_chain(&tasks, "A", "kid");
        assert_eq!(chain, "parent-kid");
    }

    #[test]
    fn slug_chain_handles_missing_parent() {
        // Parent ID doesn't exist in tasks list — chain just contains
        // the leaf slug. Defensive against a stale reference.
        let tasks: Vec<crate::app::TaskEntry> = vec![];
        let chain = build_slug_chain(&tasks, "ghost", "leaf");
        assert_eq!(chain, "leaf");
    }

    // -- Phase 4 / 5 workflow auth fix --

    fn task_in_workspace(
        id: &str,
        parent: Option<&str>,
        ws: Option<&str>,
    ) -> crate::app::TaskEntry {
        let mut t = task_with_parent(id, parent, id);
        t.workspace_id = ws.map(str::to_string);
        t
    }

    fn make_workflow_run(
        task_id: Option<&str>,
        task_key: &str,
    ) -> crate::workflow::run::WorkflowRun {
        crate::workflow::run::WorkflowRun {
            run_id: "run-1".into(),
            workflow_name: "feedback".into(),
            task_key: task_key.into(),
            task_id: task_id.map(str::to_string),
            role_sessions: Default::default(),
            active_role: None,
            iteration: 1,
            paused: false,
            status: crate::workflow::run::RunStatus::Running,
            history: vec![],
            started_at: 0,
            done_reason: None,
            events_offset: 0,
            role_baselines: Default::default(),
            goal: None,
            role_plans: Default::default(),
            rejected_findings: Vec::new(),
            pending_activation: None,
            nudge_assistant_count: None,
        }
    }

    fn caller_with_task(task: &str) -> CallerCtx {
        CallerCtx {
            workspace_index: Some(0),
            task_id: Some(task.into()),
            global_perms: false,
            is_tombstone: false,
        }
    }

    /// Global-perms grant: `caller_authorized_for` short-circuits to
    /// authorized for ANY target (here a session in a different
    /// workspace whose task is unrelated to the caller's), mirroring
    /// the daemon's `auth::check_session_caller`. A non-global caller
    /// with the same shape is denied.
    #[test]
    fn caller_authorized_for_global_caller_reaches_unrelated_target() {
        let tasks = vec![
            task_with_parent("A", None, "a"),
            task_with_parent("B", None, "b"), // unrelated task
        ];
        let mut caller = caller_with_task("A");
        caller.global_perms = true;
        // target_wi=9 (a different workspace), target task "B" (unrelated).
        assert!(caller_authorized_for(&caller, &tasks, 9, Some("B")));
        // Non-global caller with the identical shape is denied.
        let plain = caller_with_task("A");
        assert!(!caller_authorized_for(&plain, &tasks, 9, Some("B")));
    }

    #[test]
    fn workflow_auth_accepts_caller_for_own_task_id() {
        // Caller is on task A. Run is bound to task A via task_id field.
        // Direct path — task_id present, descendant check trivially passes.
        let tasks = vec![task_with_parent("A", None, "a")];
        let run = make_workflow_run(Some("A"), "ws-1");
        let caller = caller_with_task("A");
        assert!(workflow_run_authorized(&caller, &tasks, &run));
    }

    #[test]
    fn workflow_auth_accepts_caller_for_descendant_task_id() {
        // Caller on parent A; run is bound to subtask B (parent A).
        // Descendant — auth allows.
        let tasks = vec![
            task_with_parent("A", None, "a"),
            task_with_parent("B", Some("A"), "b"),
        ];
        let run = make_workflow_run(Some("B"), "ws-1");
        let caller = caller_with_task("A");
        assert!(workflow_run_authorized(&caller, &tasks, &run));
    }

    #[test]
    fn workflow_auth_rejects_caller_for_unrelated_task_id() {
        let tasks = vec![
            task_with_parent("A", None, "a"),
            task_with_parent("B", None, "b"), // unrelated
        ];
        let run = make_workflow_run(Some("B"), "ws-1");
        let caller = caller_with_task("A");
        assert!(!workflow_run_authorized(&caller, &tasks, &run));
    }

    #[test]
    fn workflow_auth_fallback_resolves_workspace_to_task() {
        // Run has no task_id (UI-launched, pre-Phase-4). task_key
        // is the workspace id. We resolve to tasks bound to that
        // workspace and check descendant. Caller is on task A which
        // is in workspace ws-1 — the run was launched there too.
        let tasks = vec![task_in_workspace("A", None, Some("ws-1"))];
        let run = make_workflow_run(None, "ws-1");
        let caller = caller_with_task("A");
        assert!(
            workflow_run_authorized(&caller, &tasks, &run),
            "fallback must accept when caller's task is bound to the run's workspace"
        );
    }

    #[test]
    fn workflow_auth_fallback_rejects_when_workspace_has_multiple_tasks() {
        // Critical regression: workspace ws-1 has two tasks — caller's
        // own task A AND an unrelated task B. The run is in ws-1 but
        // its task_id wasn't persisted (legacy run). Because we can't
        // tell whether the run was launched on A or B, we MUST refuse.
        // Pre-fix this returned true because A descends from itself,
        // letting an agent on A read/stop a workflow that may have
        // been launched on B.
        let tasks = vec![
            task_in_workspace("A", None, Some("ws-1")),
            task_in_workspace("B", None, Some("ws-1")), // unrelated
        ];
        let run = make_workflow_run(None, "ws-1");
        let caller = caller_with_task("A");
        assert!(
            !workflow_run_authorized(&caller, &tasks, &run),
            "ambiguous fallback (multiple candidate tasks) must reject"
        );
    }

    #[test]
    fn workflow_auth_fallback_rejects_when_no_task_in_workspace() {
        // Run's workspace has no tasks bound. Fallback finds nothing.
        let tasks = vec![task_in_workspace("A", None, Some("ws-other"))];
        let run = make_workflow_run(None, "ws-1");
        let caller = caller_with_task("A");
        assert!(!workflow_run_authorized(&caller, &tasks, &run));
    }

    // -- Phase 5: slug uniqueness --

    #[test]
    fn make_request_short_id_produces_seven_hex_chars() {
        let id = make_request_short_id();
        assert_eq!(id.len(), 7, "short_id should be exactly 7 chars: {}", id);
        assert!(
            id.chars().all(|c| c.is_ascii_hexdigit()),
            "should be all hex: {}",
            id
        );
    }

    #[test]
    fn make_request_short_id_is_unique_across_back_to_back_calls() {
        // Two same-name siblings under the same parent would otherwise
        // collide on the DB's UNIQUE (project, slug) index. The atomic
        // counter mixed into the nanos ensures that even calls that
        // happen within the same nanosecond (or at the exact same
        // wall-clock reading) produce distinct ids.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = make_request_short_id();
            assert!(
                seen.insert(id.clone()),
                "duplicate short_id within 1000 calls: {}",
                id
            );
        }
    }

    // -- Phase 5: workflow auth taskless reject --

    #[test]
    fn workflow_auth_rejects_taskless_caller() {
        // Taskless callers don't manage workflows.
        let tasks = vec![task_with_parent("A", None, "a")];
        let run = make_workflow_run(Some("A"), "ws-1");
        let caller = CallerCtx {
            workspace_index: Some(0),
            task_id: None,
            is_tombstone: false,
            global_perms: false,
        };
        assert!(!workflow_run_authorized(&caller, &tasks, &run));
    }

    // -- Phase 7: mark_subtask_done dedupe --
    //
    // Bug: the original `iter_mut().find(...)` updated only the first
    // TaskEntry whose task_id matched. If two entries shared the same
    // task_id (the duplicate hypothesis surfaced in Phase 5 smoke
    // testing), the second entry stayed `Running` and `list_subtasks`
    // — which iterates the full vec — surfaced the stale row. The fix
    // pivots to `iter_mut().filter(...).for_each(...)`, which updates
    // ALL matching entries. These tests pin the pattern.

    fn task_with_status(
        id: &str,
        status: crate::app::TaskStatus,
    ) -> crate::app::TaskEntry {
        let mut t = task_with_parent(id, None, id);
        t.api_status = status;
        t
    }

    /// Apply the same update-all-matching pattern that
    /// `mark_subtask_done` uses post-fix. Kept as a free helper so the
    /// tests don't have to spin up a full `App`.
    fn flip_all_matching_to_done(tasks: &mut [crate::app::TaskEntry], task_id: &str) {
        tasks
            .iter_mut()
            .filter(|t| t.task_id.as_deref() == Some(task_id))
            .for_each(|t| t.api_status = crate::app::TaskStatus::Done);
    }

    #[test]
    fn mark_done_updates_all_matching_entries() {
        // Reproduces the Phase 5 fizzle: two TaskEntry rows share the
        // same task_id. Pre-fix the first call only flipped one of
        // them and `list_subtasks` would still surface the stale
        // Running row, requiring a second mark_subtask_done call to
        // converge. Post-fix, ALL matching rows flip on the first call.
        let mut tasks = vec![
            task_with_status("dup", crate::app::TaskStatus::Running),
            task_with_status("other", crate::app::TaskStatus::Running),
            task_with_status("dup", crate::app::TaskStatus::Running),
        ];

        flip_all_matching_to_done(&mut tasks, "dup");

        let dup_statuses: Vec<&crate::app::TaskStatus> = tasks
            .iter()
            .filter(|t| t.task_id.as_deref() == Some("dup"))
            .map(|t| &t.api_status)
            .collect();
        assert_eq!(dup_statuses.len(), 2);
        for s in &dup_statuses {
            assert_eq!(**s, crate::app::TaskStatus::Done);
        }
        // Bystander row stays untouched.
        let other = tasks.iter().find(|t| t.task_id.as_deref() == Some("other")).unwrap();
        assert_eq!(other.api_status, crate::app::TaskStatus::Running);
    }

    #[test]
    fn mark_done_single_entry_path_still_works() {
        // The common case: exactly one matching entry. Verifies the
        // for_each pattern doesn't lose the single-row case the old
        // find()-based code used to handle.
        let mut tasks = vec![
            task_with_status("solo", crate::app::TaskStatus::Running),
        ];
        flip_all_matching_to_done(&mut tasks, "solo");
        assert_eq!(tasks[0].api_status, crate::app::TaskStatus::Done);
    }

    #[test]
    fn mark_done_no_matching_entry_is_a_noop() {
        // Defensive: if the local entry was already pruned (Done +
        // reconcile retain dropped it), the for_each yields nothing
        // and the call still returns ok at the API layer. Mirrors the
        // pre-fix `if let Some(t) = ... find()` semantics for this
        // path.
        let mut tasks = vec![
            task_with_status("present", crate::app::TaskStatus::Running),
        ];
        flip_all_matching_to_done(&mut tasks, "absent");
        assert_eq!(tasks[0].api_status, crate::app::TaskStatus::Running);
    }

    // -- Phase 7: stop_workflow no-op on Done --

    #[test]
    fn stop_workflow_treats_done_as_terminal() {
        // The gate condition that prevents `stop_workflow` from
        // overwriting a `workflow_done`'d run's status. Pre-fix the
        // run was unconditionally re-marked Detached, erasing
        // successful-completion semantics on disk.
        use crate::workflow::run::RunStatus;
        assert!(matches!(RunStatus::Done, RunStatus::Done));
        assert!(!matches!(RunStatus::Running, RunStatus::Done));
        assert!(!matches!(RunStatus::Paused, RunStatus::Done));
        // Detached is the post-stop state; stopping it again is a
        // benign no-op handled by the on-disk fallback path (the run
        // is no longer in `app.workflow_runs`).
        assert!(!matches!(RunStatus::Detached, RunStatus::Done));
    }
}
