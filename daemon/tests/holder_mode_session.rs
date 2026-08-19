//! Session-level proofs of the holder-split kill-authority and
//! exit-observer semantics (DESIGN_HOLDER_BRAIN_SPLIT § Kill
//! authority S2/O4 + § Spawn path), against a REAL child on a real
//! PTY — no holder process involved: the `ExitAuthority::Holder`
//! wiring is driven directly with recording closures.
//!
//! Own process (integration test) per the repo convention for
//! child-spawning suites. The only child touched is the one this
//! test spawns; cleanup signals it by pid only after this process
//! (its parent) confirms it still exists.
#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use cm_daemon::adopt::SessionCandidate;
use cm_daemon::session::{
    AdoptedSessionMeta, DaemonExitStatus, DaemonSession, ExitAuthority, HolderAttribution,
    HolderExit,
};

fn open_pidfd(pid: libc::pid_t) -> OwnedFd {
    // SAFETY: plain syscall; on success we own the fd.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    assert!(ret >= 0, "pidfd_open: {}", std::io::Error::last_os_error());
    unsafe { OwnedFd::from_raw_fd(ret as RawFd) }
}

fn dup_cloexec(raw: RawFd) -> OwnedFd {
    // SAFETY: fcntl dup of a valid fd.
    let duped = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
    assert!(duped >= 0, "dup: {}", std::io::Error::last_os_error());
    unsafe { OwnedFd::from_raw_fd(duped) }
}

