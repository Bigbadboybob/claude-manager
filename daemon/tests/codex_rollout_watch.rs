//! Integration tests for the codex live-rollout resolver + rotation
//! re-stamp — DESIGN_SEAMLESS_RESTART phase 4f (codex lineage).
//!
//! Codex rollout ids rotate on `/compact` and codex runs no cm Stop
//! hook, so the daemon's resume key went stale the moment a codex
//! session compacted. Phase 4f observes the rollout file the codex
//! process actually holds OPEN (`/proc/<pid>/fd`, walked over the
//! bounded descendant tree) and re-stamps the resume identity through
//! the canonical `set_transcript_path` flow when it changes.
//!
//! Proven here against real fakes — bash children that hold rollout-
//! shaped files open the way codex does — never a real codex install:
//!
//! 1. **Resolver** — `scan_live_codex_rollout` finds the open rollout
//!    through a wrapper level (outer bash → inner bash holding fd 3,
//!    the same one-level-down shape as the npm `codex` JS launcher →
//!    native binary), and follows a close+reopen rotation to the new
//!    file.
//! 2. **Most-recently-modified tie-break** — two rollouts open at once
//!    resolve to the newer one.
//! 3. **Re-stamp end to end** — `observe_codex_rollout_once` driven
//!    directly against a `DaemonState` holding a bash child typed
//!    `codex`: first observation stamps (generation 0→1) and persists
//!    the extracted uuid resume key to daemon-sessions.json; a scripted
//!    rotation re-stamps (1→2) and re-persists; non-codex sessions and
//!    removed sessions terminate the watch (`NotCodex` /
//!    `SessionGone`).
//!
//! Lives in `tests/` (own process) per the repo convention for
//! child-spawning suites (see `adopt_candidate.rs`); deadlines are
//! generous because the suite runs under machine load. Only /proc
//! entries of children spawned by THIS test process are read.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cm_daemon::session::{DaemonSession, SpawnParams};
use cm_daemon::state::DaemonState;
use cm_daemon::transcript_detect::{
    observe_codex_rollout_once, scan_live_codex_rollout, RolloutObserveOutcome,
};

const UUID_A: &str = "0199aaaa-1111-2222-3333-444444444444";
const UUID_B: &str = "0199bbbb-5555-6666-7777-888888888888";

/// Lay out a rollout-shaped date bucket under a tempdir "HOME" and
/// return `(bucket, rollout_a, rollout_b)`. The resolver matches on
/// path SHAPE (`…/.codex/sessions/YYYY/MM/DD/*.jsonl`), so no HOME
/// env override is needed anywhere in this suite.
fn rollout_fixture(home: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let bucket = home.join(".codex/sessions/2026/08/18");
    std::fs::create_dir_all(&bucket).unwrap();
    let a = bucket.join(format!("rollout-2026-08-18T10-00-00-{}.jsonl", UUID_A));
    let b = bucket.join(format!("rollout-2026-08-18T10-05-00-{}.jsonl", UUID_B));
    (bucket, a, b)
}

/// The holder script: open rollout `$1` on fd 3, touch ready-marker
/// `$2`, wait for rotate-trigger `$3`, close + reopen onto rollout
/// `$4`, touch rotated-marker `$5`, wait for exit-trigger `$6`.
/// Marker files (not stdout) signal progress so the same script works
/// under a piped `Command` child and under a `DaemonSession` PTY.
const HOLDER_SCRIPT: &str = r#"
exec 3>>"$1"
: >"$2"
until [ -e "$3" ]; do sleep 0.05; done
exec 3>&-
exec 3>>"$4"
: >"$5"
until [ -e "$6" ]; do sleep 0.05; done
"#;

/// Write `HOLDER_SCRIPT` to disk and return its path.
fn write_holder_script(dir: &Path) -> PathBuf {
    let path = dir.join("holder.sh");
    std::fs::write(&path, HOLDER_SCRIPT).unwrap();
    path
}

/// The argv tail that runs `holder.sh` one wrapper level down —
/// outer bash spawns an inner bash (the trailing `:` stops the outer
/// from exec-replacing itself), mirroring the npm `codex` JS launcher
/// that `spawn`s the native binary as a child.
fn wrapped_holder_args(
    script: &Path,
    a: &Path,
    ready: &Path,
    trig: &Path,
    b: &Path,
    rotated: &Path,
    exit_trig: &Path,
) -> Vec<String> {
    vec![
        "-c".into(),
        r#"bash "$0" "$1" "$2" "$3" "$4" "$5" "$6"; :"#.into(),
        script.to_str().unwrap().into(),
        a.to_str().unwrap().into(),
        ready.to_str().unwrap().into(),
        trig.to_str().unwrap().into(),
        b.to_str().unwrap().into(),
        rotated.to_str().unwrap().into(),
        exit_trig.to_str().unwrap().into(),
    ]
}

