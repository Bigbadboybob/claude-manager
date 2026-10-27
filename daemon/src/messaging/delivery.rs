//! A wake is a durable, coalesced hint to read the inbox. Submission is never
//! confused with a read receipt. A current inbound transcript marker confirms
//! delivery. Native adapters never resend an ambiguous submission.
pub mod alarm;
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
/// Who wrote the newest unread message, where, and its first line. Lets a
/// wake say what arrived instead of only "new chat activity".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub place: String,
    pub sender: String,
    pub excerpt: String,
}
type PreviewFn<'a> = &'a dyn Fn(&str) -> Option<Preview>;
fn no_preview(_: &str) -> Option<Preview> {
    None
}
const EXCERPT_CHARS: usize = 120;
const WAKE_TEXT_BUDGET: usize = 400;

fn clip(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() <= max {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Store lookup for one event. Reads only that event's record.
pub fn preview(store: &Store, event_id: &str) -> Option<Preview> {
    let wire = store.wire_event(event_id).ok()?;
    let event: Value = serde_json::from_str(wire["event"].as_str()?).ok()?;
    let actor = &event["actor"];
    let sender = actor["id"]
        .as_str()
        .and_then(|id| store.names.get(id))
        .map(|n| n.name.clone())
        .or_else(|| actor["name"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "someone".into());
    let place = conversation_label(store, event["conversation_id"].as_str().unwrap_or(""));
    Some(Preview {
        place,
        sender: clip(&sender, 40),
        excerpt: clip(event["body"].as_str().unwrap_or(""), EXCERPT_CHARS),
    })
}

fn conversation_label(store: &Store, conversation_id: &str) -> String {
    store
        .channel_path_of(conversation_id)
        .map(|p| clip(&format!("#{p}"), 48))
        .unwrap_or_else(|| "DM".into())
}

/// Readable conversation names for `messaging.read` pages (slim view uses
/// them). Channels get `#path`; DMs are left to the reader's sender label.
pub fn annotate_conversations(store: &Store, value: &mut Value) {
    for key in ["items", "context"] {
        for item in value[key].as_array_mut().into_iter().flatten() {
            if let Some(path) = item["conversation_id"]
                .as_str()
                .and_then(|id| store.channel_path_of(id))
            {
                item["conversation_path"] = json!(format!("#{path}"));
            }
        }
    }
}

fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
#[derive(Default, Serialize, Deserialize)]
struct Queue {
    batches: Vec<Batch>,
    last_attempt: u64,
    // In-memory checkpoint state, including migrations applied during load.
    #[serde(skip)]
    dirty: bool,
    // Effect-scoped keys whose task authority is unknown. Never persisted:
    // every caller rebuilds this projection while holding authority guards.
    #[serde(skip)]
    deferred: BTreeSet<String>,
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
    // Pre-batching ledgers did not record fetches. Retain their dedup IDs and
    // receipts as history rather than blocking new wakes behind old unread IDs.
    released: bool,
    coalesced: BTreeSet<String>,
    // When the wake first reached the agent (confirmed / submitted / uncertain).
    // A delivered batch the agent never read stops absorbing new arrivals after
    // `LATCH_HORIZON_SECS`, so a session that ignored one wake still gets the
    // next one (2026-09-14: an orchestrator's 11:01 wake swallowed a 16:17
    // mention for good).
    #[serde(default)]
    delivered_at: u64,
}
impl Batch {
    fn deferred(&self, keys: &BTreeSet<String>) -> bool {
        self.ids.iter().any(|id| keys.contains(id))
    }
    fn settled_history(&self) -> bool {
        self.released && matches!(self.status.as_str(), "confirmed" | "cancelled")
    }

    fn set_status(&mut self, status: &str) -> bool {
        if self.status == status {
            return false;
        }
        self.status = status.into();
        true
    }
}
/// How long a delivered-but-unread wake batch keeps coalescing new arrivals.
const LATCH_HORIZON_SECS: u64 = 120;
fn delivered(status: &str) -> bool {
    matches!(status, "confirmed" | "submitted_unverified" | "uncertain")
}
/// Stamp delivery times and release latches past the horizon. Released
/// batches keep their ids as history; read receipts are separate anyway.
fn expire_latches(q: &mut Queue, now: u64) -> bool {
    let mut changed = false;
    for batch in &mut q.batches {
        if batch.deferred(&q.deferred) || batch.released || !delivered(&batch.status) {
            continue;
        }
        if batch.delivered_at == 0 {
            batch.delivered_at = now;
            changed = true;
        } else if now.saturating_sub(batch.delivered_at) >= LATCH_HORIZON_SECS {
            // Arrivals absorbed after delivery never had a wake of their own;
            // hand the unfetched ones to a successor so `collect` re-batches
            // them instead of treating them as covered history.
            let orphaned: BTreeSet<String> = batch
                .coalesced
                .iter()
                .filter(|id| !batch.checked.contains(*id))
                .cloned()
                .collect();
            batch.ids.retain(|id| !orphaned.contains(id));
            batch.coalesced.retain(|id| !orphaned.contains(id));
            batch.released = true;
            batch.sealed = true;
            changed = true;
        }
    }
    changed
}
fn queue_path(root: &Path, uid: &str) -> PathBuf {
    root.join("_delivery").join(format!("{}.json", digest(uid)))
}
#[cfg(test)]
thread_local! {
    static QUEUE_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static QUEUE_SAVE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FAIL_QUEUE_SAVE_AT: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static AFTER_RECONCILE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(super) fn queue_load_count() -> usize {
    QUEUE_LOADS.with(|count| count.get())
}
fn load(path: &Path) -> io::Result<Queue> {
    #[cfg(test)]
    QUEUE_LOADS.with(|count| count.set(count.get() + 1));
    match fs::read(path) {
        Ok(b) => {
            let mut value: Value = serde_json::from_slice(&b)?;
            let mut migrated = false;
            for batch in value["batches"].as_array_mut().into_iter().flatten() {
                if batch.get("released").is_none() {
                    let status = batch["status"].as_str().unwrap_or("pending");
                    // Keep old unsubmitted work live. Delivered/ambiguous old
                    // receipts have no fetch history and cannot own a new latch.
                    batch["released"] = json!(!pending(status) && status != "native_pending");
                    migrated = true;
                }
            }
            let mut queue: Queue = serde_json::from_value(value)?;
            queue.dirty = migrated;
            Ok(queue)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Queue::default()),
        Err(e) => Err(e),
    }
}
fn save(path: &Path, q: &Queue) -> io::Result<()> {
    #[cfg(test)]
    {
        let calls = QUEUE_SAVE_CALLS.with(|count| {
            count.set(count.get() + 1);
            count.get()
        });
        if FAIL_QUEUE_SAVE_AT.with(|at| at.get() == Some(calls)) {
            return Err(io::Error::other("injected delivery checkpoint failure"));
        }
    }
    atomic_replace(path, &serde_json::to_value(q)?)
}
fn checkpoint(path: &Path, q: &mut Queue) -> io::Result<()> {
    if q.dirty {
        save(path, q)?;
        q.dirty = false;
    }
    Ok(())
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
            let state = match state.try_lock() {
                Ok(s) => s,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return None,
            };
            let s = state.sessions.get(uid)?;
            return marker::binding(s.transcript_path.as_deref(), s.generation);
        }
        self.binding.clone()
    }
    fn still_current(&self) -> bool {
        self.live.as_ref().is_none_or(|(state, uid)| {
            state.upgrade().is_some_and(|state| {
                // Publication may retain task authority guards. Never wait on
                // the registry in that lock order; this tick can retry later.
                let state = match state.try_lock() {
                    Ok(s) => s,
                    Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                    Err(std::sync::TryLockError::WouldBlock) => return false,
                };
                state.sessions.get(uid).is_some_and(|s| {
                    Arc::ptr_eq(&s.fanout, &self.fanout)
                        && marker::binding(s.transcript_path.as_deref(), s.generation)
                            == self.binding
                })
            })
        })
    }
}
const SKIP_OUTPUT_CLOSED: &str =
    "its terminal output has ended (PTY reader closed) but the session is still registered";

/// Sessions currently skipped with wakes waiting, and why. One log line when
/// a session starts being skipped, one when it is reachable again.
static SKIPPED: Mutex<BTreeMap<String, &'static str>> = Mutex::new(BTreeMap::new());

/// Record whether `uid` was skipped this tick. Returns true when a line was
/// logged.
fn note_skip(uid: &str, reason: Option<&'static str>, waiting: usize) -> bool {
    let mut skipped = SKIPPED.lock().unwrap_or_else(|p| p.into_inner());
    match reason.filter(|_| waiting > 0) {
        Some(reason) => {
            if skipped.get(uid) == Some(&reason) {
                return false;
            }
            skipped.insert(uid.to_owned(), reason);
            eprintln!(
                "cm messaging: not waking {uid} ({waiting} wake(s) waiting): {reason}; \
                 its messages wait until the session is restarted (A-R) or exits"
            );
            true
        }
        None => {
            if skipped.remove(uid).is_none() {
                return false;
            }
            eprintln!("cm messaging: {uid} is reachable again; delivering its waiting wakes");
            true
        }
    }
}

pub fn tick(state: &Arc<Mutex<DaemonState>>) {
    // Release legacy held Owner requests after upgrade, independently of
    // the messaging store lock below. Availability no longer filters them.
    crate::owner_availability::tick(state);
    let (handle, root, gate, recipients) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        if s.draining { return; }
        (s.messaging.clone(), s.messaging_root.clone(), s.messaging_delivery.clone(),
         s.sessions.keys().cloned().collect::<Vec<_>>())
    };
    {
        let _guard = gate.lock().unwrap_or_else(|p| p.into_inner());
        let Ok(mut slot) = handle.try_lock() else { return; };
        let Some(store) = slot.as_mut() else { return; };
        let mut authority = super::task_authority::View::new(store);
        let store = &mut *authority;
        if store.degraded.is_some() || store.advance_monitors_at(chrono::Utc::now()).is_err() {
            return;
        }
    }
    for uid in recipients {
        // Keep the existing cancellation/publication boundary, but evaluate
        // only this live recipient. A drain ends the tick between recipients.
        let _guard = gate.lock().unwrap_or_else(|p| p.into_inner());
        let recipient = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            if s.draining { return; }
            let Some(s) = s.sessions.get(&uid) else { continue; };
            if s.fanout.snapshot_since(None).closed {
                Err(SKIP_OUTPUT_CLOSED)
            } else {
                Ok(Recipient {
                    engine: s.session_type.clone(),
                    fanout: s.fanout.clone(),
                    binding: marker::binding(s.transcript_path.as_deref(), s.generation),
                    live: Some((Arc::downgrade(state), uid.clone())),
                })
            }
        };
        let recipient = match recipient {
            Ok(recipient) => {
                note_skip(&uid, None, 0);
                recipient
            }
            Err(reason) => {
                // Still registered, so it looks live, but nothing can reach it.
                // Say so once, while it has wakes waiting (2026-10-16: an
                // orchestrator missed 5 minutes of Owner DMs with no log line).
                let waiting = handle
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .as_mut()
                    .map_or(0, |store| super::task_authority::View::new(store).wake_intents_for_session(&uid).len());
                note_skip(&uid, Some(reason), waiting);
                continue;
            }
        };
        let (ids, store_root, mut queue, _authority, previews) = {
            let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
            let Some(store) = slot.as_mut() else { return; };
            if store.degraded.is_some() { return; }
            let mut authority = super::task_authority::View::new(store);
            let store = &mut *authority;
            let ids = store.wake_intents_for_session(&uid);
            let queue = match reconcile_intents(&root, store, &uid, &ids) {
                Ok(queue) => queue,
                Err(e) => {
                    eprintln!("cm messaging: wake reconciliation for {uid}: {e}");
                    return;
                }
            };
            let previews: BTreeMap<_, _> = ids.iter().filter_map(|i| preview(store, &i.event_id)
                .map(|p| (i.event_id.clone(), p))).collect();
            let store_root = store.root.clone();
            (ids, store_root, queue, authority.take_guards(), previews)
        };
        #[cfg(test)]
        if let Some(hook) = AFTER_RECONCILE.with(|hook| hook.borrow_mut().take()) { hook(); }
        // No blocking state/store reacquisition while task authority is held.
        // Previews refer to the same immutable messages as this intent batch.
        let lookup = |id: &str| previews.get(id).cloned();
        if let Err(e) = deliver_queue(
            &root,
            &store_root,
            &uid,
            &ids,
            &recipient,
            crate::continuous::task::now_unix(),
            &lookup,
            &mut queue,
        ) {
            eprintln!("cm messaging: wake for {uid} pending: {e}");
        }
    }
    alarm_tick(state, &root);
}

