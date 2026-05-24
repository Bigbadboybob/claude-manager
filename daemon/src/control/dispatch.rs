//! JSON-RPC dispatcher. Slice 10a-shell laid the wire path with an
//! all-`UnknownMethod` placeholder; subsequent slices route methods
//! that can be served from today's `DaemonState`.
//!
//! ## What's wired NOW
//!
//! - **`ping`** (slice 10b) — was inline in the TUI's old dispatcher;
//!   the daemon's version preserves the same response shape
//!   byte-for-byte so MCP clients that already use the `uid` field
//!   for `CM_TUI_SESSION_ID` smoke tests keep working.
//! - **`start_session`** (slice 10c-b) — Operator-callable only at
//!   this slice. Calls `DaemonSession::spawn` from 10c-a, inserts
//!   into `DaemonState.sessions`, returns `{session_uid}`. The
//!   handler body lives in `crate::control::methods::start_session`;
//!   this dispatcher arm wraps it with the Caller-authorization
//!   check.
//! - **`session.attach`** (slice 10c-c) — validates the requested
//!   `uid` against the live `DaemonState.sessions` registry, then
//!   delegates to `crate::attach::session_attach` (the slice-5
//!   frozen pure function) to mint a ticket bound to
//!   `(uid, caller)`. Returns `{ attach_ticket, attach_addr }`.
//! - **`attach.open`** (slice 10c-c) — consumes the ticket via
//!   `crate::attach::attach_open` and returns `{ session_uid }`.
//!   The accept loop in `lib.rs::handle_connection` reads
//!   `req.method == "attach.open" && response.ok` and switches
//!   the connection to streaming mode after writing the RPC
//!   response (see `crate::control::stream::handle_attach_stream`).
//!
//! ## What's NOT wired (TODO(slice-10c))
//!
//! - **`send_input`, `kill_session`, `read_session_output`**: take
//!   `&App` in today's TUI `methods.rs`. Relocation alongside the
//!   live registry — slice 10c-d.
//! - **Session-callable `start_session`**: requires descendant-task-
//!   tree validation against a live task list, which the daemon
//!   doesn't have yet (same gate as the methods above). Session
//!   callers get `Unauthorized` for now; Operator callers (the
//!   TUI driving the daemon) work.
//!
//! ## Do NOT add TUI-socket fallback proxying here
//!
//! **Standing rejection from review rounds 19, 27, 28, 29.** When
//! a daemon-spawned MCP caller hits a method the daemon doesn't
//! yet route (anything in the agent-orchestration surface — most
//! Session-caller methods — until slice 10d-mcp-surface ships),
//! it gets `UnknownMethod` / `Unauthorized`. **That is the
//! intended behavior.** Do not propose proxying those calls
//! through to the TUI socket as a fallback. Specifically:
//!
//! 1. The opt-in (`CM_USE_DAEMON_SOCKET=1`) exists to surface
//!    these gaps as loud errors during the slice-by-slice
//!    migration, not to mask them. The default path (opt-in
//!    off) routes agents to `tui.sock` which serves every
//!    method; users are unaffected by the gap.
//! 2. Silent fallback to `tui.sock` for daemon-spawned sessions
//!    would re-introduce the bug class we've fixed three times
//!    during Phase 1 (slice-11 strict-fatal opt-in, slice-13
//!    empty-env-as-unset, slice-17 authoritative env-injection).
//!    Each of those fixes existed precisely because we kept
//!    being bitten by silent fallbacks. Do not add a fourth.
//! 3. MCP-surface relocation has its own slice arc:
//!    `10d-memory-cap-relocation` → `10d-mcp-surface` →
//!    `10d-workflow-controller` (per `daemon/NOTES.md`).
//!    `10d-mcp-surface` is specifically where Session-caller
//!    dispatch with task-tree validation lands.
//! 4. The 10c-e-3c smoke is scoped to PTY mechanics only;
//!    "MCP from inside the session" was dropped from the
//!    checklist for exactly this reason.
//!
//! If a future review surfaces "daemon-spawned agents lose MCP
//! tools" again: reject in place, point at this paragraph and
//! the slice-arc ordering. Don't re-litigate it round by round.

use std::sync::{mpsc, Arc, Mutex};

use crate::attach::{
    self, AttachOpenParams, AttachOpenResponse, SessionAttachParams, SessionAttachResponse,
};
use crate::control::methods;
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::session::SharedLastExit;
use crate::state::DaemonState;

/// Handle that an [`DispatchOutcome::AttachStream`] carries
/// alongside the OK response. `handle_connection` passes it into
/// [`crate::control::stream::handle_attach_stream`] after writing
/// the response, so the stream module reuses the pre-built
/// subscription instead of re-locking + re-looking-up the session
/// (which would re-introduce the TOCTOU window the slice-10c-e-2
/// review-2 fix closes).
pub struct AttachStreamHandle {
    pub session_uid: String,
    /// Pre-built fanout subscriber. Held independent of registry
    /// membership — if the session is removed before stream start,
    /// the subscriber still observes `Disconnected` on producer
    /// close (via `PtyByteFanout::close`) and the End frame fires
    /// with whatever LastExit the reaper recorded.
    pub fanout_rx: mpsc::Receiver<Vec<u8>>,
    /// Shared exit slot. Populated by the reaper thread on
    /// `waitpid` return; read by the stream module on End-frame
    /// emission to encode `{exit_code, memory_cap_kill}` into the
    /// payload.
    pub last_exit: SharedLastExit,
    /// Echoed back on every outbound `StreamFrame`. Matches the
    /// `attach.open` request id so the client can demux.
    pub request_id: String,
}

/// What `dispatch_request` returns. Most arms produce `Done` with
/// the response that should be written verbatim. The `attach.open`
/// arm produces `AttachStream { response, handle }` so
/// `handle_connection` can write the response AND then run the
/// stream loop with the held subscription — closing the TOCTOU
/// the slice-10c-e-2 review-2 flagged.
pub enum DispatchOutcome {
    Done(Response),
    AttachStream {
        response: Response,
        handle: AttachStreamHandle,
    },
}

impl DispatchOutcome {
    /// Accessor for the response shape (used by tests that only
    /// care about the wire envelope).
    pub fn response(&self) -> &Response {
        match self {
            DispatchOutcome::Done(r) => r,
            DispatchOutcome::AttachStream { response, .. } => response,
        }
    }

    /// Consume and return the response only. Used by callers
    /// that don't run the attach-stream half (test helpers, the
    /// integration accept-loop driver).
    pub fn into_response(self) -> Response {
        match self {
            DispatchOutcome::Done(r) => r,
            DispatchOutcome::AttachStream { response, .. } => response,
        }
    }
}

/// Route `req` to the appropriate method handler. Returns
/// `UnknownMethod` for everything that depends on App state that
/// hasn't migrated; see the module doc for the cutoff.
pub fn dispatch_request(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> DispatchOutcome {
    match req.method.as_str() {
        // No state needed — pure shape check on `req.caller`.
        "ping" => DispatchOutcome::Done(dispatch_ping(req)),

        // `start_session` manages its own locking. The reaper
        // thread's cleanup callback re-acquires the state mutex
        // after waitpid, so this arm cannot hold the lock across
        // the spawn-and-insert critical section. See slice-10c-c
        // review fix #2 for the race-safety argument; the Arc
        // serializes the insert against the callback's remove.
        "start_session" => DispatchOutcome::Done(dispatch_start_session(state, req)),

        // Read-mostly methods lock for the duration of the dispatch
        // arm. Brief critical sections — no daemon-state-blocking
        // I/O inside.
        "session.attach" => {
            let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
            DispatchOutcome::Done(dispatch_session_attach(&mut s, req))
        }
        // `attach.open` is the one method that returns a non-Done
        // outcome on success — the slice-10c-e-2 review-2 fix
        // moves the session live-check + fanout subscription
        // inside the same critical section as ticket consume.
        "attach.open" => {
            let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
            dispatch_attach_open(&mut s, req)
        }

        // Session-mutation methods (slice 10c-d). Each manages its
        // own locking inside `methods::*`; the dispatcher just
        // does the Caller-authorization shape check.
        "send_input" => DispatchOutcome::Done(dispatch_send_input(state, req)),
        "kill_session" => DispatchOutcome::Done(dispatch_kill_session(state, req)),
        "read_session_output" => {
            DispatchOutcome::Done(dispatch_read_session_output(state, req))
        }

        // Slice 10d-mcp-surface: list_sessions. Session callers
        // see only their own workspace; Operator callers see the
        // full registry.
        "list_sessions" => DispatchOutcome::Done(dispatch_list_sessions(state, req)),

        // Slice 10d-mcp-surface-2a: TUI-pushed task-tree
        // snapshot. Operator-only — Session callers can't
        // rewrite the tree.
        "task.update_tree" => DispatchOutcome::Done(dispatch_task_update_tree(state, req)),

        // Sub-2b-1: `resolve_authorized_session` — the Python
        // MCP `read_session_output` tool's first leg of its
        // composed pattern. Returns `{state, engine,
        // transcript_path, generation, idle}` so the tool can
        // parse the transcript file directly (no per-message
        // daemon round-trip). Session-caller auth runs in the
        // method body (sub-2a's TOCTOU shape).
        "resolve_authorized_session" => {
            DispatchOutcome::Done(dispatch_resolve_authorized_session(state, req))
        }

        // Sub-2b-1 review #1: TUI pushes the discovered
        // transcript path post-detection so the resolver can
        // transition `pending` → `ready`. Operator-only — the
        // TUI is the source of truth for engine-specific path
        // conventions; a Session-caller setting this could lie
        // to the resolver about which file the Python tool
        // reads (same auth shape as `task.update_tree`).
        "session.set_transcript_path" => {
            DispatchOutcome::Done(dispatch_set_transcript_path(state, req))
        }

        // Sub-2b-2: `propose_task` — daemon-side HTTP forwarder
        // to the planning API. Both Operator and Session callers
        // allowed (any agent can propose; project owner reviews
        // the queue and accepts/rejects). Auth shape matches the
        // Python tool's pre-2b-2 behavior of "anyone with
        // CM_API_TOKEN can call /tasks" — we don't reinvent
        // task-subtree gating here.
        "propose_task" => DispatchOutcome::Done(dispatch_propose_task(state, req)),

        _ => DispatchOutcome::Done(Response::err(
            req.id.clone(),
            ErrorCode::UnknownMethod,
            format!(
                "method '{}' is still served by the TUI; relocation deferred to slice 10c (see doc/persistent-host-daemon.md + daemon/NOTES.md)",
                req.method,
            ),
        )),
    }
}

/// `start_session` — spawn a daemon-owned session. Operator-only
/// at slice 10c-b through 10d-mcp-surface-1.
///
/// TODO(slice 10d-mcp-surface-2): re-enable Session-caller dispatch
/// once (a) `DaemonSession` carries `task_id` and the daemon has
/// access to the planning task tree (descendant-task auth), and
/// (b) the wire shape supports the Python MCP tool's minimal
/// `{type, label, prompt?, task_id?}` start_session params (today
/// the daemon requires `uid`, `workspace_id`, `argv`, `working_dir`
/// which the Session-caller can't supply). The same-workspace
/// auth shape briefly explored in sub-1 was reverted because
/// (a) it widened access for task-bound callers vs the TUI rule
/// (Finding #1 from sub-1 review), and (b) the wire-shape mismatch
/// would have caused agents to get `InvalidParams` errors even
/// when auth passed (Finding #3).
fn dispatch_start_session(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    if matches!(req.caller, Caller::Session(_)) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "start_session is Operator-callable only through slice 10d-mcp-surface-1; Session-caller path re-enables in sub-2 after task-subtree auth + wire-shape alignment with the Python MCP tool",
        );
    }
    match methods::start_session(state, &req.params) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `list_sessions` — enumerate the daemon's live session