fn wait_for_file(path: &Path, deadline: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Kill + reap a plain `Command` child, tolerating already-exited.
fn cleanup_child(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// 1. Resolver follows the open fd through a wrapper level and
///    tracks a close+reopen rotation to the new rollout.
#[test]
fn resolver_finds_open_rollout_through_wrapper_and_follows_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let (_bucket, a, b) = rollout_fixture(tmp.path());
    let script = write_holder_script(tmp.path());
    let ready = tmp.path().join("ready");
    let trig = tmp.path().join("rotate-now");
    let rotated = tmp.path().join("rotated");
    let exit_trig = tmp.path().join("exit-now");

    let child = Command::new("bash")
        .args(wrapped_holder_args(
            &script, &a, &ready, &trig, &b, &rotated, &exit_trig,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wrapped holder");

    wait_for_file(&ready, Duration::from_secs(10));
    // The fd is open before the ready marker lands, and the holder is
    // the INNER bash — one level below the pid we scan.
    let scan = scan_live_codex_rollout(child.id());
    assert!(!scan.permission_denied, "own children must be readable");
    assert_eq!(
        scan.rollout.as_deref(),
        Some(a.as_path()),
        "resolver must find the rollout the wrapped child holds open",
    );

    // Rotate: close A, reopen onto B.
    std::fs::write(&trig, b"").unwrap();
    wait_for_file(&rotated, Duration::from_secs(10));
    let scan = scan_live_codex_rollout(child.id());
    assert_eq!(
        scan.rollout.as_deref(),
        Some(b.as_path()),
        "post-rotation the resolver must return the NEW rollout",
    );

    std::fs::write(&exit_trig, b"").unwrap();
    cleanup_child(child);
}

/// 2. Two rollouts open at once (rotation overlap) resolve to the
///    most recently modified — here the direct child holds both, so
///    this also covers depth-0 resolution.
#[test]
fn resolver_picks_most_recently_modified_when_two_are_open() {
    let tmp = tempfile::tempdir().unwrap();
    let (_bucket, a, b) = rollout_fixture(tmp.path());
    let ready = tmp.path().join("ready");
    let exit_trig = tmp.path().join("exit-now");
    // Open A, wait long enough for a distinct mtime, open B (newer),
    // then park until told to exit. No wrapper: the DIRECT child is
    // the holder.
    let script = r#"
exec 3>>"$1"
sleep 0.3
exec 4>>"$2"
: >"$3"
until [ -e "$4" ]; do sleep 0.05; done
"#;
    let script_path = tmp.path().join("multi.sh");
    std::fs::write(&script_path, script).unwrap();
    let child = Command::new("bash")
        .arg(&script_path)
        .arg(&a)
        .arg(&b)
        .arg(&ready)
        .arg(&exit_trig)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn multi holder");

    wait_for_file(&ready, Duration::from_secs(10));
    let scan = scan_live_codex_rollout(child.id());
    assert_eq!(
        scan.rollout.as_deref(),
        Some(b.as_path()),
        "with A and B both open, the newer-mtime rollout (B) wins",
    );

    std::fs::write(&exit_trig, b"").unwrap();
    cleanup_child(child);
}

/// Extract `sessions[].transcript_id` for `uid` from a persisted
/// daemon-sessions.json.
fn persisted_transcript_id(path: &Path, uid: &str) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let workspaces = v.get("workspaces")?.as_object()?;
    for ws in workspaces.values() {
        for sess in ws.get("sessions")?.as_array()? {
            if sess.get("uid").and_then(|u| u.as_str()) == Some(uid) {
                return sess
                    .get("transcript_id")
                    .and_then(|t| t.as_str())
                    .map(str::to_string);
            }
        }
    }
    None
}

/// 3. End-to-end re-stamp: drive the periodic observation directly
///    against a `DaemonState` whose codex-typed session is a bash
///    child holding rollout files open, asserting the in-memory
///    stamp, the generation bumps, the persisted uuid resume key,
///    and the terminal outcomes.
#[test]
fn observe_stamps_persists_and_follows_rotation_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let (_bucket, a, b) = rollout_fixture(tmp.path());
    let script = write_holder_script(tmp.path());
    let ready = tmp.path().join("ready");
    let trig = tmp.path().join("rotate-now");
    let rotated = tmp.path().join("rotated");
    let exit_trig = tmp.path().join("exit-now");
    let sessions_json = tmp.path().join("daemon-sessions.json");

    let state = Arc::new(Mutex::new(DaemonState::new()));
    state.lock().unwrap().daemon_sessions_path = Some(sessions_json.clone());

    // A bash child in the same wrapped shape as test 1, TYPED codex —
    // exactly the "mark a bash session codex in test state" harness
    // the phase-4f spec calls for (no real codex binary anywhere).
    let uid = "ts-4f00-c0de";
    let mut sp = SpawnParams::new(uid, "codex-under-test", "/bin/bash");
    sp.args = wrapped_holder_args(&script, &a, &ready, &trig, &b, &rotated, &exit_trig);
    sp.workspace_id = "ws-4f".into();
    sp.session_type = "codex".to_string();
    sp.working_dir = Some(tmp.path().to_path_buf());
    let session = DaemonSession::spawn(sp).expect("spawn codex-typed bash session");
    state.lock().unwrap().sessions.insert(uid.into(), session);

    wait_for_file(&ready, Duration::from_secs(10));
    let mut warned = false;

    // First observation: binds the live rollout (old = None) through
    // the canonical set_transcript_path flow → generation 0→1.
    let outcome = observe_codex_rollout_once(&state, uid, &mut warned);
    assert_eq!(
        outcome,
        RolloutObserveOutcome::Stamped {
            old: None,
            new: UUID_A.to_string(),
        },
        "first observation must stamp the live rollout",
    );
    {
        let s = state.lock().unwrap();
        let sess = s.sessions.get(uid).unwrap();
        assert_eq!(sess.transcript_path.as_deref(), a.to_str());
        assert_eq!(sess.generation, 1, "stamp bumps generation on change");
    }
    assert_eq!(
        persisted_transcript_id(&sessions_json, uid).as_deref(),
        Some(UUID_A),
        "persisted resume key must be the extracted rollout uuid, \
         not the file stem",
    );

    // Steady state: same rollout → Unchanged, no bump.
    assert_eq!(
        observe_codex_rollout_once(&state, uid, &mut warned),
        RolloutObserveOutcome::Unchanged,
    );
    assert_eq!(state.lock().unwrap().sessions.get(uid).unwrap().generation, 1);

    // Rotate (the /compact analogue) and drive the periodic fn until
    // it observes the new rollout — bounded, no watch thread needed.
    std::fs::write(&trig, b"").unwrap();
    wait_for_file(&rotated, Duration::from_secs(10));
    let start = Instant::now();
    loop {
        match observe_codex_rollout_once(&state, uid, &mut warned) {
            RolloutObserveOutcome::Stamped { old, new } => {
                assert_eq!(old.as_deref(), Some(UUID_A));
                assert_eq!(new, UUID_B);
                break;
            }
            outcome @ (RolloutObserveOutcome::SessionGone
            | RolloutObserveOutcome::NotCodex) => {
                panic!("unexpected terminal outcome mid-rotation: {:?}", outcome);
            }
            // NoRollout / Unchanged / StampFailed: the child may be
            // mid-swap between fds — re-observe.
            _ => {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "rotation never observed",
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    {
        let s = state.lock().unwrap();
        let sess = s.sessions.get(uid).unwrap();
        assert_eq!(sess.transcript_path.as_deref(), b.to_str());
        assert_eq!(sess.generation, 2, "rotation bumps generation again");
    }
    assert_eq!(
        persisted_transcript_id(&sessions_json, uid).as_deref(),
        Some(UUID_B),
        "a hard restart must resume the post-rotation lineage",
    );

    // Non-codex sessions are never touched: a claude-typed sibling
    // observes NotCodex (terminal) and keeps its stamp verbatim.
    let claude_uid = "ts-4f00-c1de";
    let mut sp = SpawnParams::new(claude_uid, "claude-bystander", "/bin/sleep");
    sp.args = vec!["30".into()];
    sp.workspace_id = "ws-4f".into();
    sp.session_type = "claude-code".to_string();
    let mut claude_sess = DaemonSession::spawn(sp).expect("spawn bystander");
    claude_sess.transcript_path = Some("/somewhere/claude.jsonl".into());
    state.lock().unwrap().sessions.insert(claude_uid.into(), claude_sess);
    assert_eq!(
        observe_codex_rollout_once(&state, claude_uid, &mut warned),
        RolloutObserveOutcome::NotCodex,
    );
    assert_eq!(
        state
            .lock()
            .unwrap()
            .sessions
            .get(claude_uid)
            .unwrap()
            .transcript_path
            .as_deref(),
        Some("/somewhere/claude.jsonl"),
        "non-codex stamp must be untouched",
    );

    // Removal is terminal: the watch loop's exit condition.
    std::fs::write(&exit_trig, b"").unwrap();
    state.lock().unwrap().sessions.remove(uid); // Drop SIGKILLs the child
    assert_eq!(
        observe_codex_rollout_once(&state, uid, &mut warned),
        RolloutObserveOutcome::SessionGone,
    );
    state.lock().unwrap().sessions.remove(claude_uid);
}
