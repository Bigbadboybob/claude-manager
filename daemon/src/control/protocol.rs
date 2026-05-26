//! Wire types for the cm-daemon control socket. Length-prefixed JSON over a
//! Unix domain socket (default `~/.cm/daemon.sock`; `~/.cm/tui.sock` accepted
//! as a secondary fallback for one release). 4-byte big-endian length, then
//! a UTF-8 JSON object.
//!
//! See doc/persistent-host-daemon.md and AGENT_ORCHESTRATION.md for the
//! envelope shape and the auth model.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request envelope. The MCP server (or any other client) sends this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Client-supplied request id; echoed in the response so async clients
    /// can match. The server doesn't interpret it.
    pub id: String,
    /// Caller identity. `Session` for agents (today's case); `Operator`
    /// for the TUI itself (new in Phase 1, used for spawn/attach/kill).
    /// Variants have disjoint field sets so untagged dispatch picks the
    /// right one without a discriminator — preserving the legacy wire
    /// shape `{"session_uid":"..."}` byte-for-byte.
    pub caller: Caller,
    /// Method name (e.g. "ping", "resolve_authorized_session").
    pub method: String,
    /// Method parameters. Shape is method-specific.
    #[serde(default)]
    pub params: Value,
}

/// Caller identity for a JSON-RPC request.
///
/// `serde(untagged)` over two newtype-wrapped structs, each with
/// `deny_unknown_fields`, so:
///   - Legacy payload `{"session_uid":"..."}` parses as `Session`.
///   - New payload `{"token_id":"..."}` parses as `Operator`.
///   - Mixed payload (`session_uid` AND `token_id`) errors — neither
///     variant accepts both.
///   - Empty payload `{}` errors — neither variant accepts no fields.
///
/// The `deny_unknown_fields` lives on the per-variant struct (not on
/// the enum) because serde silently ignores struct-level attrs on
/// inline enum variants; the newtype wrappers give us a place to put
/// the attribute that serde actually honors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Caller {
    Session(CallerSession),
    Operator(CallerOperator),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallerSession {
    pub session_uid: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallerOperator {
    pub token_id: String,
}

impl Caller {
    /// Convenience constructor for the legacy `Session` shape — useful
    /// in tests and in the MCP-client crate where most callers are agents.
    pub fn session(uid: impl Into<String>) -> Self {
        Caller::Session(CallerSession {
            session_uid: uid.into(),
        })
    }

    /// Convenience constructor for the new `Operator` shape — used by
    /// the TUI when it talks to the daemon.
    pub fn operator(token_id: impl Into<String>) -> Self {
        Caller::Operator(CallerOperator {
            token_id: token_id.into(),
        })
    }

    /// Returns the session uid for `Session` callers; `None` for `Operator`.
    ///
    /// Most existing methods (ping, resolve_authorized_session, etc.)
    /// require a session-scoped caller. Those dispatch sites use this to
    /// short-circuit with `Unauthorized` when an `Operator` reaches them.
    pub fn session_uid(&self) -> Option<&str> {
        match self {
            Caller::Session(s) => Some(&s.session_uid),
            Caller::Operator(_) => None,
        }
    }
}

/// Response envelope. Either `ok=true` with `result`, or `ok=false` with
/// `error`. Never both.
///
/// `deny_unknown_fields` lives here intentionally — adding a `kind` or
/// `stream_id` field to `Response` would break strict clients. Streaming
/// uses `StreamFrame` (below), which is distinguished from `Response`
/// by carrying a `kind` field that `Response` doesn't have and won't
/// accept.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Caller doesn't have authority over the target.
    Unauthorized,
    /// Target session/task doesn't exist (or not visible to caller).
    NotFound,
    /// Method params don't match the schema.
    InvalidParams,
    /// Method name unknown.
    UnknownMethod,
    /// The operation can't be performed in the current state — e.g.
    /// `send_input` to a session whose PTY child has exited but the
    /// row hasn't been tombstoned yet. Distinct from `not_found`
    /// because the target exists; distinct from `unauthorized`
    /// because the caller has authority. Retry-after-state-change
    /// is the natural client response.
    Conflict,
    /// Internal server error (panic, IO, etc.). Treat as transient.
    Internal,
}

