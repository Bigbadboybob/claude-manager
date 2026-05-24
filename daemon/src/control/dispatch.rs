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
        // Slice 10d-mcp-surface wired `list_sessions`; pick a method
        // still served by the TUI to exercise the deferred-arm fallback.
        let resp = dispatch_request(
            &state,
            &session_request("propose_task", serde_json::Value::Null, "ts-x"),
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
            // Sub-1 defaults: type defaults to "claude-code"
            // (overridden via SpawnParams.session_type when the
            // wire carries it; the test helper uses the
            // SpawnParams default), state="ready" (the only
            // reachable value from this code path — see the
            // doc-comment in `methods::list_sessions`),
            // idle=false (daemon doesn't track idle yet).
            assert_eq!(s["state"], "ready");
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

    /// Slice 10d-mcp-surface-1 review fix #2: `state` is
    /// always `"ready"` for entries in the live registry. Pre-
    /// fix the value was `"running"` which doesn't match the
    /// Python tool's `ready|pending|exited` enum.
    #[test]
    fn list_sessions_emits_ready_state_for_live_session() {
        let state = state_with_session_in_workspace("ts-a", "ws-1");
        let req = operator_request("list_sessions", serde_json::Value::Null);
        let resp = dispatch_request(&state, &req).into_response();
        assert!(resp.ok);
        let arr = resp.result.unwrap();
        let entry = &arr.as_array().expect("top-level array")[0];
        assert_eq!(
            entry["state"], "ready",
            "live registry entry must surface as state='ready' (Python tool's enum)",
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
}
