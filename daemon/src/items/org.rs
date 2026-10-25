//! Org model S2 (doc/org-model.md §4, §6, §10-§12): daemon proxies to the
//! planning API's `/org` endpoints, the per-session facts the heartbeat
//! reports for the roster, and the member cache `ping` reads.
//!
//! The daemon stamps the actor as for items: Owner is the operator caller
//! (pid `owner`). Roster and adoption are Owner-only; the API re-checks
//! every write. Nothing here enforces anything: holds stay keyed by pid and
//! session scope is unchanged until S6/S7.
//!
//! Lock discipline: the `DaemonState` mutex is never held across an API call.

use super::rpc::{bad, Ctx, RpcResult};
use crate::control::protocol::{ErrorCode, Request, Response};
use crate::session::DaemonSession;
use crate::state::DaemonState;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub const METHODS: &[&str] = &[
    "org.snapshot",
    "org.member",
    "org.team",
    "org.roster",
    "org.roster_apply",
    "org.adopt",
    "org.team_create",
    "org.team_update",
    "org.move",
];

/// The read-only ones (restart-barrier classification).
pub const READ_METHODS: &[&str] = &["org.snapshot", "org.member", "org.team", "org.roster"];

pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let result = Ctx::capture(state, req).and_then(|ctx| {
        let p = params(&req.params)?;
        match req.method.as_str() {
            "org.snapshot" => {
                let mut query = Vec::new();
                if let Some(v) = p.get("since_version").and_then(Value::as_i64) {
                    query.push(("since_version", v.to_string()));
                }
                api(ctx.api.get("/org/snapshot", &query))
            }
            "org.member" => member(&ctx, &p),
            "org.team" => {
                let team = str_param(&p, "team")?.ok_or_else(|| bad("org.team needs team"))?;
                api(ctx.api.get(&format!("/org/teams/{}", encode(&team)), &[]))
            }
            "org.roster" => {
                owner_only(&ctx, "the roster")?;
                api(ctx.api.get("/org/roster", &[]))
            }
            "org.roster_apply" => {
                owner_only(&ctx, "approving the roster")?;
                api(ctx.api.post("/org/roster/apply", &with_actor(&ctx, p)))
            }
            "org.adopt" => adopt(&ctx, p),
            "org.team_create" => api(ctx.api.post("/org/teams", &with_actor(&ctx, p))),
            "org.team_update" => {
                let mut p = p;
                let team = take_str(&mut p, "team")?.ok_or_else(|| bad("org.team_update needs team"))?;
                api(ctx.api.patch(&format!("/org/teams/{}", encode(&team)), &with_actor(&ctx, p)))
            }
            "org.move" => {
                let mut p = p;
                let member = take_str(&mut p, "member")?.ok_or_else(|| bad("org.move needs member"))?;
                api(ctx.api.post(&format!("/org/members/{}/move", encode(&member)), &with_actor(&ctx, p)))
            }
            other => Err((ErrorCode::UnknownMethod, format!("unknown org method {other}"))),
        }
    });
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

fn params(raw: &Value) -> RpcResult<Map<String, Value>> {
    match raw {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map.clone()),
        _ => Err(bad("expected object parameters")),
    }
}

fn str_param(p: &Map<String, Value>, key: &str) -> RpcResult<Option<String>> {
    match p.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim().to_string())),
        Some(_) => Err(bad(format!("{key} must be a non-empty string"))),
    }
}

fn take_str(p: &mut Map<String, Value>, key: &str) -> RpcResult<Option<String>> {
    let value = str_param(p, key)?;
    p.remove(key);
    Ok(value)
}

fn owner_only(ctx: &Ctx, what: &str) -> RpcResult<()> {
    if ctx.owner {
        Ok(())
    } else {
        Err((ErrorCode::Unauthorized, format!("owner_only: {what} is Owner's (the TUI roster screen)")))
    }
}

fn with_actor(ctx: &Ctx, mut p: Map<String, Value>) -> Value {
    p.insert("actor".into(), ctx.actor());
    Value::Object(p)
}

/// An old planning API has no `/org` routes: FastAPI answers a bare
/// `Not Found`, which callers (the TUI) read as "feature off".
fn api(result: RpcResult) -> RpcResult {
    result.map_err(|(code, message)| {
        if matches!(code, ErrorCode::NotFound) && message == "Not Found" {
            (ErrorCode::UnknownMethod, "unsupported: the planning API has no org endpoints yet".into())
        } else {
            (code, message)
        }
    })
}

