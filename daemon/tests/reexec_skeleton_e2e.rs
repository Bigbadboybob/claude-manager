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

/// Panic-safe cleanup: kill the sandbox daemon AND its bash
/// children. Bash first — SIGKILLing the daemon skips every `Drop`,
/// so the children would otherwise leak past the test. Each bash pid
/// is only signaled after re-verifying its recorded start time, so a
/// recycled pid can never be hit. (A `Vec` since phase 4b's
/// dead-record test runs two sessions per sandbox.)
struct SandboxGuard {
    daemon: Child,
    /// `(pid, recorded starttime)` per tracked bash child.
    bash: Vec<(i32, u64)>,
    log_path: PathBuf,
}

impl SandboxGuard {
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines.len().saturating_sub(40);
        lines[tail..].join("\n")
    }

    fn track_bash(&mut self, pid: i32, start: u64) {
        self.bash.push((pid, start));
    }
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        for &(pid, start) in &self.bash {
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
        bash: Vec::new(),
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
    guard.track_bash(bash_pid, bash_start);
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
    // Phase 4b: the adopted row carries the ORIGINAL identity — the
    // 3b/4a skeleton hard-noted `"bash"` and re-used the uid as the
    // label; the v2 record restores both for real (the full identity
    // matrix — task binding, perms, transcript, done_report — is
    // exercised by `reexec_full_record_and_dead_record_tombstone`).
    assert_eq!(
        row.get("label").and_then(|v| v.as_str()),
        Some("reexec-e2e-bash"),
        "adopted session lost its label: {}",
        row
    );
    assert_eq!(
        row.get("type").and_then(|v| v.as_str()),
        Some("bash"),
        "adopted session lost its session_type: {}",
        row
    );
    assert_eq!(
        row.get("workspace_id").and_then(|v| v.as_str()),
        Some("ws-reexec-e2e"),
        "adopted session lost its workspace binding: {}",
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

// ===================================================================
// Phase 4a: the failure ladder (rollback exec, attempt machine,
// terminal fallback) — DESIGN_SEAMLESS_RESTART "Failure classes,
// exhaustively", driven end-to-end through the real binary via the
// CM_RESTART_TEST_FAIL_REHYDRATE knob (honored only under the
// CM_REEXEC=1 dev flag; the knob string NAMES the attempts to fail,
// so it can ride the env across both execs and still scope itself).
// ===================================================================

/// Sandbox daemon for the ladder tests — same discipline as the
/// skeleton test above (env_clear + tempdir HOME/socket, /proc
/// environ verification before anything is asserted, panic-safe kill
/// guard), factored so each scenario reads as its ladder walk.
struct Sandbox {
    /// Owns the tempdir for the sandbox's lifetime.
    _dir: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
    log_path: PathBuf,
    token: String,
    daemon_pid: i32,
    guard: SandboxGuard,
}

/// Spawn the real daemon with the sandbox environment plus
/// `extra_env` (the fail knob), verify its /proc environ carries the
/// tempdir HOME, and wait for health.
fn spawn_sandbox(extra_env: &[(&str, &str)]) -> Sandbox {
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
    let log_file = std::fs::File::create(&log_path).expect("create log");
    let log_for_stderr = log_file.try_clone().expect("clone log handle");
    let mut cmd = Command::new(bin);
    cmd.env_clear()
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
        .stderr(Stdio::from(log_for_stderr));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let daemon = cmd.spawn().expect("spawn sandbox cm-daemon");
    let daemon_pid = daemon.id() as i32;
    let guard = SandboxGuard {
        daemon,
        bash: Vec::new(),
        log_path: log_path.clone(),
    };

    // Sandbox verification (hard constraint): the daemon's OWN
    // environment must carry the tempdir HOME before we assert
    // anything — proof it cannot be looking at the real ~/.cm.
    let expect_home = format!("HOME={}", home.display());
    let environ = wait_for(
        Instant::now() + Duration::from_secs(10),
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

    let sb = Sandbox {
        _dir: dir,
        home,
        socket,
        log_path,
        token,
        daemon_pid,
        guard,
    };
    // Startup (MCP preflight shells out; generous — the ladder tests
    // run beside the skeleton test's own sandbox daemon).
    let startup_deadline = Instant::now() + Duration::from_secs(45);
    wait_for(startup_deadline, "daemon.health to answer", &sb.guard, || {
        round_trip(
            &sb.socket,
            &operator_request(&sb.token, "daemon.health", serde_json::json!({})),
        )
        .ok()
        .filter(|r| r.ok)
    });
    sb
}

/// Start one bash session, prove the live reader drains (PRE
/// markers), and record the child's identity into the panic-safe
/// guard. Returns `(bash_pid, bash_start)`.
fn start_bash_session(sb: &mut Sandbox, uid: &str) -> (i32, u64) {
    let start = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "start_session",
            serde_json::json!({
                "uid": uid,
                "workspace_id": "ws-reexec-e2e",
                "worktree_path": sb.home.to_string_lossy(),
                "label": "reexec-e2e-bash",
                "argv": ["bash", "--norc"],
                "working_dir": sb.home.to_string_lossy(),
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
        sb.guard.log_tail()
    );

    let send = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
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
    let socket = sb.socket.clone();
    let token = sb.token.clone();
    wait_for(pre_deadline, "PRE-50 in pre-exec output", &sb.guard, || {
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

    let bash_pid =
        find_bash_child(sb.daemon_pid, Instant::now() + Duration::from_secs(10))
            .unwrap_or_else(|| {
                panic!(
                    "no bash child of daemon {} found.\n--- daemon log tail \
                     ---\n{}",
                    sb.daemon_pid,
                    sb.guard.log_tail()
                )
            });
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    sb.guard.track_bash(bash_pid, bash_start);
    (bash_pid, bash_start)
}

/// (a) The ROLLBACK round trip: `CM_RESTART_TEST_FAIL_REHYDRATE=0`
/// fails ONLY the attempt-0 image's rehydrate (after validation,
/// before commit — the escrow is intact), which must then exec the
/// pinned rollback binary with attempt 1; the rollback image sees the
/// same knob but a different attempt number, so it rehydrates
/// cleanly. Same daemon PID through TWO execs, crash note names the
/// attempt-0 failure (and no attempt-1 one), the bash child rides
/// through both execs untouched, and the PTY flows post-rollback.
#[test]
fn reexec_rollback_round_trip_via_fail_knob() {
    let bin = env!("CARGO_BIN_EXE_cm-daemon");
    let mut sb = spawn_sandbox(&[("CM_RESTART_TEST_FAIL_REHYDRATE", "0")]);
    let uid = "ts-e2eb-1";
    let (bash_pid, bash_start) = start_bash_session(&mut sb, uid);
    println!(
        "rollback e2e pre-exec: daemon pid {} | bash child pid {} \
         (starttime {})",
        sb.daemon_pid, bash_pid, bash_start
    );

    // Fire. Success = the connection dies at the (first) exec.
    let fired = fire_expect_drop(
        &sb.socket,
        &operator_request(
            &sb.token,
            "daemon.reexec_dev",
            serde_json::json!({ "binary_path": bin }),
        ),
    );
    if let Some(resp) = fired {
        panic!(
            "daemon.reexec_dev answered instead of exec'ing: {:?}\n--- \
             daemon log tail ---\n{}",
            resp,
            sb.guard.log_tail()
        );
    }

    // exec #1 → attempt-0 image fails rehydrate (knob) → rollback
    // exec #2 → attempt-1 image rehydrates. Generous: two full
    // startups (two MCP preflights) under suite load.
    let post_deadline = Instant::now() + Duration::from_secs(120);
    wait_for(
        post_deadline,
        "daemon.health after the rollback round trip",
        &sb.guard,
        || {
            round_trip(
                &sb.socket,
                &operator_request(&sb.token, "daemon.health", serde_json::json!({})),
            )
            .ok()
            .filter(|r| r.ok)
        },
    );

    // Same daemon process through BOTH execs.
    assert!(
        matches!(sb.guard.daemon.try_wait(), Ok(None)),
        "daemon process (pid {}) exited across the rollback round trip.\n\
         --- daemon log tail ---\n{}",
        sb.daemon_pid,
        sb.guard.log_tail()
    );

    // The ladder actually walked: attempt-0 validation, the injected
    // failure, the rollback exec, the attempt-1 validation, and a
    // clean attempt-1 adoption (the knob must NOT fire at attempt 1).
    let log = std::fs::read_to_string(&sb.log_path).unwrap_or_default();
    for needle in [
        "session(s), attempt 0)",                              // attempt-0 manifest validated
        "CM_RESTART_TEST_FAIL_REHYDRATE=0 matched attempt 0",  // injected failure
        "ROLLBACK EXEC",                                       // the rollback execveat
        "session(s), attempt 1)",                              // attempt-1 manifest validated
        "adopted 1/1 session(s)",                              // rollback image committed
    ] {
        assert!(
            log.contains(needle),
            "daemon log missing {:?}.\n--- daemon log tail ---\n{}",
            needle,
            sb.guard.log_tail()
        );
    }
    assert!(
        !log.contains("matched attempt 1"),
        "the fail knob fired in the ROLLBACK image — its value must scope \
         it to attempt 0.\n--- daemon log tail ---\n{}",
        sb.guard.log_tail()
    );

    // Crash note: exists, names the attempt-0 failure and the
    // rollback action; no attempt-1 failure was recorded.
    let note =
        std::fs::read_to_string(sb.home.join(".cm/reexec-crash-note.log"))
            .expect("crash note must exist after a rollback");
    println!("rollback e2e crash note:\n{}", note.trim_end());
    assert!(
        note.contains("attempt 0 failed"),
        "crash note must name attempt 0: {:?}",
        note
    );
    assert!(
        note.contains("exec pinned rollback binary with attempt 1"),
        "crash note must record the rollback action: {:?}",
        note
    );
    assert!(
        !note.contains("attempt 1 failed"),
        "crash note records an attempt-1 failure — the rollback image \
         should have rehydrated cleanly: {:?}",
        note
    );

    // The bash child rode through BOTH execs untouched: same pid,
    // same kernel start time, still parented to the daemon.
    assert_eq!(
        proc_starttime(bash_pid),
        Some(bash_start),
        "bash child pid {} is gone or recycled across the rollback round trip",
        bash_pid
    );
    assert_eq!(
        proc_ppid(bash_pid),
        Some(sb.daemon_pid),
        "bash child pid {} is no longer parented to the daemon",
        bash_pid
    );

    // Same session uid, adopted (not exited), and the PTY flows
    // post-rollback: POST markers drain through the rollback image's
    // fresh reader.
    let sessions = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "list_sessions",
            serde_json::json!({ "include_exited": true }),
        ),
    )
    .expect("list_sessions after rollback");
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
                "session '{}' missing post-rollback: {}\n--- daemon log \
                 tail ---\n{}",
                uid,
                serde_json::to_string_pretty(rows).unwrap_or_default(),
                sb.guard.log_tail()
            )
        });
    assert_ne!(
        row.get("state").and_then(|v| v.as_str()),
        Some("exited"),
        "adopted session reads as exited after the rollback: {}",
        row
    );
    let send = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "send_input",
            serde_json::json!({
                "session_uid": uid,
                "text": "for i in $(seq 1 50); do echo POST-$i; done",
                "submit": true
            }),
        ),
    )
    .expect("send_input POST");
    assert!(send.ok, "send_input POST failed: {:?}", send.error);
    let post_out_deadline = Instant::now() + Duration::from_secs(20);
    wait_for(
        post_out_deadline,
        "POST-50 in post-rollback output",
        &sb.guard,
        || {
            let resp = round_trip(
                &sb.socket,
                &operator_request(
                    &sb.token,
                    "read_session_output",
                    serde_json::json!({ "session_uid": uid }),
                ),
            )
            .ok()?;
            output_text(&resp).filter(|t| t.contains("POST-50"))
        },
    );

    println!(
        "rollback e2e post-exec: daemon pid {} unchanged through 2 execs | \
         bash pid {} alive (starttime {} unchanged) | attempt ladder \
         0-fail → rollback → 1-commit | POST-50 drained",
        sb.daemon_pid, bash_pid, bash_start
    );

    // Teardown through the daemon so the reaper path runs.
    let _ = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "kill_session",
            serde_json::json!({ "session_uid": uid }),
        ),
    );
}

