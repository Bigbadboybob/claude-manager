//! Read the session-owned MCP monitor journal without mistaking missing data
//! for an empty obligation set. The producer lives in mcp_server/monitor_state.py.
use std::collections::BTreeMap;
use std::io::Read;
use std::io::{Seek, SeekFrom};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::drain::{Obligation, SessionObservation};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub recorded_at: f64,
    pub monitor_fingerprint: String,
    pub worker_fingerprint: String,
    pub notes: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Acknowledgment {
    pub received_at: f64,
    pub session_uid: String,
    pub run_seq: Option<u64>,
    pub fire_token: Option<String>,
}

#[derive(Default)]
pub struct MonitorEvidence {
    pub fingerprint: Option<String>,
    pub outstanding: Vec<Obligation>,
}

#[derive(Deserialize)]
struct Journal {
    schema_version: u32,
    #[serde(default)]
    coverage_version: u32,
    session_uid: String,
    revision: u64,
    producers: BTreeMap<String, Producer>,
}

#[derive(Deserialize)]
struct Producer {
    records: BTreeMap<String, Monitor>,
}

#[derive(Deserialize)]
struct Monitor {
    monitor_id: String,
    state: String,
    #[serde(default)]
    delivery_uncertain: bool,
}

pub fn load_monitors(uid: &str) -> MonitorEvidence {
    let mut evidence = MonitorEvidence::default();
    let load = || -> Result<(Vec<u8>, Journal), String> {
        if uid.is_empty()
            || uid.len() > 160
            || !uid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err("Invalid monitor owner UID.".into());
        }
        let path = crate::path::dot_cm_dir()
            .join("monitor-state")
            .join(format!("{uid}.json"));
        let file = std::fs::File::open(path)
            .map_err(|_| "No readable durable monitor journal for this session.")?;
        let mut bytes = Vec::new();
        file.take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read monitor journal.")?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err("Monitor journal exceeds size limit.".into());
        }
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid monitor journal.")?;
        if journal.schema_version != 1
            || journal.coverage_version != 1
            || journal.session_uid != uid
            || journal.revision == 0
            || journal.producers.is_empty()
        {
            return Err("Monitor journal has no valid producer coverage for this session.".into());
        }
        Ok((bytes, journal))
    };
    match load() {
        Ok((bytes, journal)) => {
            for producer in journal.producers.values() {
                for (id, monitor) in &producer.records {
                    if id != &monitor.monitor_id
                        || monitor.delivery_uncertain
                        || !matches!(
                            monitor.state.as_str(),
                            "delivered" | "cancelled" | "replaced"
                        )
                    {
                        evidence.outstanding.push(Obligation {
                            kind: "monitor_delivery",
                            session_uid: Some(uid.to_string()),
                            detail: format!("Monitor {id}: {}{}; inspect list_monitors, including retained producers.", monitor.state,
                                if monitor.delivery_uncertain { " (delivery still unverified)" } else { "" }),
                        });
                    }
                }
            }
            evidence.fingerprint = Some(format!("{:x}", Sha256::digest(&bytes)));
        }
        Err(detail) => evidence.outstanding.push(Obligation {
            kind: "completion_evidence_unavailable",
            session_uid: Some(uid.to_string()),
            detail,
        }),
    }
    evidence
}

pub fn worker_fingerprint(sessions: &[SessionObservation]) -> String {
    let mut workers: Vec<_> = sessions
        .iter()
        .filter(|s| !s.orchestrator)
        .map(|s| {
            // A reported worker may subsequently exit without invalidating its
            // report. New input/report, a new worker, or an unreported exit does.
            (
                s.session_uid.as_str(),
                s.reported_at,
                s.reported_at.is_none() && s.exited,
                s.failed,
            )
        })
        .collect();
    workers.sort_by(|a, b| a.0.cmp(b.0));
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&workers).unwrap_or_default())
    )
}

/// Include descendant producers: a worker's delayed notification can start
/// another turn even after its completion report, just like the root's.
pub fn load_session_monitors(
    root: Option<&str>,
    sessions: &[SessionObservation],
) -> MonitorEvidence {
    let mut owners = std::collections::BTreeSet::new();
    owners.extend(root);
    owners.extend(sessions.iter().map(|s| s.session_uid.as_str()));
    if owners.is_empty() {
        return MonitorEvidence::default();
    }
    let mut evidence = MonitorEvidence::default();
    let mut fingerprints = Vec::new();
    let mut covered = true;
    for uid in owners {
        let current = load_monitors(uid);
        covered &= current.fingerprint.is_some();
        fingerprints.push((uid, current.fingerprint));
        evidence.outstanding.extend(current.outstanding);
    }
    if covered {
        evidence.fingerprint = Some(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&fingerprints).unwrap())
        ));
    }
    evidence
}

/// Codex's explicit task_complete after the latest input/report. Bookkeeping
/// after completion is harmless; a new prompt, tool call, or partial JSON is
/// not. Bound the read and use the bound rollout path (including /compact).
pub fn codex_turn_finished_after(path: &str, after: f64) -> bool {
    let read = || -> Option<bool> {
        let mut file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(256 * 1024)))
            .ok()?;
        let mut bytes = Vec::new();
        file.take(256 * 1024 + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 256 * 1024 {
            return None;
        }
        let text = std::str::from_utf8(&bytes).ok()?;
        for line in text
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(200)
        {
            let record: serde_json::Value = serde_json::from_str(line).ok()?;
            let top = record.get("type")?.as_str()?;
            let kind = record
                .pointer("/payload/type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match (top, kind) {
                ("event_msg", "task_complete") => {
                    if record
                        .pointer("/payload/error")
                        .is_some_and(|error| !error.is_null())
                    {
                        return Some(false);
                    }
                    let stamp = record.get("timestamp")?.as_str()?;
                    if !stamp.ends_with('Z') {
                        return None;
                    }
                    let ms = crate::workflow::history::iso8601_to_ms(stamp)?;
                    return Some(ms as f64 >= (after * 1000.0).ceil());
                }
                ("event_msg", "token_count") | ("session_meta", _) => {}
                _ => return Some(false),
            }
        }
        None
    };
    read() == Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_completion_requires_explicit_final_event_after_report() {
        let path =
            std::env::temp_dir().join(format!("cm-drain-rollout-{}.jsonl", uuid::Uuid::new_v4()));
        let complete = r#"{"timestamp":"2026-09-06T22:00:00.500Z","type":"event_msg","payload":{"type":"task_complete"}}"#;
        let after = crate::workflow::history::iso8601_to_ms("2026-09-06T22:00:00.499Z").unwrap()
            as f64
            / 1000.0;
        for (suffix, expected) in [
            ("", true),
            (
                "\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\"}}\n",
                true,
            ),
            (
                "\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n",
                false,
            ),
            (
                "\n{\"type\":\"response_item\",\"payload\":{\"type\":\"function_call\"}}\n",
                false,
            ),
            ("\n{unfinished", false),
        ] {
            std::fs::write(&path, format!("{complete}{suffix}")).unwrap();
            assert_eq!(
                codex_turn_finished_after(path.to_str().unwrap(), after),
                expected,
                "{suffix}"
            );
            assert!(!codex_turn_finished_after(
                path.to_str().unwrap(),
                after + 1.0
            ));
        }
        std::fs::remove_file(&path).unwrap();
        assert!(!codex_turn_finished_after(path.to_str().unwrap(), after));
    }
}
