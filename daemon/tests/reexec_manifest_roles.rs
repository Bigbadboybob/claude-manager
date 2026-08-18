//! Integration tests for the sealed re-exec manifest's per-role
//! fd-type validation. DESIGN_SEAMLESS_RESTART phase 3a (R7/R8/R14).
//!
//! `validate_fd_roles` is the first — and only — manifest code that
//! touches escrow fds, so it is proven here against REAL kernel
//! objects of every role, plus the near-miss of each (the fd kind an
//! honest bug would most plausibly put in that slot):
//!
//! - `pty_master_fd`: an `openpty(3)` master passes; a pipe fails.
//! - `pidfd`: a `pidfd_open(2)` on a spawned child passes; a regular
//!   file fails.
//! - `listener_fd`: a bound+listening `UnixListener` (at a temp
//!   path, never anywhere near `~/.cm/`) passes; a CONNECTED stream
//!   from the same socket fails (`SO_ACCEPTCONN` false).
//! - `rollback_bin_fd`: an executable regular file passes; the same
//!   bytes without the execute bit fail, and a char device fails.
//!
//! Plus the full write → offset-parked-at-EOF → read → roles happy
//! path over the same real fds — the whole 3a surface in one pass.
//!
//! Lives in `tests/` (own process) per the repo convention for
//! child-spawning suites (see `adopt_candidate.rs`); every spawned
//! child is killed and reaped explicitly at the end of its test.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command, Stdio};

use cm_daemon::reexec_manifest::{
    read_manifest, validate_fd_roles, write_manifest, ManifestError,
    ReexecManifest, SessionRecord, MANIFEST_SCHEMA_VERSION,
};

/// Open a fresh pty pair via `openpty(3)` — same helper shape as
/// `adopt_candidate.rs` (raw fds, no portable-pty, because the
/// scenario is an inherited fd portable-pty never constructed).
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

/// `pidfd_open(2)` on a raw pid — the manifest hands off spawn-time
/// pidfds; tests build the same object the inheriting side holds.
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

/// Explicit end-of-test cleanup so no child (or zombie) outlives the
/// test binary.
fn kill_and_reap(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// A pipe's read end — the classic wrong-kind fd for the pty slot.
fn pipe_read_end() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: valid out-array for the two fds.
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    assert_eq!(ret, 0, "pipe2 failed: {:?}", std::io::Error::last_os_error());
    // SAFETY: pipe2 succeeded, so both are fresh fds we own.
    (unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
        OwnedFd::from_raw_fd(fds[1])
    })
}

/// Everything a happy-path manifest's roles need, all real, all
/// test-owned: a sleeping child on nothing (pidfd), a raw pty pair
/// (master), a listening UnixListener at a temp path, and an
/// executable regular file (rollback stand-in).
struct RealFds {
    child: Option<Child>,
    pidfd: OwnedFd,
    master: OwnedFd,
    _slave: OwnedFd,
    listener: UnixListener,
    rollback: std::fs::File,
    _dir: tempfile::TempDir,
}

impl RealFds {
    fn new() -> Self {
        let (master, slave) = openpty_raw();

        let child = Command::new("/bin/sleep")
            .arg("300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn /bin/sleep");
        let pidfd = pidfd_open(child.id() as libc::pid_t);

        let dir = tempfile::TempDir::new().expect("tempdir");
        // Our OWN throwaway listener at a temp path — never the real
        // ~/.cm sockets.
        let listener =
            UnixListener::bind(dir.path().join("roles-test.sock"))
                .expect("bind throwaway listener");

        let rollback_path = dir.path().join("fake-rollback-bin");
        {
            let mut f =
                std::fs::File::create(&rollback_path).expect("create");
            f.write_all(b"#!/bin/sh\nexit 0\n").expect("write");
        }
        std::fs::set_permissions(
            &rollback_path,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod +x");
        let rollback =
            std::fs::File::open(&rollback_path).expect("open rollback");

        RealFds {
            child: Some(child),
            pidfd,
            master,
            _slave: slave,
            listener,
            rollback,
            _dir: dir,
        }
    }

    /// A manifest whose every fd slot names one of the real fds.
    fn manifest(&self) -> ReexecManifest {
        ReexecManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            attempt: 0,
            rollback_bin_fd: self.rollback.as_raw_fd(),
            sessions: vec![SessionRecord {
                uid: "roles-test".into(),
                generation: 1,
                transcript_id: None,
                transcript_path: None,
                session_type: "bash".into(),
                title: "roles-test".into(),
                workspace_id: String::new(),
                task_id: None,
                managed_by_uid: None,
                workflow_run_id: None,
                workflow_role: None,
                continuous_task_id: None,
                global_perms: false,
                memory_cap_soft_bytes: None,
                memory_cap_hard_bytes: None,
                last_activity_age_s: Some(0.5),
                last_input_age_s: None,
                last_operator_input_age_s: None,
                last_turn_end_age_s: None,
                done_report: None,
                child_pid: self
                    .child
                    .as_ref()
                    .expect("child not yet reaped")
                    .id() as i32,
                child_start_time: 1,
                pty_master_fd: self.master.as_raw_fd(),
                pidfd: self.pidfd.as_raw_fd(),
                cgroup_prefix: None,
                watcher_checkpoint: None,
            }],
            listener_fd: self.listener.as_raw_fd(),
            tls_listener_fd: None,
        }
    }

    fn cleanup(mut self) {
        if let Some(child) = self.child.take() {
            kill_and_reap(child);
        }
    }
}

