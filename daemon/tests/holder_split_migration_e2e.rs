//! End-to-end proof of phase 7 (DESIGN_HOLDER_BRAIN_SPLIT § Live
//! migration + § Holder upgrades): the four maneuvers that move a
//! host between the monolith and the split — each riding live
//! sessions through un-restarted.
//!
//!   1. `daemon.migrate_split`: a REAL monolith with a live bash
//!      session becomes holder+brain at the SAME PID — child
//!      un-reparented, PTY continuous, session count intact.
//!   2. Migration-failure rollback (C3's trusted branch): a
//!      post-validation holder-init failure (forced via the
//!      `CM_HOLDER_TEST_FAIL_MIGRATION_INIT` hook) writes a fresh
//!      standard-schema manifest and execs the pinned monolith back —
//!      session intact.
//!   3. `daemon.split_rollback` (reverse migration): drain + C1
//!      record streaming + the holder's manifest projection → the
//!      monolith again at the holder's PID, session intact.
//!   4. `daemon.upgrade_holder`: the holder re-execs itself with the
//!      holder-upgrade manifest; brain + session untouched, and the
//!      RESTORED holder still supervises (a post-upgrade brain
//!      SIGKILL recovers) — proof the manifest state is real, V3's
//!      incarnation continuity included.
//!
//! Sandbox discipline as in `holder_mode_e2e.rs`: env_clear + tempdir
//! HOME/CM_DAEMON_SOCKET, never the real daemon; children identity-
//! verified via /proc before any kill.
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
        id: format!("split-mig-e2e-{}-{}", method, std::process::id()),
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

fn proc_comm(pid: i32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{}/comm", pid))
        .ok()
        .map(|s| s.trim().to_string())
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
                if proc_comm(pid).as_deref() == Some(comm_want) {
                    return Some(pid);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    None
}

fn output_text(resp: &Response) -> Option<String> {
    let b64 = resp.result.as_ref()?.get("bytes")?.as_str()?;
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn locate_cm_holder() -> PathBuf {
    let daemon_bin = PathBuf::from(env!("CARGO_BIN_EXE_cm-daemon"));
    let holder_bin = daemon_bin.parent().expect("bin dir").join("cm-holder");
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    let status = Command::new(env!("CARGO"))
        .args(["build", "-q", "-p", "cm-holder", "--bin", "cm-holder"])
        .current_dir(&workspace_root)
        .status()
        .expect("run cargo build -p cm-holder");
    assert!(status.success(), "cargo build -p cm-holder failed");
    assert!(holder_bin.exists(), "cm-holder missing after build");
    holder_bin
}

struct SandboxGuard {
    root: Child,
    bash: Vec<(i32, u64)>,
    log_path: PathBuf,
}

impl SandboxGuard {
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(&self.log_path).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let tail = lines.len().saturating_sub(60);
        lines[tail..].join("\n")
    }

    fn log_full(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
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
        let _ = self.root.kill();
        let _ = self.root.wait();
    }
}

/// One sandbox process tree, launched either as a MONOLITH (the
/// daemon binary directly) or SPLIT (cm-holder --brain). The root
/// child keeps its PID across every phase-7 maneuver — that
/// invariance is itself an assertion in each test.
struct Sandbox {
    guard: SandboxGuard,
    socket: PathBuf,
    token: String,
    root_pid: i32,
    home: PathBuf,
    _dir: tempfile::TempDir,
}

fn launch(tag: &str, split: bool, extra_env: &[(&str, &str)]) -> Sandbox {
    let daemon_bin = env!("CARGO_BIN_EXE_cm-daemon");
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("mk sandbox HOME");
    let socket = dir.path().join("daemon.sock");
    let log_path = dir.path().join("sandbox.log");
    let token = format!("split-mig-{tag}-{}", std::process::id());
    let log_file = std::fs::File::create(&log_path).expect("create log");
    let log_for_stderr = log_file.try_clone().expect("clone log handle");
    let mut cmd = if split {
        let holder_bin = locate_cm_holder();
        let mut c = Command::new(holder_bin);
        c.arg("--brain").arg(daemon_bin);
        c
    } else {
        Command::new(daemon_bin)
    };
    cmd.env_clear()
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
        .stderr(Stdio::from(log_for_stderr));
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let root = cmd.spawn().expect("spawn sandbox root");
    let root_pid = root.id() as i32;
    let sb = Sandbox {
        guard: SandboxGuard {
            root,
            bash: Vec::new(),
            log_path,
        },
        socket,
        token,
        root_pid,
        home,
        _dir: dir,
    };
    sb.wait_health();
    sb
}

impl Sandbox {
    fn op(&self, method: &str, params: serde_json::Value) -> Response {
        round_trip(&self.socket, &operator_request(&self.token, method, params))
            .expect("operator round trip")
    }

    fn try_op(&self, method: &str, params: serde_json::Value) -> std::io::Result<Response> {
        round_trip(&self.socket, &operator_request(&self.token, method, params))
    }

    fn wait_for<T>(&self, what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = f() {
                return v;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {}.\n--- sandbox log tail ---\n{}",
                    what,
                    self.guard.log_tail()
                );
            }
            std::thread::sleep(Duration::from_millis(150));
        }
    }

    fn wait_health(&self) {
        self.wait_for("daemon.health", 45, || {
            self.try_op("daemon.health", serde_json::json!({}))
                .ok()
                .filter(|r| r.ok)
        });
    }

    fn health(&self) -> Option<serde_json::Value> {
        self.try_op("daemon.health", serde_json::json!({}))
            .ok()
            .filter(|r| r.ok)
            .and_then(|r| r.result)
    }

    fn split_flag(&self) -> Option<bool> {
        Some(
            self.health()?
                .get("split")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        )
    }

    fn spawn_bash(&mut self, uid: &str, parent: i32) -> i32 {
        let start = self.op(
            "start_session",
            serde_json::json!({
                "uid": uid,
                "workspace_id": "ws-split-mig",
                "worktree_path": self.home.to_string_lossy(),
                "label": uid,
                "argv": ["bash", "--norc"],
                "working_dir": self.home.to_string_lossy(),
                "session_type": "bash",
                "cols": 120,
                "rows": 40,
                "env": {}
            }),
        );
        assert!(
            start.ok,
            "spawn failed: {:?}\n{}",
            start.error,
            self.guard.log_tail()
        );
        let pid = find_child_by_comm(parent, "bash", Instant::now() + Duration::from_secs(10))
            .expect("bash child");
        let started = proc_starttime(pid).expect("starttime");
        self.guard.bash.push((pid, started));
        pid
    }

    fn shell_echo_round_trip(&self, uid: &str, marker: &str) {
        let send = self.op(
            "send_input",
            serde_json::json!({
                "session_uid": uid,
                "text": format!("echo {marker}-$((20+3))"),
                "submit": true
            }),
        );
        assert!(send.ok, "send_input failed: {:?}", send.error);
        let expect = format!("{marker}-23");
        self.wait_for(&format!("'{expect}' in session output"), 20, || {
            let resp = self
                .try_op(
                    "read_session_output",
                    serde_json::json!({ "session_uid": uid }),
                )
                .ok()?;
            output_text(&resp).filter(|t| t.contains(&expect))
        });
    }
}

