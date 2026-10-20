//! Delivered Owner alerts over the authenticated messaging peer connection.
//! Relay disk work never holds the daemon registry or messaging store locks.
//! Seen is durable at the hub before hiding an exact (origin, queue key, ID).
use crate::{
    control::protocol::{ErrorCode, Request, Response},
    owner_attention::{self, Alert},
    state::DaemonState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAX_SNAPSHOT: usize = 600 * 1024;
const MAX_HOSTS: usize = 16;
// Every origin can retain its full snapshot and a Seen map no larger than
// that snapshot, plus the local source snapshot and JSON envelope overhead.
const MAX_BYTES: usize = (2 * MAX_HOSTS + 1) * MAX_SNAPSHOT + 64 * 1024;
pub(crate) const RESYNC_REQUIRED: &str = "Owner alert snapshot needs a new generation";
type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    version: u32,
    pub generation: String,
    pub revision: u64,
    pub host_label: String,
    pub alerts: BTreeMap<String, Alert>,
}
impl Snapshot {
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || uuid::Uuid::parse_str(&self.generation).is_err()
            || self.revision == 0
            || self.host_label.is_empty()
            || self.host_label.len() > 256
            || self.host_label.chars().any(char::is_control)
            || self.alerts.len() > 4096
            || serde_json::to_vec(self).map_err(|e| e.to_string())?.len() > MAX_SNAPSHOT
            || self.alerts.iter().any(|(k, a)| {
                k.is_empty()
                    || k.len() > 1024
                    || a.id.is_empty()
                    || a.id.len() > 1024
                    || a.session_uid.len() > 1024
                    || a.label.len() > 4096
                    || a.message.len() > 4096
                    || a.created_at
                        .as_ref()
                        .is_some_and(|t| chrono::DateTime::parse_from_rfc3339(t).is_err())
            })
        {
            return Err("Invalid Owner alert snapshot".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Seen {
    pub key: String,
    pub id: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Remote {
    snapshot: Snapshot,
    // Retain while that exact ID remains in the source snapshot, including
    // equal-revision retries. A newer ID never matches or inherits a Seen.
    pending: BTreeMap<String, String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Disk {
    version: u32,
    source: Option<Snapshot>,
    origins: BTreeMap<String, Remote>,
}
impl Default for Disk {
    fn default() -> Self {
        Self {
            version: 1,
            source: None,
            origins: BTreeMap::new(),
        }
    }
}
#[derive(Default)]
pub struct Relay {
    inner: Mutex<Option<(PathBuf, Disk)>>,
}
impl Relay {
    fn load<'a>(&self, slot: &'a mut Option<(PathBuf, Disk)>, root: &Path) -> Result<&'a mut Disk> {
        if slot.is_none() {
            let disk = match std::fs::File::open(root.join("owner-alert-relay.json")) {
                Ok(f) => {
                    let mut bytes = Vec::new();
                    f.take(MAX_BYTES as u64 + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    if bytes.len() > MAX_BYTES {
                        return Err("Owner alert relay checkpoint is oversized".into());
                    }
                    let d: Disk = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
                    if d.version != 1 || d.origins.len() > MAX_HOSTS {
                        return Err("Invalid Owner alert relay checkpoint".into());
                    }
                    if let Some(s) = &d.source {
                        s.validate()?;
                    }
                    for (host, r) in &d.origins {
                        uuid::Uuid::parse_str(host).map_err(|e| e.to_string())?;
                        r.snapshot.validate()?;
                        if r.pending.len() > 4096
                            || r.pending
                                .iter()
                                .any(|(k, id)| r.snapshot.alerts.get(k).is_none_or(|a| &a.id != id))
                        {
                            return Err("Invalid pending Owner alert acknowledgement".into());
                        }
                    }
                    d
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Disk::default(),
                Err(e) => return Err(e.to_string()),
            };
            *slot = Some((root.into(), disk));
        }
        let (path, d) = slot.as_mut().unwrap();
        if path != root {
            return Err("Owner alert relay root changed".into());
        }
        Ok(d)
    }
    fn save(root: &Path, d: &Disk) -> Result<()> {
        let bytes = serde_json::to_string(d).map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BYTES {
            return Err("Owner alert relay checkpoint full; previous state retained".into());
        }
        crate::state::write_json_atomic(&root.join("owner-alert-relay.json"), &bytes, true)
            .map_err(|e| e.to_string())
    }
    pub(crate) fn source(
        &self,
        root: &Path,
        host_label: String,
        mut alerts: BTreeMap<String, Alert>,
    ) -> Result<Snapshot> {
        for alert in alerts.values_mut() {
            // Legacy local queues can contain labels predating the source cap.
            if alert.label.len() > 4096 {
                let mut end = 4096;
                while !alert.label.is_char_boundary(end) {
                    end -= 1;
                }
                alert.label.truncate(end);
            }
        }
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let disk = self.load(&mut guard, root)?;
        if let Some(s) = disk
            .source
            .as_ref()
            .filter(|s| s.host_label == host_label && s.alerts == alerts)
        {
            return Ok(s.clone());
        }
        let snapshot = Snapshot {
            version: 1,
            generation: disk
                .source
                .as_ref()
                .map(|s| s.generation.clone())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            revision: disk
                .source
                .as_ref()
                .map_or(Some(1), |s| s.revision.checked_add(1))
                .ok_or("Alert revision exhausted")?,
            host_label,
            alerts,
        };
        snapshot.validate()?;
        let mut next = disk.clone();
        next.source = Some(snapshot.clone());
        Self::save(root, &next)?;
        *disk = next;
        Ok(snapshot)
    }
    pub(crate) fn remint(&self, root: &Path, expected: &str) -> Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let disk = self.load(&mut guard, root)?;
        if disk
            .source
            .as_ref()
            .is_none_or(|s| s.generation != expected)
        {
            return Ok(());
        }
        let mut next = disk.clone();
        let source = next.source.as_mut().unwrap();
        source.generation = uuid::Uuid::new_v4().to_string();
        source.revision = 1;
        Self::save(root, &next)?;
        *disk = next;
        Ok(())
    }
    pub(crate) fn origins(&self, root: &Path) -> Result<Vec<String>> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(self
            .load(&mut guard, root)?
            .origins
            .keys()
            .cloned()
            .collect())
    }
    pub(crate) fn prune(
        &self,
        root: &Path,
        revoked: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let disk = self.load(&mut guard, root)?;
        if !disk.origins.keys().any(|k| revoked.contains(k)) {
            return Ok(());
        }
        let mut next = disk.clone();
        next.origins.retain(|k, _| !revoked.contains(k));
        Self::save(root, &next)?;
        *disk = next;
        Ok(())
    }
    /// Caller authenticates origin independently; it is never taken from wire data.
    pub(crate) fn ingest(
        &self,
        root: &Path,
        origin: &str,
        snapshot: Snapshot,
    ) -> Result<Vec<Alert>> {
        uuid::Uuid::parse_str(origin).map_err(|e| e.to_string())?;
        snapshot.validate()?;
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let disk = self.load(&mut guard, root)?;
        let old = disk.origins.get(origin);
        if let Some(old) = old.filter(|r| r.snapshot.generation == snapshot.generation) {
            if old.snapshot.revision > snapshot.revision {
                return Err(RESYNC_REQUIRED.into());
            }
            if old.snapshot.revision == snapshot.revision {
                if old.snapshot != snapshot {
                    return Err(RESYNC_REQUIRED.into());
                }
                return Ok(Vec::new());
            }
        }
        if old.is_none() && disk.origins.len() >= MAX_HOSTS {
            return Err("Owner alert relay host limit reached".into());
        }
        let mut pending = old.map(|r| r.pending.clone()).unwrap_or_default();
        pending.retain(|key, id| snapshot.alerts.get(key).is_some_and(|a| a.id == *id));
        let added = snapshot
            .alerts
            .iter()
            .filter(|(k, a)| {
                pending.get(*k) != Some(&a.id)
                    && (old.is_some()
                        || a.created_at
                            .as_deref()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .is_some_and(|t| t.timestamp() >= chrono::Utc::now().timestamp() - 900))
                    && old.is_none_or(|r| {
                        r.snapshot.alerts.get(*k).is_none_or(|prev| prev.id != a.id)
                    })
            })
            .map(|(_, a)| a.clone())
            .collect();
        let mut next = disk.clone();
        next.origins
            .insert(origin.into(), Remote { snapshot, pending });
        Self::save(root, &next)?;
        *disk = next;
        Ok(added)
    }
    pub(crate) fn pending(&self, root: &Path, origin: &str) -> Result<Vec<Seen>> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(self
            .load(&mut guard, root)?
            .origins
            .get(origin)
            .map(|r| {
                r.pending
                    .iter()
                    .map(|(k, id)| Seen {
                        key: k.clone(),
                        id: id.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }
    fn seen(&self, root: &Path, origin: &str, key: &str, id: &str) -> Result<bool> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let disk = self.load(&mut guard, root)?;
        let Some(remote) = disk
            .origins
            .get(origin)
            .filter(|r| r.snapshot.alerts.get(key).is_some_and(|a| a.id == id))
        else {
            return Ok(false);
        };
        if remote.pending.get(key).is_some_and(|prev| prev == id) {
            return Ok(true);
        }
        let mut next = disk.clone();
        next.origins
            .get_mut(origin)
            .unwrap()
            .pending
            .insert(key.into(), id.into());
        Self::save(root, &next)?;
        *disk = next;
        Ok(true)
    }
    fn remotes(&self, root: &Path) -> Result<BTreeMap<String, Remote>> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(self.load(&mut guard, root)?.origins.clone())
    }
}

// Always namespace web keys, even local ones; the existing TUI still sends its
// raw local queue key. JSON + URL-safe base64 makes arbitrary keys unambiguous.
fn wire_key(host: &str, key: &str) -> String {
    use base64::Engine;
    format!(
        "cm-alert-v1:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&(host, key)).unwrap())
    )
}
fn parse_key(key: &str) -> Result<(String, String)> {
    use base64::Engine;
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(
            key.strip_prefix("cm-alert-v1:")
                .ok_or("Invalid relay key")?,
        )
        .map_err(|e| e.to_string())?;
    let (host, key): (String, String) = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
    uuid::Uuid::parse_str(&host).map_err(|e| e.to_string())?;
    if key.is_empty() || key.len() > 1024 {
        return Err("Invalid alert key".into());
    }
    Ok((host, key))
}
fn local_id(root: &Path) -> Result<String> {
    let s = std::fs::read_to_string(root.join("daemon-id")).map_err(|e| e.to_string())?;
    let s = s.trim();
    uuid::Uuid::parse_str(s).map_err(|e| e.to_string())?;
    Ok(s.into())
}
fn row(host: &str, label: &str, key: &str, alert: Alert) -> Value {
    let mut v = serde_json::to_value(alert).unwrap();
    v["key"] = json!(wire_key(host, key));
    v["origin_key"] = json!(key);
    v["origin_host"] = json!(host);
    v["host"] = json!(label);
    v
}
pub fn list(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let result = (|| -> Result<Value> {
        let (root, relay, sync, path) = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            (
                s.messaging_root.clone(),
                s.owner_alert_relay.clone(),
                s.messaging_sync.clone(),
                owner_attention::path(&s),
            )
        };
        let local = owner_attention::snapshot_at(&path)?;
        let id = local_id(&root)?;
        let label = crate::items::heartbeat::host_label().unwrap_or_else(|| id.clone());
        let mut items: Vec<Value> = local
            .into_iter()
            .map(|(k, a)| row(&id, &label, &k, a))
            .collect();
        let mut hosts = vec![json!({"id":id,"label":label,"local":true,"connected":true})];
        if let Some(sync) = sync {
            sync.prune_owner_alerts(&relay).map_err(|e| e.to_string())?;
            let remotes = relay.remotes(&root)?;
            let allowed = sync.owner_alert_hosts(remotes.keys());
            for (host, r) in remotes
                .into_iter()
                .filter(|(host, _)| allowed.contains(host))
            {
                hosts.push(json!({"id":host,"label":r.snapshot.host_label,"local":false,"connected":sync.peer_connected(&host)}));
                items.extend(
                    r.snapshot
                        .alerts
                        .into_iter()
                        .filter(|(key, a)| r.pending.get(key) != Some(&a.id))
                        .map(|(k, a)| row(&host, &r.snapshot.host_label, &k, a)),
                );
            }
        }
        items.sort_by(|a, b| {
            a["created_at"]
                .as_str()
                .unwrap_or("")
                .cmp(b["created_at"].as_str().unwrap_or(""))
                .then_with(|| a["key"].as_str().cmp(&b["key"].as_str()))
        });
        Ok(
            json!({"items":items,"seen_supported":true,"scope":if hosts.len()>1 {"hub"} else {"local"},"alert_hosts":hosts}),
        )
    })();
    match result {
        Ok(v) => Response::ok(req.id.clone(), v),
        Err(e) => Response::err(req.id.clone(), ErrorCode::Internal, e),
    }
}
pub fn acknowledge(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    let Some(key) = req.params["session_uid"].as_str() else {
        return Response::err(
            req.id.clone(),
            ErrorCode::InvalidParams,
            "Missing alert key",
        );
    };
    if !key.starts_with("cm-alert-v1:") {
        return owner_attention::acknowledge(&state.lock().unwrap_or_else(|p| p.into_inner()), req);
    }
    let (host, key) = match parse_key(key) {
        Ok(k) => k,
        Err(e) => return Response::err(req.id.clone(), ErrorCode::InvalidParams, e),
    };
    let Some(id) = req.params["alert_id"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 1024)
    else {
        return Response::err(req.id.clone(), ErrorCode::InvalidParams, "Invalid alert ID");
    };
    let (root, relay, sync) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging_root.clone(),
            s.owner_alert_relay.clone(),
            s.messaging_sync.clone(),
        )
    };
    if local_id(&root).is_ok_and(|id| id == host) {
        let local = Request {
            id: req.id.clone(),
            method: req.method.clone(),
            caller: req.caller.clone(),
            params: json!({"session_uid":key,"alert_id":id}),
        };
        return owner_attention::acknowledge(
            &state.lock().unwrap_or_else(|p| p.into_inner()),
            &local,
        );
    }
    let Some(sync) = sync.filter(|s| s.owner_alert_hosts([&host]).contains(&host)) else {
        return Response::err(
            req.id.clone(),
            ErrorCode::Unauthorized,
            "Alert origin is not an enrolled Owner host",
        );
    };
    match relay.seen(&root, &host, &key, id) {
        Ok(matched) => {
            sync.signal_owner_alerts();
            Response::ok(
                req.id.clone(),
                json!({"ok":true,"acknowledged":matched,"pending_origin":matched}),
            )
        }
        Err(e) => Response::err(req.id.clone(), ErrorCode::Internal, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn alert(id: &str) -> Alert {
        Alert {
            id: id.into(),
            session_uid: "agent".into(),
            label: "lane".into(),
            message: "review".into(),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        }
    }
    #[test]
    fn owner_relay_durable_seen_survives_restart_and_stale_snapshots_without_clearing_new_id() {
        let source = tempfile::tempdir().unwrap();
        let hub = tempfile::tempdir().unwrap();
        let host = uuid::Uuid::new_v4().to_string();
        let relay = Relay::default();
        let cache = Relay::default();
        let first = relay
            .source(
                source.path(),
                "sessions".into(),
                BTreeMap::from([("key".into(), alert("a"))]),
            )
            .unwrap();
        assert_eq!(
            cache
                .ingest(hub.path(), &host, first.clone())
                .unwrap()
                .len(),
            1
        );
        assert!(!cache.seen(hub.path(), &host, "key", "stale").unwrap());
        assert!(cache.seen(hub.path(), &host, "key", "a").unwrap());
        drop(cache);
        let cache = Relay::default();
        assert_eq!(
            cache.pending(hub.path(), &host).unwrap(),
            vec![Seen {
                key: "key".into(),
                id: "a".into()
            }]
        );
        assert!(cache
            .ingest(hub.path(), &host, first.clone())
            .unwrap()
            .is_empty());
        assert_eq!(cache.pending(hub.path(), &host).unwrap().len(), 1);
        drop(relay);
        let relay = Relay::default();
        let second = relay
            .source(
                source.path(),
                "sessions".into(),
                BTreeMap::from([("key".into(), alert("b"))]),
            )
            .unwrap();
        assert_eq!(second.generation, first.generation);
        assert_eq!(second.revision, first.revision + 1);
        assert_eq!(
            cache
                .ingest(hub.path(), &host, second.clone())
                .unwrap()
                .len(),
            1
        );
        assert!(cache.pending(hub.path(), &host).unwrap().is_empty());
        assert_eq!(
            cache.ingest(hub.path(), &host, first).unwrap_err(),
            RESYNC_REQUIRED
        );
        assert!(!cache.seen(hub.path(), &host, "key", "a").unwrap());
        assert_eq!(
            cache.remotes(hub.path()).unwrap()[&host].snapshot.alerts["key"].id,
            "b"
        );
        let mut conflict = second.clone();
        conflict.alerts.clear();
        assert!(cache.ingest(hub.path(), &host, conflict).is_err());
        assert!(cache.seen(hub.path(), &host, "key", "b").unwrap());
        let empty = relay
            .source(source.path(), "sessions".into(), BTreeMap::new())
            .unwrap();
        cache.ingest(hub.path(), &host, empty).unwrap();
        assert!(Relay::default()
            .pending(hub.path(), &host)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn owner_relay_keys_bounds_and_failed_persistence_preserve_prior_state() {
        let source = tempfile::tempdir().unwrap();
        let hub = tempfile::tempdir().unwrap();
        let host = uuid::Uuid::new_v4().to_string();
        let key = "cm-alert-v1:odd/雪#&:";
        assert_eq!(
            parse_key(&wire_key(&host, key)).unwrap(),
            (host.clone(), key.into())
        );
        assert_ne!(
            wire_key(&host, key),
            wire_key(&uuid::Uuid::new_v4().to_string(), key)
        );
        assert!(parse_key("cm-alert-v1:garbage").is_err());
        let snapshot = Relay::default()
            .source(
                source.path(),
                "sessions".into(),
                BTreeMap::from([(key.into(), alert("a"))]),
            )
            .unwrap();
        let cache = Relay::default();
        cache.ingest(hub.path(), &host, snapshot.clone()).unwrap();
        let mut bad = snapshot.clone();
        bad.revision += 1;
        bad.alerts.get_mut(key).unwrap().message = "x".repeat(4097);
        assert!(cache.ingest(hub.path(), &host, bad).is_err());
        let mut foreign = serde_json::to_value(&snapshot).unwrap();
        foreign["origin_host"] = json!(host);
        assert!(serde_json::from_value::<Snapshot>(foreign).is_err());
        std::fs::remove_file(hub.path().join("owner-alert-relay.json")).unwrap();
        std::fs::create_dir(hub.path().join("owner-alert-relay.json")).unwrap();
        assert!(cache.seen(hub.path(), &host, key, "a").is_err());
        assert!(cache.pending(hub.path(), &host).unwrap().is_empty());
    }
}

#[cfg(test)]
mod review_tests {
    use super::*;
    fn alerts() -> BTreeMap<String, Alert> {
        [
            ("fresh", Some(chrono::Utc::now().to_rfc3339())),
            (
                "old",
                Some((chrono::Utc::now() - chrono::Duration::minutes(16)).to_rfc3339()),
            ),
            ("legacy", None),
        ]
        .into_iter()
        .map(|(id, created_at)| {
            (
                id.into(),
                Alert {
                    id: id.into(),
                    label: "雪".repeat(2000),
                    message: "Review".into(),
                    created_at,
                    ..Default::default()
                },
            )
        })
        .collect()
    }
    #[test]
    fn owner_relay_source_label_cap_initial_push_age_and_generation_resync() {
        let source = tempfile::tempdir().unwrap();
        let hub = tempfile::tempdir().unwrap();
        let host = uuid::Uuid::new_v4().to_string();
        let relay = Relay::default();
        let cache = Relay::default();
        let snapshot = relay
            .source(source.path(), "sessions".into(), alerts())
            .unwrap();
        assert_eq!(snapshot.alerts["fresh"].label.len(), 4095);
        let pushed = cache.ingest(hub.path(), &host, snapshot.clone()).unwrap();
        assert_eq!(
            pushed.iter().map(|a| a.id.as_str()).collect::<Vec<_>>(),
            vec!["fresh"]
        );
        assert_eq!(
            cache.remotes(hub.path()).unwrap()[&host]
                .snapshot
                .alerts
                .len(),
            3
        );
        assert!(cache.seen(hub.path(), &host, "fresh", "fresh").unwrap());
        let mut rollback = snapshot.clone();
        rollback.alerts.remove("old");
        assert_eq!(
            cache.ingest(hub.path(), &host, rollback).unwrap_err(),
            RESYNC_REQUIRED
        );
        relay.remint(source.path(), &snapshot.generation).unwrap();
        let reset = relay
            .source(source.path(), "sessions".into(), alerts())
            .unwrap();
        assert_ne!(reset.generation, snapshot.generation);
        // A delayed request for the old generation must not mint another one.
        relay.remint(source.path(), &snapshot.generation).unwrap();
        assert_eq!(
            relay
                .source(source.path(), "sessions".into(), reset.alerts.clone())
                .unwrap()
                .generation,
            reset.generation
        );
        assert!(cache
            .ingest(hub.path(), &host, reset.clone())
            .unwrap()
            .is_empty());
        assert_eq!(cache.pending(hub.path(), &host).unwrap().len(), 1);
        drop(relay);
        assert_eq!(
            Relay::default()
                .source(source.path(), "sessions".into(), reset.alerts)
                .unwrap()
                .generation,
            reset.generation
        );
    }
    #[test]
    fn owner_relay_capacity_covers_all_origins_and_pruning_preserves_other_hosts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut disk = Disk::default();
        let big = (0..140)
            .map(|i| {
                let id = format!("id-{i}");
                (
                    id.clone(),
                    Alert {
                        id,
                        label: "Agent".into(),
                        message: "x".repeat(4000),
                        ..Default::default()
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let snapshot = Snapshot {
            version: 1,
            generation: uuid::Uuid::new_v4().to_string(),
            revision: 1,
            host_label: "sessions".into(),
            alerts: big,
        };
        snapshot.validate().unwrap();
        let pending = snapshot
            .alerts
            .iter()
            .map(|(k, a)| (k.clone(), a.id.clone()))
            .collect::<BTreeMap<_, _>>();
        let ids = (0..MAX_HOSTS)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect::<Vec<_>>();
        for host in &ids {
            disk.origins.insert(
                host.clone(),
                Remote {
                    snapshot: snapshot.clone(),
                    pending: pending.clone(),
                },
            );
        }
        disk.source = Some(snapshot.clone());
        assert!(serde_json::to_vec(&disk).unwrap().len() > 8 * 1024 * 1024);
        Relay::save(tmp.path(), &disk).unwrap();
        let cache = Relay::default();
        assert_eq!(cache.origins(tmp.path()).unwrap().len(), MAX_HOSTS);
        cache
            .prune(
                tmp.path(),
                &std::collections::BTreeSet::from([ids[0].clone()]),
            )
            .unwrap();
        assert_eq!(cache.origins(tmp.path()).unwrap().len(), MAX_HOSTS - 1);
        cache
            .ingest(tmp.path(), &uuid::Uuid::new_v4().to_string(), snapshot)
            .unwrap();
        assert_eq!(
            Relay::default().origins(tmp.path()).unwrap().len(),
            MAX_HOSTS
        );
    }
}