fn child_alive(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 = existence probe on our own child.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Spawn `/bin/cat` on a fresh PTY (it parks reading the terminal),
/// returning (pid, master dup, pidfd) with the portable-pty handles
/// dropped — the dups keep the PTY's open file description alive,
/// mirroring how a brain-side session holds only dups.
fn spawn_cat() -> (libc::pid_t, OwnedFd, OwnedFd) {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new("/bin/cat");
    cmd.env_clear();
    let child = pair.slave.spawn_command(cmd).expect("spawn cat");
    drop(pair.slave);
    let pid = child.process_id().expect("child pid") as libc::pid_t;
    let master_raw = pair.master.as_raw_fd().expect("master raw fd");
    let master = dup_cloexec(master_raw);
    let pidfd = open_pidfd(pid);
    // Dropping the portable-pty master is safe: our dup shares the
    // open file description. The Child box's drop neither kills nor
    // waits.
    drop(pair.master);
    drop(child);
    (pid, master, pidfd)
}

fn test_meta(title: &str) -> AdoptedSessionMeta {
    AdoptedSessionMeta {
        title: title.into(),
        session_type: "bash".into(),
        workspace_id: "ws-test".into(),
        managed_by_uid: None,
        task_id: None,
        transcript_path: None,
        memory_cap_soft_bytes: None,
        memory_cap_hard_bytes: None,
        cgroup_prefix: None,
        workflow_run_id: None,
        workflow_role: None,
        continuous_task_id: None,
        global_perms: false,
        generation: 0,
        last_activity_at: None,
        last_input_at: None,
        last_operator_input_at: None,
        last_turn_end_at: None,
        done_report: None,
        kills_dir: None,
        kills_baseline: None,
    }
}

struct Recorded {
    kills: Arc<Mutex<Vec<(i32, String)>>>,
    settled: Arc<Mutex<bool>>,
    exits: Arc<Mutex<Vec<DaemonExitStatus>>>,
}

/// Build a holder-authority session over a real cat child with
/// recording closures in place of a HolderClient.
fn build_holder_session(
    uid: &str,
) -> (
    DaemonSession,
    mpsc::Sender<HolderExit>,
    Recorded,
    libc::pid_t,
) {
    let (pid, master, pidfd) = spawn_cat();
    let candidate = SessionCandidate::from_raw_parts(uid.to_string(), pid, pidfd, master);
    let parts = candidate.promote();
    let build = DaemonSession::build_adopted(parts, test_meta(uid)).expect("build");

    let kills: Arc<Mutex<Vec<(i32, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(Mutex::new(false));
    let exits: Arc<Mutex<Vec<DaemonExitStatus>>> = Arc::new(Mutex::new(Vec::new()));

    let (tx, rx) = mpsc::channel::<HolderExit>();
    let kills_rec = Arc::clone(&kills);
    let settled_rec = Arc::clone(&settled);
    let exits_rec = Arc::clone(&exits);
    let on_exit: cm_daemon::session::OnExitCallback = Box::new(move |st| {
        exits_rec.lock().unwrap().push(st.clone());
    });
    let session = build
        .arm(
            Some(on_exit),
            ExitAuthority::Holder {
                events: rx,
                settle: Box::new(move || {
                    *settled_rec.lock().unwrap() = true;
                }),
                kill: Box::new(move |sig, who| {
                    kills_rec.lock().unwrap().push((sig, who.to_string()));
                    Ok(())
                }),
            },
        )
        .expect("arm");
    (
        session,
        tx,
        Recorded {
            kills,
            settled,
            exits,
        },
        pid,
    )
}

fn cleanup_child(pid: libc::pid_t) {
    if child_alive(pid) {
        // SAFETY: our own spawned child, existence just probed.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    // Reap (we are the parent).
    // SAFETY: plain waitpid on our own child.
    unsafe {
        let mut status = 0;
        libc::waitpid(pid, &mut status, 0);
    }
}

#[test]
fn drop_is_inert_and_kill_routes_the_verb_with_attribution() {
    // S2's headline: a holder-owned session's Drop signals NOTHING —
    // a brain that unwinds with the registry live must not SIGKILL
    // the fleet. And O4: an explicit kill routes the verb, carrying
    // the stamped attribution.
    let (mut session, _tx, rec, pid) = build_holder_session("ts-hold-1");

    // Explicit kill: routed, attributed, and NOT a real signal (our
    // recording closure is the "verb"): the child must survive it.
    session.set_kill_attribution("operator");
    session.kill().expect("verb-routed kill");
    assert_eq!(
        rec.kills.lock().unwrap().as_slice(),
        &[(libc::SIGKILL, "operator".to_string())]
    );
    assert!(child_alive(pid), "the verb closure signals; kill() itself must not");

    // Drop: INERT. No new closure calls, child untouched.
    drop(session);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        rec.kills.lock().unwrap().len(),
        1,
        "Drop must not route (or perform) any kill"
    );
    assert!(
        child_alive(pid),
        "S2 violated: dropping a holder-owned session touched the child"
    );

    cleanup_child(pid);
}

#[test]
fn unattributed_kill_falls_back_to_daemon() {
    let (mut session, _tx, rec, pid) = build_holder_session("ts-hold-2");
    session.kill().expect("kill");
    assert_eq!(
        rec.kills.lock().unwrap().as_slice(),
        &[(libc::SIGKILL, "daemon".to_string())]
    );
    drop(session);
    cleanup_child(pid);
}

#[test]
fn exit_observer_stashes_provenance_fires_on_exit_then_settles() {
    // The § Spawn path exit pipeline, driven by hand: an exit event
    // arrives on the subscription → provenance stashed on the cell →
    // kernel status cached (try_wait) → on_exit fired → settle runs
    // LAST (the C4 ack-after-persist order).
    let (mut session, tx, rec, pid) = build_holder_session("ts-hold-3");

    let event = HolderExit {
        status: DaemonExitStatus {
            code: None,
            signal: Some(libc::SIGKILL),
        },
        exited_at: 1_700_000_000.5,
        attribution: Some(HolderAttribution {
            sig: libc::SIGKILL,
            who: "operator".into(),
            at: 1_700_000_000.0,
        }),
        memory_events: None,
    };
    tx.send(event).expect("deliver exit event");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if *rec.settled.lock().unwrap() {
            break;
        }
        assert!(Instant::now() < deadline, "observer never settled");
        std::thread::sleep(Duration::from_millis(20));
    }
    // on_exit fired with the holder's status, before settle.
    let exits = rec.exits.lock().unwrap();
    assert_eq!(exits.len(), 1);
    assert_eq!(exits[0].signal, Some(libc::SIGKILL));
    drop(exits);
    // The provenance cell carries the holder's clock + echo for
    // handle_session_exit's tombstone merge.
    let cell = session.holder_exit.lock().unwrap().clone();
    let info = cell.expect("provenance stashed");
    assert_eq!(info.exited_at, 1_700_000_000.5);
    assert_eq!(info.attribution.unwrap().who, "operator");
    // try_wait sees the cached status.
    assert_eq!(
        session.try_wait().map(|s| s.signal),
        Some(Some(libc::SIGKILL))
    );

    drop(session);
    cleanup_child(pid);
}
