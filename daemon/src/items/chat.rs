//! Chat → item freshness (doc/items-board.md §4): a holder's post naming
//! `#N`, or a reply in a thread whose root names `#N`, touches that item.
//!
//! Runs after a successful `messaging.send` on the sender's own daemon. The
//! hook never holds the daemon lock while it reads the messaging store, and
//! never blocks the send: a single worker thread behind a bounded channel
//! does the planning-API calls, and a burst beyond the queue is dropped.

use super::api::Api;
use crate::state::DaemonState;
use serde_json::{json, Value};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const QUEUE: usize = 64;
const EXCERPT_CHARS: usize = 200;
/// Best effort: a slow API must not back the queue up for long.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Item numbers written as a word-bounded `#N` (not `a#1`, `#1a` or `##`).
pub(crate) fn refs(text: &str) -> Vec<i64> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'#' || (i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'#')) {
            continue;
        }
        let digits: String = bytes[i + 1..].iter().take_while(|c| c.is_ascii_digit()).map(|c| *c as char).collect();
        let after = bytes.get(i + 1 + digits.len());
        if digits.is_empty() || digits.len() > 9 || after.is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            continue;
        }
        if let Ok(n) = digits.parse::<i64>() {
            if n > 0 && !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

struct Job {
    api: Api,
    actor: Value,
    ns: Vec<i64>,
    message_id: Option<String>,
    excerpt: String,
}

fn worker() -> &'static SyncSender<Job> {
    static TX: OnceLock<SyncSender<Job>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<Job>(QUEUE);
        let _ = std::thread::Builder::new().name("items-chat-touch".into()).spawn(move || {
            for job in rx {
                run(&job);
            }
        });
        tx
    })
}

/// Touch, on every board where the sender holds one of the named items, the
/// items it holds there. 404s and timeouts are skipped silently.
fn run(job: &Job) {
    let pid = job.actor["pid"].as_str().unwrap_or_default();
    let Ok(held) = job.api.get("/items", &[("holder_pid", pid.to_owned()), ("open", "true".into())]) else {
        return;
    };
    let mut by_board: std::collections::BTreeMap<String, Vec<i64>> = Default::default();
    for row in held.as_array().into_iter().flatten() {
        let (Some(board), Some(n)) = (row["board"].as_str(), row["n"].as_i64()) else { continue };
        if job.ns.contains(&n) {
            by_board.entry(board.to_owned()).or_default().push(n);
        }
    }
    for (board, ns) in by_board {
        let _ = job.api.post(
            &format!("/boards/{board}/items/touch"),
            &json!({"actor": job.actor, "ns": ns, "source": "chat",
                    "message_id": job.message_id, "excerpt": job.excerpt}),
        );
    }
}