/// Maneuver 1: monolith → split at the same PID, session riding
/// through.
#[test]
fn migrate_split_rides_a_live_session_through() {
    let holder_bin = locate_cm_holder();
    let daemon_bin = env!("CARGO_BIN_EXE_cm-daemon");
    let mut sb = launch("migrate", false, &[]);
    assert_eq!(sb.split_flag(), Some(false), "launched a monolith");

    let uid = "ts-a1c0-1";
    let bash_pid = sb.spawn_bash(uid, sb.root_pid);
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    sb.shell_echo_round_trip(uid, "PRE-MIGRATE");

    // Fire the migration. The connection dies at the exec on success;
    // a well-formed error response means it FAILED.
    match sb.try_op(
        "daemon.migrate_split",
        serde_json::json!({
            "holder_path": holder_bin.to_string_lossy(),
            "brain_path": daemon_bin,
        }),
    ) {
        Ok(resp) => panic!(
            "migrate_split returned instead of exec'ing: {:?}\n{}",
            resp.error,
            sb.guard.log_tail()
        ),
        Err(_) => {} // connection died at the exec — expected
    }

    // The SAME PID becomes the holder…
    sb.wait_for("the root process to become cm-holder", 30, || {
        (proc_comm(sb.root_pid).as_deref() == Some("cm-holder")).then_some(())
    });
    // …supervising a brain child…
    let brain_pid = find_child_by_comm(
        sb.root_pid,
        "cm-daemon",
        Instant::now() + Duration::from_secs(20),
    )
    .expect("brain child of the holder post-migration");
    assert_ne!(brain_pid, sb.root_pid);
    // …and health reports split with the session still registered.
    sb.wait_for("split health with 1 session", 45, || {
        let h = sb.health()?;
        (h.get("split").and_then(|v| v.as_bool()) == Some(true)
            && h.get("sessions").and_then(|v| v.as_u64()) == Some(1))
        .then_some(())
    });

    // The child never moved: same pid, same start time, same parent
    // (the parent PROCESS changed image, not identity).
    assert_eq!(proc_starttime(bash_pid), Some(bash_start), "bash survived");
    assert_eq!(proc_ppid(bash_pid), Some(sb.root_pid), "bash parent unchanged");

    // PTY continuity through the adopted split.
    sb.shell_echo_round_trip(uid, "POST-MIGRATE");

    // The verb-routed kill works in the migrated world.
    let kill = sb.op("kill_session", serde_json::json!({ "session_uid": uid }));
    assert!(kill.ok, "kill_session post-migration: {:?}", kill.error);
    sb.wait_for("bash to die post-kill", 15, || {
        (proc_starttime(bash_pid) != Some(bash_start)).then_some(())
    });
}

