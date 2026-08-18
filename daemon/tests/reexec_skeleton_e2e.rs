//! End-to-end proof of the re-exec handoff skeleton:
//! DESIGN_SEAMLESS_RESTART phase 3b, driving the REAL daemon binary
//! (`env!("CARGO_BIN_EXE_cm-daemon")`) through a REAL in-place
//! re-exec — with one live bash session and a live reader draining,
//! the exact condition the phase-1 OS proof deliberately skipped.
//!
//! The load-bearing assertions:
//!   1. the daemon PID is UNCHANGED across `daemon.reexec_dev`
//!      (same process — `Child::try_wait()` would have reaped an
//!      exited daemon);
//!   2. the handoff actually happened (the daemon log carries the
//!      adoption line — same-PID-still-alive alone can't distinguish
//!      "re-exec'd" from "never exec'd");
//!   3. the bash child's PID and kernel start time are unchanged and
//!      it is still parented to the daemon (never signaled, never
//!      respawned);
//!   4. the SAME session uid answers `list_sessions` non-exited;
//!   5. the PTY is WRITABLE and READABLE after the exec: pre-exec
//!      markers (`PRE-*`) drained through the old image's reader,
//!      post-exec markers (`POST-*`) drain through the adopted
//!      session's fresh reader (the fanout ring restarts empty by
//!      design — daemon memory dies at exec).
//!
//! Sandbox discipline (hard constraint): the daemon is spawned with
//! `env_clear()` + an explicit environment — `HOME` inside a
//! tempdir, `CM_DAEMON_SOCKET` inside the same tempdir — so it
//! structurally cannot see the real `~/.cm`. The test additionally
//! VERIFIES the running daemon's `/proc/<pid>/environ` carries the
//! sandbox HOME before asserting anything. The only processes
//! touched are the daemon this test spawned and the bash child that
//! daemon spawned; a panic-safe guard kills both (bash first — a
//! SIGKILLed daemon never runs `DaemonSession::drop`, so its child
//! would otherwise outlive the test), re-verifying the bash pid's
//! start time before signaling so a recycled pid is never hit.
//!
//! Own process (integration test), generous deadlines throughout —
//! this suite runs under load beside the rest of the workspace.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use cm_daemon::control::protocol::{Caller, Request, Response};

/// One operator round trip over a fresh connection: 4-byte BE length
/// prefix + JSON, response in the same framing (mirrors
/// `tests/accept_loop.rs` and `scripts/e2e_bind.py`).
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
        id: format!("e2e-{}-{}", method, std::process::id()),
        caller: Caller::operator(token),
        method: method.into(),
        params,
    }
}

/// Fire a request and EXPECT the connection to die without a
/// response (the `daemon.reexec_dev` success contract: the
/// per-connection socket is CLOEXEC'd by the R9 audit and closes at
/// the exec). Returns `Some(response)` only if the daemon answered —
/// which for reexec_dev means the failure case.
fn fire_expect_drop(socket: &Path, req: &Request) -> Option<Response> {
    let mut stream = UnixStream::connect(socket).expect("connect for fire");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = serde_json::to_vec(req).expect("serialize request");
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .expect("write len");
    stream.write_all(&body).expect("write body");
    stream.flush().expect("flush");
    let mut len = [0u8; 4];
    match stream.read_exact(&mut len) {
        Err(_) => None, // EOF / reset — the connection died at the exec.
        Ok(()) => {
            let len = u32::from_be_bytes(len) as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).expect("read body after len");
            Some(serde_json::from_slice(&buf).expect("parse response"))
        }
    }
}

