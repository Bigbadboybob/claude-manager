//! A wake is a durable, coalesced hint to read the inbox. Submission is never
//! confused with a read receipt. A current inbound transcript marker confirms
//! delivery. Native adapters never resend an ambiguous submission.
mod marker;
use super::atomic_replace;
use super::{Store, WakeIntent};
use crate::{session::PtyByteFanout, state::DaemonState};
use marker::{Binding, Evidence, Scan};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
#[derive(Default, Serialize, Deserialize)]
struct Queue {
    batches: Vec<Batch>,
    last_attempt: u64,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Batch {
    wake_id: String,
    ids: Vec<String>,
    status: String,
    attempts: u8,
    attempted_at: u64,
    original: Option<Binding>,
    scan: Scan,
    hook_keys: Vec<String>,
}
fn queue_path(root: &Path, uid: &str) -> PathBuf {
    root.join("_delivery").join(format!("{}.json", digest(uid)))
}
fn load(path: &Path) -> io::Result<Queue> {
    match fs::read(path) {
        Ok(b) => Ok(serde_json::from_slice(&b)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Queue::default()),
        Err(e) => Err(e),
    }
}
fn save(path: &Path, q: &Queue) -> io::Result<()> {
    atomic_replace(path, &serde_json::to_value(q)?)
}
fn pending(status: &str) -> bool {
    matches!(status, "pending" | "deferred" | "deferred_unsupported")
}
pub fn status(root: &Path, uid: &str, event: &str) -> Value {
    let state = match load(&queue_path(root, uid)) {
        Ok(q) => q
            .batches
            .into_iter()
            .find(|b| {
                b.ids.iter().any(|id| {
                    id == event
                        || id
                            .strip_prefix("monitor:")
                            .and_then(|s| s.split_once(':'))
                            .is_some_and(|(_, id)| id == event)
                })
            })
            .map(|b| b.status)
            .unwrap_or_else(|| "pending".into()),
        Err(_) => "state_unavailable".into(),
    };
    json!({"recipient":uid,"status":state})
}
pub fn spawn(state: &Arc<Mutex<DaemonState>>) {
    let weak = Arc::downgrade(state);
    let wake = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging_wake
        .clone();
    thread::spawn(move || loop {
        let ready = wake.0.lock().unwrap_or_else(|p| p.into_inner());
        let (mut ready, _) = wake
            .1
            .wait_timeout_while(ready, Duration::from_secs(2), |v| !*v)
            .unwrap_or_else(|p| p.into_inner());
        *ready = false;
        drop(ready);
        let Some(state) = weak.upgrade() else {
            break;
        };
        tick(&state);
    });
}
pub fn signal(state: &Arc<Mutex<DaemonState>>) {
    let wake = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging_wake
        .clone();
    *wake.0.lock().unwrap_or_else(|p| p.into_inner()) = true;
    wake.1.notify_one();
}
struct Recipient {
    engine: String,
    fanout: Arc<PtyByteFanout>,
    binding: Option<Binding>,
    live: Option<(std::sync::Weak<Mutex<DaemonState>>, String)>,
}
impl Recipient {
    fn current_binding(&self) -> Option<Binding> {
        if let Some((state, uid)) = &self.live {
            let state = state.upgrade()?;
            let state = state.lock().unwrap_or_else(|p| p.into_inner());
            let s = state.sessions.get(uid)?;
            return marker::binding(s.transcript_path.as_deref(), s.generation);
        }
        self.binding.clone()
    }
    fn still_current(&self) -> bool {
        self.live.as_ref().is_none_or(|(state, uid)| {
            state.upgrade().is_some_and(|state| {
                let state = state.lock().unwrap_or_else(|p| p.into_inner());
                state.sessions.get(uid).is_some_and(|s| {
                    Arc::ptr_eq(&s.fanout, &self.fanout)
                        && marker::binding(s.transcript_path.as_deref(), s.generation)
                            == self.binding
                })
            })
        })
    }
}
pub fn tick(state: &Arc<Mutex<DaemonState>>) {
    let (handle, root, draining) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.messaging_root.clone(), s.draining)
    };
    if draining {
        return;
    }
    let gate = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging_delivery
        .clone();
    let (groups, store_root) = {
        let _guard = gate.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut slot) = handle.try_lock() else {
            return;
        };
        let Some(store) = slot.as_mut() else {
            return;
        };
        if store.degraded.is_some() || store.advance_monitors_at(chrono::Utc::now()).is_err() {
            return;
        }
        if let Err(e) = reconcile(&root, store) {
            eprintln!("cm messaging: wake reconciliation: {e}");
            return;
        }
        (store.wake_intents(), store.root.clone())
    };
    for (uid, _) in groups {
        // Give cancellations a boundary between recipients, and re-evaluate
        // eligibility after acquiring it. Never keep the store locked on PTY I/O.
        let _guard = gate.lock().unwrap_or_else(|p| p.into_inner());
        let ids = {
            let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
            let Some(store) = slot.as_mut() else {
                return;
            };
            if store.degraded.is_some() {
                return;
            }
            if let Err(e) = reconcile(&root, store) {
                eprintln!("cm messaging: wake reconciliation: {e}");
                return;
            }
            store.wake_intents().remove(&uid).unwrap_or_default()
        };
        let recipient = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            s.sessions
                .get(&uid)
                .filter(|s| !s.fanout.snapshot_since(None).closed)
                .map(|s| Recipient {
                    engine: s.session_type.clone(),
                    fanout: s.fanout.clone(),
                    binding: marker::binding(s.transcript_path.as_deref(), s.generation),
                    live: Some((Arc::downgrade(state), uid.clone())),
                })
        };
        let Some(recipient) = recipient else {
            continue;
        };
        if let Err(e) = deliver_intents(&root, &store_root, &uid, ids, &recipient) {
            eprintln!("cm messaging: wake for {uid} pending: {e}");
        }
    }
}
fn deliver_intents(
    cm_root: &Path,
    store_root: &Path,
    uid: &str,
    intents: Vec<WakeIntent>,
    r: &Recipient,
) -> io::Result<()> {
    let path = queue_path(store_root, uid);
    let mut q = load(&path)?;
    let covered: BTreeSet<_> = q
        .batches
        .iter()
        .flat_map(|b| b.ids.iter())
        .cloned()
        .collect();
    let new: Vec<_> = intents
        .iter()
        .map(|i| i.key.clone())
        .filter(|id| !covered.contains(id))
        .collect();
    if !new.is_empty() {
        if let Some(batch) = q.batches.iter_mut().find(|b| pending(&b.status)) {
            batch.ids.extend(new);
        } else {
            q.batches.push(Batch {
                wake_id: uuid::Uuid::new_v4().to_string(),
                ids: new,
                status: "pending".into(),
                ..Batch::default()
            });
        }
        save(&path, &q)?;
    }
    for batch in &mut q.batches {
        let id = format!("chat:{}", batch.wake_id);
        if let Some(event) = crate::notifications::get(cm_root, uid, &id)? {
            batch.status = native_status(&event).into();
            continue;
        }
        // Old Stop/PTY attempts must never be replayed through the new adapter.
        if !pending(&batch.status) {
            if matches!(
                batch.status.as_str(),
                "idle_attempt_pending" | "hook_attempt_pending" | "hook_pending"
            ) {
                let inbox = cm_root
                    .join("inbox")
                    .join(uid)
                    .join(format!("chat-{}.json", batch.wake_id));
                // Compete with the legacy hook's claim. A successful rename is
                // proof it was not consumed; otherwise leave delivery uncertain.
                match fs::rename(&inbox, inbox.with_extension("json.retired")) {
                    Ok(()) => batch.status = "pending".into(),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        batch.status = "uncertain".into()
                    }
                    Err(e) => return Err(e),
                }
            }
            if !pending(&batch.status) {
                if matches!(batch.status.as_str(), "submitted_unverified" | "uncertain") {
                    let current = r.current_binding();
                    if marker::inspect(
                        &mut batch.scan,
                        batch.original.as_ref(),
                        current.as_ref(),
                        &r.engine,
                        &batch.wake_id,
                    ) == Evidence::Found
                    {
                        batch.status = "confirmed".into();
                    }
                }
                continue;
            }
        }
        if !["claude-code", "codex"].contains(&r.engine.as_str()) {
            batch.status = "deferred_unsupported".into();
            continue;
        }
        if !r.still_current() {
            continue;
        }
        let text = wake_text(batch, &intents);
        let event = crate::notifications::publish(
            cm_root,
            uid,
            &id,
            "chat",
            &text,
            &format!("[cm-chat {}]", batch.wake_id),
        )?;
        batch.status = native_status(&event).into();
    }
    save(&path, &q)
}
fn native_status(event: &Value) -> &str {
    match event["status"].as_str() {
        Some("observed") => "confirmed",
        Some("submitted") => "submitted_unverified",
        Some("pending") => "native_pending",
        Some("submitting") => "submitting",
        Some("cancelled") => "cancelled",
        _ => "uncertain",
    }
}

