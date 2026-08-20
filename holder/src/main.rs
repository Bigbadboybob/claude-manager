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
//! Phase 7 (§ Live migration, § Holder upgrades) adds three boot
//! modes and two exec paths:
//!
//! - **Migration boot**: exec'd BY the monolith (same PID) with a
//!   schema-v4 sealed manifest — adopt the sessions + listener
//!   custody + the parked brain the monolith spawned, and serve.
//!   Post-validation init failure rolls back by writing a fresh
//!   standard-schema manifest and exec'ing the pinned monolith (C3's
//!   trusted branch); a manifest that fails validation boots FRESH
//!   touching no escrow fd (the corrupt-manifest rule — the named
//!   crash-class residual of the migration exec).
//! - **Upgrade boot**: exec'd by OURSELVES (`reexec_holder`) with the
//!   holder-upgrade manifest — rebuild full holder state (incarnation
//!   high-water included, V3) and resume the SAME brain generation
//!   via the unsolicited `rehello`.
//! - **Reverse migration**: the `split_rollback` armed deploy — on
//!   the brain's exit, write a standard-schema manifest from the
//!   brain-composed rollback records + our fd/pid fields and exec the
//!   pinned monolith instead of respawning a brain.
//!
//! Env tunables (test knobs; production uses the defaults):
//!   CM_HOLDER_PING_MS            watchdog ping cadence (30000)
//!   CM_HOLDER_HELD_DOWN_RETRY_MS held-down path-retry (60000)
//!   CM_HOLDER_STABLE_HORIZON_MS  breaker stability horizon (600000)
//!   CM_HOLDER_TEST_FAIL_MIGRATION_INIT  e2e hook: fail migration
//!     init AFTER manifest validation (exercises the C3 rollback)
#![cfg(target_os = "linux")]

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use cm_holder::holder::{
    ArmedDeploy, Holder, HolderConfig, ServeOutcome, SignalDirective, StatusSnapshot,
};
use cm_holder::reap;
use cm_holder::supervisor::{Breaker, BreakerDecision, PinSet};
use cm_holder_proto::channel::{ENV_CHANNEL_FD, PROTO_VERSION_MAX, PROTO_VERSION_MIN};
use cm_holder_proto::holder_manifest as hm;
use cm_holder_proto::reexec_manifest as rm;

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

/// The brain the supervisor watches — pid + pidfd, NOT a
/// `std::process::Child`: a migration-boot holder supervises a brain
/// the MONOLITH forked (our child only by virtue of the same-PID
/// exec), and an upgrade-boot holder supervises one its previous
/// image spawned. pidfd-based kill/wait works uniformly for all
/// three origins.
struct BrainHandle {
    pid: libc::pid_t,
    pidfd: OwnedFd,
}

impl BrainHandle {
    fn from_pid(pid: libc::pid_t) -> io::Result<BrainHandle> {
        Ok(BrainHandle {
            pid,
            pidfd: reap::open_pidfd(pid)?,
        })
    }

    fn sigkill_and_reap(&self) {
        let _ = reap::pidfd_send_signal(&self.pidfd, libc::SIGKILL);
        let _ = reap::consume_exit_status(&self.pidfd, self.pid);
    }

    /// Reap after an observed exit (channel EOF): non-racy — the
    /// pidfd names exactly the process we spawned.
    fn reap(&self) {
        let _ = reap::consume_exit_status(&self.pidfd, self.pid);
    }
}

