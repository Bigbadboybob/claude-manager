//! #28: spawn-time setup runs each step with the creator's authority and
//! reports it per step; a refused step never stops the others.

use super::*;
use std::sync::{Arc, Mutex};

fn spawn(uid: &str, task: &str, spawner: Option<&str>, kind: &str) -> crate::session::DaemonSession {
    let mut sp = crate::session::SpawnParams::new(uid, uid, "/bin/sleep");
    sp.args = vec!["60".into()];
    sp.session_type = kind.into();
    let mut ds = crate::session::DaemonSession::spawn(sp).expect("spawn /bin/sleep");
    ds.task_id = Some(task.into());
    ds.managed_by_uid = spawner.map(str::to_string);
    ds
}

/// A planning API that knows one board (`team`), lets the creator edit it,
/// and refuses `set_poc` the way the API does for a caller without
/// authority over the board.
fn api(poc: (u16, &'static str)) -> (String, Arc<Mutex<Vec<(String, String, Value)>>>) {
    crate::items::org::test_stub(move |method, path, body| match (method, path) {
        ("POST", "/boards/resolve") => (200, r#"{"id":"b-1","slug":"team","name":"Team"}"#.into()),
        ("GET", p) if p.starts_with("/items?") => (200, "[]".into()),
        ("GET", p) if p.starts_with("/boards/team/members") => (200, r#"{"members":[]}"#.into()),
        ("POST", "/boards/team/members") => (200, json!({"board":"team","pid":body["pid"],"joined":true}).to_string()),
        ("PATCH", "/boards/team/items") => (200, json!({"board":{"slug":"team"},"items":[
            {"n":4,"title":"lead the epic","status":"active","holders":[{"pid":body["set"]["holders"][0]["pid"],"name":"child"}]}],
            "unblocked":[],"warnings":[]}).to_string()),
        ("POST", "/boards/team/epics/g-1/poc") => (poc.0, poc.1.into()),
        _ => (500, r#"{"detail":"unexpected"}"#.into()),
    })
}

fn state(url: &str, root: &std::path::Path) -> Arc<Mutex<DaemonState>> {
    let mut st = DaemonState::default();
    st.messaging_root = root.into();
    st.config.api_url = url.into();
    st.config.api_token = "tok".into();
    st.sessions.insert("ss-creator".into(), spawn("ss-creator", "task-a", None, "codex"));
    st.sessions.insert("ss-child".into(), spawn("ss-child", "task-a", Some("ss-creator"), "claude-code"));
    st.sessions.insert("ss-stranger".into(), spawn("ss-stranger", "task-x", None, "codex"));
    st.sessions.insert("ss-shell".into(), spawn("ss-shell", "task-a", Some("ss-creator"), "bash"));
    let state = Arc::new(Mutex::new(st));
    crate::messaging::startup::open_for_test(&state).unwrap();
    state
}

fn setup(state: &Arc<Mutex<DaemonState>>, caller: Caller, params: Value) -> Response {
    dispatch(state, &Request { id: "t".into(), caller, method: METHOD.into(), params })
}

fn kill_all(state: &Arc<Mutex<DaemonState>>) {
    let mut st = state.lock().unwrap();
    for s in st.sessions.values_mut() {
        let _ = s.kill();
    }
}

#[test]
fn every_step_runs_with_the_creators_authority_and_a_refused_one_stops_nothing() {
    let _env = crate::test_support::env_lock();
    let root = tempfile::tempdir().unwrap();
    let (url, seen) = api((403, r#"{"detail":{"code":"forbidden","message":"only Owner, the board's lead (its orchestrator), its steward or the epic's current PoC chain may set poc"}}"#));
    let state = state(&url, root.path());
    let resp = setup(&state, Caller::session("ss-creator"), json!({
        "session_uid": "ss-child", "name": "Lead-Alpha", "channels": ["general"],
        "board": "team", "epic": "g-1", "items": [4], "poc_of": "g-1",
        "hire": {"handle": "alpha", "title": "Alpha lead"},
    }));
    assert!(resp.ok, "{:?}", resp.error);
    let out = resp.result.unwrap();
    let s = &out["setup"];
    assert_eq!(s["name"], "ok", "{s}");
    assert_eq!(crate::messaging::rpc::current_name(&state, "ss-child").map(|(n, _)| n).as_deref(),
               Some("Lead-Alpha"), "claimed before the session's first message");
    assert!(s["channels"][0]["result"].as_str().unwrap().starts_with("ok"), "{s}");
    assert_eq!(s["board"], "ok");
    assert_eq!(s["items"], json!([{"n": 4, "result": "ok"}]));
    assert!(s["poc"].as_str().unwrap().starts_with("refused: "), "{s}");
    let hire = s["hire"].as_str().unwrap();
    assert!(hire.starts_with("failed: ") && hire.contains("#27"), "no promote on this daemon yet: {hire}");

    // Every board write went out as the CREATOR, naming the child.
    let pid = out["pid"].as_str().unwrap().to_string();
    assert!(pid.ends_with(":ss-child"));
    let seen = seen.lock().unwrap();
    let join = seen.iter().find(|(m, p, _)| m == "POST" && p == "/boards/team/members").unwrap();
    assert_eq!((join.2["pid"].as_str(), join.2["group_id"].as_str()), (Some(pid.as_str()), Some("g-1")));
    assert!(join.2["actor"]["pid"].as_str().unwrap().ends_with(":ss-creator"));
    let set = seen.iter().find(|(m, p, _)| m == "PATCH" && p == "/boards/team/items").unwrap();
    assert!(set.2["actor"]["pid"].as_str().unwrap().ends_with(":ss-creator"));
    let poc = seen.iter().find(|(_, p, _)| p == "/boards/team/epics/g-1/poc").unwrap();
    assert_eq!(poc.2["member"].as_str(), Some(pid.as_str()), "the PoC is named by its session");
    drop(seen);

    // Setting it up again is idempotent where it can be.
    let again = setup(&state, Caller::session("ss-creator"), json!({"session_uid": "ss-child", "name": "Lead-Alpha"}));
    assert_eq!(again.result.unwrap()["setup"]["name"], "ok (already named)");
    kill_all(&state);
}

#[test]
fn only_the_spawner_owner_or_a_global_session_may_set_a_session_up() {
    let _env = crate::test_support::env_lock();
    let root = tempfile::tempdir().unwrap();
    let (url, _) = api((200, r#"{"team_id":"t-1","joined_board":[]}"#));
    let state = state(&url, root.path());
    let err = setup(&state, Caller::session("ss-stranger"), json!({"session_uid": "ss-child", "name": "Nope"}))
        .error.unwrap();
    assert!(matches!(err.code, ErrorCode::Unauthorized) && err.message.starts_with("not_spawner"), "{err:?}");
    assert!(crate::messaging::rpc::current_name(&state, "ss-child").is_none());
    let err = setup(&state, Caller::session("ss-creator"), json!({"session_uid": "ss-shell", "name": "sh"}))
        .error.unwrap();
    assert!(err.message.starts_with("agent_only"), "a shell has nothing to set up: {err:?}");
    let err = setup(&state, Caller::session("ss-creator"), json!({"session_uid": "ss-gone"})).error.unwrap();
    assert!(matches!(err.code, ErrorCode::NotFound));
    // A global session may set up any session.
    state.lock().unwrap().sessions.get_mut("ss-stranger").unwrap().global_perms = true;
    let resp = setup(&state, Caller::session("ss-stranger"), json!({"session_uid": "ss-child", "poc_of": "g-1", "board": "team"}));
    assert_eq!(resp.result.unwrap()["setup"]["poc"]["ok"], true);
    kill_all(&state);
}

#[test]
fn step_results_separate_refusals_from_failures() {
    assert_eq!(step(Ok(json!({"x": 1}))), json!("ok"));
    assert_eq!(step(Ok(json!("ok (already named)"))), json!("ok (already named)"));
    assert_eq!(step(Err((ErrorCode::Unauthorized, "no".into()))), json!("refused: no"));
    assert_eq!(step(Err((ErrorCode::Conflict, "forbidden: x".into()))), json!("refused: forbidden: x"));
    assert_eq!(step(Err((ErrorCode::Conflict, "boom".into()))), json!("failed: boom"));
}