/// Maneuver 2 (C3's trusted branch): a post-validation holder-init
/// failure rolls back to the pinned monolith — session intact,
/// nothing armed, no waitid consumed.
#[test]
fn migration_init_failure_rolls_back_to_the_monolith() {
    let holder_bin = locate_cm_holder();
    let daemon_bin = env!("CARGO_BIN_EXE_cm-daemon");
    // The hook rides the monolith's env across the exec into the
    // holder image, which fails its init AFTER manifest validation.
    let mut sb = launch(
        "migfail",
        false,
        &[("CM_HOLDER_TEST_FAIL_MIGRATION_INIT", "1")],
    );
    let uid = "ts-a1c1-1";
    let bash_pid = sb.spawn_bash(uid, sb.root_pid);
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    sb.shell_echo_round_trip(uid, "PRE-FAIL");

    let _ = sb.try_op(
        "daemon.migrate_split",
        serde_json::json!({
            "holder_path": holder_bin.to_string_lossy(),
            "brain_path": daemon_bin,
        }),
    );

    // The rollback exec lands us back in a serving MONOLITH at the
    // same PID, with the session adopted through the fresh
    // standard-schema manifest.
    sb.wait_for("monolith health after the rollback", 45, || {
        let h = sb.health()?;
        (h.get("split").and_then(|v| v.as_bool()).unwrap_or(false) == false
            && h.get("sessions").and_then(|v| v.as_u64()) == Some(1))
        .then_some(())
    });
    assert_eq!(
        proc_comm(sb.root_pid).as_deref(),
        Some("cm-daemon"),
        "the root image is the monolith again"
    );
    assert_eq!(proc_starttime(bash_pid), Some(bash_start), "bash survived");
    assert_eq!(proc_ppid(bash_pid), Some(sb.root_pid), "bash parent unchanged");
    sb.shell_echo_round_trip(uid, "POST-FAIL");

    let log = sb.guard.log_full();
    assert!(
        log.contains("migration init FAILED after validation"),
        "the failure branch actually ran:\n{}",
        sb.guard.log_tail()
    );
    assert!(
        log.contains("rollback exec"),
        "the rollback exec path actually ran:\n{}",
        sb.guard.log_tail()
    );
}

/// Maneuver 3: split → monolith (reverse migration) at the holder's
/// PID, session riding through the C1 record projection.
#[test]
fn split_rollback_returns_to_a_single_process_daemon() {
    let daemon_bin = env!("CARGO_BIN_EXE_cm-daemon");
    let mut sb = launch("revmig", true, &[("CM_HOLDER_PING_MS", "500")]);
    assert_eq!(sb.split_flag(), Some(true), "launched split");

    let uid = "ts-a1c2-1";
    let bash_pid = sb.spawn_bash(uid, sb.root_pid);
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    sb.shell_echo_round_trip(uid, "PRE-REVERSE");

    let resp = sb.op(
        "daemon.split_rollback",
        serde_json::json!({ "monolith_path": daemon_bin }),
    );
    assert!(
        resp.ok,
        "split_rollback refused: {:?}\n{}",
        resp.error,
        sb.guard.log_tail()
    );

    // The holder's PID becomes the monolith image.
    sb.wait_for("the root process to become cm-daemon", 45, || {
        (proc_comm(sb.root_pid).as_deref() == Some("cm-daemon")).then_some(())
    });
    sb.wait_for("monolith health with 1 session", 45, || {
        let h = sb.health()?;
        (h.get("split").and_then(|v| v.as_bool()).unwrap_or(false) == false
            && h.get("sessions").and_then(|v| v.as_u64()) == Some(1))
        .then_some(())
    });
    // Single-process again: no brain child under the root.
    assert!(
        find_child_by_comm(
            sb.root_pid,
            "cm-daemon",
            Instant::now() + Duration::from_secs(2)
        )
        .is_none(),
        "no residual brain child after the reverse migration"
    );
    assert_eq!(proc_starttime(bash_pid), Some(bash_start), "bash survived");
    assert_eq!(proc_ppid(bash_pid), Some(sb.root_pid), "bash parent unchanged");
    sb.shell_echo_round_trip(uid, "POST-REVERSE");

    // The monolith owns reaping again: kill + tombstone.
    let kill = sb.op("kill_session", serde_json::json!({ "session_uid": uid }));
    assert!(kill.ok, "kill_session post-reverse: {:?}", kill.error);
    sb.wait_for("bash to die post-kill", 15, || {
        (proc_starttime(bash_pid) != Some(bash_start)).then_some(())
    });
}

