//! Wire types for the TUI control socket. Length-prefixed JSON over a
//! Unix domain socket (`~/.cm/tui.sock`). 4-byte big-endian length, then
//! a UTF-8 JSON object.
//!
//! See AGENT_ORCHESTRATION.md for the full envelope shape and auth model.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request envelope. The MCP server (or any other client) sends this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Client-supplied request id; echoed in the response so async clients
    /// can match. The server doesn't interpret it.
    pub id: String,
    /// Caller identity. Only `session_uid` is sent — the server looks up
    /// task/workspace from its own manifest. Trusting an env-supplied
    /// task id would be a forgeable hole (see "Authorization model" in
    /// the design doc).
    pub caller: Caller,
    /// Method name (e.g. "ping", "resolve_authorized_session").
    pub method: String,
    /// Method parameters. Shape is method-specific.
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Caller {
    pub session_uid: String,
}

/// Response envelope. Either `ok=true` with `result`, or `ok=false` with
/// `error`. Never both.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrips() {
        let r = Request {
            id: "1".into(),
            caller: Caller {
                session_uid: "uid-x".into(),
            },
            method: "ping".into(),
            params: serde_json::json!({}),
        };
        let s = serde_json::to_string(&r).unwrap();
        let back: Request = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id, "1");
        assert_eq!(back.caller.session_uid, "uid-x");
        assert_eq!(back.method, "ping");
    }

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
    fn request_default_params_when_missing() {
        let s = r#"{"id":"1","caller":{"session_uid":"x"},"method":"ping"}"#;
        let req: Request = serde_json::from_str(s).unwrap();
        assert_eq!(req.params, Value::Null);
    }
}
