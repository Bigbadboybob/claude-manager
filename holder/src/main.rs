//! `cm-holder` binary — the near-frozen supervisor.
//! DESIGN_HOLDER_BRAIN_SPLIT phase 6 (§ Supervision): pinned-FD
//! brain binaries (R7 — a mid-deploy `cp` can never half-apply),
//! the breaker state machine (BACKOFF → ROLLBACK with
//! discard-not-demote → HELD_DOWN with path-retry + SIGUSR2), the
//! wedge-watchdog consequence (SIGKILL + respawn + strike), signal
//! handling (SIGTERM/SIGINT = the stop-everything sequence executed
//! BY THE HOLDER; SIGHUP ignored; SIGUSR1 status dump), OOM posture,
//! and the armed-deploy consumption (`restart_brain` /
//! `rollback_brain`, C8's arm-late rule).
//!
//! Env tunables (test knobs; production uses the defaults):
//!   CM_HOLDER_PING_MS            watchdog ping cadence (30000)
//!   CM_HOLDER_HELD_DOWN_RETRY_MS held-down path-retry (60000)
//!   CM_HOLDER_STABLE_HORIZON_MS  breaker stability horizon (600000)
#![cfg(target_os = "linux")]

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use cm_holder::holder::{
    ArmedDeploy, Holder, HolderConfig, ServeOutcome, SignalDirective, StatusSnapshot,
};
use cm_holder::supervisor::{Breaker, BreakerDecision, PinSet};
use cm_holder::reap;
use cm_holder_proto::channel::ENV_CHANNEL_FD;

/// The fd numbers the brain child sees: its channel end, and the
/// pinned binary it was exec'd from (via /proc/self/fd — the checked
/// artifact IS the executed artifact).
const BRAIN_CHANNEL_FD: RawFd = 3;
const BRAIN_EXEC_FD: RawFd = 4;

fn env_ms(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_ms),
    )
}

