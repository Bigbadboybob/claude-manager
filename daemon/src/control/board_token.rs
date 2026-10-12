//! Local board capability: a distinct token, restricted before normal dispatch.
use super::{
    operator,
    protocol::{Caller, ErrorCode, Request, Response},
};
use crate::state::DaemonState;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    sync::{Arc, Mutex},
};

const PREFIX: &str = "cm-board-v1:";

fn allowed(method: &str, p: &Value) -> bool {
    match method {
        "messaging.channels" => matches!(p["action"].as_str(), Some("list" | "join")),
        "messaging.availability" => matches!(p["action"].as_str(), Some("get" | "set")),
        "messaging.dms"
        | "messaging.people"
        | "messaging.counts"
        | "messaging.read"
        | "messaging.send"
        | "messaging.mark_read"
        | "messaging.react"
        | "owner_attention.list"
        | "owner_attention.ack"
        | "owner.notifications.next" => true,
        _ => false,
    }
}

fn token(root: &Path) -> std::io::Result<String> {
    let mut value = String::new();
    fs::File::open(root.join("board-token"))?
        .take(1025)
        .read_to_string(&mut value)?;
    let value = value.trim();
    if value.len() != PREFIX.len() + 64
        || !value.starts_with(PREFIX)
        || !value[PREFIX.len()..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(std::io::Error::other(
            "Invalid board token; use explicit rotation",
        ));
    }
    Ok(value.into())
}

pub(super) fn authorize(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
) -> Result<Option<Request>, Response> {
    let Caller::Operator(caller) = &req.caller else {
        return Ok(None);
    };
    if !caller.token_id.starts_with(PREFIX) {
        return Ok(None);
    }
    let deny = || {
        Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "Board credential is invalid or does not permit this operation",
        )
    };
    if !allowed(&req.method, &req.params) {
        return Err(deny());
    }
    let root = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging_root
        .clone();
    let expected = token(&root).map_err(|_| deny())?;
    if expected.len() != caller.token_id.len()
        || expected
            .bytes()
            .zip(caller.token_id.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            != 0
    {
        return Err(deny());
    }
    let operator = operator::configured_caller().ok_or_else(deny)?;
    Ok(Some(Request {
        id: req.id.clone(),
        method: req.method.clone(),
        params: req.params.clone(),
        caller: operator,
    }))
}

pub(super) fn manage(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    if let Err((code, message)) = operator::require_strong_operator(&req.caller) {
        return Response::err(req.id.clone(), code, message);
    }
    let Some(p) = req.params.as_object().filter(|p| p.len() == 1) else {
        return Response::err(
            req.id.clone(),
            ErrorCode::InvalidParams,
            "Choose action issue, rotate or revoke",
        );
    };
    let action = p.get("action").and_then(Value::as_str).unwrap_or("");
    if !matches!(action, "issue" | "rotate" | "revoke") {
        return Response::err(
            req.id.clone(),
            ErrorCode::InvalidParams,
            "Choose action issue, rotate or revoke",
        );
    }
    // Serialize issue/rotation with one another. No board method can call this.
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    let root = &s.messaging_root;
    let path = root.join("board-token");
    let result = (|| -> std::io::Result<Value> {
        if action == "revoke" {
            match fs::remove_file(&path) {
                Ok(()) => fs::File::open(root)?.sync_all()?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            return Ok(json!({"revoked":true}));
        }
        if action == "issue" && path.exists() {
            token(root)?;
            return Ok(json!({"path":path,"issued":false}));
        }
        fs::create_dir_all(root)?;
        let secret = format!(
            "{PREFIX}{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let tmp = root.join(format!(".board-token-{}", uuid::Uuid::new_v4()));
        let result = (|| -> std::io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(secret.as_bytes())?;
            f.sync_all()?;
            fs::rename(&tmp, &path)?;
            fs::File::open(root)?.sync_all()
        })();
        let _ = fs::remove_file(&tmp);
        result?;
        Ok(json!({"path":path,"issued":true}))
    })();
    match result {
        Ok(v) => Response::ok(req.id.clone(), v),
        Err(e) => Response::err(req.id.clone(), ErrorCode::Internal, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::dispatch::dispatch_request;
    use super::*;
    fn req(token: &str, method: &str, params: Value) -> Request {
        Request {
            id: "test".into(),
            caller: Caller::operator(token),
            method: method.into(),
            params,
        }
    }
    #[test]
    fn owner_board_capability_scopes_actions_rotates_revokes_and_never_falls_open() {
        let _guard = crate::test_support::env_lock();
        let _op = operator::test_override::set(Some("real-operator"));
        let temp = tempfile::tempdir().unwrap();
        let mut state = DaemonState::default();
        state.messaging_root = temp.path().into();
        let state = Arc::new(Mutex::new(state));
        let issue = req(
            "real-operator",
            "owner.board_token",
            json!({"action":"issue"}),
        );
        assert!(manage(&state, &issue).ok);
        let first = token(temp.path()).unwrap();
        assert!(!serde_json::to_string(&manage(&state, &issue))
            .unwrap()
            .contains(&first));
        for (method, params) in [
            ("messaging.counts", json!({})),
            ("messaging.channels", json!({"action":"join"})),
            ("owner_attention.list", json!({})),
            ("owner.notifications.next", json!({"wait_ms":0})),
        ] {
            assert!(authorize(&state, &req(&first, method, params))
                .unwrap()
                .is_some());
        }
        for (method, params) in [
            ("daemon.restart", json!({})),
            ("session.spawn", json!({})),
            ("owner.board_token", json!({"action":"rotate"})),
            ("messaging.sync", json!({})),
            ("messaging.channels", json!({"action":"create"})),
            ("messaging.follow", json!({})),
        ] {
            assert_eq!(
                dispatch_request(&state, &req(&first, method, params))
                    .into_response()
                    .error
                    .unwrap()
                    .code,
                ErrorCode::Unauthorized
            );
        }
        assert!(
            manage(
                &state,
                &req(
                    "real-operator",
                    "owner.board_token",
                    json!({"action":"rotate"})
                )
            )
            .ok
        );
        assert!(authorize(&state, &req(&first, "messaging.counts", json!({}))).is_err());
        let second = token(temp.path()).unwrap();
        assert_ne!(first, second);
        assert!(
            manage(
                &state,
                &req(
                    "real-operator",
                    "owner.board_token",
                    json!({"action":"revoke"})
                )
            )
            .ok
        );
        assert!(authorize(&state, &req(&second, "messaging.counts", json!({}))).is_err());
        drop(_op);
        let _disabled = operator::test_override::set(None);
        assert!(authorize(&state, &req(&first, "messaging.counts", json!({}))).is_err());
        assert!(!manage(&state, &issue).ok);
    }
}
