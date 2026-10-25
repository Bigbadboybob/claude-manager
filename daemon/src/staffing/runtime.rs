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
const FULL_CACHE_FILE: &str = "staffing-v2.json";
const LEGACY_INDEFINITE_EXPIRY: &str = "9999-12-31T23:59:59Z";

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
    fn read(path: &Path) -> Option<Self> {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Self>(&b).ok())
            .filter(|c| c.snapshot.policy.validate().is_ok())
    }
    fn load(root: &Path) -> Self {
        let path = root.join("staffing.json");
        let legacy = Self::read(&path);
        let mut cache = Self::read(&root.join(FULL_CACHE_FILE))
            .unwrap_or_else(|| legacy.clone().unwrap_or_default());
        if let Some(legacy) = legacy {
            // An old brain may have refreshed policy or recorded spawners
            // during rollback. Prefer the full policy on equal versions.
            if legacy.snapshot.version > cache.snapshot.version {
                cache.snapshot = legacy.snapshot;
            }
            for (uid, spawner) in legacy.spawners {
                cache
                    .spawners
                    .entry(uid)
                    .and_modify(|s| {
                        s.at = s.at.max(spawner.at);
                        s.notified_version = s.notified_version.max(spawner.notified_version);
                    })
                    .or_insert(spawner);
            }
        }
        cache.path = Some(path);
        // Also migrate caches written by a pre-dual-file build, even if the
        // API is offline or still serves the same policy version.
        cache.save();
        cache
    }
    fn save(&self) {
        if let Some(path) = &self.path {
            let mut legacy = self.clone();
            legacy
                .snapshot
                .policy
                .rules
                .retain(|kind, _| kind != "busywork");
            for rule in legacy.snapshot.policy.rules.values_mut() {
                *rule = Rule::Engine(rule.engine());
            }
            if let Some(preference) = legacy.snapshot.policy.preference.as_mut() {
                if preference.expires_at.is_none() {
                    preference.expires_at = Some(LEGACY_INDEFINITE_EXPIRY.parse().unwrap());
                }
            }
            // Write rollback protection first. Both files use atomic renames;
            // load reconciles versions if a crash interrupts the pair.
            for (path, cache) in [
                (path.clone(), &legacy),
                (path.with_file_name(FULL_CACHE_FILE), self),
            ] {
                if let Err(e) = crate::state::write_json_atomic(
                    &path,
                    &serde_json::to_string(cache).unwrap(),
                    true,
                ) {
                    eprintln!("cm staffing: cache save failed ({}): {e}", path.display());
                }
            }
        }
    }
    fn apply(&mut self, snapshot: Snapshot) -> Result<()> {
        snapshot
            .policy
            .validate()
            .map_err(|e| (ErrorCode::InvalidParams, e))?;
        // An equal-version API response restores fields omitted by a legacy
        // cache after rollback or an interrupted dual-file save.
        if snapshot.version >= self.snapshot.version && snapshot != self.snapshot {
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

    // Frozen a9ffdcb cache contract: deliberately do not use today's Policy
    // or Preference, which accept the new wire shapes.
    #[derive(Serialize, Deserialize)]
    struct LegacyCache {
        snapshot: LegacySnapshot,
        spawners: BTreeMap<String, Spawner>,
    }
    #[derive(Serialize, Deserialize)]
    struct LegacySnapshot {
        version: u64,
        policy: LegacyPolicy,
        #[serde(default)]
        updated_at: Option<DateTime<Utc>>,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyPolicy {
        default_engine: Engine,
        rules: BTreeMap<String, Engine>,
        preference: Option<LegacyPreference>,
    }
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct LegacyPreference {
        engine: Engine,
        strength: Strength,
        reason: String,
        expires_at: DateTime<Utc>,
    }
    impl LegacyCache {
        fn load(root: &Path) -> Self {
            let legacy: Self =
                serde_json::from_slice(&std::fs::read(root.join("staffing.json")).unwrap())
                    .unwrap();
            assert!(legacy.snapshot.policy.rules.keys().all(|k| {
                [
                    "math",
                    "design",
                    "architecture",
                    "metric",
                    "coding",
                    "fanout",
                ]
                .contains(&k.as_str())
            }));
            if let Some(p) = &legacy.snapshot.policy.preference {
                assert!(!p.reason.trim().is_empty());
                assert!(p.reason.chars().count() <= 500);
                assert!(!p.reason.chars().any(char::is_control));
            }
            legacy
        }
        fn save(&self, root: &Path) {
            std::fs::write(
                root.join("staffing.json"),
                serde_json::to_vec(self).unwrap(),
            )
            .unwrap();
        }
    }
    fn indefinite_snapshot(now: DateTime<Utc>) -> Snapshot {
        let mut policy = Policy::default();
        policy.preference = Some(Preference {
            engine: Engine::Codex,
            strength: Strength::Strong,
            reason: "limits".into(),
            expires_at: None,
        });
        policy.rules.insert(
            "coding".into(),
            Rule::new(Engine::Codex, Some("gpt-6.1-sol".into())),
        );
        Snapshot {
            version: 4,
            policy,
            updated_at: Some(now),
        }
    }
    #[test]
    fn staffing_cache_legacy_parser_preserves_preference_and_engine_rules() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let snapshot = indefinite_snapshot(now);
        let mut cache = Cache::load(root.path());
        cache.apply(snapshot.clone()).unwrap();
        cache.record("recent", 4, now);
        let legacy = LegacyCache::load(root.path());
        assert_eq!(legacy.snapshot.version, 4);
        assert_eq!(legacy.snapshot.updated_at, Some(now));
        assert_eq!(legacy.snapshot.policy.default_engine, Engine::Codex);
        assert_eq!(legacy.snapshot.policy.rules["coding"], Engine::Codex);
        assert_eq!(legacy.snapshot.policy.rules["fanout"], Engine::Claude);
        assert!(!legacy.snapshot.policy.rules.contains_key("busywork"));
        let pref = legacy.snapshot.policy.preference.unwrap();
        assert_eq!(pref.engine, Engine::Codex);
        assert_eq!(pref.strength, Strength::Strong);
        assert!(pref.expires_at > now + chrono::Duration::days(36500));
        assert_eq!(legacy.spawners["recent"].notified_version, 4);
        assert_eq!(Cache::load(root.path()).snapshot, snapshot);

        // Real dated expiries must not be extended by the legacy projection.
        let mut dated = snapshot;
        dated.version += 1;
        dated.policy.preference.as_mut().unwrap().expires_at = Some(now);
        cache.apply(dated.clone()).unwrap();
        assert_eq!(
            LegacyCache::load(root.path())
                .snapshot
                .policy
                .preference
                .unwrap()
                .expires_at,
            now
        );
        assert_eq!(Cache::load(root.path()).snapshot, dated);
    }
    #[test]
    fn staffing_cache_migrates_existing_new_shape_and_recovers_missing_full_cache() {
        let root = tempfile::tempdir().unwrap();
        let snapshot = indefinite_snapshot(Utc::now());
        let old_path = root.path().join("staffing.json");
        std::fs::write(
            &old_path,
            serde_json::to_vec(&Cache {
                snapshot: snapshot.clone(),
                ..Cache::default()
            })
            .unwrap(),
        )
        .unwrap();
        assert!(serde_json::from_slice::<LegacyCache>(&std::fs::read(&old_path).unwrap()).is_err());
        assert_eq!(Cache::load(root.path()).snapshot, snapshot);
        LegacyCache::load(root.path()); // load repaired rollback compatibility
        assert_eq!(Cache::load(root.path()).snapshot, snapshot);

        for invalid in [
            None,
            Some("{broken"),
            Some(
                r#"{"snapshot":{"version":99,"policy":{"default_engine":"codex","rules":{"unknown":"codex"},"preference":null}},"spawners":{}}"#,
            ),
        ] {
            let full_path = root.path().join(FULL_CACHE_FILE);
            if let Some(invalid) = invalid {
                std::fs::write(&full_path, invalid).unwrap();
            } else {
                std::fs::remove_file(&full_path).unwrap();
            }
            let restored = Cache::load(root.path());
            assert_eq!(restored.snapshot.version, 4);
            assert!(restored
                .snapshot
                .policy
                .active_preference(Utc::now())
                .is_some());
            // The API can rehydrate full fields at the same version, even
            // after another offline restart persisted the legacy fallback.
            let mut restored = Cache::load(root.path());
            restored.apply(snapshot.clone()).unwrap();
            assert_eq!(Cache::load(root.path()).snapshot, snapshot);
        }
    }
    #[test]
    fn staffing_cache_preserves_changes_made_during_rollback() {
        let root = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let snapshot = indefinite_snapshot(now);
        let mut cache = Cache::load(root.path());
        cache.apply(snapshot.clone()).unwrap();
        cache.record("recent", 3, now);
        let mut legacy = LegacyCache::load(root.path());
        legacy.spawners.get_mut("recent").unwrap().notified_version = 4;
        legacy.spawners.insert(
            "during-rollback".into(),
            Spawner {
                at: now,
                notified_version: 4,
            },
        );
        legacy.save(root.path());
        let restored = Cache::load(root.path());
        assert_eq!(restored.snapshot, snapshot); // same version keeps full fields
        assert!(restored.pending(now).is_empty()); // no duplicate notices
        assert!(restored.spawners.contains_key("during-rollback"));

        legacy.snapshot.version = 5;
        legacy.snapshot.policy.preference = None;
        legacy.save(root.path());
        let restored = Cache::load(root.path());
        assert_eq!(restored.snapshot.version, 5); // newer legacy policy wins
        assert!(restored.snapshot.policy.preference.is_none());
        assert_eq!(restored.pending(now).len(), 2);
    }
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
