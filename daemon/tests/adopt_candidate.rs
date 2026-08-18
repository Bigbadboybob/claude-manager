//! Integration tests for the non-killing adoption primitives.
//! Restart hardening / DESIGN_SEAMLESS_RESTART phase 2c (review
//! finding R5 + the portable-pty adoption gap).
//!
//! Proven here against real `sh` children on manually-opened ptys
//! (`openpty(3)` + `pidfd_open(2)` — the same raw shape a future
//! rehydrate inherits, with no `DaemonSession` anywhere near):
//!
//! 1. **THE property (R5)** — dropping a `SessionCandidate` closes
//!    its own fds and never signals, waits on, or reaps the child.
//!    The child of a dropped candidate is still alive afterwards.
//! 2. **Probes are non-consuming** — `child_alive` flips to
//!    `Ok(false)` after the child exits, and the exit status is
//!    still fully consumable by the real waiter afterwards (the
//!    child parked as a zombie; nothing was reaped).
//! 3. **The adopted master is a real master** — bytes round-trip
//!    through `take_writer`/`try_clone_reader`, and `resize` via
//!    the adopted fd is observed by the child (`stty size`).
//! 4. **Start-time probe** — stable, nonzero, identical across
//!    reads (the R6 identity cross-check value).
//!
//! Lives in `tests/` (own process) per the repo convention for
//! child-spawning suites (see `reap_gate_freeze.rs`); deadlines are
//! generous throughout because the suite runs under machine load.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cm_daemon::adopt::SessionCandidate;
use portable_pty::{MasterPty, PtySize};

/// Open a fresh pty pair via `openpty(3)` — deliberately NOT via
/// portable-pty, because the scenario under test is an inherited raw
/// master fd that portable-pty never constructed.
fn openpty_raw() -> (OwnedFd, OwnedFd) {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    // SAFETY: valid out-pointers for the two fds; name/termios/
    // winsize are all optional (null).
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(ret, 0, "openpty failed: {:?}", std::io::Error::last_os_error());
    // SAFETY: openpty succeeded, so both are fresh fds we own.
    (unsafe { OwnedFd::from_raw_fd(master) }, unsafe {
        OwnedFd::from_raw_fd(slave)
    })
}

/// `pidfd_open(2)` on a raw pid — same technique as
/// `session.rs::open_pidfd`, duplicated here because the tests model
/// the *inheriting* side, which builds pidfds without a
/// `DaemonSession`.
fn pidfd_open(pid: libc::pid_t) -> OwnedFd {
    // SAFETY: pidfd_open takes a pid and flags; 0 flags is valid.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    assert!(
        ret >= 0,
        "pidfd_open({}) failed: {:?}",
        pid,
        std::io::Error::last_os_error()
    );
    // SAFETY: non-negative return is a fresh fd we own.
    unsafe { OwnedFd::from_raw_fd(ret as i32) }
}

/// Spawn `/bin/sh` (optionally `-c <script>`) with all three stdio
/// streams on the slave end of a fresh pty; hand back the std
/// `Child` (kept OUTSIDE any candidate, for ground-truth probing and
/// explicit end-of-test reaping) plus the master fd. The parent's
/// slave copies are consumed by the `Stdio`s and dropped with the
/// `Command`, so the child holds the only slave references.
fn spawn_sh_on_pty(script: Option<&str>) -> (Child, OwnedFd) {
    let (master, slave) = openpty_raw();
    let stdin = Stdio::from(slave.try_clone().expect("dup slave for stdin"));
    let stdout = Stdio::from(slave.try_clone().expect("dup slave for stdout"));
    let stderr = Stdio::from(slave);
    let mut cmd = Command::new("/bin/sh");
    if let Some(s) = script {
        cmd.arg("-c").arg(s);
    }
    let child = cmd
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .unwrap_or_else(|e| panic!("spawn /bin/sh ({:?}): {}", script, e));
    (child, master)
}

/// Explicit end-of-test cleanup so no child (or zombie) outlives the
/// test binary — the candidate under test must never be the thing
/// doing this.
fn kill_and_reap(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Drain a reader on a background thread into a shared buffer. The
/// thread ends when the reader returns `Ok(0)` (the adopted reader
/// maps the master's post-teardown `EIO` to that) or errors.
fn drain_in_background(mut reader: Box<dyn Read + Send>) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let buf_clone = Arc::clone(&buf);
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf_clone.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });
    buf
}

