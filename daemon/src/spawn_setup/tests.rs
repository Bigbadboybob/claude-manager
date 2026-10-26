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
        // #27's promote, as the API answers it.
        ("POST", "/org/assistants/promote") => (200, json!({
            "member": {"id": "mem_new", "handle": body["handle"], "manager_member_id": "mem_cr",
                       "manager_handle": "cr"},
            "promoted_from": "mem_cr", "notes": [], "home": null, "home_reason": "shared_checkout",
        }).to_string()),
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
    // The creator is a member's main; its spawn is that member's assistant,
    // which is what #27's promote turns into a member.
    crate::items::org::reset_cache_for_test();
    crate::items::org::absorb_reply(&json!({"org": {"members": {"ss-creator":
        {"id": "mem_cr", "handle": "cr", "kind": "agent", "acting_as": "main"}}}}), 1_700_000_000.0);
    {
        let mut st = state.lock().unwrap();
        let child = st.sessions.get_mut("ss-child").unwrap();
        crate::assistants::stamp(&mut child.org, "mem_cr", crate::assistants::ASSISTANT, "spawn:ss-creator");
    }
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
    // hire went through #27's real promote: the session is now the main
    // session of a new member (before poc_of ran).
    assert_eq!((s["hire"]["ok"].as_bool(), s["hire"]["member"]["id"].as_str()), (Some(true), Some("mem_new")), "{s}");
    {
        let st = state.lock().unwrap();
        let m = crate::assistants::of_session(&st.sessions["ss-child"]).unwrap();
        assert_eq!((m.member_id.as_str(), m.acting_as), ("mem_new", crate::assistants::MAIN));
    }

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

    // Setting it up again is idempotent where it can be, and never renames a
    // session that has a name (its own claim may still be on its way to the
    // hub; msgfix).
    let again = setup(&state, Caller::session("ss-creator"), json!({"session_uid": "ss-child", "name": "Lead-Alpha"}));
    assert_eq!(again.result.unwrap()["setup"]["name"], "ok (already named)");
    let other = setup(&state, Caller::session("ss-creator"), json!({"session_uid": "ss-child", "name": "Other"}));
    assert!(other.result.unwrap()["setup"]["name"].as_str().unwrap().starts_with("skipped: it is already named Lead-Alpha"));
    assert_eq!(crate::messaging::rpc::current_name(&state, "ss-child").map(|(n, _)| n).as_deref(), Some("Lead-Alpha"));
    // Hiring it again: promote refuses a member's main, for that step alone.
    let again = setup(&state, Caller::session("ss-creator"), json!({"session_uid": "ss-child", "hire": {"handle": "alpha"}}));
    let hire = again.result.unwrap()["setup"]["hire"].as_str().unwrap().to_string();
    assert!(hire.contains("already_member"), "{hire}");
    crate::items::org::reset_cache_for_test();
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

/// A slow hub never holds start_session up: past the wait the in-spawn claim
/// reads "pending" and still lands in the background.
#[test]
fn a_slow_name_claim_reads_pending_and_completes_in_the_background() {
    let _env = crate::test_support::env_lock();
    let root = tempfile::tempdir().unwrap();
    let (url, _) = api((200, "{}"));
    let state = state(&url, root.path());
    NAME_DELAY_MS.store(400, std::sync::atomic::Ordering::SeqCst);
    let started = std::time::Instant::now();
    let got = claim_name_within(&state, "ss-child", "Slow-Lead", std::time::Duration::from_millis(50));
    NAME_DELAY_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(got, json!("pending"));
    assert!(started.elapsed() < std::time::Duration::from_millis(350), "returned without waiting the claim out");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while crate::messaging::rpc::current_name(&state, "ss-child").is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(crate::messaging::rpc::current_name(&state, "ss-child").map(|(n, _)| n).as_deref(), Some("Slow-Lead"));
    assert_eq!(CLAIM_WAIT, std::time::Duration::from_secs(5));
    kill_all(&state);
}

/// #28 review: a membership can end. board.leave ends the caller's own; the
/// daemon tells the API whether a remover can still edit the board.
#[test]
fn leaving_and_removing_a_board_member_go_to_the_api_with_the_callers_rights() {
    let _env = crate::test_support::env_lock();
    let root = tempfile::tempdir().unwrap();
    let (url, seen) = crate::items::org::test_stub(|method, path, body| match (method, path) {
        ("POST", "/boards/resolve") => (200, r#"{"id":"b-1","slug":"team","name":"Team"}"#.into()),
        ("GET", p) if p.starts_with("/items?") => (200, "[]".into()),
        ("GET", p) if p.starts_with("/boards/team/members") => (200, r#"{"members":[]}"#.into()),
        ("POST", "/boards/team/members/remove") => (200, json!({"board":"team","pid":body["pid"],"removed":true}).to_string()),
        _ => (500, "{}".into()),
    });
    let state = state(&url, root.path());
    let call = |uid: &str, method: &str, params: Value| crate::items::rpc::dispatch(&state, &Request {
        id: "t".into(), caller: Caller::session(uid), method: method.into(), params,
    });
    assert!(call("ss-child", "board.leave", json!({"board": "team"})).ok);
    assert!(call("ss-creator", "board.remove_member", json!({"board": "team", "session": "ss-child"})).ok);
    assert!(!call("ss-creator", "board.remove_member", json!({"board": "team"})).ok, "session is required");
    let seen = seen.lock().unwrap();
    let removes: Vec<&Value> = seen.iter().filter(|(_, p, _)| p == "/boards/team/members/remove").map(|(_, _, b)| b).collect();
    assert_eq!(removes.len(), 2);
    assert!(removes[0]["pid"].as_str().unwrap().ends_with(":ss-child"), "leave names the caller");
    assert_eq!(removes[0]["editor"], false);
    assert!(removes[1]["pid"].as_str().unwrap().ends_with(":ss-child"));
    assert_eq!(removes[1]["editor"], true, "the creator's task resolves to the board: it can edit");
    assert!(removes[1]["actor"]["pid"].as_str().unwrap().ends_with(":ss-creator"));
    drop(seen);
    kill_all(&state);
}
