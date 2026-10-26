//! Spawn-time setup by the creator (#28).
//!
//! Whoever launches a session can set it up in the same breath, without
//! waiting for the new session to act: `start_session(name=, channels=,
//! board=, group=, epic=, items=, poc_of=, hire=)` runs this right after the
//! spawn, and `org(action="setup", session=...)` runs it for a session the
//! caller spawned earlier. Both are the daemon RPC `session.setup`.
//!
//! Every step runs with the CREATOR's authority, through the same RPC the
//! creator would call itself, so a step it could not do on its own is
//! refused for that step only and the others still run:
//!
//! | step | how | refused when |
//! |---|---|---|
//! | `hire` | `org.promote` as the creator (#27) | the creator may not promote it, or the daemon has no promote yet |
//! | `name` | `session.set_name` as the new session, before its first message | the name is taken / the hub predates first claims |
//! | `channels` | join as the new session (an open channel); else `add_member` as the creator | the creator is no admin of a closed channel |
//! | `board` (+ `group` / `epic`) | `board.join` as the creator | the creator may not edit the board |
//! | `items` | `item.set(holder=<new session>)` as the creator | the board's item rules refuse the creator |
//! | `poc_of` | `epic.set_poc(session=<new session>)` as the creator | the creator has no authority over the board, or the session is no member yet (`hire` it) |
//!
//! The result is per step: `"ok"`, `"refused: <why>"` (an authority refusal)
//! or `"failed: <why>"` (anything else), with `channels` and `items` per
//! entry.

use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::state::DaemonState;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

pub const METHOD: &str = "session.setup";

/// The setup keys `start_session` passes through (`session_uid` names the
/// target).
pub const KEYS: &[&str] = &["name", "channels", "board", "group", "epic", "items", "poc_of", "hire"];

type RpcResult<T = Value> = Result<T, (ErrorCode, String)>;

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    match run(state, req) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

fn bad(message: impl Into<String>) -> (ErrorCode, String) {
    (ErrorCode::InvalidParams, message.into())
}

fn str_of<'a>(p: &'a Value, key: &str) -> Option<&'a str> {
    p.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// Who may set up `child`: Owner (the operator socket), the session that
/// spawned it, or a session with global permissions.
fn authorize(state: &DaemonState, caller: &Caller, child: &str) -> RpcResult<()> {
    let Some(sess) = state.sessions.get(child) else {
        return Err((ErrorCode::NotFound, format!("not_found: no live session '{child}' on this daemon")));
    };
    if crate::assistants::is_shell(&sess.session_type) {
        return Err(bad("agent_only: a shell has no chat name, board or items to set up"));
    }
    match caller {
        Caller::Operator(_) => Ok(()),
        Caller::Session(c) if sess.managed_by_uid.as_deref() == Some(c.session_uid.as_str()) => Ok(()),
        Caller::Session(c) if state.sessions.get(&c.session_uid).is_some_and(|s| s.global_perms) => Ok(()),
        Caller::Session(_) => Err((
            ErrorCode::Unauthorized,
            format!("not_spawner: '{child}' was not spawned by you; set up only the sessions you start"),
        )),
    }
}

fn run(state: &Arc<Mutex<DaemonState>>, req: &Request) -> RpcResult {
    if let Caller::Operator(_) = &req.caller {
        crate::control::operator::validate_operator(&req.caller)
            .map_err(|e| (ErrorCode::Unauthorized, e.to_string()))?;
    }
    let p = &req.params;
    let child = str_of(p, "session_uid").or_else(|| str_of(p, "session"))
        .ok_or_else(|| bad("session_uid (the session to set up) is required"))?
        .to_string();
    let daemon_id = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        authorize(&s, &req.caller, &child)?;
        let handle = s.messaging.clone();
        drop(s);
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref().map(|m| m.daemon_id.clone())
            .ok_or_else(|| (ErrorCode::Internal, "daemon_id_unavailable: messaging store is not open".to_string()))?
    };
    let pid = format!("agent:{daemon_id}:{child}");
    let creator = req.caller.clone();
    let mut setup = Map::new();

    if let Some(hire) = p.get("hire").filter(|v| !v.is_null()) {
        setup.insert("hire".into(), step(hire_step(state, &creator, &child, hire)));
    }
    if let Some(name) = str_of(p, "name") {
        setup.insert("name".into(), step(name_step(state, &child, name)));
    }
    if let Some(channels) = p.get("channels").filter(|v| !v.is_null()) {
        let list: Vec<String> = match channels {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
            _ => return Err(bad("channels must be a channel path or a list of them")),
        };
        let results: Vec<Value> = list.iter().map(|c| {
            json!({"channel": c, "result": step(channel_step(state, &creator, &child, &pid, c))})
        }).collect();
        setup.insert("channels".into(), json!(results));
    }
    let board = str_of(p, "board");
    if let Some(board) = board {
        let mut params = json!({"board": board, "session": pid, "reason": "added by its creator at spawn"});
        if let Some(g) = str_of(p, "group") {
            params["group"] = json!(g);
        }
        if let Some(e) = str_of(p, "epic") {
            params["group_id"] = json!(e);
        }
        setup.insert("board".into(), step(call(state, &creator, "board.join", params).map(|v| {
            if v["joined"] == false { json!("ok (already on the board)") } else { json!("ok") }
        })));
    }
    if let Some(items) = p.get("items").filter(|v| !v.is_null()) {
        let ns: Vec<i64> = match items {
            Value::Number(n) => n.as_i64().into_iter().collect(),
            Value::Array(a) => a.iter().filter_map(Value::as_i64).collect(),
            _ => return Err(bad("items must be an item number or a list of them")),
        };
        let results: Vec<Value> = ns.iter().map(|n| {
            let mut params = json!({"n": n, "holder": pid, "reason": "handed to the session its holder spawned"});
            if let Some(b) = board {
                params["board"] = json!(b);
            }
            json!({"n": n, "result": step(call(state, &creator, "item.set", params))})
        }).collect();
        setup.insert("items".into(), json!(results));
    }
    if let Some(group_id) = str_of(p, "poc_of") {
        let mut params = json!({"group_id": group_id, "session": pid});
        if let Some(b) = board {
            params["board"] = json!(b);
        }
        setup.insert("poc".into(), step(call(state, &creator, "epic.set_poc", params).map(|v| {
            json!({"ok": true, "team_id": v["team_id"], "joined_board": v["joined_board"]})
        })));
    }
    Ok(json!({"session_uid": child, "pid": pid, "setup": setup}))
}