/// Spin until the drained output contains `needle`. Generous 20s
/// deadline per repo convention (suite runs under load).
fn await_output_contains(buf: &Arc<Mutex<Vec<u8>>>, needle: &str, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if String::from_utf8_lossy(&buf.lock().unwrap()).contains(needle) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{}: output never contained {:?} within 20s; got: {:?}",
        label,
        needle,
        String::from_utf8_lossy(&buf.lock().unwrap())
    );
}

/// THE property (R5): dropping a candidate never signals, waits on,
/// or otherwise touches the child — it only closes the candidate's
/// own fds. This is the guarantee that makes a failed rehydrate's
/// unwind safe for already-verified children.
///
/// The child here is `sh -c 'sleep 300'` — deliberately one that
/// never reads its pty — so the one unavoidable consequence of the
/// drop (the candidate's master fd closing) cannot end the child
/// through ordinary EOF-on-stdin. That mirrors production topology
/// anyway: rehydrate candidates hold CLOEXEC *dups* while the escrow
/// originals keep the pty alive, so a candidate drop never takes the
/// pty down there either. What this test isolates is the type's own
/// behavior: any kill/signal/wait in the drop path WOULD end or reap
/// this child, and every probe below would catch it.
#[test]
fn candidate_drop_never_touches_child() {
    let (mut child, master) = spawn_sh_on_pty(Some("sleep 300"));
    let pid = child.id() as libc::pid_t;

    // Ground-truth probe handle held OUTSIDE the candidate.
    let probe_pidfd = pidfd_open(pid);

    let candidate = SessionCandidate::from_raw_parts(
        "adopt-drop-test",
        pid,
        pidfd_open(pid),
        master,
    );
    assert_eq!(
        candidate.child_alive().expect("pre-drop liveness probe"),
        true,
        "child must be alive before the drop"
    );

    drop(candidate);

    // Give any misdirected asynchronous signal generous time to
    // land before asserting survival.
    std::thread::sleep(Duration::from_millis(500));

    // 1. kill(pid, 0): the pid still names a live (un-reaped)
    //    process we may signal.
    // SAFETY: signal 0 performs permission/existence checks only.
    let ret = unsafe { libc::kill(pid, 0) };
    assert_eq!(
        ret,
        0,
        "kill(pid, 0) after candidate drop: {:?} — the drop touched the child",
        std::io::Error::last_os_error()
    );

    // 2. The independent pidfd shows no exit-readiness event.
    let mut pfd = libc::pollfd {
        fd: probe_pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: valid pollfd, nfds=1, zero timeout.
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    assert_eq!(
        ret, 0,
        "independent pidfd reported exit-readiness after candidate drop \
         (revents {:#x}) — the drop killed the child",
        pfd.revents
    );

    // 3. And the real parent-side waiter agrees.
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "child exited after candidate drop — the drop touched it"
    );

    kill_and_reap(child);
}

/// `child_alive` is a repeatable, NON-CONSUMING probe: it reports
/// live → exited truthfully, and after reporting "exited" the wait
/// status is still fully consumable by the real waiter — the probe
/// left the child parked as a kernel zombie, exactly what the
/// rehydrate transaction needs (pre-commit, nothing may consume;
/// post-commit, `waitid(P_PIDFD)` reaps with full status).
#[test]
fn child_alive_flips_on_exit_without_consuming_status() {
    let (mut child, master) = spawn_sh_on_pty(Some("exit 7"));
    let pid = child.id() as libc::pid_t;
    let candidate = SessionCandidate::from_raw_parts(
        "adopt-alive-test",
        pid,
        pidfd_open(pid),
        master,
    );

    // Flip to exited within the (generous) deadline.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match candidate.child_alive().expect("liveness probe") {
            false => break,
            true if Instant::now() >= deadline => {
                panic!("sh -c 'exit 7' still 'alive' after 20s")
            }
            true => std::thread::sleep(Duration::from_millis(25)),
        }
    }

    // Repeatable: a second read answers the same, consuming nothing.
    assert_eq!(candidate.child_alive().expect("second probe"), false);

    // The status was NOT consumed by the probes: the real waiter
    // still gets the full, correct code.
    let status = child.wait().expect(
        "child.wait() failed after child_alive probes — a probe consumed \
         the status (candidate probes must be non-consuming)",
    );
    assert_eq!(
        status.code(),
        Some(7),
        "exit status must survive the probes intact"
    );
}

