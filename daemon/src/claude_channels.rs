//! Opt-in native Claude channels and their narrowly scoped startup consent.
//! This never handles tool approvals and never runs on adopted live sessions.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::state::DaemonState;
use crate::workflow::pty_tracker::PtyModeTracker;

pub const FLAG: &str = "--dangerously-load-development-channels";
pub const ENTRY: &str = "server:claude-manager";

/// Host-owned preference, sampled only when a fresh/resumed process launches.
/// Default stays socket until the operator enables the preview on that host.
pub fn enabled() -> bool {
    std::env::var_os("HOME")
        .and_then(|h| std::fs::read(Path::new(&h).join(".cm/claude-notifications.json")).ok())
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v["transport"] == "channel")
}

/// Read the launch's frozen MCP config so argv and capability cannot disagree
/// if the host preference changes between writing the config and building argv.
pub fn args(config: &Path) -> Vec<String> {
    let opted_in = std::fs::read(config).ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v["mcpServers"]["claude-manager"]["env"]["CM_CLAUDE_CHANNEL"] == "1");
    if opted_in { vec![FLAG.into(), ENTRY.into()] } else { Vec::new() }
}

fn only_cm_channel(argv: &[String]) -> bool {
    let flags: Vec<_> = argv.iter().enumerate().filter(|(_, a)| a.starts_with(FLAG)).collect();
    flags.len() == 1 && flags[0].1 == FLAG
        && argv.get(flags[0].0 + 1).is_some_and(|a| a == ENTRY)
        && argv.get(flags[0].0 + 2).is_none_or(|a| a.starts_with('-'))
}

fn is_cm_consent(screen: &str) -> bool {
    let text: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
    text.contains("WARNING:Loadingdevelopmentchannels")
        && text.contains("--dangerously-load-development-channelsisforlocalchanneldevelopmentonly.")
        && text.contains("Channels:server:claude-manager❯1.Iamusingthisforlocaldevelopment2.Exit")
        && text.contains("Entertoconfirm")
}

/// Arm only from the successful spawn funnel. One bounded watcher, one Enter,
/// and only this exact dialog with CM as its sole development-channel entry.
/// Any human input cancels automation; the writer rechecks under its mutex.
pub fn accept_own_startup(state: &Arc<Mutex<DaemonState>>, uid: &str, argv: &[String]) {
    if !only_cm_channel(argv) { return; }
    let (input, fanout, cols, rows) = {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(s) = st.sessions.get(uid) else { return; };
        if s.session_type != "claude-code" { return; }
        (s.input_handle(), Arc::clone(&s.fanout), s.last_cols, s.last_rows)
    };
    let uid = uid.to_owned();
    let rx = fanout.subscribe();
    let _ = std::thread::Builder::new().name(format!("cm-channel-consent-{uid}")).spawn(move || {
        let start = Instant::now();
        let mut tracker = PtyModeTracker::with_size(cols as usize, rows as usize);
        while start.elapsed() < Duration::from_secs(30) {
            if input.operator_quiet_for().is_some() { return; }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(bytes) => tracker.feed(&bytes, Instant::now()),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {},
            }
            // Match the CURRENT rendered screen, never accumulated raw output
            // where an already-dismissed prompt would remain in scrollback.
            let screen = tracker.visible_text();
            if is_cm_consent(&screen) {
                match input.write_startup_confirmation(tracker.enter_bytes()) {
                    Ok(true) => eprintln!("cm-daemon: accepted own CM channel startup confirmation for {uid}"),
                    Ok(false) => {},
                    Err(e) => eprintln!("cm-daemon: CM channel startup confirmation failed for {uid}: {e}"),
                }
                return;
            }
            let compact: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
            if compact.contains("ClaudeCodev") { return; } // already at the client, not onboarding
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    const SCREEN: &str = "WARNING: Loading development channels\n\
        --dangerously-load-development-channels is for local channel development only.\n\
        Channels: server:claude-manager\n❯ 1. I am using this for local development\n2. Exit\nEnter to confirm";

    #[test]
    fn accepts_only_the_cm_channel_dialog() {
        assert!(is_cm_consent(SCREEN));
        for bad in [SCREEN.replace(ENTRY, "server:other"),
                    SCREEN.replace(ENTRY, "server:claude-manager, server:other"),
                    SCREEN.replace("❯ 1.", "1."),
                    "Do you want to proceed? 1. Yes 2. No".into()] {
            assert!(!is_cm_consent(&bad));
        }
    }

    #[test]
    fn no_other_development_entries_or_flags_are_accepted() {
        let args = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(only_cm_channel(&args(&["claude", FLAG, ENTRY, "--resume", "saved"])));
        for bad in [vec!["claude"], vec!["claude", FLAG, ENTRY, "server:other"],
                    vec!["claude", FLAG, "server:other"], vec!["claude", FLAG, ENTRY, FLAG, ENTRY]] {
            assert!(!only_cm_channel(&args(&bad)));
        }
    }

    #[test]
    fn stale_screen_does_not_accept_a_second_prompt() {
        let mut tracker = PtyModeTracker::with_size(160, 30);
        tracker.feed(SCREEN.replace('\n', "\r\n").as_bytes(), Instant::now());
        assert!(is_cm_consent(&tracker.visible_text()));
        tracker.feed(b"\x1b[2J\x1b[HDo you want to proceed? 1. Yes 2. No", Instant::now());
        assert!(!is_cm_consent(&tracker.visible_text()));
    }

    #[test]
    fn startup_confirmation_never_submits_human_input() {
        let (input, writes) = crate::session::InputHandle::test_handle_capturing();
        input.stamp_operator_input_at(Instant::now());
        assert!(!input.write_startup_confirmation(b"\r").unwrap());
        assert!(writes.lock().unwrap().is_empty());
        let (input, writes) = crate::session::InputHandle::test_handle_capturing();
        assert!(input.write_startup_confirmation(b"\r").unwrap());
        assert_eq!(writes.lock().unwrap()[0].1, b"\r");
    }
}
