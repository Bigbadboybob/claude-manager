//! Durable assistants (O5): stamping, backfill, teardown survival and the
//! deliberate close.

use super::*;
use crate::control::protocol::{Caller, ErrorCode, Request};
use crate::manifest::ManifestWorkspace;
use std::sync::{Arc, Mutex};

/// Members on this daemon, as a heartbeat reply hands them over.
fn members(mains: &[(&str, &str)]) {
    let map: serde_json::Map<String, Value> = mains
        .iter()
        .map(|(uid, member)| {
            (uid.to_string(), json!({"id": member, "handle": member, "acting_as": "main"}))
        })
        .collect();
    crate::items::org::absorb_reply(&json!({"org": {"members": map}}), 1_700_000_000.0);
}

fn session(uid: &str, task: Option<&str>, spawner: Option<&str>, ws: &str) -> DaemonSession {
    let mut sp = crate::session::SpawnParams::new(uid, "worker", "/bin/sleep");
    sp.args = vec!["60".into()];
    sp.session_type = "claude-code".into();
    let mut ds = DaemonSession::spawn(sp).expect("spawn /bin/sleep");
    ds.task_id = task.map(str::to_string);
    ds.managed_by_uid = spawner.map(str::to_string);
    ds.workspace_id = ws.to_string();
    // The terminal sweep only closes idle sessions.
    *ds.last_activity_at.lock().unwrap() = None;
    ds
}

fn workspace(id: &str, kind: Option<&str>) -> ManifestWorkspace {
    let mut ws = ManifestWorkspace { id: id.into(), ..Default::default() };
    if let Some(kind) = kind {
        bridge::set(&mut ws.org, bridge::WORKSPACE_KIND, json!(kind));
    }
    ws
}

fn kill_all(state: &Arc<Mutex<DaemonState>>) {
    let mut guard = state.lock().unwrap();
    for s in guard.sessions.values_mut() {
        let _ = s.kill();
    }
    guard.sessions.clear();
}

fn member_of(state: &DaemonState, uid: &str) -> Option<String> {
    state.sessions.get(uid).and_then(of_session).map(|m| m.member_id)
}

#[test]
fn a_spawn_by_a_members_session_is_its_assistant_and_inherits_through_assistants() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("as-ep-main", "mem_ep")]);
    let mut st = DaemonState::default();
    st.sessions.insert("as-ep-main".into(), session("as-ep-main", Some("t-ep"), None, "ws-home"));
    st.sessions.insert("as-loner".into(), session("as-loner", Some("t-x"), None, "ws-x"));

    // By the main: an assistant of ep.
    let mut org = None;
    assert!(stamp_spawn(&st, "as-rev1", Some("as-ep-main"), &mut org));
    assert_eq!(stamped_member(org.as_ref()).as_deref(), Some("mem_ep"));
    assert_eq!(org.as_ref().unwrap()[MEMBER_VIA], "spawn:as-ep-main");
    let mut rev1 = session("as-rev1", Some("t-sub"), Some("as-ep-main"), "ws-rev1");
    rev1.org = org;
    st.sessions.insert("as-rev1".into(), rev1);

    // By the assistant: still ep's.
    let mut org = None;
    assert!(stamp_spawn(&st, "as-rev2", Some("as-rev1"), &mut org));
    assert_eq!(stamped_member(org.as_ref()).as_deref(), Some("mem_ep"));

    // By a session that belongs to no member: today's behaviour.
    let mut org = None;
    assert!(!stamp_spawn(&st, "as-plain", Some("as-loner"), &mut org));
    assert!(org.is_none());

    // A respawn at a uid that already names a member keeps its stamp.
    let mut org = None;
    stamp(&mut org, "mem_other", "spawn:x");
    assert!(!stamp_spawn(&st, "as-rev3", Some("as-ep-main"), &mut org));
    assert_eq!(stamped_member(org.as_ref()).as_deref(), Some("mem_other"));

    // A member's main is never stamped as anyone's assistant.
    let mut org = None;
    assert!(!stamp_spawn(&st, "as-ep-main", Some("as-rev1"), &mut org));

    for s in st.sessions.values_mut() {
        let _ = s.kill();
    }
    crate::items::org::reset_cache_for_test();
}

