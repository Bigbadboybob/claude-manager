//! `--verify-handoff` mode of the REAL daemon binary
//! (`env!("CARGO_BIN_EXE_cm-daemon")`) — DESIGN_SEAMLESS_RESTART
//! phase 4c (restart-sequence step 2): the candidate new image runs
//! as a verify-only subprocess against a sealed dry manifest and the
//! current on-disk state, and must
//!
//!   * exit 0 with one PASS line when the manifest validates and
//!     every present state file parses;
//!   * exit 1 with a diagnosis NAMING what failed on a corrupt
//!     manifest fd, a corrupt `daemon-sessions.json`, or a corrupt
//!     workflow-run / continuous-task `state.json`;
//!   * write NOTHING — no socket bind, no state files (proven with a
//!     before/after recursive directory snapshot of the sandbox HOME).
//!
//! Sandbox discipline (same as `reexec_skeleton_e2e.rs`): the child
//! runs with `env_clear()` + HOME inside a tempdir, so it structurally
//! cannot see the real `~/.cm`. The MCP selftest is pointed at a stub
//! server via `CM_MCP_SERVER` (kept OUTSIDE the snapshotted HOME) so
//! the PASS case exercises the preflight's subprocess plumbing without
//! multi-second real-server imports.
//!
//! The manifest fd is passed the same way the daemon's own preflight
//! passes it: the sealed memfd stays CLOEXEC in this (parent) process
//! and the flag is cleared inside the child via `pre_exec`, so the fd
//! crosses at the same number named in argv.

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use cm_daemon::reexec_manifest::{
    write_manifest, ReexecManifest, MANIFEST_SCHEMA_VERSION,
};

/// A structurally valid, sessionless dry manifest. Fake fd numbers are
/// fine: verify-handoff deliberately does NOT probe the fds a manifest
/// names (they reference the PARENT's fd table; roles are re-validated
/// post-exec) — only the envelope + structure are checked.
fn minimal_manifest() -> ReexecManifest {
    ReexecManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        attempt: 0,
        reexec_generation: 1,
        rollback_bin_fd: 10,
        sessions: vec![],
        listener_fd: 11,
        tls_listener_fd: None,
        rollback_schema_version: None,
        split: None,
    }
}

/// Spawn `cm-daemon --verify-handoff <fd>` with the sandbox env,
/// inheriting `fd` by clearing CLOEXEC inside the child (pre_exec) —
/// the same discipline the daemon's own preflight uses.
fn run_verify(home: &Path, fd: &OwnedFd, extra_env: &[(&str, String)]) -> Output {
    let bin = env!("CARGO_BIN_EXE_cm-daemon");
    let raw = fd.as_raw_fd();
    let mut cmd = Command::new(bin);
    cmd.env_clear()
        .env("HOME", home)
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .arg("--verify-handoff")
        .arg(raw.to_string())
        .stdin(Stdio::null());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    // SAFETY: pre_exec runs post-fork in the child; fcntl is
    // async-signal-safe and mutates only the child's fd table.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        cmd.pre_exec(move || {
            let flags = libc::fcntl(raw, libc::F_GETFD);
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.output().expect("run cm-daemon --verify-handoff")
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Recursive listing of every path under `root` with file sizes —
/// the "verify-handoff wrote nothing" snapshot.
fn snapshot(root: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = entry.metadata().ok();
            let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            if path.is_dir() {
                out.push((path.clone(), 0));
                stack.push(path);
            } else {
                out.push((path, len));
            }
        }
    }
    out.sort();
    out
}

