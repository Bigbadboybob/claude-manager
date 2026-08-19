//! `cm-holder` binary — the thin bootstrap + brain-respawn loop
//! around [`cm_holder::holder::Holder::serve`]. DESIGN_HOLDER_BRAIN
//! _SPLIT phase 2: deliberately MINIMAL — this is a dev artifact for
//! phase-3 integration work. Phase 6 hardens it into the real
//! supervisor: pinned-FD brain binaries + one-deep rollback, the
//! breaker state table (BACKOFF/ROLLBACK/HELD_DOWN + path-retry +
//! SIGUSR2), wedge-watchdog consequences, SIGTERM forwarding with
//! the holder-executed stop, and OOM-posture writes. Until then:
//! immediate respawn with a flat 500ms pause, default signal
//! dispositions.
#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::Duration;

use cm_holder::holder::{Holder, HolderConfig, ServeOutcome};
use cm_holder_proto::channel::ENV_CHANNEL_FD;

/// The fd number the brain's channel end lands on in the child.
const BRAIN_CHANNEL_FD: i32 = 3;

fn main() {
    let mut brain_path: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--brain" => brain_path = args.next(),
            other => {
                eprintln!("cm-holder: unknown arg '{other}'");
                std::process::exit(2);
            }
        }
    }
    let Some(brain_path) = brain_path else {
        eprintln!("usage: cm-holder --brain <path>");
        std::process::exit(2);
    };

    raise_nofile_limit();

    let mut holder = Holder::new(HolderConfig::default());
    loop {
        let (ours, theirs) = match socketpair_cloexec() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("cm-holder: socketpair: {e}; retrying in 1s");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let mut cmd = Command::new(&brain_path);
        cmd.env(ENV_CHANNEL_FD, BRAIN_CHANNEL_FD.to_string());
        let theirs_raw = theirs.as_raw_fd();
        // SAFETY (pre_exec): dup2 in the child right before exec —
        // async-signal-safe, and dup2 clears CLOEXEC on the copy.
        unsafe {
            cmd.pre_exec(move || {
                if libc::dup2(theirs_raw, BRAIN_CHANNEL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut brain = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("cm-holder: brain spawn '{brain_path}': {e}; retrying in 1s");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        // The parent's copy of the brain's end closes now — channel
        // EOF then reliably means the brain (and its end) is gone.
        drop(theirs);

        let outcome = holder.serve(ours);
        eprintln!(
            "cm-holder: brain generation ended: {outcome:?} ({} sessions held)",
            holder.session_count()
        );
        match outcome {
            ServeOutcome::BrainEof => {
                let _ = brain.wait();
            }
            // Wedge / protocol violation / silent brain: the channel
            // is dead by law — kill and reap before respawning.
            ServeOutcome::Wedged
            | ServeOutcome::Protocol(_)
            | ServeOutcome::HelloTimeout
            | ServeOutcome::HelloRefused => {
                let _ = brain.kill();
                let _ = brain.wait();
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
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

/// 2 fds per session × MAX_SESSIONS ≈ 8k, plus headroom (design
/// § Bootstrap). Best-effort.
fn raise_nofile_limit() {
    let lim = libc::rlimit {
        rlim_cur: 65536,
        rlim_max: 65536,
    };
    // SAFETY: plain setrlimit; failure is tolerated.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } != 0 {
        eprintln!(
            "cm-holder: setrlimit(NOFILE, 65536) failed: {} (continuing)",
            std::io::Error::last_os_error()
        );
    }
}
