//! Read-only cross-host roster proxy. Caller facts come only from the daemon
//! registry and are fenced around both the roster read and central recheck.

use std::sync::{Arc, Mutex};
use serde_json::{json, Value};
use crate::control::operator;
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::state::DaemonState;

// A taskless caller's workspace authority comes from this daemon's current
// registry, never a stale remote workspace stamp. Sorted for stable cursors.
fn workspace_uids(s: &DaemonState, identity: &super::org::ReadCallerIdentity) -> Vec<String> {
    if identity.task_id.is_some() || identity.global_perms { return Vec::new(); }
    let mut ids: Vec<String> = s.sessions.iter()
        .filter(|(_, session)| session.workspace_id == identity.workspace_id)
        .map(|(uid, _)| uid.clone()).collect();
    ids.extend(s.tui_sessions.iter()
        .filter(|(uid, session)| !s.sessions.contains_key(*uid)
            && session.workspace_id.as_deref() == Some(identity.workspace_id.as_str()))
        .map(|(uid, _)| uid.clone()));
    ids.sort();
    ids.dedup();
    ids
}

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    match read(state, req) {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

fn read(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Result<Value, (ErrorCode, String)> {
    let filters = if req.params.is_null() { json!({}) } else { req.params.clone() };
    let obj = filters.as_object().ok_or_else(|| (ErrorCode::InvalidParams, "roster filters must be an object".into()))?;
    if obj.keys().any(|k| !matches!(k.as_str(), "host" | "member" | "manager" | "state" | "task" | "include_exited" | "limit" | "cursor")) {
        return Err((ErrorCode::InvalidParams, "unknown roster filter".into()));
    }
    let uid = match &req.caller {
        Caller::Session(c) => Some(c.session_uid.as_str()),
        Caller::Operator(_) => { operator::require_strong_operator(&req.caller)?; None }
    };
    for attempt in 0..2 {
        let (identity, workspace, handle, url, token) = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            let identity = match uid {
                Some(uid) => {
                    let session = s.sessions.get(uid).filter(|s| !s.last_exit.kernel_set())
                        .ok_or_else(|| (ErrorCode::Unauthorized, "roster caller is not live".into()))?;
                    let _ = session;
                    Some(super::org::caller_identity(&s, uid).ok_or_else(||
                        (ErrorCode::Unauthorized, "roster caller is not registered".into()))?)
                }
                None => None,
            };
            let workspace = identity.as_ref().map(|id| workspace_uids(&s, id)).unwrap_or_default();
            if workspace.len() > 2000 {
                return Err((ErrorCode::Conflict, "roster workspace scope exceeds bound".into()));
            }
            (identity, workspace, s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone())
        };
        let daemon_id = handle.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
            .map(|h| h.daemon_id.clone())
            .ok_or_else(|| (ErrorCode::Internal, "daemon_id_unavailable".into()))?;
        let client = super::api::Api::from_config(&url, &token)?;
        let pid = uid.map(|u| format!("agent:{daemon_id}:{u}")).unwrap_or_else(|| "owner".into());
        let caller = if let Some(id) = identity.as_ref() {
            json!({"pid": pid, "daemon_id": daemon_id, "session_uid": uid,
                   "task_id": id.task_id, "workspace_id": id.workspace_id,
                   "workspace_uids": workspace,
                   "generation": id.generation, "process_identity": id.process_identity,
                   "global": id.global_perms, "owner": false})
        } else {
            json!({"pid": "owner", "daemon_id": daemon_id, "session_uid": null,
                   "task_id": null, "workspace_id": null, "generation": null,
                   "workspace_uids": [],
                   "process_identity": null, "global": false, "owner": true})
        };
        let response = client.post("/session-roster", &json!({"caller": caller, "filters": filters}))?;
        let basis = response.get("scope_basis").and_then(Value::as_object)
            .ok_or_else(|| (ErrorCode::Internal, "invalid roster scope basis".into()))?;
        let mut valid = true;
        if let Some(id) = identity.as_ref() {
            // One central caller lookup, never one request per row. API
            // unavailability fails closed rather than widening the result.
            let member = client.get("/org/members/by-pid", &[("pid", pid.clone())])?;
            let central = &member["member"];
            valid = basis.get("member_id") == Some(&central["id"])
                && basis.get("revision") == Some(&central["revision"])
                && basis.get("role") == member.get("acting_as")
                && basis.get("member_id").and_then(Value::as_str) == id.member_id.as_deref()
                && basis.get("role").and_then(Value::as_str) == id.acting_as.as_deref()
                && basis.get("global").and_then(Value::as_bool) == Some(id.global_perms)
                && basis.get("owner").and_then(Value::as_bool) == Some(false)
                && basis.get("task_roots") == Some(&match &id.task_id {
                    Some(task) => json!([task]), None => json!([]),
                });
            if id.acting_as.as_deref() == Some("main") {
                valid &= basis.get("occupancy_generation") == Some(&central["main"]["occupancy_generation"]);
            }
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            valid &= s.sessions.get(uid.unwrap()).is_some_and(|s| !s.last_exit.kernel_set())
                && super::org::caller_identity(&s, uid.unwrap()).as_ref() == Some(id)
                && workspace_uids(&s, id) == workspace
                && std::sync::Arc::ptr_eq(&s.messaging, &handle)
                && s.config.api_url == url && s.config.api_token == token;
        } else {
            operator::require_strong_operator(&req.caller)?;
            valid &= basis.get("owner").and_then(Value::as_bool) == Some(true);
        }
        let current_daemon = handle.lock().unwrap_or_else(|p| p.into_inner()).as_ref()
            .map(|h| h.daemon_id.clone());
        valid &= current_daemon.as_deref() == Some(daemon_id.as_str());
        if valid { return Ok(response); }
        if attempt == 1 { break; }
    }
    Err((ErrorCode::Conflict, "scope_changed: roster caller or scope changed during the read".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn unbound_caller_is_stamped_and_scope_mismatch_retries_once() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let lookups = Arc::new(AtomicUsize::new(0));
        let observed = lookups.clone();
        let mode = Arc::new(AtomicUsize::new(0));
        let stub_mode = mode.clone();
        let (url, seen) = super::super::org::test_stub(move |method, path, body| {
            if method == "POST" && path == "/session-roster" {
                let c = &body["caller"];
                return (200, json!({"scope_basis": {"member_id":null,"role":null,"revision":null,
                    "global":c["global"],"owner":false,"task_roots":[]}, "rows": []}).to_string());
            }
            if method == "GET" && path.starts_with("/org/members/by-pid?") {
                let n = observed.fetch_add(1, Ordering::SeqCst);
                if stub_mode.load(Ordering::SeqCst) == 2 {
                    return (500, "{}".into());
                }
                if n == 0 || stub_mode.load(Ordering::SeqCst) == 1 {
                    return (200, json!({"member":{"id":"mem_changed","revision":2},"acting_as":"assistant"}).to_string());
                }
                return (200, json!({"member":null,"acting_as":null}).to_string());
            }
            (500, "{}".into())
        });
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "fixture".into();
        let mut params = crate::session::SpawnParams::new("caller-uid", "caller", "/bin/sleep");
        params.args = vec!["30".into()];
        params.session_type = "codex".into();
        params.workspace_id = "fixture-ws".into();
        st.sessions.insert("caller-uid".into(), crate::session::DaemonSession::spawn(params).unwrap());
        let state = Arc::new(Mutex::new(st));
        crate::messaging::startup::open_for_test(&state).unwrap();
        let req = Request { id:"fixture".into(), caller: Caller::session("caller-uid"),
                            method:"session.roster".into(), params:json!({}) };
        let reply = dispatch(&state, &req);
        assert!(reply.ok, "{:?}", reply.error);
        assert_eq!(lookups.load(Ordering::SeqCst), 2);
        let log = seen.lock().unwrap();
        let posts: Vec<_> = log.iter().filter(|(method, path, _)| method == "POST" && path == "/session-roster").collect();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0].2["caller"]["session_uid"], "caller-uid");
        assert_eq!(posts[0].2["caller"]["workspace_uids"], json!(["caller-uid"]));
        assert_eq!(posts[0].2["caller"]["global"], false);
        assert_eq!(posts[0].2["caller"]["owner"], false);
        assert!(posts[0].2["filters"].get("caller").is_none());
        drop(posts);
        drop(log);
        mode.store(1, Ordering::SeqCst);
        let before = lookups.load(Ordering::SeqCst);
        let error = dispatch(&state, &req).error.unwrap();
        assert!(matches!(error.code, ErrorCode::Conflict));
        assert!(error.message.starts_with("scope_changed:"));
        assert_eq!(lookups.load(Ordering::SeqCst) - before, 2);
        mode.store(2, Ordering::SeqCst);
        let error = dispatch(&state, &req).error.unwrap();
        assert!(matches!(error.code, ErrorCode::Internal));
    }

    #[test]
    fn client_cannot_supply_a_caller_stamp() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let req = Request { id:"fixture".into(), caller: Caller::session("u"),
                            method:"session.roster".into(),
                            params:json!({"caller":{"owner":true}}) };
        assert!(matches!(dispatch(&state, &req).error.unwrap().code, ErrorCode::InvalidParams));
    }

    #[test]
    fn changed_local_global_grant_never_returns_the_old_snapshot() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let state_slot: Arc<Mutex<Option<std::sync::Weak<Mutex<DaemonState>>>>> = Arc::default();
        let slot = state_slot.clone();
        let (url, seen) = super::super::org::test_stub(move |method, path, body| {
            if method == "POST" && path == "/session-roster" {
                if let Some(state) = slot.lock().unwrap().as_ref().and_then(|s| s.upgrade()) {
                    let mut s = state.lock().unwrap();
                    let session = s.sessions.get_mut("changing").unwrap();
                    session.global_perms = !session.global_perms;
                }
                return (200, json!({"scope_basis": {"member_id":null,"role":null,"revision":null,
                    "global":body["caller"]["global"],"owner":false,"task_roots":[]},
                    "rows":[{"sensitive":"old scope"}]}).to_string());
            }
            if method == "GET" && path.starts_with("/org/members/by-pid?") {
                return (200, json!({"member":null,"acting_as":null}).to_string());
            }
            (500, "{}".into())
        });
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "fixture".into();
        let mut params = crate::session::SpawnParams::new("changing", "changing", "/bin/sleep");
        params.args = vec!["30".into()];
        params.workspace_id = "fixture-ws".into();
        st.sessions.insert("changing".into(), crate::session::DaemonSession::spawn(params).unwrap());
        let state = Arc::new(Mutex::new(st));
        *state_slot.lock().unwrap() = Some(Arc::downgrade(&state));
        crate::messaging::startup::open_for_test(&state).unwrap();
        let req = Request { id:"fixture".into(), caller: Caller::session("changing"),
                            method:"session.roster".into(), params:json!({}) };
        let reply = dispatch(&state, &req);
        assert!(!reply.ok);
        assert!(reply.result.is_none());
        assert!(reply.error.unwrap().message.starts_with("scope_changed:"));
        assert_eq!(seen.lock().unwrap().iter().filter(|(m, p, _)| m == "POST" && p == "/session-roster").count(), 2);
    }
}