/// Write the sandbox HOME's on-disk state the way the OLD binary
/// would have left it: a parseable `daemon-sessions.json`, one
/// workflow run's `state.json`, one continuous task's `state.json`.
fn plant_valid_state(home: &Path) {
    let cm = home.join(".cm");
    std::fs::create_dir_all(&cm).expect("mk .cm");
    // Minimal-but-valid daemon-sessions.json: `{}` parses via the
    // Manifest struct's serde defaults (the same loader startup uses).
    std::fs::write(cm.join("daemon-sessions.json"), "{}\n")
        .expect("write daemon-sessions.json");

    // A real WorkflowRun, serialized with the same serde shape the
    // daemon persists (written manually so this test process never
    // calls the HOME-relative saver).
    let run = cm_daemon::workflow::run::WorkflowRun::new(
        "wf-verify-1".into(),
        "feedback".into(),
        "/tmp/repo".into(),
        Default::default(),
        "worker".into(),
        Default::default(),
        None,
        Default::default(),
        0,
    );
    let run_dir = cm.join("workflow-runs/wf-verify-1");
    std::fs::create_dir_all(&run_dir).expect("mk run dir");
    std::fs::write(
        run_dir.join("state.json"),
        serde_json::to_string_pretty(&run).expect("serialize run"),
    )
    .expect("write run state.json");

    // A minimal continuous task (required fields only — later-phase
    // fields fill from serde defaults, mirroring the module's own
    // forward-compat test fixture).
    let task = serde_json::json!({
        "task_id": "verify-ct-1",
        "label": "verify continuous",
        "host_id": "local",
        "workspace_id": "ws-1",
        "worktree_path": "/tmp/repo",
        "engine": "claude",
        "run_mode": "fresh",
        "schedule": {"kind": "on_demand"},
        "default_prompt": "go",
        "enabled": true,
        "paused": false,
        "started_at": 123
    });
    let task_dir = cm.join("continuous-tasks/verify-ct-1");
    std::fs::create_dir_all(&task_dir).expect("mk task dir");
    std::fs::write(
        task_dir.join("state.json"),
        serde_json::to_string_pretty(&task).expect("serialize task"),
    )
    .expect("write task state.json");
}

/// A stub MCP server the selftest can run in milliseconds. Lives
/// OUTSIDE the snapshotted HOME so its `__pycache__`-free execution
/// can never perturb the no-writes assertion.
fn plant_stub_mcp_server(outside_home: &Path) -> String {
    let stub = outside_home.join("stub_mcp_server.py");
    std::fs::write(
        &stub,
        "import sys\nprint('stub selftest ok', file=sys.stderr)\n",
    )
    .expect("write stub server");
    stub.to_string_lossy().into_owned()
}

/// (i) Valid sealed manifest + parseable state files → exit 0, one
/// PASS line, and NOT ONE byte written under HOME.
#[test]
fn verify_handoff_passes_and_writes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("mk home");
    plant_valid_state(&home);
    let stub = plant_stub_mcp_server(dir.path());

    let fd = write_manifest(&minimal_manifest()).expect("sealed manifest");
    let before = snapshot(&home);
    let out = run_verify(&home, &fd, &[("CM_MCP_SERVER", stub)]);
    let after = snapshot(&home);

    let text = stdout_of(&out);
    assert!(
        out.status.success(),
        "verify must PASS.\nstdout: {}\nstderr: {}",
        text,
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("--verify-handoff: PASS"),
        "PASS line expected: {}",
        text
    );
    assert!(
        text.contains("1 workflow run(s)") && text.contains("1 continuous task(s)"),
        "PASS line reports what it parsed: {}",
        text
    );
    assert_eq!(
        before, after,
        "verify-handoff wrote into the sandbox HOME.\nstdout: {}",
        text
    );
    println!("verify PASS line: {}", text.trim());
}