/// registry. Sub-2a Session-caller flow: caller's task/workspace
/// scoping is computed up front (looking up the caller's
/// session in the registry) and passed into `methods::list_sessions`
/// via the `caller_scope` parameter. The method body filters to
/// sessions the caller is authorized for via
/// `crate::control::auth::check_session_caller`. Operator
/// callers bypass scoping (see all sessions).
fn dispatch_list_sessions(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    let caller_uid: Option<String> = match &req.caller {
        Caller::Operator(_) => None,
        Caller::Session(s) => Some(s.session_uid.clone()),
    };
    // Validate caller-existence up front so we surface
    // Unauthorized at the dispatcher rather than letting the
    // method body return an empty list (which would be
    // ambiguous: "no sessions" vs "you can't see any").
    if let Some(uid) = &caller_uid {
        let state_guard = state.lock().unwrap_or_else(|p| p.into_inner());
        if !state_guard.sessions.contains_key(uid) {
            return Response::err(
                req.id.clone(),
                ErrorCode::Unauthorized,
                format!(
                    "Session caller '{}' is not in the daemon registry",
                    uid
                ),
            );
        }
    }
    match methods::list_sessions(state, &req.params, caller_uid.as_deref()) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `resolve_authorized_session` — sub-2b-1. Returns
/// `{state, engine, transcript_path, generation, idle}` for the
/// Python MCP `read_session_output` tool's compose pattern.
/// Caller extraction + Operator-bypass shape mirrors the
/// sub-2a dispatch arms (`send_input` / `kill_session` /
/// `read_session_output`). Method body does auth + lookup in
/// one critical section.
fn dispatch_resolve_authorized_session(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    let caller_uid: Option<String> = match &req.caller {
        Caller::Operator(_) => None,
        Caller::Session(s) => Some(s.session_uid.clone()),
    };
    match methods::resolve_authorized_session(state, &req.params, caller_uid.as_deref()) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `propose_task` — sub-2b-2. Both Operator and Session callers
/// allowed (the planning queue is intentionally open to all
/// agents; the project owner accepts/rejects manually). The
/// method body does its own param validation + HTTP forwarding
/// via `daemon::planning_client::propose_task`.
fn dispatch_propose_task(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    match methods::propose_task(state, &req.params) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `session.set_transcript_path` — TUI pushes the discovered
/// transcript path post-detection (sub-2b-1 review #1).
/// Operator-only — see method-level doc on
/// `methods::set_transcript_path` for the auth rationale.
fn dispatch_set_transcript_path(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    if matches!(req.caller, Caller::Session(_)) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "session.set_transcript_path is Operator-callable only — \
             the TUI owns transcript-path discovery; a Session caller \
             setting this would let an agent redirect the Python MCP \
             `read_session_output` tool to an attacker-chosen file",
        );
    }
    match methods::set_transcript_path(state, &req.params) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `task.update_tree` — TUI-pushed task-tree snapshot
/// (slice 10d-mcp-surface-2a). Operator-only.
fn dispatch_task_update_tree(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    if matches!(req.caller, Caller::Session(_)) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "task.update_tree is Operator-callable only — a Session caller \
             rewriting the task tree could escape their own auth scope",
        );
    }
    match methods::task_update_tree(state, &req.params) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `session.attach` — mint an attach ticket for `uid`. The slice-5
/// pure function `crate::attach::session_attach` doesn't validate
/// uid against any registry (by design — it's a transport-agnostic
/// primitive). This dispatcher arm adds the **live-registry check**:
/// the requested uid must exist in `DaemonState.sessions` (which
/// slice 10c-b's `start_session` populates) before a ticket is
/// minted. Without this, a caller could mint a ticket for any uid
/// and have `attach.open` later fail with NotFound — the live
/// check surfaces the error at issue time instead.
///
/// **Caller authorization**: delegated to the pure function. Today
/// only `Operator` callers can issue tickets; Session callers get
/// `Unauthorized`. See `crate::attach::session_attach` for the
/// rationale.
fn dispatch_session_attach(state: &mut DaemonState, req: &Request) -> Response {
    let params: SessionAttachParams =
        match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return Response::err(
                    req.id.clone(),
                    ErrorCode::InvalidParams,
                    format!("session.attach params: {}", e),
                );
            }
        };

    // Live-registry validation. The pure function doesn't know
    // about `DaemonState.sessions`; we check here so a ticket
    // can't be minted for a uid that isn't actually attachable.
    if !state.sessions.contains_key(&params.uid) {
        return Response::err(
            req.id.clone(),
            ErrorCode::NotFound,
            format!(
                "session '{}' not in daemon registry (was it ever started? did it exit?)",
                params.uid
            ),
        );
    }

    match attach::session_attach(
        &state.tickets,
        &req.caller,
        &params,
        &state.attach_addr,
    ) {
        Ok(SessionAttachResponse {
            attach_ticket,
            attach_addr,
        }) => Response::ok(
            req.id.clone(),
            serde_json::json!({
                "attach_ticket": attach_ticket,
                "attach_addr": attach_addr,
            }),
        ),
        Err(e) => Response::err(req.id.clone(), e.code(), e.message().to_string()),
    }
}

/// `attach.open` — consume a ticket and (on success) subscribe
/// the caller's connection to the bound session's fanout, all in
/// one locked critical section.
///
/// ## Why subscribe in the same lock (slice-10c-e-2 review-2 fix)
///
/// Previously the dispatcher only consumed the ticket; the
/// session live-check happened later in `handle_attach_stream`
/// after the OK response was already on the wire. Window: a
/// session that exited between OK and stream-start would leave
/// the client believing attach succeeded while landing on a dead
/// handle. Fix: clone `Arc<PtyByteFanout>` + `subscribe()` + clone
/// `last_exit` while still holding the state lock, package as
/// `AttachStreamHandle`, hand to `handle_connection` which passes
/// it to `handle_attach_stream`. The subscription is now held
/// independent of registry membership — if the session is removed
/// after we subscribed, the producer-side `close()` still fires
/// and the subscriber sees `Disconnected`, surfacing a clean End
/// frame.
fn dispatch_attach_open(state: &mut DaemonState, req: &Request) -> DispatchOutcome {
    let params: AttachOpenParams =
        match serde_json::from_value(req.params.clone()) {
            Ok(p) => p,
            Err(e) => {
                return DispatchOutcome::Done(Response::err(
                    req.id.clone(),
                    ErrorCode::InvalidParams,
                    format!("attach.open params: {}", e),
                ));
            }
        };

    let session_uid = match attach::attach_open(&state.tickets, &req.caller, &params) {
        Ok(AttachOpenResponse { session_uid }) => session_uid,
        Err(e) => {
            return DispatchOutcome::Done(Response::err(
                req.id.clone(),
                e.code(),
                e.message().to_string(),
            ));
        }
    };

    // Subscribe + clone last_exit inside the same critical
    // section as the ticket consume above (`state` is &mut and
    // we still hold the dispatch-arm lock from `dispatch_request`).
    // If the session is absent here, the dispatcher's
    // session.attach already minted a ticket for a uid that has
    // since exited — surface as Conflict so the client knows the
    // attach can't proceed.
    let (fanout_rx, last_exit) = match state.sessions.get(&session_uid) {
        Some(session) => (session.fanout.subscribe(), session.last_exit.clone()),
        None => {
            return DispatchOutcome::Done(Response::err(
                req.id.clone(),
                ErrorCode::Conflict,
                format!(
                    "session '{}' exited between session.attach and attach.open",
                    session_uid
                ),
            ));
        }
    };

    let response = Response::ok(
        req.id.clone(),
        serde_json::json!({ "session_uid": session_uid.clone() }),
    );
    let handle = AttachStreamHandle {
        session_uid,
        fanout_rx,
        last_exit,
        request_id: req.id.clone(),
    };
    DispatchOutcome::AttachStream { response, handle }
}

