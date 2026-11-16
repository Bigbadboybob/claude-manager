//! Bounded Owner notification decisions. Producers enqueue without disk I/O;
//! one writer checkpoints outside daemon/store locks. The feed lock is a leaf.
use crate::control::protocol::{ErrorCode, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Condvar, Mutex,
    },
    time::Duration,
};
const RETAIN: usize = 256;
const MAX_BYTES: u64 = 512 * 1024;
const QUEUE: usize = 256;

#[derive(Clone, Serialize, Deserialize)]
pub struct Notification {
    pub sequence: u64,
    pub created_at: i64,
    pub id: String,
    pub title: String,
    pub body: String,
    pub url: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    version: u32,
    generation: String,
    next: u64,
    events: VecDeque<Notification>,
}
impl Default for Record {
    fn default() -> Self {
        Self {
            version: 1,
            generation: uuid::Uuid::new_v4().to_string(),
            next: 1,
            events: VecDeque::new(),
        }
    }
}
#[derive(Default)]
struct Shared {
    inner: Mutex<Option<(PathBuf, Record)>>,
    changed: Condvar,
    queued: AtomicU64,
    dropped: AtomicU64,
    last_error: Mutex<Option<String>>,
    recovered: Mutex<Option<String>>,
}
enum Command {
    Publish(PathBuf, Notification),
    #[cfg(test)]
    Flush(mpsc::Sender<()>),
}
#[derive(Default)]
pub struct Feed {
    shared: Arc<Shared>,
    writer: Mutex<Option<mpsc::SyncSender<Command>>>,
}

