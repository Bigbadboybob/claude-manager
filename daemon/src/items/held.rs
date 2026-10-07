//! `report_done` lists the items the caller still holds (doc/items-board.md §7).

use super::api::Api;
use crate::state::DaemonState;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Best effort: never longer than this, and never an error for report_done.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Add `held_items` (and a hint when any are open) to a successful
/// `report_done` reply, or `held_items_error` when the board is unreachable.
/// Also pokes the heartbeat so the board sees the report promptly.
pub fn attach(state: &Arc<Mutex<DaemonState>>, uid: &str, reply: &mut Value) {
    super::heartbeat::poke();
    let (handle, api_url, api_token) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone())
    };
    let daemon_id = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref().map(|store| store.daemon_id.clone())
    };
    let result = daemon_id
        .ok_or_else(|| "daemon_id_unavailable".to_string())
        .and_then(|daemon_id| {
            let api = Api::from_config(&api_url, &api_token).map_err(|(_, m)| m)?.with_timeout(TIMEOUT);
            api.get("/items", &[("holder_pid", format!("agent:{daemon_id}:{uid}")), ("open", "true".into())])
                .map_err(|(_, m)| m)
        });
    shape(reply, result);
}

pub(crate) fn shape(reply: &mut Value, result: Result<Value, String>) {
    match result {
        Ok(rows) => {
            let held: Vec<Value> = rows
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| json!({"n": r["n"], "board": r["board"], "title": r["title"], "status": r["status"]}))
                .collect();
            if !held.is_empty() {
                reply["held_items_hint"] = json!(
                    "you still hold these items: close each (item_set(n, \"done\", board=...)) \
                     or hand it back (item_set(n, holder=\"none\", board=...))"
                );
            }
            reply["held_items"] = json!(held);
        }
        Err(message) => reply["held_items_error"] = json!(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_items_are_listed_with_a_hint() {
        let mut reply = json!({"ok": true});
        shape(&mut reply, Ok(json!([{"board": "sfd", "n": 14, "title": "fuse", "status": "active", "x": 1}])));
        assert_eq!(reply["held_items"], json!([{"n": 14, "board": "sfd", "title": "fuse", "status": "active"}]));
        assert!(reply["held_items_hint"].as_str().unwrap().contains("holder=\"none\""));
        let mut none = json!({"ok": true});
        shape(&mut none, Ok(json!([])));
        assert_eq!(none["held_items"], json!([]));
        assert!(none.get("held_items_hint").is_none());
    }

    #[test]
    fn failures_never_fail_report_done() {
        let mut reply = json!({"ok": true, "reported": true});
        shape(&mut reply, Err("planning_api_unavailable: timeout".into()));
        assert_eq!(reply["reported"], true);
        assert_eq!(reply["held_items_error"], "planning_api_unavailable: timeout");
    }

    #[test]
    fn attach_is_bounded_when_the_api_hangs() {
        let _env = crate::test_support::env_lock();
        // A listener that accepts and never answers.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let _hold = std::thread::spawn(move || {
            let _conns: Vec<_> = listener.incoming().take(1).collect();
            std::thread::sleep(Duration::from_secs(10));
        });
        let root = tempfile::tempdir().unwrap();
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "tok".into();
        let state = Arc::new(Mutex::new(st));
        crate::messaging::rpc::initialize(&state).unwrap();
        let started = std::time::Instant::now();
        let mut reply = json!({"ok": true});
        attach(&state, "me-uid", &mut reply);
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert!(reply["held_items_error"].is_string());
    }
}
