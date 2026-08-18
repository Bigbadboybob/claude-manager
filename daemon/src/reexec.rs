//! Re-exec handoff skeleton — the exec half of the in-place restart.
//! DESIGN_SEAMLESS_RESTART phase 3b (restart-sequence steps 1, 3–5
//! plus the minimal rehydrate of step 6; review findings R7, R9,
//! R13), gated behind the `CM_REEXEC=1` dev flag.
//!
//! ## What this is (and is not)
//!
//! This is the FIRST slice where exec code exists: it wires the
//! phase-2/3a primitives together — quiesce barrier
//! (`crate::restart_coordinator`), reap/reader gate freezes
//! (`crate::reap_gate` / `crate::reader_gate`), sealed FD manifest
//! (`crate::reexec_manifest`), non-killing adoption
//! (`crate::adopt`) — around a real `execveat`, and proves PTY
//! continuity through a real daemon re-exec with one live bash
//! session and a live reader draining
//! (`daemon/tests/reexec_skeleton_e2e.rs`, the condition the phase-1
//! OS proof deliberately skipped).
//!
//! Deliberately OUT of scope, deferred to later phases:
//!
//! - **The rollback exec.** The pinned `/proc/self/exe` fd rides in
//!   the manifest (R7) and the attempt counter is written as 0 and
//!   read back, but a rehydrate failure in the new image restores
//!   state and logs — it never execs the rollback fd. Phase 4.
//! - **`--verify-handoff` preflight subprocess** (phase 4).
//! - **Watcher checkpoints / memory-cap re-adoption** (R12, phase 4)
//!   — `watcher_checkpoint` is written as `None`.
//! - **Workflow / continuous / TUI-reattach anything** beyond what
//!   normal startup already rebuilds from disk (phases 4–5).
//! - **The public `daemon.restart` RPC** (phase 6). The skeleton's
//!   trigger is the dev-gated `daemon.reexec_dev`, dispatched only
//!   when the daemon started with `CM_REEXEC=1`.
//! - **The full close-every-unlisted-inherited-fd audit** on the
//!   rehydrate side (phase 4); the skeleton closes the manifest fd
//!   and the rollback fd and restores CLOEXEC on what it adopts.
//! - **TLS listeners.** The skeleton writes `tls_listener_fd: None`;
//!   don't point the dev flag at a `[tls]`-configured daemon.
//!
//! ## The abort invariant
//!
//! [`perform_reexec`] returns ONLY on failure — success is the exec.
//! Every failure path restores the daemon to a state
//! indistinguishable from before the call: the quiesce guard's drop
//! un-drains and clears `restarting`, the gate freezes drop, the fd
//! table's CLOEXEC flags are restored from a pre-audit snapshot, and
//! the signal mask is restored from the pre-block set. The pinned
//! target/rollback/manifest fds are locals whose drop closes them.
//!
//! ## Lock/gate ordering (why this can't deadlock)
//!
//! [`perform_reexec`] must be called holding NO state lock. The
//! sequence is: quiesce barrier (waits on in-flight mutating RPCs,
//! which need the state lock to finish) → reap-gate freeze →
//! reader-gate freeze → brief state-lock holds for the manifest
//! build. Freezes are taken lock-free per the reap-gate module's
//! rule (a reaper holds its read permit ACROSS `on_exit`, which
//! takes the state lock — a freezer holding that lock would deadlock
//! against the very permit it waits out). The reap-before-reader
//! freeze order is arbitrary and safe: the two gates' holder sets
//! are disjoint (reapers take only the reap permit, readers only the
//! reader permit, and no holder of either ever acquires the other
//! gate), so there is no thread against which the two
//! write-acquisitions could invert — each is bounded by a single
//! in-flight consume/push unit. Taking the state lock AFTER the
//! freezes is consistent with the only other gate+state path in the
//! process (reaper: permit → state lock), so gate → state is a
//! process-wide order.

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::reexec_manifest::{
    self, ReexecManifest, SessionRecord, MANIFEST_SCHEMA_VERSION,
};
use crate::session::{AdoptedSessionMeta, DaemonSession};
use crate::state::DaemonState;
use crate::{reader_gate, reap_gate, restart_coordinator};

