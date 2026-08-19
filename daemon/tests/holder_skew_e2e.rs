//! The version-skew scenario (DESIGN_HOLDER_BRAIN_SPLIT § Version-
//! skew testing): one compat-floor exercise runnable over ANY
//! {holder binary} × {brain binary} pairing — driven by
//! `scripts/holder-skew-matrix`, which builds the matrix cells and
//! sets:
//!
//!   CM_SKEW_HOLDER_BIN = path to the cm-holder binary under test
//!   CM_SKEW_BRAIN_BIN  = path to the cm-daemon (brain) binary
//!
//! Without both vars the test SKIPS (passes with a notice) — running
//! `cargo test` normally must not depend on foreign-ref builds. The
//! HEAD × HEAD cell is separately covered by `holder_mode_e2e.rs`,
//! which additionally asserts HEAD-only features (listener-custody
//! log lines, holder_epoch); THIS scenario asserts only the
//! compat FLOOR every supported pairing must clear:
//!
//!   1. hello negotiation succeeds (the pairing speaks a common
//!      version) and the daemon reports split mode;
//!   2. a session spawns through the split and its PTY works;
//!   3. the crash-class headline: SIGKILL the brain → the holder
//!      respawns → re-adopt → same child, PTY continuity;
//!   4. kill_session tears down cleanly.
//!
//! Anything version-specific (custody, checkpoints, provenance
//! detail) is deliberately NOT asserted here — an old holder
//! answering `unsupported_verb` to a new brain's `store_listener` is
//! the additive-only discipline working, not a failure.
//!
//! Helpers are duplicated from `holder_mode_e2e.rs` (test crates
//! can't share modules without path tricks; the repo convention is
//! per-suite re-derivation — see `reexec_skeleton_e2e.rs`).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cm_daemon::control::protocol::{Caller, Request, Response};

fn round_trip(socket: &Path, req: &Request) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let body = serde_json::to_vec(req).expect("serialize request");
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(&body)?;
    stream.flush()?;
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(serde_json::from_slice(&buf).expect("parse response"))
}

fn operator_request(token: &str, method: &str, params: serde_json::Value) -> Request {
    Request {
        id: format!("skew-{}-{}", method, std::process::id()),
        caller: Caller::operator(token),
        method: method.into(),
        params,
    }
}

