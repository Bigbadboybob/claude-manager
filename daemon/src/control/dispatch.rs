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

/// `start_session` — spawn a daemon-owned session. Operator-callable
/// only at slice 10c-b; Session callers get `Unauthorized` with a
/// pointer to 10c-d (when descendant-task-tree validation against
/// a live task list lands). See `crate::control::methods` module
/// doc for the full disposition.
fn dispatch_start_session(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    if matches!(req.caller, Caller::Session(_)) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "start_session is Operator-callable only at slice 10c-b; Session-caller path (with descendant-task-tree validation) wires in 10c-d alongside send_input / kill_session relocations",
        );
    }
    match methods::start_session(state, &req.params) {
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

/// `send_input` — write bytes to a session's PTY. Operator-only
/// at slice 10c-d; Session callers get Unauthorized with a
/// pointer to 10c-e (when descendant-task-tree validation lands).
/// Body lives in `crate::control::methods::send_input`.
fn dispatch_send_input(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    if matches!(req.caller, Caller::Session(_)) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "send_input is Operator-callable only at slice 10c-d; Session-caller path (with descendant-task-tree validation) wires in 10c-e alongside agent-module relocation",
        );
    }
    match methods::send_input(state, &req.params) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

/// `kill_session` — terminate a session. Operator-only at slice
/// 10c-d. The TUI's tombstone-and-manifest-write is deferred to
/// slice 10e (manifest-ownership flip); this slice just removes
/// from the in-memory registry, which is sufficient for
/// `session.attach` to subsequently return NotFound.
fn dispatch_kill_session(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    if matches!(req.caller, Caller::Session(_)) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "kill_session is Operator-callable only at slice 10c-d; Session-caller path wires in 10c-e",
        );
    }
    match methods::kill_session(state, &req.params) {
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
fn dispatch_read_session_output(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Response {
    if matches!(req.caller, Caller::Session(_)) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "read_session_output is Operator-callable only at slice 10c-d; Session-caller path wires in 10c-e",
        );
    }
    match methods::read_session_output(state, &req.params) {
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
        let resp = dispatch_request(
            &state,
            &session_request("list_sessions", serde_json::Value::Null, "ts-x"),
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
        let state = make_state();
        let mut params =
            crate::session::SpawnParams::new(uid, "test", "/bin/sleep");
        params.args = vec!["30".into()];
        let session =
            crate::session::DaemonSession::spawn(params).expect("spawn /bin/sleep");
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert(uid.into(), session);
        }
        state
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
    fn start_session_session_caller_is_unauthorized_with_slice_10c_d_pointer() {
        // The named disposition: Session callers can't reach
        // start_session until descendant-task-tree validation lands
        // in 10c-d. Until then, they get Unauthorized with a
        // pointer to the slice that lights them up.
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
        assert!(!resp.ok, "Session caller must get Unauthorized");
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(
            err.message.contains("10c-d"),
            "error should point at the slice that lights this up: {}",
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

    #[test]
    fn send_input_session_caller_is_unauthorized() {
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &session_request(
                "send_input",
                serde_json::json!({ "session_uid": "ts-live", "text": "hi" }),
                "ts-agent",
            ),
        ).into_response();
        assert!(!resp.ok);
        let err = resp.error.expect("error body");
        assert_eq!(err.code, ErrorCode::Unauthorized);
        assert!(err.message.contains("10c-e"));
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

    #[test]
    fn kill_session_session_caller_is_unauthorized() {
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &session_request(
                "kill_session",
                serde_json::json!({ "session_uid": "ts-live" }),
                "ts-agent",
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

    #[test]
    fn read_session_output_session_caller_is_unauthorized() {
        let state = state_with_session("ts-live");
        let resp = dispatch_request(
            &state,
            &session_request(
                "read_session_output",
                serde_json::json!({ "session_uid": "ts-live" }),
                "ts-agent",
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
}
