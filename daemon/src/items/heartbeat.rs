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
    {
        // Fail-static launch grants: what the last beat granted holds until
        // the API answers again (doc/org-model.md §7.4).
        let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
        let root = s.messaging_root.clone();
        s.launch_grants = crate::items::grants::load(&root);
        // Members' main sessions stay protected across a brain restart.
        super::org::load_cache(&root);
    }
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
            let mut row = session_row(
                &daemon_id,
                uid,
                &name,
                sess.task_id.as_deref(),
                &sess.session_type,
                &st,
                sess.reported_done().is_some(),
                now,
            );
            // Org model S2: what the roster needs (workspace, continuous
            // ownership, age). An API before S2 ignores these keys.
            if let Some(obj) = row.as_object_mut() {
                obj.extend(super::org::session_facts(&s, sess, now));
            }
            row
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
pub(crate) fn host_label() -> Option<String> {
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
    /// Home/stream workspace marks applied since the last successful beat
    /// (org model S2), acked on the next one as `acked_mark_ids`.
    mark_acks: BTreeSet<i64>,
    warned: bool,
    /// When an API that predates `HeartbeatBody.capabilities` last refused
    /// the field (its body model forbids unknown keys). The beat then goes
    /// without it, and tries again after [`CAPS_RETRY`].
    caps_refused_at: Option<std::time::Instant>,
}

