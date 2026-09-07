//! A wake is a durable, coalesced hint to read the inbox. Submission is never
//! confused with a read receipt. A current inbound transcript marker confirms
//! delivery; one retry requires complete negative evidence and fresh safety gates.
mod marker;
use super::atomic_replace;
use super::{Store, WakeIntent};
use crate::{
    session::{InputHandle, PtyByteFanout, SharedLastActivity},
    state::DaemonState,
};
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
    time::{Duration, Instant},
};
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(2));
        let Some(state) = weak.upgrade() else {
            break;
        };
        tick(&state);
    });
}
struct Recipient {
    engine: String,
    idle: Option<bool>,
    input: InputHandle,
    turn_end: SharedLastActivity,
    fanout: Arc<PtyByteFanout>,
    expected_input: Option<Instant>,
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
                    idle: s.semantic_idle(),
                    input: s.input_handle(),
                    turn_end: s.last_turn_end_at.clone(),
                    fanout: s.fanout.clone(),
                    expected_input: *s.last_input_at.lock().unwrap_or_else(|p| p.into_inner()),
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
    for i in 0..q.batches.len() {
        let inbox = cm_root
            .join("inbox")
            .join(uid)
            .join(format!("chat-{}.json", q.batches[i].wake_id));
        let claim = inbox.with_extension("json.idle-claimed");
        let prior = q.batches[i].status.clone();
        if pending(&prior) && claim.exists() {
            fs::remove_file(&claim)?;
            fs::File::open(claim.parent().unwrap())?.sync_all()?;
        }
        if matches!(
            prior.as_str(),
            "idle_attempt_pending" | "hook_attempt_pending"
        ) {
            // A process stopped during submission. A visible hook file proves
            // publication, but disappearance never proves delivery or failure.
            q.batches[i].status = if prior == "hook_attempt_pending" && inbox.exists() {
                "hook_pending"
            } else {
                "uncertain"
            }
            .into();
            save(&path, &q)?;
        }
        let mut retry = false;
        if matches!(
            q.batches[i].status.as_str(),
            "submitted_unverified" | "uncertain"
        ) {
            let current = r.current_binding();
            let batch = &mut q.batches[i];
            let evidence = marker::inspect(
                &mut batch.scan,
                batch.original.as_ref(),
                current.as_ref(),
                &r.engine,
                &batch.wake_id,
            );
            if evidence == Evidence::Found && r.current_binding() == current && r.still_current() {
                batch.status = "confirmed".into();
            } else if evidence == Evidence::Absent
                && r.current_binding() == current
                && batch.attempts == 1
                && seconds().saturating_sub(batch.attempted_at) >= 45
                && r.idle == Some(true)
            {
                retry = true;
            }
            save(&path, &q)?;
            if !retry {
                continue;
            }
        }
        let mut reclaimed = false;
        if q.batches[i].status == "hook_pending" {
            if !inbox.exists() && !claim.exists() {
                q.batches[i].status = "submitted_unverified".into();
                save(&path, &q)?;
                continue;
            }
            if r.idle != Some(true) {
                continue;
            }
            // Compete with the hook's rename, never read-then-delete its file.
            if !claim.exists() {
                match fs::rename(&inbox, &claim) {
                    Ok(()) => fs::File::open(inbox.parent().unwrap())?.sync_all()?,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e),
                }
            }
            reclaimed = true;
        } else if !retry && !pending(&q.batches[i].status) {
            continue;
        }
        if !reclaimed && seconds().saturating_sub(q.last_attempt) < 30 {
            continue;
        }
        let text = wake_text(&q.batches[i], &intents);
        if !["claude-code", "codex"].contains(&r.engine.as_str()) {
            if q.batches[i].status != "deferred_unsupported" {
                q.batches[i].status = "deferred_unsupported".into();
                save(&path, &q)?;
            }
            continue;
        }
        if !r.still_current() {
            continue;
        }
        if r.engine == "claude-code" && r.idle != Some(true) {
            q.batches[i].original = r.current_binding();
            q.batches[i].attempts = 1;
            q.batches[i].attempted_at = seconds();
            q.batches[i].hook_keys = intents
                .iter()
                .filter(|n| q.batches[i].ids.contains(&n.key))
                .map(|n| n.key.clone())
                .collect();
            q.batches[i].status = "hook_attempt_pending".into();
            q.last_attempt = seconds();
            save(&path, &q)?;
            match atomic_replace(&inbox, &json!({"text":text})) {
                Ok(()) => q.batches[i].status = "hook_pending".into(),
                Err(e) => {
                    q.batches[i].status = "uncertain".into();
                    save(&path, &q)?;
                    return Err(e);
                }
            }
            save(&path, &q)?;
            break;
        }
        if r.idle != Some(true) {
            if q.batches[i].status != "deferred" {
                q.batches[i].status = "deferred".into();
                save(&path, &q)?;
            }
            continue;
        }
        // Persist the attempt before touching a PTY. A crash cannot grant an
        // extra retry. A definitely deferred adapter call restores the count.
        let before = q.batches[i].attempts;
        if !reclaimed {
            q.batches[i].attempts += 1;
        }
        if q.batches[i].original.is_none() {
            q.batches[i].original = r.current_binding();
        }
        q.batches[i].attempted_at = seconds();
        q.batches[i].status = "idle_attempt_pending".into();
        save(&path, &q)?;
        match r
            .input
            .try_chat_prompt(&text, r.expected_input, &r.turn_end, &r.fanout)
        {
            Ok(false) => {
                q.batches[i].attempts = before;
                q.batches[i].status = if retry {
                    "submitted_unverified"
                } else {
                    "deferred"
                }
                .into();
            }
            Ok(true) => {
                q.batches[i].status = "submitted_unverified".into();
                q.last_attempt = seconds();
            }
            Err(_) => {
                q.batches[i].status = "uncertain".into();
                q.last_attempt = seconds();
            }
        }
        save(&path, &q)?;
        if reclaimed {
            fs::remove_file(&claim)?;
        }
        if q.batches[i].status != "deferred" {
            break;
        }
    }
    Ok(())
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
    format!("[cm-chat {}] New chat activity. Use chat_read(inbox=true, unread_only=true) for messages.{} This is a notification, not message content.",batch.wake_id,hint)
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
/// A hook consumer competes with rename; a lost claim means it was submitted.
pub fn reconcile(cm_root: &Path, store: &Store) -> io::Result<()> {
    for (uid, intents) in store.wake_intents() {
        let path = queue_path(&store.root, &uid);
        let mut q = load(&path)?;
        let eligible: BTreeSet<_> = intents.iter().map(|i| i.key.as_str()).collect();
        let mut changed = false;
        for batch in &mut q.batches {
            let active: Vec<_> = batch
                .ids
                .iter()
                .filter(|id| eligible.contains(id.as_str()))
                .cloned()
                .collect();
            if !active.is_empty() {
                let previous = if batch.hook_keys.is_empty() {
                    &batch.ids
                } else {
                    &batch.hook_keys
                };
                if matches!(
                    batch.status.as_str(),
                    "hook_pending" | "hook_attempt_pending"
                ) && &active != previous
                {
                    let inbox = cm_root
                        .join("inbox")
                        .join(&uid)
                        .join(format!("chat-{}.json", batch.wake_id));
                    let claim = inbox.with_extension("json.revised");
                    match fs::rename(&inbox, &claim) {
                        Ok(()) => {
                            // Only a won claim permits replacing a hint. Never
                            // resurrect a file already consumed by the hook.
                            batch.hook_keys = active;
                            atomic_replace(&inbox, &json!({"text":wake_text(batch,&intents)}))?;
                            fs::remove_file(&claim)?;
                            fs::File::open(inbox.parent().unwrap())?.sync_all()?;
                            batch.status = "hook_pending".into();
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            batch.status = "submitted_unverified".into()
                        }
                        Err(e) => return Err(e),
                    }
                    changed = true;
                }
                continue;
            }
            if batch.status == "hook_pending" || batch.status == "hook_attempt_pending" {
                let inbox = cm_root
                    .join("inbox")
                    .join(&uid)
                    .join(format!("chat-{}.json", batch.wake_id));
                let claim = inbox.with_extension("json.cancelled");
                match fs::rename(&inbox, &claim) {
                    Ok(()) => {
                        fs::remove_file(&claim)?;
                        fs::File::open(inbox.parent().unwrap())?.sync_all()?;
                        batch.status = "cancelled".into();
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        batch.status = "submitted_unverified".into();
                    }
                    Err(e) => return Err(e),
                }
                changed = true;
            } else if pending(&batch.status)
                || matches!(
                    batch.status.as_str(),
                    "submitted_unverified" | "uncertain" | "idle_attempt_pending"
                )
            {
                // Submitted hints cannot be taken back; they must not retry.
                batch.status = if pending(&batch.status) {
                    "cancelled"
                } else {
                    "submitted_no_retry"
                }
                .into();
                changed = true;
            }
        }
        if changed {
            save(&path, &q)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn deliver(
        cm: &Path,
        store: &Path,
        uid: &str,
        ids: Vec<String>,
        r: &Recipient,
    ) -> io::Result<()> {
        deliver_intents(
            cm,
            store,
            uid,
            ids.into_iter()
                .map(|id| WakeIntent {
                    key: id.clone(),
                    event_id: id,
                    monitor: None,
                })
                .collect(),
            r,
        )
    }
    fn recipient(engine: &str, idle: Option<bool>) -> Recipient {
        Recipient {
            engine: engine.into(),
            idle,
            input: InputHandle::test_handle(),
            turn_end: Arc::new(Mutex::new(Some(Instant::now()))),
            fanout: Arc::new(PtyByteFanout::new(1024)),
            expected_input: None,
            binding: None,
            live: None,
        }
    }
    #[test]
    fn messaging_verified_retry_is_once_with_same_marker_and_fresh_safety() {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("live.jsonl");
        fs::write(&transcript, "{}\n").unwrap();
        let mut r = recipient("claude-code", Some(true));
        r.binding = marker::binding(transcript.to_str(), 1);
        r.fanout.push(b"\x1b[?2004h");
        let path = queue_path(tmp.path(), "a");
        let q = Queue {
            batches: vec![Batch {
                wake_id: "same-wake".into(),
                ids: vec!["one".into()],
                status: "submitted_unverified".into(),
                attempts: 1,
                attempted_at: seconds() - 46,
                original: r.binding.clone(),
                ..Batch::default()
            }],
            last_attempt: 0,
        };
        save(&path, &q).unwrap();
        // No operator-idle timeout can bypass the draft gate.
        r.input
            .stamp_operator_input_at(Instant::now() - Duration::from_secs(600));
        deliver(tmp.path(), tmp.path(), "a", vec!["one".into()], &r).unwrap();
        assert_eq!(load(&path).unwrap().batches[0].attempts, 1);
        r.input = InputHandle::test_handle();
        let mut q = load(&path).unwrap();
        q.batches[0].attempted_at = seconds() - 46;
        save(&path, &q).unwrap();
        deliver(tmp.path(), tmp.path(), "a", vec!["one".into()], &r).unwrap();
        let mut q = load(&path).unwrap();
        assert_eq!(q.batches[0].attempts, 2);
        assert_eq!(q.batches[0].wake_id, "same-wake");
        assert_eq!(q.batches[0].status, "submitted_unverified");
        q.batches[0].attempted_at = seconds() - 46;
        save(&path, &q).unwrap();
        deliver(tmp.path(), tmp.path(), "a", vec!["one".into()], &r).unwrap();
        assert_eq!(load(&path).unwrap().batches[0].attempts, 2);
        fs::write(&transcript,"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"[cm-chat same-wake]\"}}\n").unwrap();
        let mut q = load(&path).unwrap();
        q.batches[0].scan = Scan::default();
        save(&path, &q).unwrap();
        deliver(tmp.path(), tmp.path(), "a", vec!["one".into()], &r).unwrap();
        assert_eq!(load(&path).unwrap().batches[0].status, "confirmed");
    }
    #[test]
    fn messaging_cancel_retracts_hook_without_erasing_monitor_hit() {
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
        let r = recipient("claude-code", Some(false));
        deliver_intents(
            tmp.path(),
            &store.root,
            "b",
            store.wake_intents()["b"].clone(),
            &r,
        )
        .unwrap();
        assert_eq!(fs::read_dir(tmp.path().join("inbox/b")).unwrap().count(), 1);
        store
            .monitors(
                &actor,
                &json!({"action":"cancel","monitor_id":m["id"],"request_id":"cancel"}),
            )
            .unwrap();
        reconcile(tmp.path(), &store).unwrap();
        let files: Vec<_> = fs::read_dir(tmp.path().join("inbox/b"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1);
        let body = fs::read_to_string(&files[0]).unwrap();
        assert!(!body.contains(m["id"].as_str().unwrap()));
        assert!(body.contains(other["id"].as_str().unwrap()));
        store
            .monitors(
                &actor,
                &json!({"action":"cancel","monitor_id":other["id"],"request_id":"cancel-other"}),
            )
            .unwrap();
        reconcile(tmp.path(), &store).unwrap();
        assert_eq!(fs::read_dir(tmp.path().join("inbox/b")).unwrap().count(), 0);
        assert_eq!(
            store
                .monitors(&actor, &json!({"action":"get","monitor_id":m["id"]}))
                .unwrap()["monitor"]["hits"],
            1
        );
    }
    #[test]
    fn messaging_hook_coalesces_and_restart_never_republishes_consumed_wake() {
        let t = tempfile::tempdir().unwrap();
        let store = t.path().join("messages");
        let r = recipient("claude-code", Some(false));
        deliver(t.path(), &store, "a", vec!["one".into(), "two".into()], &r).unwrap();
        let path = queue_path(&store, "a");
        let q = load(&path).unwrap();
        assert_eq!(q.batches.len(), 1);
        assert_eq!(q.batches[0].ids.len(), 2);
        let inbox = t.path().join("inbox/a");
        let files: Vec<_> = fs::read_dir(&inbox).unwrap().collect();
        assert_eq!(files.len(), 1);
        fs::remove_file(files[0].as_ref().unwrap().path()).unwrap();
        deliver(t.path(), &store, "a", vec!["one".into(), "two".into()], &r).unwrap();
        assert_eq!(fs::read_dir(&inbox).unwrap().count(), 0);
        assert_eq!(
            load(&path).unwrap().batches[0].status,
            "submitted_unverified"
        );
    }
    #[test]
    fn messaging_uncertain_attempt_and_corrupt_checkpoint_are_not_retried() {
        let t = tempfile::tempdir().unwrap();
        let r = recipient("claude-code", Some(false));
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
                last_attempt: 0,
            },
        )
        .unwrap();
        deliver(t.path(), t.path(), "a", vec!["one".into()], &r).unwrap();
        assert_eq!(load(&path).unwrap().batches[0].status, "uncertain");
        fs::write(&path, b"broken").unwrap();
        assert!(deliver(t.path(), t.path(), "a", vec!["one".into()], &r).is_err());
        assert!(!t.path().join("inbox").exists());
    }
    #[test]
    fn messaging_unknown_codex_and_unsafe_idle_leave_durable_pending_work() {
        let t = tempfile::tempdir().unwrap();
        let r = recipient("codex", None);
        deliver(t.path(), t.path(), "a", vec!["one".into()], &r).unwrap();
        assert_eq!(status(t.path(), "a", "one")["status"], "deferred");
        let r = recipient("codex", Some(true));
        r.fanout.push(b"\x1b[?2004h");
        r.input
            .stamp_operator_input_at(Instant::now() - Duration::from_secs(600));
        deliver(t.path(), t.path(), "a", vec!["one".into()], &r).unwrap();
        assert_eq!(status(t.path(), "a", "one")["status"], "deferred");
    }
}
