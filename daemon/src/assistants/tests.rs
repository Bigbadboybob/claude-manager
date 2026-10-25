//! Durable assistants (O5): stamping, backfill, teardown survival, the
//! deliberate close, every other death path and continuous workers.

use super::*;
use crate::control::protocol::{Caller, ErrorCode, Request};
use crate::lifecycle_guard;
use crate::manifest::ManifestWorkspace;
use std::sync::{Arc, Mutex};

/// Members on this daemon, as a heartbeat reply hands them over:
/// `(main session uid, member id, kind)`.
fn members(mains: &[(&str, &str, &str)]) {
    let map: serde_json::Map<String, Value> = mains
        .iter()
        .map(|(uid, member, kind)| {
            (uid.to_string(), json!({"id": member, "handle": member, "kind": kind, "acting_as": "main"}))
        })
        .collect();
    crate::items::org::absorb_reply(&json!({"org": {"members": map}}), 1_700_000_000.0);
}

fn session_of(uid: &str, task: Option<&str>, spawner: Option<&str>, ws: &str, kind: &str) -> DaemonSession {
    let mut sp = crate::session::SpawnParams::new(uid, "worker", "/bin/sleep");
    sp.args = vec!["60".into()];
    sp.session_type = kind.into();
    let mut ds = DaemonSession::spawn(sp).expect("spawn /bin/sleep");
    ds.task_id = task.map(str::to_string);
    ds.managed_by_uid = spawner.map(str::to_string);
    ds.workspace_id = ws.to_string();
    // The terminal sweep only closes idle sessions.
    *ds.last_activity_at.lock().unwrap() = None;
    ds
}

fn session(uid: &str, task: Option<&str>, spawner: Option<&str>, ws: &str) -> DaemonSession {
    session_of(uid, task, spawner, ws, "claude-code")
}

