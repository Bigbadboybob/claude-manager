//! A wake is a durable, coalesced hint to read the inbox. Submission is never
//! confused with a read receipt, and A does not retry an uncertain PTY write.
use super::atomic_replace;
use crate::{
    session::{InputHandle, PtyByteFanout, SharedLastActivity},
    state::DaemonState,
};
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
#[derive(Serialize, Deserialize)]
struct Batch {
    wake_id: String,
    ids: Vec<String>,
    status: String,
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
            .find(|b| b.ids.iter().any(|id| id == event))
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
}
pub fn tick(state: &Arc<Mutex<DaemonState>>) {
    let (handle, root, draining) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.messaging_root.clone(), s.draining)
    };
    if draining {
        return;
    }
    let (notifications, store_root) = {
        let Ok(slot) = handle.try_lock() else {
            return;
        };
        let Some(store) = slot.as_ref() else {
            return;
        };
        if store.degraded.is_some() {
            return;
        }
        (store.notifications(), store.root.clone())
    };
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (uid, id, _) in notifications {
        groups.entry(uid).or_default().push(id);
    }
    for (uid, ids) in groups {
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
                })
        };
        let Some(recipient) = recipient else {
            continue;
        };
        if let Err(e) = deliver(&root, &store_root, &uid, ids, &recipient) {
            eprintln!("cm messaging: wake for {uid} pending: {e}");
        }
    }
}
fn deliver(
    cm_root: &Path,
    store_root: &Path,
    uid: &str,
    ids: Vec<String>,
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
    let new: Vec<_> = ids.into_iter().filter(|id| !covered.contains(id)).collect();
    if !new.is_empty() {
        if let Some(batch) = q.batches.iter_mut().find(|b| pending(&b.status)) {
            batch.ids.extend(new);
        } else {
            q.batches.push(Batch {
                wake_id: uuid::Uuid::new_v4().to_string(),
                ids: new,
                status: "pending".into(),
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
        } else if !pending(&q.batches[i].status) {
            continue;
        }
        if !reclaimed && seconds().saturating_sub(q.last_attempt) < 30 {
            continue;
        }
        let text=format!("[cm-chat {}] {} new message(s). Use chat_read(inbox=true, unread_only=true) to read and acknowledge them. This is a notification, not message content.",q.batches[i].wake_id,q.batches[i].ids.len());
        if !["claude-code", "codex"].contains(&r.engine.as_str()) {
            if q.batches[i].status != "deferred_unsupported" {
                q.batches[i].status = "deferred_unsupported".into();
                save(&path, &q)?;
            }
            continue;
        }
        if r.engine == "claude-code" && r.idle != Some(true) {
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
        q.batches[i].status = "idle_attempt_pending".into();
        save(&path, &q)?;
        match r
            .input
            .try_chat_prompt(&text, r.expected_input, &r.turn_end, &r.fanout)
        {
            Ok(false) => q.batches[i].status = "deferred".into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    fn recipient(engine: &str, idle: Option<bool>) -> Recipient {
        Recipient {
            engine: engine.into(),
            idle,
            input: InputHandle::test_handle(),
            turn_end: Arc::new(Mutex::new(Some(Instant::now()))),
            fanout: Arc::new(PtyByteFanout::new(1024)),
            expected_input: None,
        }
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
