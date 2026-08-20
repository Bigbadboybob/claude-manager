//! Live migration: monolith → holder/brain split, zero session loss
//! (DESIGN_HOLDER_BRAIN_SPLIT § Live migration, phase 7).
//!
//! The running single-process daemon becomes the holder — its
//! children cannot be re-parented, so the process that owns them must
//! BECOME the process that keeps them. The shipped re-exec machinery
//! is the vehicle; the deltas against `reexec::perform_reexec`:
//!
//! - TWO extra pins (the holder image — the exec target — and the
//!   brain binary), each preflighted before any point of side effect.
//! - The parked brain is spawned AFTER the quiesce + checked persist
//!   (its boot-time state reads must see the frozen snapshot), with
//!   one socketpair end at fd 3 — and ON THIS THREAD: the brain sets
//!   `PR_SET_PDEATHSIG(SIGKILL)` keyed to its parent THREAD, and
//!   `execve` destroys every thread but the calling one, so fork and
//!   exec must share a thread or the brain dies at our own exec.
//! - The manifest is schema v4: the standard v3 content plus the
//!   [`SplitRoles`] block (channel end, brain pid/pidfd, brain pin)
//!   and `rollback_schema_version` (what the rollback pin reads).
//! - The exec target is the HOLDER image with an argv of its own
//!   (`--brain <path>`), never our argv.
//!
//! Failure anywhere returns with the daemon restored (fd flags,
//! signal mask, gates, drain) and the parked brain — if it was ever
//! spawned — killed and reaped. Post-exec failure handling lives in
//! the holder image (C3's two branches: corrupt manifest → fresh
//! boot; post-validation init failure → rollback exec of the pinned
//! monolith, i.e. us).

#![cfg(target_os = "linux")]

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

use cm_holder_proto::reexec_manifest::{
    self, SplitRoles, MANIFEST_SCHEMA_VERSION, MANIFEST_SCHEMA_VERSION_SPLIT,
};

use crate::reap_gate;
use crate::reader_gate;
use crate::reexec;
use crate::restart_coordinator;
use crate::state::DaemonState;

/// The fd numbers the parked brain sees (the holder's spawn
/// contract, mirrored exactly): channel on 3, its own pinned binary
/// on 4.
const BRAIN_CHANNEL_FD: RawFd = 3;
const BRAIN_EXEC_FD: RawFd = 4;

/// Perform the monolith→split migration. Returns ONLY on failure —
/// on success this process is the holder image at the same PID, the
/// parked brain adopts, and the caller's connection died at the exec
/// (fire-and-verify via `daemon.health`).
pub fn perform_migrate_split(
    state: &Arc<Mutex<DaemonState>>,
    holder_path: &Path,
    brain_path: &Path,
) -> anyhow::Error {
    // ---- Refusals, before any side effect. ----
    if crate::holder_mode::global().is_some() {
        return anyhow::anyhow!(
            "daemon.migrate_split REFUSED: this daemon is ALREADY in \
             holder/brain split mode"
        );
    }
    {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tls) = &st.config.tls {
            return anyhow::anyhow!(
                "daemon.migrate_split REFUSED: this daemon carries a [tls] \
                 listener config (listen_addr {}), and TLS listener handoff \
                 is unimplemented on the migration path (it inherits the \
                 shipped daemon.restart refusal). Use drain + legacy restart \
                 into a supervisor-flipped holder launch instead. Nothing \
                 has happened.",
                tls.listen_addr,
            );
        }
    }

    // ---- Pins (R7): holder (exec target), brain, self (rollback). ----
    let holder_fd = match reexec::open_pinned_executable(holder_path) {
        Ok(fd) => fd,
        Err(e) => return e,
    };
    let brain_fd = match reexec::open_pinned_executable(brain_path) {
        Ok(fd) => fd,
        Err(e) => return e,
    };
    let rollback_fd: OwnedFd = match File::open("/proc/self/exe") {
        Ok(f) => f.into(),
        Err(e) => {
            return anyhow::anyhow!(
                "open /proc/self/exe read-only (rollback pin, R7): {e}"
            )
        }
    };

    // ---- Preflights (O14): BOTH images prove themselves. ----
    if let Err(msg) = crate::holder_mode::holder_preflight(&holder_fd) {
        return anyhow::anyhow!("daemon.migrate_split: {msg}");
    }
    if let Err(msg) = crate::holder_mode::brain_deploy_preflight(&brain_fd) {
        return anyhow::anyhow!("daemon.migrate_split: {msg}");
    }

    // ---- Quiesce: the full re-exec barrier. ----
    let guard = match restart_coordinator::begin(state) {
        Ok(g) => g,
        Err(busy) => return anyhow::anyhow!("{busy}"),
    };
    if let Err(timeout) = guard.wait_quiesced(reexec::QUIESCE_TIMEOUT) {
        let err = anyhow::anyhow!("{timeout}");
        guard.abort();
        return err;
    }
    let writer_pause = crate::writer_gate::request_pause();
    let writer_freeze = match crate::writer_gate::freeze(
        &writer_pause,
        reexec::WRITER_FREEZE_TIMEOUT,
    ) {
        Ok(f) => f,
        Err(timeout) => {
            let err = anyhow::anyhow!("{timeout}");
            drop(writer_pause);
            guard.abort();
            return err;
        }
    };
    let reap_freeze = reap_gate::freeze();
    let reader_freeze = reader_gate::freeze();

    let err = migrate_stage(
        state,
        holder_path,
        brain_path,
        &holder_fd,
        &brain_fd,
        &rollback_fd,
    );
    drop(reader_freeze);
    drop(reap_freeze);
    drop(writer_freeze);
    drop(writer_pause);
    guard.abort();
    err
}

