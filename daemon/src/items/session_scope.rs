//! Fail-static, complete org snapshot for session control.
use crate::state::DaemonState;
use cm_holder_proto::session_scope::{session_key, Snapshot};
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static REFRESH_REQUESTED: AtomicBool = AtomicBool::new(false);
static REFRESH_GATE: OnceLock<Mutex<()>> = OnceLock::new();

pub(crate) fn request_refresh() { REFRESH_REQUESTED.store(true, Ordering::Release); }

#[derive(Clone, Default)]
pub struct Cache {
    pub graph: Snapshot,
    pub daemon_id: String,
    pub version: Option<u64>,
}

impl Cache {
    pub fn role(&self, uid: &str) -> Option<cm_holder_proto::session_scope::Role> {
        self.graph.sessions.get(&session_key(&self.daemon_id, uid)).map(|s| s.role)
    }

    pub fn reaches(&self, caller: &str, target: &str) -> bool {
        self.graph.reaches(&session_key(&self.daemon_id, caller), &session_key(&self.daemon_id, target))
    }

    /// Spawner links belong to this daemon and remain live during API outages.
    /// Member, manager and role facts always come from the retained snapshot.
    pub fn reaches_local(&self, state: &DaemonState, caller: &str, target: &str) -> bool {
        let prefix = format!("agent:{}:", self.daemon_id);
        self.graph.reaches_with_spawners(&session_key(&self.daemon_id, caller),
            &session_key(&self.daemon_id, target), |key| {
                let uid = key.strip_prefix(&prefix)?;
                if let Some(session) = state.sessions.get(uid) {
                    return session.managed_by_uid.as_deref().map(|parent| session_key(&self.daemon_id, parent));
                }
                if let Some(tomb) = state.exited_tombstone(uid) {
                    return tomb.managed_by_uid.as_deref().map(|parent| session_key(&self.daemon_id, parent));
                }
                // Viewer-owned sessions do not have a daemon-local spawner
                // field; use their last reported snapshot link.
                state.tui_sessions.contains_key(uid).then(|| self.graph.sessions.get(key)
                    .and_then(|fact| fact.spawned_by.clone())).flatten()
            })
    }
}

/// Obtain a complete snapshot outside the state lock; only an intact reply
/// replaces the previous cache. Session UIDs identify the same CM session
/// across revive and rotation.
pub fn refresh(state: &Arc<Mutex<DaemonState>>) -> bool {
    // Session facts can change without an org version bump. Serialize full
    // reads so an older in-flight reply cannot replace a newer snapshot.
    let _refresh = REFRESH_GATE.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|p| p.into_inner());
    let (url, token, messaging, root) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.config.api_url.clone(), s.config.api_token.clone(), s.messaging.clone(), s.messaging_root.clone())
    };
    let daemon_id = {
        let slot = messaging.lock().unwrap_or_else(|p| p.into_inner());
        match slot.as_ref() { Some(store) => store.daemon_id.clone(), None => return false }
    };
    let Ok(api_identity) = crate::planning_client::resolve_api_url(Some(&url)) else { return false };
    // A brain restart during an outage must preserve the last complete
    // snapshot, but never import a different daemon's saved identities.
    let path = root.join("session-scope-cache.json");
    if state.lock().unwrap_or_else(|p| p.into_inner()).session_scope_cache.version.is_none() {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(saved) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                if saved["daemon_id"].as_str() == Some(&daemon_id)
                    && saved["api_url"].as_str() == Some(api_identity.as_str()) {
                    if let (Some(graph), Some(version)) = (Snapshot::from_org_snapshot(&saved["snapshot"]), saved["snapshot"]["version"].as_u64()) {
                        let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
                        if s.session_scope_cache.version.is_none() {
                            s.session_scope_cache = Cache { graph, daemon_id: daemon_id.clone(), version: Some(version) };
                        }
                    }
                }
            }
        }
    }
    let Ok(api) = super::api::Api::from_config(&url, &token) else { return false };
    let Ok(value) = api.get("/org/snapshot", &[]) else { return false };
    if let Some(members) = value["members"].as_array() {
        let now = crate::agent_state::unix_now();
        for fact in members.iter().filter_map(|m| m["assistants"].as_array()).flatten() {
            if fact["daemon_id"].as_str() != Some(daemon_id.as_str()) { continue; }
            let Some(uid) = fact["session_uid"].as_str() else { continue };
            let Some(local_age) = super::org::spawned_age_s(uid, now) else { continue };
            let Some(reported) = fact["spawned_at"].as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()) else { continue };
            let actual = now - local_age;
            let recorded = reported.timestamp_millis() as f64 / 1000.0;
            if (actual - recorded).abs() > 2.0 {
                eprintln!("cm-daemon: org spawn time mismatch session={uid:?} uid_time={actual} snapshot_time={recorded}");
            }
        }
    }
    let Some(graph) = Snapshot::from_org_snapshot(&value) else { return false };
    let Some(version) = value.get("version").and_then(serde_json::Value::as_u64) else { return false };
    let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
    if s.config.api_url != url || s.config.api_token != token
        || s.session_scope_cache.version.is_some_and(|current| current > version) { return false; }
    s.session_scope_cache = Cache { graph, daemon_id: daemon_id.clone(), version: Some(version) };
    drop(s);
    let saved = serde_json::json!({"daemon_id": daemon_id, "api_url": api_identity, "snapshot": value});
    if let Ok(text) = serde_json::to_string(&saved) {
        if let Err(e) = crate::state::write_json_atomic(&path, &text, true) {
            eprintln!("cm-daemon: could not persist session scope cache: {e}");
        }
    }
    true
}