impl Response {
    pub fn ok(id: String, result: Value) -> Self {
        Response {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: String, code: ErrorCode, message: impl Into<String>) -> Self {
        Response {
            id,
            ok: false,
            result: None,
            error: Some(ErrorBody {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Stream frame envelope. Carried on:
///   - The control connection after an `events.subscribe` /
///     `manifest.watch` / `workflow.get_state` stream is established
///     (Phase 2 for events.subscribe; Phase 1 for manifest.watch).
///   - The dedicated attach connection after `attach.open` consumes
///     the ticket — both directions thereafter carry `StreamFrame`s.
///
/// Distinguished from `Response` by the presence of `kind`, which
/// `Response` does not accept (it has `deny_unknown_fields`). The `id`
/// matches the request id that opened the stream so multiplexed control
/// streams can be demuxed by the receiver.
///
/// ## Direction-scoped kind set on an attach stream
///
/// The kinds split by direction; mixing a server-only kind onto the
/// client→server half (or vice versa) is a wire-protocol bug:
///
/// **Server → client** (outbound from the daemon):
///   - `Data`: PTY output bytes. Payload: `{"bytes": "<base64>"}`.
///   - `End`: stream termination on session exit. Payload:
///     `{"exit_code": <int|null>, "memory_cap_kill": <bool>}` so
///     the TUI can render the same toast it does today for a
///     `signal 9` exit.
///   - `Error`: abnormal termination. Payload: `{"message": "..."}`.
///
/// **Client → server** (inbound to the daemon):
///   - `Input`: keystroke bytes. Payload: `{"bytes": "<base64>"}`.
///   - `Resize`: terminal size update. Payload:
///     `{"cols": <u16>, "rows": <u16>}`. The daemon-side reader
///     dispatches to `DaemonSession::resize` which calls
///     `portable-pty::MasterPty::resize` to ioctl TIOCSWINSZ on
///     the kernel PTY.
///
/// The slice-10c-e-2 review flagged that the prior single-Data
/// scheme (`{"bytes": "..."}` for output *and* input,
/// `{"resize": {...}}` for resize) was ambiguous on the wire. The
/// per-direction kind set makes the schema unambiguous and lets
/// the daemon-side read thread reject a stray server-direction
/// frame (e.g. `Data` from the client) loudly instead of treating
/// it as input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamFrame {
    pub id: String,
    pub kind: StreamKind,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// Server → client: a PTY-output data frame. Payload
    /// `{"bytes": "<base64>"}`. Repeats until the stream ends.
    Data,
    /// Client → server: a keystroke / input data frame. Payload
    /// `{"bytes": "<base64>"}`. Slice 10c-e-2 review added the
    /// kind so the inbound half of `handle_attach_stream` can
    /// route input distinctly from output.
    Input,
    /// Client → server: terminal-size update. Payload
    /// `{"cols": <u16>, "rows": <u16>}`.
    Resize,
    /// Server → client: normal stream termination. Last frame the
    /// daemon sends on a session exit.
    End,
    /// Server → client: abnormal stream termination. The daemon
    /// sends this and closes; the client treats the stream as dead.
    Error,
    /// Server → client: first frame on a `manifest.watch` stream.
    /// Payload `{"workspaces": {...}, "bindings": {...}}` — the
    /// canonical snapshot of `state.workspaces` + `state.bindings`
    /// at subscribe time. Sent exactly once, before any
    /// [`StreamKind::ManifestDiff`] frames. Slice 10e-b.
    ManifestSnapshot,
    /// Server → client: a [`crate::manifest::ManifestDiff`] frame
    /// on a `manifest.watch` stream. Payload is the serialized
    /// diff (variant-tagged JSON, e.g.
    /// `{"Exited": {"uid": "...", "last_exit": {...}}}`).
    /// Repeats one frame per broadcast until the client
    /// disconnects. Slice 10e-b.
    ManifestDiff,
    /// Server → client: liveness probe on a long-lived stream
    /// (manifest.watch) during quiet periods. Payload is empty.
    /// The client treats it as a no-op; the daemon uses the
    /// write attempt's success/failure to detect idle-client
    /// disconnect — without it, a TUI that died during a quiet
    /// period would leak a handler thread + subscriber slot
    /// until the next real broadcast. Slice 10e-b r1 fix.
    Heartbeat,
    /// Server → client: initial snapshot frame on an
    /// `events.subscribe` stream. One frame per active
    /// [`crate::workflow::run::WorkflowRun`] at subscribe time,
    /// sent before any live [`StreamKind::WorkflowEvent`] frames.
    /// Payload is the serialized `WorkflowRun` (variant-tagged
    /// JSON). Slice 11b.
    ///
    /// Read from disk via `workflow::run::load_all()` rather than
    /// the in-memory `state.workflow_runs` cache — the cache
    /// update trails the broadcast point in
    /// `append_event_with_retry`, so a subscriber landing between
    /// broadcast and cache-update would otherwise miss the
    /// just-broadcast event.
    WorkflowEventStateSnapshot,
    /// Server → client: a [`crate::workflow::events::Event`]
    /// frame on an `events.subscribe` stream. Payload is the
    /// serialized event (the same JSON the daemon writes to
    /// `events.jsonl`). Repeats one frame per broadcast until
    /// the client disconnects. Slice 11b.
    WorkflowEvent,
}

impl StreamFrame {
    pub fn data(id: impl Into<String>, payload: Value) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::Data,
            payload,
        }
    }

    pub fn end(id: impl Into<String>, payload: Value) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::End,
            payload,
        }
    }

    pub fn error(id: impl Into<String>, message: impl Into<String>) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::Error,
            payload: serde_json::json!({ "message": message.into() }),
        }
    }

    /// 10e-b: build the initial `ManifestSnapshot` frame for a
    /// `manifest.watch` stream. Payload structure matches what the
    /// TUI consumer in 10e-c will deserialize.
    pub fn manifest_snapshot(id: impl Into<String>, payload: Value) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::ManifestSnapshot,
            payload,
        }
    }

    /// 10e-b: build a `ManifestDiff` frame for a `manifest.watch`
    /// stream. Payload is the variant-tagged JSON of
    /// [`crate::manifest::ManifestDiff`].
    pub fn manifest_diff(id: impl Into<String>, payload: Value) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::ManifestDiff,
            payload,
        }
    }

    /// 10e-b r1: build a heartbeat frame for a `manifest.watch`
    /// stream. Empty payload; the client ignores it. The
    /// daemon's write-attempt success/failure is the actual
    /// liveness probe.
    pub fn heartbeat(id: impl Into<String>) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::Heartbeat,
            payload: serde_json::Value::Null,
        }
    }

    /// 11b: build an initial `WorkflowEventStateSnapshot` frame
    /// for an `events.subscribe` stream. Payload is the
    /// serialized `WorkflowRun`. One frame per active run is
    /// sent before any live `WorkflowEvent` frames.
    pub fn workflow_event_state_snapshot(
        id: impl Into<String>,
        payload: Value,
    ) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::WorkflowEventStateSnapshot,
            payload,
        }
    }

    /// 11b: build a live `WorkflowEvent` frame. Payload is the
    /// serialized `crate::workflow::events::Event`.
    pub fn workflow_event(id: impl Into<String>, payload: Value) -> Self {
        StreamFrame {
            id: id.into(),
            kind: StreamKind::WorkflowEvent,
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Caller -----------------------------------------------------------

    #[test]
    fn legacy_session_caller_payload_parses_as_session() {
        // The exact byte shape mcp_server/control_client.py sends today —
        // must continue to deserialize as `Caller::Session`.
        let s = r#"{"session_uid":"uid-x"}"#;
        let c: Caller = serde_json::from_str(s).unwrap();
        match c {
            Caller::Session(s) => assert_eq!(s.session_uid, "uid-x"),
            Caller::Operator(_) => panic!("expected Session, got Operator"),
        }
    }

    #[test]
    fn operator_caller_payload_parses_as_operator() {
        let s = r#"{"token_id":"tk-y"}"#;
        let c: Caller = serde_json::from_str(s).unwrap();
        match c {
            Caller::Operator(o) => assert_eq!(o.token_id, "tk-y"),
            Caller::Session(_) => panic!("expected Operator, got Session"),
        }
    }

    #[test]
    fn mixed_caller_payload_errors() {
        // A payload with BOTH session_uid and token_id is ambiguous;
        // per-variant deny_unknown_fields ensures neither variant
        // accepts it, and untagged dispatch propagates the error.
        let s = r#"{"session_uid":"uid-x","token_id":"tk-y"}"#;
        let res: Result<Caller, _> = serde_json::from_str(s);
        assert!(res.is_err(), "mixed-field payload should not deserialize");
    }

    #[test]
    fn empty_caller_payload_errors() {
        // The pre-Phase-1 `Caller` struct would have accepted `{}`
        // (defaulting session_uid to ""). The Phase-1 tightening makes
        // this an error — caller identity is now mandatory.
        let s = r#"{}"#;
        let res: Result<Caller, _> = serde_json::from_str(s);
        assert!(res.is_err(), "empty caller payload should not deserialize");
    }

    #[test]
    fn unknown_caller_field_errors() {
        // Future variants would have to claim disjoint field names.
        // Unknown field on what would otherwise look like a Session
        // payload must error.
        let s = r#"{"session_uid":"uid-x","extra":"nope"}"#;
        let res: Result<Caller, _> = serde_json::from_str(s);
        assert!(res.is_err(), "unknown fields should not deserialize");
    }

    #[test]
    fn session_caller_serializes_to_flat_legacy_shape() {
        // Round-trip: serializing a `Caller::Session` must produce the
        // exact bytes mcp_server/control_client.py sends today — no
        // discriminator tag, no wrapper key.
        let c = Caller::session("uid-x");
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#"{"session_uid":"uid-x"}"#);
    }

    #[test]
    fn operator_caller_serializes_to_flat_shape() {
        let c = Caller::operator("tk-y");
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(s, r#"{"token_id":"tk-y"}"#);
    }

    #[test]
    fn caller_session_uid_accessor() {
        assert_eq!(Caller::session("uid-x").session_uid(), Some("uid-x"));
        assert_eq!(Caller::operator("tk-y").session_uid(), None);
    }

    // --- Request ----------------------------------------------------------

    #[test]
    fn request_roundtrips() {
        let r = Request {
            id: "1".into(),
            caller: Caller::session("uid-x"),
            method: "ping".into(),
            params: serde_json::json!({}),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "1");
        assert_eq!(back.caller.session_uid(), Some("uid-x"));
        assert_eq!(back.method, "ping");
    }

    #[test]
    fn request_default_params_when_missing() {
        let s = r#"{"id":"1","caller":{"session_uid":"x"},"method":"ping"}"#;
        let req: Request = serde_json::from_str(s).unwrap();
        assert_eq!(req.params, Value::Null);
        assert_eq!(req.caller.session_uid(), Some("x"));
    }

    #[test]
    fn legacy_request_bytes_parse_unchanged() {
        // Belt-and-braces: a full Request payload in the exact shape
        // mcp_server sends today must parse end-to-end.
        let s = r#"{"id":"42","caller":{"session_uid":"ts-abc-1"},"method":"list_sessions","params":{}}"#;
        let req: Request = serde_json::from_str(s).unwrap();
        assert_eq!(req.id, "42");
        assert_eq!(req.method, "list_sessions");
        assert_eq!(req.caller.session_uid(), Some("ts-abc-1"));
    }

    // --- Response ---------------------------------------------------------

    #[test]
    fn response_ok_serializes_without_error_field() {
        let r = Response::ok("1".into(), serde_json::json!({"pong": true}));
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ok\":true"));
        assert!(s.contains("\"pong\":true"));
        assert!(!s.contains("\"error\""));
    }

    #[test]
    fn response_err_omits_result() {
        let r = Response::err("1".into(), ErrorCode::Unauthorized, "no");
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"ok\":false"));
        assert!(s.contains("\"unauthorized\""));
        assert!(!s.contains("\"result\""));
    }

    #[test]
    fn response_rejects_unknown_field() {
        // Belt-and-braces: confirm `deny_unknown_fields` on `Response`
        // still rejects a `kind` field — that's the disambiguator from
        // `StreamFrame` and must not silently coexist on `Response`.
        let s = r#"{"id":"1","ok":true,"kind":"data"}"#;
        let res: Result<Response, _> = serde_json::from_str(s);
        assert!(res.is_err(), "Response must reject unknown fields");
    }

    // --- StreamFrame ------------------------------------------------------

    #[test]
    fn stream_frame_data_roundtrips() {
        let f = StreamFrame::data("attach-1", serde_json::json!({"bytes": "aGVsbG8="}));
        let s = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "attach-1");
        assert_eq!(back.kind, StreamKind::Data);
        assert_eq!(back.payload["bytes"], "aGVsbG8=");
    }

    #[test]
    fn stream_frame_end_roundtrips() {
        let f = StreamFrame::end(
            "attach-1",
            serde_json::json!({"exit_code": 137, "memory_cap_kill": true}),
        );
        let s = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, StreamKind::End);
        assert_eq!(back.payload["exit_code"], 137);
        assert_eq!(back.payload["memory_cap_kill"], true);
    }

    #[test]
    fn stream_frame_error_carries_message() {
        let f = StreamFrame::error("attach-1", "broken pipe");
        let s = serde_json::to_string(&f).unwrap();
        let back: StreamFrame = serde_json::from_str(&s).unwrap();
        assert_eq!(back.kind, StreamKind::Error);
        assert_eq!(back.payload["message"], "broken pipe");
    }

    #[test]
    fn stream_frame_distinguishable_from_response_on_the_wire() {
        // A StreamFrame's `kind` field is what tells the receiver
        // "this is a stream, not an RPC response." If a receiver tries
        // to deserialize a StreamFrame as a Response, it must error
        // (because of deny_unknown_fields on Response).
        let frame_bytes = serde_json::to_string(
            &StreamFrame::data("x", serde_json::json!({"bytes": "abc"})),
        )
        .unwrap();
        let as_response: Result<Response, _> = serde_json::from_str(&frame_bytes);
        assert!(
            as_response.is_err(),
            "StreamFrame must not silently parse as Response — that's the disambiguator"
        );
    }
}
