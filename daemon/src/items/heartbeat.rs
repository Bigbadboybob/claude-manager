//! Holder-state heartbeat and board push delivery (doc/items-board.md §3, §5).
//!
//! Every 30 s, or 2 s after `poke()`, the daemon posts a full snapshot of its
//! live sessions to `POST /hosts/{daemon_id}/heartbeat`. Durations are
//! relative so the API's clock decides. The reply carries the board pushes
//! addressed to this daemon's sessions; each is delivered through the native
//! notification queue (or Owner escalation) and acked on the next beat.

use super::api::Api;
use crate::owner_attention::{self, Escalation};
use crate::owner_availability::Urgency;
use crate::state::DaemonState;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

const INTERVAL: Duration = Duration::from_secs(30);
const DEBOUNCE: Duration = Duration::from_secs(2);

fn wake() -> &'static (Mutex<bool>, Condvar) {
    static WAKE: OnceLock<(Mutex<bool>, Condvar)> = OnceLock::new();
    WAKE.get_or_init(|| (Mutex::new(false), Condvar::new()))
}

/// Ask for an early beat (session exit, `report_done`, state change).
/// Debounced: pokes within 2 s share one beat.
pub fn poke() {
    let (flag, cv) = wake();
    *flag.lock().unwrap_or_else(|p| p.into_inner()) = true;
    cv.notify_one();
}

pub fn start(state: &Arc<Mutex<DaemonState>>) {
    let state = Arc::clone(state);
    let spawned = std::thread::Builder::new()
        .name("items-heartbeat".into())
        .spawn(move || {
            let mut beat = Beat::default();
            loop {
                {
                    let (flag, cv) = wake();
                    let guard = flag.lock().unwrap_or_else(|p| p.into_inner());
                    let (mut guard, _) = cv
                        .wait_timeout_while(guard, INTERVAL, |poked| !*poked)
                        .unwrap_or_else(|p| p.into_inner());
                    let poked = *guard;
                    *guard = false;
                    drop(guard);
                    if poked {
                        std::thread::sleep(DEBOUNCE);
                        *flag.lock().unwrap_or_else(|p| p.into_inner()) = false;
                    }
                }
                beat.run(&state);
            }
        });
    if let Err(e) = spawned {
        eprintln!("cm items: heartbeat thread failed to start: {e}");
    }
}

/// What one beat reads from the daemon, captured under the lock.
pub(crate) struct Snapshot {
    pub daemon_id: String,
    pub host_label: Option<String>,
    pub sessions: Vec<Value>,
    pub api: Option<Api>,
    pub root: PathBuf,
}

/// At most `limit` characters (the API clips too; this keeps rows small).
fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// Session rows in the heartbeat wire shape. `now` is Unix seconds.
pub(crate) fn session_row(
    daemon_id: &str,
    uid: &str,
    name: &str,
    task_id: Option<&str>,
    engine: &str,
    st: &crate::agent_state::AgentState,
    reported_done: bool,
    now: f64,
) -> Value {
    let state = st.state.as_str();
    let age = (now - st.since).max(0.0);
    json!({
        "pid": format!("agent:{daemon_id}:{uid}"),
        "session_uid": uid,
        "task_id": task_id,
        "name": clip(name, 200),
        "engine": clip(engine, 40),
        "state": state,
        "state_age_s": age,
        "idle_for_s": if state == "idle" { json!(age) } else { Value::Null },
        "reported_done": reported_done,
        "agent_state": st,
    })
}

