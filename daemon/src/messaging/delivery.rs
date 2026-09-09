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
    // Fetching full messages closes only their wake work, not read receipts.
    checked: BTreeSet<String>,
    // Freeze a batch at its first read so concurrent arrivals form a successor.
    sealed: bool,
    released: bool,
    coalesced: BTreeSet<String>,
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
            .map(|b| {
                if b.coalesced.iter().any(|id| {
                    id == event
                        || id
                            .strip_prefix("monitor:")
                            .and_then(|s| s.split_once(':'))
                            .is_some_and(|(_, id)| id == event)
                }) {
                    "coalesced".into()
                } else {
                    b.status
                }
            })
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
    if collect(&mut q, &intents) {
        save(&path, &q)?;
    }
    let first = q
        .batches
        .iter()
        .position(|b| !b.released && b.status != "cancelled");
    for (index, batch) in q.batches.iter_mut().enumerate() {
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
        // A successor waits for the preceding batch's read boundary. Native
        // delivery/observation alone never releases this latch.
        if Some(index) != first || batch.released {
            continue;
        }
        if !["claude-code", "codex"].contains(&r.engine.as_str()) {
            batch.status = "deferred_unsupported".into();
            continue;
        }
        if !r.still_current() {
            continue;
        }
        let event = crate::notifications::publish(
            cm_root,
            uid,
            &id,
            "chat",
            &wake_text(batch, &intents),
            &format!("[cm-chat {}]", batch.wake_id),
        )?;
        batch.status = native_status(&event).into();
    }
    save(&path, &q)
}

fn settle(q: &mut Queue, intents: &[WakeIntent]) -> bool {
    let eligible: BTreeSet<_> = intents.iter().map(|i| &i.key).collect();
    let mut changed = false;
    for batch in &mut q.batches {
        if !batch.released
            && batch
                .ids
                .iter()
                .all(|id| batch.checked.contains(id) || !eligible.contains(id))
        {
            batch.released = true;
            changed = true;
        }
    }
    changed
}

fn collect(q: &mut Queue, intents: &[WakeIntent]) -> bool {
    let changed = settle(q, intents);
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
        if let Some(batch) = q
            .batches
            .iter_mut()
            .find(|b| !b.released && !b.sealed && b.status != "cancelled")
        {
            if !pending(&batch.status) {
                batch.coalesced.extend(new.iter().cloned());
            }
            batch.ids.extend(new);
        } else {
            q.batches.push(Batch {
                wake_id: uuid::Uuid::new_v4().to_string(),
                ids: new,
                status: "pending".into(),
                ..Batch::default()
            });
        }
        return true;
    }
    changed
}

