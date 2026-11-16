//! Chat → item freshness (doc/items-board.md §4). A post refreshes item N
//! when it is a top-level post naming `#N`, or a reply in a thread whose root
//! names `#N` (a reply naming some other `#M` is only a reference), and the
//! sender holds N or is its board's steward or orchestrator. Quoted lines
//! (`>`) never count. The API resolves which board N is on: the one bound to
//! the channel, else the only board where the sender may refresh it.
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

/// [`refs`] outside quoted lines (`> …`), which repeat someone else's words.
pub(crate) fn unquoted_refs(text: &str) -> Vec<i64> {
    let own: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with('>')).collect();
    refs(&own.join("\n"))
}

/// A result word in the sender's own text that suggests a status
/// (Board-Steward, 2026-10-20): `DONE`, `GREEN` or `DEPLOYED` (→ done) and
/// `STOP` (→ blocked), in capitals only. Never counted: quoted lines (`>`),
/// code (fenced blocks and backtick spans), a word followed by `?`,
/// "GREEN light", or a word with a negation, condition or future tense
/// among the four words before it (`not yet DONE`, `don't STOP`,
/// `once DEPLOYED`, `will be GREEN`).
/// Conflicting words suggest nothing. Only a suggestion: the holder or
/// steward confirms it on the board.
pub(crate) fn suggestion(text: &str) -> Option<(&'static str, &'static str)> {
    const WORDS: &[(&str, &str)] = &[("DONE", "done"), ("GREEN", "done"), ("DEPLOYED", "done"), ("STOP", "blocked")];
    const BLOCKERS: &[&str] = &[
        "not", "no", "never", "nor", "don't", "dont", "doesn't", "didn't", "isn't", "isnt", "aren't",
        "wasn't", "won't", "can't", "cannot", "without", "if", "once", "when", "unless", "until", "before",
        // Future or expected, not a result yet.
        "will", "would", "should", "be", "expect", "expected", "awaiting", "waiting", "pending",
    ];
    const LOOKBACK: usize = 4;
    let mut found: Option<(&'static str, &'static str)> = None;
    let mut in_fence = false;
    for raw in text.lines() {
        let line = raw.trim_start();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || line.starts_with('>') {
            continue;
        }
        // Drop inline `code` spans.
        let mut plain = String::with_capacity(line.len());
        let mut in_tick = false;
        for c in line.chars() {
            if c == '`' {
                in_tick = !in_tick;
                plain.push(' ');
            } else if !in_tick {
                plain.push(c);
            }
        }
        // Words with the character right after each.
        let chars: Vec<char> = plain.chars().collect();
        let mut words: Vec<(String, Option<char>)> = vec![];
        let mut i = 0;
        while i < chars.len() {
            if chars[i].is_alphanumeric() || chars[i] == '\'' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '\'') {
                    i += 1;
                }
                let next = chars[i..].iter().copied().find(|c| !c.is_whitespace());
                words.push((chars[start..i].iter().collect(), next));
            } else {
                i += 1;
            }
        }
        for (k, (word, next)) in words.iter().enumerate() {
            let Some((keyword, status)) = WORDS.iter().find(|(w, _)| w == word).copied() else {
                continue;
            };
            if *next == Some('?') {
                continue;
            }
            if keyword == "GREEN" && words.get(k + 1).is_some_and(|(w, _)| w.eq_ignore_ascii_case("light")) {
                continue;
            }
            let blocked = words[k.saturating_sub(LOOKBACK)..k]
                .iter()
                .any(|(w, _)| BLOCKERS.contains(&w.to_lowercase().as_str()));
            if blocked {
                continue;
            }
            match found {
                Some((_, s)) if s != status => return None,
                _ => found = Some((keyword, status)),
            }
        }
    }
    found
}

/// The suggestion a post makes, if any: only when the post is about exactly
/// one item and, for a thread reply, its own text names that same item.
pub(crate) fn post_suggestion(body: &str, ns: &[i64], is_reply: bool) -> Option<(&'static str, &'static str)> {
    if ns.len() != 1 {
        return None;
    }
    if is_reply && unquoted_refs(body) != ns {
        return None;
    }
    suggestion(body)
}