/// Poll the ordered org-change feed; refresh on any observed event, and at
/// least every five minutes even if membership facts change without an event.
pub fn start(state: &Arc<Mutex<DaemonState>>) {
    let state = Arc::clone(state);
    let result = std::thread::Builder::new().name("session-scope-refresh".into()).spawn(move || {
        let mut last: Option<Instant> = None;
        loop {
            if (REFRESH_REQUESTED.swap(false, Ordering::AcqRel)
                || last.is_none_or(|at| at.elapsed() >= Duration::from_secs(300)))
                && refresh(&state) { last = Some(Instant::now()); }
            let (url, token, version) = {
                let s = state.lock().unwrap_or_else(|p| p.into_inner());
                (s.config.api_url.clone(), s.config.api_token.clone(), s.session_scope_cache.version)
            };
            if let (Ok(api), Some(version)) = (super::api::Api::from_config(&url, &token), version) {
                if api.get("/org/changes", &[("since", version.to_string())]).ok()
                    .and_then(|v| v.get("version").and_then(serde_json::Value::as_u64))
                    .is_some_and(|new| new > version) && refresh(&state) {
                    last = Some(Instant::now());
                }
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    if let Err(e) = result { eprintln!("cm items: session scope refresh thread failed: {e}"); }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn api_outage_preserves_last_complete_snapshot() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let down = Arc::new(AtomicBool::new(false));
        let response = Arc::new(Mutex::new(serde_json::Value::Null));
        let (flag, value) = (down.clone(), response.clone());
        let (url, _) = super::super::org::test_stub(move |_, path, _| {
            if path != "/org/snapshot" || flag.load(Ordering::SeqCst) {
                (503, "{}".into())
            } else {
                (200, value.lock().unwrap().to_string())
            }
        });
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "test".into();
        for uid in ["main", "worker"] {
            let mut p = crate::session::SpawnParams::new(uid, uid, "/bin/sleep");
            p.args = vec!["3".into()];
            st.sessions.insert(uid.into(), crate::session::DaemonSession::spawn(p).unwrap());
        }
        let state = Arc::new(Mutex::new(st));
        crate::messaging::startup::open_for_test(&state).unwrap();
        let handle = state.lock().unwrap().messaging.clone();
        let daemon_id = handle.lock().unwrap().as_ref().unwrap().daemon_id.clone();
        *response.lock().unwrap() = json!({"version": 1, "members": [
            {"id":"lead", "org_id":"org", "status":"active", "manager_member_id":null,
             "assistants":[{"daemon_id":daemon_id,"session_uid":"main","acting_as":"main","spawned_by":null}]},
            {"id":"report", "org_id":"org", "status":"active", "manager_member_id":"lead",
             "assistants":[{"daemon_id":daemon_id,"session_uid":"worker","acting_as":"worker","spawned_by":null}]}
        ]});
        assert!(refresh(&state));
        assert!(state.lock().unwrap().session_scope_cache.reaches("main", "worker"));
        down.store(true, Ordering::SeqCst);
        assert!(!refresh(&state));
        let mut st = state.lock().unwrap();
        assert!(st.session_scope_cache.reaches("main", "worker"));
        assert!(!st.session_scope_cache.reaches("main", "new-worker"));
        let mut p = crate::session::SpawnParams::new("new-worker", "new-worker", "/bin/sleep");
        p.args = vec!["3".into()];
        p.managed_by_uid = Some("main".into());
        st.sessions.insert("new-worker".into(), crate::session::DaemonSession::spawn(p).unwrap());
        assert!(crate::control::auth::check_session_caller(&st, "main", "new-worker").is_allow());
    }
}