/// Bound on the quiesce barrier (design: a barrier that can't be
/// reached must abort the restart, not hang the daemon). 10s is
/// generous for "every in-flight mutating RPC returns" — spawns are
/// the slowest at ~1s worst case.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(10);

// ============================================================
// The exec side (old image)
// ============================================================

/// Execute the re-exec handoff: quiesce, freeze the gates, seal the
/// FD manifest, clear CLOEXEC on exactly the handed-off fds, block
/// SIGHUP/SIGTERM, and `execveat` the pinned target binary in place.
///
/// **Returns ONLY on failure** — on success the calling thread (and
/// every other thread in the process) is replaced by the new image
/// at the same PID. On any failure the daemon has been restored to
/// its pre-call state (see the module docs' abort invariant) and the
/// error says which step refused.
///
/// Must be called from a context holding NO state lock and NO gate
/// permit (see the module docs' ordering argument). The dispatch
/// entry (`daemon.reexec_dev`) is on `RESTART_BARRIER_EXEMPT_METHODS`
/// for the same reason: it RUNS the coordinator, so counting it as a
/// mutation would deadlock the barrier against its own caller.
pub fn perform_reexec(
    state: &Arc<Mutex<DaemonState>>,
    target: &Path,
) -> anyhow::Error {
    // ---- Step (a): pin the executables (R7). ----
    // The target fd IS the binary that will be exec'd — everything
    // downstream goes through the fd, never the pathname, so the
    // checked artifact is the executed artifact even if a deploy
    // overwrites the path mid-sequence. The rollback pin is
    // /proc/self/exe (still the ORIGINAL inode even after a deploy
    // overwrote the path); it rides in the manifest for the phase-4
    // rollback exec and is only shape-validated in this slice.
    let target_fd = match open_pinned_executable(target) {
        Ok(fd) => fd,
        Err(e) => return e,
    };
    let rollback_fd: OwnedFd = match File::open("/proc/self/exe") {
        Ok(f) => f.into(),
        Err(e) => {
            return anyhow::anyhow!(
                "open /proc/self/exe read-only (rollback pin, R7): {}",
                e
            )
        }
    };

    // ---- Step (b): quiesce (prepare/commit/abort barrier). ----
    let guard = match restart_coordinator::begin(state) {
        Ok(g) => g,
        Err(busy) => return anyhow::anyhow!("{}", busy),
    };
    if let Err(timeout) = guard.wait_quiesced(QUIESCE_TIMEOUT) {
        let err = anyhow::anyhow!("{}", timeout);
        // Guard drop (abort) un-drains and clears `restarting`;
        // nothing else has been touched yet.
        guard.abort();
        return err;
    }

    // ---- Step (c): gate freezes, on this thread, lock-free. ----
    // Taken AFTER wait_quiesced succeeded; both are bounded in
    // practice by a single in-flight consume/push unit. Order and
    // deadlock-freedom argued in the module docs.
    let reap_freeze = reap_gate::freeze();
    let reader_freeze = reader_gate::freeze();

    // Steps (d)–(g) live in `exec_stage`, which restores every
    // mutation IT made (fd flags, signal mask) before returning its
    // error. The freezes and the quiesce guard are released here, in
    // reverse order of acquisition, so the daemon is indistinguishable
    // from before the call.
    let err = exec_stage(state, target, &target_fd, &rollback_fd);
    drop(reader_freeze);
    drop(reap_freeze);
    guard.abort();
    err
}