/// Throttled stall alarm over live native-capable sessions.
fn alarm_tick(state: &Arc<Mutex<DaemonState>>, root: &Path) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::continuous::task::now_unix();
    let last = LAST.load(Ordering::Relaxed);
    if now.saturating_sub(last) < alarm::ALARM_INTERVAL_SECS
        || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err()
    {
        return;
    }
    let uids: Vec<String> = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        if s.draining {
            return;
        }
        s.sessions
            .iter()
            .filter(|(_, x)| ["claude-code", "codex"].contains(&x.session_type.as_str()))
            .filter(|(_, x)| !x.fanout.snapshot_since(None).closed)
            .map(|(uid, _)| uid.clone())
            .collect()
    };
    let now = chrono::Utc::now().timestamp_millis() as f64 / 1000.0;
    alarm::pass(state, root, &uids, now, alarm::threshold_secs());
    // Sender-side: a session's own messages stuck pending_sync (hub link
    // stalled). Skipped this round if the store is busy.
    let handle = state.lock().unwrap_or_else(|p| p.into_inner()).messaging.clone();
    let outboxes: Option<Vec<(String, Option<serde_json::Value>)>> = handle.try_lock().ok().and_then(|slot| {
        slot.as_ref().map(|store| {
            let overdue = store.outbox_overdue();
            uids.iter()
                .map(|uid| (uid.clone(), overdue.get(&store.participant_id(uid)).cloned()))
                .collect()
        })
    });
    if let Some(outboxes) = outboxes {
        alarm::outbox_pass(state, root, &outboxes, alarm::OUTBOX_ALERT_SECS);
    }
}