/// `/proc/<pid>/stat` starttime (field 22) — split after the LAST
/// `)` because comm is unescaped (same parse as the daemon's
/// `adopt::proc_starttime`, re-derived here since the test crate
/// can't reach `pub(crate)` items). `None` when the process is gone
/// or the line doesn't parse.
fn proc_starttime(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// `/proc/<pid>/stat` ppid (field 4 — index 1 after the comm split).
fn proc_ppid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Find the daemon's direct bash child by walking /proc: ppid ==
/// daemon pid && comm == "bash". Retries briefly — the spawn RPC
/// returns before we look.
fn find_bash_child(daemon_pid: i32, deadline: Instant) -> Option<i32> {
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
                if proc_ppid(pid) != Some(daemon_pid) {
                    continue;
                }
                let comm = std::fs::read_to_string(format!("/proc/{}/comm", pid))
                    .unwrap_or_default();
                if comm.trim() == "bash" {
                    return Some(pid);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

/// Panic-safe cleanup: kill the sandbox daemon AND its bash child.
/// Bash first — SIGKILLing the daemon skips every `Drop`, so the
/// child would otherwise leak past the test. The bash pid is only
/// signaled after re-verifying its recorded start time, so a
/// recycled pid can never be hit.
struct SandboxGuard {
    daemon: Child,
    bash_pid: Option<i32>,
    bash_start: Option<u64>,
    log_path: PathBuf,
}

impl SandboxGuard {
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines.len().saturating_sub(40);
        lines[tail..].join("\n")
    }
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        if let (Some(pid), Some(start)) = (self.bash_pid, self.bash_start) {
            if proc_starttime(pid) == Some(start) {
                // SAFETY: our sandbox daemon's child, identity
                // re-verified via starttime the instant before.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// Poll until `f` returns Some, or panic at the deadline with
/// `what` + the daemon log tail for diagnosis.
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
                "timed out waiting for {}.\n--- daemon log tail ---\n{}",
                what,
                guard.log_tail()
            );
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Decode a `read_session_output` result's base64 bytes to a lossy
/// string (PTY output includes control sequences; we only look for
/// plain markers).
fn output_text(resp: &Response) -> Option<String> {
    let b64 = resp.result.as_ref()?.get("bytes")?.as_str()?;
    // Minimal base64 decode without pulling a dep into the test:
    // route through the `base64` crate the daemon already depends on.
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn reexec_skeleton_pty_continuity_end_to_end() {
    let bin = env!("CARGO_BIN_EXE_cm-daemon");
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("mk sandbox HOME");
    let socket = dir.path().join("daemon.sock");
    let log_path = dir.path().join("daemon.log");
    let token = format!(
        "reexec-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );

    // Spawn the REAL daemon with a from-scratch environment: no
    // CM_*/CLAUDE_* inherited from the test runner (env_clear), HOME
    // and the socket pinned inside the tempdir, the dev flag on, and
    // a strong operator token (daemon.reexec_dev fails CLOSED
    // without one — R14).
    let log_file = std::fs::File::create(&log_path).expect("create log");
    let log_for_stderr = log_file.try_clone().expect("clone log handle");
    let daemon = Command::new(bin)
        .env_clear()
        .env("HOME", &home)
        .env("CM_DAEMON_SOCKET", &socket)
        .env("CM_REEXEC", "1")
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
        .expect("spawn sandbox cm-daemon");
    let daemon_pid = daemon.id() as i32;
    let mut guard = SandboxGuard {
        daemon,
        bash_pid: None,
        bash_start: None,
        log_path: log_path.clone(),
    };

    // Sandbox verification (hard constraint): the daemon's OWN
    // environment must carry the tempdir HOME before we assert
    // anything — proof it cannot be looking at the real ~/.cm.
    // Polled briefly: /proc/<pid>/environ reads empty in the window
    // between the fork and the execve completing.
    let expect_home = format!("HOME={}", home.display());
    let environ_deadline = Instant::now() + Duration::from_secs(10);
    let environ = wait_for(
        environ_deadline,
        "daemon /proc environ to populate",
        &guard,
        || {
            std::fs::read_to_string(format!("/proc/{}/environ", daemon_pid))
                .ok()
                .filter(|e| !e.is_empty())
        },
    );
    assert!(
        environ.split('\0').any(|kv| kv == expect_home),
        "sandbox daemon's HOME is not the tempdir — refusing to proceed \
         (environ: {:?})",
        environ
    );

    // Startup (MCP preflight shells out; be generous).
    let startup_deadline = Instant::now() + Duration::from_secs(30);
    wait_for(startup_deadline, "daemon.health to answer", &guard, || {
        round_trip(
            &socket,
            &operator_request(&token, "daemon.health", serde_json::json!({})),
        )
        .ok()
        .filter(|r| r.ok)
    });

    // (1) Start a real BASH session (operator spawn RPC — no
    // claude/MCP anywhere near this test).
    let uid = "ts-e2e0-1";
    let start = round_trip(
        &socket,
        &operator_request(
            &token,
            "start_session",
            serde_json::json!({
                "uid": uid,
                "workspace_id": "ws-reexec-e2e",
                "worktree_path": home.to_string_lossy(),
                "label": "reexec-e2e-bash",
                "argv": ["bash", "--norc"],
                "working_dir": home.to_string_lossy(),
                "session_type": "bash",
                "cols": 120,
                "rows": 40,
                "env": {}
            }),
        ),
    )
    .expect("start_session round trip");
    assert!(
        start.ok,
        "start_session failed: {:?}\n--- daemon log tail ---\n{}",
        start.error,
        guard.log_tail()
    );

    // (2) Pre-exec markers, and prove the OLD image's reader drained
    // them (the live-reader condition).
    let send = round_trip(
        &socket,
        &operator_request(
            &token,
            "send_input",
            serde_json::json!({
                "session_uid": uid,
                "text": "for i in $(seq 1 50); do echo PRE-$i; done",
                "submit": true
            }),
        ),
    )
    .expect("send_input PRE");
    assert!(send.ok, "send_input PRE failed: {:?}", send.error);

    let pre_deadline = Instant::now() + Duration::from_secs(20);
    wait_for(pre_deadline, "PRE-50 in pre-exec output", &guard, || {
        let resp = round_trip(
            &socket,
            &operator_request(
                &token,
                "read_session_output",
                serde_json::json!({ "session_uid": uid }),
            ),
        )
        .ok()?;
        output_text(&resp).filter(|t| t.contains("PRE-50"))
    });

    // (3) Record identities: daemon pid (recorded above) and the
    // bash child's pid + kernel start time.
    let bash_pid = find_bash_child(daemon_pid, Instant::now() + Duration::from_secs(10))
        .unwrap_or_else(|| {
            panic!(
                "no bash child of daemon {} found.\n--- daemon log tail ---\n{}",
                daemon_pid,
                guard.log_tail()
            )
        });
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    guard.bash_pid = Some(bash_pid);
    guard.bash_start = Some(bash_start);
    println!(
        "pre-exec: daemon pid {} | bash child pid {} (starttime {}) | PRE-50 drained",
        daemon_pid, bash_pid, bash_start
    );

    // (3b) The DEEP abort path: a target that opens fine (regular
    // file, exec bit) but cannot exec — a shebang-less text file, so
    // `execveat` itself fails with ENOEXEC AFTER the quiesce, the
    // gate freezes, the sealed manifest, the CLOEXEC audit, and the
    // signal block have all happened. The RPC must answer INLINE
    // (the failure half of the fire-and-verify contract), and the
    // daemon must be indistinguishable from before the call: health
    // shows restarting=false/draining=false, and the PTY still flows
    // (readers thawed, fd flags restored).
    let bogus = dir.path().join("not-actually-a-binary");
    std::fs::write(&bogus, "definitely not an ELF and no shebang\n")
        .expect("write bogus target");
    let mut perms = std::fs::metadata(&bogus).expect("stat bogus").permissions();
    use std::os::unix::fs::PermissionsExt as _;
    perms.set_mode(0o755);
    std::fs::set_permissions(&bogus, perms).expect("chmod bogus");
    let failed = round_trip(
        &socket,
        &operator_request(
            &token,
            "daemon.reexec_dev",
            serde_json::json!({ "binary_path": bogus.to_string_lossy() }),
        ),
    )
    .expect("failure case must answer inline");
    assert!(!failed.ok, "ENOEXEC target must fail: {:?}", failed.result);
    let msg = failed.error.as_ref().map(|e| e.message.as_str()).unwrap_or("");
    assert!(
        msg.contains("execveat"),
        "error should name the execveat failure (deep abort path): {}",
        msg
    );
    let health = round_trip(
        &socket,
        &operator_request(&token, "daemon.health", serde_json::json!({})),
    )
    .expect("health after aborted attempt");
    let hr = health.result.expect("health result");
    assert_eq!(hr["restarting"], serde_json::json!(false), "abort must clear restarting");
    assert_eq!(hr["draining"], serde_json::json!(false), "abort must un-drain");
    // PTY still flows after the abort (readers thawed, flags restored).
    let send = round_trip(
        &socket,
        &operator_request(
            &token,
            "send_input",
            serde_json::json!({
                "session_uid": uid,
                "text": "echo MID-ABORT-OK",
                "submit": true
            }),
        ),
    )
    .expect("send_input MID");
    assert!(send.ok, "send_input MID failed: {:?}", send.error);
    let mid_deadline = Instant::now() + Duration::from_secs(20);
    wait_for(mid_deadline, "MID-ABORT-OK after aborted attempt", &guard, || {
        let resp = round_trip(
            &socket,
            &operator_request(
                &token,
                "read_session_output",
                serde_json::json!({ "session_uid": uid }),
            ),
        )
        .ok()?;
        output_text(&resp).filter(|t| t.contains("MID-ABORT-OK"))
    });
    println!("aborted attempt: inline error + full restore verified ({})", msg);

    // (4) Fire the re-exec into the SAME binary. Success = the
    // connection dies with no response.
    let fired = fire_expect_drop(
        &socket,
        &operator_request(
            &token,
            "daemon.reexec_dev",
            serde_json::json!({ "binary_path": bin }),
        ),
    );
    if let Some(resp) = fired {
        panic!(
            "daemon.reexec_dev answered instead of exec'ing: {:?}\n--- \
             daemon log tail ---\n{}",
            resp,
            guard.log_tail()
        );
    }

    // (5) The new image serves health again — same PID, same live
    // bash child, same session uid.
    let post_deadline = Instant::now() + Duration::from_secs(30);
    wait_for(post_deadline, "daemon.health after re-exec", &guard, || {
        round_trip(
            &socket,
            &operator_request(&token, "daemon.health", serde_json::json!({})),
        )
        .ok()
        .filter(|r| r.ok)
    });

    // Same daemon process: try_wait() reaps an exited child, so
    // Ok(None) proves the ORIGINAL pid is still this live process —
    // exec preserved it.
    assert!(
        matches!(guard.daemon.try_wait(), Ok(None)),
        "daemon process (pid {}) exited across the re-exec.\n--- daemon \
         log tail ---\n{}",
        daemon_pid,
        guard.log_tail()
    );
    // And the handoff genuinely ran (same-pid-alive alone can't
    // distinguish "re-exec'd" from "never exec'd").
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains("re-exec handoff manifest validated"),
        "daemon log carries no handoff-validation line.\n--- daemon log \
         tail ---\n{}",
        guard.log_tail()
    );
    assert!(
        log.contains("adopted 1/1 session(s)"),
        "daemon log carries no adoption line.\n--- daemon log tail ---\n{}",
        guard.log_tail()
    );

    // Same live bash child: pid alive, same kernel start time (not a
    // recycled pid), still parented to the daemon (same-PID exec
    // keeps the parent link).
    assert_eq!(
        proc_starttime(bash_pid),
        Some(bash_start),
        "bash child pid {} is gone or recycled across the re-exec",
        bash_pid
    );
    assert_eq!(
        proc_ppid(bash_pid),
        Some(daemon_pid),
        "bash child pid {} is no longer parented to the daemon",
        bash_pid
    );

    // Same session uid, not exited, in the adopted registry.
    let sessions = round_trip(
        &socket,
        &operator_request(
            &token,
            "list_sessions",
            serde_json::json!({ "include_exited": true }),
        ),
    )
    .expect("list_sessions after re-exec");
    assert!(sessions.ok, "list_sessions failed: {:?}", sessions.error);
    let rows = sessions
        .result
        .as_ref()
        .and_then(|v| v.as_array())
        .expect("list_sessions array");
    let row = rows
        .iter()
        .find(|r| r.get("session_uid").and_then(|v| v.as_str()) == Some(uid))
        .unwrap_or_else(|| {
            panic!(
                "session '{}' missing from post-exec list_sessions: {}\n--- \
                 daemon log tail ---\n{}",
                uid,
                serde_json::to_string_pretty(rows).unwrap_or_default(),
                guard.log_tail()
            )
        });
    assert_ne!(
        row.get("state").and_then(|v| v.as_str()),
        Some("exited"),
        "adopted session reads as exited: {}",
        row
    );

    // (6)+(7) PTY writable AND readable through the ADOPTED session:
    // post-exec markers go in via send_input and come back out of
    // the fresh fanout (which restarted empty by design — the PRE
    // markers died with the old image's ring, so seeing POST here is
    // pure post-exec byte flow).
    let send = round_trip(
        &socket,
        &operator_request(
            &token,
            "send_input",
            serde_json::json!({
                "session_uid": uid,
                "text": "for i in $(seq 1 50); do echo POST-$i; done",
                "submit": true
            }),
        ),
    )
    .expect("send_input POST");
    assert!(
        send.ok,
        "send_input POST failed: {:?}\n--- daemon log tail ---\n{}",
        send.error,
        guard.log_tail()
    );
    let post_out_deadline = Instant::now() + Duration::from_secs(20);
    wait_for(
        post_out_deadline,
        "POST-50 in post-exec output",
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
            output_text(&resp).filter(|t| t.contains("POST-50"))
        },
    );

    println!(
        "post-exec: daemon pid {} unchanged+alive | bash pid {} alive \
         (starttime {} unchanged, ppid {}) | session '{}' state {:?} | \
         POST-50 drained via adopted reader",
        daemon_pid,
        bash_pid,
        bash_start,
        proc_ppid(bash_pid).unwrap_or(-1),
        uid,
        row.get("state").and_then(|v| v.as_str()),
    );

    // Graceful-ish teardown before the guard's SIGKILLs: ask the
    // daemon to kill the session so the reaper path runs; the guard
    // then reaps the daemon itself (and the bash pid if it somehow
    // survived — starttime-verified).
    let _ = round_trip(
        &socket,
        &operator_request(
            &token,
            "kill_session",
            serde_json::json!({ "session_uid": uid }),
        ),
    );
    // Guard drop kills the daemon (ours, sandboxed — killing it is
    // the required cleanup) and the bash child if still alive.
}