/// A fixture shaped like the 2026-10-25 roster: Swarm-Coord leads, lanes are
/// members under it, and lanes spawn workers.
#[test]
fn the_backfill_follows_spawners_then_the_task_chain_and_mains_win() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    let mut st = DaemonState::default();
    // root ── lane-items ── items-sub ── items-subsub
    //      └─ lane-org
    //      └─ orphan (no member's main on it or above it below root… root has coord)
    for (task, parent) in [
        ("t-root", None),
        ("t-items", Some("t-root")),
        ("t-items-sub", Some("t-items")),
        ("t-items-subsub", Some("t-items-sub")),
        ("t-org", Some("t-root")),
        ("t-elsewhere", None),
    ] {
        st.task_tree.insert(task.into(), parent.map(str::to_string));
    }
    let add = |st: &mut DaemonState, uid: &str, task: Option<&str>, spawner: Option<&str>| {
        st.sessions.insert(uid.into(), session(uid, task, spawner, &format!("ws-{uid}")));
        st.workspaces.insert(format!("ws-{uid}"), workspace(&format!("ws-{uid}"), Some("ephemeral")));
    };
    add(&mut st, "bf-coord", Some("t-root"), None);
    add(&mut st, "bf-items", Some("t-items"), Some("bf-coord")); // a member spawned by the lead
    add(&mut st, "bf-org", Some("t-org"), Some("bf-coord"));
    add(&mut st, "bf-w1", Some("t-items-sub"), Some("bf-items")); // spawned by the lane
    add(&mut st, "bf-w2", Some("t-items-subsub"), Some("bf-w1")); // by its worker
    add(&mut st, "bf-solo", Some("t-items-subsub"), None); // no spawner: task chain
    add(&mut st, "bf-gone", Some("t-items-sub"), Some("bf-exited")); // spawner exited, no record
    add(&mut st, "bf-out", Some("t-elsewhere"), None); // nobody's
    add(&mut st, "bf-taskless", None, None);

    // No beat yet: nothing is stamped.
    assert!(backfill(&mut st).is_empty());

    members(&[("bf-coord", "mem_coord"), ("bf-items", "mem_items"), ("bf-org", "mem_org")]);
    let mut stamped = backfill(&mut st);
    stamped.sort();
    assert_eq!(stamped, ["bf-gone", "bf-solo", "bf-w1", "bf-w2"]);
    for uid in ["bf-w1", "bf-w2", "bf-solo", "bf-gone"] {
        assert_eq!(member_of(&st, uid).as_deref(), Some("mem_items"), "{uid}: the nearest member");
        assert!(is_assistant(&st.sessions[uid]));
    }
    assert_eq!(st.sessions["bf-w1"].org.as_ref().unwrap()[MEMBER_VIA], "spawner:bf-items");
    assert_eq!(st.sessions["bf-solo"].org.as_ref().unwrap()[MEMBER_VIA], "task:t-items",
               "the task where the member's main was found");
    // Mains win: a lane spawned by the lead stays its own member's main.
    for (uid, member) in [("bf-items", "mem_items"), ("bf-org", "mem_org"), ("bf-coord", "mem_coord")] {
        assert_eq!(of_session(&st.sessions[uid]), Some(Membership { member_id: member.into(), acting_as: MAIN }));
        assert!(st.sessions[uid].org.is_none(), "{uid}: a main is never stamped");
    }
    assert!(member_of(&st, "bf-out").is_none());
    assert!(member_of(&st, "bf-taskless").is_none());
    // An assistant's own ephemeral checkout is now a stream of its member's office.
    let ws = &st.workspaces["ws-bf-w1"];
    assert_eq!(lifecycle_guard::workspace_kind(ws), lifecycle_guard::WorkspaceKind::Stream);
    assert_eq!(ws.org.as_ref().unwrap()[crate::items::org::OFFICE_OF], "mem_items");
    assert_eq!(lifecycle_guard::workspace_kind(&st.workspaces["ws-bf-out"]),
               lifecycle_guard::WorkspaceKind::Ephemeral, "nobody's checkout keeps its kind");
    // Idempotent.
    assert!(backfill(&mut st).is_empty());
    // A wire view for list_sessions / the manifest.
    assert_eq!(wire("bf-w1", st.sessions["bf-w1"].org.as_ref()),
               json!({"member_id": "mem_items", "acting_as": "assistant"}));
    assert_eq!(wire("bf-items", None), json!({"member_id": "mem_items", "acting_as": "main"}));
    assert_eq!(wire("bf-out", None), Value::Null);

    for s in st.sessions.values_mut() {
        let _ = s.kill();
    }
    crate::items::org::reset_cache_for_test();
}