fn wake_text(batch: &Batch, intents: &[WakeIntent]) -> String {
    let monitors: BTreeSet<_> = intents
        .iter()
        .filter(|n| batch.ids.contains(&n.key))
        .filter_map(|n| n.monitor.as_deref())
        .collect();
    let hint = if monitors.is_empty() {
        String::new()
    } else {
        format!(" Monitor results: {}. Use chat_monitors(action=list) for all watches, then get their results.",monitors.into_iter().take(8).collect::<Vec<_>>().join(", "))
    };
    format!("[cm-chat {}] New chat activity. Use chat_read(inbox=true, unread_only=true) for messages.{} Automated CM notification, not Owner input or message content.",batch.wake_id,hint)
}
pub fn monitor_status(root: &Path, uid: &str, monitor: &str) -> Value {
    let q = match load(&queue_path(root, uid)) {
        Ok(q) => q,
        Err(_) => return json!({"state_unavailable":true}),
    };
    let mut statuses: BTreeMap<String, usize> = BTreeMap::new();
    for batch in q.batches.iter().filter(|b| {
        b.ids
            .iter()
            .any(|id| id.starts_with(&format!("monitor:{monitor}:")))
    }) {
        *statuses.entry(batch.status.clone()).or_default() += 1;
    }
    json!(statuses)
}