fn proc_starttime(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

fn proc_ppid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

fn find_child_by_comm(parent: i32, comm_want: &str, deadline: Instant) -> Option<i32> {
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|s| s.parse::<i32>().ok())
                else {
                    continue;
                };
                if proc_ppid(pid) != Some(parent) {
                    continue;
                }
                let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid))
                    .unwrap_or_default();
                if comm.trim() == comm_want {
                    return Some(pid);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

struct SandboxGuard {
    holder: Child,
    bash: Vec<(i32, u64)>,
    log_path: PathBuf,
}

impl SandboxGuard {
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines.len().saturating_sub(50);
        lines[tail..].join("\n")
    }
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        for &(pid, start) in &self.bash {
            if proc_starttime(pid) == Some(start) {
                // SAFETY: our sandbox's child, identity re-verified.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        let _ = self.holder.kill();
        let _ = self.holder.wait();
    }
}

fn wait_for<T>(
    deadline: Instant,
    what: &str,
    guard: &SandboxGuard,
    mut f: impl FnMut() -> Option<T>,
) -> T {
    loop {
        if let Some(v) = f() {
            return v;
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {}.\n--- log tail ---\n{}",
                what,
                guard.log_tail()
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn output_text(resp: &Response) -> Option<String> {
    let b64 = resp.result.as_ref()?.get("bytes")?.as_str()?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn skew_pair_clears_the_compat_floor() {
    let (Some(holder_bin), Some(brain_bin)) = (
        std::env::var_os("CM_SKEW_HOLDER_BIN"),
        std::env::var_os("CM_SKEW_BRAIN_BIN"),
    ) else {
        eprintln!(
            "holder_skew_e2e: CM_SKEW_HOLDER_BIN/CM_SKEW_BRAIN_BIN unset — \
             skipping (run scripts/holder-skew-matrix to exercise the matrix)"
        );
        return;
    };
    assert!(
        Path::new(&holder_bin).exists() && Path::new(&brain_bin).exists(),
        "skew binaries missing: {holder_bin:?} / {brain_bin:?}"
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("mk sandbox HOME");
    let socket = dir.path().join("daemon.sock");
    let log_path = dir.path().join("skew.log");
    let token = format!("skew-{}", std::process::id());

    let log_file = std::fs::File::create(&log_path).expect("create log");
    let log_for_stderr = log_file.try_clone().expect("clone log handle");
    let holder = Command::new(&holder_bin)
        .arg("--brain")
        .arg(&brain_bin)
        .env_clear()
        .env("HOME", &home)
        .env("CM_DAEMON_SOCKET", &socket)
        .env("CM_OPERATOR_TOKEN", &token)
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .current_dir(&home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_for_stderr))
        .spawn()
        .expect("spawn skew cm-holder");
    let holder_pid = holder.id() as i32;
    let mut guard = SandboxGuard {
        holder,
        bash: Vec::new(),
        log_path: log_path.clone(),
    };

    // Floor 1: negotiation + split mode.
    let health = wait_for(
        Instant::now() + Duration::from_secs(45),
        "daemon.health from the skew pair",
        &guard,
        || {
            round_trip(
                &socket,
                &operator_request(&token, "daemon.health", serde_json::json!({})),
            )
            .ok()
            .filter(|r| r.ok)
        },
    );
    assert_eq!(
        health.result.as_ref().and_then(|r| r.get("split")),
        Some(&serde_json::json!(true)),
        "the pair negotiated but does not report split mode: {:?}",
        health.result
    );

    let brain1 = find_child_by_comm(
        holder_pid,
        "cm-daemon",
        Instant::now() + Duration::from_secs(10),
    )
    .expect("brain #1");

    // Floor 2: spawn + PTY.
    let uid = "ts-5e11-1";
    let start = round_trip(
        &socket,
        &operator_request(
            &token,
            "start_session",
            serde_json::json!({
                "uid": uid,
                "workspace_id": "ws-skew",
                "worktree_path": home.to_string_lossy(),
                "label": "skew-bash",
                "argv": ["bash", "--norc"],
                "working_dir": home.to_string_lossy(),
                "session_type": "bash",
                "cols": 100,
                "rows": 30,
                "env": {}
            }),
        ),
    )
    .expect("start_session");
    assert!(start.ok, "spawn failed: {:?}\n{}", start.error, guard.log_tail());
    let bash_pid = find_child_by_comm(
        holder_pid,
        "bash",
        Instant::now() + Duration::from_secs(10),
    )
    .expect("bash parented to the holder");
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    guard.bash.push((bash_pid, bash_start));

    let send = round_trip(
        &socket,
        &operator_request(
            &token,
            "send_input",
            serde_json::json!({ "session_uid": uid, "text": "echo SKEW-PRE", "submit": true }),
        ),
    )
    .expect("send PRE");
    assert!(send.ok);
    wait_for(
        Instant::now() + Duration::from_secs(20),
        "SKEW-PRE in output",
        &guard,
        || {
            let resp = round_trip(
                &socket,
                &operator_request(
                    &token,
                    "read_session_output",
                    serde_json::json!({ "session_uid": uid }),
                ),
            )
            .ok()?;
            output_text(&resp).filter(|t| t.contains("SKEW-PRE"))
        },
    );

    // Floor 3: the crash-class headline.
    // SAFETY: brain #1 is our sandbox holder's child.
    unsafe {
        libc::kill(brain1, libc::SIGKILL);
    }
    wait_for(
        Instant::now() + Duration::from_secs(45),
        "brain #2 with the session re-adopted",
        &guard,
        || {
            let r = round_trip(
                &socket,
                &operator_request(&token, "daemon.health", serde_json::json!({})),
            )
            .ok()
            .filter(|r| r.ok)?;
            (r.result?.get("sessions").and_then(|v| v.as_u64()) == Some(1)).then_some(())
        },
    );
    assert_eq!(proc_starttime(bash_pid), Some(bash_start), "child disturbed");
    assert_eq!(proc_ppid(bash_pid), Some(holder_pid));
    let send = round_trip(
        &socket,
        &operator_request(
            &token,
            "send_input",
            serde_json::json!({ "session_uid": uid, "text": "echo SKEW-POST", "submit": true }),
        ),
    )
    .expect("send POST");
    assert!(send.ok);
    wait_for(
        Instant::now() + Duration::from_secs(20),
        "SKEW-POST in output",
        &guard,
        || {
            let resp = round_trip(
                &socket,
                &operator_request(
                    &token,
                    "read_session_output",
                    serde_json::json!({ "session_uid": uid }),
                ),
            )
            .ok()?;
            output_text(&resp).filter(|t| t.contains("SKEW-POST"))
        },
    );

    // Floor 4: clean teardown.
    let kill = round_trip(
        &socket,
        &operator_request(&token, "kill_session", serde_json::json!({ "session_uid": uid })),
    )
    .expect("kill_session");
    assert!(kill.ok);
    wait_for(
        Instant::now() + Duration::from_secs(20),
        "sessions to reach 0",
        &guard,
        || {
            let r = round_trip(
                &socket,
                &operator_request(&token, "daemon.health", serde_json::json!({})),
            )
            .ok()
            .filter(|r| r.ok)?;
            (r.result?.get("sessions").and_then(|v| v.as_u64()) == Some(0)).then_some(())
        },
    );
}