/// Percent-encode a path segment, keeping `/` (qualified handles `main/ep`
/// travel as a path).
fn encode(segment: &str) -> String {
    let mut out = String::new();
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn member(ctx: &Ctx, p: &Map<String, Value>) -> RpcResult {
    if let Some(reference) = str_param(p, "member")? {
        return api(ctx.api.get(&format!("/org/members/{}", encode(&reference)), &[]));
    }
    let pid = match (str_param(p, "pid")?, str_param(p, "session_uid")?) {
        (Some(pid), _) => pid,
        (None, Some(uid)) => format!("agent:{}:{uid}", ctx.daemon_id),
        (None, None) if !ctx.owner => ctx.pid.clone(),
        (None, None) => return api(ctx.api.get("/org/members/owner", &[])),
    };
    api(ctx.api.get("/org/members/by-pid", &[("pid", pid)]))
}

/// `org.adopt`: Owner binds a live session to a new member. The TUI may
/// not know a remote host's daemon id, so `session_uid` (resolved against
/// this daemon) can stand in for `pid`; when both are given they must agree.
fn adopt(ctx: &Ctx, mut p: Map<String, Value>) -> RpcResult {
    owner_only(ctx, "adopting a session")?;
    let uid = take_str(&mut p, "session_uid")?;
    let pid = str_param(&p, "pid")?;
    let pid = match (pid, uid) {
        (Some(pid), Some(uid)) if pid != format!("agent:{}:{uid}", ctx.daemon_id) => {
            return Err(bad(format!("pid {pid} and session_uid {uid} name different sessions")));
        }
        (Some(pid), _) => pid,
        (None, Some(uid)) => format!("agent:{}:{uid}", ctx.daemon_id),
        (None, None) => return Err(bad("org.adopt needs pid or session_uid")),
    };
    p.insert("pid".into(), json!(pid));
    api(ctx.api.post("/org/adopt", &with_actor(ctx, p)))
}

/// `GET /org/members/by-pid` for a session on this daemon (the `org` MCP
/// tool's `view="me"`). `Ok(None)` when the planning API predates S2.
pub fn member_for_session(state: &Arc<Mutex<DaemonState>>, uid: &str) -> RpcResult<Option<Value>> {
    let (handle, api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone())
    };
    let daemon_id = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref().map(|s| s.daemon_id.clone())
    }
    .ok_or_else(|| (ErrorCode::Internal, "daemon_id_unavailable: messaging store is not open".to_string()))?;
    let client = super::api::Api::from_config(&api_url, &api_token)?;
    match api(client.get("/org/members/by-pid", &[("pid", format!("agent:{daemon_id}:{uid}"))])) {
        Ok(v) => Ok(Some(v)),
        Err((ErrorCode::UnknownMethod, _)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// A team's detail (`GET /org/teams/{ref}`); `Ok(None)` on an old API or an
/// unknown team.
pub fn team_detail(state: &Arc<Mutex<DaemonState>>, reference: &str) -> RpcResult<Option<Value>> {
    let (api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.config.api_url.clone(), s.config.api_token.clone())
    };
    let client = super::api::Api::from_config(&api_url, &api_token)?;
    match api(client.get(&format!("/org/teams/{}", encode(reference)), &[])) {
        Ok(v) => Ok(Some(v)),
        Err((ErrorCode::UnknownMethod | ErrorCode::NotFound, _)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `GET /org/members/{ref}` (the `org` MCP tool's `view="member"`).
pub fn member_detail(state: &Arc<Mutex<DaemonState>>, reference: &str) -> RpcResult {
    let (api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.config.api_url.clone(), s.config.api_token.clone())
    };
    let client = super::api::Api::from_config(&api_url, &api_token)?;
    api(client.get(&format!("/org/members/{}", encode(reference)), &[]))
}

// ---- heartbeat: facts out, member cache in ----------------------------------

/// The per-session facts the roster needs, merged into this session's
/// heartbeat row. An old API ignores unknown row keys.
pub(crate) fn session_facts(state: &DaemonState, sess: &DaemonSession, now: f64) -> Map<String, Value> {
    let mut facts = Map::new();
    let worktree = state
        .workspaces
        .get(&sess.workspace_id)
        .and_then(|w| w.worktree_path.as_ref())
        .map(|p| p.display().to_string());
    facts.insert("workspace_id".into(), json!(sess.workspace_id));
    facts.insert("worktree_path".into(), json!(worktree));
    facts.insert("continuous_task_id".into(), json!(sess.continuous_task_id));
    facts.insert("managed_by_uid".into(), json!(sess.managed_by_uid));
    facts.insert("workflow_run_id".into(), json!(sess.workflow_run_id));
    if let Some(age) = spawned_age_s(&sess.uid, now) {
        facts.insert("spawned_age_s".into(), json!(age));
    }
    facts
}

/// Seconds since the spawn stamped in a `ts-<nanos hex>-<counter>` uid
/// (A-R keeps the uid, so this is the session's original start).
pub(crate) fn spawned_age_s(uid: &str, now: f64) -> Option<f64> {
    let (rank, nanos, _) = crate::org::spawn_order(uid);
    if rank != 0 {
        return None;
    }
    let age = now - (nanos as f64) / 1e9;
    (age >= 0.0 && age < 10.0 * 365.0 * 86400.0).then_some(age)
}

#[derive(Default)]
struct MemberCache {
    /// session uid → member summary (`acting_as: main`), from the last reply.
    by_uid: HashMap<String, Value>,
    at: Option<f64>,
}

fn cache() -> &'static Mutex<MemberCache> {
    static CACHE: OnceLock<Mutex<MemberCache>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Fold a heartbeat reply's `org.members` into the cache. A reply without
/// the key (an API before S2) leaves the cache as it was.
pub(crate) fn absorb_reply(reply: &Value, now: f64) {
    let Some(members) = reply["org"]["members"].as_object() else { return };
    let mut c = cache().lock().unwrap_or_else(|p| p.into_inner());
    c.by_uid = members.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    c.at = Some(now);
}

/// The member a session on this daemon works as, from the last heartbeat
/// (at most ~30 s old). Never blocks on the network: `ping` reads it.
pub fn cached_member(uid: &str) -> Value {
    let c = cache().lock().unwrap_or_else(|p| p.into_inner());
    match (c.by_uid.get(uid), c.at) {
        (Some(m), Some(at)) => {
            let mut m = m.clone();
            m["as_of"] = json!(chrono::DateTime::from_timestamp(at as i64, 0).map(|t| t.to_rfc3339()));
            m
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
pub(crate) fn reset_cache_for_test() {
    *cache().lock().unwrap_or_else(|p| p.into_inner()) = MemberCache::default();
}

/// A stub planning API for org tests: `reply(method, path, body)`; every
/// request is recorded as (method, path, body).
#[cfg(test)]
pub(crate) fn test_stub(
    reply: impl Fn(&str, &str, &Value) -> (u16, String) + Send + 'static,
) -> (String, Arc<Mutex<Vec<(String, String, Value)>>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let seen: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let parts: Vec<&str> = first.split_whitespace().collect();
            let (method, path) = (parts[0].to_string(), parts[1].to_string());
            let value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let (status, text) = reply(&method, &path, &value);
            log.lock().unwrap().push((method, path, value));
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    (url, seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::protocol::Caller;

    fn state(root: &std::path::Path, url: &str) -> Arc<Mutex<DaemonState>> {
        let mut st = DaemonState::default();
        st.messaging_root = root.into();
        st.config.api_url = url.into();
        st.config.api_token = "tok".into();
        st.tui_sessions.insert(
            "me-uid".into(),
            serde_json::from_value(json!({"uid":"me-uid","label":"me","task_id":"t-1","type":"codex"})).unwrap(),
        );
        let state = Arc::new(Mutex::new(st));
        crate::messaging::startup::open_for_test(&state).unwrap();
        state
    }

    fn daemon_id(state: &Arc<Mutex<DaemonState>>) -> String {
        let handle = state.lock().unwrap().messaging.clone();
        let slot = handle.lock().unwrap();
        slot.as_ref().unwrap().daemon_id.clone()
    }

    fn call(state: &Arc<Mutex<DaemonState>>, caller: Caller, method: &str, params: Value) -> Response {
        dispatch(state, &Request { id: "t".into(), caller, method: method.into(), params })
    }

    #[test]
    fn adopt_and_the_roster_are_owners_and_adopt_fills_in_this_daemons_id() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = test_stub(|method, path, body| match (method, path) {
            ("POST", "/org/adopt") => (200, json!({"member": {"handle": body["handle"]}, "home_mark": null}).to_string()),
            ("GET", "/org/roster") => (200, r#"{"proposals":[],"declined":[],"last_applied_at":null}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state(root.path(), &url);
        let d = daemon_id(&state);
        let me = Caller::session("me-uid");
        for (method, params) in [("org.adopt", json!({"session_uid": "u2", "handle": "rev"})),
                                 ("org.roster", json!({})),
                                 ("org.roster_apply", json!({"rows": []}))] {
            let resp = call(&state, me.clone(), method, params);
            assert!(matches!(resp.error.unwrap().code, ErrorCode::Unauthorized), "{method}");
        }
        let owner = Caller::operator("op");
        let resp = call(&state, owner.clone(), "org.adopt", json!({"session_uid": "u2", "handle": "rev", "request_id": "r1"}));
        assert!(resp.ok, "{:?}", resp.error);
        let resp = call(&state, owner.clone(), "org.adopt",
                        json!({"session_uid": "u2", "pid": "agent:other:u2", "handle": "rev"}));
        assert!(matches!(resp.error.unwrap().code, ErrorCode::InvalidParams));
        assert!(call(&state, owner, "org.roster", json!({})).ok);
        let seen = seen.lock().unwrap();
        let (_, _, body) = seen.iter().find(|(m, p, _)| m == "POST" && p == "/org/adopt").unwrap();
        assert_eq!(body["pid"], format!("agent:{d}:u2"));
        assert_eq!(body["actor"]["pid"], "owner");
        assert!(body.get("session_uid").is_none());
        assert_eq!(seen.iter().filter(|(m, p, _)| m == "POST" && p == "/org/adopt").count(), 1);
    }

    #[test]
    fn reads_and_moves_proxy_with_the_callers_actor() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let (url, seen) = test_stub(|method, path, _| match (method, path) {
            ("GET", "/org/snapshot?since_version=7") => (200, r#"{"unchanged":true,"version":7}"#.into()),
            ("GET", p) if p.starts_with("/org/members/by-pid?pid=") => (200, r#"{"member":null}"#.into()),
            ("POST", "/org/members/main/ep/move") => (200, r#"{"member":{"handle":"ep"}}"#.into()),
            ("GET", "/org/teams/swarm") => (404, r#"{"detail":"Not Found"}"#.into()),
            _ => (500, "{}".into()),
        });
        let state = state(root.path(), &url);
        let d = daemon_id(&state);
        let me = Caller::session("me-uid");
        let resp = call(&state, me.clone(), "org.snapshot", json!({"since_version": 7}));
        assert_eq!(resp.result.unwrap()["unchanged"], true);
        assert!(call(&state, me.clone(), "org.member", json!({})).ok);
        let resp = call(&state, me.clone(), "org.move",
                        json!({"member": "main/ep", "manager": "owner", "reason": "r"}));
        assert!(resp.ok, "{:?}", resp.error);
        // A planning API before S2: the TUI turns the feature off.
        let err = call(&state, me, "org.team", json!({"team": "swarm"})).error.unwrap();
        assert!(matches!(err.code, ErrorCode::UnknownMethod) && err.message.starts_with("unsupported"));
        let seen = seen.lock().unwrap();
        let (_, path, _) = seen.iter().find(|(_, p, _)| p.starts_with("/org/members/by-pid")).unwrap();
        assert_eq!(path, &format!("/org/members/by-pid?pid=agent%3A{d}%3Ame-uid"));
        let (_, _, body) = seen.iter().find(|(m, _, _)| m == "POST").unwrap();
        assert_eq!(body["actor"]["pid"], format!("agent:{d}:me-uid"));
        assert!(body.get("member").is_none());
    }

    #[test]
    fn path_segments_are_encoded_but_qualified_handles_keep_their_slash() {
        assert_eq!(encode("main/ep"), "main/ep");
        assert_eq!(encode("a b?c"), "a%20b%3Fc");
    }

    #[test]
    fn spawn_age_comes_from_the_uid() {
        let now = 2_000_000_000.0;
        let uid = format!("ts-{:x}-1", (now as u128 - 3600) * 1_000_000_000);
        let age = spawned_age_s(&uid, now).unwrap();
        assert!((age - 3600.0).abs() < 1.0, "{age}");
        assert_eq!(spawned_age_s("legacy-uid", now), None);
    }

    #[test]
    fn the_cache_follows_replies_and_ignores_old_apis() {
        let _env = crate::test_support::env_lock();
        reset_cache_for_test();
        assert!(cached_member("u1").is_null());
        absorb_reply(&json!({"org": {"members": {"u1": {"id": "mem_1", "handle": "ep", "acting_as": "main"}}}}), 1_700_000_000.0);
        assert_eq!(cached_member("u1")["handle"], "ep");
        assert!(cached_member("u1")["as_of"].as_str().is_some());
        absorb_reply(&json!({"pushes": []}), 1_700_000_100.0); // pre-S2 API
        assert_eq!(cached_member("u1")["handle"], "ep");
        absorb_reply(&json!({"org": {"members": {}}}), 1_700_000_200.0);
        assert!(cached_member("u1").is_null());
        reset_cache_for_test();
    }

    #[test]
    fn an_old_api_reads_as_unsupported() {
        let err = api(Err((ErrorCode::NotFound, "Not Found".into()))).unwrap_err();
        assert!(matches!(err.0, ErrorCode::UnknownMethod));
        assert!(err.1.starts_with("unsupported"));
        let err = api(Err((ErrorCode::NotFound, "not_found: no member 'x'".into()))).unwrap_err();
        assert!(matches!(err.0, ErrorCode::NotFound));
    }
}