/// Steps (d)–(g): manifest build/seal, CLOEXEC discipline, signal
/// bracketing, the exec itself. Runs entirely under the caller's
/// gate freezes + quiesce guard; returns ONLY on failure, having
/// restored the fd-table flags and signal mask it changed.
fn exec_stage(
    state: &Arc<Mutex<DaemonState>>,
    target: &Path,
    target_fd: &OwnedFd,
    rollback_fd: &OwnedFd,
) -> anyhow::Error {
    // ---- Step (d): build + seal the FD manifest. ----
    let manifest = match build_manifest(state, rollback_fd) {
        Ok(m) => m,
        Err(e) => return e,
    };
    let manifest_fd = match reexec_manifest::write_manifest(&manifest) {
        Ok(fd) => fd,
        Err(e) => {
            return anyhow::anyhow!("write sealed manifest memfd: {}", e)
        }
    };

    // ---- Step (e): CLOEXEC discipline (R9). ----
    // With mutations quiesced (nothing can fork) and the gates
    // frozen: snapshot the whole fd table's flags, audit EVERYTHING
    // ≥3 to CLOEXEC, then clear the flag on exactly the fds that
    // must cross the exec. The snapshot is what makes the abort path
    // able to restore both directions (flags we set AND flags we
    // cleared). Known skeleton gap: the workflow poller / continuous
    // scheduler are not yet paused at tick boundaries (design step
    // 3's last bullet, phase 4), so a transient fd they open during
    // this window could be missed by the snapshot — worst case a
    // transient fd's CLOEXEC bit flips, never a leak into a child
    // (nothing can spawn while quiesced).
    let flags_snapshot = match snapshot_fd_flags() {
        Ok(s) => s,
        Err(e) => return anyhow::anyhow!("snapshot /proc/self/fd flags: {}", e),
    };
    if let Err(e) = set_all_cloexec() {
        restore_fd_flags(&flags_snapshot);
        return anyhow::anyhow!("CLOEXEC audit (close_range / fd walk): {}", e);
    }
    // The inheritance allowlist: every manifest-listed fd (sessions'
    // masters + pidfds, listener, rollback pin), the manifest fd
    // itself (the bootstrap pointer's target), and the target-binary
    // fd the execveat consumes.
    let mut inherit: Vec<RawFd> = vec![
        manifest_fd.as_raw_fd(),
        target_fd.as_raw_fd(),
        manifest.rollback_bin_fd,
        manifest.listener_fd,
    ];
    if let Some(fd) = manifest.tls_listener_fd {
        inherit.push(fd);
    }
    for s in &manifest.sessions {
        inherit.push(s.pty_master_fd);
        inherit.push(s.pidfd);
    }
    for &fd in &inherit {
        if let Err(e) = set_fd_flags(fd, 0) {
            restore_fd_flags(&flags_snapshot);
            return anyhow::anyhow!(
                "clear CLOEXEC on handed-off fd {}: {}",
                fd,
                e
            );
        }
    }

    // ---- Step (f): signal bracketing + exec (R13/F8). ----
    // The signal MASK survives exec while caught dispositions reset
    // to default — and SIGHUP's default is terminate, so an
    // operator's config-reload reflex (`kill -HUP`) landing between
    // the exec and the new image's handler install would kill the
    // daemon mid-rehydrate. Block both; the new image unblocks after
    // installing its handler (see `run()`).
    let old_mask = match block_sighup_sigterm() {
        Ok(m) => m,
        Err(e) => {
            restore_fd_flags(&flags_snapshot);
            return e;
        }
    };

    // Unbuffered logging at the exec point (design requirement: the
    // phase-1 proof demonstrated stdio USERSPACE buffers dying at
    // exec — stderr is unbuffered, so these lines survive).
    eprintln!(
        "cm-daemon: re-exec — exec'ing pinned target fd {} ({}) with {} \
         session(s), listener fd {}, manifest fd {}, attempt 0",
        target_fd.as_raw_fd(),
        target.display(),
        manifest.sessions.len(),
        manifest.listener_fd,
        manifest_fd.as_raw_fd(),
    );

    let err = do_execveat(target_fd, target, manifest_fd.as_raw_fd());

    // ---- Step (g): execveat returned — failure. Restore. ----
    eprintln!(
        "cm-daemon: re-exec FAILED at execveat ({}); restoring pre-call \
         state (CLOEXEC flags, signal mask, gates, drain)",
        err
    );
    restore_sigmask(&old_mask);
    restore_fd_flags(&flags_snapshot);
    // manifest_fd / target_fd / rollback_fd close via drop in the
    // callers' scopes.
    err
}