fn capture(state: &Arc<Mutex<DaemonState>>) -> Option<Snapshot> {
    let handle = state.lock().unwrap_or_else(|p| p.into_inner()).messaging.clone();
    let daemon_id = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref()?.daemon_id.clone()
    };
    let s = state.lock().unwrap_or_else(|p| p.into_inner());
    let now = crate::agent_state::unix_now();
    let mut sessions: Vec<Value> = s
        .sessions
        .iter()
        .map(|(uid, sess)| {
            let st = crate::agent_state::current(sess);
            let name = s
                .messaging_names
                .get(uid)
                .map(|n| n.name.clone())
                .unwrap_or_else(|| sess.title.clone());
            session_row(
                &daemon_id,
                uid,
                &name,
                sess.task_id.as_deref(),
                &sess.session_type,
                &st,
                sess.reported_done().is_some(),
                now,
            )
        })
        .collect();
    // TUI-owned rows have no engine state here; report them as unknown
    // rather than letting the board treat their holders as gone.
    for (uid, t) in s.tui_sessions.iter().filter(|(uid, _)| !s.sessions.contains_key(*uid)) {
        let name = s
            .messaging_names
            .get(uid)
            .map(|n| n.name.clone())
            .or_else(|| t.label.clone())
            .unwrap_or_else(|| uid.clone());
        sessions.push(json!({
            "pid": format!("agent:{daemon_id}:{uid}"),
            "session_uid": uid,
            "task_id": t.task_id,
            "name": clip(&name, 200),
            "engine": t.session_type.as_deref().map(|e| clip(e, 40)),
            "state": "unknown",
        }));
    }
    let api = Api::from_config(&s.config.api_url, &s.config.api_token).ok();
    Some(Snapshot {
        daemon_id,
        host_label: host_label(),
        sessions,
        api,
        root: s.messaging_root.clone(),
    })
}

/// This machine's name for the board's host column.
fn host_label() -> Option<String> {
    let from_env = std::env::var("HOSTNAME").ok();
    let from_file = || std::fs::read_to_string("/etc/hostname").ok();
    let from_libc = || {
        let mut buf = [0u8; 256];
        // SAFETY: the buffer is valid for its length; gethostname writes a
        // NUL-terminated name into it (truncated if longer).
        let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
        (rc == 0).then(|| {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..end]).into_owned()
        })
    };
    from_env
        .or_else(from_file)
        .or_else(from_libc)
        .map(|s| s.trim().split('.').next().unwrap_or_default().to_string())
        .filter(|s| !s.is_empty())
}

/// What to do with one push.
#[derive(Debug, PartialEq)]
pub(crate) enum Plan {
    /// Native notification into a live agent session.
    Publish { uid: String, id: String, text: String, marker: String },
    /// Owner escalation on the target session's row.
    Escalate { uid: Option<String>, source: String, dedupe_key: String, text: String },
    /// Nothing can receive it here (session gone, or a bash pane): ack it.
    Drop { reason: String },
}

/// `engine_of(uid)` is the live session's engine on this daemon, if any.
pub(crate) fn plan(push: &Value, engine_of: impl Fn(&str) -> Option<String>) -> Plan {
    let id = push["id"].as_i64().unwrap_or_default();
    let uid = push["session_uid"].as_str().unwrap_or_default().to_string();
    let text = push["text"].as_str().unwrap_or_default().to_string();
    let board = push["board"].as_str().unwrap_or("board");
    if push["owner_alert"] == true {
        let source = format!("board:{}", push["board_id"].as_str().unwrap_or(board));
        // Keyed by board and session so a pending notify_user alert on the
        // same session neither swallows nor is replaced by this one.
        let dedupe_key = format!("board:{board}:{uid}");
        let uid = engine_of(&uid).map(|_| uid);
        return Plan::Escalate { uid, source, dedupe_key, text };
    }
    match engine_of(&uid).as_deref() {
        Some("claude-code" | "codex") => Plan::Publish {
            uid,
            id: format!("board-push:{id}"),
            text,
            marker: format!("[cm-board {board}]"),
        },
        Some(engine) => Plan::Drop { reason: format!("{engine} session cannot receive pushes") },
        None => Plan::Drop { reason: "session is not live on this daemon".into() },
    }
}

