//! Owner attention is separate from agent messaging and out-of-band escalation.
//! Persist before broadcasting; manifest.watch replays pending alerts on reconnect.
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::manifest::ManifestDiff;
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Alert {
    pub id: String,
    pub session_uid: String,
    pub label: String,
    pub message: String,
    pub task_id: Option<String>,
    pub continuous_task_id: Option<String>,
}

fn path(state: &DaemonState) -> PathBuf {
    state
        .daemon_sessions_path
        .clone()
        .unwrap_or_else(crate::state::default_daemon_sessions_path)
        .with_file_name("owner-attention.json")
}

pub fn snapshot(state: &DaemonState) -> Result<BTreeMap<String, Alert>, String> {
    match std::fs::read(path(state)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("read Owner alerts: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("read Owner alerts: {e}")),
    }
}

fn save(state: &DaemonState, alerts: &BTreeMap<String, Alert>) -> Result<(), String> {
    let json = serde_json::to_string(alerts).unwrap();
    if json.len() > 512 * 1024 {
        return Err("Owner alert queue is full; pending alerts were retained".into());
    }
    crate::state::write_json_atomic(&path(state), &json, true)
        .map_err(|e| format!("persist Owner alerts: {e}"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotifyParams {
    #[serde(default)]
    message: String,
}

// Called under the daemon state lock and the ordinary restart mutation barrier.
pub fn notify(state: &DaemonState, req: &Request) -> Response {
    let Caller::Session(caller) = &req.caller else {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "notify_user requires a live Session caller",
        );
    };
    let uid = &caller.session_uid;
    let Some(view) = state.lookup_session_any(uid) else {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "notify_user caller is not a live session",
        );
    };
    if state
        .sessions
        .get(uid)
        .is_some_and(|s| s.last_exit.kernel_set())
    {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "notify_user caller has exited",
        );
    }
    let params: NotifyParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e.to_string()),
    };
    if params.message.len() > 4096 {
        return Response::err(
            req.id.clone(),
            ErrorCode::InvalidParams,
            "message must be at most 4096 UTF-8 bytes",
        );
    }
    let (label, continuous_task_id) = if let Some(s) = state.sessions.get(uid) {
        (s.title.clone(), s.continuous_task_id.clone())
    } else {
        (
            state
                .tui_sessions
                .get(uid)
                .and_then(|s| s.label.clone())
                .unwrap_or_else(|| uid.clone()),
            None,
        )
    };
    let mut alerts = match snapshot(state) {
        Ok(a) => a,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::Internal, e),
    };
    // Collapse retries of the same pending request. A changed reason re-alerts;
    // after Owner acknowledges, the same reason can raise a new alert.
    if let Some(old) = alerts.get(uid).filter(|a| a.message == params.message) {
        return Response::ok(
            req.id.clone(),
            serde_json::json!({"ok": true, "status": "queued", "alert_id": old.id}),
        );
    }
    if alerts.len() >= 4096 && !alerts.contains_key(uid) {
        return Response::err(
            req.id.clone(),
            ErrorCode::Internal,
            "Owner alert queue is full; pending alerts were retained",
        );
    }
    let alert = Alert {
        id: uuid::Uuid::new_v4().to_string(),
        session_uid: uid.clone(),
        label,
        message: params.message,
        task_id: view.task_id,
        continuous_task_id,
    };
    alerts.insert(uid.clone(), alert.clone());
    if let Err(e) = save(state, &alerts) {
        return Response::err(req.id.clone(), ErrorCode::Internal, e);
    }
    state.manifest_watcher.broadcast(ManifestDiff::Updated {
        uid: uid.clone(),
        entry: serde_json::json!({"owner_attention": alert}),
    });
    Response::ok(
        req.id.clone(),
        serde_json::json!({"ok": true, "status": "queued", "alert_id": alert.id}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AckParams {
    session_uid: String,
    alert_id: String,
}

/// Operator gate is enforced in dispatch. Compare IDs so a delayed acknowledgement
/// cannot delete a newer request from the same session.
pub fn acknowledge(state: &DaemonState, req: &Request) -> Response {
    let params: AckParams = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e.to_string()),
    };
    let mut alerts = match snapshot(state) {
        Ok(a) => a,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::Internal, e),
    };
    let matched = alerts
        .get(&params.session_uid)
        .is_some_and(|a| a.id == params.alert_id);
    if matched {
        alerts.remove(&params.session_uid);
        if let Err(e) = save(state, &alerts) {
            return Response::err(req.id.clone(), ErrorCode::Internal, e);
        }
        state.manifest_watcher.broadcast(ManifestDiff::Updated {
            uid: params.session_uid,
            entry: serde_json::json!({"owner_attention": null}),
        });
    }
    Response::ok(
        req.id.clone(),
        serde_json::json!({"ok": true, "acknowledged": matched}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::dispatch::{dispatch_request, DispatchOutcome};
    use crate::session::{DaemonSession, SpawnParams};
    use std::sync::{Arc, Mutex};

    fn fixture() -> (tempfile::TempDir, Arc<Mutex<DaemonState>>) {
        let dir = tempfile::tempdir().unwrap();
        let mut state = DaemonState::new();
        state.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        (dir, Arc::new(Mutex::new(state)))
    }
    fn request(method: &str, caller: Caller, params: serde_json::Value) -> Request {
        Request {
            id: "request".into(),
            method: method.into(),
            caller,
            params,
        }
    }
    fn publish(state: &Arc<Mutex<DaemonState>>, uid: &str, message: &str) -> Response {
        dispatch_request(
            state,
            &request(
                "notify_user",
                Caller::session(uid),
                serde_json::json!({"message": message}),
            ),
        )
        .into_response()
    }
    fn local(state: &Arc<Mutex<DaemonState>>) {
        state.lock().unwrap().tui_sessions.insert(
            "local".into(),
            serde_json::from_value(serde_json::json!({"uid":"local", "label":"Local task"}))
                .unwrap(),
        );
    }
    fn cloud(state: &Arc<Mutex<DaemonState>>, continuous: bool) {
        let mut params = SpawnParams::new("cloud", "Cloud task", "/bin/sleep");
        params.args = vec!["30".into()];
        params.task_id = Some("task".into());
        if continuous {
            params.continuous_task_id = Some("orchestrator".into());
        }
        state
            .lock()
            .unwrap()
            .sessions
            .insert("cloud".into(), DaemonSession::spawn(params).unwrap());
    }
    #[test]
    fn owner_attention_local_cloud_and_continuous_without_global_permissions() {
        let _guard = crate::test_support::env_lock();
        for continuous in [false, true] {
            let (_dir, state) = fixture();
            local(&state);
            cloud(&state, continuous);
            for uid in ["cloud", "local"] {
                assert!(publish(&state, uid, "Ready for review").ok);
            }
            let alerts = snapshot(&state.lock().unwrap()).unwrap();
            assert_eq!(alerts["local"].label, "Local task");
            assert_eq!(alerts["cloud"].task_id.as_deref(), Some("task"));
            assert_eq!(alerts["cloud"].continuous_task_id.is_some(), continuous);
        }
    }
    #[test]
    fn owner_attention_rejects_unknown_exited_operator_and_spoofed_targets() {
        let _guard = crate::test_support::env_lock();
        let (_dir, state) = fixture();
        local(&state);
        cloud(&state, false);
        assert_eq!(
            publish(&state, "unknown", "x").error.unwrap().code,
            ErrorCode::Unauthorized
        );
        state.lock().unwrap().sessions["cloud"]
            .last_exit
            .set_kernel(crate::session::KernelExitStatus {
                code: Some(0),
                signal: None,
            });
        assert_eq!(
            publish(&state, "cloud", "x").error.unwrap().code,
            ErrorCode::Unauthorized
        );
        for caller in [Caller::operator("forged"), Caller::session("local")] {
            let response = dispatch_request(
                &state,
                &request(
                    "notify_user",
                    caller,
                    serde_json::json!({"message":"x","session_uid":"victim"}),
                ),
            )
            .into_response();
            assert!(!response.ok);
        }
        assert!(!publish(&state, "local", &"x".repeat(4097)).ok);
        assert!(snapshot(&state.lock().unwrap()).unwrap().is_empty());
    }
    #[test]
    fn owner_attention_survives_restart_and_watch_reconnect_without_a_viewer() {
        let _guard = crate::test_support::env_lock();
        let _token = crate::control::operator::test_override::set(Some("owner"));
        let (dir, state) = fixture();
        local(&state);
        let response = publish(&state, "local", "Need a decision");
        assert!(response.ok);
        let id = response.result.unwrap()["alert_id"].clone();
        let mut restored = DaemonState::new();
        restored.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        let restored = Arc::new(Mutex::new(restored));
        for _ in 0..2 {
            let outcome = dispatch_request(
                &restored,
                &request(
                    "manifest.watch",
                    Caller::operator("owner"),
                    serde_json::json!({}),
                ),
            );
            let DispatchOutcome::ManifestWatchStream { handle, .. } = outcome else {
                panic!("watch not established")
            };
            assert_eq!(
                handle.initial_snapshot["owner_attention"]["local"]["id"],
                id
            );
        }
    }
    #[test]
    fn owner_attention_retry_coalesces_and_stale_ack_cannot_clear_newer_alert() {
        let _guard = crate::test_support::env_lock();
        let _token = crate::control::operator::test_override::set(Some("owner"));
        let (_dir, state) = fixture();
        local(&state);
        let first = publish(&state, "local", "first").result.unwrap()["alert_id"].clone();
        assert_eq!(
            publish(&state, "local", "first").result.unwrap()["alert_id"],
            first
        );
        let second = publish(&state, "local", "second").result.unwrap()["alert_id"].clone();
        for caller in [Caller::session("local")] {
            let r = dispatch_request(
                &state,
                &request(
                    "owner_attention.ack",
                    caller,
                    serde_json::json!({"session_uid":"local","alert_id":second}),
                ),
            )
            .into_response();
            assert_eq!(r.error.unwrap().code, ErrorCode::Unauthorized);
        }
        for (id, matched) in [(first, false), (second.clone(), true), (second, false)] {
            let r = dispatch_request(
                &state,
                &request(
                    "owner_attention.ack",
                    Caller::operator("owner"),
                    serde_json::json!({"session_uid":"local","alert_id":id}),
                ),
            )
            .into_response();
            assert_eq!(r.result.unwrap()["acknowledged"], matched);
        }
        assert!(snapshot(&state.lock().unwrap()).unwrap().is_empty());
    }
    #[test]
    fn owner_attention_corrupt_or_unwritable_storage_fails_without_false_success() {
        let (_dir, state) = fixture();
        local(&state);
        let file = path(&state.lock().unwrap());
        std::fs::write(&file, "broken").unwrap();
        assert_eq!(
            publish(&state, "local", "x").error.unwrap().code,
            ErrorCode::Internal
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "broken");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(!publish(&state, "local", "x").ok);
    }
}
