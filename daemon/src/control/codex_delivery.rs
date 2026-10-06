//! Submission evidence for one PTY delivery, never for the lifetime of a thread.
//! A resumed thread already has a nonempty rollout before its composer is ready.
use crate::state::DaemonState;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const READ_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn read(path: &Path) -> Option<Self> {
        let m = std::fs::metadata(path).ok()?;
        Some(Self {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            modified: m.modified().ok(),
        })
    }

    fn same_file(&self, other: &Self) -> bool {
        self.dev == other.dev && self.ino == other.ino
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Observation {
    Pending,
    Accepted,
    /// New runtime activity without an exact prompt receipt. Do not interrupt
    /// it or claim we confirmed this particular prompt.
    Active,
    Unknown,
    Gone,
}

pub(super) struct DeliveryEvidence {
    pid: libc::pid_t,
    generation: u64,
    since_ms: u64,
    pub started: Instant,
    baseline: Option<(PathBuf, FileStamp)>,
    last_scan: Option<(PathBuf, FileStamp)>,
    last_observation: Observation,
}

impl DeliveryEvidence {
    /// Called immediately before the body write, after startup/typing waits.
    /// Only copy registry fields while locked; filesystem I/O stays outside.
    pub fn capture(state: &Arc<Mutex<DaemonState>>, uid: &str, body: &str) -> Option<Self> {
        if body.trim().is_empty() || body.trim_start().starts_with('/') {
            return None; // Enter-only nudges and slash commands aren't user turns.
        }
        let (pid, generation, path) = {
            let state = state.lock().unwrap_or_else(|p| p.into_inner());
            let session = state.sessions.get(uid)?;
            if session.session_type != "codex" {
                return None;
            }
            (
                session.pid,
                session.generation,
                session.transcript_path.clone(),
            )
        };
        let baseline = path.and_then(|p| {
            let p = PathBuf::from(p);
            Some((p.clone(), FileStamp::read(&p)?))
        });
        Some(Self {
            pid,
            generation,
            since_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_millis() as u64,
            started: Instant::now(),
            baseline,
            last_scan: None,
            last_observation: Observation::Pending,
        })
    }

    pub fn observe(
        &mut self,
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        body: &str,
    ) -> Observation {
        let path = {
            let state = state.lock().unwrap_or_else(|p| p.into_inner());
            let Some(session) = state.sessions.get(uid) else {
                return Observation::Gone;
            };
            if session.pid != self.pid || session.generation != self.generation {
                return Observation::Gone; // A replacement using the same CM UID.
            }
            session.transcript_path.clone()
        };
        self.observe_path(path.as_deref().map(Path::new), body)
    }

    fn observe_path(&mut self, path: Option<&Path>, body: &str) -> Observation {
        let Some(path) = path else {
            return Observation::Pending;
        };
        let Some(stamp) = FileStamp::read(path) else {
            return if matches!(path.try_exists(), Ok(false)) {
                Observation::Pending
            } else {
                Observation::Unknown
            };
        };
        if self
            .last_scan
            .as_ref()
            .is_some_and(|(p, s)| p == path && s == &stamp)
        {
            return match self.last_observation {
                Observation::Active => Observation::Active,
                Observation::Accepted => Observation::Accepted,
                Observation::Unknown => Observation::Unknown,
                _ => Observation::Pending,
            };
        }
        let baseline = self
            .baseline
            .as_ref()
            .filter(|(p, s)| p == path && s.same_file(&stamp));
        if baseline.is_some_and(|(_, old)| stamp.len < old.len) {
            return Observation::Unknown;
        }
        let offset = baseline
            .map(|(_, s)| s.len)
            .unwrap_or(0)
            .max(stamp.len.saturating_sub(READ_LIMIT));
        let observation = self
            .read_since(path, offset, body)
            .unwrap_or(Observation::Unknown);
        self.last_scan = Some((path.to_path_buf(), stamp));
        self.last_observation = match observation {
            Observation::Active => Observation::Active,
            Observation::Accepted => Observation::Accepted,
            Observation::Unknown => Observation::Unknown,
            _ => Observation::Pending,
        };
        observation
    }

    fn read_since(&self, path: &Path, offset: u64, body: &str) -> Option<Observation> {
        let mut file = std::fs::File::open(path).ok()?;
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut bytes = Vec::new();
        file.take(READ_LIMIT).read_to_end(&mut bytes).ok()?;
        let mut active = false;
        // Only newline-terminated records are durable evidence. A cut-off
        // leading/trailing record cannot confirm anything.
        for line in bytes
            .split_inclusive(|c| *c == b'\n')
            .filter(|l| l.ends_with(b"\n"))
        {
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            let Some(stamp) = v["timestamp"]
                .as_str()
                .and_then(crate::workflow::history::iso8601_to_ms)
            else {
                continue;
            };
            if stamp < self.since_ms {
                continue;
            }
            let p = &v["payload"];
            match (v["type"].as_str(), p["type"].as_str()) {
                (Some("event_msg"), Some("user_message")) => {
                    if p["message"].as_str().is_some_and(|s| same_body(s, body)) {
                        return Some(Observation::Accepted);
                    }
                    active = true;
                }
                (Some("response_item"), Some("message")) if p["role"] == "user" => {
                    let text: String = p["content"]
                        .as_array()?
                        .iter()
                        .filter_map(|c| c["text"].as_str())
                        .collect();
                    if same_body(&text, body) {
                        return Some(Observation::Accepted);
                    }
                }
                (
                    Some("event_msg"),
                    Some("task_started" | "task_complete" | "agent_message" | "agent_reasoning"),
                ) => active = true,
                (
                    Some("response_item"),
                    Some("function_call" | "custom_tool_call" | "reasoning"),
                ) => active = true,
                _ => {}
            }
        }
        Some(if active {
            Observation::Active
        } else {
            Observation::Pending
        })
    }
}

fn same_body(a: &str, b: &str) -> bool {
    a.trim_end_matches(['\r', '\n']) == b.trim_end_matches(['\r', '\n'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn event(stamp: &str, kind: &str, text: &str) -> String {
        format!(
            "{}\n",
            serde_json::json!({"timestamp":stamp,"type":"event_msg","payload":{"type":kind,"message":text}})
        )
    }
    fn evidence(path: &Path) -> DeliveryEvidence {
        DeliveryEvidence {
            pid: 1,
            generation: 0,
            since_ms: crate::workflow::history::iso8601_to_ms("2026-09-12T06:45:00.000Z").unwrap(),
            started: Instant::now(),
            baseline: FileStamp::read(path).map(|s| (path.to_path_buf(), s)),
            last_scan: None,
            last_observation: Observation::Pending,
        }
    }

    #[test]
    fn resumed_nonempty_rollout_requires_new_exact_prompt() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("old.jsonl");
        std::fs::write(
            &p,
            event("2026-09-11T00:00:00.000Z", "user_message", "batch 407"),
        )
        .unwrap();
        let mut e = evidence(&p);
        assert_eq!(e.observe_path(Some(&p), "batch 407"), Observation::Pending);
        // Even newly appended imported history must not acknowledge this fire.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(event("2026-09-11T00:00:00.000Z", "user_message", "batch 407").as_bytes())
            .unwrap();
        assert_eq!(e.observe_path(Some(&p), "batch 407"), Observation::Pending);
        f.write_all(event("2026-09-12T06:45:01.000Z", "user_message", "batch 407").as_bytes())
            .unwrap();
        assert_eq!(
            e.observe_path(Some(&p), "batch 407\n"),
            Observation::Accepted
        );
    }

    #[test]
    fn rotated_rollout_ignores_old_history_and_accepts_new_large_prompt() {
        let d = tempfile::tempdir().unwrap();
        let old = d.path().join("old");
        let new = d.path().join("new");
        let body = "batch item\n".repeat(5000);
        let mut e = evidence(&old);
        std::fs::write(
            &new,
            event("2026-09-11T00:00:00.000Z", "user_message", &body),
        )
        .unwrap();
        assert_eq!(e.observe_path(Some(&new), &body), Observation::Pending);
        let v = serde_json::json!({"timestamp":"2026-09-12T06:45:02.000Z","type":"response_item",
            "payload":{"type":"message","role":"user","content":[{"type":"input_text","text":body}]}});
        std::fs::write(&new, format!("{v}\n")).unwrap();
        assert_eq!(e.observe_path(Some(&new), &body), Observation::Accepted);
    }

    #[test]
    fn startup_metadata_partial_records_and_unrelated_turns_are_not_receipts() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new");
        let mut e = evidence(&p);
        std::fs::write(&p, "{\"type\":\"session_meta\",\"payload\":{}}\n").unwrap();
        assert_eq!(e.observe_path(Some(&p), "batch"), Observation::Pending);
        let row = event("2026-09-12T06:45:03.000Z", "user_message", "batch");
        std::fs::write(&p, row.trim_end()).unwrap();
        assert_eq!(e.observe_path(Some(&p), "batch"), Observation::Pending);
        std::fs::write(&p, row).unwrap();
        assert_eq!(e.observe_path(Some(&p), "batch"), Observation::Accepted);
        std::fs::write(&p, event("2026-09-12T06:45:03.000Z", "task_started", "")).unwrap();
        assert_eq!(e.observe_path(Some(&p), "batch"), Observation::Active);
    }
}