struct Job {
    api: Api,
    actor: Value,
    ns: Vec<i64>,
    channel: Option<String>,
    message_id: Option<String>,
    excerpt: String,
    suggest: Option<(&'static str, &'static str)>,
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

/// One call: the API resolves boards and applies the sender rules. An API
/// that predates it (404) gets the holder-only touches below.
fn run(job: &Job) {
    let mut body = json!({"actor": job.actor, "ns": job.ns, "channel": job.channel, "source": "chat",
                          "message_id": job.message_id, "excerpt": job.excerpt});
    if let Some((keyword, status)) = job.suggest {
        body["suggest"] = json!({"keyword": keyword, "status": status});
    }
    if let Err((crate::control::protocol::ErrorCode::NotFound, _)) = job.api.post("/items/chat-touch", &body) {
        run_holder_only(job);
    }
}

/// Pre-2026-10-20 API: touch, on every board where the sender holds one of
/// the named items, the items it holds there. Timeouts are skipped silently.
fn run_holder_only(job: &Job) {
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
    let channel = params["channel"].as_str().map(|c| c.trim_start_matches('#').to_owned());
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
        let root = thread_root.as_ref().and_then(|root| store.message_body(root));
        (store.daemon_id.clone(), name, root)
    };
    // A reply refreshes the items its thread is about; a top-level post the
    // items it names.
    let ns = match (&thread_root, root_body) {
        (Some(_), Some(root)) => unquoted_refs(&root),
        (Some(_), None) => Vec::new(),
        (None, _) => unquoted_refs(body),
    };
    if ns.is_empty() {
        return;
    }
    let Ok(api) = Api::from_config(&api_url, &api_token) else { return };
    let suggest = post_suggestion(body, &ns, thread_root.is_some());
    let job = Job {
        api: api.with_timeout(TIMEOUT),
        actor: json!({"pid": format!("agent:{daemon_id}:{uid}"), "name": name.unwrap_or(title),
                      "session_uid": uid, "daemon_id": daemon_id, "task_id": task_id}),
        ns,
        channel,
        message_id: reply["event_id"].as_str().or_else(|| reply["event"]["id"].as_str()).map(str::to_owned),
        excerpt: body.chars().take(EXCERPT_CHARS).collect(),
        suggest,
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

    #[test]
    fn keywords_suggest_a_status_only_when_unambiguous() {
        assert_eq!(suggestion("#14 gate GREEN on a1b2c3d"), Some(("GREEN", "done")));
        assert_eq!(suggestion("#14 DONE."), Some(("DONE", "done")));
        assert_eq!(suggestion("#14 DEPLOYED to cm-manager"), Some(("DEPLOYED", "done")));
        assert_eq!(suggestion("#14 STOP: OOM on devbox6"), Some(("STOP", "blocked")));
        for prose in [
            "#14 not DONE yet",
            "#14 not yet DONE",
            "#14 isn't DEPLOYED",
            "#14 don't STOP the run",
            "#14 will be DONE once the gate is GREEN... once GREEN",
            "#14 needs to be deployed",
            "#14 deployed to cm-manager",
            "#14 almost done, green-ish",
            "#14 DONE?",
            "#14 is it GREEN ?",
            "#14 got the GREEN light from EP",
            "#14 run `make STOP` then check",
            "#14 log:\n```\nstatus: DONE\n```",
            "> #14 DONE\nthat was yesterday",
            "#14 GREEN but STOP the deploy",
            "#14 DONEish",
        ] {
            assert_eq!(suggestion(prose), None, "{prose}");
        }
    }

    #[test]
    fn a_reply_suggests_only_for_the_one_item_it_names_itself() {
        assert_eq!(post_suggestion("#14 DONE", &[14], false), Some(("DONE", "done")));
        assert_eq!(post_suggestion("#14 #15 DONE", &[14, 15], false), None, "more than one item");
        // A reply in #14's thread: the keyword must sit in a reply that names #14 itself.
        assert_eq!(post_suggestion("DONE", &[14], true), None);
        assert_eq!(post_suggestion("#14 DONE", &[14], true), Some(("DONE", "done")));
        assert_eq!(post_suggestion("#15 DONE", &[14], true), None, "names another item");
        assert_eq!(post_suggestion("#14 DONE", &[14, 15], true), None);
    }

    use crate::control::protocol::{Caller, Request};
    use std::io::{BufRead, BufReader, Read, Write};

    /// A tiny planning API: records every request; answers POST
    /// /items/chat-touch with `chat_touch` status (404 = an older API).
    fn fake_api(chat_touch: u16) -> (String, Arc<Mutex<Vec<(String, String, Value)>>>) {
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
                let (status, reply) = if parts[1] == "/items/chat-touch" {
                    (chat_touch, r#"{"touched":{"sfd":[7]},"skipped":[]}"#)
                } else if parts[0] == "GET" {
                    // Holds #7 on two boards, and #8 elsewhere (not named).
                    (200, r#"[{"board":"sfd","n":7},{"board":"other","n":7},{"board":"sfd","n":8}]"#)
                } else {
                    (200, r#"{"touched":[7],"skipped":[]}"#)
                };
                log.lock().unwrap().push((parts[0].clone(), parts[1].clone(),
                    serde_json::from_slice(&body).unwrap_or(Value::Null)));
                let reason = if status == 200 { "OK" } else { "Not Found" };
                let _ = write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len());
            }
        });
        (url, seen)
    }

