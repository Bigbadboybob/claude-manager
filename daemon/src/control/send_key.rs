//! `session.send_key` (also served as `send_key`): press exactly ONE key in
//! one or more sessions. No body, no trailing Enter. The main uses are
//! waking a session parked on a rate-limit or error screen (`enter`) and
//! stepping through a menu (`down`, `1`, `escape`).
//!
//! - Scope is `send_input`'s: a Session caller reaches itself and its
//!   descendants (or anything, with global perms); the Operator reaches all.
//! - Each target is handled on its own and gets its own result (`ok`,
//!   `not_found`, `unauthorized`, `exited`, or `error` for a failed write);
//!   one bad target never stops the rest. At most [`MAX_TARGETS`] per call.
//! - The key is encoded for the target's keyboard mode by
//!   [`crate::keys::key_bytes`] (the mapping Owner's own keystrokes share).
//!   The mode is the session's tracked kitty state
//!   ([`crate::keys::KeyboardTracker`], fed by every output chunk and carried
//!   across brain restarts). When it is unknown (a session adopted from a
//!   brain that did not persist it, whose output has not set it since), the
//!   engine default prompt delivery uses applies: kitty for an agent, legacy
//!   for bash; the result then says `mode: "assumed"`.
//! - A key is not new work: the write stamps nothing, so it neither
//!   supersedes a `reported` status nor re-arms watches.
//! - Lock order: the state lock only to authorize and clone the session's
//!   input handle, released before the write; the write takes the retirement
//!   gate, then the writer permit and the session's writer (gate → slot →
//!   state is never inverted: no state lock is held while writing).

use super::auth::{check_session_caller, AuthDecision};
use super::methods::{already_exited_message, MethodResult};
use super::protocol::ErrorCode;
use crate::state::DaemonState;
use alacritty_terminal::term::TermMode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// The most sessions one call may target.
pub const MAX_TARGETS: usize = 50;

#[derive(Deserialize)]
#[serde(untagged)]
enum Targets {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
struct Params {
    session_uids: Targets,
    key: String,
}

pub fn send_key(state: &Arc<Mutex<DaemonState>>, params: &Value, caller_uid: Option<&str>) -> MethodResult {
    let p: Params = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("send_key params: {e}")))?;
    crate::keys::parse_key(&p.key).map_err(|e| (ErrorCode::InvalidParams, e))?;
    let mut uids: Vec<String> = Vec::new();
    for uid in match p.session_uids {
        Targets::One(u) => vec![u],
        Targets::Many(v) => v,
    } {
        let uid = uid.trim().to_owned();
        if !uid.is_empty() && !uids.contains(&uid) {
            uids.push(uid);
        }
    }
    if uids.is_empty() {
        return Err((ErrorCode::InvalidParams, "send_key needs at least one session uid".into()));
    }
    if uids.len() > MAX_TARGETS {
        return Err((
            ErrorCode::InvalidParams,
            format!("send_key takes at most {MAX_TARGETS} sessions per call, got {}", uids.len()),
        ));
    }
    // A caller the daemon doesn't know can reach nothing: refuse the call
    // as a whole rather than report every target unauthorized.
    if let Some(cuid) = caller_uid {
        let known = state.lock().unwrap_or_else(|p| p.into_inner()).sessions.contains_key(cuid);
        if !known {
            return Err((ErrorCode::Unauthorized, format!("Session caller '{cuid}' is not in the daemon registry")));
        }
    }
    let results: Vec<Value> = uids.iter().map(|uid| send_one(state, uid, &p.key, caller_uid)).collect();
    let sent = results.iter().filter(|r| r["status"] == "ok").count();
    Ok(json!({ "key": p.key, "sent": sent, "results": results }))
}

fn outcome(uid: &str, status: &str, message: impl Into<String>) -> Value {
    json!({ "session_uid": uid, "status": status, "message": message.into() })
}