#[cfg(test)]
fn deliver_intents(
    cm_root: &Path,
    store_root: &Path,
    uid: &str,
    intents: Vec<WakeIntent>,
    r: &Recipient,
) -> io::Result<()> {
    deliver_intents_at(
        cm_root,
        store_root,
        uid,
        intents,
        r,
        crate::continuous::task::now_unix(),
    )
}
#[cfg(test)]
fn deliver_intents_at(
    cm_root: &Path,
    store_root: &Path,
    uid: &str,
    intents: Vec<WakeIntent>,
    r: &Recipient,
    now: u64,
) -> io::Result<()> {
    deliver_intents_previewed(cm_root, store_root, uid, intents, r, now, &no_preview)
}
#[cfg(test)]
fn deliver_intents_previewed(
    cm_root: &Path,
    store_root: &Path,
    uid: &str,
    intents: Vec<WakeIntent>,
    r: &Recipient,
    now: u64,
    preview: PreviewFn,
) -> io::Result<()> {
    let path = queue_path(store_root, uid);
    let mut q = load(&path)?;
    deliver_queue(cm_root, store_root, uid, &intents, r, now, preview, &mut q)
}

// The production caller retains the recipient delivery gate across reconcile
// and delivery, so the checkpointed queue can be reused without a second load.
fn deliver_queue(
    cm_root: &Path,
    store_root: &Path,
    uid: &str,
    intents: &[WakeIntent],
    r: &Recipient,
    now: u64,
    preview: PreviewFn,
    q: &mut Queue,
) -> io::Result<()> {
    let path = queue_path(store_root, uid);
    q.dirty |= expire_latches(q, now);
    q.dirty |= collect(q, intents);
    checkpoint(&path, q)?;
    let first = q
        .batches
        .iter()
        .position(|b| !b.deferred(&q.deferred) && !b.released && b.status != "cancelled");
    for index in 0..q.batches.len() {
        let batch = &mut q.batches[index];
        // Released ambiguous submissions still need late native/transcript
        // confirmation. Only final receipts are safe to leave as history.
        if batch.deferred(&q.deferred) || batch.settled_history() {
            continue;
        }
        let id = format!("chat:{}", batch.wake_id);
        if let Some(event) = crate::notifications::get(cm_root, uid, &id)? {
            q.dirty |= batch.set_status(native_status(&event));
            if delivered(&batch.status) && batch.delivered_at == 0 {
                batch.delivered_at = now;
                q.dirty = true;
            }
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
                    Ok(()) => q.dirty |= batch.set_status("pending"),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        q.dirty |= batch.set_status("uncertain")
                    }
                    Err(e) => return Err(e),
                }
            }
            if !pending(&batch.status) {
                if matches!(batch.status.as_str(), "submitted_unverified" | "uncertain") {
                    let current = r.current_binding();
                    let before = batch.scan.clone();
                    if marker::inspect(
                        &mut batch.scan,
                        batch.original.as_ref(),
                        current.as_ref(),
                        &r.engine,
                        &batch.wake_id,
                    ) == Evidence::Found
                    {
                        q.dirty |= batch.set_status("confirmed");
                    }
                    q.dirty |= batch.scan != before;
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
            q.dirty |= batch.set_status("deferred_unsupported");
            continue;
        }
        if !r.still_current() {
            continue;
        }
        let text = wake_text(batch, intents, preview);
        let marker = format!("[cm-chat {}]", batch.wake_id);
        // Also checkpoint a legacy retirement before its replacement can be
        // claimed. A failed pre-publication save must leave no new native wake.
        checkpoint(&path, q)?;
        let event = crate::notifications::publish(
            cm_root,
            uid,
            &id,
            "chat",
            &text,
            &marker,
        )?;
        q.dirty |= q.batches[index].set_status(native_status(&event));
    }
    checkpoint(&path, q)
}