/// Must run under the delivery gate, before committing a cancellation reply.
/// The native queue lock serializes retraction with adapter claims.
pub fn reconcile(cm_root: &Path, store: &Store) -> io::Result<()> {
    for (uid, intents) in store.wake_intents() {
        let path = queue_path(&store.root, &uid);
        let mut q = load(&path)?;
        let before = serde_json::to_value(&q)?;
        let eligible: BTreeSet<_> = intents.iter().map(|i| i.key.as_str()).collect();
        for batch in &mut q.batches {
            let active = batch.ids.iter().any(|id| eligible.contains(id.as_str()));
            let id = format!("chat:{}", batch.wake_id);
            let text = active.then(|| wake_text(batch, &intents));
            if let Some(event) =
                crate::notifications::update_pending(cm_root, &uid, &id, text.as_deref())?
            {
                batch.status = native_status(&event).into();
            } else if !active && pending(&batch.status) {
                batch.status = "cancelled".into();
            } else if !active
                && matches!(
                    batch.status.as_str(),
                    "hook_pending" | "hook_attempt_pending"
                )
            {
                let inbox = cm_root
                    .join("inbox")
                    .join(&uid)
                    .join(format!("chat-{}.json", batch.wake_id));
                let claim = inbox.with_extension("json.retired");
                match fs::rename(&inbox, &claim) {
                    Ok(()) => {
                        fs::remove_file(&claim)?;
                        fs::File::open(inbox.parent().unwrap())?.sync_all()?;
                        batch.status = "cancelled".into();
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        batch.status = "submitted_no_retry".into()
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        if serde_json::to_value(&q)? != before {
            save(&path, &q)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn recipient(engine: &str) -> Recipient {
        Recipient {
            engine: engine.into(),
            fanout: Arc::new(PtyByteFanout::new(1024)),
            binding: None,
            live: None,
        }
    }
    #[test]
    fn messaging_native_batches_survive_restart_without_a_pty_or_duplicate() {
        let t = tempfile::tempdir().unwrap();
        let intents = vec![WakeIntent {
            key: "one".into(),
            event_id: "one".into(),
            monitor: None,
        }];
        for engine in ["claude-code", "codex"] {
            let r = recipient(engine);
            deliver_intents(t.path(), t.path(), engine, intents.clone(), &r).unwrap();
            deliver_intents(t.path(), t.path(), engine, intents.clone(), &r).unwrap();
            let q = load(&queue_path(t.path(), engine)).unwrap();
            assert_eq!(q.batches.len(), 1);
            assert_eq!(q.batches[0].status, "native_pending");
            let event = crate::notifications::get(
                t.path(),
                engine,
                &format!("chat:{}", q.batches[0].wake_id),
            )
            .unwrap()
            .unwrap();
            assert_eq!(event["recipient"], engine);
            assert!(!t.path().join("inbox").exists());
        }
    }
    #[test]
    fn messaging_cancel_retracts_native_hint_without_erasing_monitor_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path()).unwrap();
        let actor = store.participant_id("b");
        let m = store
            .register_monitor(
                &actor,
                &json!({"scope":{"channel":"general"},"request_id":"watch"}),
                &[],
            )
            .unwrap();
        let other = store
            .register_monitor(
                &actor,
                &json!({"scope":{"channel":"general"},"request_id":"other-watch"}),
                &[],
            )
            .unwrap();
        store
            .send(
                "owner",
                "",
                "owner",
                &json!({"channel":"general","body":"Work ready","request_id":"message"}),
                &[],
            )
            .unwrap();
        deliver_intents(
            tmp.path(),
            &store.root,
            "b",
            store.wake_intents()["b"].clone(),
            &recipient("claude-code"),
        )
        .unwrap();
        let q = load(&queue_path(&store.root, "b")).unwrap();
        let id = format!("chat:{}", q.batches[0].wake_id);
        store
            .monitors(
                &actor,
                &json!({"action":"cancel","monitor_id":m["id"],"request_id":"cancel"}),
            )
            .unwrap();
        reconcile(tmp.path(), &store).unwrap();
        let event = crate::notifications::get(tmp.path(), "b", &id)
            .unwrap()
            .unwrap();
        assert!(!event["text"]
            .as_str()
            .unwrap()
            .contains(m["id"].as_str().unwrap()));
        assert!(event["text"]
            .as_str()
            .unwrap()
            .contains(other["id"].as_str().unwrap()));
        store
            .monitors(
                &actor,
                &json!({"action":"cancel","monitor_id":other["id"],"request_id":"cancel-other"}),
            )
            .unwrap();
        reconcile(tmp.path(), &store).unwrap();
        assert_eq!(
            crate::notifications::get(tmp.path(), "b", &id)
                .unwrap()
                .unwrap()["status"],
            "cancelled"
        );
        assert_eq!(
            store
                .monitors(&actor, &json!({"action":"get","monitor_id":m["id"]}))
                .unwrap()["monitor"]["hits"],
            1
        );
    }
    #[test]
    fn messaging_legacy_ambiguous_submission_is_never_replayed() {
        let t = tempfile::tempdir().unwrap();
        let path = queue_path(t.path(), "a");
        save(
            &path,
            &Queue {
                batches: vec![Batch {
                    wake_id: "test".into(),
                    ids: vec!["one".into()],
                    status: "idle_attempt_pending".into(),
                    ..Batch::default()
                }],
                ..Queue::default()
            },
        )
        .unwrap();
        deliver_intents(t.path(), t.path(), "a", vec![], &recipient("codex")).unwrap();
        assert_eq!(load(&path).unwrap().batches[0].status, "uncertain");
        assert!(crate::notifications::get(t.path(), "a", "chat:test")
            .unwrap()
            .is_none());
    }
}