/// `send_input` — write bytes to a session's PTY. Session-caller
/// flow auth-checked via `crate::control::auth::check_session_caller`
/// (sub-2a TUI-mirror rule: self / same-task / descendant-task /
/// taskless+same-workspace). Operator callers bypass auth.
///
/// ## Sub-2a Finding #2 TOCTOU fix
///
/// Pre-fix the dispatcher called a shared
/// `authorize_session_caller_for_session_param` helper that
/// locked-checked-dropped, then `methods::send_input` re-locked
/// to mutate. The window between the two locks let the target
/// session be removed (or replaced via uid reuse) AFTER auth
/// passed but BEFORE the method body acted. Fix: pass the
/// caller uid into the method so auth + Arc-clone happen in
/// the same critical section. Operator callers pass
/// `caller_uid: None`, which the method body interprets as
/// "skip auth."
fn dispatch_send_input(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    let caller_uid: Option<String> = match &req.caller {
        Caller::Operator(_) => None,
        Caller::Session(s) => Some(s.session_uid.clone()),
    };
    match methods::send_input(state, &req.params, caller_uid.as_deref()) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `kill_session` — terminate a session. Operator-only at slice
/// 10c-d. The TUI's tombstone-and-manifest-write is deferred to
/// slice 10e (manifest-ownership flip); this slice just removes
/// from the in-memory registry, which is sufficient for
/// `session.attach` to subsequently return NotFound.
///
/// ## Sub-2a Finding #2 TOCTOU fix
///
/// Same shape as `dispatch_send_input` above — auth + remove
/// happen in the same critical section inside the method body.
fn dispatch_kill_session(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    let caller_uid: Option<String> = match &req.caller {
        Caller::Operator(_) => None,
        Caller::Session(s) => Some(s.session_uid.clone()),
    };
    match methods::kill_session(state, &req.params, caller_uid.as_deref()) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `read_session_output` — snapshot read of a session's PTY-output
/// fanout. Cursor-based; returns base64-encoded bytes + eviction
/// flag. Distinct from the Python MCP tool of the same name in
/// `mcp_server/server.py`, which composes
/// `resolve_authorized_session` (TUI-side) with a Python
/// transcript-file read. See `crate::control::methods::read_session_output`
/// for the full disposition.
///
/// ## Sub-2a Finding #2 TOCTOU fix
///
/// Same shape as `dispatch_send_input` above — auth + fanout
/// Arc-clone happen in the same critical section inside the
/// method body.
fn dispatch_read_session_output(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    let caller_uid: Option<String> = match &req.caller {
        Caller::Operator(_) => None,
        Caller::Session(s) => Some(s.session_uid.clone()),
    };
    match methods::read_session_output(state, &req.params, caller_uid.as_deref()) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `ping` — Session callers get `{pong: true, uid: <session_uid>}`
/// (matches the TUI's old dispatcher byte-for-byte, including the
/// `uid` field that `mcp_server.ping()` documents as the smoke test
/// for `CM_TUI_SESSION_ID` propagation). Operator callers get
/// `Unauthorized` (the TUI's dispatcher rejected them upfront, so
/// this is the same parity).
///
/// `caller_kind` is appended as an additive field for Session
/// callers — useful diagnostic when an operator is reading raw
/// dispatcher logs, doesn't change any existing client behavior.
fn dispatch_ping(req: &Request) -> Response {
    match &req.caller {
        Caller::Session(s) => Response::ok(
            req.id.clone(),
            serde_json::json!({
                "pong": true,
                "uid": s.session_uid,
                "caller_kind": "session",
            }),
        ),
        Caller::Operator(_) => Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "method requires a session-scoped caller (matches TUI dispatcher parity; Operator-callable methods land in a later slice)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session_request(method: &str, params: serde_json::Value, uid: &str) -> Request {
        Request {
            id: format!("test-{}", method),
            caller: Caller::session(uid),
            method: method.into(),
            params,
        }
    }

    fn operator_request(method: &str, params: serde_json::Value) -> Request {
        Request {
            id: format!("test-{}", method),
            caller: Caller::operator("op-default"),
            method: method.into(),
            params,
        }
    }

    /// Construct the daemon state already wrapped in `Arc<Mutex<…>>`.
    /// All `dispatch_request` calls take an Arc now (slice-10c-c
    /// review fix #2 — the state arc is what the reaper-cleanup
    /// callback re-locks to remove exited sessions from the
    /// registry).
    fn make_state() -> Arc<Mutex<DaemonState>> {
        let mut s = DaemonState::new();
        s.attach_addr = "/tmp/cm-daemon-test.sock".into();
        Arc::new(Mutex::new(s))
    }

    // --- ping ---------------------------------------------------------

    #[test]
    fn ping_session_caller_returns_pong_and_uid_byte_for_byte_tui_parity() {
        // The named acceptance criterion: "CM_TUI_SESSION_ID
        // scoping behaves identically to today." The MCP server's
        // ping smoke test reads `uid` out of the response and
        // compares it to the env-injected CM_TUI_SESSION_ID. Daemon
        // must produce the same shape the TUI does.
        let state = make_state();
        let req = session_request("ping", serde_json::Value::Null, "ts-abc-123");
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok, "ping must succeed for Session callers");
        let result = resp.result.expect("result body");
        // Both the legacy `pong` and `uid` fields are present with
        // exactly the TUI's old shape.
        assert_eq!(result["pong"], true);
        assert_eq!(result["uid"], "ts-abc-123");
        // `caller_kind` is the only additive field — clients that
        // ignore it are unaffected.
        assert_eq!(result["caller_kind"], "session");
        assert_eq!(
            result.as_object().map(|o| o.len()),
            Some(3),
            "only three keys present: pong, uid, caller_kind. Any other key drift would be a client-visible change.",
        );
    }

    #[test]
    fn ping_operator_caller_is_unauthorized_matching_tui_parity() {
        // The TUI's dispatcher rejects Operator callers before
        // reaching any handler (see `tui/src/app.rs::dispatch_control`,
        // slice-1 rewrite). The daemon's ping mirrors that
        // exactly — Operator-callable methods are an
        // additive-after-10c concept; today, parity rules.
        let state = make_state();
        let req = operator_request("ping", serde_json::Value::Null);
        let resp = dispatch_request(&state, &req).into_response();
        assert!(!resp.ok, "Operator must get Unauthorized");
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(
            err.message.contains("session-scoped"),
            "message should explain the parity rule: {}",
            err.message
        );
    }

    // --- deferred methods --------------------------------------------

    #[test]
    fn deferred_method_returns_unknown_with_slice_10c_pointer() {
        let state = make_state();
        // Slice 10d-mcp-surface wired list_sessions; sub-2b-2
        // wired propose_task; pick a workflow tool (deferred to
        // sub-2c per NOTES.md) to exercise the deferred-arm
        // fallback.
        let resp = dispatch_request(
            &state,
            &session_request("workflow_transition", serde_json::Value::Null, "ts-x"),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::UnknownMethod);
        assert!(
            err.message.contains("slice 10c"),
            "unknown-method error should point at the migration slice: {}",
            err.message
        );
    }

    // --- session.attach / attach.open (slice 10c-c) -------------------------
    //
    // The pure functions in `crate::attach` are exhaustively tested in
    // `crate::attach::tests` (ticket TTL, single-use, identity binding,
    // Session-caller rejection). Tests here focus on the dispatcher's
    // *wrapper* responsibilities:
    //   - live-registry validation for session.attach (the slice-10b
    //     punt this slice closes).
    //   - InvalidParams when the request payload doesn't match.
    //   - the bridge from `attach::AttachError` → `protocol::ErrorCode`.

    /// Insert a stub session into `state.sessions` so the dispatcher's
    /// live-registry check finds it. Uses `/bin/sleep 30` — long-
    /// lived enough that the reaper-cleanup callback (slice-10c-c
    /// review fix #2) doesn't race the test by removing the entry
    /// before the dispatcher arm reads it. The session's Drop sends
    /// SIGKILL when the Arc<Mutex<DaemonState>> drops at end of test.
    fn state_with_session(uid: &str) -> Arc<Mutex<DaemonState>> {
        state_with_session_in_workspace(uid, "")
    }

    /// Workspace-parameterized variant introduced in slice
    /// 10d-mcp-surface-1 for the Session-caller auth tests.
    /// Leaves `state_with_session` callers untouched (empty
    /// workspace_id matches the pre-#10d default).
    fn state_with_session_in_workspace(uid: &str, workspace_id: &str) -> Arc<Mutex<DaemonState>> {
        let state = make_state();
        let mut params =
            crate::session::SpawnParams::new(uid, "test", "/bin/sleep");
        params.args = vec!["30".into()];
        params.workspace_id = workspace_id.to_string();
        let session =
            crate::session::DaemonSession::spawn(params).expect("spawn /bin/sleep");
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert(uid.into(), session);
        }
        state
    }

    /// Insert a second session into an existing state — used by
    /// auth tests that need two sessions in the same or
    /// different workspaces.
    fn add_session(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        workspace_id: &str,
    ) {
        add_session_typed(state, uid, workspace_id, "claude-code");
    }

    /// Like `add_session` but with explicit `session_type` —
    /// used by `list_sessions_returns_correct_type_for_*` tests
    /// that pin the wire field.
    fn add_session_typed(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        workspace_id: &str,
        session_type: &str,
    ) {
        let mut params =
            crate::session::SpawnParams::new(uid, "test", "/bin/sleep");
        params.args = vec!["30".into()];
        params.workspace_id = workspace_id.to_string();
        params.session_type = session_type.to_string();
        let session =
            crate::session::DaemonSession::spawn(params).expect("spawn /bin/sleep");
        let mut s = state.lock().unwrap();
        s.sessions.insert(uid.into(), session);
    }

    #[test]
    fn session_attach_with_operator_and_live_uid_mints_ticket() {
        // Headline acceptance: the live-registry check passes for a
        // real session uid, the pure function mints a ticket, the
        // response carries `attach_ticket` + `attach_addr`.
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &operator_request("session.attach", serde_json::json!({ "uid": "ts-live" })),
        ).into_response();
        assert!(resp.ok, "live uid must succeed: {:?}", resp.error);
        let result = resp.result.expect("result body");
        assert!(
            result["attach_ticket"].as_str().is_some_and(|s| !s.is_empty()),
            "attach_ticket must be a non-empty string: {}",
            result,
        );
        assert_eq!(
            result["attach_addr"].as_str().unwrap(),
            "/tmp/cm-daemon-test.sock",
            "attach_addr passed through from state",
        );
    }

    #[test]
    fn session_attach_with_unknown_uid_returns_not_found() {
        // The named regression-vs-slice-5: the pure function would
        // blindly mint a ticket for any uid. The dispatcher's
        // live-registry check rejects unknown uids BEFORE the
        // allocator runs. NotFound (not Internal) so a copy-from-
        // list_sessions round-trip is debuggable.
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "session.attach",
                serde_json::json!({ "uid": "ts-ghost" }),
            ),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(err.message.contains("ts-ghost"), "must name the missing uid: {}", err.message);
    }

    #[test]
    fn session_attach_rejects_session_caller_through_pure_function() {
        // Strict Phase-1 rule: only Operator callers issue tickets.
        // The dispatcher delegates to `attach::session_attach` which
        // returns `AttachError::Unauthorized`; the bridge maps that
        // onto `ErrorCode::Unauthorized`.
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &session_request(
                "session.attach",
                serde_json::json!({ "uid": "ts-live" }),
                "ts-some-agent",
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::Unauthorized,
        );
    }

    #[test]
    fn session_attach_malformed_params_is_invalid_params() {
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &operator_request("session.attach", serde_json::json!({})),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::InvalidParams,
        );
    }

    #[test]
    fn attach_open_round_trip_consumes_ticket_and_returns_session_uid() {
        // Mint via session.attach, consume via attach.open, observe
        // the bound uid in the response. The accept loop reads this
        // uid to route the stream-transition (handle_attach_stream).
        let state = state_with_session("ts-live");
        let mint = dispatch_request(
            &state,
            &operator_request("session.attach", serde_json::json!({ "uid": "ts-live" })),
        ).into_response();
        let ticket = mint
            .result
            .expect("mint result")
            ["attach_ticket"]
            .as_str()
            .expect("ticket id")
            .to_string();

        let resp = dispatch_request(
            &state,
            &operator_request("attach.open", serde_json::json!({ "ticket": ticket })),
        ).into_response();
        assert!(resp.ok, "ticket consume should succeed: {:?}", resp.error);
        assert_eq!(
            resp.result.expect("result body")["session_uid"]
                .as_str()
                .unwrap(),
            "ts-live",
        );
    }

    #[test]
    fn attach_open_with_unknown_ticket_collapses_to_not_found() {
        // Three failure modes in `TicketAllocator::consume` (unknown
        // / expired / already-consumed) fold into NotFound at the
        // method-handler level so a probe-style caller can't
        // distinguish them. Surface that here too.
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &operator_request("attach.open", serde_json::json!({ "ticket": "never-issued" })),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::NotFound,
        );
    }

    #[test]
    fn attach_open_with_wrong_caller_is_unauthorized() {
        // Identity-bound ticket: minted to op-alice, presented by
        // op-bob → Unauthorized. Defends against a leaked ticket
        // being replayed by an unrelated operator.
        let state = state_with_session("ts-live");
        let mint = dispatch_request(
            &state,
            &Request {
                id: "mint".into(),
                caller: Caller::operator("op-alice"),
                method: "session.attach".into(),
                params: serde_json::json!({ "uid": "ts-live" }),
            },
        ).into_response();
        let ticket = mint.result.expect("mint")["attach_ticket"]
            .as_str()
            .unwrap()
            .to_string();

        let resp = dispatch_request(
            &state,
            &Request {
                id: "attack".into(),
                caller: Caller::operator("op-bob"),
                method: "attach.open".into(),
                params: serde_json::json!({ "ticket": ticket }),
            },
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::Unauthorized,
        );
    }

    #[test]
    fn attach_open_malformed_params_is_invalid_params() {
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &operator_request("attach.open", serde_json::json!({})),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::InvalidParams,
        );
    }

    #[test]
    fn dispatcher_echoes_request_id_on_error() {
        let state = make_state();
        let req = Request {
            id: "unique-id-xyz".into(),
            caller: Caller::operator("op"),
            method: "any.method".into(),
            params: serde_json::json!({}),
        };
        let resp = dispatch_request(&state, &req).into_response();
        assert_eq!(resp.id, "unique-id-xyz");
    }

    // --- start_session (slice 10c-b) -----------------------------------------
    //
    // The full spawn flow is tested exhaustively in
    // `crate::control::methods::tests` (real PTY spawns, env injection,
    // uid format, error paths). Tests here focus on dispatcher routing
    // and the Caller-authorization disposition — fast, no real
    // children spawned.

    #[test]
    fn start_session_session_caller_still_unauthorized_pending_sub_2() {
        // Sub-1 review reverted the auth-flip for Session
        // callers (Findings #1-#3: same-workspace widening +
        // wire-shape mismatch with Python MCP tool). Session-
        // caller dispatch re-enables in sub-2 alongside
        // task-subtree auth and wire-shape alignment.
        let state = make_state();
        let req = session_request(
            "start_session",
            serde_json::json!({
                "uid": "ts-deadbeef-1",
                "workspace_id": "ws-1",
                "label": "x",
                "argv": ["/bin/bash"],
                "working_dir": "/tmp",
            }),
            "ts-agent",
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(
            err.message.contains("sub-2"),
            "error should point at the slice that re-enables Session-caller dispatch: {}",
            err.message
        );
    }

    #[test]
    fn start_session_operator_caller_routes_to_methods_layer() {
        // Routing check: an Operator caller with malformed params
        // must reach `methods::start_session`, which returns
        // InvalidParams. (If the dispatcher didn't route correctly
        // we'd get UnknownMethod instead.)
        let state = make_state();
        let req = operator_request(
            "start_session",
            serde_json::json!({ "label": "no-workspace-id" }),
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(
            err.code,
            ErrorCode::InvalidParams,
            "operator caller with malformed params should reach methods layer and surface InvalidParams, not UnknownMethod",
        );
    }

    #[test]
    fn start_session_operator_caller_missing_workspace_returns_not_found() {
        // The 10c-b registry is `state.workspaces` (manifest snapshot).
        // A workspace_id not in the snapshot must surface as NotFound,
        // not Internal — clients distinguish "you got the id wrong"
        // from "the daemon is broken."
        let state = make_state();
        let req = operator_request(
            "start_session",
            serde_json::json!({
                "uid": "ts-deadbeef-2",
                "workspace_id": "ws-ghost",
                "label": "x",
                "argv": ["/bin/bash"],
                "working_dir": "/tmp",
            }),
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(err.message.contains("ws-ghost"));
    }

    // --- session-mutation arms (slice 10c-d) -------------------------------
    //
    // Full method-body tests live in `crate::control::methods::tests`
    // (real PTY spawns, real bytes through the fanout). Tests here
    // focus on dispatcher routing + Session-caller Unauthorized.

    /// Sub-2a: Session-caller `send_input` is auth-checked via
    /// the TUI-mirror rule. A caller targeting their own
    /// session passes auth (and reaches the methods layer,
    /// which `InvalidParams`s on missing `text` — the "got
    /// past auth" signal).
    #[test]
    fn send_input_session_caller_self_passes_auth() {
        let state = state_with_session_in_workspace("ts-self", "ws-x");
        let resp = dispatch_request(
            &state,
            &session_request(
                "send_input",
                serde_json::json!({ "session_uid": "ts-self" }),
                "ts-self",
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::InvalidParams,
            "self-target must pass auth and reach methods layer",
        );
    }

    /// Sub-2a: Session-caller `send_input` from a taskless
    /// caller targeting a same-workspace sibling passes auth.
    #[test]
    fn send_input_session_caller_taskless_same_workspace_passes_auth() {
        let state = state_with_session_in_workspace("ts-caller", "ws-shared");
        add_session(&state, "ts-target", "ws-shared");
        let resp = dispatch_request(
            &state,
            &session_request(
                "send_input",
                serde_json::json!({ "session_uid": "ts-target" }),
                "ts-caller",
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::InvalidParams,
        );
    }

    /// Sub-2a: cross-workspace taskless target → Unauthorized.
    #[test]
    fn send_input_session_caller_cross_workspace_is_unauthorized() {
        let state = state_with_session_in_workspace("ts-caller", "ws-1");
        add_session(&state, "ts-target", "ws-2");
        let resp = dispatch_request(
            &state,
            &session_request(
                "send_input",
                serde_json::json!({ "session_uid": "ts-target", "text": "hi" }),
                "ts-caller",
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::Unauthorized,
        );
    }

    #[test]
    fn send_input_operator_caller_routes_to_methods_layer() {
        let state = state_with_session("ts-live");
        // Missing `text` → InvalidParams from the methods layer.
        let resp = dispatch_request(
            &state,
            &operator_request(
                "send_input",
                serde_json::json!({ "session_uid": "ts-live" }),
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::InvalidParams,
            "operator with malformed params reaches the methods layer",
        );
    }

    /// Sub-2a: Session-caller `kill_session` of a sibling
    /// in the same workspace (taskless caller) passes auth and
    /// the target is removed.
    #[test]
    fn kill_session_session_caller_taskless_same_workspace_removes_target() {
        let state = state_with_session_in_workspace("ts-caller", "ws-shared");
        add_session(&state, "ts-victim", "ws-shared");
        let resp = dispatch_request(
            &state,
            &session_request(
                "kill_session",
                serde_json::json!({ "session_uid": "ts-victim" }),
                "ts-caller",
            ),
        ).into_response();
        assert!(resp.ok, "same-workspace kill must succeed: {:?}", resp.error);
        let s = state.lock().unwrap();
        assert!(!s.sessions.contains_key("ts-victim"));
        assert!(s.sessions.contains_key("ts-caller"));
    }

    /// Sub-2a: Session-caller `kill_session` of a target in a
    /// different workspace is rejected.
    #[test]
    fn kill_session_session_caller_cross_workspace_is_unauthorized() {
        let state = state_with_session_in_workspace("ts-caller", "ws-1");
        add_session(&state, "ts-other", "ws-2");
        let resp = dispatch_request(
            &state,
            &session_request(
                "kill_session",
                serde_json::json!({ "session_uid": "ts-other" }),
                "ts-caller",
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::Unauthorized,
        );
    }

    #[test]
    fn kill_session_operator_removes_live_session_via_dispatcher() {
        let state = state_with_session("ts-live");
        assert!(state.lock().unwrap().sessions.contains_key("ts-live"));

        let resp = dispatch_request(
            &state,
            &operator_request(
                "kill_session",
                serde_json::json!({ "session_uid": "ts-live" }),
            ),
        ).into_response();
        assert!(resp.ok, "kill must succeed: {:?}", resp.error);
        assert!(
            !state.lock().unwrap().sessions.contains_key("ts-live"),
            "dispatcher arm routes through methods::kill_session and removes the entry",
        );
    }

    #[test]
    fn kill_session_unknown_uid_returns_not_found_via_dispatcher() {
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "kill_session",
                serde_json::json!({ "session_uid": "ts-ghost" }),
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::NotFound,
        );
    }

    /// Sub-2a: Session-caller `read_session_output` of self
    /// passes auth (and returns a real snapshot, not just
    /// InvalidParams — read_session_output only needs the
    /// session_uid).
    #[test]
    fn read_session_output_session_caller_self_passes_auth() {
        let state = state_with_session_in_workspace("ts-self", "ws-x");
        let resp = dispatch_request(
            &state,
            &session_request(
                "read_session_output",
                serde_json::json!({ "session_uid": "ts-self" }),
                "ts-self",
            ),
        ).into_response();
        assert!(resp.ok, "self-target must pass: {:?}", resp.error);
    }

    /// Cross-workspace target gets Unauthorized.
    #[test]
    fn read_session_output_session_caller_cross_workspace_is_unauthorized() {
        let state = state_with_session_in_workspace("ts-caller", "ws-1");
        add_session(&state, "ts-other", "ws-2");
        let resp = dispatch_request(
            &state,
            &session_request(
                "read_session_output",
                serde_json::json!({ "session_uid": "ts-other" }),
                "ts-caller",
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(
            resp.error.expect("error body").code,
            ErrorCode::Unauthorized,
        );
    }

    #[test]
    fn read_session_output_operator_returns_snapshot_envelope() {
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &operator_request(
                "read_session_output",
                serde_json::json!({ "session_uid": "ts-live" }),
            ),
        ).into_response();
        assert!(resp.ok, "operator must succeed: {:?}", resp.error);
        let result = resp.result.expect("result body");
        // Envelope shape contract — bytes (string), cursor (u64),
        // start_offset (u64), evicted_since_cursor (bool), closed (bool).
        assert!(result["bytes"].is_string());
        assert!(result["cursor"].is_number());
        assert!(result["start_offset"].is_number());
        assert!(result["evicted_since_cursor"].is_boolean());
        assert!(result["closed"].is_boolean());
    }

    // ============================================================
    // Slice 10d-mcp-surface-1: scaffolding-only tests.
    //
    // The Session-caller dispatch arms were briefly auth-flipped
    // during sub-1 development; review caught three findings
    // (same-workspace widening vs TUI rule, list_sessions wire
    // mismatch with Python MCP tool, start_session wire mismatch).
    // Sub-1 reverted the flips with TODO(sub-2) markers and kept
    // the auth scaffolding (auth module + workspace_id threading
    // on DaemonSession). Tests here pin the reverted shape so
    // sub-2 inherits a clean baseline.
    // ============================================================

    // --- list_sessions (Operator-side; sub-2 owns Session-side) ------

    /// Operator caller sees every session in the wire shape the
    /// Python MCP tool expects: top-level JSON array of
    /// `{session_uid, label, type, state, idle, managed_by_uid}`
    /// objects (mirrors `mcp_server/server.py:319-322`).
    #[test]
    fn list_sessions_operator_returns_python_mcp_tool_wire_shape() {
        let state = state_with_session_in_workspace("ts-a", "ws-1");
        add_session(&state, "ts-b", "ws-2");
        let req = operator_request("list_sessions", serde_json::Value::Null);
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok, "operator must succeed: {:?}", resp.error);
        let result = resp.result.expect("result body");
        // Top-level array — NOT `{sessions: [...]}`. This is
        // the contract `mcp_server/server.py:660` iterates.
        let sessions = result.as_array().expect("response is a top-level array");
        assert_eq!(sessions.len(), 2, "operator sees all sessions");
        let uids: Vec<&str> = sessions.iter()
            .map(|s| s["session_uid"].as_str().unwrap())
            .collect();
        assert_eq!(uids, vec!["ts-a", "ts-b"], "stable order by session_uid");
        // Each entry has the Python tool's expected fields.
        for s in sessions {
            assert!(s["session_uid"].is_string());
            assert!(s["label"].is_string());
            assert!(s["type"].is_string());
            assert!(s["state"].is_string());
            assert!(s["idle"].is_boolean());
            // managed_by_uid is null for sessions spawned without
            // an MCP parent (the default for these test sessions).
            assert!(s["managed_by_uid"].is_null());
            // Type defaults to "claude-code" (the SpawnParams
            // default). `state` is `"pending"` because the test
            // helper doesn't set transcript_path — sub-2b-1
            // review-r#3 #1 unified this with
            // resolve_authorized_session, which also reports
            // "pending" for sessions without a path. `idle` is
            // `false` because sub-2b-1 review-r#4 #1 stamps
            // spawn-time at construction; a freshly-spawned
            // session reports busy until `IDLE_THRESHOLD` of
            // post-spawn quiet. Both keys' shapes are still
            // pinned above (is_string / is_boolean).
            assert_eq!(s["state"], "pending");
            assert_eq!(s["idle"], false);
        }
    }

    /// Sub-2a: `task_id` filter is honored. A session whose
    /// `task_id` is not the filter (nor a descendant in the
    /// task tree) is excluded. `include_exited` stays a no-op
    /// at sub-2a (no tombstones daemon-side until slice 10e).
    #[test]
    fn list_sessions_honors_task_id_filter() {
        let state = state_with_session_in_workspace("ts-a", "ws-1");
        // Set ts-a's task_id by re-adding with the typed helper
        // (the basic helper leaves task_id None).
        {
            let mut s = state.lock().unwrap();
            s.sessions.get_mut("ts-a").unwrap().task_id = Some("task-target".into());
        }
        add_session(&state, "ts-other", "ws-1");
        // ts-other is taskless — should be filtered out by
        // task_id filter.
        let req = operator_request(
            "list_sessions",
            serde_json::json!({
                "task_id": "task-target",
            }),
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok, "params should be accepted: {:?}", resp.error);
        let arr = resp.result.unwrap();
        let arr = arr.as_array().expect("top-level array");
        // Only ts-a matches (its task_id == "task-target");
        // ts-other has no task_id and is excluded.
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["session_uid"], "ts-a");
    }

    /// `include_exited` stays a no-op until slice 10e plumbs
    /// tombstones daemon-side. Pin the wire shape so the
    /// Python tool's signature is accepted.
    #[test]
    fn list_sessions_accepts_include_exited_param() {
        let state = state_with_session_in_workspace("ts-a", "ws-1");
        let req = operator_request(
            "list_sessions",
            serde_json::json!({ "include_exited": true }),
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok);
        let arr = resp.result.unwrap();
        let arr = arr.as_array().expect("top-level array");
        // include_exited is a no-op at sub-2a — only live
        // entries returned regardless.
        assert_eq!(arr.len(), 1);
    }

    /// Slice 10d-mcp-surface-1 review fix #1: the `type` field
    /// reflects the actual session_type stored on `DaemonSession`,
    /// not the default. Pre-fix this defaulted to "claude-code"
    /// for every session (rpc_start_session never sent the
    /// field on the wire); the Python MCP tool's dispatch on
    /// `type` would have misrouted codex / bash sessions.
    #[test]
    fn list_sessions_returns_correct_type_for_codex_session() {
        let state = state_with_session_in_workspace("ts-claude", "ws-1");
        add_session_typed(&state, "ts-codex", "ws-1", "codex");
        let req = operator_request("list_sessions", serde_json::Value::Null);
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok);
        let sessions = resp.result.unwrap();
        let arr = sessions.as_array().expect("top-level array");
        let codex = arr.iter()
            .find(|s| s["session_uid"] == "ts-codex")
            .expect("codex entry");
        assert_eq!(codex["type"], "codex", "codex session must surface as type='codex'");
        let claude = arr.iter()
            .find(|s| s["session_uid"] == "ts-claude")
            .expect("claude entry");
        assert_eq!(claude["type"], "claude-code");
    }

    #[test]
    fn list_sessions_returns_correct_type_for_bash_session() {
        let state = state_with_session_in_workspace("ts-c", "ws-1");
        add_session_typed(&state, "ts-shell", "ws-1", "bash");
        let req = operator_request("list_sessions", serde_json::Value::Null);
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok);
        let arr = resp.result.unwrap();
        let arr = arr.as_array().expect("top-level array");
        let shell = arr.iter()
            .find(|s| s["session_uid"] == "ts-shell")
            .expect("bash entry");
        assert_eq!(shell["type"], "bash");
    }

    /// Slice 10d-mcp-surface-1 review fix #2 + sub-2b-1 review-r#3 #1:
    /// `state` is one of the Python tool's `ready|pending|exited`
    /// enum (pre-fix it was `"running"`, which the tool didn't
    /// recognize). Under r#3, the value is computed from
    /// `transcript_path` (Some → ready, None → pending) instead
    /// of being hardcoded. The pre-r#3 test asserted "ready"
    /// because list_sessions hardcoded it; post-r#3 a session
    /// without a transcript_path correctly reports "pending"
    /// (matching `resolve_authorized_session`).
    ///
    /// Both flavors verified here:
    ///   - default test-helper session → pending (no path).
    ///   - same session after `set_transcript_path` → ready.
    #[test]
    fn list_sessions_emits_helper_driven_state_matching_resolve() {
        let state = state_with_session_in_workspace("ts-a", "ws-1");
        // No transcript_path → pending.
        let arr = dispatch_request(
            &state,
            &operator_request("list_sessions", serde_json::Value::Null),
        ).into_response().result.unwrap();
        let entry = &arr.as_array().expect("top-level array")[0];
        assert_eq!(
            entry["state"], "pending",
            "fresh session without transcript_path: pending",
        );
        // Set path → ready.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-a",
                    "transcript_path": "/tmp/x.jsonl",
                }),
            ),
        );
        let arr = dispatch_request(
            &state,
            &operator_request("list_sessions", serde_json::Value::Null),
        ).into_response().result.unwrap();
        let entry = &arr.as_array().expect("top-level array")[0];
        assert_eq!(
            entry["state"], "ready",
            "after set_transcript_path: ready",
        );
    }

    /// Slice 10d-mcp-surface-1 review fix #1: the daemon
    /// validates `session_type` at the wire boundary. Unknown
    /// values get `InvalidParams` rather than landing on
    /// `DaemonSession.session_type` and propagating downstream.
    #[test]
    fn start_session_with_invalid_session_type_returns_invalid_params() {
        // Operator-callable path is the only Session-typed entry
        // sub-1 exposes (Session-caller dispatch is reverted to
        // Unauthorized). Verify the validator runs for
        // Operator callers — the typed boundary.
        let state = make_state();
        let req = operator_request(
            "start_session",
            serde_json::json!({
                "uid": "ts-deadbeef-1",
                "workspace_id": "ws-test",
                "label": "x",
                "session_type": "future_unknown_engine",
                "argv": ["/bin/bash"],
                "working_dir": "/tmp",
            }),
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(
            err.message.contains("session_type") && err.message.contains("claude-code"),
            "error should name the field and list canonical values: {}",
            err.message,
        );
    }

    /// Sub-2a: Session-caller `list_sessions` is auth-checked
    /// via the TUI-mirror rule. A taskless caller sees only
    /// same-workspace siblings; cross-workspace sessions are
    /// filtered out.
    #[test]
    fn list_sessions_session_caller_taskless_scopes_to_own_workspace() {
        let state = state_with_session_in_workspace("ts-caller", "ws-1");
        add_session(&state, "ts-sibling", "ws-1");
        add_session(&state, "ts-other", "ws-2");
        let req = session_request("list_sessions", serde_json::Value::Null, "ts-caller");
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok, "must succeed: {:?}", resp.error);
        let arr = resp.result.unwrap();
        let arr = arr.as_array().expect("top-level array");
        let uids: Vec<&str> = arr.iter().map(|s| s["session_uid"].as_str().unwrap()).collect();
        // Caller sees itself + same-workspace sibling. Cross-
        // workspace ts-other is filtered out by the auth scope.
        assert_eq!(uids, vec!["ts-caller", "ts-sibling"]);
    }

    /// Session-caller not in registry → Unauthorized (caught
    /// at dispatch before reaching the method body).
    #[test]
    fn list_sessions_session_caller_not_in_registry_is_unauthorized() {
        let state = state_with_session_in_workspace("ts-a", "ws-1");
        let req = session_request("list_sessions", serde_json::Value::Null, "ts-ghost");
        let resp = dispatch_request(&state, &req).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(
            err.message.contains("not in the daemon registry"),
            "error should name the missing-caller cause: {}",
            err.message,
        );
    }

    // ============================================================
    // task.update_tree (sub-2a TUI-pushed snapshot)
    // ============================================================

    #[test]
    fn task_update_tree_operator_replaces_snapshot() {
        let state = make_state();
        let req = operator_request(
            "task.update_tree",
            serde_json::json!({
                "tasks": [
                    { "task_id": "task-root", "parent_task_id": null },
                    { "task_id": "task-child", "parent_task_id": "task-root" },
                ],
            }),
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok, "operator must succeed: {:?}", resp.error);
        let result = resp.result.unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["task_count"], 2);
        // Verify the snapshot landed in state.
        let s = state.lock().unwrap();
        assert_eq!(s.task_tree.get("task-root"), Some(&None));
        assert_eq!(
            s.task_tree.get("task-child"),
            Some(&Some("task-root".to_string())),
        );
    }

    #[test]
    fn task_update_tree_replaces_not_merges() {
        let state = make_state();
        // First push: two tasks.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "task.update_tree",
                serde_json::json!({
                    "tasks": [
                        { "task_id": "old-a", "parent_task_id": null },
                        { "task_id": "old-b", "parent_task_id": null },
                    ],
                }),
            ),
        );
        // Second push: replace with a different tree.
        let resp = dispatch_request(
            &state,
            &operator_request(
                "task.update_tree",
                serde_json::json!({
                    "tasks": [
                        { "task_id": "new-a", "parent_task_id": null },
                    ],
                }),
            ),
        ).into_response();
        assert!(resp.ok);
        let s = state.lock().unwrap();
        // Old tasks gone, new task present.
        assert_eq!(s.task_tree.len(), 1);
        assert!(!s.task_tree.contains_key("old-a"));
        assert!(!s.task_tree.contains_key("old-b"));
        assert!(s.task_tree.contains_key("new-a"));
    }

    #[test]
    fn task_update_tree_session_caller_is_unauthorized() {
        let state = state_with_session_in_workspace("ts-caller", "ws-x");
        let req = session_request(
            "task.update_tree",
            serde_json::json!({ "tasks": [] }),
            "ts-caller",
        );
        let resp = dispatch_request(&state, &req).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(
            err.message.contains("escape their own auth scope"),
            "error should explain why: {}",
            err.message,
        );
    }

    /// End-to-end: TUI pushes the task tree → daemon caches →
    /// a tasked Session caller can act on a descendant target
    /// across workspaces. Pins the full sub-2a contract.
    #[test]
    fn task_subtree_auth_e2e_descendant_across_workspaces() {
        // ts-parent in ws-1 has task-parent; ts-child in ws-2
        // has task-child whose parent is task-parent. Without
        // the task tree, parent → child is OutOfScope (different
        // workspaces, no shared task). With the tree pushed,
        // it's Allow.
        let state = make_state();
        // Insert both sessions via add_session_typed-style with
        // task_id set.
        {
            let mut sp = crate::session::SpawnParams::new(
                "ts-parent",
                "parent",
                "/bin/sleep",
            );
            sp.args = vec!["30".into()];
            sp.workspace_id = "ws-1".into();
            sp.task_id = Some("task-parent".into());
            let session = crate::session::DaemonSession::spawn(sp).unwrap();
            state.lock().unwrap().sessions.insert("ts-parent".into(), session);
        }
        {
            let mut sp = crate::session::SpawnParams::new(
                "ts-child",
                "child",
                "/bin/sleep",
            );
            sp.args = vec!["30".into()];
            sp.workspace_id = "ws-2".into();
            sp.task_id = Some("task-child".into());
            let session = crate::session::DaemonSession::spawn(sp).unwrap();
            state.lock().unwrap().sessions.insert("ts-child".into(), session);
        }
        // Before pushing the tree, parent → child is OutOfScope.
        let before = dispatch_request(
            &state,
            &session_request(
                "send_input",
                serde_json::json!({ "session_uid": "ts-child", "text": "hi" }),
                "ts-parent",
            ),
        ).into_response();
        assert!(!before.ok);
        assert_eq!(before.error.unwrap().code, ErrorCode::Unauthorized);
        // TUI pushes the task tree.
        let push = dispatch_request(
            &state,
            &operator_request(
                "task.update_tree",
                serde_json::json!({
                    "tasks": [
                        { "task_id": "task-parent", "parent_task_id": null },
                        { "task_id": "task-child", "parent_task_id": "task-parent" },
                    ],
                }),
            ),
        ).into_response();
        assert!(push.ok);
        // Now parent → child succeeds (reaches methods layer,
        // which InvalidParams's only because real send_input
        // would deliver bytes — we want to confirm auth passed).
        let after = dispatch_request(
            &state,
            &session_request(
                "send_input",
                serde_json::json!({ "session_uid": "ts-child" }),
                "ts-parent",
            ),
        ).into_response();
        assert!(!after.ok);
        // Reached the methods layer (InvalidParams on missing
        // `text`) — that's the "got past auth" signal.
        assert_eq!(
            after.error.unwrap().code,
            ErrorCode::InvalidParams,
            "tasked-caller descendant target must pass auth after task tree push",
        );
    }

    // ============================================================
    // resolve_authorized_session (sub-2b-1)
    // ============================================================

    /// Helper: insert a session with a known `transcript_path`
    /// at spawn time, for the "ready" branch.
    fn add_session_with_transcript(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        workspace_id: &str,
        transcript_path: &str,
    ) {
        let mut params = crate::session::SpawnParams::new(uid, "test", "/bin/sleep");
        params.args = vec!["30".into()];
        params.workspace_id = workspace_id.to_string();
        params.transcript_path = Some(transcript_path.to_string());
        let session = crate::session::DaemonSession::spawn(params).expect("spawn ok");
        let mut s = state.lock().unwrap();
        s.sessions.insert(uid.into(), session);
    }

    /// Operator caller, transcript_path supplied at spawn time →
    /// state=ready, transcript_path in response, engine derived
    /// from session_type. Mirrors the wire shape the Python MCP
    /// `read_session_output` tool's first leg expects.
    #[test]
    fn resolve_authorized_session_operator_ready_echoes_transcript_path() {
        let state = make_state();
        add_session_with_transcript(
            &state,
            "ts-live",
            "ws-1",
            "/home/user/.claude/projects/encoded/abc-123.jsonl",
        );
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-live" }),
            ),
        ).into_response();
        assert!(resp.ok, "operator must succeed: {:?}", resp.error);
        let r = resp.result.expect("result");
        assert_eq!(r["state"], "ready");
        assert_eq!(r["engine"], "claude-code");
        assert_eq!(
            r["transcript_path"],
            "/home/user/.claude/projects/encoded/abc-123.jsonl",
        );
        // Generation defaults to 0 (daemon doesn't track /clear
        // yet — sub-2b-1 review-r#2 #2 bumps on path change,
        // and `add_session_with_transcript` skips the
        // `set_transcript_path` rpc that increments). Idle is
        // computed from `last_activity_at`; sub-2b-1 review-r#4
        // #1 stamps spawn-time at construction so a fresh
        // session reports `idle: false` until `IDLE_THRESHOLD`
        // has elapsed. Dedicated tests pin the idle semantics
        // independently (`resolve_idle_*`,
        // `send_input_bumps_last_activity_*`).
        assert_eq!(r["generation"], 0);
        assert_eq!(r["idle"], false);
    }

    /// Session without a `transcript_path` (the common fresh-
    /// spawn case before any detection RPC) → state=pending,
    /// transcript_path=null. Python tool short-circuits to
    /// empty messages + poll-again on pending.
    #[test]
    fn resolve_authorized_session_no_transcript_returns_pending() {
        let state = make_state();
        add_session(&state, "ts-fresh", "ws-1"); // helper sets no transcript_path
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-fresh" }),
            ),
        ).into_response();
        assert!(resp.ok);
        let r = resp.result.expect("result");
        assert_eq!(r["state"], "pending");
        assert!(r["transcript_path"].is_null());
    }

    /// Codex sessions get `engine: "codex"` (the Python tool
    /// dispatches its parser on this field — wrong value would
    /// route a Codex transcript through the Claude parser).
    #[test]
    fn resolve_authorized_session_engine_string_matches_session_type_for_codex() {
        let state = make_state();
        let mut params = crate::session::SpawnParams::new("ts-cdx", "test", "/bin/sleep");
        params.args = vec!["30".into()];
        params.workspace_id = "ws-1".into();
        params.session_type = "codex".into();
        params.transcript_path = Some("/home/u/.codex/sessions/2026/01/15/x.jsonl".into());
        let session = crate::session::DaemonSession::spawn(params).expect("spawn");
        state.lock().unwrap().sessions.insert("ts-cdx".into(), session);
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-cdx" }),
            ),
        ).into_response();
        assert!(resp.ok);
        assert_eq!(resp.result.unwrap()["engine"], "codex");
    }

    /// Unknown session_uid → NotFound (operator path; Session
    /// callers hit auth first).
    #[test]
    fn resolve_authorized_session_unknown_uid_is_not_found() {
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-ghost" }),
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, ErrorCode::NotFound);
    }

    /// Session-caller targeting self → Allow → ready/pending
    /// shape returned. Pin the auth-passing path so a regression
    /// that wires auth incorrectly would surface here.
    #[test]
    fn resolve_authorized_session_session_caller_self_passes_auth() {
        let state = state_with_session_in_workspace("ts-self", "ws-x");
        let resp = dispatch_request(
            &state,
            &session_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-self" }),
                "ts-self",
            ),
        ).into_response();
        assert!(resp.ok, "self must pass auth: {:?}", resp.error);
        // No transcript supplied at spawn → pending.
        assert_eq!(resp.result.unwrap()["state"], "pending");
    }

    /// Session-caller targeting a cross-workspace sibling →
    /// Unauthorized (taskless caller can't reach across
    /// workspaces; same rule sub-2a wired for the other arms).
    #[test]
    fn resolve_authorized_session_cross_workspace_is_unauthorized() {
        let state = state_with_session_in_workspace("ts-caller", "ws-1");
        add_session(&state, "ts-other", "ws-2");
        let resp = dispatch_request(
            &state,
            &session_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-other" }),
                "ts-caller",
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, ErrorCode::Unauthorized);
    }

    // ============================================================
    // session.set_transcript_path (sub-2b-1 review #1)
    // ============================================================

    /// Operator push lands on `DaemonSession.transcript_path` and
    /// the next `resolve_authorized_session` returns
    /// `state: "ready"` with the supplied value. The fix for the
    /// "transcript_path always None in production" finding.
    #[test]
    fn set_transcript_path_operator_updates_daemon_session() {
        let state = make_state();
        add_session(&state, "ts-late", "ws-1");
        // Before push: pending.
        let before = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-late" }),
            ),
        ).into_response();
        assert_eq!(before.result.unwrap()["state"], "pending");
        // Push.
        let set = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-late",
                    "transcript_path": "/home/u/.claude/projects/x/late.jsonl",
                }),
            ),
        ).into_response();
        assert!(set.ok, "set must succeed: {:?}", set.error);
        // After push: ready.
        let after = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-late" }),
            ),
        ).into_response();
        let r = after.result.unwrap();
        assert_eq!(r["state"], "ready");
        assert_eq!(r["transcript_path"], "/home/u/.claude/projects/x/late.jsonl");
    }

    /// Session-caller cannot push transcript_path — Operator-only.
    /// Defends against a Session-caller redirecting the Python
    /// MCP `read_session_output` tool to an attacker-chosen file.
    #[test]
    fn set_transcript_path_session_caller_is_unauthorized() {
        let state = state_with_session_in_workspace("ts-caller", "ws-x");
        let resp = dispatch_request(
            &state,
            &session_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-caller",
                    "transcript_path": "/etc/passwd",
                }),
                "ts-caller",
            ),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(
            err.message.contains("attacker-chosen"),
            "error must spell out the threat model: {}",
            err.message,
        );
    }

    /// Unknown session_uid → NotFound (the standard
    /// dispatch-layer parameter validation).
    #[test]
    fn set_transcript_path_unknown_uid_is_not_found() {
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-ghost",
                    "transcript_path": "/whatever",
                }),
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, ErrorCode::NotFound);
    }

    /// Empty transcript_path → InvalidParams. Callers should not
    /// push to clear (let the session naturally exit instead).
    #[test]
    fn set_transcript_path_empty_string_is_invalid_params() {
        let state = make_state();
        add_session(&state, "ts-fresh", "ws-1");
        let resp = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-fresh",
                    "transcript_path": "",
                }),
            ),
        ).into_response();
        assert!(!resp.ok);
        assert_eq!(resp.error.unwrap().code, ErrorCode::InvalidParams);
    }

    /// Re-push updates the stored value (e.g. /clear-driven
    /// rebind). Pin the latest-wins semantic.
    #[test]
    fn set_transcript_path_repush_overwrites() {
        let state = make_state();
        add_session(&state, "ts-rebind", "ws-1");
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-rebind",
                    "transcript_path": "/path/v1.jsonl",
                }),
            ),
        );
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-rebind",
                    "transcript_path": "/path/v2.jsonl",
                }),
            ),
        );
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-rebind" }),
            ),
        ).into_response();
        assert_eq!(
            resp.result.unwrap()["transcript_path"],
            "/path/v2.jsonl",
        );
    }

    // ============================================================
    // resolve_authorized_session.idle (sub-2b-1 review #2)
    // ============================================================

    /// Sub-2b-1 review-r#4 #1: fresh session reports
    /// `idle: false` because spawn-time is stamped at
    /// construction. Pre-r#4 `last_activity_at` was `None`,
    /// which the idle predicate mapped to "infinitely long
    /// ago" → idle=true. Agents polling
    /// `wait_for_session_idle` would observe idle=true on a
    /// session that hadn't even attached its transcript yet
    /// and return prematurely.
    ///
    /// (Time-based flip to idle=true after
    /// `IDLE_THRESHOLD` of post-spawn quiet is exercised by
    /// `resolve_idle_flips_true_after_threshold_of_quiet`.)
    #[test]
    fn resolve_idle_false_immediately_after_spawn() {
        let state = make_state();
        add_session(&state, "ts-fresh-spawn", "ws-1");
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-fresh-spawn" }),
            ),
        ).into_response();
        assert_eq!(
            resp.result.unwrap()["idle"],
            false,
            "fresh session must not report idle=true — \
             spawn-time stamp protects against \
             wait_for_session_idle returning before the agent \
             has had a chance to attach its transcript",
        );
    }

    /// Sub-2b-1 review-r#2 #1: `send_input` bumps
    /// `last_activity_at`. Pre-fix the daemon only stamped
    /// output, so an agent calling `send_input` then
    /// `wait_for_session_idle` would return early because the
    /// daemon never observed input as activity.
    #[test]
    fn send_input_bumps_last_activity_so_idle_flips_false() {
        // Spawn /bin/sleep (no output) and verify the session
        // is idle initially.
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().display().to_string();
        let state = make_state();
        let uid = format!(
            "ts-{:x}-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            42,
        );
        let spawn = dispatch_request(
            &state,
            &operator_request(
                "start_session",
                serde_json::json!({
                    "uid": &uid,
                    "workspace_id": "ws-act",
                    "label": "act",
                    "argv": ["/bin/sleep", "30"],
                    "working_dir": &worktree,
                    "worktree_path": &worktree,
                }),
            ),
        ).into_response();
        assert!(spawn.ok, "spawn: {:?}", spawn.error);
        // Wait past IDLE_THRESHOLD so any spawn-time noise has
        // gone quiet (cat/sleep emit no bytes; reader thread
        // just blocks). The pre-spawn baseline last_activity
        // is None, so this might already be idle — sleep
        // anyway to be deterministic.
        std::thread::sleep(std::time::Duration::from_millis(2_100));
        let before = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": &uid }),
            ),
        ).into_response();
        assert_eq!(before.result.unwrap()["idle"], true);
        // Send input. /bin/sleep ignores stdin, so this bump
        // does NOT cause output activity — input alone must
        // suffice for the daemon to see idle=false.
        let send = dispatch_request(
            &state,
            &operator_request(
                "send_input",
                serde_json::json!({
                    "session_uid": &uid,
                    "text": "ignored-by-sleep",
                }),
            ),
        ).into_response();
        assert!(send.ok, "send_input: {:?}", send.error);
        // Immediately after send: idle MUST be false.
        let after = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": &uid }),
            ),
        ).into_response();
        assert_eq!(
            after.result.unwrap()["idle"],
            false,
            "send_input must bump activity — pre-fix this stayed true \
             because only output bumped the clock",
        );
        // Cleanup.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "kill_session",
                serde_json::json!({ "session_uid": &uid }),
            ),
        );
    }

    /// Sub-2b-1 review-r#2 #2: `set_transcript_path` increments
    /// `generation` on path change. Idempotent re-pushes
    /// (same path) MUST NOT bump — otherwise idle-polling
    /// TUI detector re-pushes invalidate the agent's cursor
    /// every tick.
    #[test]
    fn set_transcript_path_increments_generation_on_change_only() {
        let state = make_state();
        add_session(&state, "ts-gen", "ws-1");
        // Initial resolve: generation=0 (no path set).
        let r0 = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-gen" }),
            ),
        ).into_response();
        assert_eq!(r0.result.unwrap()["generation"], 0);
        // Set path A: first transition, generation = 1.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-gen",
                    "transcript_path": "/a/x.jsonl",
                }),
            ),
        );
        let r1 = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-gen" }),
            ),
        ).into_response();
        assert_eq!(r1.result.unwrap()["generation"], 1);
        // Re-push same path A: no bump.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-gen",
                    "transcript_path": "/a/x.jsonl",
                }),
            ),
        );
        let r1b = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-gen" }),
            ),
        ).into_response();
        assert_eq!(
            r1b.result.unwrap()["generation"],
            1,
            "idempotent re-push of same path MUST NOT bump generation",
        );
        // Set path B (rotation): generation = 2.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-gen",
                    "transcript_path": "/b/y.jsonl",
                }),
            ),
        );
        let r2 = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-gen" }),
            ),
        ).into_response();
        assert_eq!(r2.result.unwrap()["generation"], 2);
        // Set path A again (rotate back): generation = 3.
        // Idempotency is on the PATH, not the file identity —
        // the cursor must invalidate.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-gen",
                    "transcript_path": "/a/x.jsonl",
                }),
            ),
        );
        let r3 = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-gen" }),
            ),
        ).into_response();
        assert_eq!(r3.result.unwrap()["generation"], 3);
    }

    // ============================================================
    // Sub-2b-1 review-r#3 #1: list_sessions ↔ resolve agreement
    // ============================================================

    /// Both methods must report the same `(state, idle)` for
    /// the same session under every reachable combination:
    ///   - pending + fresh spawn → state=pending, idle=false
    ///     (spawn-time stamp; sub-2b-1 review-r#4 #1)
    ///   - pending + post-threshold quiet → state=pending,
    ///     idle=true
    ///   - ready + fresh spawn → state=ready, idle=false
    ///   - ready + recent activity → state=ready, idle=false
    ///   - ready + post-threshold quiet → state=ready,
    ///     idle=true (covered by other tests; one
    ///     time-passes case is enough here)
    ///
    /// Pre-fix list_sessions hardcoded `("ready", false)`, so a
    /// caller polling list_sessions for "session is ready and
    /// idle" would observe a different answer than the same
    /// caller resolving via resolve_authorized_session. The
    /// Python MCP `wait_for_session_idle` polls list_sessions
    /// while `read_session_output` resolves through
    /// resolve_authorized_session — divergent answers broke
    /// the wait-then-read flow.
    #[test]
    fn list_sessions_and_resolve_agree_on_state_and_idle() {
        let state = make_state();
        add_session(&state, "ts-agree", "ws-1");

        // Case 1: fresh spawn, no transcript_path.
        // Both methods → state=pending, idle=false (spawn-time
        // stamp keeps the session busy until IDLE_THRESHOLD).
        let r1_resolve = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-agree" }),
            ),
        ).into_response().result.unwrap();
        let r1_list = dispatch_request(
            &state,
            &operator_request("list_sessions", serde_json::Value::Null),
        ).into_response().result.unwrap();
        let r1_list_entry = r1_list.as_array().unwrap()
            .iter()
            .find(|s| s["session_uid"] == "ts-agree")
            .expect("ts-agree in list");
        assert_eq!(r1_resolve["state"], "pending");
        assert_eq!(r1_list_entry["state"], "pending");
        assert_eq!(r1_resolve["idle"], false);
        assert_eq!(r1_list_entry["idle"], false);
        assert_eq!(
            r1_resolve["state"], r1_list_entry["state"],
            "resolve.state must match list.state",
        );
        assert_eq!(
            r1_resolve["idle"], r1_list_entry["idle"],
            "resolve.idle must match list.idle",
        );

        // Case 2: set transcript path → state=ready, both
        // methods.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-agree",
                    "transcript_path": "/tmp/agree.jsonl",
                }),
            ),
        );
        let r2_resolve = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-agree" }),
            ),
        ).into_response().result.unwrap();
        let r2_list = dispatch_request(
            &state,
            &operator_request("list_sessions", serde_json::Value::Null),
        ).into_response().result.unwrap();
        let r2_list_entry = r2_list.as_array().unwrap()
            .iter()
            .find(|s| s["session_uid"] == "ts-agree")
            .expect("ts-agree in list");
        assert_eq!(r2_resolve["state"], "ready");
        assert_eq!(r2_list_entry["state"], "ready");
        assert_eq!(
            r2_resolve["state"], r2_list_entry["state"],
            "ready: resolve.state == list.state",
        );

        // Case 3: drive activity via fanout push → both
        // methods report idle=false.
        {
            let s = state.lock().unwrap();
            s.sessions["ts-agree"].fanout.push(b"hi");
        }
        let r3_resolve = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-agree" }),
            ),
        ).into_response().result.unwrap();
        let r3_list = dispatch_request(
            &state,
            &operator_request("list_sessions", serde_json::Value::Null),
        ).into_response().result.unwrap();
        let r3_list_entry = r3_list.as_array().unwrap()
            .iter()
            .find(|s| s["session_uid"] == "ts-agree")
            .expect("ts-agree in list");
        assert_eq!(r3_resolve["idle"], false);
        assert_eq!(r3_list_entry["idle"], false);
        assert_eq!(
            r3_resolve["idle"], r3_list_entry["idle"],
            "post-activity: resolve.idle == list.idle",
        );
    }

    // ============================================================
    // Sub-2b-1 review-r#3 #2: attach-stream input bumps idle
    // ============================================================

    /// Drive an Input frame through the attach-stream path and
    /// verify it bumps `last_activity_at`. Pre-fix the stream
    /// path cloned only the writer Arc and skipped the activity
    /// stamp — operator typing through attach.open didn't move
    /// the daemon's idle clock.
    ///
    /// We don't go through the full attach socket here (that's
    /// a heavier integration test); we exercise the same
    /// `InputHandle` path the stream handler now uses, which
    /// is what the fix centralizes. The shared helper means
    /// the stream path and `methods::send_input` cannot
    /// diverge by construction.
    #[test]
    fn stream_input_path_via_input_handle_bumps_activity() {
        use std::time::Duration;
        let state = make_state();
        add_session(&state, "ts-stream", "ws-1");
        // Wait past the threshold so the fresh-spawn idle
        // baseline is unambiguously idle=true.
        std::thread::sleep(Duration::from_millis(2_100));
        let before = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-stream" }),
            ),
        ).into_response();
        assert_eq!(before.result.unwrap()["idle"], true);
        // Simulate the stream handler's path: extract the
        // input handle, drop the state lock, call
        // `write_and_stamp` (the same call the stream path
        // makes for every Input frame). The PTY is a real
        // /bin/sleep so writes go to /dev/null effectively;
        // what matters is the activity stamp.
        let handle = {
            let s = state.lock().unwrap();
            s.sessions["ts-stream"].input_handle()
        };
        // /bin/sleep ignores stdin, but write_and_stamp still
        // returns Ok and stamps activity post-write. (If the
        // child were dead the write would Err and stamp would
        // not fire — also correct behavior.)
        handle
            .write_and_stamp(b"keystroke-through-attach\n")
            .expect("write+stamp");
        let after = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-stream" }),
            ),
        ).into_response();
        assert_eq!(
            after.result.unwrap()["idle"],
            false,
            "attach-stream Input must bump activity — pre-r#3 \
             the stream handler skipped the stamp, so an \
             operator typing didn't move the idle clock",
        );
    }

    // ============================================================
    // propose_task (sub-2b-2)
    // ============================================================

    /// Param validation: missing `project` → InvalidParams. Same
    /// shape for `name` and `repo_url`. Tests pin the
    /// daemon-side validator surface (Python tool wrapper
    /// independently enforces some of the same rules).
    #[test]
    fn propose_task_rejects_missing_project() {
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "propose_task",
                serde_json::json!({
                    "project": "",
                    "name": "x",
                    "repo_url": "git@x.com:a/b.git",
                }),
            ),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("project"), "msg: {}", err.message);
    }

    #[test]
    fn propose_task_rejects_missing_name() {
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "propose_task",
                serde_json::json!({
                    "project": "p",
                    "name": "  ",
                    "repo_url": "git@x.com:a/b.git",
                }),
            ),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(err.message.contains("name"), "msg: {}", err.message);
    }

    #[test]
    fn propose_task_rejects_missing_repo_url() {
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "propose_task",
                serde_json::json!({
                    "project": "p",
                    "name": "n",
                    "repo_url": "",
                }),
            ),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, ErrorCode::InvalidParams);
        assert!(
            err.message.contains("repo_url"),
            "must explain why the daemon can't auto-detect: {}",
            err.message,
        );
    }

    /// No `CM_API_URL`/`CM_API_TOKEN` in the test env → the
    /// method body's HTTP call fails with `MissingConfig`,
    /// surfaced as `Internal` with a diagnostic naming the
    /// missing var. Pins the daemon-side error path that
    /// the Python tool surfaces when CM_DAEMON_SOCKET is set
    /// but the daemon wasn't launched with planning-API
    /// credentials.
    #[test]
    fn propose_task_without_api_env_surfaces_internal_with_diagnostic() {
        // Clear any inherited env (other tests in this binary
        // run set/clear these too; serialize via the env_lock
        // in planning_client::tests so we don't race them).
        let _g = crate::planning_client::test_env_lock();
        // SAFETY: serialized by the env_lock above.
        unsafe {
            std::env::remove_var("CM_API_URL");
            std::env::remove_var("CM_API_TOKEN");
        }
        let state = make_state();
        let resp = dispatch_request(
            &state,
            &operator_request(
                "propose_task",
                serde_json::json!({
                    "project": "p",
                    "name": "n",
                    "repo_url": "git@x.com:a/b.git",
                }),
            ),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(err.code, ErrorCode::Internal);
        assert!(
            err.message.contains("CM_API_URL"),
            "diagnostic must name the missing env var so operators \
             know what to configure: {}",
            err.message,
        );
    }

    /// Session callers AND Operator callers both reach the
    /// method body (no auth gate). Pin both via the
    /// missing-env-var path (gives a deterministic
    /// "method body ran" signal without needing a stub HTTP
    /// server in this dispatch-level test). The
    /// `planning_client::tests` cover the actual HTTP-side
    /// success path.
    #[test]
    fn propose_task_allows_session_caller() {
        let _g = crate::planning_client::test_env_lock();
        unsafe {
            std::env::remove_var("CM_API_URL");
            std::env::remove_var("CM_API_TOKEN");
        }
        let state = state_with_session_in_workspace("ts-prop", "ws-1");
        let resp = dispatch_request(
            &state,
            &session_request(
                "propose_task",
                serde_json::json!({
                    "project": "p",
                    "name": "n",
                    "repo_url": "git@x.com:a/b.git",
                }),
                "ts-prop",
            ),
        ).into_response();
        // Session caller passes auth (no gate); the method
        // body's missing-env-var check then surfaces Internal.
        assert!(!resp.ok);
        let err = resp.error.unwrap();
        assert_eq!(
            err.code, ErrorCode::Internal,
            "session caller must pass auth and reach the method body — \
             the failure here is the missing CM_API_URL, not Unauthorized",
        );
    }

    /// Sub-2b-1 review-r#4 #2: simulate the TUI's
    /// workflow-launch transcript binding. The TUI's
    /// `WorkflowControllerCtx::launch_workflow` sets/rebinds
    /// `transcript_id` for each Existing slot (initial-bind
    /// + codex-resume-rebind branches) and then post-loop
    /// calls `push_transcript_path_to_daemon_if_attached`
    /// for each updated index. Verify the daemon side: a
    /// workflow launch followed by the push transitions
    /// pending → ready and resets generation correctly.
    #[test]
    fn workflow_launch_pushes_transcript_path_to_daemon() {
        let state = make_state();
        add_session(&state, "ts-wf", "ws-1");
        // Pre-launch resolve: pending (no transcript yet).
        let pre = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-wf" }),
            ),
        ).into_response();
        let pre_r = pre.result.unwrap();
        assert_eq!(pre_r["state"], "pending");
        assert_eq!(pre_r["generation"], 0);
        // Workflow launch: TUI detects transcript and pushes
        // the path. This is the wire equivalent of what
        // workflow/controller.rs:341+ now does post-loop.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-wf",
                    "transcript_path": "/proj/wf/worker-role.jsonl",
                }),
            ),
        );
        let post = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-wf" }),
            ),
        ).into_response();
        let post_r = post.result.unwrap();
        assert_eq!(post_r["state"], "ready");
        assert_eq!(post_r["transcript_path"], "/proj/wf/worker-role.jsonl");
        assert_eq!(
            post_r["generation"], 1,
            "first transcript_path push bumps gen 0 → 1",
        );
        // Codex resume rebind during workflow launch: same
        // session, different file. Generation must bump.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-wf",
                    "transcript_path": "/proj/wf/worker-resumed.jsonl",
                }),
            ),
        );
        let rebind = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-wf" }),
            ),
        ).into_response();
        let rebind_r = rebind.result.unwrap();
        assert_eq!(rebind_r["transcript_path"], "/proj/wf/worker-resumed.jsonl");
        assert_eq!(
            rebind_r["generation"], 2,
            "rebind to a different path bumps gen 1 → 2",
        );
    }

    /// Sub-2b-1 review-r#2 #3: simulate the TUI's history-
    /// rotation push (a session bound to file A rotates to
    /// file B; the TUI's
    /// `push_transcript_path_to_daemon_if_attached` fires at
    /// the rebind site). Verifies the daemon transitions
    /// cleanly: new path surfaced + generation bumped so any
    /// in-flight Python tool cursor invalidates.
    #[test]
    fn history_rotation_path_swap_bumps_generation_and_surfaces_new_path() {
        let state = make_state();
        add_session(&state, "ts-rotate", "ws-1");
        // Initial bind (discovery loop pushes path A).
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-rotate",
                    "transcript_path": "/proj/x/abc-pre-rotate.jsonl",
                }),
            ),
        );
        // History rotation: TUI detects new sid, calls
        // `ts.rebind_transcript(Some(new_sid))`, then the
        // post-rebind hook pushes the new path.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "session.set_transcript_path",
                serde_json::json!({
                    "session_uid": "ts-rotate",
                    "transcript_path": "/proj/x/def-post-rotate.jsonl",
                }),
            ),
        );
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-rotate" }),
            ),
        ).into_response();
        let r = resp.result.expect("result");
        assert_eq!(r["transcript_path"], "/proj/x/def-post-rotate.jsonl");
        assert_eq!(
            r["generation"], 2,
            "rotation = generation bump; agent's cursor invalidates",
        );
    }

    /// Time-based transition: push bytes (busy), wait past
    /// `IDLE_THRESHOLD` (2s), verify idle flips to true. This
    /// is the actual `wait_for_session_idle` user story
    /// (worker writes a result, agent sees idle and reads the
    /// transcript).
    #[test]
    fn resolve_idle_flips_true_after_threshold_of_quiet() {
        let state = make_state();
        add_session(&state, "ts-flip", "ws-1");
        {
            let s = state.lock().unwrap();
            s.sessions["ts-flip"].fanout.push(b"busy\n");
        }
        // Immediately after push: not idle.
        let busy = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-flip" }),
            ),
        ).into_response();
        assert_eq!(busy.result.unwrap()["idle"], false);
        // Past IDLE_THRESHOLD (2s) of quiet.
        std::thread::sleep(std::time::Duration::from_millis(2_100));
        let after = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-flip" }),
            ),
        ).into_response();
        assert_eq!(
            after.result.unwrap()["idle"],
            true,
            "fanout quiet >= IDLE_THRESHOLD must flip idle=true",
        );
    }

    /// Recent fanout activity → idle=false. The "agent is
    /// producing output" case the Python `wait_for_session_idle`
    /// polls against.
    #[test]
    fn resolve_idle_false_after_recent_fanout_push() {
        let state = make_state();
        add_session(&state, "ts-busy", "ws-1");
        // Drive a fanout push directly (bypasses the PTY reader
        // thread so the test is deterministic).
        {
            let s = state.lock().unwrap();
            s.sessions["ts-busy"].fanout.push(b"hello\n");
        }
        let resp = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": "ts-busy" }),
            ),
        ).into_response();
        assert_eq!(
            resp.result.unwrap()["idle"],
            false,
            "fresh fanout push must report busy (idle=false)",
        );
    }

    /// `transcript_path` rides through `start_session` and lands
    /// on `DaemonSession.transcript_path`, observable via
    /// `resolve_authorized_session`. Pins the wire-shape
    /// addition end-to-end against the real `start_session`
    /// arm (the unit-level field tests above use the test
    /// helper `add_session_with_transcript`).
    #[test]
    fn start_session_threads_transcript_path_into_resolve_response() {
        // Tempdir for working_dir (auto-register branch).
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().display().to_string();
        let state = make_state();
        // Pre-generate a TUI-format uid for the spawn.
        let uid = format!(
            "ts-{:x}-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            0,
        );
        let spawn_resp = dispatch_request(
            &state,
            &operator_request(
                "start_session",
                serde_json::json!({
                    "uid": &uid,
                    "workspace_id": "ws-trsc",
                    "label": "trsc",
                    "argv": ["/bin/sleep", "30"],
                    "working_dir": worktree,
                    "worktree_path": worktree,
                    "transcript_path": "/tmp/x.jsonl",
                }),
            ),
        ).into_response();
        assert!(
            spawn_resp.ok,
            "start_session must accept transcript_path field: {:?}",
            spawn_resp.error,
        );
        let resolved = dispatch_request(
            &state,
            &operator_request(
                "resolve_authorized_session",
                serde_json::json!({ "session_uid": &uid }),
            ),
        ).into_response();
        assert!(resolved.ok);
        let r = resolved.result.expect("result");
        assert_eq!(r["state"], "ready");
        assert_eq!(r["transcript_path"], "/tmp/x.jsonl");
        // Cleanup so the test's reaper thread doesn't outlive
        // the daemon state and trip the lock-on-drop callback.
        let _ = dispatch_request(
            &state,
            &operator_request(
                "kill_session",
                serde_json::json!({ "session_uid": &uid }),
            ),
        );
    }
}