/// Step (a) helper: open `target` read-only and shape-check THE FD
/// (never the pathname — R7): regular file with an execute bit. A
/// mode check can't prove `execveat` will succeed (noexec mounts,
/// wrong arch), but it fails the obvious wrong-path cases before any
/// quiesce work happens. Mirrors
/// `reexec_manifest::validate_fd_roles`'s rollback-fd probe.
fn open_pinned_executable(target: &Path) -> Result<OwnedFd, anyhow::Error> {
    let f = File::open(target).map_err(|e| {
        anyhow::anyhow!(
            "open target binary {} read-only: {}",
            target.display(),
            e
        )
    })?;
    let fd: OwnedFd = f.into();
    // SAFETY: zeroed stat is a valid out-buffer; the fd is open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::fstat(fd.as_raw_fd(), &mut st) };
    if ret != 0 {
        return Err(anyhow::anyhow!(
            "fstat(target fd): {}",
            io::Error::last_os_error()
        ));
    }
    if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Err(anyhow::anyhow!(
            "target {} is not a regular file (st_mode {:#o})",
            target.display(),
            st.st_mode
        ));
    }
    if st.st_mode & 0o111 == 0 {
        return Err(anyhow::anyhow!(
            "target {} has no execute permission (st_mode {:#o})",
            target.display(),
            st.st_mode
        ));
    }
    Ok(fd)
}

/// Step (d) helper: project live daemon state into the manifest.
/// Holds the state lock briefly (cheap /proc reads only). Runs under
/// the gate freezes, so the session set cannot change underneath:
/// no reaper can remove an entry (consumption frozen) and no
/// mutating RPC can add one (quiesced + draining).
fn build_manifest(
    state: &Arc<Mutex<DaemonState>>,
    rollback_fd: &OwnedFd,
) -> Result<ReexecManifest, anyhow::Error> {
    let st = state.lock().unwrap_or_else(|p| p.into_inner());
    let listener_fd = st.listener_raw_fd.ok_or_else(|| {
        anyhow::anyhow!(
            "DaemonState carries no listener_raw_fd — this daemon was not \
             started through run(), so there is no bound listener to hand off"
        )
    })?;
    let mut sessions: Vec<SessionRecord> = Vec::with_capacity(st.sessions.len());
    for (uid, s) in &st.sessions {
        let (pty_master_fd, pidfd) = s.reexec_handoff_fds().ok_or_else(|| {
            anyhow::anyhow!("session '{}' exposes no raw PTY master fd", uid)
        })?;
        // R6 cross-check value. Read NOW from /proc (starttime is
        // constant for the process's lifetime, and the child is
        // alive-or-zombie under the frozen reap gate, so the stat
        // line exists) with the same parser the rehydrate-side probe
        // uses — see `crate::adopt::proc_starttime`.
        let child_start_time =
            crate::adopt::proc_starttime(s.pid).map_err(|e| {
                anyhow::anyhow!(
                    "read /proc/{}/stat starttime for session '{}': {}",
                    s.pid,
                    uid,
                    e
                )
            })?;
        sessions.push(SessionRecord {
            uid: uid.clone(),
            generation: s.generation,
            transcript_id: s
                .transcript_path
                .as_deref()
                .and_then(crate::session::transcript_id_from_path),
            child_pid: s.pid,
            child_start_time,
            pty_master_fd,
            pidfd,
            cgroup_prefix: s
                .cgroup_prefix
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            // R12 watcher checkpoints are phase-4 scope; the manifest
            // carries the slot opaquely so the framing doesn't churn.
            watcher_checkpoint: None,
        });
    }
    Ok(ReexecManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        // Written as 0 and read back by the new image; the rollback
        // attempt state machine that ACTS on it is phase-4 scope.
        attempt: 0,
        rollback_bin_fd: rollback_fd.as_raw_fd(),
        sessions,
        listener_fd,
        // Skeleton: TLS handoff is out of scope (module docs).
        tls_listener_fd: None,
    })
}