/// Assert an `Err(FdRoleMismatch)` naming the expected role and fd.
fn assert_role_mismatch(
    result: Result<(), ManifestError>,
    role: &str,
    fd: i32,
) {
    match result {
        Err(ManifestError::FdRoleMismatch {
            role: r,
            fd: f,
            detail,
        }) => {
            assert_eq!(r, role, "wrong role named (detail: {})", detail);
            assert_eq!(f, fd, "wrong fd named (detail: {})", detail);
        }
        other => panic!(
            "expected FdRoleMismatch for {} fd {}, got {:?}",
            role, fd, other
        ),
    }
}

/// The flagship happy path: all real fds, full round trip. Write the
/// manifest, park the memfd's file offset at EOF (the post-exec
/// state, R8), read it back byte-identical, then role-validate every
/// slot against the live kernel objects.
#[test]
fn real_fds_pass_full_write_read_roles_pipeline() {
    let fixture = RealFds::new();
    let manifest = fixture.manifest();

    let memfd = write_manifest(&manifest).expect("write_manifest");
    // SAFETY: plain lseek on an open fd — simulating the inherited
    // offset a real handoff sees.
    let end = unsafe { libc::lseek(memfd.as_raw_fd(), 0, libc::SEEK_END) };
    assert!(end > 0, "lseek(SEEK_END) failed");

    let got = read_manifest(memfd.as_fd()).expect("read_manifest");
    assert_eq!(got, manifest, "manifest must round-trip exactly");

    validate_fd_roles(&got)
        .expect("all-real-fd manifest must pass role validation");

    fixture.cleanup();
}

/// pty role: the openpty master passes (proven inside the happy path
/// above too); a pipe fd in the same slot fails — a pipe is exactly
/// the not-a-terminal byte stream a confused coordinator might
/// escrow.
#[test]
fn pipe_fails_pty_master_role() {
    let fixture = RealFds::new();
    let (pipe_r, _pipe_w) = pipe_read_end();

    let mut m = fixture.manifest();
    m.sessions[0].pty_master_fd = pipe_r.as_raw_fd();
    assert_role_mismatch(
        validate_fd_roles(&m),
        "pty_master_fd",
        pipe_r.as_raw_fd(),
    );

    fixture.cleanup();
}

/// pidfd role: the real pidfd passes (happy path); a regular file in
/// the pidfd slot fails — its /proc link is a path, not
/// `anon_inode:[pidfd]`.
#[test]
fn regular_file_fails_pidfd_role() {
    let fixture = RealFds::new();

    let mut m = fixture.manifest();
    // The rollback file doubles as the wrong-kind fd here; give the
    // rollback slot a fresh legitimate copy so only ONE slot is bad
    // (duplicate fd numbers would be a different rejection — and a
    // structural one at that).
    let file_fd = fixture.rollback.as_raw_fd();
    let rollback_dup =
        fixture.rollback.try_clone().expect("dup rollback file");
    m.sessions[0].pidfd = file_fd;
    m.rollback_bin_fd = rollback_dup.as_raw_fd();
    assert_role_mismatch(validate_fd_roles(&m), "pidfd", file_fd);

    fixture.cleanup();
}

/// listener role: the bound+listening UnixListener passes (happy
/// path); a CONNECTED stream — a socket, but one `listen(2)` was
/// never called on — fails on `SO_ACCEPTCONN`.
#[test]
fn connected_stream_fails_listener_role() {
    let fixture = RealFds::new();

    let addr = fixture
        .listener
        .local_addr()
        .expect("listener addr")
        .as_pathname()
        .expect("pathname addr")
        .to_path_buf();
    let stream = UnixStream::connect(&addr).expect("connect");
    let _accepted = fixture.listener.accept().expect("accept");

    let mut m = fixture.manifest();
    m.listener_fd = stream.as_raw_fd();
    assert_role_mismatch(
        validate_fd_roles(&m),
        "listener_fd",
        stream.as_raw_fd(),
    );

    // Same probe guards the TLS slot.
    let mut m = fixture.manifest();
    m.tls_listener_fd = Some(stream.as_raw_fd());
    assert_role_mismatch(
        validate_fd_roles(&m),
        "tls_listener_fd",
        stream.as_raw_fd(),
    );

    fixture.cleanup();
}

/// rollback role: the executable regular file passes (happy path); a
/// mode-0644 copy of the same bytes fails on the missing execute
/// bit, and a char device (the pty master) fails on file type.
#[test]
fn non_executable_and_non_regular_fail_rollback_role() {
    let fixture = RealFds::new();

    // Same bytes, no execute bit.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let noexec_path = dir.path().join("not-executable");
    std::fs::write(&noexec_path, b"#!/bin/sh\nexit 0\n").expect("write");
    std::fs::set_permissions(
        &noexec_path,
        std::fs::Permissions::from_mode(0o644),
    )
    .expect("chmod 644");
    let noexec = std::fs::File::open(&noexec_path).expect("open");

    let mut m = fixture.manifest();
    m.rollback_bin_fd = noexec.as_raw_fd();
    assert_role_mismatch(
        validate_fd_roles(&m),
        "rollback_bin_fd",
        noexec.as_raw_fd(),
    );

    // A char device (fresh pty master) in the rollback slot: wrong
    // file type entirely.
    let (spare_master, _spare_slave) = openpty_raw();
    let mut m = fixture.manifest();
    m.rollback_bin_fd = spare_master.as_raw_fd();
    assert_role_mismatch(
        validate_fd_roles(&m),
        "rollback_bin_fd",
        spare_master.as_raw_fd(),
    );

    fixture.cleanup();
}