/// (b) The TERMINAL fallback: `CM_RESTART_TEST_FAIL_REHYDRATE=0,1`
/// fails the attempt-0 image AND the rollback image, so the ladder
/// ends in the deliberate-kill terminal fallback. The daemon must
/// come back SERVING (same PID through both execs, health answers,
/// inherited listener adopted), the bash child must be DELIBERATELY
/// killed AND fully reaped (no zombie — a zombie would still show
/// the recorded starttime in /proc), the log must carry the
/// kill-then-reap line with the session's uid + pid, and the session
/// row must not be FALSELY live (a live row is legitimate only when
/// legacy restore respawned a NEW child behind it).
#[test]
fn reexec_terminal_fallback_after_double_failure() {
    let bin = env!("CARGO_BIN_EXE_cm-daemon");
    let mut sb = spawn_sandbox(&[("CM_RESTART_TEST_FAIL_REHYDRATE", "0,1")]);
    let uid = "ts-e2ec-1";
    let (bash_pid, bash_start) = start_bash_session(&mut sb, uid);
    println!(
        "terminal e2e pre-exec: daemon pid {} | bash child pid {} \
         (starttime {})",
        sb.daemon_pid, bash_pid, bash_start
    );

    // Fire. Success = the connection dies at the (first) exec.
    let fired = fire_expect_drop(
        &sb.socket,
        &operator_request(
            &sb.token,
            "daemon.reexec_dev",
            serde_json::json!({ "binary_path": bin }),
        ),
    );
    if let Some(resp) = fired {
        panic!(
            "daemon.reexec_dev answered instead of exec'ing: {:?}\n--- \
             daemon log tail ---\n{}",
            resp,
            sb.guard.log_tail()
        );
    }

    // exec #1 → attempt 0 fails → rollback exec #2 → attempt 1 fails
    // → terminal fallback (kill+reap) → legacy restore → serving.
    let post_deadline = Instant::now() + Duration::from_secs(120);
    wait_for(
        post_deadline,
        "daemon.health after the terminal fallback",
        &sb.guard,
        || {
            round_trip(
                &sb.socket,
                &operator_request(&sb.token, "daemon.health", serde_json::json!({})),
            )
            .ok()
            .filter(|r| r.ok)
        },
    );

    // Same daemon process through BOTH execs — the terminal fallback
    // RETURNS into normal startup; it never exits the process.
    assert!(
        matches!(sb.guard.daemon.try_wait(), Ok(None)),
        "daemon process (pid {}) exited across the terminal-fallback walk.\n\
         --- daemon log tail ---\n{}",
        sb.daemon_pid,
        sb.guard.log_tail()
    );

    // The full ladder in the log: both injected failures, the
    // rollback exec between them, the terminal fallback with the
    // deliberate kill-then-reap line naming uid + pid, and the
    // legacy-restore wiring line.
    let log = std::fs::read_to_string(&sb.log_path).unwrap_or_default();
    let kill_line = format!(
        "deliberately SIGKILLed and reaped session '{}' child pid {}",
        uid, bash_pid
    );
    for needle in [
        "CM_RESTART_TEST_FAIL_REHYDRATE=0,1 matched attempt 0",
        "ROLLBACK EXEC",
        "CM_RESTART_TEST_FAIL_REHYDRATE=0,1 matched attempt 1",
        "TERMINAL FALLBACK",
        kill_line.as_str(),
        "running legacy restore_sessions",
    ] {
        assert!(
            log.contains(needle),
            "daemon log missing {:?}.\n--- daemon log tail ---\n{}",
            needle,
            sb.guard.log_tail()
        );
    }

    // Crash note records BOTH failures.
    let note =
        std::fs::read_to_string(sb.home.join(".cm/reexec-crash-note.log"))
            .expect("crash note must exist after the ladder walk");
    println!("terminal e2e crash note:\n{}", note.trim_end());
    println!("terminal e2e kill line observed: {}", kill_line);
    assert!(
        note.contains("attempt 0 failed") && note.contains("attempt 1 failed"),
        "crash note must name both failed attempts: {:?}",
        note
    );

    // The child was deliberately killed AND fully reaped: no process
    // with the recorded (pid, starttime) identity remains — a zombie
    // (killed but unreaped) would still show our starttime in /proc.
    assert_ne!(
        proc_starttime(bash_pid),
        Some(bash_start),
        "bash child pid {} still exists with its original starttime — \
         either it survived the terminal fallback or its zombie was never \
         reaped.\n--- daemon log tail ---\n{}",
        bash_pid,
        sb.guard.log_tail()
    );

    // The session row is not FALSELY live: a live row must be backed
    // by a live NEW child (legacy restore respawning bash fresh is
    // the designed outcome — "killed deliberately, then resumed");
    // the killed child's identity must not stand behind any row.
    let sessions = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "list_sessions",
            serde_json::json!({ "include_exited": true }),
        ),
    )
    .expect("list_sessions after terminal fallback");
    let rows = sessions
        .result
        .as_ref()
        .and_then(|v| v.as_array())
        .expect("list_sessions array");
    let row = rows
        .iter()
        .find(|r| r.get("session_uid").and_then(|v| v.as_str()) == Some(uid));
    let row_state = row
        .and_then(|r| r.get("state"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if row.is_some() && row_state.as_deref() != Some("exited") {
        let new_bash = find_bash_child(
            sb.daemon_pid,
            Instant::now() + Duration::from_secs(15),
        )
        .unwrap_or_else(|| {
            panic!(
                "session '{}' reads live but no live bash child backs it — \
                 falsely live.\n--- daemon log tail ---\n{}",
                uid,
                sb.guard.log_tail()
            )
        });
        let new_start = proc_starttime(new_bash).expect("restored bash starttime");
        assert!(
            new_bash != bash_pid || new_start != bash_start,
            "the live row is backed by the KILLED child's identity — \
             falsely live"
        );
        // Hand the restored child to the panic-safe guard, then tear
        // down through the daemon so the reaper path runs.
        sb.guard.track_bash(new_bash, new_start);
        let _ = round_trip(
            &sb.socket,
            &operator_request(
                &sb.token,
                "kill_session",
                serde_json::json!({ "session_uid": uid }),
            ),
        );
        println!(
            "terminal e2e: row live and honestly backed by restored bash \
             pid {} (old {} killed+reaped)",
            new_bash, bash_pid
        );
    } else {
        println!(
            "terminal e2e: row state {:?} (restore did not resurrect it) — \
             honest either way; old bash {} killed+reaped",
            row_state, bash_pid
        );
    }

    println!(
        "terminal e2e post-exec: daemon pid {} unchanged through 2 execs | \
         ladder 0-fail → rollback → 1-fail → terminal kill+reap of pid {} | \
         daemon serving",
        sb.daemon_pid, bash_pid
    );
}

// ===================================================================
// Phase 4b: full session records across the handoff (R11) + honest
// tombstones for records whose child exited during the swap.
// ===================================================================

/// All the daemon's direct bash children right now (one /proc walk,
/// no retry — callers diff before/after a spawn to map uid → pid
/// when more than one session is live).
fn find_all_bash_children(daemon_pid: i32) -> Vec<i32> {
    let mut out = Vec::new();
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
                out.push(pid);
            }
        }
    }
    out
}

