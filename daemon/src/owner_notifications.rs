//! Durable, bounded Owner notification decisions for Web Push consumers.
//! Producers call this only after their existing delivery/availability gate.
//! Long-poll waits hold neither the daemon state lock nor the messaging lock.
use crate::control::protocol::{ErrorCode, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex},
    time::Duration,
};

const RETAIN: usize = 1024;

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
pub struct Feed {
    inner: Mutex<Option<(PathBuf, Record)>>,
    changed: Condvar,
}

fn record<'a>(
    slot: &'a mut Option<(PathBuf, Record)>,
    root: &Path,
) -> Result<&'a mut Record, String> {
    if slot.is_none() {
        let path = root.join("owner-notifications.json");
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Record>(&bytes).map_err(|e| e.to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Record::default(),
            Err(e) => return Err(e.to_string()),
        };
        if data.version != 1 || data.next == 0 || data.events.len() > RETAIN {
            return Err("Invalid Owner notification checkpoint".into());
        }
        *slot = Some((root.to_path_buf(), data));
    }
    let (loaded_root, data) = slot.as_mut().unwrap();
    if loaded_root != root {
        return Err("Owner notification feed changed state root".into());
    }
    Ok(data)
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
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let old = record(&mut guard, root)?;
        if old.events.iter().any(|e| e.id == id) {
            return Ok(());
        }
        let mut next = old.clone();
        let event = Notification {
            sequence: next.next,
            id,
            created_at: chrono::Utc::now().timestamp(),
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
        next.next = next
            .next
            .checked_add(1)
            .ok_or("Owner notification sequence exhausted")?;
        next.events.push_back(event);
        while next.events.len() > RETAIN {
            next.events.pop_front();
        }
        crate::state::write_json_atomic(
            &root.join("owner-notifications.json"),
            &serde_json::to_string(&next).map_err(|e| e.to_string())?,
            true,
        )
        .map_err(|e| e.to_string())?;
        *old = next;
        self.changed.notify_all();
        Ok(())
    }

    pub fn next(&self, root: &Path, p: &Value) -> Result<Value, String> {
        let mut guard = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let data = record(&mut guard, root)?;
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
                .changed
                .wait_timeout_while(guard, Duration::from_millis(wait), |g| {
                    g.as_ref().is_some_and(|(_, r)| r.next - 1 == after)
                })
                .unwrap_or_else(|p| p.into_inner());
            guard = g;
        }
        let data = record(&mut guard, root)?;
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
}

pub fn next_rpc(
    state: &std::sync::Arc<Mutex<crate::state::DaemonState>>,
    req: &Request,
) -> Response {
    let (feed, root) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.owner_notifications.clone(), s.messaging_root.clone())
    };
    match feed.next(&root, &req.params) {
        Ok(v) => Response::ok(req.id.clone(), v),
        Err(e) => Response::err(req.id.clone(), ErrorCode::Internal, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkpoint_replays_deduplicates_and_clips_without_advancing_after_failure() {
        let dir = tempfile::tempdir().unwrap();
        let feed = Feed::default();
        feed.publish(
            dir.path(),
            "one".into(),
            "Agent",
            &"x".repeat(500),
            "/#alerts".into(),
        )
        .unwrap();
        feed.publish(
            dir.path(),
            "one".into(),
            "Agent",
            "retry",
            "/#alerts".into(),
        )
        .unwrap();
        let page = feed.next(dir.path(), &json!({"wait_ms":0})).unwrap();
        assert_eq!(page["events"].as_array().unwrap().len(), 1);
        assert_eq!(page["events"][0]["body"].as_str().unwrap().len(), 140);
        let reopened = Feed::default();
        assert_eq!(
            reopened.next(dir.path(), &json!({"wait_ms":0})).unwrap(),
            page
        );
        assert!(feed
            .next(dir.path(), &json!({"wait_ms":0,"cursor":page["cursor"]}))
            .unwrap()["events"]
            .as_array()
            .unwrap()
            .is_empty());
        std::fs::remove_file(dir.path().join("owner-notifications.json")).unwrap();
        std::fs::create_dir(dir.path().join("owner-notifications.json")).unwrap();
        assert!(feed
            .publish(dir.path(), "two".into(), "x", "x", "/".into())
            .is_err());
        assert_eq!(feed.next(dir.path(), &json!({"wait_ms":0})).unwrap(), page);
    }

    #[test]
    fn bounded_replay_reports_overflow_and_wait_wakes_on_publication() {
        let dir = tempfile::tempdir().unwrap();
        let feed = std::sync::Arc::new(Feed::default());
        let cursor = feed.next(dir.path(), &json!({"wait_ms":0})).unwrap()["cursor"].clone();
        let f = feed.clone();
        let root = dir.path().to_path_buf();
        let c = cursor.clone();
        let waiting =
            std::thread::spawn(move || f.next(&root, &json!({"cursor":c,"wait_ms":2000})).unwrap());
        feed.publish(dir.path(), "new".into(), "x", "y", "/".into())
            .unwrap();
        assert_eq!(waiting.join().unwrap()["events"][0]["id"], "new");
        // Exercise retention without 1024 fsyncs in a unit test.
        {
            let mut g = feed.inner.lock().unwrap();
            let r = record(&mut g, dir.path()).unwrap();
            r.events[0].sequence = 10;
            r.next = 11;
        }
        let page = feed
            .next(dir.path(), &json!({"cursor":cursor,"wait_ms":0}))
            .unwrap();
        assert_eq!(page["lost"], true);
        assert_eq!(page["events"][0]["sequence"], 10);
    }
}