/// Start-time probe (the R6 cross-check value): nonzero and stable —
/// two reads through the candidate agree exactly.
#[test]
fn child_start_time_probe_stable_and_nonzero() {
    let (child, master) = spawn_sh_on_pty(Some("sleep 300"));
    let pid = child.id() as libc::pid_t;
    let candidate = SessionCandidate::from_raw_parts(
        "adopt-starttime-test",
        pid,
        pidfd_open(pid),
        master,
    );

    let first = candidate.child_start_time().expect("first start-time read");
    assert!(first > 0, "starttime must be nonzero (got {})", first);
    let second = candidate
        .child_start_time()
        .expect("second start-time read");
    assert_eq!(
        first, second,
        "starttime is a spawn-time kernel constant — reads must agree"
    );

    kill_and_reap(child);
}

/// Promotion hands out a working master: bytes written through the
/// adopted master's taken writer reach the child, and the child's
/// output comes back through the cloned reader. The `tr` transform
/// guarantees the awaited marker can only have been produced by the
/// child (the echoed input contains dashes, never the underscore
/// form).
#[test]
fn adopted_master_round_trips_child_io() {
    // Interactive sh: reads commands from the pty slave.
    let (child, master) = spawn_sh_on_pty(None);
    let pid = child.id() as libc::pid_t;
    let candidate = SessionCandidate::from_raw_parts(
        "adopt-roundtrip-test",
        pid,
        pidfd_open(pid),
        master,
    );
    assert_eq!(candidate.child_alive().expect("pre-promote probe"), true);

    let parts = candidate.promote();
    assert_eq!(parts.uid, "adopt-roundtrip-test");
    assert_eq!(parts.pid, pid);

    let reader = parts.master.try_clone_reader().expect("clone reader");
    let output = drain_in_background(reader);
    let mut writer = parts.master.take_writer().expect("take writer");

    writer
        .write_all(b"echo CM-ADOPT-RT | tr - _\n")
        .expect("write command to adopted master");
    writer.flush().expect("flush adopted writer");

    await_output_contains(&output, "CM_ADOPT_RT", "adopted-master round trip");

    kill_and_reap(child);
}

/// Resize through the adopted master is observed by the child:
/// `stty size` (which reads the shared kernel winsize via the
/// slave) reports each newly-set size — including a CHANGE between
/// two sizes, so this can't pass on a stale initial value.
#[test]
fn adopted_master_resize_observed_by_child() {
    let (child, master) = spawn_sh_on_pty(None);
    let pid = child.id() as libc::pid_t;
    let parts = SessionCandidate::from_raw_parts(
        "adopt-resize-test",
        pid,
        pidfd_open(pid),
        master,
    )
    .promote();

    let reader = parts.master.try_clone_reader().expect("clone reader");
    let output = drain_in_background(reader);
    let mut writer = parts.master.take_writer().expect("take writer");

    let first = PtySize {
        rows: 43,
        cols: 107,
        pixel_width: 0,
        pixel_height: 0,
    };
    parts.master.resize(first).expect("first resize");
    assert_eq!(
        parts.master.get_size().expect("get_size"),
        first,
        "get_size must reflect the resize"
    );
    writer.write_all(b"stty size\n").expect("write stty size");
    writer.flush().expect("flush");
    await_output_contains(&output, "43 107", "child observes first resize");

    let second = PtySize {
        rows: 21,
        cols: 91,
        pixel_width: 0,
        pixel_height: 0,
    };
    parts.master.resize(second).expect("second resize");
    writer.write_all(b"stty size\n").expect("write stty size again");
    writer.flush().expect("flush");
    await_output_contains(&output, "21 91", "child observes changed size");

    kill_and_reap(child);
}