/// Called only for successful full-message reads, under the store and delivery
/// locks. Previews, status queries and invalid reads cannot consume wake work.
/// Exact returned IDs preserve filtered/paginated reads and late arrivals.
pub fn read_boundary(store: &Store, uid: &str, result: &Value) -> io::Result<()> {
    let ids: BTreeSet<_> = result["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["id"].as_str())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let intents = store.wake_intents().remove(uid).unwrap_or_default();
    let path = queue_path(&store.root, uid);
    let mut q = load(&path)?;
    // Include arrivals not yet visited by the delivery worker before sealing.
    collect(&mut q, &intents);
    let checked: BTreeSet<_> = intents
        .iter()
        .filter(|i| ids.contains(i.event_id.as_str()))
        .map(|i| i.key.clone())
        .collect();
    for batch in &mut q.batches {
        for id in &batch.ids {
            if checked.contains(id) {
                batch.checked.insert(id.clone());
                batch.sealed = true;
            }
        }
    }
    settle(&mut q, &intents);
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
    format!("[cm-chat {}] New chat activity. Before responding, read pending messages with chat_read(inbox=true, unread_only=true); follow next_cursor for all pages and acknowledge each receipt after reading.{} Continue the existing task. Do not repeat a completed answer or summary; report only meaningful changes, blockers, or decisions needing Owner. If nothing needs attention, no user-facing update is needed. Automated CM notification, not Owner input or message content.",batch.wake_id,hint)
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
        settle(&mut q, &intents);
        let eligible: BTreeSet<_> = intents.iter().map(|i| i.key.as_str()).collect();
        for batch in &mut q.batches {
            let active = !batch.released
                && batch
                    .ids
                    .iter()
                    .any(|id| eligible.contains(id.as_str()) && !batch.checked.contains(id));
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
    fn fixture() -> (tempfile::TempDir, Store, String, Vec<super::super::Person>) {
        let t = tempfile::tempdir().unwrap();
        let mut store = Store::open(t.path()).unwrap();
        let actor = store.participant_id("b");
        let people = vec![super::super::Person {
            id: actor.clone(),
            name: "Reader".into(),
            session_uid: "b".into(),
            kind: "agent".into(),
            present: true,
            task: None,
        }];
        store.enroll_participants(&people).unwrap();
        (t, store, actor, people)
    }
    fn dm(store: &mut Store, actor: &str, people: &[super::super::Person], key: &str) -> String {
        store
            .send(
                "owner",
                "",
                "owner",
                &json!({"dm":actor,"body":key,"request_id":key}),
                people,
            )
            .unwrap()["event_id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn deliver(store: &Store, cm_root: &Path, engine: &str) -> Queue {
        deliver_intents(
            cm_root,
            &store.root,
            "b",
            store.wake_intents().remove("b").unwrap_or_default(),
            &recipient(engine),
        )
        .unwrap();
        load(&queue_path(&store.root, "b")).unwrap()
    }
    fn native_event(cm_root: &Path, batch: &Batch) -> Option<Value> {
        crate::notifications::get(cm_root, "b", &format!("chat:{}", batch.wake_id)).unwrap()
    }
    fn set_native_status(cm_root: &Path, batch: &Batch, status: &str) {
        let mut event = native_event(cm_root, batch).unwrap();
        event["status"] = json!(status);
        atomic_replace(
            &crate::notifications::directory(cm_root, "b")
                .join(format!("{}.json", digest(event["id"].as_str().unwrap()))),
            &event,
        )
        .unwrap();
    }
    fn read_page(store: &mut Store, actor: &str, params: &Value) -> Value {
        let result = store.read(actor, params, &[]).unwrap();
        read_boundary(store, "b", &result).unwrap();
        result
    }
    #[test]
    fn messaging_bursts_share_one_wake_until_read_for_all_native_states_and_engines() {
        for engine in ["claude-code", "codex"] {
            for state in [
                "pending",
                "submitting",
                "submitted",
                "observed",
                "uncertain",
            ] {
                let (t, mut store, actor, people) = fixture();
                dm(&mut store, &actor, &people, "one");
                let first = deliver(&store, t.path(), engine);
                set_native_status(t.path(), &first.batches[0], state);
                let second = dm(&mut store, &actor, &people, "two");
                deliver(&store, t.path(), engine);
                drop(store);
                let mut store = Store::open(t.path()).unwrap();
                dm(&mut store, &actor, &people, "three");
                let q = deliver(&store, t.path(), engine);
                assert_eq!(q.batches.len(), 1, "{engine} {state}");
                assert_eq!(q.batches[0].ids.len(), 3);
                assert_eq!(q.batches[0].wake_id, first.batches[0].wake_id);
                assert_eq!(
                    native_event(t.path(), &q.batches[0]).unwrap()["status"],
                    state
                );
                assert_eq!(status(&store.root, "b", &second)["status"], "coalesced");
                let page = read_page(
                    &mut store,
                    &actor,
                    &json!({"inbox":true,"unread_only":true}),
                );
                assert_eq!(page["items"].as_array().unwrap().len(), 3);
                // Retrieval advances notification handling, never message read state.
                assert!(page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|e| e["read"] == false));
                reconcile(t.path(), &store).unwrap();
                dm(&mut store, &actor, &people, "four");
                let q = deliver(&store, t.path(), engine);
                assert_eq!(q.batches.len(), 2);
                assert!(q.batches[0].released);
                assert!(native_event(t.path(), &q.batches[1]).is_some());
            }
        }
    }
    #[test]
    fn messaging_paginated_read_freezes_batch_and_late_arrivals_wait_for_remaining_page() {
        let (t, mut store, actor, people) = fixture();
        let one = dm(&mut store, &actor, &people, "one");
        let two = dm(&mut store, &actor, &people, "two");
        let q = deliver(&store, t.path(), "codex");
        set_native_status(t.path(), &q.batches[0], "observed");
        let params = json!({"inbox":true,"unread_only":true,"limit":1});
        let page = read_page(&mut store, &actor, &params);
        assert_eq!(page["items"][0]["id"], one);
        assert!(page["next_cursor"].is_object());
        let three = dm(&mut store, &actor, &people, "three");
        let q = deliver(&store, t.path(), "codex");
        assert_eq!(q.batches.len(), 2);
        assert!(q.batches[0].sealed);
        assert!(!q.batches[0].released);
        assert!(native_event(t.path(), &q.batches[1]).is_none());
        drop(store);
        let mut store = Store::open(t.path()).unwrap();
        let mut next = params.clone();
        next["cursor"] = page["next_cursor"].clone();
        next["ack_receipt"] = page["receipt"].clone();
        let page2 = read_page(&mut store, &actor, &next);
        assert_eq!(page2["items"][0]["id"], two);
        assert!(page2["next_cursor"].is_null());
        let q = deliver(&store, t.path(), "codex");
        assert!(q.batches[0].released);
        assert_eq!(q.batches[1].ids, vec![three.clone()]);
        assert!(native_event(t.path(), &q.batches[1]).is_some());
        // The old receipt cannot acknowledge an arrival after its snapshot.
        store.acknowledge(&actor, &page2["receipt"]).unwrap();
        let fresh = read_page(&mut store, &actor, &params);
        assert_eq!(fresh["items"][0]["id"], three);
        assert_eq!(fresh["items"][0]["read"], false);
    }
    #[test]
    fn messaging_proactive_and_filtered_reads_cover_only_returned_message_ids() {
        let (t, mut store, actor, people) = fixture();
        let one = dm(&mut store, &actor, &people, "one");
        let two = dm(&mut store, &actor, &people, "two");
        // No worker pass yet: the first proactive read must cover its own IDs.
        let first = read_page(&mut store, &actor, &json!({"inbox":true,"limit":1}));
        assert_eq!(first["items"][0]["id"], one);
        let q = deliver(&store, t.path(), "claude-code");
        assert_eq!(q.batches.len(), 1);
        assert!(native_event(t.path(), &q.batches[0]).is_some()); // unread second page
        let before = q.batches[0].checked.clone();
        read_page(&mut store, &actor, &json!({"channel":"general"}));
        let q = deliver(&store, t.path(), "claude-code");
        assert_eq!(q.batches[0].checked, before); // unrelated empty read
        let full = read_page(&mut store, &actor, &json!({"inbox":true}));
        assert_eq!(full["items"][1]["id"], two);
        reconcile(t.path(), &store).unwrap();
        let q = deliver(&store, t.path(), "claude-code");
        assert_eq!(
            native_event(t.path(), &q.batches[0]).unwrap()["status"],
            "cancelled"
        );
        let three = dm(&mut store, &actor, &people, "three");
        // Read before publication: there must be no now-useless successor wake.
        read_page(&mut store, &actor, &json!({"inbox":true}));
        let q = deliver(&store, t.path(), "claude-code");
        assert_eq!(q.batches[1].ids, vec![three]);
        assert!(q.batches[1].released);
        assert!(native_event(t.path(), &q.batches[1]).is_none());
    }
    #[test]
    fn messaging_read_discharges_chat_and_monitor_wakes_but_preserves_monitor_results() {
        let (t, mut store, actor, people) = fixture();
        let m = store
            .register_monitor(
                &actor,
                &json!({"scope":{"dms":true},"mode":"continuous","request_id":"watch"}),
                &people,
            )
            .unwrap();
        dm(&mut store, &actor, &people, "one");
        store.advance_monitors_at(chrono::Utc::now()).unwrap();
        let q = deliver(&store, t.path(), "codex");
        assert_eq!(q.batches[0].ids.len(), 2); // default DM and watch, one wake
        read_page(&mut store, &actor, &json!({"inbox":true}));
        reconcile(t.path(), &store).unwrap();
        let q = deliver(&store, t.path(), "codex");
        assert!(q.batches[0].released);
        assert_eq!(
            native_event(t.path(), &q.batches[0]).unwrap()["status"],
            "cancelled"
        );
        let result = store
            .monitors(
                &actor,
                &json!({"action":"get","monitor_id":m["id"],"unacknowledged_only":true}),
            )
            .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
        dm(&mut store, &actor, &people, "two");
        store.advance_monitors_at(chrono::Utc::now()).unwrap();
        let q = deliver(&store, t.path(), "codex");
        assert_eq!(q.batches.len(), 2);
        assert!(native_event(t.path(), &q.batches[1]).is_some());
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
    fn messaging_default_dm_direct_and_here_reach_native_queue_for_both_engines() {
        for engine in ["claude-code", "codex"] {
            let t = tempfile::tempdir().unwrap();
            let mut store = Store::open(t.path()).unwrap();
            let actor = store.participant_id(engine);
            let people = vec![super::super::Person {
                id: actor.clone(),
                name: engine.into(),
                session_uid: engine.into(),
                kind: "agent".into(),
                present: true,
                task: None,
            }];
            store.enroll_participants(&people).unwrap();
            for (key, mut params) in [
                ("dm", json!({"dm":actor})),
                ("direct", json!({"channel":"general","mentions":[actor]})),
                ("here", json!({"channel":"general","mention_here":true})),
            ] {
                params["body"] = json!("Work ready");
                params["request_id"] = json!(key);
                store.send("owner", "", "owner", &params, &people).unwrap();
            }
            let intents = store.wake_intents()[engine].clone();
            assert_eq!(intents.len(), 3);
            assert!(intents.iter().all(|i| i.monitor.is_none()));
            deliver_intents(
                t.path(),
                t.path(),
                engine,
                intents.clone(),
                &recipient(engine),
            )
            .unwrap();
            drop(store);
            let store = Store::open(t.path()).unwrap();
            deliver_intents(
                t.path(),
                t.path(),
                engine,
                store.wake_intents()[engine].clone(),
                &recipient(engine),
            )
            .unwrap();
            let queue = load(&queue_path(t.path(), engine)).unwrap();
            assert_eq!(queue.batches.len(), 1);
            assert_eq!(queue.batches[0].ids.len(), 3);
            let event = crate::notifications::get(
                t.path(),
                engine,
                &format!("chat:{}", queue.batches[0].wake_id),
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
