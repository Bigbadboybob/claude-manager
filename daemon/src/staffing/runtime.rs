use super::*;
use crate::control::protocol::{Caller, ErrorCode, Request, Response};
use crate::items::api::Api;
use crate::state::DaemonState;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type State = Arc<Mutex<DaemonState>>;
type Result<T> = std::result::Result<T, (ErrorCode, String)>;
const RECENT_DAYS: i64 = 3;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Cache {
    pub snapshot: Snapshot,
    spawners: BTreeMap<String, Spawner>,
    #[serde(skip)]
    path: Option<PathBuf>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Spawner {
    at: DateTime<Utc>,
    notified_version: u64,
}
impl Cache {
    fn load(root: &Path) -> Self {
        let path = root.join("staffing.json");
        let mut cache: Self = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .filter(|c| c.snapshot.policy.validate().is_ok())
            .unwrap_or_default();
        cache.path = Some(path);
        cache
    }
    fn save(&self) {
        if let Some(path) = &self.path {
            if let Err(e) =
                crate::state::write_json_atomic(path, &serde_json::to_string(self).unwrap(), true)
            {
                eprintln!("cm staffing: cache save failed: {e}");
            }
        }
    }
    fn apply(&mut self, snapshot: Snapshot) -> Result<()> {
        snapshot
            .policy
            .validate()
            .map_err(|e| (ErrorCode::InvalidParams, e))?;
        if snapshot.version > self.snapshot.version {
            self.snapshot = snapshot;
            self.save();
        }
        Ok(())
    }
    fn record(&mut self, uid: &str, used_version: u64, now: DateTime<Utc>) {
        self.spawners
            .retain(|_, s| s.at >= now - chrono::Duration::days(RECENT_DAYS));
        // The policy may refresh while the child starts. Record what this
        // caller saw, without undoing a newer concurrent spawn or notice.
        let notified_version = self
            .spawners
            .get(uid)
            .map_or(used_version, |s| s.notified_version.max(used_version));
        self.spawners.insert(
            uid.into(),
            Spawner {
                at: now,
                notified_version,
            },
        );
        self.save();
    }
    fn pending(&self, now: DateTime<Utc>) -> Vec<String> {
        self.spawners
            .iter()
            .filter(|(_, s)| {
                s.at >= now - chrono::Duration::days(RECENT_DAYS)
                    && s.notified_version < self.snapshot.version
            })
            .map(|(uid, _)| uid.clone())
            .collect()
    }
}

fn cache(state: &State) -> Arc<Mutex<Cache>> {
    state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .staffing
        .clone()
}
fn api(state: &State) -> Result<Api> {
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    Api::from_config(&s.config.api_url, &s.config.api_token)
}
pub fn exposure(state: &State) -> Value {
    let handle = cache(state);
    let cache = handle.lock().unwrap_or_else(|p| p.into_inner());
    let snapshot = &cache.snapshot;
    json!({"version":snapshot.version,"policy":snapshot.policy,"updated_at":snapshot.updated_at,
        "digest":snapshot.policy.digest(Utc::now()),
        "recommended_engine":snapshot.policy.recommend(None,Utc::now()).0,
        "source":if snapshot.version==0 {"fallback"} else {"cache"}})
}
pub fn record_spawn(state: &State, caller_uid: Option<&str>, used_version: u64) {
    if let Some(uid) = caller_uid {
        let handle = cache(state);
        let mut cache = handle.lock().unwrap_or_else(|p| p.into_inner());
        cache.record(uid, used_version, Utc::now());
    }
}
fn refresh(state: &State) -> Result<()> {
    let response = api(state)?.get("/staffing", &[])?;
    let snapshot = serde_json::from_value(response).map_err(|e| {
        (
            ErrorCode::Internal,
            format!("invalid staffing response: {e}"),
        )
    })?;
    cache(state)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .apply(snapshot)
}
fn notify(state: &State) {
    let (root, live) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging_root.clone(),
            s.sessions
                .keys()
                .chain(s.tui_sessions.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
        )
    };
    let handle = cache(state);
    let (version, text, targets) = {
        let cache = handle.lock().unwrap_or_else(|p| p.into_inner());
        (
            cache.snapshot.version,
            format!(
                "[cm-staffing] Policy v{}: {} Check ping().staffing before your next launch.",
                cache.snapshot.version,
                cache.snapshot.policy.digest(Utc::now())
            ),
            cache.pending(Utc::now()),
        )
    };
    let id = format!("staffing-policy-{version}");
    // Publishing may touch disk: neither daemon state nor selection cache is
    // locked while the native notification producer runs.
    let mut delivered = Vec::new();
    for uid in targets.into_iter().filter(|u| live.contains(u)) {
        if crate::notifications::get(&root, &uid, &id)
            .ok()
            .flatten()
            .is_some()
            || crate::notifications::publish(&root, &uid, &id, "cm", &text, "[cm-staffing]").is_ok()
        {
            delivered.push(uid);
        }
    }
    if !delivered.is_empty() {
        let mut cache = handle.lock().unwrap_or_else(|p| p.into_inner());
        for uid in delivered {
            if let Some(spawner) = cache.spawners.get_mut(&uid) {
                spawner.notified_version = spawner.notified_version.max(version);
            }
        }
        cache.save();
    }
}