fn assistant(uid: &str, task: Option<&str>, spawner: &str, ws: &str, member: &str) -> DaemonSession {
    let mut s = session(uid, task, Some(spawner), ws);
    stamp(&mut s.org, member, ASSISTANT, &format!("spawn:{spawner}"));
    s
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

fn kill_st(st: &mut DaemonState) {
    for s in st.sessions.values_mut() {
        let _ = s.kill();
    }
}

fn role_of(state: &DaemonState, uid: &str) -> Option<(String, &'static str)> {
    state.sessions.get(uid).and_then(of_session).map(|m| (m.member_id, m.acting_as))
}

fn kind(ws: &ManifestWorkspace) -> lifecycle_guard::WorkspaceKind {
    lifecycle_guard::workspace_kind(ws)
}

#[test]
fn a_spawn_by_a_members_session_is_its_assistant_and_inherits_through_assistants() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("as-ep-main", "mem_ep", "agent")]);
    let mut st = DaemonState::default();
    st.sessions.insert("as-ep-main".into(), session("as-ep-main", Some("t-ep"), None, "ws-home"));
    st.sessions.insert("as-loner".into(), session("as-loner", Some("t-x"), None, "ws-x"));

    let mut org = None;
    assert_eq!(stamp_spawn(&st, "as-rev1", Some("as-ep-main"), "claude-code", &mut org), Some(ASSISTANT));
    assert_eq!(stamped_member(org.as_ref()).as_deref(), Some("mem_ep"));
    assert_eq!(org.as_ref().unwrap()[MEMBER_VIA], "spawn:as-ep-main");
    let mut rev1 = session("as-rev1", Some("t-sub"), Some("as-ep-main"), "ws-rev1");
    rev1.org = org;
    st.sessions.insert("as-rev1".into(), rev1);

    // By the assistant: still ep's.
    let mut org = None;
    assert_eq!(stamp_spawn(&st, "as-rev2", Some("as-rev1"), "codex", &mut org), Some(ASSISTANT));
    assert_eq!(stamped_member(org.as_ref()).as_deref(), Some("mem_ep"));
    // A shell spawned by a member is never stamped (Owner's rule).
    let mut org = None;
    assert_eq!(stamp_spawn(&st, "as-sh", Some("as-ep-main"), "bash", &mut org), None);
    assert!(org.is_none());
    // No spawner (Owner/operator-started) and a member-less spawner: nobody's.
    let mut org = None;
    assert_eq!(stamp_spawn(&st, "as-owner-started", None, "claude-code", &mut org), None);
    assert_eq!(stamp_spawn(&st, "as-plain", Some("as-loner"), "claude-code", &mut org), None);
    assert!(org.is_none());
    // A respawn at a uid that already names a member keeps its stamp.
    let mut org = None;
    stamp(&mut org, "mem_other", ASSISTANT, "spawn:x");
    assert_eq!(stamp_spawn(&st, "as-rev3", Some("as-ep-main"), "claude-code", &mut org), None);
    assert_eq!(stamped_member(org.as_ref()).as_deref(), Some("mem_other"));
    // A member's main is never stamped as anyone's assistant.
    let mut org = None;
    assert_eq!(stamp_spawn(&st, "as-ep-main", Some("as-rev1"), "claude-code", &mut org), None);
    // A fork works for its SOURCE's member, whoever forks it.
    let fork = fork_org(&st, "as-rev1", "claude").unwrap();
    assert_eq!((fork[MEMBER_ID].as_str(), fork[ACTING_AS].as_str()), (Some("mem_ep"), Some(ASSISTANT)));
    assert_eq!(fork[MEMBER_VIA], "fork:as-rev1");
    assert!(fork_org(&st, "as-loner", "claude").is_none());

    kill_st(&mut st);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn a_continuous_members_spawns_are_workers_attributed_but_torn_down_as_today() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("cw-orch", "mem_bt", "continuous")]);
    let state = Arc::new(Mutex::new(DaemonState::new()));
    {
        let mut st = state.lock().unwrap();
        st.sessions.insert("cw-orch".into(), session("cw-orch", Some("t-bt"), None, "ws-bt"));
        let mut org = None;
        assert_eq!(stamp_spawn(&st, "cw-1", Some("cw-orch"), "claude-code", &mut org), Some(WORKER));
        let mut w1 = session("cw-1", Some("t-fix"), Some("cw-orch"), "ws-fix");
        w1.org = org;
        st.sessions.insert("cw-1".into(), w1);
        // A worker's spawn stays a worker.
        let mut org = None;
        assert_eq!(stamp_spawn(&st, "cw-2", Some("cw-1"), "codex", &mut org), Some(WORKER));
        let mut w2 = session("cw-2", Some("t-fix"), Some("cw-1"), "ws-fix");
        w2.org = org;
        st.sessions.insert("cw-2".into(), w2);
        // The backfill agrees, for a worker spawned before the stamp existed.
        st.sessions.insert("cw-old".into(), session("cw-old", Some("t-fix"), Some("cw-orch"), "ws-fix"));
        st.workspaces.insert("ws-fix".into(), workspace("ws-fix", Some("ephemeral")));
        assert_eq!(backfill(&mut st), ["cw-old"]);
        assert_eq!(role_of(&st, "cw-old"), Some(("mem_bt".into(), WORKER)));
        assert_eq!(kind(&st.workspaces["ws-fix"]), lifecycle_guard::WorkspaceKind::Ephemeral,
                   "a worker's checkout is never promoted");
        assert_eq!(workspace_kept(&st, "ws-fix"), None, "triage close-out reclaims it");
        // Attributed: the heartbeat reports them, list_sessions shows them.
        let facts = crate::items::org::session_facts(&st, &st.sessions["cw-1"], 1_700_000_000.0);
        assert_eq!((facts["member_id"].as_str(), facts["acting_as"].as_str()), (Some("mem_bt"), Some(WORKER)));
        assert_eq!(wire("cw-1", st.sessions["cw-1"].org.as_ref()),
                   json!({"member_id": "mem_bt", "acting_as": "worker"}));
    }
    // mark_subtask_done's sweep closes workers exactly as today.
    crate::control::methods::sweep_task_sessions(&state, "t-fix", "cw-orch");
    {
        let st = state.lock().unwrap();
        for uid in ["cw-1", "cw-2", "cw-old"] {
            assert!(st.sessions[uid].last_exit.operator_kill_requested(), "{uid}");
        }
        assert!(!st.sessions["cw-orch"].last_exit.operator_kill_requested());
    }
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