fn send_one(state_arc: &Arc<Mutex<DaemonState>>, uid: &str, key: &str, caller_uid: Option<&str>) -> Value {
    let (handle, fanout, is_agent) = {
        let mut state = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        let gone = |state: &DaemonState| match already_exited_message(state, uid) {
            Some(msg) => outcome(uid, "exited", msg),
            None => outcome(uid, "not_found", format!("session '{uid}' not in daemon registry")),
        };
        if let Some(cuid) = caller_uid {
            match check_session_caller(&state, cuid, uid) {
                AuthDecision::Allow => {}
                AuthDecision::TargetNotInRegistry => return gone(&state),
                _ => return outcome(uid, "unauthorized", "session is outside the caller's scope"),
            }
        }
        let Some(session) = state.sessions.get_mut(uid) else {
            return gone(&state);
        };
        if session.try_wait().is_some() {
            return outcome(uid, "exited", format!("session '{uid}' has exited; PTY no longer writable"));
        }
        (session.input_handle(), session.fanout.clone(), session.session_type != "bash")
    };
    let (mode, source) = keyboard_mode(&fanout, is_agent);
    let bytes = match crate::keys::key_bytes(key, mode) {
        Ok(b) => b,
        Err(e) => return outcome(uid, "error", e),
    };
    if let Err(e) = handle.write_key(&bytes) {
        return outcome(uid, "error", format!("write to PTY: {e}"));
    }
    json!({
        "session_uid": uid,
        "status": "ok",
        "keyboard": if crate::keys::is_kitty(mode) { "kitty" } else { "legacy" },
        "mode": source,
    })
}

