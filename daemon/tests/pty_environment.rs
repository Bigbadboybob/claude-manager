//! Verify the non-holder spawn path with real PTY children. Run through
//! scripts/cm-test-isolated, like the holder-mode counterpart.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use cm_daemon::session::{DaemonSession, SpawnParams};

fn terminal_probe(overrides: &[(&str, &str)]) -> String {
    let dir = tempfile::tempdir().unwrap();
    let mut params = SpawnParams::new("ts-color-probe", "terminal probe", "/bin/bash");
    params.working_dir = Some(dir.path().into());
    params.env = overrides
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    params.args = vec!["--noprofile".into(), "--norc".into(), "-c".into(),
        "{ printf 'TERM=%s\\nCOLORTERM=%s\\nNO_COLOR=%s\\n' \"$TERM\" \"$COLORTERM\" \"$NO_COLOR\"; printf 'COLORS='; tput colors; } > terminal.txt".into()];
    let mut session = DaemonSession::spawn(params).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while session.try_wait().is_none() {
        if Instant::now() >= deadline {
            let _ = session.kill();
            panic!("terminal probe did not exit");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::read_to_string(dir.path().join("terminal.txt")).unwrap()
}

#[test]
fn pty_environment_monolith_advertises_color() {
    let output = terminal_probe(&[]);
    let values: HashMap<_, _> = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    assert_eq!(values.get("TERM"), Some(&"xterm-256color"));
    assert_eq!(values.get("COLORTERM"), Some(&"truecolor"));
    assert_eq!(values.get("COLORS"), Some(&"256"));
}

#[test]
fn pty_environment_monolith_preserves_explicit_overrides() {
    let output = terminal_probe(&[("TERM", "vt100"), ("COLORTERM", ""), ("NO_COLOR", "1")]);
    let values: HashMap<_, _> = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    assert_eq!(values.get("TERM"), Some(&"vt100"));
    assert_eq!(values.get("COLORTERM"), Some(&""));
    assert_eq!(values.get("NO_COLOR"), Some(&"1"));
    assert_eq!(values.get("COLORS"), Some(&"-1"));
}