use crate::lifecycle_guard;

#[test]
fn an_assistant_survives_its_subtasks_done_and_the_terminal_sweep_and_a_plain_worker_does_not() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("sw-main", "mem_ep")]);
    let state = Arc::new(Mutex::new(DaemonState::new()));
    {
        let mut st = state.lock().unwrap();
        st.sessions.insert("sw-main".into(), session("sw-main", Some("t-ep"), None, "ws-home"));
        let mut asst = session("sw-asst", Some("t-done"), Some("sw-main"), "ws-asst");
        stamp(&mut asst.org, "mem_ep", "spawn:sw-main");
        st.sessions.insert("sw-asst".into(), asst);
        st.sessions.insert("sw-plain".into(), session("sw-plain", Some("t-done"), None, "ws-plain"));
        st.workspaces.insert("ws-asst".into(), workspace("ws-asst", Some("stream")));
        st.workspaces.insert("ws-plain".into(), workspace("ws-plain", Some("ephemeral")));
    }
    // The terminal sweep: a board close sweeps only ephemeral workspaces'
    // sessions (the plain worker's), a planning close every idle worker's;
    // neither touches the assistant, whose checkout is a stream.
    let swept = crate::control::methods::sweep_terminal_task_sessions(&state, "t-done", "sweep", None, true);
    assert_eq!(swept, ["sw-plain"]);
    let swept = crate::control::methods::sweep_terminal_task_sessions(&state, "t-done", "sweep", None, false);
    assert!(!swept.contains(&"sw-asst".to_string()), "{swept:?}");
    assert!(!state.lock().unwrap().sessions["sw-asst"].last_exit.operator_kill_requested());

    // mark_subtask_done's sweep, and its worktree decision.
    {
        let mut st = state.lock().unwrap();
        st.sessions.insert("sw-plain2".into(), session("sw-plain2", Some("t-done"), None, "ws-plain"));
    }
    crate::control::methods::sweep_task_sessions(&state, "t-done", "sw-main");
    let st = state.lock().unwrap();
    assert!(!st.sessions["sw-asst"].last_exit.operator_kill_requested(), "the assistant is only unassigned");
    assert!(st.sessions["sw-plain2"].last_exit.operator_kill_requested());
    assert_eq!(workspace_kept(&st, "ws-asst"), Some("stream"));
    drop(st);
    {
        // An unclassified checkout an assistant works in is kept too.
        let mut st = state.lock().unwrap();
        st.workspaces.insert("ws-asst".into(), workspace("ws-asst", None));
        assert_eq!(workspace_kept(&st, "ws-asst"), Some("assistant"));
        assert_eq!(workspace_kept(&st, "ws-home"), Some("member"));
        assert_eq!(workspace_kept(&st, "ws-plain"), None, "a member-less checkout keeps today's teardown");
    }
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn a_central_mark_is_never_downgraded_and_an_assistants_stream_is_released_on_close() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    let mut st = DaemonState::default();
    let mut a = session("rw-a", None, None, "ws-a");
    stamp(&mut a.org, "mem_ep", "spawn:x");
    st.sessions.insert("rw-a".into(), a);
    st.workspaces.insert("ws-a".into(), workspace("ws-a", Some("ephemeral")));
    st.workspaces.insert("ws-home".into(), workspace("ws-home", Some("home")));
    st.workspaces.insert("ws-plain".into(), workspace("ws-plain", None));
    assert!(claim_workspace(&mut st, "ws-a", "mem_ep"));
    assert!(!claim_workspace(&mut st, "ws-home", "mem_ep"), "home stays home");
    assert!(!claim_workspace(&mut st, "ws-plain", "mem_ep"), "unclassified is never classified by inference");
    // Another member's assistant in the same checkout: no claim.
    let mut b = session("rw-b", None, None, "ws-b");
    stamp(&mut b.org, "mem_other", "spawn:y");
    st.sessions.insert("rw-b".into(), b);
    st.workspaces.insert("ws-b".into(), workspace("ws-b", Some("ephemeral")));
    assert!(!claim_workspace(&mut st, "ws-b", "mem_ep"));

    assert!(release_workspace(&mut st, "ws-a", "rw-a"));
    assert_eq!(lifecycle_guard::workspace_kind(&st.workspaces["ws-a"]), lifecycle_guard::WorkspaceKind::Ephemeral);
    assert!(!release_workspace(&mut st, "ws-home", "rw-a"));
    for s in st.sessions.values_mut() {
        let _ = s.kill();
    }
}

