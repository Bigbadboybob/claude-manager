//! Real framed socket/auth/manifest delivery, in a separate process so the
//! production operator-token OnceLock is initialized without polluting fixtures.
use cm_daemon::{
    control::{
        dispatch::{dispatch_request, DispatchOutcome},
        operator,
        protocol::{Caller, ErrorCode, Request, Response},
        wire,
    },
    state::DaemonState,
};
use serde_json::json;
use std::{
    os::unix::net::{UnixListener, UnixStream},
    sync::{Arc, Mutex},
};

#[test]
fn owner_attention_wire_delivery_enforces_operator_auth_and_streams_alert() {
    let tmp = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("CM_OPERATOR_TOKEN", "test-owner");
    }
    operator::init_from_env();
    let mut state = DaemonState::new();
    state.daemon_sessions_path = Some(tmp.path().join("daemon-sessions.json"));
    state.tui_sessions.insert(
        "local".into(),
        serde_json::from_value(json!({"uid":"local","label":"Local"})).unwrap(),
    );
    let mut params = cm_daemon::session::SpawnParams::new("cloud", "Continuous", "/bin/sleep");
    params.args = vec!["30".into()];
    params.continuous_task_id = Some("orchestrator".into());
    state.sessions.insert(
        "cloud".into(),
        cm_daemon::session::DaemonSession::spawn(params).unwrap(),
    );
    let state = Arc::new(Mutex::new(state));
    let req = |method: &str, caller, params| Request {
        id: "wire".into(),
        method: method.into(),
        caller,
        params,
    };
    let watch = dispatch_request(
        &state,
        &req("manifest.watch", Caller::operator("test-owner"), json!({})),
    );
    let DispatchOutcome::ManifestWatchStream { handle, .. } = watch else {
        panic!("watch failed")
    };
    assert_eq!(handle.initial_snapshot["owner_attention"], json!({}));
    let socket = tmp.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let worker_state = state.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..5 {
            let (mut stream, _) = listener.accept().unwrap();
            let req = wire::read_request(&mut stream).unwrap().unwrap();
            let response = dispatch_request(&worker_state, &req).into_response();
            wire::write_response(&mut stream, &response).unwrap();
        }
    });
    let call = |request| -> Response {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        wire::write_request(&mut stream, &request).unwrap();
        wire::read_response(&mut stream).unwrap().unwrap()
    };
    for uid in ["local", "cloud"] {
        let response = call(req(
            "notify_user",
            Caller::session(uid),
            json!({"message":"Review ready"}),
        ));
        assert!(response.ok);
        let msg = handle
            .diff_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        let cm_daemon::manifest::ManifestDiff::Updated {
            uid: delivered_uid,
            entry,
        } = msg
        else {
            panic!("missing delivery")
        };
        assert_eq!(delivered_uid, uid);
        assert_eq!(
            entry["owner_attention"]["id"],
            response.result.unwrap()["alert_id"]
        );
    }
    let alert =
        cm_daemon::owner_attention::snapshot(&state.lock().unwrap()).unwrap()["cloud"].clone();
    for caller in [
        Caller::session("cloud"),
        Caller::operator("wrong-token"),
        Caller::operator("test-owner"),
    ] {
        let permitted = matches!(&caller, Caller::Operator(c) if c.token_id == "test-owner");
        let response = call(req(
            "owner_attention.ack",
            caller,
            json!({"session_uid":"cloud","alert_id":alert.id}),
        ));
        assert_eq!(response.ok, permitted);
        if !permitted {
            assert_eq!(response.error.unwrap().code, ErrorCode::Unauthorized);
        }
    }
    worker.join().unwrap();
    assert!(
        !cm_daemon::owner_attention::snapshot(&state.lock().unwrap())
            .unwrap()
            .contains_key("cloud")
    );
}