#[derive(Default)]
pub(crate) struct Beat {
    /// Pushes handled since the last successful beat, acked on the next one.
    acks: BTreeSet<i64>,
    warned: bool,
}

impl Beat {
    pub(crate) fn run(&mut self, state: &Arc<Mutex<DaemonState>>) {
        let Some(snap) = capture(state) else { return };
        let Some(api) = snap.api.clone() else {
            if !self.warned {
                eprintln!("cm items: no planning API configured; holder-state heartbeat is off");
                self.warned = true;
            }
            return;
        };
        let acked: Vec<i64> = self.acks.iter().copied().collect();
        let body = json!({
            "host_label": snap.host_label,
            "sessions": snap.sessions,
            "exited": [],
            "acked_push_ids": acked,
        });
        let reply = match api.post(&format!("/hosts/{}/heartbeat", snap.daemon_id), &body) {
            Ok(reply) => reply,
            Err((_, message)) => {
                eprintln!("cm items: heartbeat failed: {message}");
                return;
            }
        };
        for id in &acked {
            self.acks.remove(id);
        }
        for push in reply["pushes"].as_array().into_iter().flatten() {
            let Some(id) = push["id"].as_i64() else { continue };
            if self.deliver(state, &snap.root, push) {
                self.acks.insert(id);
            }
        }
        if !self.acks.is_empty() {
            poke(); // ack promptly rather than in 30 s
        }
    }