// ---- org.close_assistant ---------------------------------------------------

fn rpc_state(url: &str, root: &std::path::Path) -> Arc<Mutex<DaemonState>> {
    let mut st = DaemonState::default();
    st.messaging_root = root.into();
    st.config.api_url = url.into();
    st.config.api_token = "tok".into();
    st.daemon_sessions_path = Some(root.join("daemon-sessions.json"));
    for (uid, task) in [("ca-main", "t-ep"), ("ca-stranger", "t-x")] {
        st.sessions.insert(uid.into(), session(uid, Some(task), None, &format!("ws-{uid}")));
    }
    let mut asst = session("ca-asst", Some("t-sub"), Some("ca-main"), "ws-asst");
    stamp(&mut asst.org, "mem_ep", "spawn:ca-main");
    st.sessions.insert("ca-asst".into(), asst);
    let mut ws = workspace("ws-asst", Some("ephemeral"));
    ws.worktree_path = Some(root.join("wt"));
    st.workspaces.insert("ws-asst".into(), ws);
    st.sessions.insert("ca-plain".into(), session("ca-plain", Some("t-x"), None, "ws-plain"));
    assert!(claim_workspace(&mut st, "ws-asst", "mem_ep"));
    let state = Arc::new(Mutex::new(st));
    crate::messaging::startup::open_for_test(&state).unwrap();
    state
}

fn close(state: &Arc<Mutex<DaemonState>>, caller: Caller, uid: &str) -> crate::control::protocol::Response {
    crate::items::org::dispatch(state, &Request {
        id: "t".into(),
        caller,
        method: "org.close_assistant".into(),
        params: json!({"session_uid": uid, "reason": "done with it", "request_id": "r1"}),
    })
}

