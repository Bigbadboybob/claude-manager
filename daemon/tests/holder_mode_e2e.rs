//! End-to-end proof of the holder/brain split's headline property
//! (DESIGN_HOLDER_BRAIN_SPLIT phase 3): **a crash-class brain
//! failure kills no session.** Drives the REAL `cm-holder` binary
//! supervising the REAL `cm-daemon` binary (as the brain) in a
//! sandbox, spawns a bash session THROUGH the split, SIGKILLs the
//! brain mid-life, and proves:
//!
//!   1. the daemon runs split (`daemon.health` → `split: true`, and
//!      `daemon.restart` refuses with the phase-6 pointer);
//!   2. the bash child is parented to the HOLDER, not the brain —
//!      the structural point of the whole design;
//!   3. after the brain's SIGKILL (the crash-class event re-exec
//!      cannot survive), the holder respawns a brain that re-adopts:
//!      same session uid live, same child pid + kernel start time,
//!      still parented to the (unchanged) holder;
//!   4. the PTY is writable and readable across the crash — pre-kill
//!      markers drained through brain #1, post-kill markers drain
//!      through brain #2's adopted reader;
//!   5. `kill_session` still works post-crash (the verb-routed kill
//!      path) and the session tombstones cleanly.
//!
//! Sandbox discipline (hard constraint): the HOLDER is spawned with
//! `env_clear()` + tempdir HOME + tempdir CM_DAEMON_SOCKET (the
//! brain inherits the holder's env), verified via /proc before any
//! assertion. Cleanup kills the bash child (identity re-verified by
//! start time) then the holder; the brain dies with the holder via
//! its PR_SET_PDEATHSIG.
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
        id: format!("holder-e2e-{}-{}", method, std::process::id()),
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

/// Find a direct child of `parent` whose comm matches.
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
        // Killing the holder takes the brain with it (PDEATHSIG).
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
                "timed out waiting for {}.\n--- holder/brain log tail ---\n{}",
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

/// The cm-holder binary: sibling of the cm-daemon test binary in the
/// shared target dir; built on demand (running daemon tests alone
/// doesn't build other packages' bins).
fn locate_cm_holder() -> PathBuf {
    let daemon_bin = PathBuf::from(env!("CARGO_BIN_EXE_cm-daemon"));
    let holder_bin = daemon_bin
        .parent()
        .expect("bin dir")
        .join("cm-holder");
    if holder_bin.exists() {
        return holder_bin;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "cm-holder", "--bin", "cm-holder"])
        .current_dir(&workspace_root)
        .status()
        .expect("run cargo build -p cm-holder");
    assert!(status.success(), "cargo build -p cm-holder failed");
    assert!(holder_bin.exists(), "cm-holder still missing after build");
    holder_bin
}