/// How long to leave `capabilities` off after an older API refused it.
const CAPS_RETRY: std::time::Duration = std::time::Duration::from_secs(600);

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
        let mut body = json!({
            "host_label": snap.host_label,
            "sessions": snap.sessions,
            "exited": [],
            "acked_push_ids": acked,
        });
        // Only an API that hands out marks (org S2+) knows this field.
        let mark_acked: Vec<i64> = self.mark_acks.iter().copied().collect();
        if !mark_acked.is_empty() {
            body["acked_mark_ids"] = json!(mark_acked);
        }
        // doc/org-model.md §12: what this host advertises (brain ∩ holder ∩
        // rollback pin), unless an older API refused the field recently.
        let send_caps = self.caps_refused_at.is_none_or(|at| at.elapsed() >= CAPS_RETRY);
        if send_caps {
            body["capabilities"] = json!(crate::capabilities::advertised());
        }
        let path = format!("/hosts/{}/heartbeat", snap.daemon_id);
        let mut result = api.post(&path, &body);
        if send_caps {
            if let Err((crate::control::protocol::ErrorCode::InvalidParams, message)) = &result {
                if message.contains("capabilities") {
                    eprintln!("cm items: the planning API predates heartbeat capabilities; beating without them");
                    self.caps_refused_at = Some(std::time::Instant::now());
                    body.as_object_mut().map(|b| b.remove("capabilities"));
                    result = api.post(&path, &body);
                }
            }
        }
        let reply = match result {
            Ok(reply) => reply,
            Err((_, message)) => {
                eprintln!("cm items: heartbeat failed: {message}");
                return;
            }
        };
        for id in &acked {
            self.acks.remove(id);
        }
        for id in &mark_acked {
            self.mark_acks.remove(id);
        }
        if let Some(grants) = crate::items::grants::from_reply(&reply) {
            let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
            let root = s.messaging_root.clone();
            crate::items::grants::replace(&mut s.launch_grants, &root, grants);
        }
        super::org::absorb_reply(&reply, crate::agent_state::unix_now());
        if reply["org"]["members"].is_object() {
            super::org::save_cache(&snap.root);
        }
        let applied = {
            let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
            let applied = super::org::apply_marks(&mut s, &reply);
            // A promote whose local half was lost (the brain died after the
            // API committed it): finish it now the cache shows the main.
            let promoted = crate::assistants::reconcile_promoted(&mut s);
            if !promoted.is_empty() {
                eprintln!("cm org: finished {} promotion(s) from the member cache: {}",
                          promoted.len(), promoted.join(", "));
                s.persist_sessions_best_effort();
                for uid in &promoted {
                    let org = s.sessions.get(uid).and_then(|sess| sess.org.clone());
                    s.manifest_watcher.broadcast(crate::manifest::ManifestDiff::Updated {
                        uid: uid.clone(),
                        entry: json!({"member": crate::assistants::wire(uid, org.as_ref()), "org": org}),
                    });
                    super::org::notify_promoted(&snap.root, uid, &crate::items::org::cached_member(uid));
                }
            }
            // O5: sessions a member's session spawned, or that work under a
            // member's task, are that member's assistants.
            let stamped = crate::assistants::backfill(&mut s);
            // A stream whose assistant has been gone 7 days goes back to
            // normal reaper eligibility.
            let (released, changed) =
                crate::assistants::release_idle_streams(&mut s, crate::agent_state::unix_now());
            if !released.is_empty() {
                eprintln!("cm org: released {} idle assistant stream(s) to ephemeral: {}",
                          released.len(), released.join(", "));
            }
            if changed && stamped.is_empty() {
                s.persist_sessions_best_effort();
            }
            if !stamped.is_empty() {
                eprintln!("cm org: stamped {} session(s) as members' assistants: {}",
                          stamped.len(), stamped.join(", "));
                s.persist_sessions_best_effort();
                // The TUI's chart reads `member` off manifest diffs.
                for uid in &stamped {
                    let org = s.sessions.get(uid).and_then(|sess| sess.org.clone());
                    s.manifest_watcher.broadcast(crate::manifest::ManifestDiff::Updated {
                        uid: uid.clone(),
                        entry: json!({"member": crate::assistants::wire(uid, org.as_ref()),
                                      "org": org}),
                    });
                }
            }
            applied
        };
        self.mark_acks.extend(applied);
        for push in reply["pushes"].as_array().into_iter().flatten() {
            let Some(id) = push["id"].as_i64() else { continue };
            if self.deliver(state, &snap.root, push) {
                self.acks.insert(id);
            }
        }
        if !self.acks.is_empty() || !self.mark_acks.is_empty() {
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

    /// Org model S2's home push: a beat applies the reply's workspace marks
    /// and acks them on the next beat only (`acked_mark_ids`), once.
    #[test]
    fn a_beat_applies_workspace_marks_and_acks_them_next_time() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let (url, seen) = super::super::org::test_stub(move |_, _, _| {
            // The API confirms the ack, so it pushes the mark only once.
            let reply = if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                json!({"pushes": [], "org": {"members": {}, "workspace_marks": []}})
            } else {
                json!({"pushes": [], "org": {"members": {}, "workspace_marks": [
                    {"id": 7, "workspace_id": "ws-home", "path": "/w/home", "kind": "home", "member_id": "mem_1"}]}})
            };
            (200, reply.to_string())
        });
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "tok".into();
        st.workspaces.insert(
            "ws-home".into(),
            serde_json::from_value(json!({"id": "ws-home", "worktree_path": "/w/home", "sessions": []})).unwrap(),
        );
        let state = Arc::new(Mutex::new(st));
        crate::messaging::startup::open_for_test(&state).unwrap();
        let mut beat = Beat::default();
        beat.run(&state);
        assert_eq!(beat.mark_acks, BTreeSet::from([7]));
        let kind = state.lock().unwrap().workspaces["ws-home"].org.as_ref().unwrap()["workspace_kind"].clone();
        assert_eq!(kind, "home");
        beat.run(&state);
        beat.run(&state);
        assert!(beat.mark_acks.is_empty());
        let bodies: Vec<Value> = seen.lock().unwrap().iter().map(|(_, _, b)| b.clone()).collect();
        assert!(bodies[0].get("acked_mark_ids").is_none());
        assert_eq!(bodies[1]["acked_mark_ids"], json!([7]));
        assert!(bodies[2].get("acked_mark_ids").is_none(), "acked once");
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
                    r#"{"pushes":[{"id":41,"session_uid":"me-uid","text":"[cm-board sfd] assigned","board":"sfd","board_id":"b-1","owner_alert":false},{"id":42,"session_uid":"gone","text":"x","board":"sfd","owner_alert":false}],"org":{"members":{"me-uid":{"id":"mem_1","handle":"ep","acting_as":"main"}}},"launch_grants":[{"task_id":"t-1","session_uid":"me-uid","pid":"agent:d:me-uid","board":"sfd","n":4}]}"#
                } else {
                    // An older API sends no grants: the cache stays.
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
        crate::messaging::startup::open_for_test(&state).unwrap();
        super::super::org::reset_cache_for_test();
        let mut beat = Beat::default();
        beat.run(&state);
        assert_eq!(beat.acks, BTreeSet::from([41, 42]));
        // Org model S2: the reply's member map feeds `ping`'s member.
        assert_eq!(super::super::org::cached_member("me-uid")["handle"], "ep");
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
        // Every beat sends what the host advertises (doc/org-model.md
        // §12); with org's mark-apply and items' sweep guard both in, this
        // build claims lifecycle_guard_v1.
        assert_eq!(bodies[0]["capabilities"], json!(crate::capabilities::advertised()));
        // (What is advertised also depends on the rollback pin; the brain
        // list is this build's own claim.)
        assert!(crate::capabilities::brain().iter().any(|c| c == crate::lifecycle_guard::CAPABILITY));
        assert!(crate::capabilities::brain().iter().any(|c| c == crate::capabilities::ORG_MARKS_V1));
        assert!(bodies[0].get("acked_mark_ids").is_none(), "no marks yet: the field stays off");
        // The reply's launch grants replace the cache and are persisted.
        let s = state.lock().unwrap();
        assert_eq!(s.launch_grants.len(), 1);
        assert_eq!(crate::items::grants::load(root.path()), s.launch_grants);
        // The second reply had no `org` key (a pre-S2 API): the cache stays.
        assert_eq!(super::super::org::cached_member("me-uid")["handle"], "ep");
        super::super::org::reset_cache_for_test();
    }

    /// An API whose heartbeat model predates `capabilities` refuses the
    /// unknown field (422): the beat is resent without it at once, and
    /// later beats leave it off for a while instead of failing every 30 s.
    #[test]
    fn an_older_api_that_refuses_capabilities_still_gets_the_beat() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
        let log = bodies.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Read, Write};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
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
                let body: Value = serde_json::from_slice(&body).unwrap();
                let refuse = body.get("capabilities").is_some();
                log.lock().unwrap().push(body);
                let (status, reply) = if refuse {
                    ("422 Unprocessable Entity", r#"{"detail":[{"type":"extra_forbidden","loc":["body","capabilities"],"msg":"Extra inputs are not permitted"}]}"#)
                } else {
                    ("200 OK", r#"{"pushes":[]}"#)
                };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "tok".into();
        let state = Arc::new(Mutex::new(st));
        crate::messaging::startup::open_for_test(&state).unwrap();
        let mut beat = Beat::default();
        beat.run(&state);
        beat.run(&state);
        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 3, "refused, resent without, then sent without: {bodies:?}");
        assert!(bodies[0].get("capabilities").is_some());
        assert!(bodies[1].get("capabilities").is_none());
        assert!(bodies[2].get("capabilities").is_none());
        assert!(beat.caps_refused_at.is_some());
    }
}