fn settle(q: &mut Queue, intents: &[WakeIntent]) -> bool {
    let eligible: BTreeSet<_> = intents.iter().map(|i| &i.key).collect();
    let mut changed = false;
    for batch in &mut q.batches {
        if !batch.deferred(&q.deferred) && !batch.released
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
            .find(|b| !b.deferred(&q.deferred) && !b.released && !b.sealed && b.status != "cancelled")
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
    let intents = store.wake_intents_for_session(uid);
    let path = queue_path(&store.root, uid);
    let mut q = load(&path)?;
    // Include arrivals not yet visited by the delivery worker before sealing.
    defer_task_keys(store, uid, &mut q);
    q.dirty |= collect(&mut q, &intents);
    let checked: BTreeSet<_> = intents
        .iter()
        .filter(|i| ids.contains(i.event_id.as_str()))
        .map(|i| i.key.clone())
        .collect();
    for batch in &mut q.batches {
        for id in &batch.ids {
            if checked.contains(id) {
                q.dirty |= batch.checked.insert(id.clone());
                if !batch.sealed {
                    batch.sealed = true;
                    q.dirty = true;
                }
            }
        }
    }
    q.dirty |= settle(&mut q, &intents);
    checkpoint(&path, &mut q)
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

/// Short (<= ~400 chars) wake: count, newest sender/place/excerpt, how to
/// read it (slim inbox), and event-specific watch IDs. The longer handling
/// rules live in the MCP initialization instructions and AGENT_GUIDE.
fn wake_text(batch: &Batch, intents: &[WakeIntent], preview: PreviewFn) -> String {
    let mut unread: Vec<&str> = Vec::new();
    for intent in intents
        .iter()
        .filter(|n| batch.ids.contains(&n.key) && !batch.checked.contains(&n.key))
    {
        if !unread.contains(&intent.event_id.as_str()) {
            unread.push(&intent.event_id);
        }
    }
    let monitors: BTreeSet<_> = intents
        .iter()
        .filter(|n| batch.ids.contains(&n.key))
        .filter_map(|n| n.monitor.as_deref())
        .collect();
    let marker = format!("[cm-chat {}]", batch.wake_id);
    let read = " Read with chat_read(inbox=True, unread_only=True, view=\"slim\"), follow next_cursor, and ack the receipt.";
    let hint = if monitors.is_empty() {
        String::new()
    } else {
        let more = monitors.len().saturating_sub(1);
        format!(
            " Watch results: {}{}; see chat_monitors(action=\"list\").",
            monitors.iter().next().copied().unwrap_or_default(),
            if more > 0 { format!(" (+{more})") } else { String::new() }
        )
    };
    let tail = " Continue your task; automated CM notice, not Owner input.";
    let count = unread.len();
    let head = match unread.last().and_then(|id| preview(id)) {
        Some(p) => {
            let more = if count > 1 { format!(" (+{} more)", count - 1) } else { String::new() };
            let fixed = format!("{marker} {count} new: {} — {}: \"\"{more}.", p.place, p.sender);
            let room = WAKE_TEXT_BUDGET
                .saturating_sub(fixed.chars().count() + read.len() + hint.chars().count() + tail.len())
                .clamp(24, EXCERPT_CHARS);
            format!("{marker} {count} new: {} — {}: \"{}\"{more}.", p.place, p.sender, clip(&p.excerpt, room))
        }
        None if count > 0 => format!("{marker} {count} new chat message{}.", if count == 1 { "" } else { "s" }),
        None => format!("{marker} New chat activity."),
    };
    format!("{head}{read}{hint}{tail}")
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
#[cfg(test)]
pub fn reconcile(cm_root: &Path, store: &Store) -> io::Result<()> {
    for (uid, intents) in store.wake_intents() {
        reconcile_intents(cm_root, store, &uid, &intents)?;
    }
    Ok(())
}

/// Agent read receipts, follows and monitor changes affect only that actor.
/// Keep offline actors' durable queues until they resume or explicitly cancel.
pub fn reconcile_session(cm_root: &Path, store: &Store, uid: &str) -> io::Result<()> {
    reconcile_intents(cm_root, store, uid, &store.wake_intents_for_session(uid)).map(|_| ())
}

fn defer_task_keys(store: &Store, uid: &str, q: &mut Queue) {
    let prefixes = store.deferred_task_wake_prefixes(&store.participant_id(uid));
    q.deferred = q.batches.iter().flat_map(|b| &b.ids)
        .filter(|id| prefixes.iter().any(|p| id.starts_with(p)))
        .cloned().collect();
}

/// Move eligible personal work out of a never-published mixed batch. An
/// existing native receipt (even pending), or any legacy/ambiguous attempt,
/// keeps its identity and dedup keys intact. Independent new arrivals can
/// bypass that frozen batch, but already submitted work is never replayed.
fn split_unpublished(cm_root: &Path, uid: &str, q: &mut Queue, intents: &[WakeIntent]) -> io::Result<()> {
    let eligible: BTreeSet<_> = intents.iter().map(|i| i.key.as_str()).collect();
    let mut split = Vec::new();
    for batch in &mut q.batches {
        if !batch.deferred(&q.deferred) || batch.released || !pending(&batch.status)
            || batch.attempts != 0 || batch.attempted_at != 0 || batch.original.is_some()
            || crate::notifications::get(cm_root, uid, &format!("chat:{}", batch.wake_id))?.is_some() {
            continue;
        }
        let ids: Vec<_> = batch.ids.iter()
            .filter(|id| !q.deferred.contains(*id) && !batch.checked.contains(*id) && eligible.contains(id.as_str()))
            .cloned().collect();
        if ids.is_empty() { continue; }
        let moved: BTreeSet<_> = ids.iter().cloned().collect();
        batch.ids.retain(|id| !moved.contains(id));
        batch.coalesced.retain(|id| !moved.contains(id));
        split.push(Batch { wake_id: uuid::Uuid::new_v4().to_string(), ids, status: "pending".into(), ..Batch::default() });
    }
    if !split.is_empty() {
        q.batches.extend(split);
        q.dirty = true;
    }
    Ok(())
}

fn reconcile_intents(cm_root: &Path, store: &Store, uid: &str, intents: &[WakeIntent]) -> io::Result<Queue> {
    let path = queue_path(&store.root, uid);
    let mut q = load(&path)?;
    defer_task_keys(store, uid, &mut q);
    split_unpublished(cm_root, uid, &mut q, intents)?;
    // The separation is one durable write before any native queue effect.
    // Reload/retry sees the same personal wake identity and all retained keys.
    checkpoint(&path, &mut q)?;
    q.dirty |= settle(&mut q, intents);
    let eligible: BTreeSet<_> = intents.iter().map(|i| i.key.as_str()).collect();
    for batch in &mut q.batches {
        if batch.deferred(&q.deferred) || batch.settled_history() {
            continue;
        }
        let active = batch
            .ids
            .iter()
            .any(|id| eligible.contains(id.as_str()) && !batch.checked.contains(id));
        let id = format!("chat:{}", batch.wake_id);
        // Only a still-pending native event can be rewritten; skip the store
        // lookup for delivered batches an agent has not read yet.
        let text = active.then(|| {
            if matches!(batch.status.as_str(), "native_pending" | "pending" | "deferred" | "deferred_unsupported") {
                wake_text(batch, intents, &|id: &str| preview(store, id))
            } else {
                wake_text(batch, intents, &no_preview)
            }
        });
        if let Some(event) =
            crate::notifications::update_pending(cm_root, uid, &id, text.as_deref())?
        {
            q.dirty |= batch.set_status(native_status(&event));
        } else if !active && pending(&batch.status) {
            q.dirty |= batch.set_status("cancelled");
        } else if !active
            && matches!(
                batch.status.as_str(),
                "hook_pending" | "hook_attempt_pending"
            )
        {
            let inbox = cm_root
                .join("inbox")
                .join(uid)
                .join(format!("chat-{}.json", batch.wake_id));
            let claim = inbox.with_extension("json.retired");
            match fs::rename(&inbox, &claim) {
                Ok(()) => {
                    fs::remove_file(&claim)?;
                    fs::File::open(inbox.parent().unwrap())?.sync_all()?;
                    q.dirty |= batch.set_status("cancelled");
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    q.dirty |= batch.set_status("submitted_no_retry")
                }
                Err(e) => return Err(e),
            }
        }
    }
    checkpoint(&path, &mut q)?;
    Ok(q)
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
            engine: None,
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

    fn save_calls() -> usize {
        QUEUE_SAVE_CALLS.with(|count| count.get())
    }

    fn mixed_task_queue() -> (tempfile::TempDir, Store, String, Vec<super::super::Person>, String, Vec<WakeIntent>) {
        let (tmp, mut store, actor, people) = fixture();
        let sub = uuid::Uuid::new_v4().to_string();
        let channel = store.channel_id_at_path("general").unwrap().to_string();
        store.bind_task_subscription(super::super::TaskBinding {
            id: sub.clone(), task_id: "mixed-task".into(), channel_id: channel.clone(), actor: actor.clone(),
            revision: 1, active: true, acknowledged: BTreeSet::new(), start_position: None,
        }).unwrap();
        store.post_system_notice(&channel, "task work", "mixed-task-notice").unwrap();
        dm(&mut store, &actor, &people, "old-personal");
        let all = store.wake_intents_for_session("b");
        assert!(all.iter().any(|i| i.monitor.is_some()) && all.iter().any(|i| i.monitor.is_none()));
        let mut q = Queue::default();
        q.dirty |= collect(&mut q, &all);
        checkpoint(&queue_path(&store.root, "b"), &mut q).unwrap();
        store.task_authority = Some(BTreeMap::from([(sub.clone(), super::super::task_authority::Authority::Deferred)]));
        (tmp, store, actor, people, sub, all)
    }

    #[test]
    fn deferred_mixed_split_is_durable_before_publication_and_retry_keeps_keys() {
        let (tmp, mut store, _, _, sub, all) = mixed_task_queue();
        let path = queue_path(&store.root, "b");
        let original = fs::read(&path).unwrap();
        let eligible = store.wake_intents_for_session("b");
        assert!(eligible.iter().all(|i| i.monitor.is_none()));
        FAIL_QUEUE_SAVE_AT.with(|at| at.set(Some(save_calls() + 1)));
        assert!(reconcile_intents(tmp.path(), &store, "b", &eligible).is_err());
        FAIL_QUEUE_SAVE_AT.with(|at| at.set(None));
        assert_eq!(original, fs::read(&path).unwrap(), "failed split must preserve original ledger");
        let old = load(&path).unwrap();
        assert!(native_event(tmp.path(), &old.batches[0]).is_none());

        let q = reconcile_intents(tmp.path(), &store, "b", &eligible).unwrap();
        assert_eq!(q.batches.len(), 2);
        let split_id = q.batches[1].wake_id.clone();
        let all_keys: BTreeSet<_> = all.iter().map(|i| i.key.clone()).collect();
        assert_eq!(q.batches.iter().flat_map(|b| b.ids.clone()).collect::<BTreeSet<_>>(), all_keys);
        assert!(q.batches[0].deferred(&q.deferred));
        assert!(native_event(tmp.path(), &q.batches[1]).is_none());
        // Crash/reload after the single split checkpoint, before publication.
        drop(q);
        let mut q = reconcile_intents(tmp.path(), &store, "b", &eligible).unwrap();
        assert_eq!(q.batches.len(), 2);
        assert_eq!(q.batches[1].wake_id, split_id);
        deliver_queue(tmp.path(), &store.root, "b", &eligible, &recipient("codex"), 1, &|id| preview(&store, id), &mut q).unwrap();
        assert!(native_event(tmp.path(), &q.batches[0]).is_none());
        let personal = native_event(tmp.path(), &q.batches[1]).unwrap();
        assert!(!personal["text"].as_str().unwrap().contains(&sub));
        let mut q = reconcile_intents(tmp.path(), &store, "b", &eligible).unwrap();
        deliver_queue(tmp.path(), &store.root, "b", &eligible, &recipient("codex"), 2, &|id| preview(&store, id), &mut q).unwrap();
        assert_eq!(native_event(tmp.path(), &q.batches[1]).unwrap(), personal);
        store.task_authority = None; // Existing Store-only/scheduler contract.
        let mut q = reconcile_intents(tmp.path(), &store, "b", &all).unwrap();
        deliver_queue(tmp.path(), &store.root, "b", &all, &recipient("codex"), 3, &|id| preview(&store, id), &mut q).unwrap();
        assert!(native_event(tmp.path(), &q.batches[0]).is_some());
        assert_eq!(native_event(tmp.path(), &q.batches[1]).unwrap(), personal);
        assert_eq!(q.batches.iter().flat_map(|b| b.ids.clone()).collect::<BTreeSet<_>>(), all_keys);
    }

    #[test]
    fn deferred_mixed_native_receipts_keep_identity_while_new_personal_work_bypasses() {
        for status in ["pending", "submitting", "observed"] {
            let (tmp, mut store, actor, people, _, all) = mixed_task_queue();
            let path = queue_path(&store.root, "b");
            let mut original = load(&path).unwrap();
            // Native publication won a crash race before its ledger checkpoint.
            let b = &original.batches[0];
            crate::notifications::publish(tmp.path(), "b", &format!("chat:{}", b.wake_id), "chat",
                &wake_text(b, &all, &no_preview), &format!("[cm-chat {}]", b.wake_id)).unwrap();
            set_native_status(tmp.path(), b, status);
            let receipt = native_event(tmp.path(), b).unwrap();
            let batch = serde_json::to_value(b).unwrap();
            dm(&mut store, &actor, &people, "new-independent");
            let eligible = store.wake_intents_for_session("b");
            original = reconcile_intents(tmp.path(), &store, "b", &eligible).unwrap();
            deliver_queue(tmp.path(), &store.root, "b", &eligible, &recipient("codex"), 1, &|id| preview(&store, id), &mut original).unwrap();
            assert_eq!(original.batches.len(), 2);
            assert_eq!(serde_json::to_value(&original.batches[0]).unwrap(), batch);
            assert_eq!(native_event(tmp.path(), &original.batches[0]).unwrap(), receipt);
            assert_eq!(original.batches[1].ids.len(), 1, "already published personal key must not be replayed");
            let fresh = native_event(tmp.path(), &original.batches[1]).unwrap();
            let mut q = reconcile_intents(tmp.path(), &store, "b", &eligible).unwrap();
            deliver_queue(tmp.path(), &store.root, "b", &eligible, &recipient("codex"), 2, &|id| preview(&store, id), &mut q).unwrap();
            assert_eq!(q.batches.len(), 2);
            assert_eq!(native_event(tmp.path(), &q.batches[1]).unwrap(), fresh);
        }
    }
    #[test]
    fn messaging_unchanged_live_tick_loads_once_and_writes_zero_queues() {
        let _env = crate::test_support::env_lock();
        let (tmp, mut store, actor, people) = fixture();
        dm(&mut store, &actor, &people, "tick-once");
        let path = queue_path(&store.root, "b");
        let mut daemon = DaemonState::default();
        daemon.messaging_root = tmp.path().into();
        *daemon.messaging.lock().unwrap() = Some(store);
        let mut params = crate::session::SpawnParams::new("b", "delivery-test", "/bin/cat");
        params.session_type = "codex".into();
        daemon.sessions.insert("b".into(), crate::session::DaemonSession::spawn(params).unwrap());
        let state = Arc::new(Mutex::new(daemon));
        let loads = queue_load_count();
        tick(&state);
        assert_eq!(queue_load_count() - loads, 1, "reconcile and deliver share a queue");
        let q = load(&path).unwrap();
        assert_eq!(q.batches[0].status, "native_pending");
        let bytes = fs::read(&path).unwrap();
        let native = native_event(tmp.path(), &q.batches[0]).unwrap();
        let (loads, writes) = (queue_load_count(), save_calls());
        tick(&state);
        assert_eq!(queue_load_count() - loads, 1);
        assert_eq!(save_calls() - writes, 0, "count writes, not merely equal saved content");
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(native_event(tmp.path(), &q.batches[0]).unwrap(), native);
    }

    /// 2026-10-16: Owner's DMs to an orchestrator waited 5 minutes for its
    /// restart and the log said nothing. A registered session whose output
    /// has ended gets one line naming why it is skipped, not one per tick.
    #[test]
    fn a_registered_session_with_closed_output_and_waiting_wakes_is_logged_once() {
        let uid = "skip-closed-output";
        let t = tempfile::tempdir().unwrap();
        let mut store = Store::open(t.path()).unwrap();
        let actor = store.participant_id(uid);
        let people = vec![super::super::Person {
            id: actor.clone(),
            name: "Stuck".into(),
            session_uid: uid.into(),
            kind: "agent".into(),
            engine: None,
            present: true,
            task: None,
        }];
        store.enroll_participants(&people).unwrap();
        dm(&mut store, &actor, &people, "are-you-there");
        let mut daemon = DaemonState::default();
        daemon.messaging_root = t.path().into();
        *daemon.messaging.lock().unwrap() = Some(store);
        let mut params = crate::session::SpawnParams::new(uid, "delivery-test", "/bin/true");
        params.session_type = "claude-code".into();
        let session = crate::session::DaemonSession::spawn(params).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !session.fanout.snapshot_since(None).closed {
            assert!(std::time::Instant::now() < deadline, "the child's output never ended");
            thread::sleep(Duration::from_millis(20));
        }
        daemon.sessions.insert(uid.into(), session);
        let state = Arc::new(Mutex::new(daemon));

        tick(&state);
        assert_eq!(
            SKIPPED.lock().unwrap().get(uid).copied(),
            Some(SKIP_OUTPUT_CLOSED),
            "the skip and its reason are recorded",
        );
        assert!(!note_skip(uid, Some(SKIP_OUTPUT_CLOSED), 1), "the next tick logs nothing new");
        let published = crate::notifications::directory(t.path(), uid)
            .read_dir()
            .map_or(0, |d| d.count());
        assert_eq!(published, 0, "nothing was published for a session that cannot read it");
        assert!(note_skip(uid, None, 0), "recovery is logged once");
        assert!(SKIPPED.lock().unwrap().get(uid).is_none());
    }

    #[test]
    fn messaging_large_settled_history_needs_no_native_io_or_queue_writes() {
        let (tmp, store, _, _) = fixture();
        let path = queue_path(&store.root, "b");
        let q = Queue {
            batches: (0..7745).map(|i| Batch {
                wake_id: format!("history-{i}"),
                ids: vec![format!("message-{i}")],
                status: if i % 2 == 0 { "confirmed" } else { "cancelled" }.into(),
                released: true,
                ..Batch::default()
            }).collect(),
            ..Queue::default()
        };
        save(&path, &q).unwrap();
        let bytes = fs::read(&path).unwrap();
        // Any native lookup/lock would fail: settled history must not touch it.
        fs::write(tmp.path().join("notifications"), "not a directory").unwrap();
        let (loads, writes) = (queue_load_count(), save_calls());
        let started = std::time::Instant::now();
        let mut q = reconcile_intents(tmp.path(), &store, "b", &[]).unwrap();
        deliver_queue(tmp.path(), &store.root, "b", &[], &recipient("codex"),
            1_000_000, &no_preview, &mut q).unwrap();
        eprintln!("P2 synthetic history: 7745 batches, {} bytes, {:.3} ms, one load, zero writes/native reads",
            bytes.len(), started.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(queue_load_count() - loads, 1);
        assert_eq!(save_calls() - writes, 0);
        assert_eq!(fs::read(&path).unwrap(), bytes, "all dedup history remains intact");
        assert_eq!(q.batches.len(), 7745);
    }

    #[test]
    fn messaging_settled_ids_becoming_eligible_again_do_not_rewake_or_read_native() {
        for status in ["confirmed", "cancelled"] {
            let (tmp, store, _, _) = fixture();
            let path = queue_path(&store.root, "b");
            save(&path, &Queue {
                batches: vec![Batch {
                    wake_id: "settled".into(), ids: vec!["eligible-again".into()],
                    status: status.into(), released: true, ..Batch::default()
                }], ..Queue::default()
            }).unwrap();
            fs::write(tmp.path().join("notifications"), "no native I/O allowed").unwrap();
            let intents = vec![WakeIntent {
                key: "eligible-again".into(), event_id: "eligible-again".into(), monitor: None,
            }];
            let writes = save_calls();
            let mut queue = reconcile_intents(tmp.path(), &store, "b", &intents).unwrap();
            deliver_queue(tmp.path(), &store.root, "b", &intents, &recipient("codex"),
                1_000_000, &no_preview, &mut queue).unwrap();
            assert_eq!(save_calls() - writes, 0);
            assert_eq!(queue.batches.len(), 1);
            assert_eq!(queue.batches[0].wake_id, "settled");
            assert_eq!(queue.batches[0].status, status);
            assert!(queue.batches[0].released);
        }
    }

    #[test]
    fn messaging_shared_queue_tick_serializes_cancel_against_native_claim() {
        use std::os::fd::AsRawFd;
        // Simulate the adapter's pending -> submitting transition under the
        // actual native queue flock, independently of the daemon delivery gate.
        fn claim(root: &Path, wake: &str) -> bool {
            let dir = crate::notifications::directory(root, "b");
            let lock = fs::OpenOptions::new().read(true).write(true).open(dir.join("queue.lock")).unwrap();
            assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
            let path = dir.join(format!("{}.json", digest(&format!("chat:{wake}"))));
            let mut event: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            if event["status"] != "pending" { return false; }
            event["status"] = json!("submitting");
            atomic_replace(&path, &event).unwrap();
            true
        }
        let _env = crate::test_support::env_lock();
        for claim_first in [true, false] {
            let (tmp, mut store, actor, people) = fixture();
            dm(&mut store, &actor, &people, "claim-race");
            let path = queue_path(&store.root, "b");
            let mut daemon = DaemonState::default();
            daemon.messaging_root = tmp.path().into();
            *daemon.messaging.lock().unwrap() = Some(store);
            let mut params = crate::session::SpawnParams::new("b", "delivery-race", "/bin/cat");
            params.session_type = "codex".into();
            daemon.sessions.insert("b".into(), crate::session::DaemonSession::spawn(params).unwrap());
            let gate = daemon.messaging_delivery.clone();
            let slot = daemon.messaging.clone();
            let state = Arc::new(Mutex::new(daemon));
            tick(&state);
            let queue = load(&path).unwrap();
            let wake = queue.batches[0].wake_id.clone();
            let (go, start) = std::sync::mpsc::channel();
            let (started, trying) = std::sync::mpsc::channel();
            let (done, cancelled) = std::sync::mpsc::channel();
            let root = tmp.path().to_owned();
            let canceller = std::thread::spawn(move || {
                start.recv().unwrap();
                started.send(()).unwrap();
                let _guard = gate.lock().unwrap();
                let mut slot = slot.lock().unwrap();
                let store = slot.as_mut().unwrap();
                store.follow(&actor, &json!({"action":"set","dnd":true,"request_id":"race-cancel"}), &people).unwrap();
                reconcile_session(&root, store, "b").unwrap();
                let _ = done.send(());
            });
            let root = tmp.path().to_owned();
            let hook_wake = wake.clone();
            AFTER_RECONCILE.with(|hook| *hook.borrow_mut() = Some(Box::new(move || {
                go.send(()).unwrap();
                trying.recv_timeout(Duration::from_secs(2)).unwrap();
                assert!(cancelled.recv_timeout(Duration::from_millis(50)).is_err(),
                    "cancellation must wait for the recipient gate through shared-queue delivery");
                if claim_first {
                    assert!(claim(&root, &hook_wake), "adapter can claim after reconcile before deliver");
                }
            })));
            let loads = queue_load_count();
            tick(&state);
            assert_eq!(queue_load_count() - loads, 1, "deliver must reuse reconcile's snapshot");
            canceller.join().unwrap();
            let final_queue = load(&path).unwrap();
            assert_eq!(final_queue.batches.len(), 1);
            assert!(final_queue.batches[0].released);
            if claim_first {
                assert_eq!(final_queue.batches[0].status, "submitting", "claimed native work cannot be recalled");
            } else {
                assert_eq!(final_queue.batches[0].status, "cancelled");
                assert!(!claim(tmp.path(), &wake), "cancelled work cannot be claimed later");
            }
        }
    }

    #[test]
    fn messaging_released_ambiguous_native_receipts_still_confirm_late() {
        for status in ["submitted", "uncertain"] {
            let (tmp, mut store, actor, people) = fixture();
            dm(&mut store, &actor, &people, "late");
            let mut q = deliver(&store, tmp.path(), "codex");
            set_native_status(tmp.path(), &q.batches[0], status);
            q.batches[0].status = native_status(&json!({"status":status})).into();
            q.batches[0].released = true;
            q.batches[0].delivered_at = 1;
            save(&queue_path(&store.root, "b"), &q).unwrap();
            let writes = save_calls();
            reconcile_session(tmp.path(), &store, "b").unwrap();
            deliver(&store, tmp.path(), "codex");
            assert_eq!(save_calls() - writes, 0, "unchanged ambiguous receipts do not rewrite");
            set_native_status(tmp.path(), &q.batches[0], "observed");
            let writes = save_calls();
            reconcile_session(tmp.path(), &store, "b").unwrap();
            let confirmed = deliver(&store, tmp.path(), "codex");
            assert_eq!(save_calls() - writes, 1);
            assert_eq!(confirmed.batches[0].status, "confirmed");
            assert!(confirmed.batches[0].released);
        }
    }

    #[test]
    fn messaging_marker_scan_progress_is_checkpointed_without_status_change() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        fs::write(&transcript, "{}\n".repeat(700_000)).unwrap();
        let mut r = recipient("codex");
        r.binding = marker::binding(transcript.to_str(), 1);
        let path = queue_path(tmp.path(), "b");
        save(&path, &Queue {
            batches: vec![Batch {
                wake_id: "scan-progress".into(), ids: vec!["old".into()],
                status: "uncertain".into(), released: true,
                original: r.binding.clone(), ..Batch::default()
            }], ..Queue::default()
        }).unwrap();
        let writes = save_calls();
        deliver_intents(tmp.path(), tmp.path(), "b", vec![], &r).unwrap();
        let first = load(&path).unwrap();
        assert_eq!(save_calls() - writes, 1);
        let offset = serde_json::to_value(&first.batches[0].scan).unwrap()["offset"].as_u64().unwrap();
        assert!(offset > 0 && offset < fs::metadata(&transcript).unwrap().len());
        deliver_intents(tmp.path(), tmp.path(), "b", vec![], &r).unwrap();
        let scanned = load(&path).unwrap();
        assert_eq!(scanned.batches[0].status, "uncertain");
        assert_eq!(serde_json::to_value(&scanned.batches[0].scan).unwrap()["offset"], 2_100_000);
        let writes = save_calls();
        deliver_intents(tmp.path(), tmp.path(), "b", vec![], &r).unwrap();
        assert_eq!(save_calls() - writes, 0);
        writeln!(fs::OpenOptions::new().append(true).open(&transcript).unwrap(),
            "{}", json!({"type":"event_msg","payload":{"type":"user_message","message":"[cm-chat scan-progress]"}})).unwrap();
        // Changed transcript resets the scan, then the next bounded pass sees
        // the late inbound marker beyond the first 2 MiB.
        deliver_intents(tmp.path(), tmp.path(), "b", vec![], &r).unwrap();
        deliver_intents(tmp.path(), tmp.path(), "b", vec![], &r).unwrap();
        assert_eq!(load(&path).unwrap().batches[0].status, "confirmed");
        assert!(crate::notifications::get(tmp.path(), "b", "chat:scan-progress").unwrap().is_none());
    }

    #[test]
    fn messaging_checkpoint_failures_recover_without_duplicate_publication() {
        for checkpoint_number in [1, 2] {
            let (tmp, mut store, actor, people) = fixture();
            dm(&mut store, &actor, &people, "durable");
            let path = queue_path(&store.root, "b");
            FAIL_QUEUE_SAVE_AT.with(|at| at.set(Some(save_calls() + checkpoint_number)));
            let result = deliver_intents(tmp.path(), &store.root, "b",
                store.wake_intents_for_session("b"), &recipient("codex"));
            FAIL_QUEUE_SAVE_AT.with(|at| at.set(None));
            assert!(result.is_err());
            let before = load(&path).unwrap();
            let events = crate::notifications::snapshot(tmp.path(), "b").unwrap().0;
            if checkpoint_number == 1 {
                assert!(before.batches.is_empty());
                assert!(events.is_empty(), "a failed initial checkpoint must not publish");
            } else {
                assert_eq!(before.batches[0].status, "pending");
                assert_eq!(events.len(), 1, "publication precedes the receipt checkpoint");
            }
            drop(store);
            let store = Store::open(tmp.path()).unwrap();
            let intents = store.wake_intents_for_session("b");
            let mut q = reconcile_intents(tmp.path(), &store, "b", &intents).unwrap();
            deliver_queue(tmp.path(), &store.root, "b", &intents, &recipient("codex"),
                1_000_000, &no_preview, &mut q).unwrap();
            assert_eq!(q.batches.len(), 1);
            assert_eq!(q.batches[0].status, "native_pending");
            if checkpoint_number == 2 {
                assert_eq!(q.batches[0].wake_id, before.batches[0].wake_id);
            }
            assert_eq!(crate::notifications::snapshot(tmp.path(), "b").unwrap().0.len(), 1);
            let writes = save_calls();
            reconcile_session(tmp.path(), &store, "b").unwrap();
            deliver(&store, tmp.path(), "codex");
            assert_eq!(save_calls() - writes, 0);
        }
    }

    #[test]
    fn messaging_legacy_migration_and_repeated_read_checkpoint_only_changes() {
        let (tmp, mut store, actor, people) = fixture();
        dm(&mut store, &actor, &people, "legacy");
        let q = deliver(&store, tmp.path(), "codex");
        let path = queue_path(&store.root, "b");
        set_native_status(tmp.path(), &q.batches[0], "observed");
        let mut old = serde_json::to_value(&q).unwrap();
        old["batches"][0]["status"] = json!("confirmed");
        old["batches"][0].as_object_mut().unwrap().remove("released");
        atomic_replace(&path, &old).unwrap();
        let writes = save_calls();
        reconcile_session(tmp.path(), &store, "b").unwrap();
        assert_eq!(save_calls() - writes, 1, "persist the legacy migration even without other changes");
        assert!(load(&path).unwrap().batches[0].released);
        let writes = save_calls();
        reconcile_session(tmp.path(), &store, "b").unwrap();
        assert_eq!(save_calls() - writes, 0);
        let page = read_page(&mut store, &actor, &json!({"dm":"owner"}));
        let writes = save_calls();
        read_boundary(&store, "b", &page).unwrap();
        assert_eq!(save_calls() - writes, 0, "a repeated full-message fetch adds no persistent change");
    }

    #[test]
    fn messaging_legacy_unsubmitted_work_stays_live_and_coalesces_after_upgrade() {
        for initial_engine in ["codex", "bash"] {
            let (t, mut store, actor, people) = fixture();
            dm(&mut store, &actor, &people, "one");
            let q = deliver(&store, t.path(), initial_engine);
            let mut old = serde_json::to_value(&q).unwrap();
            for key in ["checked", "sealed", "released", "coalesced"] {
                old["batches"][0].as_object_mut().unwrap().remove(key);
            }
            atomic_replace(&queue_path(&store.root, "b"), &old).unwrap();
            reconcile(t.path(), &store).unwrap();
            dm(&mut store, &actor, &people, "two");
            let q = deliver(&store, t.path(), "codex");
            assert_eq!(q.batches.len(), 1);
            assert_eq!(q.batches[0].ids.len(), 2);
            assert!(!q.batches[0].released);
            assert_eq!(
                native_event(t.path(), &q.batches[0]).unwrap()["status"],
                "pending"
            );
        }
    }
    #[test]
    fn messaging_legacy_receipts_do_not_block_new_wakes_or_replay_old_messages() {
        let (t, mut store, actor, people) = fixture();
        let one = dm(&mut store, &actor, &people, "one");
        let q = deliver(&store, t.path(), "codex");
        set_native_status(t.path(), &q.batches[0], "observed");
        let mut old = serde_json::to_value(&q).unwrap();
        old["batches"][0]["status"] = json!("confirmed");
        for batch in old["batches"].as_array_mut().unwrap() {
            for key in ["checked", "sealed", "released", "coalesced"] {
                batch.as_object_mut().unwrap().remove(key);
            }
        }
        atomic_replace(&queue_path(&store.root, "b"), &old).unwrap();
        drop(store);
        let mut store = Store::open(t.path()).unwrap();
        let two = dm(&mut store, &actor, &people, "two");
        let q = deliver(&store, t.path(), "codex");
        assert_eq!(q.batches.len(), 2);
        assert!(q.batches[0].released);
        assert_eq!(q.batches[0].ids, vec![one]);
        assert_eq!(
            native_event(t.path(), &q.batches[0]).unwrap()["status"],
            "observed"
        );
        assert_eq!(q.batches[1].ids, vec![two]);
        assert_eq!(
            native_event(t.path(), &q.batches[1]).unwrap()["status"],
            "pending"
        );
    }
    #[test]
    fn messaging_delivered_but_unread_wake_stops_latching_after_horizon() {
        for state in ["observed", "submitted", "uncertain"] {
            let (t, mut store, actor, people) = fixture();
            let one = dm(&mut store, &actor, &people, "one");
            let first = deliver(&store, t.path(), "codex");
            set_native_status(t.path(), &first.batches[0], state);
            let t0 = 1_000_000u64;
            // Status is picked up and the delivery time stamped.
            deliver_intents_at(t.path(), &store.root, "b", store.wake_intents().remove("b").unwrap_or_default(), &recipient("codex"), t0).unwrap();
            let q = load(&queue_path(&store.root, "b")).unwrap();
            assert_eq!(q.batches[0].delivered_at, t0, "{state}");
            // Inside the horizon a new arrival still shares the outstanding wake.
            let two = dm(&mut store, &actor, &people, "two");
            deliver_intents_at(t.path(), &store.root, "b", store.wake_intents().remove("b").unwrap_or_default(), &recipient("codex"), t0 + 30).unwrap();
            let q = load(&queue_path(&store.root, "b")).unwrap();
            assert_eq!(q.batches.len(), 1, "{state}");
            assert!(q.batches[0].ids.contains(&two));
            // Past the horizon the unread batch releases and the next arrival
            // publishes a fresh native wake.
            let three = dm(&mut store, &actor, &people, "three");
            deliver_intents_at(t.path(), &store.root, "b", store.wake_intents().remove("b").unwrap_or_default(), &recipient("codex"), t0 + LATCH_HORIZON_SECS + 1).unwrap();
            let q = load(&queue_path(&store.root, "b")).unwrap();
            assert_eq!(q.batches.len(), 2, "{state}");
            assert!(q.batches[0].released && q.batches[0].sealed);
            // The arrival absorbed after delivery moves to the successor.
            assert_eq!(q.batches[0].ids, vec![one.clone()]);
            assert_eq!(q.batches[1].ids, vec![two.clone(), three]);
            assert_eq!(native_event(t.path(), &q.batches[1]).unwrap()["status"], "pending");
        }
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
                engine: None,
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
    fn messaging_scoped_reconciliation_ignores_unrelated_history_and_preserves_cancellation() {
        let (tmp, mut store, actor, people) = fixture();
        dm(&mut store, &actor, &people, "scoped-active");
        let q = deliver(&store, tmp.path(), "codex");
        let id = format!("chat:{}", q.batches[0].wake_id);
        let other = store.participant_id("offline");
        let offline = super::super::Person {
            id: other.clone(), name: "Offline".into(), session_uid: "offline".into(),
            kind: "agent".into(), present: false, task: None,
            engine: None,
        };
        store.enroll_participants(&[offline.clone()]).unwrap();
        dm(&mut store, &other, &[offline], "offline-pending");
        let path = queue_path(&store.root, "offline");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"unrelated corrupt historical ledger").unwrap();
        store.follow(&actor, &json!({"action":"set","dnd":true,"request_id":"scoped-mute"}), &people).unwrap();
        reconcile_session(tmp.path(), &store, "b").unwrap();
        assert_eq!(crate::notifications::get(tmp.path(), "b", &id).unwrap().unwrap()["status"], "cancelled");
        assert_eq!(fs::read(&path).unwrap(), b"unrelated corrupt historical ledger");
        assert_eq!(store.wake_intents_for_session("offline").len(), 1, "offline messages stay available on resume");
    }

    #[test]
    fn messaging_delivery_tick_does_not_reconcile_sessionless_history() {
        let (tmp, mut store, actor, people) = fixture();
        dm(&mut store, &actor, &people, "offline-tick");
        let q = deliver(&store, tmp.path(), "codex");
        let path = queue_path(&store.root, "b");
        store.follow(&actor, &json!({"action":"set","dnd":true,"request_id":"offline-mute"}), &people).unwrap();
        let before = fs::read(&path).unwrap();
        let mut daemon = DaemonState::default();
        daemon.messaging_root = store.root.clone();
        *daemon.messaging.lock().unwrap() = Some(store);
        let state = Arc::new(Mutex::new(daemon));
        tick(&state);
        assert_eq!(fs::read(&path).unwrap(), before, "background delivery visits only live sessions");
        assert_eq!(native_event(tmp.path(), &q.batches[0]).unwrap()["status"], "pending");
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
    #[test]
    fn messaging_wake_text_names_sender_place_excerpt_and_count_within_budget() {
        let (t, mut store, actor, people) = fixture();
        dm(&mut store, &actor, &people, "first message");
        let long = format!("rlso: A5 readmit FAILED {}\nsecond line must not appear", "x".repeat(300));
        store
            .send("owner", "", "owner",
                &json!({"channel":"general","mentions":[actor],"body":long,"request_id":"long"}), &people)
            .unwrap();
        let intents = store.wake_intents().remove("b").unwrap();
        let lookup = |id: &str| preview(&store, id);
        deliver_intents_previewed(t.path(), &store.root, "b", intents, &recipient("claude-code"), 1, &lookup).unwrap();
        let q = load(&queue_path(&store.root, "b")).unwrap();
        let text = native_event(t.path(), &q.batches[0]).unwrap()["text"].as_str().unwrap().to_owned();
        assert!(text.starts_with(&format!("[cm-chat {}] 2 new: #general — Owner: \"rlso: A5 readmit FAILED", q.batches[0].wake_id)), "{text}");
        assert!(text.contains("(+1 more)") && text.contains("view=\"slim\""), "{text}");
        assert!(text.contains('…') && !text.contains("second line"), "{text}");
        assert!(text.chars().count() <= WAKE_TEXT_BUDGET, "{} chars: {text}", text.chars().count());
    }
    #[test]
    fn messaging_wake_text_falls_back_without_preview_and_keeps_watch_ids() {
        let (_t, mut store, actor, people) = fixture();
        let m = store
            .register_monitor(&actor, &json!({"scope":{"channel":"general"},"request_id":"watch"}), &people)
            .unwrap();
        dm(&mut store, &actor, &people, "one");
        store
            .send("owner", "", "owner", &json!({"channel":"general","body":"watched","request_id":"w"}), &people)
            .unwrap();
        let intents = store.wake_intents().remove("b").unwrap();
        let batch = Batch {
            wake_id: "w1".into(),
            ids: intents.iter().map(|i| i.key.clone()).collect(),
            ..Batch::default()
        };
        let text = wake_text(&batch, &intents, &no_preview);
        assert!(text.starts_with("[cm-chat w1] 2 new chat messages."), "{text}");
        assert!(text.contains(m["id"].as_str().unwrap()), "{text}");
        let with = wake_text(&batch, &intents, &|id: &str| preview(&store, id));
        assert!(with.contains("#general — Owner: \"watched\""), "{with}");
        assert!(with.contains(m["id"].as_str().unwrap()), "{with}");
        // A fully fetched batch keeps the marker but claims nothing new.
        let mut read = batch;
        read.checked = read.ids.iter().cloned().collect();
        assert!(wake_text(&read, &intents, &no_preview).starts_with("[cm-chat w1] New chat activity."));
    }
}