/// `/proc/<pid>/stat` state (field 3 — index 0 after the comm split):
/// `"Z"` for a zombie. `None` when the process is fully gone.
fn proc_state(pid: i32) -> Option<String> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().next().map(|s| s.to_string())
}

/// (c) Phase 4b: the FULL record rides the manifest, and a record
/// whose child exited during the swap gets an honest tombstone.
///
/// Session A is spawned with everything the operator spawn RPC can
/// set — label, type, workspace, task binding, managed_by,
/// global_perms, transcript_path — and then calls `report_done`
/// (Session-caller frame over the same socket; the uid is the
/// caller identity on the local transport). Post-swap its
/// list_sessions row must carry ALL of it: the R11 case is
/// `reported_done` surviving so an `until="final"` watcher doesn't
/// see the worker regress to plain awaiting-input.
///
/// Session B's child is SIGKILLed *by the test* while the swap is
/// frozen mid-exec: the re-exec targets a WRAPPER script that
/// blocks on a FIFO before exec'ing the real daemon binary, so
/// between the two execs there is a deterministic window where the
/// process has no reapers at all — the kill lands there, B's child
/// parks as a zombie child of the wrapper, and the new image's
/// rehydrate finds a manifest record whose child exited during the
/// swap. Post-swap: an exited tombstone for B (killed=false — the
/// daemon never signaled it; provenance is a swap exit, not a
/// kill), and NO zombie (the pid fully reaped via waitid(P_PIDFD)).
#[test]
fn reexec_full_record_and_dead_record_tombstone() {
    let bin = env!("CARGO_BIN_EXE_cm-daemon");
    let mut sb = spawn_sandbox(&[]);

    // ---- Session A: full identity + a real report_done. ----
    let uid_a = "ts-e2ed-1";
    // A transcript file the row's `state: "ready"` derivation keys
    // off (the daemon never opens it — it only echoes the path).
    let transcript_path = sb.home.join("fake-transcript.jsonl");
    std::fs::write(&transcript_path, "{}\n").expect("touch transcript");
    let start = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "start_session",
            serde_json::json!({
                "uid": uid_a,
                "workspace_id": "ws-reexec-e2e",
                "worktree_path": sb.home.to_string_lossy(),
                "label": "identity-bash",
                "argv": ["bash", "--norc"],
                "working_dir": sb.home.to_string_lossy(),
                "session_type": "bash",
                "task_id": "task-e2e-identity",
                "managed_by_uid": "ts-e2ed-parent",
                "global_perms": true,
                "transcript_path": transcript_path.to_string_lossy(),
                "cols": 120,
                "rows": 40,
                "env": {}
            }),
        ),
    )
    .expect("start_session A");
    assert!(
        start.ok,
        "start_session A failed: {:?}\n--- daemon log tail ---\n{}",
        start.error,
        sb.guard.log_tail()
    );
    let a_pid = find_bash_child(sb.daemon_pid, Instant::now() + Duration::from_secs(10))
        .expect("bash child for session A");
    let a_start = proc_starttime(a_pid).expect("A starttime");
    sb.guard.track_bash(a_pid, a_start);

    // Prove A's reader is live (the PRE condition all these e2es keep).
    let send = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "send_input",
            serde_json::json!({
                "session_uid": uid_a,
                "text": "echo PRE-A-OK",
                "submit": true
            }),
        ),
    )
    .expect("send_input PRE A");
    assert!(send.ok, "send_input PRE A failed: {:?}", send.error);
    {
        let socket = sb.socket.clone();
        let token = sb.token.clone();
        wait_for(
            Instant::now() + Duration::from_secs(20),
            "PRE-A-OK in pre-exec output",
            &sb.guard,
            || {
                let resp = round_trip(
                    &socket,
                    &operator_request(
                        &token,
                        "read_session_output",
                        serde_json::json!({ "session_uid": uid_a }),
                    ),
                )
                .ok()?;
                output_text(&resp).filter(|t| t.contains("PRE-A-OK"))
            },
        );
    }

    // The agent's own final report, via the wire (Session-caller
    // frame — report_done is Session-callable only). AFTER the PRE
    // input, so the live superseded rule keeps it CURRENT.
    let report = round_trip(
        &sb.socket,
        &Request {
            id: "e2e-report-done".into(),
            caller: Caller::session(uid_a),
            method: "report_done".into(),
            params: serde_json::json!({
                "reason": "identity e2e final report"
            }),
        },
    )
    .expect("report_done round trip");
    assert!(report.ok, "report_done failed: {:?}", report.error);
    assert_eq!(
        report
            .result
            .as_ref()
            .and_then(|r| r.get("status"))
            .and_then(|v| v.as_str()),
        Some("reported"),
        "report_done result: {:?}",
        report.result
    );

    // ---- Session B: plain bash, doomed to die mid-swap. ----
    let uid_b = "ts-e2ed-2";
    let start = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "start_session",
            serde_json::json!({
                "uid": uid_b,
                "workspace_id": "ws-reexec-e2e",
                "worktree_path": sb.home.to_string_lossy(),
                "label": "doomed-bash",
                "argv": ["bash", "--norc"],
                "working_dir": sb.home.to_string_lossy(),
                "session_type": "bash",
                "cols": 80,
                "rows": 24,
                "env": {}
            }),
        ),
    )
    .expect("start_session B");
    assert!(start.ok, "start_session B failed: {:?}", start.error);
    // Map uid_b → pid by set difference against A's known pid.
    let b_pid = {
        let socket_deadline = Instant::now() + Duration::from_secs(10);
        wait_for(socket_deadline, "session B's bash child", &sb.guard, || {
            find_all_bash_children(sb.daemon_pid)
                .into_iter()
                .find(|p| *p != a_pid)
        })
    };
    let b_start = proc_starttime(b_pid).expect("B starttime");
    sb.guard.track_bash(b_pid, b_start);
    println!(
        "4b e2e pre-exec: daemon pid {} | A bash {} (start {}) | B bash {} \
         (start {})",
        sb.daemon_pid, a_pid, a_start, b_pid, b_start
    );

    // ---- The mid-swap gate: a wrapper that parks between execs. ----
    // exec #1 replaces the daemon with this script (same PID, no
    // reapers anywhere); it blocks opening the FIFO until the test
    // releases it, then execs the real binary, which rehydrates from
    // the inherited manifest env. The kill below lands in that
    // window, so B's record is live in the manifest but its child is
    // a zombie by validation time — the deterministic version of "a
    // child exited during the swap". `read` is a builtin and the
    // redirect-open is what blocks: the script forks NOTHING, so
    // nothing can reap B's zombie before the new image does it
    // deliberately.
    let fifo = sb.home.join("swap-gate.fifo");
    {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: plain mkfifo on a path inside our tempdir.
        let ret = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(ret, 0, "mkfifo: {}", std::io::Error::last_os_error());
    }
    let wrapper = sb.home.join("swap-gate-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nread _ < '{}'\nexec '{}'\n",
            fifo.display(),
            bin
        ),
    )
    .expect("write wrapper");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(
            &wrapper,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod wrapper");
    }

    // Fire at the wrapper. Success = the connection dies at exec #1.
    let fired = fire_expect_drop(
        &sb.socket,
        &operator_request(
            &sb.token,
            "daemon.reexec_dev",
            serde_json::json!({ "binary_path": wrapper.to_string_lossy() }),
        ),
    );
    if let Some(resp) = fired {
        panic!(
            "daemon.reexec_dev answered instead of exec'ing: {:?}\n--- \
             daemon log tail ---\n{}",
            resp,
            sb.guard.log_tail()
        );
    }

    // The swap is now frozen at the FIFO. Kill B's child — identity
    // re-verified via starttime first (never signal a recycled pid);
    // it must park as a ZOMBIE (nothing can reap it: the wrapper
    // forked nothing and the old image's reapers died at the exec).
    assert_eq!(
        proc_starttime(b_pid),
        Some(b_start),
        "B's bash vanished before the mid-swap kill"
    );
    // SAFETY: our sandbox daemon's child, starttime-verified above.
    unsafe {
        libc::kill(b_pid, libc::SIGKILL);
    }
    wait_for(
        Instant::now() + Duration::from_secs(10),
        "B's child to park as a zombie mid-swap",
        &sb.guard,
        || (proc_state(b_pid).as_deref() == Some("Z")).then_some(()),
    );
    println!(
        "4b e2e: B (pid {}) is a zombie mid-swap; releasing the gate",
        b_pid
    );

    // Release the gate: exec #2 (the real binary) runs rehydrate.
    std::fs::write(&fifo, "go\n").expect("release swap gate");

    let post_deadline = Instant::now() + Duration::from_secs(60);
    wait_for(post_deadline, "daemon.health after the swap", &sb.guard, || {
        round_trip(
            &sb.socket,
            &operator_request(&sb.token, "daemon.health", serde_json::json!({})),
        )
        .ok()
        .filter(|r| r.ok)
    });

    // Same daemon process through both execs.
    assert!(
        matches!(sb.guard.daemon.try_wait(), Ok(None)),
        "daemon process (pid {}) exited across the swap.\n--- daemon log \
         tail ---\n{}",
        sb.daemon_pid,
        sb.guard.log_tail()
    );
    let log = std::fs::read_to_string(&sb.log_path).unwrap_or_default();
    for needle in [
        "adopted 1/2 session(s)",
        "exited during the re-exec swap",
        "tombstoned",
    ] {
        assert!(
            log.contains(needle),
            "daemon log missing {:?}.\n--- daemon log tail ---\n{}",
            needle,
            sb.guard.log_tail()
        );
    }

    // ---- A: the full identity round-tripped. ----
    let sessions = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "list_sessions",
            serde_json::json!({ "include_exited": true }),
        ),
    )
    .expect("list_sessions after swap");
    assert!(sessions.ok, "list_sessions failed: {:?}", sessions.error);
    let rows = sessions
        .result
        .as_ref()
        .and_then(|v| v.as_array())
        .expect("list_sessions array");
    let row_a = rows
        .iter()
        .find(|r| r.get("session_uid").and_then(|v| v.as_str()) == Some(uid_a))
        .unwrap_or_else(|| {
            panic!(
                "session A missing post-swap: {}\n--- daemon log tail ---\n{}",
                serde_json::to_string_pretty(rows).unwrap_or_default(),
                sb.guard.log_tail()
            )
        });
    let s = |k: &str| {
        row_a.get(k).and_then(|v| v.as_str()).map(|s| s.to_string())
    };
    assert_eq!(s("label").as_deref(), Some("identity-bash"), "row A: {}", row_a);
    assert_eq!(s("type").as_deref(), Some("bash"), "row A: {}", row_a);
    assert_eq!(
        s("task_id").as_deref(),
        Some("task-e2e-identity"),
        "row A: {}",
        row_a
    );
    assert_eq!(
        s("managed_by_uid").as_deref(),
        Some("ts-e2ed-parent"),
        "row A: {}",
        row_a
    );
    assert_eq!(
        s("workspace_id").as_deref(),
        Some("ws-reexec-e2e"),
        "row A: {}",
        row_a
    );
    assert_eq!(
        row_a.get("global_perms").and_then(|v| v.as_bool()),
        Some(true),
        "global_perms lost across the swap: {}",
        row_a
    );
    // transcript_path carried → the state derivation reads "ready"
    // (a 4a amnesiac read "pending" forever).
    assert_eq!(s("state").as_deref(), Some("ready"), "row A: {}", row_a);
    // THE R11 assertion: the done report survived the swap.
    assert_eq!(
        row_a.get("reported_done").and_then(|v| v.as_bool()),
        Some(true),
        "report_done regressed across the swap (R11): {}",
        row_a
    );
    assert_eq!(
        s("report_reason").as_deref(),
        Some("identity e2e final report"),
        "row A: {}",
        row_a
    );
    // And A's child rode through untouched.
    assert_eq!(proc_starttime(a_pid), Some(a_start), "A's child disturbed");
    assert_eq!(proc_ppid(a_pid), Some(sb.daemon_pid), "A reparented");

    // ---- B: honest tombstone, no zombie. ----
    let row_b = rows
        .iter()
        .find(|r| r.get("session_uid").and_then(|v| v.as_str()) == Some(uid_b))
        .unwrap_or_else(|| {
            panic!(
                "session B has no tombstone post-swap: {}\n--- daemon log \
                 tail ---\n{}",
                serde_json::to_string_pretty(rows).unwrap_or_default(),
                sb.guard.log_tail()
            )
        });
    assert_eq!(
        row_b.get("state").and_then(|v| v.as_str()),
        Some("exited"),
        "row B: {}",
        row_b
    );
    assert_eq!(
        row_b.get("label").and_then(|v| v.as_str()),
        Some("doomed-bash"),
        "tombstone lost B's label: {}",
        row_b
    );
    // Provenance: an exit during the swap, NOT a kill — the daemon
    // never signaled this child (the test did, playing the role of
    // "exited on its own mid-swap").
    assert_eq!(
        row_b.get("killed").and_then(|v| v.as_bool()),
        Some(false),
        "swap-exit tombstone must not claim a kill: {}",
        row_b
    );
    assert!(
        row_b
            .get("exited_at")
            .and_then(|v| v.as_f64())
            .is_some_and(|t| t > 0.0),
        "tombstone has no exited_at: {}",
        row_b
    );
    // Fully reaped: no process (zombie included) holds B's identity.
    assert_ne!(
        proc_starttime(b_pid),
        Some(b_start),
        "B's pid still holds its starttime — the zombie was never reaped.\n\
         --- daemon log tail ---\n{}",
        sb.guard.log_tail()
    );

    // ---- A's PTY still flows through the adopted reader. ----
    let send = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "send_input",
            serde_json::json!({
                "session_uid": uid_a,
                "text": "echo POST-A-OK",
                "submit": true
            }),
        ),
    )
    .expect("send_input POST A");
    assert!(send.ok, "send_input POST A failed: {:?}", send.error);
    {
        let socket = sb.socket.clone();
        let token = sb.token.clone();
        wait_for(
            Instant::now() + Duration::from_secs(20),
            "POST-A-OK in post-swap output",
            &sb.guard,
            || {
                let resp = round_trip(
                    &socket,
                    &operator_request(
                        &token,
                        "read_session_output",
                        serde_json::json!({ "session_uid": uid_a }),
                    ),
                )
                .ok()?;
                output_text(&resp).filter(|t| t.contains("POST-A-OK"))
            },
        );
    }

    println!(
        "4b e2e post-swap: daemon pid {} unchanged | A adopted with full \
         identity (label/type/task/global_perms/transcript/reported_done) | \
         B tombstoned (state=exited, killed=false) + fully reaped | \
         POST-A-OK drained",
        sb.daemon_pid
    );

    // Teardown through the daemon so the reaper path runs for A.
    let _ = round_trip(
        &sb.socket,
        &operator_request(
            &sb.token,
            "kill_session",
            serde_json::json!({ "session_uid": uid_a }),
        ),
    );
}