/// The under-the-freezes stage: checked persist → parked-brain spawn
/// → v4 manifest → CLOEXEC discipline → signal bracket → exec.
/// Restores every mutation it made before returning its error.
fn migrate_stage(
    state: &Arc<Mutex<DaemonState>>,
    holder_path: &Path,
    brain_path: &Path,
    holder_fd: &OwnedFd,
    brain_fd: &OwnedFd,
    rollback_fd: &OwnedFd,
) -> anyhow::Error {
    // ---- Checked, durable persistence (the parked brain's boot
    // reads happen strictly after this — it blocks in its hello wait
    // until the holder image answers post-exec). ----
    if let Err(e) = reexec::persist_all_checked(state) {
        return e;
    }

    // ---- Spawn the parked brain (§ Live migration step 5). ----
    let (ours, theirs) = match socketpair_cloexec() {
        Ok(pair) => pair,
        Err(e) => return anyhow::anyhow!("socketpair for the brain channel: {e}"),
    };
    let brain = match spawn_parked_brain(brain_fd, theirs) {
        Ok(b) => b,
        Err(e) => return anyhow::anyhow!("spawn parked brain: {e}"),
    };
    let kill_parked = |brain: &ParkedBrain| {
        // Abort contract (O14): a monolith failure between spawn and
        // exec must not leak an invisible orphan. EOF-exit is the
        // brain's own backstop; the kill is the deterministic one.
        // SAFETY: pidfd_send_signal on our owned pidfd.
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                brain.pidfd.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::c_void>(),
                0,
            );
            let mut si: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_PIDFD,
                brain.pidfd.as_raw_fd() as libc::id_t,
                &mut si,
                libc::WEXITED,
            );
        }
    };

    // ---- Build + seal the v4 manifest (step 6). ----
    let manifest = match reexec::build_manifest(state, rollback_fd) {
        Ok(mut m) => {
            m.schema_version = MANIFEST_SCHEMA_VERSION_SPLIT;
            m.rollback_schema_version = Some(MANIFEST_SCHEMA_VERSION);
            m.split = Some(SplitRoles {
                channel_fd: ours.as_raw_fd(),
                brain_pid: brain.pid,
                brain_pidfd: brain.pidfd.as_raw_fd(),
                brain_pin_fd: brain_fd.as_raw_fd(),
                brain_pin_previous_fd: None,
                brain_path: brain_path.to_string_lossy().into_owned(),
            });
            m
        }
        Err(e) => {
            kill_parked(&brain);
            return e;
        }
    };
    let manifest_fd = match reexec_manifest::write_manifest(&manifest) {
        Ok(fd) => fd,
        Err(e) => {
            kill_parked(&brain);
            return anyhow::anyhow!("write sealed v4 manifest memfd: {e}");
        }
    };

    // ---- CLOEXEC discipline (R9), split roles included. ----
    let flags_snapshot = match reexec::snapshot_fd_flags() {
        Ok(s) => s,
        Err(e) => {
            kill_parked(&brain);
            return anyhow::anyhow!("snapshot /proc/self/fd flags: {e}");
        }
    };
    if let Err(e) = reexec::set_all_cloexec() {
        reexec::restore_fd_flags(&flags_snapshot);
        kill_parked(&brain);
        return anyhow::anyhow!("CLOEXEC audit: {e}");
    }
    let mut inherit: Vec<RawFd> = vec![
        manifest_fd.as_raw_fd(),
        holder_fd.as_raw_fd(),
        manifest.rollback_bin_fd,
        manifest.listener_fd,
        ours.as_raw_fd(),
        brain.pidfd.as_raw_fd(),
        brain_fd.as_raw_fd(),
    ];
    if let Some(fd) = manifest.tls_listener_fd {
        inherit.push(fd);
    }
    for s in &manifest.sessions {
        inherit.push(s.pty_master_fd);
        inherit.push(s.pidfd);
    }
    for &fd in &inherit {
        if let Err(e) = reexec::set_fd_flags(fd, 0) {
            reexec::restore_fd_flags(&flags_snapshot);
            kill_parked(&brain);
            return anyhow::anyhow!("clear CLOEXEC on handed-off fd {fd}: {e}");
        }
    }

    // ---- Signal bracket + exec (step 7). ----
    let old_mask = match reexec::block_sighup_sigterm() {
        Ok(m) => m,
        Err(e) => {
            reexec::restore_fd_flags(&flags_snapshot);
            kill_parked(&brain);
            return e;
        }
    };
    eprintln!(
        "cm-daemon: MIGRATE SPLIT — exec'ing pinned holder fd {} ({}) with \
         {} session(s), parked brain pid {}, channel fd {}, manifest fd {} \
         (schema v{})",
        holder_fd.as_raw_fd(),
        holder_path.display(),
        manifest.sessions.len(),
        brain.pid,
        ours.as_raw_fd(),
        manifest_fd.as_raw_fd(),
        MANIFEST_SCHEMA_VERSION_SPLIT,
    );
    let argv: Vec<std::ffi::OsString> = vec![
        holder_path.as_os_str().to_owned(),
        "--brain".into(),
        brain_path.as_os_str().to_owned(),
    ];
    let err =
        reexec::do_execveat_with_argv(holder_fd, &argv, manifest_fd.as_raw_fd());

    // ---- execveat returned — failure. Restore + reap the brain. ----
    eprintln!(
        "cm-daemon: MIGRATE SPLIT FAILED at execveat ({err}); restoring \
         pre-call state and killing the parked brain"
    );
    reexec::restore_sigmask(&old_mask);
    reexec::restore_fd_flags(&flags_snapshot);
    kill_parked(&brain);
    err
}

