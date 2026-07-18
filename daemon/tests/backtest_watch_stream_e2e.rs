//! Synthetic end-to-end for the planning-panel "watch backtest" feature
//! (Track B). The feature spawns a session whose command is a
//! `tmux attach -r -t backtest` — the operator watches a cloud backtest's
//! root-owned tmux, READ-ONLY, rendered like any other CM session.
//!
//! This test proves the load-bearing mechanism with NO cloud and NO
//! gcloud hop: spawn a session (via the daemon's real
//! `DaemonSession::spawn` + `PtyByteFanout` — the exact spawn+byte-stream
//! primitive a `start_session` RPC drives under the hood) whose command
//! is `tmux attach -r -t <session>`, pointed at a LOCAL tmux running
//! visibly-changing content, and assert that live content streams out of
//! the fanout. Read-only (`-r`) is asserted structurally by the command
//! builder's own unit tests (`app::backtest_watch_tests`); here we prove
//! the attach renders and streams.
//!
//! Isolation: a DEDICATED tmux server (`tmux -L <unique>`) is used, so
//! the user's default tmux server is never touched. Cleanup kills only
//! this test's session and tmux server.
//!
//! Skips (does not fail) when `tmux` is unavailable.

use std::process::Command;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use cm_daemon::session::{DaemonSession, SpawnParams};

/// Resolve tmux's absolute path (portable-pty's exec is happier with an
/// explicit program path than a bare name). `None` when tmux is absent.
fn tmux_path() -> Option<String> {
    let out = Command::new("sh").args(["-c", "command -v tmux"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() { None } else { Some(p) }
}

/// Drain the fanout receiver until `pred(acc)` holds or `deadline`
/// passes, returning everything accumulated.
fn read_until(
    rx: &Receiver<Vec<u8>>,
    deadline: Instant,
    pred: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let mut acc = Vec::new();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                if pred(&acc) {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    acc
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn watch_attach_streams_live_tmux_content_read_only() {
    let tmux = match tmux_path() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: tmux not available on PATH");
            return;
        }
    };

    // Unique, isolated tmux server socket + session so this never
    // collides with (or disturbs) any other tmux server.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let uniq = format!("cmbt-{}-{}", std::process::id(), nanos);
    let sock = format!("cmbtsock-{}", std::process::id());
    let session_name = "watch";

    // A pane that keeps printing a changing marker so the stream has
    // continuously updating content to observe.
    let loop_cmd = "i=0; while true; do echo BTWATCH-$i; i=$((i+1)); sleep 0.2; done";
    let created = Command::new(&tmux)
        .args(["-L", &sock, "new-session", "-d", "-s", session_name, "-x", "120", "-y", "40"])
        .arg(format!("bash -c '{}'", loop_cmd))
        .status()
        .expect("spawn tmux new-session")
        .success();
    assert!(created, "tmux new-session -L {sock} must succeed");

    // Give the pane a moment to produce output before attaching.
    std::thread::sleep(Duration::from_millis(400));

    // Spawn the "watch" session: `tmux -L <sock> attach -r -t watch`.
    // `-r` (read-only) mirrors the production command; DaemonSession
    // supplies the PTY tmux needs to render.
    let mut params = SpawnParams::new(format!("ts-btwatch-{uniq}"), "watch e2e", tmux.clone());
    params.args = vec![
        "-L".into(),
        sock.clone(),
        "attach".into(),
        "-r".into(),
        "-t".into(),
        session_name.into(),
    ];
    let mut session = DaemonSession::spawn(params).expect("spawn tmux-attach session");
    let rx = session.fanout.subscribe();

    // Read until we see the marker rendered by the attached tmux pane.
    let deadline = Instant::now() + Duration::from_secs(8);
    let bytes = read_until(&rx, deadline, |acc| contains(acc, b"BTWATCH-"));
    let saw_marker = contains(&bytes, b"BTWATCH-");

    // Best-effort "content is live/changing": look for two distinct
    // counter values in the streamed frames. Not required for the pass
    // (a single marker already proves streaming), but strengthens the
    // evidence when present.
    let text = String::from_utf8_lossy(&bytes);
    let distinct: std::collections::HashSet<&str> = text
        .match_indices("BTWATCH-")
        .filter_map(|(i, _)| {
            let tail = &text[i + "BTWATCH-".len()..];
            let end = tail.find(|c: char| !c.is_ascii_digit()).unwrap_or(tail.len());
            if end > 0 { Some(&tail[..end]) } else { None }
        })
        .collect();

    // ---- cleanup FIRST (so a failed assert still tears down) ----
    let _ = session.kill();
    let _ = Command::new(&tmux).args(["-L", &sock, "kill-server"]).status();

    assert!(
        saw_marker,
        "expected live 'BTWATCH-' content streamed from the read-only tmux attach; \
         got {} bytes: {:?}",
        bytes.len(),
        text,
    );
    eprintln!(
        "watch-attach e2e: streamed {} bytes, {} distinct counter value(s) observed",
        bytes.len(),
        distinct.len(),
    );
}