    /// True when the push is settled (delivered, escalated or dropped).
    fn deliver(&self, state: &Arc<Mutex<DaemonState>>, root: &std::path::Path, push: &Value) -> bool {
        let planned = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            plan(push, |uid| {
                s.sessions
                    .get(uid)
                    .map(|v| v.session_type.clone())
                    .or_else(|| s.tui_sessions.get(uid).and_then(|t| t.session_type.clone()))
            })
        };
        match planned {
            Plan::Publish { uid, id, text, marker } => {
                match crate::notifications::publish(root, &uid, &id, "board", &text, &marker) {
                    Ok(_) => true,
                    Err(e) => {
                        eprintln!("cm items: push {id} to {uid}: {e}");
                        // A content conflict or an unsendable text can never
                        // succeed: settle it. Other errors retry; the API
                        // gives up after a bounded number of attempts.
                        matches!(e.kind(), std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::InvalidInput)
                    }
                }
            }
            Plan::Escalate { uid, source, dedupe_key, text } => {
                let s = state.lock().unwrap_or_else(|p| p.into_inner());
                match owner_attention::escalate(
                    &s,
                    Escalation {
                        source,
                        dedupe_key,
                        // A board escalation means work has stalled with
                        // nobody acting on it.
                        urgency: Urgency::Blocking,
                        summary: text,
                        session_uid: uid,
                        task_id: None,
                    },
                ) {
                    Ok(_) => true,
                    Err(e) => {
                        eprintln!("cm items: board escalation: {e}");
                        false
                    }
                }
            }
            Plan::Drop { reason } => {
                eprintln!("cm items: dropping push {}: {reason}", push["id"]);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(extra: Value) -> Value {
        let mut p = json!({"id": 7, "session_uid": "u1", "text": "[cm-board sfd] hi", "board": "sfd",
                           "board_id": "b-1", "owner_alert": false});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    }

    #[test]
    fn agent_pushes_publish_with_stable_ids() {
        let planned = plan(&push(json!({})), |_| Some("codex".into()));
        assert_eq!(planned, Plan::Publish {
            uid: "u1".into(),
            id: "board-push:7".into(),
            text: "[cm-board sfd] hi".into(),
            marker: "[cm-board sfd]".into(),
        });
    }

    #[test]
    fn bash_and_missing_sessions_are_dropped() {
        assert!(matches!(plan(&push(json!({})), |_| Some("bash".into())), Plan::Drop { .. }));
        assert!(matches!(plan(&push(json!({})), |_| None), Plan::Drop { .. }));
    }

    #[test]
    fn owner_alerts_escalate_even_without_a_live_session() {
        let alert = push(json!({"owner_alert": true}));
        assert_eq!(plan(&alert, |_| Some("codex".into())), Plan::Escalate {
            uid: Some("u1".into()),
            source: "board:b-1".into(),
            dedupe_key: "board:sfd:u1".into(),
            text: "[cm-board sfd] hi".into(),
        });
        assert_eq!(plan(&alert, |_| None), Plan::Escalate {
            uid: None,
            source: "board:b-1".into(),
            dedupe_key: "board:sfd:u1".into(),
            text: "[cm-board sfd] hi".into(),
        });
    }

    #[test]
    fn session_rows_are_relative_and_carry_agent_state() {
        let st: crate::agent_state::AgentState = serde_json::from_value(json!({
            "state": "idle", "since": 1000.0, "detail": {}, "source": "hooks",
            "observed_at": 1500.0, "turn_seq": 3,
            "last_turn": {"ended_at": 1000.0, "status": "completed"},
            "background": {"complete": true, "observed_at": null, "jobs": [], "crons": [], "ended": []}
        }))
        .unwrap();
        let row = session_row("d1", "u1", "lane", Some("t-1"), "codex", &st, true, 1600.0);
        assert_eq!(row["pid"], "agent:d1:u1");
        assert_eq!(row["state"], "idle");
        assert_eq!(row["state_age_s"], 600.0);
        assert_eq!(row["idle_for_s"], 600.0);
        assert_eq!(row["reported_done"], true);
        assert_eq!(row["agent_state"]["turn_seq"], 3);
        let mut working = st.clone();
        working.state = crate::agent_state::State::Working;
        let long = "x".repeat(500);
        let row = session_row("d1", "u1", &long, None, &long, &working, false, 1600.0);
        assert!(row["idle_for_s"].is_null());
        assert_eq!(row["name"].as_str().unwrap().chars().count(), 200);
        assert_eq!(row["engine"].as_str().unwrap().chars().count(), 40);
    }

    #[test]
    fn a_beat_posts_the_snapshot_delivers_and_acks_next_time() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
        let log = bodies.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Read, Write};
            for (i, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                assert!(line.starts_with("POST /hosts/"), "{line}");
                let mut length = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                log.lock().unwrap().push(serde_json::from_slice(&body).unwrap());
                let reply = if i == 0 {
                    r#"{"pushes":[{"id":41,"session_uid":"me-uid","text":"[cm-board sfd] assigned","board":"sfd","board_id":"b-1","owner_alert":false},{"id":42,"session_uid":"gone","text":"x","board":"sfd","owner_alert":false}]}"#
                } else {
                    r#"{"pushes":[]}"#
                };
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "tok".into();
        st.tui_sessions.insert(
            "me-uid".into(),
            serde_json::from_value(json!({"uid":"me-uid","type":"codex"})).unwrap(),
        );
        let state = Arc::new(Mutex::new(st));
        crate::messaging::rpc::initialize(&state).unwrap();
        let mut beat = Beat::default();
        beat.run(&state);
        assert_eq!(beat.acks, BTreeSet::from([41, 42]));
        let delivered = crate::notifications::get(root.path(), "me-uid", "board-push:41").unwrap().unwrap();
        assert_eq!(delivered["text"], "[cm-board sfd] assigned");
        assert_eq!(delivered["source"], "board");
        beat.run(&state);
        assert!(beat.acks.is_empty());
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies[0]["acked_push_ids"], json!([]));
        assert_eq!(bodies[0]["sessions"].as_array().unwrap().len(), 1);
        assert_eq!(bodies[0]["sessions"][0]["state"], "unknown"); // TUI-only row
        assert_eq!(bodies[1]["acked_push_ids"], json!([41, 42]));
    }
}