/// Encode an opaque ID as one fragment/query component, including reserved
/// characters. Keep the URL structure in the producer, never in supplied IDs.
pub fn url_component(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

impl Shared {
    fn record<'a>(
        &self,
        slot: &'a mut Option<(PathBuf, Record)>,
        root: &Path,
    ) -> Result<&'a mut Record, String> {
        if slot.is_none() {
            let path = root.join("owner-notifications.json");
            let mut data = match std::fs::File::open(&path) {
                Ok(file) => {
                    let mut bytes = Vec::new();
                    file.take(MAX_BYTES + 1)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    let parsed = serde_json::from_slice::<Record>(&bytes).ok().filter(|r| {
                        r.version == 1
                            && uuid::Uuid::parse_str(&r.generation).is_ok()
                            && r.next > 0
                            && r.events.len() <= 1024
                            && r.events.iter().all(|e| {
                                e.sequence > 0
                                    && e.sequence < r.next
                                    && e.id.len() <= 1024
                                    && e.url.len() <= 4096
                                    && e.title.chars().count() <= 100
                                    && e.body.chars().count() <= 140
                            })
                            && r.events
                                .iter()
                                .zip(r.events.iter().skip(1))
                                .all(|(a, b)| a.sequence + 1 == b.sequence)
                            && r.events.back().is_none_or(|e| e.sequence == r.next - 1)
                    });
                    if bytes.len() <= MAX_BYTES as usize && parsed.is_some() {
                        parsed.unwrap()
                    } else {
                        let quarantine = root.join(format!(
                            "owner-notifications.json.corrupt-{}-{}",
                            chrono::Utc::now().timestamp(),
                            uuid::Uuid::new_v4()
                        ));
                        std::fs::rename(&path, &quarantine).map_err(|e| e.to_string())?;
                        let r = Record::default();
                        crate::state::write_json_atomic(
                            &path,
                            &serde_json::to_string(&r).unwrap(),
                            true,
                        )
                        .map_err(|e| e.to_string())?;
                        let message = format!(
                            "Recovered corrupt/oversize notification checkpoint: {}",
                            quarantine.display()
                        );
                        eprintln!("cm Owner notifications: {message}");
                        *self.recovered.lock().unwrap_or_else(|p| p.into_inner()) = Some(message);
                        r
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::default(),
                Err(e) => return Err(e.to_string()),
            };
            while data.events.len() > RETAIN {
                data.events.pop_front();
            }
            *slot = Some((root.to_path_buf(), data));
        }
        let (loaded_root, data) = slot.as_mut().unwrap();
        if loaded_root != root {
            return Err("Owner notification feed changed state root".into());
        }
        Ok(data)
    }
    fn persist(&self, root: &Path, mut event: Notification) -> Result<(), String> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let old = self.record(&mut guard, root)?;
        if old.events.iter().any(|e| e.id == event.id) {
            return Ok(());
        }
        let mut next = old.clone();
        event.sequence = next.next;
        next.next = next
            .next
            .checked_add(1)
            .ok_or("Owner notification sequence exhausted")?;
        next.events.push_back(event);
        while next.events.len() > RETAIN {
            next.events.pop_front();
        }
        let bytes = loop {
            let bytes = serde_json::to_string(&next).map_err(|e| e.to_string())?;
            if bytes.len() <= MAX_BYTES as usize {
                break bytes;
            }
            next.events.pop_front();
        };
        crate::state::write_json_atomic(&root.join("owner-notifications.json"), &bytes, true)
            .map_err(|e| e.to_string())?;
        *old = next;
        self.changed.notify_all();
        Ok(())
    }
}
impl Feed {
    pub fn publish(
        &self,
        root: &Path,
        id: String,
        title: &str,
        body: &str,
        url: String,
    ) -> Result<(), String> {
        if id.len() > 1024 || url.len() > 4096 {
            return Err("Oversize notification identity/link".into());
        }
        let event = Notification {
            sequence: 0,
            created_at: chrono::Utc::now().timestamp(),
            id,
            title: title.chars().take(100).collect(),
            body: body
                .lines()
                .map(str::trim)
                .find(|s| !s.is_empty())
                .unwrap_or("")
                .chars()
                .take(140)
                .collect(),
            url,
        };
        // This mutex only protects lazy sender creation and try_send, never
        // disk or the feed lock. Dropping the last Feed disconnects the worker.
        let mut sender = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        if sender.is_none() {
            let (tx, rx) = mpsc::sync_channel(QUEUE);
            let shared = self.shared.clone();
            std::thread::Builder::new()
                .name("cm-owner-feed".into())
                .spawn(move || {
                    while let Ok(command) = rx.recv() {
                        match command {
                            Command::Publish(root, event) => {
                                let result = shared.persist(&root, event);
                                if let Err(e) = &result {
                                    shared.dropped.fetch_add(1, Ordering::Relaxed);
                                    eprintln!("cm Owner notification checkpoint failed: {e}");
                                }
                                *shared.last_error.lock().unwrap_or_else(|p| p.into_inner()) =
                                    result.err();
                                shared.queued.fetch_sub(1, Ordering::Relaxed);
                            }
                            #[cfg(test)]
                            Command::Flush(done) => {
                                let _ = done.send(());
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            *sender = Some(tx);
        }
        self.shared.queued.fetch_add(1, Ordering::Relaxed);
        if sender
            .as_ref()
            .unwrap()
            .try_send(Command::Publish(root.into(), event))
            .is_err()
        {
            self.shared.queued.fetch_sub(1, Ordering::Relaxed);
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return Err("Owner notification queue unavailable/full; notification dropped".into());
        }
        Ok(())
    }
    /// No feed/disk lock: safe while daemon.health holds its registry lock.
    pub fn health(&self) -> Value {
        json!({"queued":self.shared.queued.load(Ordering::Relaxed),
            "dropped":self.shared.dropped.load(Ordering::Relaxed), "retained_limit":RETAIN,
            "last_error":*self.shared.last_error.lock().unwrap_or_else(|p|p.into_inner()),
            "recovery":*self.shared.recovered.lock().unwrap_or_else(|p|p.into_inner())})
    }
    pub fn next(&self, root: &Path, p: &Value) -> Result<Value, String> {
        let mut guard = self.shared.inner.lock().unwrap_or_else(|p| p.into_inner());
        let data = self.shared.record(&mut guard, root)?;
        let supplied = p["cursor"].as_object();
        let same = supplied.is_some() && p["cursor"]["generation"] == data.generation;
        let after = if same {
            p["cursor"]["sequence"]
                .as_u64()
                .ok_or("Invalid notification cursor")?
        } else {
            0
        };
        if after >= data.next {
            return Err("Notification cursor is ahead of the feed".into());
        }
        let generation = data.generation.clone();
        let wait = p["wait_ms"].as_u64().unwrap_or(25000).min(25000);
        if after == data.next - 1 && wait > 0 {
            let (g, _) = self
                .shared
                .changed
                .wait_timeout_while(guard, Duration::from_millis(wait), |g| {
                    g.as_ref().is_some_and(|(_, r)| r.next - 1 == after)
                })
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
        }
        let data = self.shared.record(&mut guard, root)?;
        let events: Vec<_> = data
            .events
            .iter()
            .filter(|e| e.sequence > after)
            .take(100)
            .cloned()
            .collect();
        let high = events.last().map_or(data.next - 1, |e| e.sequence);
        let lost = supplied.is_some()
            && (!same || data.events.front().is_some_and(|e| after + 1 < e.sequence));
        Ok(json!({"events":events,"cursor":{"generation":generation,"sequence":high},"lost":lost}))
    }
    #[cfg(test)]
    pub fn flush(&self) {
        let sender = self.writer.lock().unwrap().clone();
        if let Some(sender) = sender {
            let (tx, rx) = mpsc::channel();
            sender.send(Command::Flush(tx)).unwrap();
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
    }
}
pub fn next_rpc(state: &Arc<Mutex<crate::state::DaemonState>>, req: &Request) -> Response {
    let (feed, root) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.owner_notifications.clone(), s.messaging_root.clone())
    };
    match feed.next(&root, &req.params) {
        Ok(v) => Response::ok(req.id.clone(), v),
        Err(e) => {
            *feed
                .shared
                .last_error
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(e.clone());
            Response::err(req.id.clone(), ErrorCode::Internal, e)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn producer_never_waits_for_the_checkpoint_lock_and_replay_is_durable() {
        let dir = tempfile::tempdir().unwrap();
        let feed = Arc::new(Feed::default());
        let lock = feed.shared.inner.lock().unwrap();
        let f = feed.clone();
        let root = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        let producer = std::thread::spawn(move || {
            tx.send(f.publish(&root, "one".into(), "Agent", &"x".repeat(500), "/".into()))
                .unwrap()
        });
        rx.recv_timeout(Duration::from_millis(500))
            .expect("publisher waited for feed I/O lock")
            .unwrap();
        drop(lock);
        producer.join().unwrap();
        feed.flush();
        feed.publish(dir.path(), "one".into(), "Again", "retry", "/".into())
            .unwrap();
        feed.flush();
        let page = feed.next(dir.path(), &json!({"wait_ms":0})).unwrap();
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        assert_eq!(page["events"][0]["body"].as_str().unwrap().len(), 140);
        assert_eq!(
            Feed::default()
                .next(dir.path(), &json!({"wait_ms":0}))
                .unwrap(),
            page
        );
        std::fs::remove_file(dir.path().join("owner-notifications.json")).unwrap();
        std::fs::create_dir(dir.path().join("owner-notifications.json")).unwrap();
        feed.publish(dir.path(), "two".into(), "x", "x", "/".into())
            .unwrap();
        feed.flush();
        assert_eq!(feed.next(dir.path(), &json!({"wait_ms":0})).unwrap(), page);
        assert_eq!(feed.health()["dropped"], 1);
        assert!(feed.health()["last_error"].is_string());
    }
    #[test]
    fn corrupt_and_oversize_checkpoints_recover_with_a_new_generation_and_health_evidence() {
        for bytes in [
            b"broken".to_vec(),
            vec![b' '; MAX_BYTES as usize + 1],
            b"{\"version\":2}".to_vec(),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("owner-notifications.json"), &bytes).unwrap();
            let feed = Feed::default();
            let page = feed
                .next(
                    dir.path(),
                    &json!({"wait_ms":0,"cursor":{"generation":"old","sequence":9}}),
                )
                .unwrap();
            assert_eq!(page["lost"], true);
            assert_eq!(page["events"], json!([]));
            assert!(feed.health()["recovery"]
                .as_str()
                .unwrap()
                .contains(".corrupt-"));
            let quarantine = std::fs::read_dir(dir.path())
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".corrupt-")
                })
                .unwrap();
            assert_eq!(std::fs::read(quarantine).unwrap(), bytes);
            feed.publish(dir.path(), "recovered".into(), "x", "y", "/".into())
                .unwrap();
            feed.flush();
            assert_eq!(
                feed.next(dir.path(), &json!({"wait_ms":0})).unwrap()["events"][0]["id"],
                "recovered"
            );
        }
        assert_eq!(url_component("id:/a&b #✓"), "id%3A%2Fa%26b%20%23%E2%9C%93");
    }
    #[test]
    fn wait_wakes_and_bounded_replay_reports_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let feed = Arc::new(Feed::default());
        let cursor = feed.next(dir.path(), &json!({"wait_ms":0})).unwrap()["cursor"].clone();
        let f = feed.clone();
        let root = dir.path().to_path_buf();
        let c = cursor.clone();
        let reader =
            std::thread::spawn(move || f.next(&root, &json!({"cursor":c,"wait_ms":2000})).unwrap());
        feed.publish(dir.path(), "new".into(), "x", "y", "/".into())
            .unwrap();
        feed.flush();
        assert_eq!(reader.join().unwrap()["events"][0]["id"], "new");
        {
            let mut g = feed.shared.inner.lock().unwrap();
            let r = feed.shared.record(&mut g, dir.path()).unwrap();
            r.events[0].sequence = 10;
            r.next = 11;
        }
        assert_eq!(
            feed.next(dir.path(), &json!({"cursor":cursor,"wait_ms":0}))
                .unwrap()["lost"],
            true
        );
    }
}