/// (ii) A corrupt `daemon-sessions.json` → exit 1 with a diagnosis
/// naming the file, and still no writes.
#[test]
fn verify_handoff_fails_on_corrupt_daemon_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".cm")).expect("mk .cm");
    std::fs::write(home.join(".cm/daemon-sessions.json"), "{ definitely not json")
        .expect("write corrupt file");

    let fd = write_manifest(&minimal_manifest()).expect("sealed manifest");
    let before = snapshot(&home);
    let out = run_verify(&home, &fd, &[]);
    let after = snapshot(&home);

    let text = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "must exit 1: {}", text);
    assert!(
        text.contains("--verify-handoff: FAIL") && text.contains("daemon-sessions.json"),
        "diagnosis must name the file: {}",
        text
    );
    assert_eq!(before, after, "failure path wrote into HOME: {}", text);
    println!("corrupt-sessions diagnosis: {}", text.trim());
}

/// (iii) A corrupt manifest fd — a plain temp file instead of a
/// sealed memfd — → exit 1 with a manifest-naming diagnosis; and a
/// missing/garbage fd argument fails the strict parse the same way.
#[test]
fn verify_handoff_fails_on_corrupt_manifest_fd() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("mk home");

    // Not a memfd at all: a plain file full of garbage.
    let garbage_path = dir.path().join("not-a-manifest");
    std::fs::write(&garbage_path, "CMRXgarbage").expect("write garbage");
    let file = std::fs::File::open(&garbage_path).expect("open garbage");
    let fd: OwnedFd = file.into();

    let before = snapshot(&home);
    let out = run_verify(&home, &fd, &[]);
    let after = snapshot(&home);

    let text = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "must exit 1: {}", text);
    assert!(
        text.contains("--verify-handoff: FAIL") && text.contains("manifest"),
        "diagnosis must name the manifest: {}",
        text
    );
    assert_eq!(before, after, "failure path wrote into HOME: {}", text);
    println!("corrupt-manifest diagnosis: {}", text.trim());

    // Garbage fd ARGUMENT: strict digits-only parse refuses.
    let bin = env!("CARGO_BIN_EXE_cm-daemon");
    let out = Command::new(bin)
        .env_clear()
        .env("HOME", &home)
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .arg("--verify-handoff")
        .arg("-1")
        .stdin(Stdio::null())
        .output()
        .expect("run with garbage fd arg");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout_of(&out).contains("FAIL"),
        "garbage fd arg must FAIL: {}",
        stdout_of(&out)
    );
}

/// (iv) A present-but-unparseable workflow-run / continuous-task
/// `state.json` is a FAILURE naming the file — the tolerant startup
/// loader skips these; the strict verify twin must not.
#[test]
fn verify_handoff_fails_on_corrupt_run_and_task_state() {
    // Corrupt workflow run.
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let run_dir = home.join(".cm/workflow-runs/wf-bad");
    std::fs::create_dir_all(&run_dir).expect("mk run dir");
    std::fs::write(run_dir.join("state.json"), "not json at all")
        .expect("write corrupt run state");

    let fd = write_manifest(&minimal_manifest()).expect("sealed manifest");
    let out = run_verify(&home, &fd, &[]);
    let text = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "must exit 1: {}", text);
    assert!(
        text.contains("workflow-runs") && text.contains("state.json"),
        "diagnosis must name the workflow-run state file: {}",
        text
    );
    println!("corrupt-run diagnosis: {}", text.trim());

    // Corrupt continuous task (fresh sandbox so the run above doesn't
    // mask it — workflow runs are checked first).
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let task_dir = home.join(".cm/continuous-tasks/ct-bad");
    std::fs::create_dir_all(&task_dir).expect("mk task dir");
    std::fs::write(task_dir.join("state.json"), "]]]")
        .expect("write corrupt task state");

    let fd = write_manifest(&minimal_manifest()).expect("sealed manifest");
    let out = run_verify(&home, &fd, &[]);
    let text = stdout_of(&out);
    assert_eq!(out.status.code(), Some(1), "must exit 1: {}", text);
    assert!(
        text.contains("continuous-task") && text.contains("state.json"),
        "diagnosis must name the continuous-task state file: {}",
        text
    );
    println!("corrupt-task diagnosis: {}", text.trim());
}