fn main() {
    let mut brain_path: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--brain" => brain_path = args.next().map(PathBuf::from),
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
    let sigfd = make_signalfd();

    let ping = env_ms("CM_HOLDER_PING_MS", 30_000);
    let held_down_retry = env_ms("CM_HOLDER_HELD_DOWN_RETRY_MS", 60_000);
    let stable_horizon = env_ms("CM_HOLDER_STABLE_HORIZON_MS", 600_000);

    let mut holder = Holder::new(HolderConfig {
        ping_interval: Some(ping),
        extra_fd: Some(sigfd.as_raw_fd()),
        ..HolderConfig::default()
    });
    let mut breaker = Breaker::default();
    breaker.stable_horizon = stable_horizon;
    let mut pins: PinSet<OwnedFd> = PinSet::default();
    match pin_from_path(&brain_path) {
        Some(pin) => pins.replace_current(pin),
        None => {
            eprintln!(
                "cm-holder: cannot open --brain {} — starting HELD_DOWN",
                brain_path.display()
            );
        }
    }

    loop {
        // ---- HELD_DOWN: no workable pin. Path-retry + SIGUSR2. ----
        if pins.current.is_none() {
            holder.set_supervisor_status("held_down", pins.previous.is_some());
            eprintln!(
                "cm-holder: HELD_DOWN — no workable brain pin; retrying {} every {:?} \
                 (SIGUSR2 forces an immediate retry; sessions are held alive)",
                brain_path.display(),
                held_down_retry
            );
            match held_down_wait(&sigfd, held_down_retry, &mut holder) {
                HeldDownEvent::Retry => {
                    if let Some(pin) = pin_from_path(&brain_path) {
                        eprintln!("cm-holder: HELD_DOWN retry — fresh pin from disk; resuming");
                        pins.replace_current(pin);
                        breaker.reset();
                    }
                }
                HeldDownEvent::Shutdown => {
                    shutdown_sequence(&mut holder, None);
                }
            }
            continue;
        }

        // ---- spawn the current pin ----
        holder.set_supervisor_status("running", pins.previous.is_some());
        let pin = pins.current.as_ref().expect("checked above");
        let (mut brain, ours) = match spawn_brain(pin) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("cm-holder: brain spawn failed: {e}");
                match breaker.note_failure(Duration::ZERO, false, pins.previous.is_some()) {
                    BreakerDecision::Respawn { backoff } => {
                        if sleep_or_shutdown(&sigfd, backoff, &mut holder) {
                            shutdown_sequence(&mut holder, None);
                        }
                    }
                    BreakerDecision::Rollback => {
                        eprintln!("cm-holder: BREAKER TRIPPED — discarding the failing pin, rolling back to the previous");
                        let _ = pins.rollback();
                    }
                    BreakerDecision::HoldDown => {
                        pins.current = None;
                    }
                }
                continue;
            }
        };
        reap::oom_score_adj_zero(brain.id() as libc::pid_t);
        let started = Instant::now();

        // ---- serve this generation ----
        let sig_raw = sigfd.as_raw_fd();
        let mut on_signal = |snap: &StatusSnapshot| handle_signal(sig_raw, snap);
        let outcome = holder.serve(ours, Some(&mut on_signal));
        let helloed = holder.generation_helloed();
        let ran_for = started.elapsed();

        match outcome {
            ServeOutcome::ShutdownRequested => {
                shutdown_sequence(&mut holder, Some(&mut brain));
            }
            ServeOutcome::BrainEof => {
                let _ = brain.wait();
                if let Some(deploy) = holder.take_armed_deploy() {
                    breaker.note_deploy();
                    match deploy {
                        ArmedDeploy::NewPin(pin) => {
                            eprintln!("cm-holder: deploy — exec'ing the new pinned brain (old pin kept as rollback)");
                            pins.install_new(pin);
                        }
                        ArmedDeploy::UsePrevious => {
                            eprintln!("cm-holder: operator rollback — reverting to the previous pin");
                            let _ = pins.rollback();
                        }
                    }
                    continue; // immediate respawn, no backoff
                }
                eprintln!(
                    "cm-holder: brain exited (ran {:?}, helloed: {helloed}) — a crash, not a deploy",
                    ran_for
                );
                apply_failure(&mut breaker, &mut pins, ran_for, helloed, &sigfd, &mut holder);
            }
            ServeOutcome::Wedged(reason)
            | ServeOutcome::Protocol(reason) => {
                eprintln!("cm-holder: brain declared dead ({reason}) — SIGKILL + respawn");
                let _ = brain.kill();
                let _ = brain.wait();
                // A deploy armed by a brain that then wedged is stale.
                let _ = holder.take_armed_deploy();
                apply_failure(&mut breaker, &mut pins, ran_for, helloed, &sigfd, &mut holder);
            }
            ServeOutcome::HelloTimeout | ServeOutcome::HelloRefused => {
                eprintln!("cm-holder: brain never negotiated ({outcome:?}) — SIGKILL + respawn");
                let _ = brain.kill();
                let _ = brain.wait();
                apply_failure(&mut breaker, &mut pins, ran_for, false, &sigfd, &mut holder);
            }
        }
    }
}

fn apply_failure(
    breaker: &mut Breaker,
    pins: &mut PinSet<OwnedFd>,
    ran_for: Duration,
    helloed: bool,
    sigfd: &OwnedFd,
    holder: &mut Holder,
) {
    match breaker.note_failure(ran_for, helloed, pins.previous.is_some()) {
        BreakerDecision::Respawn { backoff } => {
            eprintln!(
                "cm-holder: respawning current pin after {:?} (consecutive failures: {})",
                backoff,
                breaker.consecutive_failures()
            );
            if sleep_or_shutdown(sigfd, backoff, holder) {
                shutdown_sequence(holder, None);
            }
        }
        BreakerDecision::Rollback => {
            eprintln!(
                "cm-holder: BREAKER TRIPPED — discarding the failing pin, \
                 rolling back to the previous pinned brain (O5: the bad pin \
                 is gone, never demoted)"
            );
            let _ = pins.rollback();
        }
        BreakerDecision::HoldDown => {
            eprintln!("cm-holder: BREAKER TRIPPED with no previous pin — HELD_DOWN");
            pins.current = None;
        }
    }
}

// ============================================================
// Brain spawning (pinned-fd exec)
// ============================================================

/// Pin the brain binary by fd (R7): open read-only; the exec goes
/// through /proc/self/fd so the checked inode is the executed inode
/// even if a deploy overwrites the path.
fn pin_from_path(path: &std::path::Path) -> Option<OwnedFd> {
    match std::fs::File::open(path) {
        Ok(f) => Some(f.into()),
        Err(e) => {
            eprintln!("cm-holder: pin open {} failed: {e}", path.display());
            None
        }
    }
}

