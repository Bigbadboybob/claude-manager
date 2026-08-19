//! pidfd / waitid / starttime kernel utilities — the holder-local
//! siblings of `cm-daemon`'s `session.rs` helpers (`open_pidfd`,
//! `consume_exit_status`, `poll_pidfd_until_exit_ready`) and
//! `adopt.rs`'s starttime parse. Deliberately a copy, not an import:
//! the dependency arrow is daemon → proto and holder → proto, never
//! holder → daemon (frozenness), and the daemon keeps its own copies
//! for the retained monolith mode (design § Crate boundaries, O3).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// `pidfd_open(2)` on a live child we just spawned.
pub fn open_pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
    // SAFETY: plain syscall; on success we own the returned fd.
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(ret as RawFd) })
}

/// `pidfd_send_signal(2)`. `Ok(true)` = delivered; `Ok(false)` =
/// ESRCH (already gone — the caller's already-exited arm).
pub fn pidfd_send_signal(pidfd: &OwnedFd, sig: i32) -> io::Result<bool> {
    // SAFETY: plain syscall over an owned fd.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            sig,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if ret == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(err)
}

/// Non-consuming exit-readiness probe: zero-timeout `poll(2)` on the
/// pidfd (readable = the child has exited; the status is NOT
/// consumed — an exited child stays a reconstructible zombie).
pub fn pidfd_exit_ready(pidfd: &OwnedFd) -> bool {
    let mut pfd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: valid pollfd, zero timeout.
    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
    ret > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// A consumed exit status.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExitStatus {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// Consume the exit status via `waitid(P_PIDFD, …, WEXITED)` —
/// parent-only, the one duty that cannot live outside the holder.
/// Falls back to `waitpid(pid)` on pre-5.4 `EINVAL`.
pub fn consume_exit_status(pidfd: &OwnedFd, pid: libc::pid_t) -> ExitStatus {
    loop {
        // SAFETY: zeroed siginfo is a valid out-param for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let ret = unsafe {
            libc::waitid(
                libc::P_PIDFD,
                pidfd.as_raw_fd() as libc::id_t,
                &mut info,
                libc::WEXITED,
            )
        };
        if ret == 0 {
            // SAFETY: WEXITED + success ⇒ SIGCHLD-shaped siginfo;
            // si_status() is the defined union read.
            let st = unsafe { info.si_status() };
            return match info.si_code {
                libc::CLD_EXITED => ExitStatus {
                    code: Some(st),
                    signal: None,
                },
                libc::CLD_KILLED | libc::CLD_DUMPED => ExitStatus {
                    code: None,
                    signal: Some(st),
                },
                _ => ExitStatus {
                    code: None,
                    signal: None,
                },
            };
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EINVAL) => return wait_for_child(pid),
            _ => {
                return ExitStatus {
                    code: None,
                    signal: None,
                }
            }
        }
    }
}

/// `waitpid` fallback (pre-pidfd-waitid kernels).
fn wait_for_child(pid: libc::pid_t) -> ExitStatus {
    let mut status: libc::c_int = 0;
    loop {
        // SAFETY: plain waitpid with a valid out-param.
        let ret = unsafe { libc::waitpid(pid, &mut status, 0) };
        if ret == pid {
            return if libc::WIFEXITED(status) {
                ExitStatus {
                    code: Some(libc::WEXITSTATUS(status)),
                    signal: None,
                }
            } else if libc::WIFSIGNALED(status) {
                ExitStatus {
                    code: None,
                    signal: Some(libc::WTERMSIG(status)),
                }
            } else {
                ExitStatus {
                    code: None,
                    signal: None,
                }
            };
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return ExitStatus {
                code: None,
                signal: None,
            };
        }
    }
}

/// Read `/proc/<pid>/stat` field 22 (kernel starttime) — the R6
/// pid-reuse cross-check, captured at spawn.
pub fn read_proc_starttime(pid: libc::pid_t) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    parse_proc_stat_starttime(&stat)
        .ok_or_else(|| io::Error::other("unparseable /proc stat line"))
}

/// Split after the LAST `)` (comm is the only unescaped field), then
/// field 22 is index 19 of the remainder — same parse as the
/// daemon's `adopt.rs`.
pub fn parse_proc_stat_starttime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// OOM posture (§ Supervision, S11): `oom_score_adj` is INHERITED
/// across fork/exec, so children of a systemd-protected holder
/// (`OOMScoreAdjust=-500`) would inherit the protection — inverting
/// the intent (the biggest consumers most protected). Raise each
/// spawned process back to 0 (raise-only is unprivileged-legal);
/// best-effort — locally the holder already runs at 0 and this is a
/// no-op.
pub fn oom_score_adj_zero(pid: libc::pid_t) {
    let _ = std::fs::write(format!("/proc/{pid}/oom_score_adj"), "0");
}

/// `F_DUPFD_CLOEXEC` a raw fd into an `OwnedFd` — used to mint the
/// SCM_RIGHTS dups the brain receives.
pub fn dup_cloexec(raw: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl dup of a valid fd; on success we own the result.
    let duped = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
    if duped < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duped) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starttime_parse_survives_hostile_comm() {
        let line = "4242 (sh) 0 0 0) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 777 21";
        assert_eq!(parse_proc_stat_starttime(line), Some(777));
    }

    #[test]
    fn starttime_parse_of_own_process_works() {
        let t = read_proc_starttime(std::process::id() as libc::pid_t).unwrap();
        assert!(t > 0);
    }
}