pub fn start(state: &State) {
    {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        *s.staffing.lock().unwrap_or_else(|p| p.into_inner()) = Cache::load(&s.messaging_root);
    }
    let weak = Arc::downgrade(state);
    if let Err(e) = std::thread::Builder::new()
        .name("cm-staffing".into())
        .spawn(move || loop {
            let Some(state) = weak.upgrade() else { return };
            // Failure retains the last policy, including across brain restarts.
            let _ = refresh(&state);
            notify(&state);
            drop(state);
            std::thread::sleep(Duration::from_secs(30));
        })
    {
        eprintln!("cm staffing: refresh worker unavailable: {e}");
    }
}

pub(crate) fn dispatch(state: &State, req: &Request) -> Response {
    let result = (|| -> Result<Value> {
        if !req.params.is_null() && !req.params.is_object() {
            return Err((
                ErrorCode::InvalidParams,
                "staffing parameters must be an object".into(),
            ));
        }
        let owner = matches!(req.caller, Caller::Operator(_));
        if owner {
            crate::control::operator::validate_operator(&req.caller)
                .map_err(|e| (ErrorCode::Unauthorized, e.to_string()))?;
        } else if req.method == "staffing.set" || req.params["refresh"] == true {
            return Err((
                ErrorCode::Unauthorized,
                "Only Owner can edit or force-refresh staffing policy".into(),
            ));
        }
        match req.method.as_str() {
            "staffing.get" => {
                if req.params["refresh"] == true {
                    refresh(state)?;
                }
            }
            "staffing.set" => {
                let mut body = req.params.clone();
                body["actor"] = json!("owner");
                let policy: Policy = serde_json::from_value(body["policy"].clone())
                    .map_err(|e| (ErrorCode::InvalidParams, e.to_string()))?;
                policy
                    .validate()
                    .map_err(|e| (ErrorCode::InvalidParams, e))?;
                let snapshot: Snapshot =
                    serde_json::from_value(api(state)?.put("/staffing", &body)?)
                        .map_err(|e| (ErrorCode::Internal, e.to_string()))?;
                cache(state)
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .apply(snapshot)?;
                notify(state);
            }
            _ => return Err((ErrorCode::UnknownMethod, "Unknown staffing method".into())),
        }
        Ok(exposure(state))
    })();
    match result {
        Ok(value) => Response::ok(req.id.clone(), value),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staffing_cache_restart_monotonic_and_recent_spawners() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut cache = Cache::load(root.path());
        cache.record("old", 0, now - chrono::Duration::days(4));
        cache.record("recent", 0, now - chrono::Duration::days(2));
        cache
            .apply(Snapshot {
                version: 2,
                policy: Policy::default(),
                updated_at: Some(now),
            })
            .unwrap();
        assert_eq!(cache.pending(now), ["recent"]);
        cache
            .apply(Snapshot {
                version: 1,
                ..Snapshot::default()
            })
            .unwrap();
        let restored = Cache::load(root.path());
        assert_eq!(restored.snapshot.version, 2);
        assert_eq!(restored.pending(now), ["recent"]);
        cache.record("in-flight", 1, now);
        assert!(cache.pending(now).contains(&"in-flight".into()));
        cache.record("in-flight", 2, now);
        cache.record("in-flight", 1, now);
        assert!(!cache.pending(now).contains(&"in-flight".into()));
    }
    #[test]
    fn staffing_notice_is_durable_and_only_for_recent_spawners() {
        let root = tempfile::tempdir().unwrap();
        let mut s = DaemonState::default();
        s.messaging_root = root.path().into();
        s.staffing = Arc::new(Mutex::new(Cache::load(root.path())));
        for uid in ["recent", "old"] {
            s.tui_sessions.insert(
                uid.into(),
                serde_json::from_value(json!({"uid":uid,"label":uid})).unwrap(),
            );
        }
        let state = Arc::new(Mutex::new(s));
        let handle = cache(&state);
        {
            let mut c = handle.lock().unwrap();
            c.record("old", 0, Utc::now() - chrono::Duration::days(4));
            c.record("recent", 0, Utc::now() - chrono::Duration::days(2));
            c.apply(Snapshot {
                version: 3,
                policy: Policy::default(),
                updated_at: None,
            })
            .unwrap();
        }
        notify(&state);
        assert!(
            crate::notifications::get(root.path(), "recent", "staffing-policy-3")
                .unwrap()
                .is_some()
        );
        assert!(
            crate::notifications::get(root.path(), "old", "staffing-policy-3")
                .unwrap()
                .is_none()
        );
        assert!(Cache::load(root.path()).pending(Utc::now()).is_empty());
        notify(&state); // a second pass or restart does not enqueue again
        assert_eq!(
            handle.lock().unwrap().spawners["recent"].notified_version,
            3
        );
    }

    #[test]
    fn staffing_global_agent_cannot_edit() {
        let state = Arc::new(Mutex::new(DaemonState::default()));
        let req = Request {
            id: "edit".into(),
            method: "staffing.set".into(),
            caller: Caller::session("global-agent"),
            params: json!({"actor":"owner","policy":Policy::default(),"expected_version":0}),
        };
        assert!(dispatch(&state, &req).error.is_some());
        assert_eq!(exposure(&state)["version"], 0);
    }
}