/// Step (f) helper: `execveat(target_fd, "", argv, envp,
/// AT_EMPTY_PATH)`. Returns only on failure.
///
/// argv\[0\] is updated to the target path (ps hygiene, per the
/// design's step 5); the rest is preserved from our own args. envp
/// is built EXPLICITLY — never `setenv` on the shared environ (R9:
/// other threads are alive and environ mutation is process-global) —
/// as the current env minus every `CM_REEXEC_*` var, plus the one
/// bootstrap pointer `CM_REEXEC_MANIFEST_FD=<fd>`. Note `CM_REEXEC`
/// itself (the dev flag, no trailing underscore) deliberately
/// survives: the new image needs it to keep serving the dev method.
fn do_execveat(
    target_fd: &OwnedFd,
    target: &Path,
    manifest_fd_num: RawFd,
) -> anyhow::Error {
    let mut argv_c: Vec<CString> = Vec::new();
    match CString::new(target.as_os_str().as_bytes()) {
        Ok(c) => argv_c.push(c),
        Err(_) => return anyhow::anyhow!("target path contains a NUL byte"),
    }
    for arg in std::env::args_os().skip(1) {
        match CString::new(arg.into_vec()) {
            Ok(c) => argv_c.push(c),
            Err(_) => {
                return anyhow::anyhow!("own argv element contains a NUL byte")
            }
        }
    }

    let mut envp_c: Vec<CString> = Vec::new();
    for (k, v) in std::env::vars_os() {
        if k.as_bytes().starts_with(b"CM_REEXEC_") {
            continue;
        }
        let mut kv = k.into_vec();
        kv.push(b'=');
        kv.extend_from_slice(v.as_bytes());
        match CString::new(kv) {
            Ok(c) => envp_c.push(c),
            // A NUL inside an env entry is unrepresentable in envp;
            // dropping the entry is the only honest option.
            Err(_) => continue,
        }
    }
    envp_c.push(
        CString::new(format!(
            "{}={}",
            reexec_manifest::ENV_MANIFEST_FD,
            manifest_fd_num
        ))
        .expect("fd number formatting contains no NUL"),
    );

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

    let empty = CString::new("").expect("empty string has no NUL");
    // SAFETY: target_fd is open; pathname is a valid empty C string
    // (AT_EMPTY_PATH execs the fd itself); argv/envp are
    // NULL-terminated arrays of pointers into CStrings that outlive
    // the call (on success nothing outlives anything — the image is
    // replaced).
    let ret = unsafe {
        libc::execveat(
            target_fd.as_raw_fd(),
            empty.as_ptr(),
            argv_ptrs.as_ptr(),
            envp_ptrs.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    // execveat returns only on failure (ret is -1 with errno set).
    anyhow::anyhow!(
        "execveat(fd {}, AT_EMPTY_PATH) returned {}: {}",
        target_fd.as_raw_fd(),
        ret,
        io::Error::last_os_error()
    )
}

// ============================================================
// CLOEXEC discipline helpers (R9)
// ============================================================

/// Snapshot every fd ≥ 3 and its `F_GETFD` flags from
/// `/proc/self/fd`. The walk's own readdir fd shows up in the
/// listing and closes right after — the restore skips vanished fds
/// (EBADF), so that's harmless.
fn snapshot_fd_flags() -> io::Result<Vec<(RawFd, libc::c_int)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd")? {
        let entry = entry?;
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<RawFd>().ok())
        else {
            continue;
        };
        if fd < 3 {
            continue;
        }
        // SAFETY: plain fcntl query; a concurrently-closed fd fails
        // with EBADF, which we skip.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            out.push((fd, flags));
        }
    }
    Ok(out)
}

/// Audit the whole fd table (sparing stdio 0–2) to CLOEXEC via
/// `close_range(3, ~0, CLOSE_RANGE_CLOEXEC)`, falling back to a
/// `/proc/self/fd` walk with per-fd `F_SETFD` when the syscall or
/// the flag is unavailable (pre-5.11 kernels).
fn set_all_cloexec() -> io::Result<()> {
    // SAFETY: close_range with CLOSE_RANGE_CLOEXEC closes nothing —
    // it only sets the flag across the range.
    let ret = unsafe {
        libc::close_range(
            3,
            libc::c_uint::MAX,
            libc::CLOSE_RANGE_CLOEXEC as libc::c_int,
        )
    };
    if ret == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) => {
            // Fallback: per-fd walk.
            for (fd, flags) in snapshot_fd_flags()? {
                let _ = set_fd_flags(fd, flags | libc::FD_CLOEXEC);
            }
            Ok(())
        }
        _ => Err(err),
    }
}