fn main() {
    // Deterministic comm regardless of how we were exec'd (a
    // /proc/self/fd or execveat(AT_EMPTY_PATH) exec derives comm from
    // the path STRING, which may be an fd number).
    set_own_comm("cm-holder");

    let mut brain_path: Option<PathBuf> = None;
    let mut preflight = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--brain" => brain_path = args.next().map(PathBuf::from),
            "--holder-preflight" => preflight = true,
            other => {
                eprintln!("cm-holder: unknown arg '{other}'");
                std::process::exit(2);
            }
        }
    }
    if preflight {
        std::process::exit(run_holder_preflight());
    }
    let Some(brain_path) = brain_path else {
        eprintln!("usage: cm-holder --brain <path> [--holder-preflight]");
        std::process::exit(2);
    };

    raise_nofile_limit();
    let sigfd = make_signalfd();

    let ping = env_ms("CM_HOLDER_PING_MS", 30_000);
    let held_down_retry = env_ms("CM_HOLDER_HELD_DOWN_RETRY_MS", 60_000);
    let stable_horizon = env_ms("CM_HOLDER_STABLE_HORIZON_MS", 600_000);

    let cfg = HolderConfig {
        ping_interval: Some(ping),
        extra_fd: Some(sigfd.as_raw_fd()),
        ..HolderConfig::default()
    };
    let mut breaker = Breaker::default();
    breaker.stable_horizon = stable_horizon;
    let mut pins: PinSet<OwnedFd> = PinSet::default();

    // ---- boot detection: upgrade manifest → migration manifest →
    // fresh. Each consume_env scrubs its var unconditionally (R14).
    let mut holder: Holder;
    // A brain that is ALREADY RUNNING at boot (migration's parked
    // brain, or the surviving brain across an upgrade exec), plus its
    // channel and whether the generation resumes (upgrade) or starts
    // (migration/fresh spawn).
    let mut live: Option<(BrainHandle, OwnedFd, bool)> = None;

    if let Some(fd) = hm::consume_env() {
        match upgrade_boot(fd, cfg.clone()) {
            Ok((h, restored, brain, pins_restored)) => {
                eprintln!(
                    "cm-holder: holder-upgrade manifest restored — {} session(s), epoch {}",
                    h.session_count(),
                    restored.epoch,
                );
                holder = h;
                breaker.restore_failures(restored.breaker_consecutive_failures);
                pins = pins_restored;
                live = brain;
            }
            Err(e) => {
                eprintln!(
                    "cm-holder: {}={} present but the holder-upgrade manifest \
                     FAILED validation: {e} — booting FRESH, touching no \
                     escrow fd (corrupt-manifest rule; this is the holder-\
                     upgrade crash-class residual: sessions lost)",
                    hm::ENV_HOLDER_MANIFEST_FD, fd,
                );
                holder = Holder::new(cfg.clone());
            }
        }
    } else if let Some(fd) = rm::consume_env() {
        match migration_boot(fd, cfg.clone(), &brain_path) {
            Ok((h, brain, channel)) => {
                eprintln!(
                    "cm-holder: MIGRATION boot committed — {} session(s) \
                     adopted, parked brain pid {} supervised, listener in \
                     custody",
                    h.session_count(),
                    brain.pid,
                );
                holder = h;
                pins = migration_pins_taken();
                live = Some((brain, channel, false));
            }
            Err(MigrationBootError::Corrupt(e)) => {
                eprintln!(
                    "cm-holder: {}={} present but the migration manifest \
                     FAILED validation: {e} — booting FRESH, touching no \
                     escrow fd (corrupt-manifest rule; the named crash-class \
                     residual of the migration exec: sessions lost). The \
                     parked brain exits on its 60s adopt deadline.",
                    rm::ENV_MANIFEST_FD, fd,
                );
                holder = Holder::new(cfg.clone());
            }
            Err(MigrationBootError::InitFailed { detail, rollback }) => {
                eprintln!(
                    "cm-holder: migration init FAILED after validation \
                     ({detail}) — rolling back to the pinned monolith (C3's \
                     trusted branch: the manifest validated, so its pins are \
                     real pins)"
                );
                // Diverges on success; returns only if the rollback
                // exec itself failed — then the honest terminal state
                // is a loud exit (the monolith image is gone, the
                // brain never adopted; children survive as orphans
                // for startup-restore).
                let err = migration_rollback_exec(rollback);
                eprintln!(
                    "cm-holder: migration ROLLBACK EXEC FAILED: {err} — \
                     exiting; recover via startup-restore"
                );
                std::process::exit(70);
            }
        }
    } else {
        holder = Holder::new(cfg.clone());
    }

    if pins.current.is_none() {
        match pin_from_path(&brain_path) {
            Some(pin) => pins.replace_current(pin),
            None => {
                eprintln!(
                    "cm-holder: cannot open --brain {} — starting HELD_DOWN",
                    brain_path.display()
                );
            }
        }
    }

    loop {
        // ---- HELD_DOWN: no workable pin. Path-retry + SIGUSR2. ----
        if pins.current.is_none() && live.is_none() {
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

        // ---- take the live brain (boot-carried), or spawn one ----
        holder.set_supervisor_status("running", pins.previous.is_some());
        let (brain, ours, resumed) = match live.take() {
            Some(t) => t,
            None => {
                let pin = pins.current.as_ref().expect("checked above");
                match spawn_brain(pin) {
                    Ok((brain, ours)) => (brain, ours, false),
                    Err(e) => {
                        eprintln!("cm-holder: brain spawn failed: {e}");
                        match breaker.note_failure(
                            Duration::ZERO,
                            false,
                            pins.previous.is_some(),
                        ) {
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
                }
            }
        };
        reap::oom_score_adj_zero(brain.pid);
        let started = Instant::now();

        // ---- serve this generation ----
        let sig_raw = sigfd.as_raw_fd();
        let mut on_signal = |snap: &StatusSnapshot| handle_signal(sig_raw, snap);
        let outcome = if resumed {
            holder.serve_resumed(ours, Some(&mut on_signal))
        } else {
            holder.serve(ours, Some(&mut on_signal))
        };
        let helloed = holder.generation_helloed();
        let ran_for = started.elapsed();

        match outcome {
            ServeOutcome::ShutdownRequested => {
                shutdown_sequence(&mut holder, Some(&brain));
            }
            ServeOutcome::HolderUpgrade { pin, channel } => {
                // § Holder upgrades: the ok reply flushed; write the
                // holder-upgrade manifest and exec the new image. The
                // brain (and every session) survives the exec.
                let err = holder_upgrade_exec(
                    &holder,
                    pin,
                    &channel,
                    &brain,
                    &pins,
                    &breaker,
                    &brain_path,
                );
                // Exec returned — failure. Resume the SAME generation
                // over the still-open channel via rehello (the brain
                // saw nothing but a pause).
                eprintln!(
                    "cm-holder: holder upgrade exec FAILED ({err}) — resuming \
                     the current image and brain generation"
                );
                live = Some((brain, channel, true));
            }
            ServeOutcome::BrainEof => {
                brain.reap();
                if let Some(deploy) = holder.take_armed_deploy() {
                    match deploy {
                        ArmedDeploy::NewPin(pin) => {
                            breaker.note_deploy();
                            eprintln!("cm-holder: deploy — exec'ing the new pinned brain (old pin kept as rollback)");
                            pins.install_new(pin);
                        }
                        ArmedDeploy::UsePrevious => {
                            breaker.note_deploy();
                            eprintln!("cm-holder: operator rollback — reverting to the previous pin");
                            let _ = pins.rollback();
                        }
                        ArmedDeploy::SplitRollback {
                            pin,
                            reexec_generation,
                            schema_version,
                        } => {
                            eprintln!(
                                "cm-holder: REVERSE MIGRATION — writing the \
                                 standard-schema manifest (v{schema_version}) and \
                                 exec'ing the pinned monolith"
                            );
                            // Diverges on success. On failure the brain
                            // is gone but sessions are held — respawn a
                            // brain from the current pin and stay split.
                            let err = split_rollback_exec(
                                &holder,
                                pin,
                                reexec_generation,
                                schema_version,
                            );
                            eprintln!(
                                "cm-holder: REVERSE MIGRATION EXEC FAILED: {err} \
                                 — staying split; respawning a brain from the \
                                 current pin (sessions held)"
                            );
                        }
                    }
                    continue; // immediate respawn / post-failure respawn
                }
                eprintln!(
                    "cm-holder: brain exited (ran {:?}, helloed: {helloed}) — a crash, not a deploy",
                    ran_for
                );
                apply_failure(&mut breaker, &mut pins, ran_for, helloed, &sigfd, &mut holder);
            }
            ServeOutcome::Wedged(reason) | ServeOutcome::Protocol(reason) => {
                eprintln!("cm-holder: brain declared dead ({reason}) — SIGKILL + respawn");
                brain.sigkill_and_reap();
                // A deploy armed by a brain that then wedged is stale.
                let _ = holder.take_armed_deploy();
                apply_failure(&mut breaker, &mut pins, ran_for, helloed, &sigfd, &mut holder);
            }
            ServeOutcome::HelloTimeout | ServeOutcome::HelloRefused => {
                eprintln!("cm-holder: brain never negotiated ({outcome:?}) — SIGKILL + respawn");
                brain.sigkill_and_reap();
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
// Phase 7: boot modes + exec paths
// ============================================================

/// Migration-boot pins, moved out through a thread-local because the
/// boot fn's return is already three-tuple-shaped; kept dead simple.
static MIGRATION_PINS: std::sync::Mutex<Option<PinSet<OwnedFd>>> =
    std::sync::Mutex::new(None);

fn migration_pins_taken() -> PinSet<OwnedFd> {
    MIGRATION_PINS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .take()
        .unwrap_or_default()
}

enum MigrationBootError {
    /// Validation failed — no escrow fd touched; boot fresh (C3's
    /// untrusted branch, the migration's crash-class residual).
    Corrupt(String),
    /// The manifest VALIDATED but a later init step failed — its pins
    /// are trusted; roll back to the monolith (C3's trusted branch).
    InitFailed {
        detail: String,
        rollback: MigrationRollback,
    },
}

/// Everything the post-validation rollback path needs, owned.
struct MigrationRollback {
    manifest: rm::ReexecManifest,
    rollback_fd: OwnedFd,
    /// Owned fds parallel to manifest.sessions (master, pidfd).
    session_fds: Vec<(OwnedFd, OwnedFd)>,
    listener_fd: OwnedFd,
    tls_listener_fd: Option<OwnedFd>,
    brain: Option<BrainHandle>,
}

/// § Live migration step 8: read + validate the v4 manifest, take
/// ownership of every escrowed fd, build the holder's state, and
/// hand back the parked brain's handle + channel.
fn migration_boot(
    fd: RawFd,
    cfg: HolderConfig,
    brain_path: &std::path::Path,
) -> Result<(Holder, BrainHandle, OwnedFd), MigrationBootError> {
    // SAFETY: validation-only borrow; ownership is taken only after
    // the manifest verifies.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let manifest = rm::read_manifest(borrowed)
        .and_then(|m| {
            rm::validate_fd_roles(&m)?;
            Ok(m)
        })
        .map_err(|e| MigrationBootError::Corrupt(e.to_string()))?;
    if manifest.schema_version != rm::MANIFEST_SCHEMA_VERSION_SPLIT {
        return Err(MigrationBootError::Corrupt(format!(
            "cm-holder was exec'd with a schema v{} manifest — only the \
             v{} migration schema boots a holder",
            manifest.schema_version,
            rm::MANIFEST_SCHEMA_VERSION_SPLIT
        )));
    }
    let split = manifest.split.clone().expect("v4 validated ⇒ split present");

    // Ownership transfer (single-owner per the duplicate-fd check).
    // SAFETY: numbers come from the sealed, role-validated manifest.
    let manifest_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let rollback_fd = unsafe { OwnedFd::from_raw_fd(manifest.rollback_bin_fd) };
    let listener_fd = unsafe { OwnedFd::from_raw_fd(manifest.listener_fd) };
    let tls_listener_fd = manifest
        .tls_listener_fd
        .map(|raw| unsafe { OwnedFd::from_raw_fd(raw) });
    let channel = unsafe { OwnedFd::from_raw_fd(split.channel_fd) };
    let brain_pidfd = unsafe { OwnedFd::from_raw_fd(split.brain_pidfd) };
    let brain_pin = unsafe { OwnedFd::from_raw_fd(split.brain_pin_fd) };
    let brain_pin_previous = split
        .brain_pin_previous_fd
        .map(|raw| unsafe { OwnedFd::from_raw_fd(raw) });
    let session_fds: Vec<(OwnedFd, OwnedFd)> = manifest
        .sessions
        .iter()
        .map(|rec| unsafe {
            (
                OwnedFd::from_raw_fd(rec.pty_master_fd),
                OwnedFd::from_raw_fd(rec.pidfd),
            )
        })
        .collect();
    drop(manifest_fd);

    // R9/S10 hygiene at the earliest moment: everything crossed the
    // exec CLOEXEC-cleared by necessity; re-set the flag so nothing
    // we spawn from here (brains most of all) can inherit a session
    // master, a pidfd, or the channel.
    for (m, p) in &session_fds {
        set_cloexec(m.as_raw_fd());
        set_cloexec(p.as_raw_fd());
    }
    set_cloexec(listener_fd.as_raw_fd());
    if let Some(fd) = &tls_listener_fd {
        set_cloexec(fd.as_raw_fd());
    }
    set_cloexec(channel.as_raw_fd());
    set_cloexec(brain_pidfd.as_raw_fd());
    set_cloexec(brain_pin.as_raw_fd());
    if let Some(fd) = &brain_pin_previous {
        set_cloexec(fd.as_raw_fd());
    }
    set_cloexec(rollback_fd.as_raw_fd());

    let brain = BrainHandle {
        pid: split.brain_pid as libc::pid_t,
        pidfd: brain_pidfd,
    };

    let fail = |detail: String,
                    brain: BrainHandle,
                    session_fds: Vec<(OwnedFd, OwnedFd)>,
                    listener_fd: OwnedFd,
                    tls_listener_fd: Option<OwnedFd>,
                    rollback_fd: OwnedFd| {
        MigrationBootError::InitFailed {
            detail,
            rollback: MigrationRollback {
                manifest: manifest.clone(),
                rollback_fd,
                session_fds,
                listener_fd,
                tls_listener_fd,
                brain: Some(brain),
            },
        }
    };

    // e2e hook: force the post-validation failure branch.
    if std::env::var("CM_HOLDER_TEST_FAIL_MIGRATION_INIT").as_deref() == Ok("1") {
        return Err(fail(
            "CM_HOLDER_TEST_FAIL_MIGRATION_INIT=1 (e2e hook)".into(),
            brain,
            session_fds,
            listener_fd,
            tls_listener_fd,
            rollback_fd,
        ));
    }

    // Identity cross-checks (R6): pidfd-alive + starttime. An exited
    // child is NOT a failure — its exit is discovered by the poll
    // loop and tombstoned through the ordinary pipeline; a STARTTIME
    // MISMATCH is (pid reuse — the manifest's fds don't name what it
    // says they name).
    for (rec, (_, pidfd)) in manifest.sessions.iter().zip(&session_fds) {
        if reap::pidfd_exit_ready(pidfd) {
            continue; // exited during the swap — adopted, then reaped
        }
        match reap::read_proc_starttime(rec.child_pid as libc::pid_t) {
            Ok(st) if st == rec.child_start_time => {}
            Ok(st) => {
                return Err(fail(
                    format!(
                        "session '{}' starttime mismatch: /proc says {st}, \
                         manifest says {} (pid reuse?)",
                        rec.uid, rec.child_start_time
                    ),
                    brain,
                    session_fds,
                    listener_fd,
                    tls_listener_fd,
                    rollback_fd,
                ));
            }
            Err(e) => {
                return Err(fail(
                    format!(
                        "session '{}' /proc starttime read failed: {e}",
                        rec.uid
                    ),
                    brain,
                    session_fds,
                    listener_fd,
                    tls_listener_fd,
                    rollback_fd,
                ));
            }
        }
    }

    // Commit: build the holder.
    let mut holder = Holder::new(cfg);
    for (rec, (master, pidfd)) in manifest.sessions.iter().zip(session_fds) {
        holder.adopt_migrated_session(rec, master, pidfd);
    }
    let unix_meta = cm_holder_proto::channel::ListenerMeta {
        kind: "unix".into(),
        meta: unix_listener_path(listener_fd.as_raw_fd()).unwrap_or_default(),
    };
    holder.store_listener_custody(unix_meta, listener_fd);
    if let Some(fd) = tls_listener_fd {
        holder.store_listener_custody(
            cm_holder_proto::channel::ListenerMeta {
                kind: "tls".into(),
                meta: String::new(),
            },
            fd,
        );
    }
    let mut pins = PinSet::default();
    pins.previous = brain_pin_previous;
    pins.replace_current(brain_pin);
    *MIGRATION_PINS.lock().unwrap_or_else(|p| p.into_inner()) = Some(pins);
    // The rollback pin's job ends at commit (nothing past this point
    // rolls back — brain failures are supervision's problem now).
    drop(rollback_fd);
    let _ = brain_path; // identity comes from the manifest; the arg
                        // is the HELD_DOWN re-pin target as always
    Ok((holder, brain, channel))
}

/// C3's trusted branch: the manifest VALIDATED, so its pins are real.
/// Write a fresh manifest at `rollback_schema_version` (the version
/// the rollback pin reads — S5's prior-version emission), kill + reap
/// the parked brain (it holds a channel end; EOF-exit is its backstop
/// anyway), clear CLOEXEC on exactly the handed-off fds, and exec the
/// pinned monolith. Nothing was ever armed, so no `waitid` was
/// consumed (C2's invariant holds by construction).
///
/// Diverges on success; returns the error if the exec failed.
fn migration_rollback_exec(rb: MigrationRollback) -> String {
    let version = rb
        .manifest
        .rollback_schema_version
        .unwrap_or(rm::MANIFEST_SCHEMA_VERSION);
    let mut fresh = rb.manifest.clone();
    fresh.schema_version = version;
    fresh.attempt = rb.manifest.attempt.saturating_add(1);
    fresh.split = None;
    fresh.rollback_schema_version = None;
    let fresh_fd = match rm::write_manifest(&fresh) {
        Ok(fd) => fd,
        Err(e) => return format!("write rollback manifest: {e}"),
    };

    if let Some(brain) = &rb.brain {
        brain.sigkill_and_reap();
    }

    // CLOEXEC-clear exactly the handed-off set.
    let mut inherit: Vec<RawFd> = vec![
        fresh_fd.as_raw_fd(),
        rb.rollback_fd.as_raw_fd(),
        rb.listener_fd.as_raw_fd(),
    ];
    if let Some(fd) = &rb.tls_listener_fd {
        inherit.push(fd.as_raw_fd());
    }
    for (m, p) in &rb.session_fds {
        inherit.push(m.as_raw_fd());
        inherit.push(p.as_raw_fd());
    }
    for fd in &inherit {
        clear_cloexec(*fd);
    }
    eprintln!(
        "cm-holder: rollback exec — execveat'ing the pinned monolith with {} \
         session(s), manifest at schema v{version}, attempt {}",
        rb.manifest.sessions.len(),
        fresh.attempt,
    );
    execveat_pinned(
        rb.rollback_fd.as_raw_fd(),
        &["cm-daemon"],
        &[(rm::ENV_MANIFEST_FD, &fresh_fd.as_raw_fd().to_string())],
    )
}

/// Upgrade boot: rebuild from the holder-upgrade manifest written by
/// our previous image. Returns the holder, the manifest (for restored
/// scalar state), the live brain (+ channel, resumed=true), and pins.
#[allow(clippy::type_complexity)]
fn upgrade_boot(
    fd: RawFd,
    cfg: HolderConfig,
) -> Result<
    (
        Holder,
        hm::HolderUpgradeManifest,
        Option<(BrainHandle, OwnedFd, bool)>,
        PinSet<OwnedFd>,
    ),
    String,
> {
    // SAFETY: validation-only borrow until the manifest verifies.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let manifest =
        hm::read_holder_manifest(borrowed).map_err(|e| e.to_string())?;
    // SAFETY: single-owner numbers from the validated manifest.
    let manifest_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    drop(manifest_fd);
    let holder = Holder::restore_from_upgrade(cfg, &manifest);
    let mut pins: PinSet<OwnedFd> = PinSet::default();
    // SAFETY: as above.
    pins.previous = manifest
        .brain_pin_previous_fd
        .map(|raw| unsafe { OwnedFd::from_raw_fd(raw) });
    pins.current = manifest
        .brain_pin_fd
        .map(|raw| unsafe { OwnedFd::from_raw_fd(raw) });
    let live = manifest.brain.as_ref().map(|b| {
        // SAFETY: as above.
        let pidfd = unsafe { OwnedFd::from_raw_fd(b.pidfd) };
        let channel = unsafe { OwnedFd::from_raw_fd(b.channel_fd) };
        (
            BrainHandle {
                pid: b.pid as libc::pid_t,
                pidfd,
            },
            channel,
            true,
        )
    });
    // Post-exec CLOEXEC hygiene, as in migration boot.
    for raw in holder.escrow_fds() {
        set_cloexec(raw);
    }
    if let Some((brain, channel, _)) = &live {
        set_cloexec(brain.pidfd.as_raw_fd());
        set_cloexec(channel.as_raw_fd());
    }
    if let Some(p) = &pins.current {
        set_cloexec(p.as_raw_fd());
    }
    if let Some(p) = &pins.previous {
        set_cloexec(p.as_raw_fd());
    }
    Ok((holder, manifest, live, pins))
}

/// § Holder upgrades: write the holder-upgrade manifest, CLOEXEC-
/// clear the escrow, and exec the pinned new holder image. The brain
/// survives; the new image re-hellos over the carried channel.
/// Diverges on success; returns the failure reason otherwise (every
/// flag it cleared is re-set before returning).
fn holder_upgrade_exec(
    holder: &Holder,
    pin: OwnedFd,
    channel: &OwnedFd,
    brain: &BrainHandle,
    pins: &PinSet<OwnedFd>,
    breaker: &Breaker,
    brain_path: &std::path::Path,
) -> String {
    let manifest = holder.upgrade_snapshot(
        Some(hm::BrainRuntime {
            pid: brain.pid,
            pidfd: brain.pidfd.as_raw_fd(),
            channel_fd: channel.as_raw_fd(),
        }),
        pins.current.as_ref().map(|p| p.as_raw_fd()),
        pins.previous.as_ref().map(|p| p.as_raw_fd()),
        breaker.consecutive_failures(),
        &brain_path.to_string_lossy(),
    );
    let manifest_fd = match hm::write_holder_manifest(&manifest) {
        Ok(fd) => fd,
        Err(e) => return format!("write holder-upgrade manifest: {e}"),
    };
    let mut inherit: Vec<RawFd> = holder.escrow_fds();
    inherit.push(manifest_fd.as_raw_fd());
    inherit.push(pin.as_raw_fd());
    inherit.push(channel.as_raw_fd());
    inherit.push(brain.pidfd.as_raw_fd());
    if let Some(p) = &pins.current {
        inherit.push(p.as_raw_fd());
    }
    if let Some(p) = &pins.previous {
        inherit.push(p.as_raw_fd());
    }
    for fd in &inherit {
        clear_cloexec(*fd);
    }
    eprintln!(
        "cm-holder: HOLDER UPGRADE — exec'ing the pinned new holder image \
         ({} session(s), epoch {}, brain pid {} survives)",
        manifest.sessions.len(),
        manifest.epoch,
        brain.pid,
    );
    let err = execveat_pinned(
        pin.as_raw_fd(),
        &[
            "cm-holder",
            "--brain",
            &brain_path.to_string_lossy(),
        ],
        &[(
            hm::ENV_HOLDER_MANIFEST_FD,
            &manifest_fd.as_raw_fd().to_string(),
        )],
    );
    // Exec failed — restore CLOEXEC so the escrow can't leak.
    for fd in &inherit {
        set_cloexec(*fd);
    }
    err
}

/// Reverse migration's exec half (§ Live migration, V4): project the
/// standard-schema manifest from the stored rollback records + our
/// fd/pid fields, CLOEXEC-clear the handed-off set, exec the pinned
/// monolith. Diverges on success.
fn split_rollback_exec(
    holder: &Holder,
    pin: OwnedFd,
    reexec_generation: u64,
    schema_version: u32,
) -> String {
    // The manifest needs a rollback slot distinct from the exec
    // target: a dup of the monolith pin (same inode, new number).
    // SAFETY: plain F_DUPFD_CLOEXEC on an owned fd.
    let rollback_dup = unsafe {
        let d = libc::fcntl(pin.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3);
        if d < 0 {
            return format!(
                "dup monolith pin for the rollback slot: {}",
                io::Error::last_os_error()
            );
        }
        OwnedFd::from_raw_fd(d)
    };
    let manifest = match holder.rollback_manifest(
        schema_version,
        reexec_generation,
        rollback_dup.as_raw_fd(),
    ) {
        Ok(m) => m,
        Err(e) => return format!("compose rollback manifest: {e}"),
    };
    let manifest_fd = match rm::write_manifest(&manifest) {
        Ok(fd) => fd,
        Err(e) => return format!("write rollback manifest: {e}"),
    };
    let mut inherit: Vec<RawFd> = holder.escrow_fds();
    inherit.push(manifest_fd.as_raw_fd());
    inherit.push(pin.as_raw_fd());
    inherit.push(rollback_dup.as_raw_fd());
    for fd in &inherit {
        clear_cloexec(*fd);
    }
    eprintln!(
        "cm-holder: reverse migration — execveat'ing the pinned monolith with \
         {} session(s), manifest at schema v{schema_version}",
        manifest.sessions.len(),
    );
    let err = execveat_pinned(
        pin.as_raw_fd(),
        &["cm-daemon"],
        &[(rm::ENV_MANIFEST_FD, &manifest_fd.as_raw_fd().to_string())],
    );
    for fd in &inherit {
        set_cloexec(*fd);
    }
    err
}

/// `--holder-preflight`: prove this image can run on this host and
/// print the machine-readable facts the brain's preflight gates need
/// (S15: the upgraded holder must still negotiate with the previous
/// brain pin — proto range is the checkable proxy). Exit 0 = fit.
fn run_holder_preflight() -> i32 {
    // openpty probe — the one capability a holder cannot do without.
    match portable_pty::native_pty_system().openpty(portable_pty::PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(_) => {}
        Err(e) => {
            eprintln!("cm-holder-preflight: openpty probe FAILED: {e}");
            return 1;
        }
    }
    println!("cm-holder-preflight: ok");
    println!("proto_min={PROTO_VERSION_MIN}");
    println!("proto_max={PROTO_VERSION_MAX}");
    println!("build_id=cm-holder/{}", env!("CARGO_PKG_VERSION"));
    0
}

// ============================================================
// Exec + fd-flag helpers
// ============================================================

fn set_own_comm(name: &str) {
    let c = CString::new(name).expect("no NUL");
    // SAFETY: PR_SET_NAME with a valid NUL-terminated string.
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c.as_ptr(), 0, 0, 0);
    }
}

fn set_cloexec(fd: RawFd) {
    // SAFETY: plain fcntl flag write; failure tolerated (hygiene).
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

fn clear_cloexec(fd: RawFd) {
    // SAFETY: as above.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
        }
    }
}

/// `execveat(fd, "", argv, envp, AT_EMPTY_PATH)`. envp = the current
/// environment minus every `CM_REEXEC_*` / `CM_HOLDER_UPGRADE_*` var,
/// plus `extra_env`. Returns only on failure.
fn execveat_pinned(
    target_fd: RawFd,
    argv: &[&str],
    extra_env: &[(&str, &str)],
) -> String {
    let argv_c: Vec<CString> = argv
        .iter()
        .filter_map(|a| CString::new(*a).ok())
        .collect();
    let mut envp_c: Vec<CString> = Vec::new();
    for (k, v) in std::env::vars_os() {
        let kb = k.as_encoded_bytes();
        if kb.starts_with(b"CM_REEXEC_")
            || kb.starts_with(b"CM_HOLDER_UPGRADE_")
        {
            continue;
        }
        let mut kv = kb.to_vec();
        kv.push(b'=');
        kv.extend_from_slice(v.as_encoded_bytes());
        if let Ok(c) = CString::new(kv) {
            envp_c.push(c);
        }
    }
    for (k, v) in extra_env {
        if let Ok(c) = CString::new(format!("{k}={v}")) {
            envp_c.push(c);
        }
    }
    let mut argv_ptrs: Vec<*mut libc::c_char> = argv_c
        .iter()
        .map(|c| c.as_ptr() as *mut libc::c_char)
        .collect();
    argv_ptrs.push(std::ptr::null_mut());
    let mut envp_ptrs: Vec<*mut libc::c_char> = envp_c
        .iter()
        .map(|c| c.as_ptr() as *mut libc::c_char)
        .collect();
    envp_ptrs.push(std::ptr::null_mut());
    let empty = CString::new("").expect("no NUL");
    // SAFETY: target fd is open; argv/envp are NULL-terminated arrays
    // of pointers into CStrings that outlive the call.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_execveat,
            target_fd,
            empty.as_ptr(),
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    format!(
        "execveat(fd {target_fd}, AT_EMPTY_PATH) returned {ret}: {}",
        io::Error::last_os_error()
    )
}

/// The bound path of a unix listener, via getsockname — the v4
/// manifest carries the fd, not the path; custody meta needs the
/// path for shutdown-unlink.
fn unix_listener_path(fd: RawFd) -> Option<String> {
    // SAFETY: zeroed sockaddr_un is a valid out-buffer.
    unsafe {
        let mut addr: libc::sockaddr_un = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
        if libc::getsockname(
            fd,
            &mut addr as *mut _ as *mut libc::sockaddr,
            &mut len,
        ) != 0
        {
            return None;
        }
        if addr.sun_family != libc::AF_UNIX as libc::sa_family_t {
            return None;
        }
        let path_bytes: Vec<u8> = addr
            .sun_path
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        if path_bytes.is_empty() {
            return None; // unnamed or abstract
        }
        String::from_utf8(path_bytes).ok()
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
fn spawn_brain(pin: &OwnedFd) -> std::io::Result<(BrainHandle, OwnedFd)> {
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
    let handle = BrainHandle::from_pid(brain.id() as libc::pid_t)?;
    // std Child's Drop neither kills nor waits; the pidfd owns the
    // supervision identity from here.
    drop(brain);
    // Our copy of the brain's end closes now — channel EOF then
    // reliably means the brain (and its dups) is gone.
    drop(theirs);
    Ok((handle, ours))
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
fn shutdown_sequence(holder: &mut Holder, brain: Option<&BrainHandle>) -> ! {
    eprintln!("cm-holder: shutdown requested — stopping everything");
    if let Some(brain) = brain {
        let _ = reap::pidfd_send_signal(&brain.pidfd, libc::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if reap::pidfd_exit_ready(&brain.pidfd) {
                brain.reap();
                break;
            }
            if Instant::now() >= deadline {
                brain.sigkill_and_reap();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let killed = holder.shutdown_kill_all();
    if let Some(path) = holder.custodied_unix_path() {
        if !path.is_empty() {
            let _ = std::fs::remove_file(&path);
        }
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