#[test]
fn brain_crash_kills_no_session_end_to_end() {
    let daemon_bin = env!("CARGO_BIN_EXE_cm-daemon");
    let holder_bin = locate_cm_holder();
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("mk sandbox HOME");
    let socket = dir.path().join("daemon.sock");
    let log_path = dir.path().join("split.log");
    let token = format!(
        "holder-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );

    let log_file = std::fs::File::create(&log_path).expect("create log");
    let log_for_stderr = log_file.try_clone().expect("clone log handle");
    let holder = Command::new(&holder_bin)
        .arg("--brain")
        .arg(daemon_bin)
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
        .expect("spawn sandbox cm-holder");
    let holder_pid = holder.id() as i32;
    let mut guard = SandboxGuard {
        holder,
        bash: Vec::new(),
        log_path: log_path.clone(),
    };

    // Sandbox verification on the HOLDER's environ (the brain
    // inherits it).
    let expect_home = format!("HOME={}", home.display());
    let environ = wait_for(
        Instant::now() + Duration::from_secs(10),
        "holder /proc environ",
        &guard,
        || {
            std::fs::read_to_string(format!("/proc/{}/environ", holder_pid))
                .ok()
                .filter(|e| !e.is_empty())
        },
    );
    assert!(
        environ.split('\0').any(|kv| kv == expect_home),
        "sandbox holder's HOME is not the tempdir — refusing to proceed"
    );

    // (1) The split is live: health answers with split:true.
    let health = wait_for(
        Instant::now() + Duration::from_secs(45),
        "daemon.health from brain #1",
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
    let result = health.result.expect("health result");
    assert_eq!(
        result.get("split"),
        Some(&serde_json::json!(true)),
        "daemon.health must report split mode: {result}"
    );
    assert!(
        result
            .get("holder_build_id")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("cm-holder/")),
        "holder_build_id: {result}"
    );

    // (2) daemon.restart refuses in split mode (the O13-adjacent
    // guard: the monolith primitive must not half-rehydrate a brain).
    let restart = round_trip(
        &socket,
        &operator_request(&token, "daemon.restart", serde_json::json!({})),
    )
    .expect("daemon.restart round trip");
    assert!(!restart.ok, "daemon.restart must refuse in split mode");
    assert!(
        restart
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("split mode")),
        "refusal names the mode: {:?}",
        restart.error
    );

    let brain1 = find_child_by_comm(
        holder_pid,
        "cm-daemon",
        Instant::now() + Duration::from_secs(10),
    )
    .expect("brain #1 pid");

    // (3) Spawn a bash session THROUGH the split.
    let uid = "ts-a11ce5b0-1";
    let start = round_trip(
        &socket,
        &operator_request(
            &token,
            "start_session",
            serde_json::json!({
                "uid": uid,
                "workspace_id": "ws-holder-e2e",
                "worktree_path": home.to_string_lossy(),
                "label": "holder-e2e-bash",
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
        "start_session failed: {:?}\n--- log tail ---\n{}",
        start.error,
        guard.log_tail()
    );

    // The child is parented to the HOLDER — the structural split.
    let bash_pid = find_child_by_comm(
        holder_pid,
        "bash",
        Instant::now() + Duration::from_secs(10),
    )
    .expect("bash child parented to the holder");
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    guard.bash.push((bash_pid, bash_start));

    // (4) Pre-crash markers drain through brain #1's reader.
    let send = round_trip(
        &socket,
        &operator_request(
            &token,
            "send_input",
            serde_json::json!({
                "session_uid": uid,
                "text": "for i in $(seq 1 20); do echo PRE-$i; done",
                "submit": true
            }),
        ),
    )
    .expect("send_input PRE");
    assert!(send.ok, "send_input PRE failed: {:?}", send.error);
    wait_for(
        Instant::now() + Duration::from_secs(20),
        "PRE-20 in pre-crash output",
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
            output_text(&resp).filter(|t| t.contains("PRE-20"))
        },
    );

    // (5) THE CRASH-CLASS EVENT: SIGKILL the brain. Re-exec dies
    // here; the split must not.
    // SAFETY: brain #1 is our sandbox holder's child, found by
    // ppid+comm moments ago.
    unsafe {
        libc::kill(brain1, libc::SIGKILL);
    }

    // (6) The holder respawns; brain #2 adopts. Health answers again
    // with the session still counted.
    let health2 = wait_for(
        Instant::now() + Duration::from_secs(45),
        "daemon.health from brain #2 with the session adopted",
        &guard,
        || {
            let r = round_trip(
                &socket,
                &operator_request(&token, "daemon.health", serde_json::json!({})),
            )
            .ok()
            .filter(|r| r.ok)?;
            let res = r.result.clone()?;
            (res.get("sessions").and_then(|v| v.as_u64()) == Some(1)).then_some(res)
        },
    );
    assert_eq!(health2.get("split"), Some(&serde_json::json!(true)));
    let brain2 = find_child_by_comm(
        holder_pid,
        "cm-daemon",
        Instant::now() + Duration::from_secs(10),
    )
    .expect("brain #2 pid");
    assert_ne!(brain2, brain1, "a fresh brain generation");

    // (7) The child never noticed: same pid, same kernel start time,
    // same parent (the holder).
    assert_eq!(
        proc_starttime(bash_pid),
        Some(bash_start),
        "bash child was disturbed by the brain crash"
    );
    assert_eq!(proc_ppid(bash_pid), Some(holder_pid));

    // The adopt happened (log line from brain #2).
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        log.contains(&format!("holder adopt '{uid}'")),
        "adopt line missing from log.\n--- log tail ---\n{}",
        guard.log_tail()
    );

    // (8) PTY continuity: post-crash markers flow through brain #2's
    // adopted reader.
    let send = round_trip(
        &socket,
        &operator_request(
            &token,
            "send_input",
            serde_json::json!({
                "session_uid": uid,
                "text": "for i in $(seq 1 20); do echo POST-$i; done",
                "submit": true
            }),
        ),
    )
    .expect("send_input POST");
    assert!(send.ok, "send_input POST failed: {:?}", send.error);
    wait_for(
        Instant::now() + Duration::from_secs(20),
        "POST-20 in post-crash output",
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
            output_text(&resp).filter(|t| t.contains("POST-20"))
        },
    );

    // (9) The verb-routed kill works post-crash and tombstones
    // cleanly.
    let kill = round_trip(
        &socket,
        &operator_request(
            &token,
            "kill_session",
            serde_json::json!({ "session_uid": uid }),
        ),
    )
    .expect("kill_session round trip");
    assert!(kill.ok, "kill_session failed: {:?}", kill.error);
    wait_for(
        Instant::now() + Duration::from_secs(20),
        "session count to reach 0 after kill_session",
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