/// Spawn the brain from the pinned fd: socketpair channel on fd 3,
/// the pin dup'd to fd 4 and exec'd through the pin (see
/// [`pinned_exec_path`] for why the exec path is a named symlink and
/// not `/proc/self/fd/4` directly).
fn spawn_brain(pin: &OwnedFd) -> std::io::Result<(Child, OwnedFd)> {
    let (ours, theirs) = socketpair_cloexec()?;
    let theirs_raw = theirs.as_raw_fd();
    let pin_raw = pin.as_raw_fd();
    let mut cmd = Command::new(pinned_exec_path(pin));
    cmd.arg0("cm-daemon");
    cmd.env(ENV_CHANNEL_FD, BRAIN_CHANNEL_FD.to_string());
    // SAFETY (pre_exec): dup/dup2/close only — async-signal-safe.
    // Two fd-table hazards make the naive dup2s wrong: a source
    // already SITTING on a target slot gets clobbered by the other
    // dup2 (pin on fd 3 vs the channel's dup2 into 3), and
    // dup2(x, x) is a no-op that leaves CLOEXEC set (the fd then
    // closes at exec and /proc/self/fd/4 vanishes under the
    // interpreter). Lift both sources clear of the target range
    // first (F_DUPFD ≥ 5 — dups are born CLOEXEC-clear), place them
    // with dup2 (clears CLOEXEC on real copies), close the lifts so
    // no stray non-CLOEXEC channel/pin fd leaks into the brain (S10).
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
            if libc::dup2(t, BRAIN_CHANNEL_FD) < 0 || libc::dup2(p, BRAIN_EXEC_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            libc::close(t);
            libc::close(p);
            Ok(())
        });
    }
    let brain = cmd.spawn()?;
    // Our copy of the brain's end closes now — channel EOF then
    // reliably means the brain (and its dups) is gone.
    drop(theirs);
    Ok((brain, ours))
}

