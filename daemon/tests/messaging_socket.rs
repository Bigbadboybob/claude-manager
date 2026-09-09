//! Wire-level conversation with two session clients and Owner, isolated from
//! installed daemons, credentials, and live agent processes.
use cm_daemon::{
    control::{
        dispatch::{dispatch_request, DispatchOutcome},
        protocol::{Caller, Request, Response},
        wire,
    },
    state::DaemonState,
};
use serde_json::{json, Value};
use std::{
    os::unix::net::{UnixListener, UnixStream},
    sync::{Arc, Mutex},
    thread,
};
#[test]
fn messaging_framed_clients_exchange_channel_dm_and_owner_reply() {
    let tmp = tempfile::tempdir().unwrap();
    // This integration test owns its process, including configuration and
    // workflow files. The only child is /bin/cat, never a live model session.
    unsafe {
        std::env::set_var("HOME", tmp.path());
    }
    let socket = tmp.path().join("chat.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut state = DaemonState::default();
    state.messaging_root = tmp.path().into();
    for uid in ["a", "b"] {
        state.tui_sessions.insert(
            uid.into(),
            serde_json::from_value(json!({"uid":uid,"label":"default"})).unwrap(),
        );
    }
    use cm_daemon::{
        session::{DaemonSession, SpawnParams},
        workflow::run::{self, RoleBinding, WorkflowRun},
    };
    let mut params = SpawnParams::new("a", "default", "/bin/cat");
    params.workflow_run_id = Some("messaging-test-run".into());
    params.workflow_role = Some("worker".into());
    state
        .sessions
        .insert("a".into(), DaemonSession::spawn(params).unwrap());
    let run = WorkflowRun::new(
        "messaging-test-run".into(),
        "test".into(),
        "task".into(),
        std::collections::BTreeMap::from([(
            "worker".into(),
            RoleBinding {
                session_label: "default".into(),
                ..Default::default()
            },
        )]),
        "worker".into(),
        Default::default(),
        None,
        Default::default(),
        0,
    );
    run::save(&run).unwrap();
    state.workflow_runs.insert(run.run_id.clone(), run);
    unsafe {
        std::env::set_var("CM_OPERATOR_TOKEN", "messaging-test-operator");
    }
    cm_daemon::control::operator::init_from_env();
    let state = Arc::new(Mutex::new(state));
    let observed = state.clone();
    let daemon = thread::spawn(move || {
        for _ in 0..10 {
            let (mut stream, _) = listener.accept().unwrap();
            let req = wire::read_request(&mut stream).unwrap().unwrap();
            match dispatch_request(&state, &req) {
                DispatchOutcome::Done(response) => {
                    wire::write_response(&mut stream, &response).unwrap()
                }
                _ => panic!("Unexpected streaming operation"),
            }
        }
    });
    let call = |caller: Caller, method: &str, params: Value| -> Value {
        let mut stream = UnixStream::connect(&socket).unwrap();
        wire::write_request(
            &mut stream,
            &Request {
                id: "wire".into(),
                caller,
                method: format!("messaging.{method}"),
                params,
            },
        )
        .unwrap();
        let Response {
            ok, result, error, ..
        } = wire::read_response(&mut stream).unwrap().unwrap();
        assert!(ok, "{error:?}");
        result.unwrap()
    };
    let people = call(Caller::session("a"), "people", json!({}));
    let b = people["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["session_uid"] == "b")
        .unwrap()["id"]
        .clone();
    call(
        Caller::session("a"),
        "channels",
        json!({"action":"create","path":"work/parser","request_id":"channel"}),
    );
    let added = call(Caller::session("a"), "channels",
        json!({"action":"add_member","path":"work/parser","participant_id":b,"request_id":"add-b"}));
    assert_eq!(added["membership"]["participant_id"], b);
    assert_eq!(added["event"]["actor"]["id"], added["membership"]["added_by"]);
    assert_eq!(call(Caller::session("b"), "channels", json!({"action":"get","path":"work/parser"}))["joined"], true);
    call(
        Caller::session("a"),
        "send",
        json!({"channel":"work/parser","name":"Parser Scout","body":"Ready.","request_id":"first"}),
    );
    let dm = call(
        Caller::session("a"),
        "send",
        json!({"dm":b,"body":"Quick check?","request_id":"dm"}),
    );
    let incoming = call(
        Caller::session("b"),
        "read",
        json!({"dms":true,"unread_only":true}),
    );
    assert_eq!(incoming["items"][0]["id"], dm["event_id"]);
    call(
        Caller::session("b"),
        "send",
        json!({"dm":"owner","name":"Parser Scout","body":"Replying to your requested private check.","request_id":"owner-dm"}),
    );
    // Owner uses the isolated test operator credential, through normal auth.
    let inbox = call(
        Caller::operator("messaging-test-operator"),
        "read",
        json!({"dms":true}),
    );
    assert_eq!(inbox["items"].as_array().unwrap().len(), 1);
    call(
        Caller::operator("messaging-test-operator"),
        "send",
        json!({"conversation":inbox["items"][0]["conversation_id"],"body":"Thanks.","request_id":"owner-reply","reply_to":inbox["items"][0]["id"]}),
    );
    daemon.join().unwrap();
    let state = observed.lock().unwrap();
    assert_eq!(state.sessions["a"].title, "Parser-Scout");
    assert_eq!(state.sessions["a"].workflow_role.as_deref(), Some("worker"));
    assert_eq!(
        run::load_one("messaging-test-run").unwrap().role_sessions["worker"]
            .daemon_session_uid
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        state.workflow_runs["messaging-test-run"].role_sessions["worker"]
            .daemon_session_uid
            .as_deref(),
        Some("a")
    );
}