/// The target's keyboard mode and how we know it: `"observed"` from its
/// tracked state, else `"assumed"` from its engine (kitty for an agent,
/// as prompt delivery assumes; legacy for bash).
fn keyboard_mode(fanout: &crate::session::PtyByteFanout, is_agent: bool) -> (TermMode, &'static str) {
    match fanout.keyboard().0 {
        Some(mode) => (mode, "observed"),
        None if is_agent => (TermMode::DISAMBIGUATE_ESC_CODES, "assumed"),
        None => (TermMode::empty(), "assumed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// A session running `script` under `sh -c` in `ws`, as `engine`.
    fn spawn(state: &Arc<Mutex<DaemonState>>, uid: &str, ws: &str, engine: &str, script: &str) {
        let mut p = crate::session::SpawnParams::new(uid, uid, "/bin/sh");
        p.args = vec!["-c".into(), script.into()];
        p.workspace_id = ws.into();
        p.session_type = engine.into();
        let s = crate::session::DaemonSession::spawn(p).expect("spawn");
        state.lock().unwrap().sessions.insert(uid.into(), s);
    }

    /// A session that records every byte it receives, raw, into `file`
    /// (after printing `preamble`, e.g. a kitty mode push).
    fn recorder(state: &Arc<Mutex<DaemonState>>, uid: &str, ws: &str, engine: &str, preamble: &str, file: &std::path::Path) {
        let script = format!("printf '{preamble}'; stty raw -echo; exec cat > '{}'", file.display());
        spawn(state, uid, ws, engine, &script);
    }

    fn wait_for(file: &std::path::Path, len: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let got = std::fs::read(file).unwrap_or_default();
            if got.len() >= len || Instant::now() > deadline {
                return got;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until the recorder is in raw mode (its preamble reached the ring).
    fn settle(state: &Arc<Mutex<DaemonState>>, uid: &str) {
        std::thread::sleep(Duration::from_millis(300));
        let _ = state.lock().unwrap().sessions[uid].fanout.snapshot_since(None);
    }

    fn kill_all(state: &Arc<Mutex<DaemonState>>) {
        for (_, mut s) in state.lock().unwrap().sessions.drain() {
            let _ = s.kill();
        }
    }

    fn statuses(reply: &Value) -> Vec<(String, String)> {
        reply["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["session_uid"].as_str().unwrap().to_owned(), r["status"].as_str().unwrap().to_owned()))
            .collect()
    }

    /// The bytes that reach the program are exactly the key: for a bash
    /// session the legacy encoding, for an agent in kitty mode the kitty
    /// one. Nothing follows them, in particular no Enter.
    #[test]
    fn exactly_the_key_reaches_the_pty_in_the_sessions_keyboard_mode() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let (legacy, kitty) = (dir.path().join("legacy"), dir.path().join("kitty"));
        recorder(&state, "ts-1-0", "ws", "bash", "", &legacy);
        recorder(&state, "ts-2-0", "ws", "codex", "\\033[>1u", &kitty);
        settle(&state, "ts-1-0");
        settle(&state, "ts-2-0");
        for key in ["down", "y", "escape", "enter"] {
            let reply = send_key(&state, &json!({"session_uids": ["ts-1-0", "ts-2-0"], "key": key}), None).unwrap();
            assert_eq!(reply["sent"], 2, "{key}: {reply}");
            assert_eq!((reply["results"][0]["keyboard"].as_str(), reply["results"][1]["keyboard"].as_str()), (Some("legacy"), Some("kitty")));
        }
        assert_eq!(wait_for(&legacy, 6), b"\x1b[By\x1b\r", "bash: legacy bytes, nothing more");
        // A key is not new work: no input stamp, so a done report stands.
        for uid in ["ts-1-0", "ts-2-0"] {
            assert!(state.lock().unwrap().sessions[uid].last_input_at.lock().unwrap().is_none(), "{uid}");
        }
        assert_eq!(wait_for(&kitty, 14), b"\x1b[By\x1b[27u\x1b[13u", "codex in kitty mode");
        // Give a stray trailing Enter time to show up: none does.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(std::fs::read(&legacy).unwrap(), b"\x1b[By\x1b\r");
        assert_eq!(std::fs::read(&kitty).unwrap(), b"\x1b[By\x1b[27u\x1b[13u");
        kill_all(&state);
    }

    /// One call, several targets: in scope, out of scope, missing and
    /// exited each get their own result, and the in-scope ones still get
    /// the key — a bad target never stops the rest.
    #[test]
    fn a_bulk_send_reports_each_target_and_never_aborts() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        spawn(&state, "ts-10-0", "ws-a", "codex", "sleep 30"); // the caller (taskless: its workspace)
        recorder(&state, "ts-11-0", "ws-a", "bash", "", &a);
        recorder(&state, "ts-12-0", "ws-a", "bash", "", &b);
        spawn(&state, "ts-13-0", "ws-other", "bash", "sleep 30");
        spawn(&state, "ts-14-0", "ws-a", "bash", "exit 0");
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.lock().unwrap().sessions.get_mut("ts-14-0").unwrap().try_wait().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        settle(&state, "ts-12-0");
        let uids = ["ts-11-0", "ts-13-0", "ts-missing", "ts-14-0", "ts-12-0", "ts-11-0"];
        let reply = send_key(&state, &json!({"session_uids": uids, "key": "1"}), Some("ts-10-0")).unwrap();
        assert_eq!(
            statuses(&reply),
            [("ts-11-0", "ok"), ("ts-13-0", "unauthorized"), ("ts-missing", "not_found"), ("ts-14-0", "exited"), ("ts-12-0", "ok")]
                .map(|(u, s)| (u.to_owned(), s.to_owned())),
            "duplicates collapse; order kept: {reply}"
        );
        assert_eq!(reply["sent"], 2);
        assert_eq!(wait_for(&a, 1), b"1");
        assert_eq!(wait_for(&b, 1), b"1");
        kill_all(&state);
    }

    /// Auth: an out-of-scope single target is refused per target (no key
    /// written); an unknown caller is refused outright; bad keys and
    /// oversized lists are invalid params.
    #[test]
    fn auth_and_parameter_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let other = dir.path().join("other");
        spawn(&state, "ts-20-0", "ws-a", "codex", "sleep 30");
        recorder(&state, "ts-21-0", "ws-b", "bash", "", &other);
        settle(&state, "ts-21-0");
        let reply = send_key(&state, &json!({"session_uids": "ts-21-0", "key": "enter"}), Some("ts-20-0")).unwrap();
        assert_eq!(statuses(&reply), [("ts-21-0".to_owned(), "unauthorized".to_owned())]);
        std::thread::sleep(Duration::from_millis(200));
        assert!(std::fs::read(&other).unwrap_or_default().is_empty(), "nothing written");
        // A global caller reaches it.
        state.lock().unwrap().sessions.get_mut("ts-20-0").unwrap().global_perms = true;
        let reply = send_key(&state, &json!({"session_uids": "ts-21-0", "key": "enter"}), Some("ts-20-0")).unwrap();
        assert_eq!(reply["sent"], 1);
        assert_eq!(wait_for(&other, 1), b"\r");
        let err = send_key(&state, &json!({"session_uids": "ts-21-0", "key": "enter"}), Some("ts-ghost")).unwrap_err();
        assert_eq!(err.0, ErrorCode::Unauthorized);
        for (params, why) in [
            (json!({"session_uids": "ts-21-0", "key": "hello"}), "unknown key"),
            (json!({"session_uids": [], "key": "enter"}), "no targets"),
            (json!({"session_uids": (0..51).map(|i| format!("ts-{i}-0")).collect::<Vec<_>>(), "key": "enter"}), "over the cap"),
        ] {
            assert_eq!(send_key(&state, &params, None).unwrap_err().0, ErrorCode::InvalidParams, "{why}");
        }
        kill_all(&state);
    }

    /// Review of a41f14d (1): the mode survives a brain restart, a wrapped
    /// ring never flips a legacy agent to kitty, and an unknown mode is
    /// assumed from the engine and said so.
    #[test]
    fn the_tracked_mode_survives_restarts_and_wrapped_rings() {
        use crate::session::PtyByteFanout;
        // A kitty agent whose push scrolled out of a small ring long ago.
        let live = PtyByteFanout::new(64);
        live.push(b"\x1b[>1u");
        for _ in 0..50 {
            live.push(b"lots of output that wraps the ring ");
        }
        assert!(!live.snapshot_since(None).bytes.windows(4).any(|w| w == b"[>1u"), "the push is gone from the ring");
        assert_eq!(keyboard_mode(&live, true), (TermMode::DISAMBIGUATE_ESC_CODES, "observed"));
        // Restart: the next brain seeds the tail and the persisted mode.
        let ring = live.snapshot_since(None).bytes;
        let (_, saved) = live.keyboard();
        let adopted = PtyByteFanout::new(64);
        adopted.seed_adopted(Some(&ring), Some(saved));
        assert_eq!(keyboard_mode(&adopted, true), (TermMode::DISAMBIGUATE_ESC_CODES, "observed"));
        // A brain that saved no mode: the tail alone can't tell. Assumed
        // from the engine, until the program sets its mode again.
        let old = PtyByteFanout::new(64);
        old.seed_adopted(Some(&ring), None);
        assert_eq!(keyboard_mode(&old, true), (TermMode::DISAMBIGUATE_ESC_CODES, "assumed"));
        assert_eq!(keyboard_mode(&old, false), (TermMode::empty(), "assumed"));
        old.push(b"\x1b[<u\x1b[=0;1u");
        assert_eq!(keyboard_mode(&old, true), (TermMode::empty(), "observed"));
        // A legacy agent (claude without kitty) whose ring wrapped stays legacy.
        let plain = PtyByteFanout::new(64);
        for _ in 0..50 {
            plain.push(b"claude output with no keyboard push ");
        }
        assert_eq!(keyboard_mode(&plain, true), (TermMode::empty(), "observed"));
        // A push split across two pushes is still seen.
        let split = PtyByteFanout::new(64);
        split.push(b"abc\x1b[>");
        split.push(b"1u");
        assert_eq!(keyboard_mode(&split, true).0, TermMode::DISAMBIGUATE_ESC_CODES);
    }
}