/// A fixture shaped like the 2026-10-25 roster: Swarm-Coord leads, lanes are
/// members under it, and lanes spawn workers.
#[test]
fn the_backfill_stamps_only_spawned_sessions_by_spawner_then_task_and_mains_win() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    let mut st = DaemonState::default();
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
    let add = |st: &mut DaemonState, uid: &str, task: Option<&str>, spawner: Option<&str>, engine: &str| {
        st.sessions.insert(uid.into(), session_of(uid, task, spawner, &format!("ws-{uid}"), engine));
        st.workspaces.insert(format!("ws-{uid}"), workspace(&format!("ws-{uid}"), Some("ephemeral")));
    };
    add(&mut st, "bf-bridge", Some("t-elsewhere"), None, "claude-code"); // initiative-bridge: Owner-started
    add(&mut st, "bf-coord", Some("t-root"), None, "claude-code"); // Owner-started member main
    add(&mut st, "bf-items", Some("t-items"), Some("bf-coord"), "claude-code"); // a member spawned by the lead
    add(&mut st, "bf-org", Some("t-org"), Some("bf-coord"), "codex");
    add(&mut st, "bf-w1", Some("t-items-sub"), Some("bf-items"), "claude-code"); // by the lane
    add(&mut st, "bf-w2", Some("t-items-subsub"), Some("bf-w1"), "codex"); // by its worker
    add(&mut st, "bf-gone", Some("t-items-sub"), Some("bf-exited"), "claude-code"); // spawner gone: task chain
    add(&mut st, "bf-solo", Some("t-items-subsub"), None, "claude-code"); // Owner-started under a lane: nobody's
    add(&mut st, "bf-shell", Some("t-items-sub"), Some("bf-items"), "bash"); // a shell: never
    add(&mut st, "bf-out", Some("t-elsewhere"), Some("bf-bridge"), "claude-code");
    // Spawned by a live session that is no member (cm-sessions 2026-10-25:
    // Swarm-Coord's lanes under the hub task): not the hub main's assistant.
    add(&mut st, "bf-nonmember", Some("t-elsewhere"), None, "claude-code");
    add(&mut st, "bf-lane", Some("t-items-sub"), Some("bf-nonmember"), "claude-code");

    assert!(backfill(&mut st).is_empty(), "no beat yet: nothing is stamped");

    members(&[("bf-coord", "mem_coord", "agent"), ("bf-items", "mem_items", "agent"),
              ("bf-org", "mem_org", "agent"), ("bf-bridge", "mem_bridge", "agent")]);
    let mut stamped = backfill(&mut st);
    stamped.sort();
    assert_eq!(stamped, ["bf-gone", "bf-out", "bf-w1", "bf-w2"]);
    for uid in ["bf-w1", "bf-w2", "bf-gone"] {
        assert_eq!(role_of(&st, uid), Some(("mem_items".into(), ASSISTANT)), "{uid}: the nearest member");
    }
    assert_eq!(st.sessions["bf-w1"].org.as_ref().unwrap()[MEMBER_VIA], "spawner:bf-items");
    assert_eq!(st.sessions["bf-gone"].org.as_ref().unwrap()[MEMBER_VIA], "task:t-items",
               "the spawner chain did not resolve, so the task chain decides");
    assert_eq!(role_of(&st, "bf-out"), Some(("mem_bridge".into(), ASSISTANT)), "spawned by the bridge's main");
    // Mains win, and Owner-started sessions are nobody's: Swarm-Coord is
    // never the bridge's assistant.
    for (uid, member) in [("bf-items", "mem_items"), ("bf-org", "mem_org"), ("bf-coord", "mem_coord")] {
        assert_eq!(role_of(&st, uid), Some((member.into(), MAIN)));
        assert!(st.sessions[uid].org.is_none(), "{uid}: a main is never stamped");
    }
    assert!(st.sessions["bf-solo"].org.is_none(), "no spawner: not stamped, even under a lane's task");
    assert!(st.sessions["bf-shell"].org.is_none(), "a shell is never stamped");
    assert!(st.sessions["bf-lane"].org.is_none(),
            "a live non-member spawner: nobody's, never the task chain's member");
    let ws = &st.workspaces["ws-bf-w1"];
    assert_eq!(kind(ws), lifecycle_guard::WorkspaceKind::Stream);
    assert_eq!(ws.org.as_ref().unwrap()[crate::items::org::OFFICE_OF], "mem_items");
    assert_eq!(kind(&st.workspaces["ws-bf-solo"]), lifecycle_guard::WorkspaceKind::Ephemeral);
    assert!(backfill(&mut st).is_empty(), "idempotent");
    assert_eq!(wire("bf-items", None), json!({"member_id": "mem_items", "acting_as": "main"}));
    assert_eq!(wire("bf-solo", None), Value::Null);

    kill_st(&mut st);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn an_assistant_survives_its_subtasks_done_and_the_terminal_sweep_and_a_plain_worker_does_not() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("sw-main", "mem_ep", "agent")]);
    let state = Arc::new(Mutex::new(DaemonState::new()));
    {
        let mut st = state.lock().unwrap();
        st.sessions.insert("sw-main".into(), session("sw-main", Some("t-ep"), None, "ws-home"));
        st.sessions.insert("sw-asst".into(), assistant("sw-asst", Some("t-done"), "sw-main", "ws-asst", "mem_ep"));
        st.sessions.insert("sw-plain".into(), session("sw-plain", Some("t-done"), None, "ws-plain"));
        st.workspaces.insert("ws-asst".into(), workspace("ws-asst", Some("stream")));
        st.workspaces.insert("ws-plain".into(), workspace("ws-plain", Some("ephemeral")));
    }
    let swept = crate::control::methods::sweep_terminal_task_sessions(&state, "t-done", "sweep", None, true);
    assert_eq!(swept, ["sw-plain"]);
    let swept = crate::control::methods::sweep_terminal_task_sessions(&state, "t-done", "sweep", None, false);
    assert!(!swept.contains(&"sw-asst".to_string()), "{swept:?}");
    assert!(!state.lock().unwrap().sessions["sw-asst"].last_exit.operator_kill_requested());

    {
        let mut st = state.lock().unwrap();
        st.sessions.insert("sw-plain2".into(), session("sw-plain2", Some("t-done"), None, "ws-plain"));
    }
    crate::control::methods::sweep_task_sessions(&state, "t-done", "sw-main");
    {
        let mut st = state.lock().unwrap();
        assert!(!st.sessions["sw-asst"].last_exit.operator_kill_requested(), "the assistant is only unassigned");
        assert!(st.sessions["sw-plain2"].last_exit.operator_kill_requested());
        assert_eq!(workspace_kept(&st, "ws-asst"), Some("stream"));
        st.workspaces.insert("ws-asst".into(), workspace("ws-asst", None));
        assert_eq!(workspace_kept(&st, "ws-asst"), Some("assistant"));
        assert_eq!(workspace_kept(&st, "ws-home"), Some("member"));
        assert_eq!(workspace_kept(&st, "ws-plain"), None, "a member-less checkout keeps today's teardown");
    }
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn claims_and_releases_respect_central_marks() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    let mut st = DaemonState::default();
    st.sessions.insert("rw-a".into(), assistant("rw-a", None, "x", "ws-a", "mem_ep"));
    st.workspaces.insert("ws-a".into(), workspace("ws-a", Some("ephemeral")));
    st.workspaces.insert("ws-home".into(), workspace("ws-home", Some("home")));
    st.workspaces.insert("ws-plain".into(), workspace("ws-plain", None));
    assert!(claim_workspace(&mut st, "ws-a", "mem_ep"));
    assert!(!claim_workspace(&mut st, "ws-home", "mem_ep"), "home stays home");
    assert!(!claim_workspace(&mut st, "ws-plain", "mem_ep"), "unclassified is never classified by inference");
    st.sessions.insert("rw-b".into(), assistant("rw-b", None, "y", "ws-b", "mem_other"));
    st.workspaces.insert("ws-b".into(), workspace("ws-b", Some("ephemeral")));
    assert!(!claim_workspace(&mut st, "ws-b", "mem_ep"), "another member's assistant is in it");
    assert!(release_workspace(&mut st, "ws-a", "rw-a"));
    assert_eq!(kind(&st.workspaces["ws-a"]), lifecycle_guard::WorkspaceKind::Ephemeral);
    assert!(!release_workspace(&mut st, "ws-home", "rw-a"));
    kill_st(&mut st);
}

