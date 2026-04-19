//! Tail `events.jsonl` for a workflow run.
//!
//! Agents talk to the workflow runner by calling the `workflow_transition` /
//! `workflow_done` MCP tools (see `mcp_server/server.py`), which append one JSON
//! object per line to `~/.cm/workflow-runs/<run-id>/events.jsonl`. This module
//! reads new lines since the last processed byte offset.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};

use serde::{Deserialize, Serialize};

use crate::workflow::run;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub ts: f64,
    pub run_id: String,
    pub role: String,
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Clone, Debug)]
pub enum EventKind {
    Transition { to: String, prompt: String },
    Done { reason: String },
    Unknown,
}

impl Event {
    pub fn kind(&self) -> EventKind {
        match self.tool.as_str() {
            "workflow_transition" => {
                let to = self
                    .args
                    .get("to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let prompt = self
                    .args
                    .get("prompt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                EventKind::Transition { to, prompt }
            }
            "workflow_done" => {
                let reason = self
                    .args
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                EventKind::Done { reason }
            }
            _ => EventKind::Unknown,
        }
    }
}

/// Read new events for `run_id` starting at `offset`. Returns the parsed events
/// plus the new byte offset to persist. Malformed lines are skipped silently
/// (they still advance the offset so we don't loop).
pub fn read_new(run_id: &str, offset: u64) -> (Vec<Event>, u64) {
    let path = run::events_path(run_id);
    let Ok(mut f) = File::open(&path) else {
        return (Vec::new(), offset);
    };
    let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if offset >= file_len {
        return (Vec::new(), offset);
    }
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return (Vec::new(), offset);
    }
    let mut reader = BufReader::new(f);
    let mut events = Vec::new();
    let mut consumed: u64 = 0;
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                consumed += n as u64;
                let line = buf.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(ev) = serde_json::from_str::<Event>(line) {
                    events.push(ev);
                }
            }
            Err(_) => break,
        }
    }
    (events, offset + consumed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let orig = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }
        f();
        if let Some(o) = orig {
            unsafe { std::env::set_var("HOME", o); }
        }
        tmp
    }

    #[test]
    fn reads_new_events_incrementally() {
        let _tmp = with_temp_home(|| {
            let run_id = "wf_evtest";
            let dir = run::run_dir(run_id);
            std::fs::create_dir_all(&dir).unwrap();
            let path = run::events_path(run_id);

            {
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .unwrap();
                writeln!(
                    f,
                    r#"{{"id":"a","ts":1.0,"run_id":"wf_evtest","role":"manager","tool":"workflow_transition","args":{{"to":"worker","prompt":"try again"}}}}"#
                )
                .unwrap();
            }

            let (events, offset) = read_new(run_id, 0);
            assert_eq!(events.len(), 1);
            match events[0].kind() {
                EventKind::Transition { to, prompt } => {
                    assert_eq!(to, "worker");
                    assert_eq!(prompt, "try again");
                }
                _ => panic!("expected transition"),
            }
            assert!(offset > 0);

            // Append a done event and confirm only the new one comes back.
            {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                writeln!(
                    f,
                    r#"{{"id":"b","ts":2.0,"run_id":"wf_evtest","role":"manager","tool":"workflow_done","args":{{"reason":"ok"}}}}"#
                )
                .unwrap();
            }

            let (events2, offset2) = read_new(run_id, offset);
            assert_eq!(events2.len(), 1);
            assert!(offset2 > offset);
            match events2[0].kind() {
                EventKind::Done { reason } => assert_eq!(reason, "ok"),
                _ => panic!("expected done"),
            }
        });
    }

    #[test]
    fn absent_file_is_noop() {
        let _tmp = with_temp_home(|| {
            let (events, offset) = read_new("wf_nonexistent", 0);
            assert!(events.is_empty());
            assert_eq!(offset, 0);
        });
    }
}