/// `fcntl(F_SETFD, flags)` on one fd.
fn set_fd_flags(fd: RawFd, flags: libc::c_int) -> io::Result<()> {
    // SAFETY: plain fcntl with an int argument.
    let ret = unsafe { libc::fcntl(fd, libc::F_SETFD, flags) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Abort-path restore: put every snapshotted fd's flags back exactly.
/// Best-effort per fd — an fd that vanished since the snapshot
/// (EBADF) is skipped; there is nothing to restore on it.
fn restore_fd_flags(snapshot: &[(RawFd, libc::c_int)]) {
    for &(fd, flags) in snapshot {
        let _ = set_fd_flags(fd, flags);
    }
}

// ============================================================
// Signal bracketing helpers (R13/F8)
// ============================================================

/// Block SIGHUP and SIGTERM on the calling thread (the exec carrier —
/// after exec the mask is the new image's initial-thread mask) and
/// return the previous mask for the abort path.
fn block_sighup_sigterm() -> Result<libc::sigset_t, anyhow::Error> {
    // SAFETY: sigemptyset/sigaddset fill a sigset we own;
    // pthread_sigmask reads it and writes the old mask into memory
    // we own.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut set) != 0 {
            return Err(anyhow::anyhow!(
                "sigemptyset: {}",
                io::Error::last_os_error()
            ));
        }
        libc::sigaddset(&mut set, libc::SIGHUP);
        libc::sigaddset(&mut set, libc::SIGTERM);
        let mut old: libc::sigset_t = std::mem::zeroed();
        let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
        if rc != 0 {
            return Err(anyhow::anyhow!(
                "pthread_sigmask(SIG_BLOCK, {{SIGHUP, SIGTERM}}): {}",
                io::Error::from_raw_os_error(rc)
            ));
        }
        Ok(old)
    }
}

/// Abort-path restore of the pre-block signal mask.
fn restore_sigmask(old: &libc::sigset_t) {
    // SAFETY: `old` came from pthread_sigmask's out-parameter.
    unsafe {
        let _ = libc::pthread_sigmask(libc::SIG_SETMASK, old, std::ptr::null_mut());
    }
}

// ============================================================
// The rehydrate side (new image) — skeleton
// ============================================================