    /// Sends a root naming #7 and returns the state plus the root's id.
    fn thread_fixture(url: String) -> (Arc<Mutex<DaemonState>>, String, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
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
        (state, root_id, root)
    }

    fn wait_for(seen: &Arc<Mutex<Vec<(String, String, Value)>>>, n: usize) -> Vec<(String, String, Value)> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while seen.lock().unwrap().len() < n && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        seen.lock().unwrap().clone()
    }

    /// A reply refreshes the item its thread is about, not other items it
    /// names (2026-10-20 activity rules); the API resolves the board.
    #[test]
    fn a_thread_reply_refreshes_the_items_its_root_names_through_chat_touch() {
        let _env = crate::test_support::env_lock();
        let (url, seen) = fake_api(200);
        let (state, root_id, _dir) = thread_fixture(url);
        on_send(&state, "me-uid", &json!({"channel": "#general", "body": "progress, see also #9"}),
                &json!({"event_id": "reply-1", "event": {"data": {"thread_root": root_id}}}));
        let seen = wait_for(&seen, 1);
        assert_eq!(seen.len(), 1, "{seen:?}");
        let (method, path, body) = &seen[0];
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/items/chat-touch"));
        assert_eq!(body["ns"], json!([7]));
        assert_eq!(body["channel"], "general");
        assert_eq!(body["message_id"], "reply-1");
        assert_eq!(body["source"], "chat");
        assert!(body["actor"]["pid"].as_str().unwrap().ends_with(":me-uid"));
    }

    /// An API without /items/chat-touch gets the old holder-only touches.
    #[test]
    fn an_older_api_falls_back_to_holder_only_touches() {
        let _env = crate::test_support::env_lock();
        let (url, seen) = fake_api(404);
        let (state, root_id, _dir) = thread_fixture(url);
        on_send(&state, "me-uid", &json!({"channel": "general", "body": "progress update"}),
                &json!({"event_id": "reply-1", "event": {"data": {"thread_root": root_id}}}));
        let seen = wait_for(&seen, 4);
        assert_eq!(seen[0].1, "/items/chat-touch");
        assert!(seen[1].0 == "GET" && seen[1].1.starts_with("/items?"), "{:?}", seen[1]);
        let mut posts: Vec<_> = seen[2..].iter().map(|(m, p, b)| (m.clone(), p.clone(), b["ns"].to_string())).collect();
        posts.sort();
        assert_eq!(posts, vec![
            ("POST".to_string(), "/boards/other/items/touch".to_string(), "[7]".to_string()),
            ("POST".to_string(), "/boards/sfd/items/touch".to_string(), "[7]".to_string()),
        ]);
    }

    #[test]
    fn quoted_lines_are_references_not_activity() {
        assert_eq!(unquoted_refs("> they said #3 is done\nworking on #4"), vec![4]);
        assert_eq!(unquoted_refs("   > #3\n>#5"), Vec::<i64>::new());
        assert_eq!(unquoted_refs("#2 and #2 again"), vec![2]);
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