#[test]
fn closing_an_assistant_asks_the_api_who_may_hands_holds_back_and_releases_its_stream() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("ca-main", "mem_ep")]);
    let root = tempfile::tempdir().unwrap();
    // The API refuses a stranger and allows the main (its own assistant).
    let (url, seen) = crate::items::org::test_stub(|method, path, body| match (method, path) {
        ("POST", "/org/assistants/close") if body["actor"]["pid"].as_str().unwrap_or("").ends_with(":ca-stranger") => (
            403,
            r#"{"detail":{"code":"not_manager","message":"only Owner, its main session, or a member up this member's manager chain may close its assistant"}}"#.into(),
        ),
        ("POST", "/org/assistants/close") => (
            200,
            json!({"pid": body["pid"], "member_id": body["member_id"], "main_pid": "agent:d:ca-main",
                   "holds": {"handed_back": [{"board": "b", "n": 3, "role": "owner", "to": "agent:d:ca-main"}],
                             "kept": []}}).to_string(),
        ),
        _ => (500, "{}".into()),
    });
    let state = rpc_state(&url, root.path());

    // Refused: the session lives on, its checkout stays a stream.
    let err = close(&state, Caller::session("ca-stranger"), "ca-asst").error.unwrap();
    assert!(matches!(err.code, ErrorCode::Unauthorized), "{err:?}");
    {
        let st = state.lock().unwrap();
        assert!(!st.sessions["ca-asst"].last_exit.operator_kill_requested());
        assert_eq!(lifecycle_guard::workspace_kind(&st.workspaces["ws-asst"]), lifecycle_guard::WorkspaceKind::Stream);
    }
    // Not assistants: a main, and a member-less session.
    let err = close(&state, Caller::operator("op"), "ca-main").error.unwrap();
    assert!(matches!(err.code, ErrorCode::Conflict) && err.message.starts_with("not_an_assistant"));
    let err = close(&state, Caller::operator("op"), "ca-plain").error.unwrap();
    assert!(matches!(err.code, ErrorCode::Conflict) && err.message.contains("belongs to no member"));

    // Allowed: the member's main session closes it.
    let resp = close(&state, Caller::session("ca-main"), "ca-asst");
    assert!(resp.ok, "{:?}", resp.error);
    let out = resp.result.unwrap();
    assert_eq!(out["holds"]["handed_back"][0]["n"], 3);
    assert_eq!(out["workspace_released"], true);
    assert_eq!(out["killed_by"], "ca-main (close_assistant)");
    {
        let st = state.lock().unwrap();
        let asst = &st.sessions["ca-asst"];
        assert!(asst.last_exit.operator_kill_requested());
        assert!(asst.org.as_ref().unwrap().contains_key(CLOSED_AT), "the tombstone says it was closed");
        assert_eq!(lifecycle_guard::workspace_kind(&st.workspaces["ws-asst"]),
                   lifecycle_guard::WorkspaceKind::Ephemeral, "the reaper's normal protections apply now");
    }
    let seen = seen.lock().unwrap();
    let (_, _, body) = seen.iter().rev().find(|(m, _, _)| m == "POST").unwrap();
    assert_eq!(body["member_id"], "mem_ep");
    assert_eq!(body["reason"], "done with it");
    assert!(body["pid"].as_str().unwrap().ends_with(":ca-asst"));
    assert!(body["actor"]["pid"].as_str().unwrap().ends_with(":ca-main"));
    drop(seen);
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn against_an_old_api_only_owner_or_the_members_main_may_close() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("ca-main", "mem_ep")]);
    let root = tempfile::tempdir().unwrap();
    let (url, _seen) = crate::items::org::test_stub(|_, _, _| (404, r#"{"detail":"Not Found"}"#.into()));
    let state = rpc_state(&url, root.path());
    let err = close(&state, Caller::session("ca-stranger"), "ca-asst").error.unwrap();
    assert!(matches!(err.code, ErrorCode::Unauthorized) && err.message.starts_with("not_manager"), "{err:?}");
    let resp = close(&state, Caller::operator("op"), "ca-asst");
    assert!(resp.ok, "{:?}", resp.error);
    assert_eq!(resp.result.unwrap()["killed_by"], "operator (close_assistant)");
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn org_member_answers_for_an_assistant_locally() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("ca-main", "mem_ep")]);
    let root = tempfile::tempdir().unwrap();
    let (url, seen) = crate::items::org::test_stub(|_, _, _| (200, r#"{"member":null,"acting_as":null}"#.into()));
    let state = rpc_state(&url, root.path());
    let resp = crate::items::org::dispatch(&state, &Request {
        id: "t".into(),
        caller: Caller::operator("op"),
        method: "org.member".into(),
        params: json!({"session_uid": "ca-asst"}),
    });
    let out = resp.result.unwrap();
    assert_eq!((out["acting_as"].as_str(), out["member"]["id"].as_str()), (Some("assistant"), Some("mem_ep")));
    assert!(seen.lock().unwrap().is_empty(), "no API round trip for a local assistant");
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn the_heartbeat_reports_an_assistants_member_and_never_a_mains() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("hb-main", "mem_ep")]);
    let st = DaemonState::default();
    let mut asst = session("hb-asst", None, Some("hb-main"), "ws");
    stamp(&mut asst.org, "mem_ep", "spawn:hb-main");
    let facts = crate::items::org::session_facts(&st, &asst, 1_700_000_000.0);
    assert_eq!((facts["member_id"].as_str(), facts["acting_as"].as_str()), (Some("mem_ep"), Some("assistant")));
    // A stale stamp on a session that is now a member's main is not reported.
    let mut main = session("hb-main", None, None, "ws");
    stamp(&mut main.org, "mem_old", "spawn:x");
    let facts = crate::items::org::session_facts(&st, &main, 1_700_000_000.0);
    assert!(facts.get("member_id").is_none());
    assert_eq!(of_session(&main).unwrap().acting_as, MAIN, "mains win over a stamp");
    let _ = asst.kill();
    let _ = main.kill();
    crate::items::org::reset_cache_for_test();
}