/// Adopt the manifest's sessions into the registry (the minimal
/// rehydrate of the design's step 6). Called by `run()` on the
/// handoff path, INSTEAD of `restore_sessions` (which would spawn
/// `--resume` duplicates of children that are still alive — R13),
/// after the manifest passed `read_manifest` + `validate_fd_roles`.
///
/// Per record: take ownership of the two inherited fds (restoring
/// CLOEXEC on them immediately — R9 hygiene, so a later ordinary
/// child spawn can't inherit another session's master), build a
/// non-killing [`crate::adopt::SessionCandidate`], probe
/// `child_alive` + the R6 start-time cross-check against the record,
/// and on ANY mismatch/death/error skip the record — tombstone-free
/// for the skeleton: just don't adopt it, and NEVER signal it (the
/// candidate's drop closes only its own fds). Survivors promote and
/// arm through [`DaemonSession::adopt`] (kill-on-drop begins there —
/// that is the commit gate) with the same registry-cleanup `on_exit`
/// the spawn path installs, under the state lock across arm+insert
/// so a fast exit can't race the insert (the `start_session`
/// discipline).
///
/// Returns the number of sessions adopted.
pub fn rehydrate_adopted_sessions(
    state_arc: &Arc<Mutex<DaemonState>>,
    manifest: &ReexecManifest,
) -> usize {
    let mut adopted = 0usize;
    for rec in &manifest.sessions {
        // Ownership transfer: the handoff path owns the inherited
        // fds from the moment the manifest validated.
        // SAFETY: fd numbers come from a sealed, role-validated
        // manifest written by the previous image; each appears in
        // exactly one slot (duplicate-fd validation), so single
        // ownership holds.
        let pidfd = unsafe { OwnedFd::from_raw_fd(rec.pidfd) };
        let master = unsafe { OwnedFd::from_raw_fd(rec.pty_master_fd) };
        // R9: these crossed the exec CLOEXEC-cleared by necessity;
        // re-set the flag now so nothing spawned later inherits them.
        let _ = set_fd_flags(pidfd.as_raw_fd(), libc::FD_CLOEXEC);
        let _ = set_fd_flags(master.as_raw_fd(), libc::FD_CLOEXEC);

        let candidate = crate::adopt::SessionCandidate::from_raw_parts(
            rec.uid.clone(),
            rec.child_pid,
            pidfd,
            master,
        );

        // Probe 1: liveness (non-consuming pidfd poll — an exited
        // child stays an unreaped zombie; skeleton skips it without
        // reaping or tombstoning).
        match candidate.child_alive() {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): child exited \
                     during the swap — skipping adoption (skeleton: no \
                     tombstone, status left unreaped for later phases)",
                    rec.uid, rec.child_pid
                );
                continue;
            }
            Err(e) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): pidfd probe \
                     failed ({}) — skipping adoption, never signaling",
                    rec.uid, rec.child_pid, e
                );
                continue;
            }
        }
        // Probe 2: R6 identity cross-check — the recorded spawn-time
        // starttime must match the live read. The pidfd already makes
        // signaling pid-reuse-proof; this catches a corrupt/mismatched
        // record before we build anything around it.
        match candidate.child_start_time() {
            Ok(t) if t == rec.child_start_time => {}
            Ok(t) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): start-time \
                     mismatch (manifest {}, /proc {}) — skipping adoption, \
                     never signaling",
                    rec.uid, rec.child_pid, rec.child_start_time, t
                );
                continue;
            }
            Err(e) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): start-time \
                     read failed ({}) — skipping adoption, never signaling",
                    rec.uid, rec.child_pid, e
                );
                continue;
            }
        }

        if let Some(tid) = rec.transcript_id.as_deref() {
            eprintln!(
                "cm-daemon: handoff session '{}': transcript_id {} noted but \
                 not rebound (skeleton carries the id, not the path; phase 4)",
                rec.uid, tid
            );
        }

        // Commit: promote + arm + insert under one state-lock hold —
        // same fast-exit race discipline as `start_session`'s
        // arm_reaper-and-insert (the reaper's on_exit blocks on the
        // lock we hold until the insert is visible).
        let parts = candidate.promote();
        let meta = AdoptedSessionMeta {
            title: rec.uid.clone(),
            // Skeleton manifest schema carries no engine field —
            // hard-noted bash (the only engine the skeleton
            // exercises); phase 4 adds the field.
            session_type: "bash".to_string(),
            generation: rec.generation,
            cgroup_prefix: rec.cgroup_prefix.clone().map(PathBuf::from),
        };
        let state_for_cleanup = Arc::clone(state_arc);
        let uid_for_cleanup = rec.uid.clone();
        let on_exit: crate::session::OnExitCallback = Box::new(move |_status| {
            let mut s = state_for_cleanup
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            crate::control::methods::handle_session_exit(&mut s, &uid_for_cleanup);
        });

        let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());
        if st.sessions.contains_key(&rec.uid) {
            eprintln!(
                "cm-daemon: handoff session '{}': uid already in the registry \
                 — skipping duplicate adoption",
                rec.uid
            );
            continue;
        }
        match DaemonSession::adopt(parts, meta, Some(on_exit)) {
            Ok(sess) => {
                st.sessions.insert(rec.uid.clone(), sess);
                adopted += 1;
                eprintln!(
                    "cm-daemon: handoff session '{}' adopted (pid {}, \
                     generation {})",
                    rec.uid, rec.child_pid, rec.generation
                );
            }
            Err(e) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' FAILED to adopt: {} \
                     (session lost — its master fd closed with the parts)",
                    rec.uid, e
                );
            }
        }
    }
    adopted
}