struct ParkedBrain {
    pid: i32,
    pidfd: OwnedFd,
}

/// Fork/exec the brain from its pinned fd with the channel end on
/// fd 3 and the pin on fd 4 — the holder's spawn contract, so the
/// brain cannot tell a migration spawn from a holder spawn. Same
/// lift-then-place pre_exec discipline as the holder's `spawn_brain`.
fn spawn_parked_brain(
    brain_fd: &OwnedFd,
    theirs: OwnedFd,
) -> std::io::Result<ParkedBrain> {
    let theirs_raw = theirs.as_raw_fd();
    let pin_raw = brain_fd.as_raw_fd();
    let mut cmd = std::process::Command::new(format!(
        "/proc/self/fd/{BRAIN_EXEC_FD}"
    ));
    cmd.arg0("cm-daemon");
    cmd.env(
        cm_holder_proto::channel::ENV_CHANNEL_FD,
        BRAIN_CHANNEL_FD.to_string(),
    );
    // SAFETY (pre_exec): dup/dup2/close only — async-signal-safe.
    // Lift-then-place (the holder's spawn_brain discipline): a source
    // sitting on a target slot gets clobbered, and dup2(x,x) is a
    // no-op that leaves CLOEXEC set.
    unsafe {
        cmd.pre_exec(move || {
            let lift = |fd: libc::c_int| -> std::io::Result<libc::c_int> {
                let d = libc::fcntl(fd, libc::F_DUPFD, 5);
                if d < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(d)
            };
            let t = lift(theirs_raw)?;
            let p = lift(pin_raw)?;
            if libc::dup2(t, BRAIN_CHANNEL_FD) < 0
                || libc::dup2(p, BRAIN_EXEC_FD) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(t);
            libc::close(p);
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let pid = child.id() as i32;
    // SAFETY: pidfd_open on a live child we just spawned.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: non-negative return is a fresh fd we own. std Child's
    // Drop neither kills nor waits.
    let pidfd = unsafe { OwnedFd::from_raw_fd(ret as RawFd) };
    drop(child);
    drop(theirs);
    Ok(ParkedBrain { pid, pidfd })
}

fn socketpair_cloexec() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut sv = [0i32; 2];
    // SAFETY: valid out-array for socketpair.
    let ret = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            sv.as_mut_ptr(),
        )
    };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: socketpair succeeded; we own both fds.
    Ok((unsafe { OwnedFd::from_raw_fd(sv[0]) }, unsafe {
        OwnedFd::from_raw_fd(sv[1])
    }))
}