/// After a successful send by `uid`: find `#N` and queue the touch.
pub fn on_send(state: &Arc<Mutex<DaemonState>>, uid: &str, params: &Value, reply: &Value) {
    let body = params["body"].as_str().unwrap_or_default();
    let mut ns = refs(body);
    // Snapshot under the daemon lock, then release it before the messaging
    // slot is touched.
    let (handle, api_url, api_token, task_id, title) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let (task_id, title) = if let Some(sess) = s.sessions.get(uid) {
            (sess.task_id.clone(), sess.title.clone())
        } else if let Some(t) = s.tui_sessions.get(uid) {
            (t.task_id.clone(), t.label.clone().unwrap_or_else(|| uid.to_owned()))
        } else {
            return;
        };
        (s.messaging.clone(), s.config.api_url.clone(), s.config.api_token.clone(), task_id, title)
    };
    let thread_root = reply["event"]["data"]["thread_root"].as_str().map(str::to_owned);
    let (daemon_id, name, root_body) = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        let Some(store) = slot.as_ref() else { return };
        let pid = store.participant_id(uid);
        let name = store.names.get(&pid).map(|n| n.name.clone());
        let root = match (&thread_root, ns.is_empty()) {
            (Some(root), true) => store.message_body(root),
            _ => None,
        };
        (store.daemon_id.clone(), name, root)
    };
    if let Some(root) = root_body {
        ns = refs(&root);
    }
    if ns.is_empty() {
        return;
    }
    let Ok(api) = Api::from_config(&api_url, &api_token) else { return };
    let job = Job {
        api: api.with_timeout(TIMEOUT),
        actor: json!({"pid": format!("agent:{daemon_id}:{uid}"), "name": name.unwrap_or(title),
                      "session_uid": uid, "daemon_id": daemon_id, "task_id": task_id}),
        ns,
        message_id: reply["event_id"].as_str().or_else(|| reply["event"]["id"].as_str()).map(str::to_owned),
        excerpt: body.chars().take(EXCERPT_CHARS).collect(),
    };
    match worker().try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => eprintln!("cm items: chat touch queue full; dropped one"),
        Err(TrySendError::Disconnected(_)) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::control::protocol::{Caller, Request};
    use std::io::{BufRead, BufReader, Read, Write};

    #[test]
    fn a_thread_reply_touches_the_items_its_root_names_on_the_holders_boards() {
        let _env = crate::test_support::env_lock();
        let root = tempfile::tempdir().unwrap();
        let seen: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::default();
        let log = seen.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let mut length = 0;
                loop {
                    let mut h = String::new();
                    reader.read_line(&mut h).unwrap();
                    if h.trim().is_empty() { break; }
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let parts: Vec<String> = first.split_whitespace().map(str::to_owned).collect();
                let reply = if parts[0] == "GET" {
                    // Holds #7 on two boards, and #8 elsewhere (not named).
                    r#"[{"board":"sfd","n":7},{"board":"other","n":7},{"board":"sfd","n":8}]"#
                } else {
                    r#"{"touched":[7],"skipped":[]}"#
                };
                log.lock().unwrap().push((parts[0].clone(), parts[1].clone(),
                    serde_json::from_slice(&body).unwrap_or(Value::Null)));
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        let mut st = DaemonState::default();
        st.messaging_root = root.path().into();
        st.config.api_url = url;
        st.config.api_token = "tok".into();
        st.tui_sessions.insert("me-uid".into(),
            serde_json::from_value(json!({"uid":"me-uid","label":"lane","task_id":"t-1"})).unwrap());
        let state = Arc::new(Mutex::new(st));
        crate::messaging::startup::open_for_test(&state).unwrap();
        let sent = crate::messaging::rpc::dispatch(&state, &Request {
            id: "r".into(),
            caller: Caller::session("me-uid"),
            method: "messaging.send".into(),
            params: json!({"channel":"general","name":"Lane","body":"plan for #7","request_id":"root-1"}),
        });
        let root_id = sent.result.unwrap()["event_id"].as_str().unwrap().to_owned();
        on_send(&state, "me-uid", &json!({"body": "progress update"}),
                &json!({"event_id": "reply-1", "event": {"data": {"thread_root": root_id}}}));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap().len() < 3 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let seen = seen.lock().unwrap();
        assert!(seen[0].0 == "GET" && seen[0].1.starts_with("/items?"), "{:?}", seen[0]);
        let mut posts: Vec<_> = seen[1..].iter().map(|(m, p, b)| (m.clone(), p.clone(), b["ns"].to_string())).collect();
        posts.sort();
        assert_eq!(posts, vec![
            ("POST".to_string(), "/boards/other/items/touch".to_string(), "[7]".to_string()),
            ("POST".to_string(), "/boards/sfd/items/touch".to_string(), "[7]".to_string()),
        ]);
        assert_eq!(seen[1].2["source"], "chat");
        assert_eq!(seen[1].2["message_id"], "reply-1");
        assert!(seen[1].2["actor"]["pid"].as_str().unwrap().ends_with(":me-uid"));
    }

    #[test]
    fn refs_are_word_bounded_item_numbers() {
        assert_eq!(refs("done with #14, and #15: see #14 again"), vec![14, 15]);
        assert_eq!(refs("#3"), vec![3]);
        assert_eq!(refs("(#7) [#8] #9."), vec![7, 8, 9]);
        assert!(refs("issue#4 #4a ##5 #0 # 6 #x").is_empty());
        assert_eq!(refs("PR #12_ no, #13 yes"), vec![13]);
    }
}