/// `"ok"` (or the step's own success value), `"refused: <why>"` for an
/// authority refusal, `"failed: <why>"` for anything else.
fn step(result: RpcResult) -> Value {
    match result {
        Ok(Value::String(s)) => json!(s),
        Ok(v) if v.is_object() && v.get("ok").is_some() => v,
        Ok(_) => json!("ok"),
        Err((ErrorCode::Unauthorized, message)) => json!(format!("refused: {message}")),
        Err((_, message)) if message.starts_with("forbidden") || message.contains("not_manager")
            || message.contains("owner_only") || message.contains("not_member") =>
        {
            json!(format!("refused: {message}"))
        }
        Err((_, message)) => json!(format!("failed: {message}")),
    }
}

/// One RPC as `caller`, through the module that serves it.
fn call(state: &Arc<Mutex<DaemonState>>, caller: &Caller, method: &str, params: Value) -> RpcResult {
    let req = Request { id: format!("setup-{method}"), caller: caller.clone(), method: method.into(), params };
    let resp = match method {
        m if m.starts_with("org.") => crate::items::org::dispatch(state, &req),
        "session.set_name" | "messaging.channels" => crate::messaging::rpc::dispatch(state, &req),
        _ => crate::items::rpc::dispatch(state, &req),
    };
    match (resp.result, resp.error) {
        (_, Some(e)) => Err((e.code, e.message)),
        (Some(v), None) => Ok(v),
        (None, None) => Ok(Value::Null),
    }
}

fn hire_step(state: &Arc<Mutex<DaemonState>>, creator: &Caller, child: &str, hire: &Value) -> RpcResult {
    let handle = str_of(hire, "handle").ok_or_else(|| bad("hire needs a handle"))?;
    let mut params = json!({"session": child, "handle": handle, "reason": "hired by its creator at spawn"});
    for key in ["title", "team", "manager"] {
        if let Some(v) = str_of(hire, key) {
            params[key] = json!(v);
        }
    }
    match call(state, creator, "org.promote", params) {
        Err((ErrorCode::UnknownMethod, _)) => Err((ErrorCode::UnknownMethod,
            "this daemon cannot promote yet (org #27): spawn, then org(action=\"promote\") once it ships".into())),
        other => other.map(|v| json!({"ok": true, "member": v.get("member").cloned().unwrap_or(v)})),
    }
}

/// Claim the new session's chat name as the session itself (a first claim:
/// no message is sent). An already-named session is renamed at its current
/// revision; the same name is a no-op.
fn name_step(state: &Arc<Mutex<DaemonState>>, child: &str, name: &str) -> RpcResult {
    let current = crate::messaging::rpc::current_name(state, child);
    let mut params = json!({"name": name, "request_id": format!("setup-name-{child}-{name}")});
    if let Some((now, revision)) = current {
        if now.eq_ignore_ascii_case(name) {
            return Ok(json!("ok (already named)"));
        }
        params["expected_name_revision"] = json!(revision);
    }
    call(state, &Caller::session(child), "session.set_name", params).map(|v| {
        match v["event"]["data"]["identity"]["name"].as_str() {
            Some(got) if !got.eq_ignore_ascii_case(name) => json!(format!("ok (named {got}: {name} was taken)")),
            _ => json!("ok"),
        }
    })
}

/// Join a channel as the new session when it is open to any agent (so the
/// creator could join it too), else add it as the creator (an admin of the
/// channel).
fn channel_step(state: &Arc<Mutex<DaemonState>>, creator: &Caller, child: &str, pid: &str, path: &str) -> RpcResult {
    let path = path.trim_start_matches('#');
    let join = json!({"action": "join", "path": path, "request_id": format!("setup-join-{child}-{path}")});
    match call(state, &Caller::session(child), "messaging.channels", join) {
        Ok(_) => Ok(json!("ok")),
        Err(first) => {
            let add = json!({"action": "add_member", "path": path, "participant_id": pid,
                             "request_id": format!("setup-add-{child}-{path}")});
            call(state, creator, "messaging.channels", add).map(|_| json!("ok (added by you)"))
                .map_err(|(code, message)| (code, format!("{message} (joining as the session: {})", first.1)))
        }
    }
}

#[cfg(test)]
mod tests;