/// Maneuver 4: the holder re-execs itself (upgrade); the brain and
/// the session never notice — and the RESTORED holder still
/// supervises (a post-upgrade brain SIGKILL recovers), proving the
/// upgrade manifest carried real state.
#[test]
fn upgrade_holder_swaps_the_holder_image_in_place() {
    let mut sb = launch(
        "upgrade",
        true,
        &[
            ("CM_HOLDER_PING_MS", "500"),
            ("CM_HOLDER_STABLE_HORIZON_MS", "1500"),
        ],
    );
    let uid = "ts-a1c3-1";
    let bash_pid = sb.spawn_bash(uid, sb.root_pid);
    let bash_start = proc_starttime(bash_pid).expect("bash starttime");
    sb.shell_echo_round_trip(uid, "PRE-UPGRADE");

    let brain_before = find_child_by_comm(
        sb.root_pid,
        "cm-daemon",
        Instant::now() + Duration::from_secs(10),
    )
    .expect("brain before upgrade");
    let epoch_before = sb
        .health()
        .and_then(|h| h.get("holder_epoch").and_then(|v| v.as_u64()));

    // A distinct on-disk path proves the path→pin plumbing (the
    // image is the same build — identity assertions carry the proof).
    let new_holder = sb.home.join("cm-holder-v2");
    std::fs::copy(locate_cm_holder(), &new_holder).expect("stage new holder");

    let resp = sb.op(
        "daemon.upgrade_holder",
        serde_json::json!({ "holder_path": new_holder.to_string_lossy() }),
    );
    assert!(
        resp.ok,
        "upgrade_holder refused: {:?}\n{}",
        resp.error,
        sb.guard.log_tail()
    );

    // The rehello proves the new image re-negotiated with the SAME
    // brain generation.
    sb.wait_for("the post-upgrade rehello in the log", 30, || {
        sb.guard
            .log_full()
            .contains("post-upgrade rehello answered")
            .then_some(())
    });
    let brain_after = find_child_by_comm(
        sb.root_pid,
        "cm-daemon",
        Instant::now() + Duration::from_secs(10),
    )
    .expect("brain after upgrade");
    assert_eq!(brain_before, brain_after, "the brain never restarted");
    let h = sb.wait_for("healthy split post-upgrade", 30, || sb.health());
    assert_eq!(
        h.get("split").and_then(|v| v.as_bool()),
        Some(true),
        "still split"
    );
    assert_eq!(
        h.get("holder_epoch").and_then(|v| v.as_u64()),
        epoch_before,
        "same brain generation across the holder swap"
    );
    assert_eq!(h.get("sessions").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(proc_starttime(bash_pid), Some(bash_start), "bash survived");
    sb.shell_echo_round_trip(uid, "POST-UPGRADE");

    // The money assertion: the upgraded holder's restored state is
    // REAL — it survives a brain crash exactly like the original
    // (respawn → re-adopt → same child).
    // SAFETY: brain_after is our sandbox's brain, just verified.
    unsafe {
        libc::kill(brain_after, libc::SIGKILL);
    }
    sb.wait_for("a respawned brain to re-adopt the session", 45, || {
        let h = sb.health()?;
        let epoch = h.get("holder_epoch").and_then(|v| v.as_u64())?;
        (epoch == epoch_before.unwrap_or(0) + 1
            && h.get("sessions").and_then(|v| v.as_u64()) == Some(1))
        .then_some(())
    });
    assert_eq!(
        proc_starttime(bash_pid),
        Some(bash_start),
        "bash survived the post-upgrade brain crash"
    );
    sb.shell_echo_round_trip(uid, "POST-CRASH");
}