/// The path string handed to exec — the kernel derives the new
/// comm from ITS basename, so exec'ing `/proc/self/fd/4` directly
/// names every brain "4" (breaking ps/pgrep and any comm-based
/// discovery, for old brain vintages with no self-rename most of
/// all). Cross-process `/proc/<pid>/comm` writes are refused by the
/// kernel, so the fix is the path itself: a symlink named after the
/// pinned binary's dentry, pointing at `/proc/self/fd/4` — resolved
/// in the CHILD after the pre_exec dup2s, so the pin (not the
/// on-disk path) is still exactly what runs. The link lives under
/// `$HOME/.cm` (user-owned, unlike /tmp) and its content is a
/// constant string, so concurrent rewrites are benign. Any failure
/// falls back to the direct fd path: ugly comm, correct exec.
fn pinned_exec_path(pin: &OwnedFd) -> std::path::PathBuf {
    let direct = std::path::PathBuf::from(format!("/proc/self/fd/{BRAIN_EXEC_FD}"));
    let Some(home) = std::env::var_os("HOME") else {
        return direct;
    };
    let name = std::fs::read_link(format!("/proc/self/fd/{}", pin.as_raw_fd()))
        .map(|t| {
            t.to_string_lossy()
                .trim_end_matches(" (deleted)")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .ok()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "cm-daemon".to_string());
    let dir = std::path::Path::new(&home).join(".cm").join("holder-exec");
    if std::fs::create_dir_all(&dir).is_err() {
        return direct;
    }
    let link = dir.join(name);
    let _ = std::fs::remove_file(&link);
    match std::os::unix::fs::symlink(&direct, &link) {
        Ok(()) => link,
        Err(_) => direct,
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

// ============================================================
// Signals
// ============================================================

/// Block TERM/INT/HUP/USR1/USR2 and expose them as a signalfd the
/// serve loop polls — the single-threaded design's signal channel.
/// SIGHUP is read and IGNORED (config reload is the brain's
/// `daemon.reload_config`).
fn make_signalfd() -> OwnedFd {
    // SAFETY: sigset built with the libc initializers; signalfd with
    // -1 creates a fresh fd.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGUSR1,
            libc::SIGUSR2,
        ] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        let fd = libc::signalfd(-1, &set, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if fd < 0 {
            eprintln!(
                "cm-holder: signalfd failed: {} — exiting",
                std::io::Error::last_os_error()
            );
            std::process::exit(1);
        }
        OwnedFd::from_raw_fd(fd)
    }
}

/// Drain every pending siginfo; the strongest directive wins.
fn drain_signals(sigfd: RawFd) -> (bool /*shutdown*/, bool /*usr2*/) {
    let mut shutdown = false;
    let mut usr2 = false;
    loop {
        let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };
        // SAFETY: read of the fixed-size siginfo struct from the signalfd.
        let n = unsafe {
            libc::read(
                sigfd,
                &mut info as *mut _ as *mut libc::c_void,
                std::mem::size_of::<libc::signalfd_siginfo>(),
            )
        };
        if n != std::mem::size_of::<libc::signalfd_siginfo>() as isize {
            break;
        }
        match info.ssi_signo as i32 {
            libc::SIGTERM | libc::SIGINT => shutdown = true,
            libc::SIGUSR2 => usr2 = true,
            libc::SIGUSR1 => { /* status dump handled by caller */ }
            libc::SIGHUP => {
                eprintln!(
                    "cm-holder: SIGHUP ignored (config reload is the brain's \
                     daemon.reload_config; the brain pid is in daemon.health)"
                );
            }
            _ => {}
        }
    }
    (shutdown, usr2)
}

/// The serve-loop signal callback: SIGUSR1 dumps status; TERM/INT
/// asks for shutdown; USR2 is a no-op while a brain is being served.
fn handle_signal(sigfd: RawFd, snap: &StatusSnapshot) -> SignalDirective {
    let (shutdown, _usr2) = drain_signals(sigfd);
    eprintln!(
        "cm-holder: status — epoch {}, {} session(s) held, {} pending exit event(s)",
        snap.epoch, snap.sessions, snap.pending_exit_events
    );
    if shutdown {
        SignalDirective::Shutdown
    } else {
        SignalDirective::Continue
    }
}

enum HeldDownEvent {
    Retry,
    Shutdown,
}

/// Wait out one held-down interval, cut short by SIGUSR2 (retry now)
/// or TERM/INT (shutdown).
fn held_down_wait(sigfd: &OwnedFd, interval: Duration, holder: &mut Holder) -> HeldDownEvent {
    let deadline = Instant::now() + interval;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return HeldDownEvent::Retry;
        }
        let timeout = deadline.saturating_duration_since(now).as_millis().max(1) as i32;
        let mut pfd = libc::pollfd {
            fd: sigfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: valid pollfd for the call's duration.
        let ret = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if ret > 0 {
            let (shutdown, usr2) = drain_signals(sigfd.as_raw_fd());
            eprintln!(
                "cm-holder: status — HELD_DOWN, {} session(s) held",
                holder.session_count()
            );
            if shutdown {
                return HeldDownEvent::Shutdown;
            }
            if usr2 {
                return HeldDownEvent::Retry;
            }
        }
    }
}

/// Interruptible backoff sleep; `true` = shutdown was requested.
fn sleep_or_shutdown(sigfd: &OwnedFd, backoff: Duration, holder: &mut Holder) -> bool {
    match held_down_wait(sigfd, backoff, holder) {
        HeldDownEvent::Shutdown => true,
        HeldDownEvent::Retry => false,
    }
}

// ============================================================
// The stop-everything sequence (§ Supervision, S7)
// ============================================================

/// `systemctl stop` semantics: forward SIGTERM to the brain (it
/// persists + exits), wait bounded, SIGKILL it if needed; then THE
/// HOLDER kills + reaps every session child via its canonical pidfds
/// (children that ignore the PTY-teardown HUP must not outlive the
/// supervisor), unlinks the custodied socket, and exits.
fn shutdown_sequence(holder: &mut Holder, brain: Option<&mut Child>) -> ! {
    eprintln!("cm-holder: shutdown requested — stopping everything");
    if let Some(brain) = brain {
        let pid = brain.id() as libc::pid_t;
        // SAFETY: our direct child.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match brain.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => {
                    let _ = brain.kill();
                    let _ = brain.wait();
                    break;
                }
            }
        }
    }
    let killed = holder.shutdown_kill_all();
    if let Some(path) = holder.custodied_unix_path() {
        let _ = std::fs::remove_file(&path);
    }
    eprintln!("cm-holder: shutdown complete ({killed} session(s) stopped)");
    std::process::exit(0);
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