// ---- death paths ------------------------------------------------------------

fn kill_rpc(state: &Arc<Mutex<DaemonState>>, caller: Option<&str>, uid: &str) -> Value {
    crate::control::methods::kill_session(state, &json!({"session_uid": uid}), caller).unwrap()
}

#[test]
fn an_agents_kill_session_ends_an_assistant_like_a_close_and_operator_kills_stay_plain() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("dk-main", "mem_ep", "agent")]);
    let state = Arc::new(Mutex::new(DaemonState::new()));
    {
        let mut st = state.lock().unwrap();
        st.sessions.insert("dk-main".into(), session("dk-main", Some("t-ep"), None, "ws-home"));
        for (uid, ws) in [("dk-a", "ws-a"), ("dk-b", "ws-b")] {
            st.sessions.insert(uid.into(), assistant(uid, Some("t-ep"), "dk-main", ws, "mem_ep"));
            st.workspaces.insert(ws.into(), workspace(ws, Some("ephemeral")));
            assert!(claim_workspace(&mut st, ws, "mem_ep"));
        }
    }
    // The member's main kills its assistant: released at once.
    let out = kill_rpc(&state, Some("dk-main"), "dk-a");
    assert_eq!(out["assistant_ended"], json!({"member_id": "mem_ep", "workspace_released": true}));
    {
        let st = state.lock().unwrap();
        assert!(st.sessions["dk-a"].org.as_ref().unwrap().contains_key(CLOSED_AT));
        assert_eq!(kind(&st.workspaces["ws-a"]), lifecycle_guard::WorkspaceKind::Ephemeral);
    }
    // An operator kill (A-R restart, workflow respawn, A-w fallback): plain.
    let out = kill_rpc(&state, None, "dk-b");
    assert!(out.get("assistant_ended").is_none());
    {
        let st = state.lock().unwrap();
        assert!(!st.sessions["dk-b"].org.as_ref().unwrap().contains_key(CLOSED_AT));
        assert_eq!(kind(&st.workspaces["ws-b"]), lifecycle_guard::WorkspaceKind::Stream,
                   "kept until the 7-day release");
    }
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn a_stream_whose_assistant_died_any_other_way_is_released_after_seven_days_and_a_revive_keeps_it() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    let mut st = DaemonState::default();
    for ws in ["ws-crash", "ws-revived", "ws-live"] {
        st.workspaces.insert(ws.into(), workspace(ws, Some("ephemeral")));
        st.sessions.insert(format!("{ws}-s"), assistant(&format!("{ws}-s"), None, "m", ws, "mem_ep"));
        assert!(claim_workspace(&mut st, ws, "mem_ep"));
    }
    st.workspaces.insert("ws-central".into(), workspace("ws-central", Some("stream")));
    let t0 = 1_800_000_000.0;
    // The assistants of ws-crash and ws-revived die (crash, memory cap,
    // natural exit or a plain kill all look the same here: no live session).
    for uid in ["ws-crash-s", "ws-revived-s"] {
        let mut s = st.sessions.remove(uid).unwrap();
        let _ = s.kill();
    }
    let (released, changed) = release_idle_streams(&mut st, t0);
    assert!(released.is_empty() && changed);
    assert_eq!(st.workspaces["ws-crash"].org.as_ref().unwrap()[IDLE_SINCE], json!(t0));
    assert!(!st.workspaces["ws-live"].org.as_ref().unwrap().contains_key(IDLE_SINCE));
    // Day 3: ws-revived's assistant is revived; its clock clears.
    st.sessions.insert("ws-revived-s".into(), assistant("ws-revived-s", None, "m", "ws-revived", "mem_ep"));
    release_idle_streams(&mut st, t0 + 3.0 * 86400.0);
    assert!(!st.workspaces["ws-revived"].org.as_ref().unwrap().contains_key(IDLE_SINCE));
    assert!(release_idle_streams(&mut st, t0 + STREAM_RELEASE_SECS - 1.0).0.is_empty());
    let (released, _) = release_idle_streams(&mut st, t0 + STREAM_RELEASE_SECS);
    assert_eq!(released, ["ws-crash"]);
    assert_eq!(kind(&st.workspaces["ws-crash"]), lifecycle_guard::WorkspaceKind::Ephemeral);
    for ws in ["ws-revived", "ws-live"] {
        assert_eq!(kind(&st.workspaces[ws]), lifecycle_guard::WorkspaceKind::Stream, "{ws}");
    }
    assert_eq!(kind(&st.workspaces["ws-central"]), lifecycle_guard::WorkspaceKind::Stream,
               "a central stream mark is never released here");
    kill_st(&mut st);
}

