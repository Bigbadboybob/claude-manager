//! The real daemon binary with a `log_path`: every line in the log carries
//! an RFC3339 UTC millisecond timestamp, the pump child holds nothing but
//! its stdio and the log, and the pump drains and exits when the brain dies.
//!
//! Sandboxed like `reexec_skeleton_e2e`: `env_clear()`, HOME, socket and
//! config inside a tempdir, a test-only operator token. The daemon and its
//! pump are the only processes touched.

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use cm_daemon::control::protocol::{Caller, Request, Response};

fn round_trip(socket: &Path, req: &Request) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let body = serde_json::to_vec(req).expect("serialize request");
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    stream.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf).expect("parse response"))
}

fn children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

fn cmdline(pid: u32) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
        .unwrap_or_default()
}

fn wait_until(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn daemon_log_lines_are_timestamped_by_a_pump_that_outlives_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".cm")).unwrap();
    let socket = dir.path().join("daemon.sock");
    let log = dir.path().join("daemon.log");
    let config = home.join(".cm/daemon.toml");
    std::fs::write(&config, format!("log_path = \"{}\"\n", log.display())).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let token = format!("log-pump-e2e-{}", std::process::id());

    let mut daemon = Command::new(env!("CARGO_BIN_EXE_cm-daemon"))
        .env_clear()
        .env("HOME", &home)
        .env("CM_DAEMON_SOCKET", &socket)
        .env("CM_DAEMON_CONFIG", &config)
        .env("CM_OPERATOR_TOKEN", &token)
        .env("PATH", std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()))
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sandbox cm-daemon");
    let pid = daemon.id();
    let health = Request {
        id: "health".into(),
        caller: Caller::operator(&token),
        method: "daemon.health".into(),
        params: serde_json::json!({}),
    };
    wait_until("daemon.health", 30, || round_trip(&socket, &health).is_ok_and(|r| r.ok));

    let pump = children(pid)
        .into_iter()
        .find(|c| cmdline(*c).contains("--log-pump"))
        .expect("a --log-pump child");
    let fds: Vec<String> = std::fs::read_dir(format!("/proc/{pump}/fd"))
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .collect();
    // Stdio plus the log file it appends to; nothing inherited from the brain.
    for fd in fds.iter().filter_map(|f| f.parse::<u32>().ok()).filter(|fd| *fd > 2) {
        let target = std::fs::read_link(format!("/proc/{pump}/fd/{fd}")).unwrap();
        assert_eq!(target, log, "pump fd {fd} is {target:?}");
    }

    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("(timestamped; daemon.toml log_path)"), "{text}");
    for line in text.lines() {
        let ts = line.split(' ').next().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok() && ts.ends_with('Z') && ts.len() == 24,
            "unstamped line: {line:?}",
        );
    }

    // A SIGKILLed brain: the pump sees EOF and exits by itself.
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    wait_until("the pump to exit", 10, || {
        std::fs::read_to_string(format!("/proc/{pump}/stat"))
            .map_or(true, |s| s.rsplit(')').next().is_some_and(|f| f.trim_start().starts_with('Z')))
    });
}
