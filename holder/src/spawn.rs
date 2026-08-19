//! The holder's fork/exec — spawn-parity with the daemon's
//! `PendingSession::spawn` (design § The holder): the exact
//! `portable_pty::native_pty_system().openpty(...)` +
//! `slave.spawn_command(...)` primitive, so child-side semantics
//! (session leader, controlling tty, slave closed in parent, fd
//! scrub in portable-pty's pre_exec) are bit-identical to today's
//! sessions.
//!
//! ## The env contract (S1)
//!
//! `SpawnBody.env` is the COMPLETE child environment, composed
//! brain-side (a snapshot of the brain's post-`env_sanitize` environ
//! + the per-session CM_* composition + the secret scrubs). The
//! holder applies it over [`portable_pty::CommandBuilder::env_clear`]
//! — its own environ reaches no session, ever. Guard-tested by the
//! behavioral suite's environ-canary test.

use std::io;
use std::os::fd::OwnedFd;

use cm_holder_proto::channel::SpawnBody;
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use crate::reap;

/// A freshly spawned session child, canonically holder-owned.
pub struct Spawned {
    /// The canonical PTY master — kept open for the session's whole
    /// life (a brain crash must not close the last master and SIGHUP
    /// the child).
    pub master: Box<dyn MasterPty + Send>,
    /// The spawn-time pidfd (canonical; the brain gets dups).
    pub pidfd: OwnedFd,
    pub pid: libc::pid_t,
    /// `/proc/<pid>/stat` field 22 captured at spawn — the R6
    /// pid-reuse cross-check.
    pub child_start_time: u64,
}

/// Typed spawn failure, mapped onto the protocol's error codes by
/// the caller.
#[derive(Debug)]
pub enum SpawnError {
    EmptyArgv,
    Openpty(String),
    Exec(String),
    PostSpawn(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::EmptyArgv => write!(f, "argv is empty"),
            SpawnError::Openpty(e) => write!(f, "openpty: {e}"),
            SpawnError::Exec(e) => write!(f, "spawn_command: {e}"),
            SpawnError::PostSpawn(e) => write!(f, "post-spawn: {e}"),
        }
    }
}

/// fork/exec per the spec. Every fallible step after the child
/// exists either succeeds or tears the child down before returning
/// (the daemon's phase-1-critical-section discipline).
pub fn do_spawn(spec: &SpawnBody) -> Result<Spawned, SpawnError> {
    if spec.argv.is_empty() {
        return Err(SpawnError::EmptyArgv);
    }
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| SpawnError::Openpty(e.to_string()))?;

    let mut cmd = CommandBuilder::new(&spec.argv[0]);
    cmd.args(&spec.argv[1..]);
    // S1: the spec's env is COMPLETE — clear the inherited snapshot
    // (portable-pty seeds CommandBuilder from the CONSTRUCTING
    // process's environ) and apply exactly the map.
    cmd.env_clear();
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &spec.cwd {
        cmd.cwd(cwd);
    }

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| SpawnError::Exec(e.to_string()))?;
    // Parent closes the slave: only the child needs it, and the
    // eventual child exit then closes the PTY cleanly.
    drop(pair.slave);

    let pid: libc::pid_t = match child.process_id() {
        Some(p) => p as libc::pid_t,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SpawnError::PostSpawn(
                "portable-pty yielded no child pid".into(),
            ));
        }
    };
    let pidfd = match reap::open_pidfd(pid) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SpawnError::PostSpawn(format!("pidfd_open: {e}")));
        }
    };
    // Starttime read: for a FAST-EXIT child the /proc entry is still
    // present (zombie — nothing has reaped it; the holder consumes
    // waitid only under arm authorization), so this read succeeds
    // for both live and instantly-dead children.
    let child_start_time = match reap::read_proc_starttime(pid) {
        Ok(t) => t,
        Err(e) => {
            let _ = reap::pidfd_send_signal(&pidfd, libc::SIGKILL);
            let _ = child.wait();
            return Err(SpawnError::PostSpawn(format!("starttime read: {e}")));
        }
    };
    // The Child box's Drop neither kills nor waits (verified against
    // vendored portable-pty in review round 1) — ownership of the
    // child is the pidfd from here on.
    drop(child);

    Ok(Spawned {
        master: pair.master,
        pidfd,
        pid,
        child_start_time,
    })
}

/// Best-effort bounded read of a cgroup's `memory.events` — the
/// frozenness carve-out (S6): a single mechanical file read at
/// `waitid` time, no interpretation, racing scope teardown by
/// design (the residual is named in the design doc).
pub fn read_memory_events_snapshot(cgroup_path: &str) -> Option<String> {
    const CAP: u64 = 4096;
    let path = std::path::Path::new(cgroup_path).join("memory.events");
    let data = std::fs::read(&path).ok()?;
    let data = if data.len() as u64 > CAP {
        data[..CAP as usize].to_vec()
    } else {
        data
    };
    String::from_utf8(data).ok()
}

/// Unix seconds (f64) on the holder's clock — the `exited_at` /
/// attribution-stamp timebase.
pub fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Silence an unused-import warning on non-test builds.
#[allow(unused)]
fn _io_used(_: io::Error) {}