#[test]
fn the_worktree_cap_error_names_the_callers_idle_assistants() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("cap-main", "mem_ep", "agent")]);
    let mut st = DaemonState::default();
    st.sessions.insert("cap-main".into(), session("cap-main", None, None, "ws"));
    let mut idle = assistant("cap-idle", None, "cap-main", "ws-i", "mem_ep");
    idle.title = "rev1".into();
    st.sessions.insert("cap-idle".into(), idle);
    let busy = assistant("cap-busy", None, "cap-main", "ws-b", "mem_ep");
    *busy.last_activity_at.lock().unwrap() = Some(std::time::Instant::now());
    st.sessions.insert("cap-busy".into(), busy);
    st.sessions.insert("cap-other".into(), assistant("cap-other", None, "x", "ws-o", "mem_other"));
    let hint = cap_hint(&st, "cap-main").unwrap();
    assert!(hint.contains("1 idle assistant(s): rev1 (cap-idle)"), "{hint}");
    assert!(hint.contains("send_input(session_uid=\"cap-idle\""), "{hint}");
    assert!(hint.contains("org(action=\"close_assistant\", session_uid=\"cap-idle\""), "{hint}");
    assert!(cap_hint(&st, "cap-busy").unwrap().contains("cap-idle"), "an assistant of the member too");
    assert!(cap_hint(&st, "cap-nobody").is_none());
    kill_st(&mut st);
    crate::items::org::reset_cache_for_test();
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
    st.sessions.insert("ca-asst".into(), assistant("ca-asst", Some("t-sub"), "ca-main", "ws-asst", "mem_ep"));
    let mut ws = workspace("ws-asst", Some("ephemeral"));
    ws.worktree_path = Some(root.join("wt"));
    st.workspaces.insert("ws-asst".into(), ws);
    st.sessions.insert("ca-plain".into(), session("ca-plain", Some("t-x"), None, "ws-plain"));
    let mut worker = session("ca-worker", Some("t-x"), Some("o"), "ws-w");
    stamp(&mut worker.org, "mem_bt", WORKER, "spawn:o");
    st.sessions.insert("ca-worker".into(), worker);
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
    members(&[("ca-main", "mem_ep", "agent")]);
    let root = tempfile::tempdir().unwrap();
    let (url, seen) = crate::items::org::test_stub(|method, path, body| match (method, path) {
        ("POST", "/org/assistants/close") if body["actor"]["pid"].as_str().unwrap_or("").ends_with(":ca-stranger") => (
            403,
            r#"{"detail":{"code":"not_manager","message":"only Owner or a member up this member's manager chain may close an assistant of it"}}"#.into(),
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

    let err = close(&state, Caller::session("ca-stranger"), "ca-asst").error.unwrap();
    assert!(matches!(err.code, ErrorCode::Unauthorized), "{err:?}");
    {
        let st = state.lock().unwrap();
        assert!(!st.sessions["ca-asst"].last_exit.operator_kill_requested());
        assert_eq!(kind(&st.workspaces["ws-asst"]), lifecycle_guard::WorkspaceKind::Stream);
    }
    for (uid, what) in [("ca-main", "main session"), ("ca-plain", "belongs to no member"),
                        ("ca-worker", "continuous member's worker")] {
        let err = close(&state, Caller::operator("op"), uid).error.unwrap();
        assert!(matches!(err.code, ErrorCode::Conflict) && err.message.contains(what), "{uid}: {err:?}");
    }
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
        assert_eq!(kind(&st.workspaces["ws-asst"]), lifecycle_guard::WorkspaceKind::Ephemeral);
    }
    let seen = seen.lock().unwrap();
    let (_, _, body) = seen.iter().rev().find(|(m, _, _)| m == "POST").unwrap();
    assert_eq!((body["member_id"].as_str(), body["reason"].as_str()), (Some("mem_ep"), Some("done with it")));
    assert!(body["pid"].as_str().unwrap().ends_with(":ca-asst"));
    assert!(body["actor"]["pid"].as_str().unwrap().ends_with(":ca-main"));
    drop(seen);
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn a_close_re_checks_membership_after_the_api_and_kills_nothing_that_changed() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("ca-main", "mem_ep", "agent")]);
    let root = tempfile::tempdir().unwrap();
    // While the API call runs, the assistant is re-stamped as someone else's.
    let slot: Arc<Mutex<Option<Arc<Mutex<DaemonState>>>>> = Arc::default();
    let hook = slot.clone();
    let (url, _seen) = crate::items::org::test_stub(move |_, _, _| {
        if let Some(state) = hook.lock().unwrap().as_ref() {
            let mut st = state.lock().unwrap();
            let s = st.sessions.get_mut("ca-asst").unwrap();
            stamp(&mut s.org, "mem_other", ASSISTANT, "spawn:z");
        }
        (200, r#"{"holds":{"handed_back":[],"kept":[]}}"#.into())
    });
    let state = rpc_state(&url, root.path());
    *slot.lock().unwrap() = Some(state.clone());
    let err = close(&state, Caller::operator("op"), "ca-asst").error.unwrap();
    assert!(matches!(err.code, ErrorCode::Conflict) && err.message.starts_with("changed"), "{err:?}");
    assert!(!state.lock().unwrap().sessions["ca-asst"].last_exit.operator_kill_requested());
    *slot.lock().unwrap() = None;
    kill_all(&state);
    crate::items::org::reset_cache_for_test();
}

#[test]
fn against_an_old_api_only_owner_or_the_members_main_may_close() {
    let _env = crate::test_support::env_lock();
    crate::items::org::reset_cache_for_test();
    members(&[("ca-main", "mem_ep", "agent")]);
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
    members(&[("ca-main", "mem_ep", "agent")]);
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
    members(&[("hb-main", "mem_ep", "agent")]);
    let st = DaemonState::default();
    let mut asst = assistant("hb-asst", None, "hb-main", "ws", "mem_ep");
    let facts = crate::items::org::session_facts(&st, &asst, 1_700_000_000.0);
    assert_eq!((facts["member_id"].as_str(), facts["acting_as"].as_str()), (Some("mem_ep"), Some("assistant")));
    let mut main = session("hb-main", None, None, "ws");
    stamp(&mut main.org, "mem_old", ASSISTANT, "spawn:x");
    let facts = crate::items::org::session_facts(&st, &main, 1_700_000_000.0);
    assert!(facts.get("member_id").is_none());
    assert_eq!(of_session(&main).unwrap().acting_as, MAIN, "mains win over a stamp");
    let _ = asst.kill();
    let _ = main.kill();
    crate::items::org::reset_cache_for_test();
}
