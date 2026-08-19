//! Re-exec handoff — the exec half of the in-place restart.
//! DESIGN_SEAMLESS_RESTART phases 3b + 4a (restart-sequence steps 1,
//! 3–6 and the "Failure classes, exhaustively" ladder; review
//! findings R5, R6, R7, R9, R13), gated behind the `CM_REEXEC=1` dev
//! flag.
//!
//! ## What this is (and is not)
//!
//! Phase 3b wired the phase-2/3a primitives together — quiesce
//! barrier (`crate::restart_coordinator`), reap/reader gate freezes
//! (`crate::reap_gate` / `crate::reader_gate`; phase 4h added the
//! writer-gate pause + freeze, `crate::writer_gate` — R10, audit
//! gap 6), sealed FD manifest
//! (`crate::reexec_manifest`), non-killing adoption
//! (`crate::adopt`) — around a real `execveat`, and proved PTY
//! continuity through a real daemon re-exec with one live bash
//! session and a live reader draining
//! (`daemon/tests/reexec_skeleton_e2e.rs`, the condition the phase-1
//! OS proof deliberately skipped).
//!
//! Phase 4a makes the failure ladder real. The new-image side is now
//! a **transaction against escrowed FDs** (R5's full form):
//!
//! - [`detect_handoff`] validates the inherited manifest and builds a
//!   [`HandoffEscrow`] that owns the ORIGINAL inherited fds untouched
//!   (CLOEXEC re-set immediately, so nothing spawned during startup —
//!   the MCP preflight subprocess included — can inherit them).
//! - [`complete_handoff`] runs the attempt state machine
//!   ([`attempt_decision`] / [`on_rehydrate_failure`]) around the
//!   rehydrate transaction: ALL records are probed on CLOEXEC
//!   **dups** first (liveness + the R6 start-time cross-check, no
//!   promotion, nothing signaled); only after every survivor
//!   validates does the commit phase arm real sessions. A failure —
//!   `Result` error or caught panic (the whole transaction runs under
//!   `catch_unwind`, the design's "top-level recovery guard that
//!   never exits while escrow FDs are live") — leaves the escrow
//!   intact and execs the **pinned rollback binary** with attempt+1
//!   ([`rollback_exec`]); attempt ≥ 2, or a rollback exec that itself
//!   returns, lands in the **terminal fallback**
//!   ([`terminal_fallback`]): deliberately SIGKILL + reap every
//!   manifest-listed child through its escrowed pidfd, then return so
//!   `run()` proceeds with legacy `restore_sessions`. Never fall
//!   through with live orphans — live children plus `--resume`
//!   duplicates is two writers per conversation, the 2026-08-18
//!   split-brain class the review found in the original spec.
//!
//! Failure injection for the e2e: [`ENV_TEST_FAIL_REHYDRATE`] — see
//! its doc for the loud dev-flag gate.
//!
//! Phase 4c (R15) added the two guards around the point of no return:
//!
//! - **The `--verify-handoff` preflight** ([`run_verify_handoff`] on
//!   the candidate side, [`preflight_verify_handoff`] on the old-image
//!   side): before the quiesce, the PINNED target binary runs as a
//!   verify-only subprocess against a DRY manifest and the current
//!   on-disk state, so a broken binary, state-schema drift, or a dead
//!   MCP interpreter refuses the restart while the old image is still
//!   fully in charge — today those were caught only at exec time
//!   (ENOEXEC) or after the exec (rehydrate failure → rollback).
//! - **The checked persistence pass + post-freeze revalidation**
//!   ([`persist_all_checked`] / [`post_freeze_revalidation`]): after
//!   quiescence, one CHECKED (error-returning, fsynced — file AND
//!   containing directory) persistence pass over sessions (tombstones
//!   included — pre-4c they were absent from the persist, the P4b
//!   known gap), workflow-runs state, and continuous state, then a
//!   cheap in-process re-assertion that what the dry preflight
//!   validated still holds against the frozen snapshot. The dry
//!   manifest is discarded; the REAL manifest is built fresh after
//!   quiescence — that ordering is the R15 point (the dry run races
//!   live mutations; the handed-off snapshot is the frozen one).
//!
//! Phase 4d (R12, audit gap 1) landed the watcher half: memory-cap
//! watchers register `memcap-watcher:<uid>` safe points with the
//! restart coordinator (parking only at no-signal loop tops — never
//! between their SIGTERM and delayed SIGKILL), [`build_manifest`]
//! snapshots each capped session's parked watcher into the manifest's
//! opaque `watcher_checkpoint` slot
//! (`session_watch::WatcherCheckpoint`: protected set, `last_high`
//! watermark, watched cgroup path, spawn-time kill-log baseline —
//! own version gate, no manifest schema bump), and the rehydrate
//! commit re-arms watchers seeded FROM the checkpoint
//! ([`respawn_adopted_watcher`]) with the kill-log baseline restored
//! into the adopted `LastExitProbe`. A missing/unparseable checkpoint
//! degrades LOUDLY to a fresh watcher and never fails the adoption.
//!
//! Deliberately OUT of scope, deferred to later phase-4 slices:
//! - **Workflow / continuous / TUI-reattach anything** beyond what
//!   normal startup already rebuilds from disk (phases 4–5).
//! - ~~The public `daemon.restart` RPC~~ — LANDED (phase 6):
//!   `daemon.restart` is the always-dispatched production twin of the
//!   dev method (`control/dispatch.rs::dispatch_daemon_restart`) —
//!   strong-operator gated, pinned-artifact target policy
//!   ([`resolve_restart_target`]: default = the daemon's own
//!   executable path re-resolved at RPC time, which the deploy flow
//!   overwrites with the new binary; the same-inode case is a
//!   WARNING, never a refusal), **no `skip_preflight`** (the P4c
//!   deviation note — the preflight is load-bearing in production;
//!   only the dev method keeps the escape hatch), and the manifest's
//!   schema-v3 `reexec_generation` + `build_id` on `daemon.health`
//!   close the fire-and-verify loop for cm-redeploy. The dev-gated
//!   `daemon.reexec_dev` remains, unchanged, for e2e work.
//! - **The full close-every-unlisted-inherited-fd audit** on the
//!   rehydrate side; escrow closes exactly what it owns.
//! - **TLS listeners.** The exec side writes `tls_listener_fd: None`.
//!   Since phase 4g (audit gap 8) [`perform_reexec`] REFUSES on a
//!   `[tls]`-configured daemon — before the preflight, nothing
//!   happened — instead of relying on this doc's old "don't point the
//!   dev flag at a TLS daemon" advisory (a swap there would drop every
//!   TLS client, and a fresh TLS re-bind failure in `run()` exits
//!   BEFORE `complete_handoff`, the crash-class outcome with escrow
//!   unresolved).
//!
//! Phase 4b (R11) closed three 4a gaps in this module: the manifest
//! carries the FULL session record (schema v2 — identity, bindings,
//! perms, transcript path, cap bytes, status cells as ages, the
//! done_report marker), the transaction builds every session BEFORE
//! arming any thread (the byte-loss corner: a mid-commit failure
//! after earlier records' readers had drained kernel bytes into
//! fanout rings that die at the rollback exec), and a record whose
//! child exited during the swap is now reaped (`waitid(P_PIDFD)`
//! under a reap-gate read permit) and tombstoned honestly instead of
//! left an unreaped zombie. Start-time-MISMATCH records keep the 4a
//! treatment exactly: unsignaled, unreaped, untombstoned — unknown
//! identity, never fabricated provenance.
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
//! One deliberate non-restoration (4c): a failure AFTER the checked
//! persistence pass leaves the freshly written
//! `daemon-sessions.json` / `daemon-tombstones.json` in place — each
//! is a valid, current snapshot of live state (the same content the
//! next lifecycle persist would write), not a mutation to undo.
//!
//! ## Lock/gate ordering (why this can't deadlock)
//!
//! [`perform_reexec`] must be called holding NO state lock. The
//! sequence is: quiesce barrier (waits on in-flight mutating RPCs,
//! which need the state lock to finish) → writer pause + writer-gate
//! freeze (phase 4h, R10 — bounded, aborts `restart_busy` on
//! timeout) → reap-gate freeze → reader-gate freeze → brief
//! state-lock holds for the manifest build. Freezes are taken
//! lock-free per the reap-gate module's rule (a reaper holds its
//! read permit ACROSS `on_exit`, which takes the state lock — a
//! freezer holding that lock would deadlock against the very permit
//! it waits out). The freeze order among the THREE gates is
//! arbitrary and safe: their holder sets are pairwise disjoint —
//! reapers take only the reap permit, readers only the reader
//! permit, PTY writers only writer permits (`handle_session_exit`,
//! the reaper's `on_exit` body, performs no PTY writes; readers only
//! push to the fanout; delivery threads / attach input / the poller
//! drainer never touch the reap or reader gates) — so there is no
//! thread against which the write-acquisitions could invert. The
//! writer freeze goes FIRST purely for hold time: it is the slow one
//! (up to one in-progress delivery step, ~3s), and taking it before
//! the reader freeze keeps the window in which chatty children can
//! block on undrained PTY output buffers as short as ever. Taking
//! the state lock AFTER the freezes is consistent with the only
//! other gate+state paths in the process (reaper: permit → state
//! lock; writers: handle cloned OUT of state, lock dropped, then
//! permit → write — the `InputHandle` contract), so gate → state is
//! a process-wide order. Writer-permit holders never take the state
//! lock while parked or writing — that established contract is now
//! load-bearing: a writer parked at the gate while holding the state
//! lock would deadlock `exec_stage`'s post-freeze state-lock holds.

use std::collections::HashSet;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::adopt::SessionCandidate;
use crate::reexec_manifest::{
    self, ReexecManifest, SessionRecord, MANIFEST_SCHEMA_VERSION,
};
use crate::session::{
    AdoptedSessionBuild, AdoptedSessionMeta, DaemonSession, ReportedDone,
};
use crate::state::DaemonState;
use crate::{reader_gate, reap_gate, restart_coordinator};

/// Bound on the quiesce barrier (design: a barrier that can't be
/// reached must abort the restart, not hang the daemon). 10s is
/// generous for "every in-flight mutating RPC returns" — spawns are
/// the slowest at ~1s worst case.
const QUIESCE_TIMEOUT: Duration = Duration::from_secs(10);

/// Bound on the writer-gate freeze (phase 4h, R10). Unlike the reap /
/// reader freezes — whose holds are microsecond consume/push units and
/// therefore acquired blocking — a writer unit is seconds long
/// (pause-aware deliveries finish within one in-progress step window,
/// ~3s worst case) and NOT structurally bounded (a child that stopped
/// draining its stdin wedges `write_all` against a full PTY input
/// buffer indefinitely). 10s covers the honest worst case with margin;
/// at the deadline the restart aborts `restart_busy` instead of
/// hanging. Shrunk under `cfg(test)` so the in-binary wiring test
/// (`perform_reexec_aborts_restart_busy_on_wedged_writer_unit`)
/// doesn't stall the suite; the e2e exercises the real binary with the
/// production value.
#[cfg(not(test))]
const WRITER_FREEZE_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const WRITER_FREEZE_TIMEOUT: Duration = Duration::from_millis(400);

/// Bound on the `--verify-handoff` preflight subprocess (phase 4c).
/// It parses small state files and runs the MCP selftest (which
/// shells out a Python import for a few seconds); 60s is generous
/// under suite/machine load. At the deadline the child is killed and
/// the restart is refused — the preflight runs BEFORE the quiesce,
/// so nothing has happened.
const VERIFY_HANDOFF_TIMEOUT: Duration = Duration::from_secs(60);

/// Cap on how much of the verify subprocess's stdout/stderr rides
/// into the inline RPC error (the full streams are still written by
/// the child; this only bounds the copy quoted in the diagnosis).
const VERIFY_OUTPUT_CAP: usize = 2000;

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
/// permit (see the module docs' ordering argument). Both dispatch
/// entries (`daemon.restart` — phase 6 — and the dev-gated
/// `daemon.reexec_dev`) are on `RESTART_BARRIER_EXEMPT_METHODS` for
/// the same reason: they RUN the coordinator, so counting them as a
/// mutation would deadlock the barrier against its own caller.
///
/// `skip_preflight` bypasses the phase-4c `--verify-handoff` step —
/// a dev/test escape hatch (the e2e uses it to reach the deep abort
/// paths, which sit BEHIND a passing preflight and are otherwise
/// unreachable with a broken target). Production callers pass
/// `false`; the production `daemon.restart` RPC does not even expose
/// the parameter (the P4c deviation note) — only `daemon.reexec_dev`
/// can reach `true`.
pub fn perform_reexec(
    state: &Arc<Mutex<DaemonState>>,
    target: &Path,
    skip_preflight: bool,
) -> anyhow::Error {
    // ---- Step (a0): refuse under [tls] (phase 4g, audit gap 8). ----
    // TLS listener handoff is unimplemented: the exec side writes
    // `tls_listener_fd: None`, and `run()` re-binds the TLS listener
    // FRESH at startup with a loud-fatal error path that sits BEFORE
    // `complete_handoff` — so on a `[tls]`-configured daemon a swap
    // drops every TLS client, and a re-bind failure exits the new
    // image with the escrow unresolved: children orphaned by PTY
    // teardown, the crash-class outcome. Refuse up front — before the
    // pin, the preflight, and every point of side effect — while
    // NOTHING has happened. (Pre-4g the module doc's "don't point the
    // dev flag at a TLS daemon" was the only guard.)
    {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tls) = &st.config.tls {
            return anyhow::anyhow!(
                "re-exec REFUSED: this daemon carries a [tls] listener \
                 config (listen_addr {}), and TLS listener handoff is \
                 unimplemented — a re-exec'd daemon would drop every TLS \
                 client and could die at the fresh TLS re-bind BEFORE \
                 complete_handoff resolves the escrow (children orphaned: \
                 the crash-class outcome). Use drain + legacy restart on \
                 TLS-configured daemons until TLS handoff (R13) lands. \
                 Nothing has happened.",
                tls.listen_addr,
            );
        }
    }

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

    // ---- Step (a2): old-image preflight (phase 4c, design step 2). ----
    // Runs BEFORE the quiesce, with NO state lock held across it (it
    // shells out for seconds; the dry-manifest build inside takes the
    // lock briefly). Any failure — nonzero exit, timeout, spawn error
    // — returns the candidate's diagnosis inline and NOTHING has
    // happened: no drain, no freeze, no fd-table or signal-mask
    // mutation.
    let dry_schema_version: Option<u32> = if skip_preflight {
        eprintln!(
            "cm-daemon: re-exec preflight SKIPPED (skip_preflight=true — \
             dev/test escape hatch; the deep abort paths sit behind a \
             passing preflight and are otherwise unreachable with a broken \
             target)"
        );
        None
    } else {
        match preflight_verify_handoff(state, target, &target_fd, &rollback_fd)
        {
            Ok(v) => Some(v),
            Err(e) => return e,
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
    // Taken AFTER wait_quiesced succeeded. Order and deadlock-freedom
    // argued in the module docs; the three gates' holder sets are
    // pairwise disjoint, so the order among them is safety-arbitrary —
    // it is chosen for HOLD TIME: the writer freeze first (the slow
    // one — up to an in-progress delivery step, ~3s worst case, and
    // bounded-with-abort because a wedged `write_all` is unbounded),
    // then the reap and reader freezes (microsecond consume/push
    // units), keeping the reader-freeze hold — which is what lets
    // chatty children block on full PTY output buffers — as short as
    // ever.
    //
    // The writer pause (phase 4h, R10) is requested only NOW, after
    // the mutation counter's zero-crossing: requesting it before
    // `wait_quiesced` would deadlock-shape the barrier (a mutating
    // dispatch parked at the writer door could never return, so the
    // counter could never reach zero and every restart would abort
    // `restart_busy`). From here, new PTY-input units park at the
    // door, in-flight delivery units wrap up promptly at their next
    // step boundary, and the freeze's bounded try_write poll
    // converges. Dropping `writer_pause` on any abort path reopens
    // the door — parked writers proceed as if nothing happened.
    let writer_pause = crate::writer_gate::request_pause();
    let writer_freeze =
        match crate::writer_gate::freeze(&writer_pause, WRITER_FREEZE_TIMEOUT) {
            Ok(f) => f,
            Err(timeout) => {
                let err = anyhow::anyhow!("{}", timeout);
                drop(writer_pause);
                guard.abort();
                return err;
            }
        };
    let reap_freeze = reap_gate::freeze();
    let reader_freeze = reader_gate::freeze();

    // Steps (c2)–(g) live in `exec_stage`, which restores every
    // mutation IT made (fd flags, signal mask) before returning its
    // error. The freezes and the quiesce guard are released here, in
    // reverse order of acquisition, so the daemon is indistinguishable
    // from before the call. (The checked persistence pass may have
    // rewritten `daemon-sessions.json` — a valid, current snapshot,
    // the same file every lifecycle event rewrites; not a mutation to
    // undo.)
    let err = exec_stage(state, target, &target_fd, &rollback_fd, dry_schema_version);
    drop(reader_freeze);
    drop(reap_freeze);
    drop(writer_freeze);
    drop(writer_pause);
    guard.abort();
    err
}

/// Steps (c2)–(g): checked persistence pass + post-freeze
/// revalidation (phase 4c, R15), then manifest build/seal, CLOEXEC
/// discipline, signal bracketing, the exec itself. Runs entirely
/// under the caller's gate freezes + quiesce guard; returns ONLY on
/// failure, having restored the fd-table flags and signal mask it
/// changed.
///
/// `dry_schema_version` is the schema version the preflight's dry
/// manifest carried (`None` when the preflight was skipped); the
/// revalidation step re-asserts it against the constant the REAL
/// manifest below will carry.
fn exec_stage(
    state: &Arc<Mutex<DaemonState>>,
    target: &Path,
    target_fd: &OwnedFd,
    rollback_fd: &OwnedFd,
    dry_schema_version: Option<u32>,
) -> anyhow::Error {
    // ---- Step (c2): checked, durable persistence pass (4c, R15). ----
    // AFTER the freezes (quiescence means this snapshot is the frozen
    // one — no writer can race it) and BEFORE the manifest build, so
    // the new image rebuilds from exactly the state that was frozen.
    // Errors abort the restart through the caller's existing restore
    // path — unlike the best-effort lifecycle persists, nothing here
    // is swallowed.
    if let Err(e) = persist_all_checked(state) {
        return e;
    }
    // ---- Step (c3): post-freeze revalidation (4c, R15). ----
    // The dry preflight raced live mutations; re-assert cheaply, in
    // process, that what it validated still holds for the frozen
    // snapshot.
    if let Err(e) = post_freeze_revalidation(state, dry_schema_version) {
        return e;
    }

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

// ============================================================
// Phase 6: daemon.restart target resolution (pinned-artifact policy)
// ============================================================

/// The resolved target of a `daemon.restart` call.
pub struct RestartTarget {
    /// The path [`perform_reexec`] will pin and exec.
    pub path: PathBuf,
    /// The target currently resolves to the SAME inode as the running
    /// image (`/proc/self/exe`) — the design's deferred
    /// new-inode ≠ rollback-inode assertion, implemented as a
    /// **warning, not a refusal** (phase 6): a same-binary restart is
    /// legitimate (config-adjacent state reload, post-crash
    /// freshening), but the caller should know no code change is
    /// taking effect. Best-effort probe (path-level stat — the real
    /// pin happens inside [`perform_reexec`]); a probe failure reads
    /// as `false`, never blocks the restart.
    pub same_binary: bool,
}

/// Resolve `daemon.restart`'s target binary (phase 6's
/// pinned-artifact policy, design § Security posture):
///
/// - **Default** (`binary_path` absent): the daemon's own current
///   executable PATH, resolved via `readlink("/proc/self/exe")` at
///   RPC time (any ` (deleted)` suffix stripped — a deploy that
///   replaced the file leaves the link pointing at the old, unlinked
///   inode's pathname). This default is exactly the intended deploy
///   flow: `cargo build` / `cp` overwrite that path with the NEW
///   binary, then `daemon.restart` opens the path and execs the new
///   inode. Note `File::open(path)` downstream opens whatever inode
///   the path NOW names, not the running image's.
/// - **Caller-supplied**: must be an ABSOLUTE path to an existing
///   regular file — anything else is refused with an error naming the
///   constraint. (Execute permission and the deeper shape checks stay
///   with [`perform_reexec`]'s pin, which is fd-based and
///   TOCTOU-free; this is the cheap parameter-shape gate.)
///
/// Errors are plain strings for the dispatcher to wrap as
/// `invalid_params`.
pub fn resolve_restart_target(
    binary_path: Option<&str>,
) -> Result<RestartTarget, String> {
    let path: PathBuf = match binary_path {
        Some(p) => {
            let p = p.trim();
            if p.is_empty() {
                return Err(
                    "binary_path, when supplied, must be a non-empty \
                     ABSOLUTE path to the binary to exec (omit it to \
                     restart into the daemon's own executable path — \
                     the normal deploy flow overwrites that path with \
                     the new binary first)"
                        .to_string(),
                );
            }
            let pb = PathBuf::from(p);
            if !pb.is_absolute() {
                return Err(format!(
                    "binary_path {:?} is not absolute — daemon.restart \
                     accepts only absolute paths to a regular file \
                     (pinned-artifact policy, DESIGN_SEAMLESS_RESTART \
                     § Security posture)",
                    p,
                ));
            }
            pb
        }
        None => {
            let link = std::fs::read_link("/proc/self/exe").map_err(|e| {
                format!(
                    "could not resolve the default target: \
                     readlink(/proc/self/exe) failed: {} — pass \
                     binary_path explicitly",
                    e,
                )
            })?;
            // A replaced-on-disk binary leaves the link reading
            // "<path> (deleted)"; the PATH half is what the deploy
            // flow overwrote with the new artifact.
            let s = link.to_string_lossy();
            match s.strip_suffix(" (deleted)") {
                Some(stripped) => PathBuf::from(stripped),
                None => PathBuf::from(s.into_owned()),
            }
        }
    };

    let st = std::fs::metadata(&path).map_err(|e| {
        format!(
            "restart target {} is not usable: {} — the target must be \
             an existing regular file (did the deploy actually land \
             the new binary there?)",
            path.display(),
            e,
        )
    })?;
    if !st.is_file() {
        return Err(format!(
            "restart target {} is not a regular file",
            path.display(),
        ));
    }

    // Same-inode probe against the RUNNING image. /proc/self/exe
    // opens the original inode even after a deploy overwrote the
    // path, so dev+ino equality means "this restart execs the code
    // already running". Best-effort — warning-grade only.
    let same_binary = File::open("/proc/self/exe")
        .ok()
        .and_then(|f| f.metadata().ok())
        .map(|self_md| {
            use std::os::unix::fs::MetadataExt as _;
            self_md.dev() == st.dev() && self_md.ino() == st.ino()
        })
        .unwrap_or(false);

    Ok(RestartTarget { path, same_binary })
}

// ============================================================
// Phase 4c: the --verify-handoff preflight (design step 2, R15)
// ============================================================

/// Step (a2): run the CANDIDATE binary as a verify-only subprocess
/// against a DRY manifest, before any point of no return.
///
/// The child is spawned as `/proc/self/fd/<target fd>` — the PINNED
/// inode from step (a), never the pathname, so the verified artifact
/// is the artifact the exec will consume even if a deploy overwrites
/// the path mid-sequence (the R7 discipline, extended to the
/// preflight).
///
/// The dry manifest is [`build_manifest`] against LIVE state (brief
/// state-lock hold — unlike the real build it runs without the gate
/// freezes, so a session mid-exit can fail it; that surfaces as an
/// inline refusal and a retry works). It is **discarded** after the
/// subprocess: the REAL manifest is built fresh inside [`exec_stage`]
/// AFTER quiescence — that ordering is the R15 point (the dry run
/// races live mutations; the handed-off snapshot is the frozen one),
/// and [`post_freeze_revalidation`] re-asserts the dry run's schema
/// assertion against the frozen state.
///
/// CLOEXEC discipline (R9): the two fds the child needs — the sealed
/// dry-manifest memfd and the pinned target-binary fd — cross into it
/// by clearing the flag INSIDE the child via `pre_exec`: fork copies
/// the fd table (flags included) and the child-side `fcntl` mutates
/// only the child's copies, so no non-CLOEXEC fd ever exists in OUR
/// table and a concurrent session spawn can never inherit either.
/// Both are harmless in any child regardless: a same-user subprocess,
/// no secrets — one is a kernel-sealed read-only manifest, the other
/// a read-only fd of the candidate binary (which must stay open
/// across the child's exec anyway so a `#!` target's interpreter can
/// re-open `/proc/self/fd/<n>`).
///
/// Bounded by [`VERIFY_HANDOFF_TIMEOUT`], then killed. Nonzero exit /
/// timeout / spawn failure → `Err` carrying the child's diagnosis;
/// the RPC returns it inline and NOTHING has happened. On success,
/// returns the dry manifest's schema version for the post-freeze
/// revalidation.
fn preflight_verify_handoff(
    state: &Arc<Mutex<DaemonState>>,
    target: &Path,
    target_fd: &OwnedFd,
    rollback_fd: &OwnedFd,
) -> Result<u32, anyhow::Error> {
    let dry = build_manifest(state, rollback_fd).map_err(|e| {
        anyhow::anyhow!(
            "re-exec preflight REFUSED: could not build the dry manifest \
             against live state: {} — nothing has happened",
            e
        )
    })?;
    let dry_schema_version = dry.schema_version;
    let dry_session_count = dry.sessions.len();
    let dry_fd = reexec_manifest::write_manifest(&dry).map_err(|e| {
        anyhow::anyhow!(
            "re-exec preflight REFUSED: could not write the dry manifest \
             memfd: {} — nothing has happened",
            e
        )
    })?;

    let target_raw = target_fd.as_raw_fd();
    let manifest_raw = dry_fd.as_raw_fd();
    let mut cmd = Command::new(format!("/proc/self/fd/{}", target_raw));
    cmd.arg("--verify-handoff")
        .arg(manifest_raw.to_string())
        // Defense in depth: a stale bootstrap pointer in our environ
        // (there should never be one — startup consumed it) must not
        // confuse the verify child, whose manifest fd arrives via argv.
        .env_remove(reexec_manifest::ENV_MANIFEST_FD)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY (pre_exec): runs post-fork in the child before exec;
    // fcntl is async-signal-safe and touches only the child's fd
    // table (see the fn docs for why this closes the R9 window
    // entirely — no non-CLOEXEC fd ever exists in the parent).
    unsafe {
        cmd.pre_exec(move || {
            for fd in [target_raw, manifest_raw] {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC)
                    != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }

    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        anyhow::anyhow!(
            "re-exec preflight REFUSED: could not spawn the candidate \
             binary {} (via /proc/self/fd/{}) in --verify-handoff mode: {} \
             — nothing has happened",
            target.display(),
            target_raw,
            e
        )
    })?;
    // Bounded poll-wait. The child's output is small by construction
    // (a PASS line or a diagnosis, both far under the pipe buffer),
    // so deferring the pipe reads until after exit cannot deadlock.
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if started.elapsed() >= VERIFY_HANDOFF_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(anyhow::anyhow!(
                        "re-exec preflight REFUSED: `{} --verify-handoff` \
                         did not finish within {:?} (killed) — nothing has \
                         happened",
                        target.display(),
                        VERIFY_HANDOFF_TIMEOUT
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!(
                    "re-exec preflight REFUSED: wait on the --verify-handoff \
                     subprocess failed: {} — nothing has happened",
                    e
                ));
            }
        }
    };
    let mut out = String::new();
    if let Some(mut s) = child.stdout.take() {
        let _ = s.read_to_string(&mut out);
    }
    let mut errout = String::new();
    if let Some(mut s) = child.stderr.take() {
        let _ = s.read_to_string(&mut errout);
    }
    // dry_fd drops at return — the dry manifest is discarded either way.

    if status.success() {
        eprintln!(
            "cm-daemon: re-exec preflight PASS — candidate {} verified the \
             handoff shape against current on-disk state ({} live \
             session(s) in the dry manifest): {}",
            target.display(),
            dry_session_count,
            out.trim(),
        );
        Ok(dry_schema_version)
    } else {
        let mut parts: Vec<String> = Vec::new();
        let o = cap_str(out.trim(), VERIFY_OUTPUT_CAP);
        if !o.is_empty() {
            parts.push(o);
        }
        let e = cap_str(errout.trim(), VERIFY_OUTPUT_CAP);
        if !e.is_empty() {
            parts.push(format!("stderr: {}", e));
        }
        let diagnosis = if parts.is_empty() {
            "<no output from the candidate>".to_string()
        } else {
            parts.join(" | ")
        };
        Err(anyhow::anyhow!(
            "re-exec preflight REFUSED the restart: `{} --verify-handoff` \
             exited {} — nothing has happened. Diagnosis: {}",
            target.display(),
            status,
            diagnosis
        ))
    }
}

/// Truncate for inline diagnosis quoting (see [`VERIFY_OUTPUT_CAP`]).
fn cap_str(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} bytes truncated]", &s[..end], s.len() - end)
}

/// Step (c2): the CHECKED, DURABLE persistence pass
/// (DESIGN_SEAMLESS_RESTART phase 4c, design step 3's final bullet,
/// R15). Runs under the quiesce barrier + gate freezes, so the
/// snapshot it lands is the frozen one. Unlike the best-effort
/// lifecycle persists (which stay log-and-swallow for normal
/// operation), every failure here is returned and aborts the restart
/// through the existing restore path.
///
/// Two shapes, matching where each store's truth lives:
///
/// - **Sessions and recently-exited tombstones** are
///   memory-authoritative (`DaemonState::sessions` /
///   `.recently_exited`), so fresh snapshots are WRITTEN —
///   [`DaemonState::save_daemon_sessions_checked`] for the registry
///   and [`DaemonState::save_daemon_tombstones_checked`] for the
///   tombstone sidecar (`daemon-tombstones.json`, beside the sessions
///   file) — each with fsynced file AND directory (the rename-swap
///   idiom needs the dir fsync for durability). Tombstones persist at
///   all only since 4c; pre-4c every pre-swap tombstone — kill
///   attribution included — died with the old image (the P4b known
///   gap).
/// - **Workflow runs and continuous tasks** are DISK-authoritative:
///   every mutation is a locked read-modify-write against the run's /
///   task's `state.json` (`workflow::run::modify`,
///   `continuous::task::save`), and the in-memory maps are caches —
///   rewriting from memory could clobber a fresher on-disk row (the
///   TUI writes `events_offset` into run state concurrently). The
///   checked pass therefore makes the EXISTING bytes durable instead:
///   fsync every `state.json`, its containing run/task dir, and the
///   tree root.
fn persist_all_checked(
    state: &Arc<Mutex<DaemonState>>,
) -> Result<(), anyhow::Error> {
    {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        match st.daemon_sessions_path.clone() {
            Some(path) => {
                st.save_daemon_sessions_checked(&path).map_err(|e| {
                    anyhow::anyhow!(
                        "checked persist of daemon sessions to {} failed: {} \
                         — aborting the restart (the swap must rebuild from \
                         a snapshot known to have landed)",
                        path.display(),
                        e
                    )
                })?;
                // The tombstone sidecar lives beside the sessions file
                // (in production both are the `default_*_path()` pair
                // under ~/.cm; deriving keeps a custom-pathed test
                // hermetic).
                let tomb_path = path.with_file_name("daemon-tombstones.json");
                st.save_daemon_tombstones_checked(&tomb_path).map_err(|e| {
                    anyhow::anyhow!(
                        "checked persist of exited-session tombstones to {} \
                         failed: {} — aborting the restart",
                        tomb_path.display(),
                        e
                    )
                })?;
                eprintln!(
                    "cm-daemon: re-exec checked persist — {} live session(s) \
                     fsynced to {}, {} tombstone(s) fsynced to {}",
                    st.sessions.len(),
                    path.display(),
                    st.recently_exited.len(),
                    tomb_path.display(),
                );
            }
            None => {
                // Tests / a daemon configured without durability. The
                // handoff still works (sessions ride the FD manifest);
                // only the disk snapshot is absent, honestly.
                eprintln!(
                    "cm-daemon: re-exec checked persist — no \
                     daemon_sessions_path configured; skipping the session \
                     and tombstone snapshots"
                );
            }
        }
    }
    fsync_state_tree(&crate::workflow::run::runs_dir(), "workflow-runs")?;
    fsync_state_tree(
        &crate::continuous::task::tasks_dir(),
        "continuous-tasks",
    )?;
    Ok(())
}

/// Fsync every `<root>/<id>/state.json` + its containing dir + the
/// tree root — the durability half of [`persist_all_checked`] for the
/// disk-authoritative stores (see its docs for why those are synced
/// in place rather than rewritten). A missing root is fine (nothing
/// was ever persisted); any real IO failure is returned.
fn fsync_state_tree(
    root: &Path,
    label: &str,
) -> Result<(), anyhow::Error> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(anyhow::anyhow!(
                "checked persist: read {} dir {}: {}",
                label,
                root.display(),
                e
            ))
        }
    };
    let mut synced = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| {
            anyhow::anyhow!(
                "checked persist: read {} dir {}: {}",
                label,
                root.display(),
                e
            )
        })?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let state_file = dir.join("state.json");
        match File::open(&state_file) {
            Ok(f) => {
                f.sync_all().map_err(|e| {
                    anyhow::anyhow!(
                        "checked persist: fsync {}: {}",
                        state_file.display(),
                        e
                    )
                })?;
                synced += 1;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "checked persist: open {} for fsync: {}",
                    state_file.display(),
                    e
                ))
            }
        }
        File::open(&dir).and_then(|d| d.sync_all()).map_err(|e| {
            anyhow::anyhow!(
                "checked persist: fsync {} dir {}: {}",
                label,
                dir.display(),
                e
            )
        })?;
    }
    File::open(root).and_then(|d| d.sync_all()).map_err(|e| {
        anyhow::anyhow!(
            "checked persist: fsync {} root {}: {}",
            label,
            root.display(),
            e
        )
    })?;
    eprintln!(
        "cm-daemon: re-exec checked persist — fsynced {} {} state file(s) \
         under {}",
        synced,
        label,
        root.display(),
    );
    Ok(())
}

/// Step (c3): post-freeze revalidation (phase 4c, R15). The dry
/// preflight raced live mutations, so after quiescence and the
/// checked persist, re-assert cheaply IN PROCESS that what it
/// validated still holds for the frozen snapshot:
///
/// - the schema version the dry run embedded equals the constant the
///   REAL manifest (built next, in [`exec_stage`]) will carry — the
///   preflighted shape and the handed-off shape cannot silently
///   diverge;
/// - the just-persisted `daemon-sessions.json` re-parses with the
///   CURRENT loader (`state::read_daemon_sessions` — a read-back
///   check, cheap at these sizes), so the file the new image will
///   rebuild non-session state from is known-parseable AND known to
///   be on disk.
///
/// A distinct, named function so phase 6's `daemon.restart` RPC can
/// surface its failure apart from the preflight's.
fn post_freeze_revalidation(
    state: &Arc<Mutex<DaemonState>>,
    dry_schema_version: Option<u32>,
) -> Result<(), anyhow::Error> {
    if let Some(v) = dry_schema_version {
        if v != MANIFEST_SCHEMA_VERSION {
            return Err(anyhow::anyhow!(
                "post-freeze revalidation failed: the preflight's dry \
                 manifest carried schema version {} but the real manifest \
                 will carry {} — the preflighted state and the handed-off \
                 state diverged; aborting the restart",
                v,
                MANIFEST_SCHEMA_VERSION
            ));
        }
    }
    let sessions_path = {
        let st = state.lock().unwrap_or_else(|p| p.into_inner());
        st.daemon_sessions_path.clone()
    };
    if let Some(path) = sessions_path {
        match crate::state::read_daemon_sessions(&path) {
            Ok(Some(m)) => {
                eprintln!(
                    "cm-daemon: re-exec post-freeze revalidation — {} \
                     re-parses with the current loader ({} workspace(s))",
                    path.display(),
                    m.workspaces.len(),
                );
            }
            Ok(None) => {
                return Err(anyhow::anyhow!(
                    "post-freeze revalidation failed: the just-persisted {} \
                     is missing at read-back — aborting the restart",
                    path.display()
                ));
            }
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "post-freeze revalidation failed: the just-persisted {} \
                     does not re-parse with the current loader: {} — \
                     aborting the restart",
                    path.display(),
                    e
                ));
            }
        }
        // The just-persisted tombstone sidecar, same read-back rule.
        let tomb_path = path.with_file_name("daemon-tombstones.json");
        if let Err(e) = crate::state::read_daemon_tombstones(&tomb_path) {
            return Err(anyhow::anyhow!(
                "post-freeze revalidation failed: the just-persisted {} does \
                 not re-parse with the current loader: {} — aborting the \
                 restart",
                tomb_path.display(),
                e
            ));
        }
    }
    Ok(())
}

// ============================================================
// The candidate side: --verify-handoff mode (phase 4c)
// ============================================================

/// Entry point for `cm-daemon --verify-handoff <manifest-fd>`
/// (dispatched by `main.rs` before `run()` is ever reached): this
/// process is the CANDIDATE new binary, spawned by the still-serving
/// old image as a verify-only subprocess (design step 2 — catch
/// state-schema drift, a broken binary, or a dead MCP interpreter
/// BEFORE any point of no return). Prints one PASS line, or a
/// diagnosis naming exactly what failed, and returns the exit code
/// (0/1) for `main` to exit with.
///
/// **Verify-only, structurally**: this mode NEVER binds the control
/// socket, spawns/kills/restores sessions, or writes ANY state file —
/// it parses the inherited dry manifest and the current on-disk state
/// with the same code paths startup uses, runs the MCP selftest, and
/// exits. (The integration suite proves the no-writes property with a
/// before/after directory snapshot.)
///
/// What it checks, in order (first failure wins the diagnosis):
///
/// 1. **The dry manifest** from the inherited fd — envelope,
///    integrity, and structure via [`reexec_manifest::read_manifest`],
///    which includes the schema-version gate: a candidate whose
///    manifest schema drifted from the running image refuses right
///    here, old image untouched. Explicitly **NOT**
///    [`reexec_manifest::validate_fd_roles`]: the fd NUMBERS inside
///    the manifest reference the PARENT's fd table — the session
///    masters / pidfds / listener are deliberately not inherited by
///    this subprocess (they stay CLOEXEC in the parent until the real
///    exec's allowlist step), so a role probe here would poke
///    unrelated slots in OUR table. Roles are re-validated for real
///    post-exec by [`detect_handoff`].
/// 2. **`daemon-sessions.json`** via the REAL loader
///    (`state::read_daemon_sessions`) — the whole point is exercising
///    THIS binary's parsers against the old binary's files. Missing
///    file: fine (clean boot shape). Present but unparseable: FAIL,
///    naming the file. The **tombstone sidecar**
///    (`daemon-tombstones.json`, `state::read_daemon_tombstones`)
///    follows the same rule.
/// 3. **Workflow-runs state** via `workflow::run::load_all_strict`
///    (the strict twin of the startup loader — same parser, but a
///    present-and-broken `state.json` fails instead of being
///    skipped).
/// 4. **Continuous-task state** via
///    `continuous::task::load_all_strict` (same shape).
/// 5. **`daemon.toml`** via `config::load_or_default` at
///    `config::default_config_path()` — exactly how `run()` loads it
///    (and `run()` refuses to start on a config error, so the
///    candidate would too).
/// 6. **The MCP preflight** (`mcp_config::run_mcp_preflight` with the
///    config's `mcp_server_path` override) — the 45-minutes-of-broken-
///    sessions incident class. When NO server path resolves at all the
///    selftest is skipped with a note (matching startup, which serves
///    without one — an already-absent server must not veto a restart);
///    a server that resolves but fails its selftest is a FAIL.
pub fn run_verify_handoff(fd_arg: &str) -> i32 {
    match verify_handoff_impl(fd_arg) {
        Ok(pass) => {
            println!("cm-daemon --verify-handoff: PASS — {}", pass);
            0
        }
        Err(fail) => {
            println!("cm-daemon --verify-handoff: FAIL — {}", fail);
            1
        }
    }
}

/// The checks behind [`run_verify_handoff`]; see its docs for the
/// list and the ordering contract.
fn verify_handoff_impl(fd_arg: &str) -> Result<String, String> {
    // Strict digits-only fd parse, mirroring the env bootstrap's rule
    // (reexec_manifest::parse_fd is private; the grammar is the same).
    if fd_arg.is_empty() || !fd_arg.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!(
            "manifest-fd argument {:?} is not a bare non-negative fd number",
            fd_arg
        ));
    }
    let fd: RawFd = fd_arg
        .parse()
        .map_err(|e| format!("manifest-fd argument {:?}: {}", fd_arg, e))?;

    // -- 1: the dry manifest (envelope + structure ONLY; see docs). --
    // SAFETY: validation-only borrow of the fd number the parent
    // passed; this mode never takes ownership (the process exits
    // right after and the kernel closes it).
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let manifest = reexec_manifest::read_manifest(borrowed)
        .map_err(|e| format!("manifest (fd {}): {}", fd, e))?;

    // -- 2: daemon-sessions.json with the real loader. --
    let sessions_path = crate::state::default_daemon_sessions_path();
    let sessions_note = match crate::state::read_daemon_sessions(&sessions_path)
    {
        Ok(Some(m)) => format!(
            "{}: {} workspace(s)",
            sessions_path.display(),
            m.workspaces.len(),
        ),
        Ok(None) => format!("{}: absent (fine)", sessions_path.display()),
        Err(e) => {
            return Err(format!(
                "state file {}: {}",
                sessions_path.display(),
                e
            ))
        }
    };
    // -- 2b: the tombstone sidecar, same rule (absent fine, corrupt
    // fails).
    let tombstones_path = crate::state::default_daemon_tombstones_path();
    let tombstones_note =
        match crate::state::read_daemon_tombstones(&tombstones_path) {
            Ok(Some(f)) => format!("{} tombstone(s)", f.recently_exited.len()),
            Ok(None) => "no tombstone sidecar (fine)".to_string(),
            Err(e) => {
                return Err(format!(
                    "state file {}: {}",
                    tombstones_path.display(),
                    e
                ))
            }
        };

    // -- 3 + 4: workflow-runs + continuous state, strictly. --
    let runs = crate::workflow::run::load_all_strict()
        .map_err(|e| format!("workflow-runs state: {}", e))?;
    let tasks = crate::continuous::task::load_all_strict()
        .map_err(|e| format!("continuous-task state: {}", e))?;

    // -- 5: daemon.toml, the way run() loads it. --
    let config_path = crate::config::default_config_path();
    let cfg = crate::config::load_or_default(&config_path)
        .map_err(|e| format!("daemon config {}: {}", config_path.display(), e))?;

    // -- 6: the MCP preflight. --
    let server_override = if cfg.mcp_server_path.trim().is_empty() {
        None
    } else {
        Some(cfg.mcp_server_path.as_str())
    };
    let mcp_note = match crate::mcp_config::resolve_server_path(server_override)
    {
        None => "no MCP server resolves on this host — selftest skipped \
                 (matches startup, which serves without one)"
            .to_string(),
        Some(_) => crate::mcp_config::run_mcp_preflight(server_override)
            .map_err(|e| {
                format!("MCP preflight: {}", e.replace('\n', " | "))
            })?,
    };

    Ok(format!(
        "manifest schema v{} sealed OK ({} session record(s), attempt {}); \
         {}; {}; {} workflow run(s); {} continuous task(s); MCP: {}",
        manifest.schema_version,
        manifest.sessions.len(),
        manifest.attempt,
        sessions_note,
        tombstones_note,
        runs.len(),
        tasks.len(),
        mcp_note,
    ))
}

/// Step (d) helper: project live daemon state into the manifest.
/// Holds the state lock briefly (cheap /proc reads + cell locks
/// only). Runs under the gate freezes, so the session set cannot
/// change underneath: no reaper can remove an entry (consumption
/// frozen) and no mutating RPC can add one (quiesced + draining).
///
/// Phase 4b (R11): the full `DaemonSession` record rides — identity,
/// bindings, perms, transcript path, memory-cap bytes — plus the
/// status cells as AGES against ONE anchor (`Instant::now()` taken
/// once here), so the read side's single-anchor reconstruction
/// preserves the cells' relative order exactly (the superseded rule
/// and `semantic_idle` compare cells against each other). The
/// monotonic-instant cells cannot ride as absolutes; the sub-second
/// swap skew the age round trip introduces is documented acceptable
/// in the manifest module.
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
    // ONE anchor for every age in this manifest (see the doc above).
    let now = std::time::Instant::now();
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
        // Status cells → ages. Cell mutexes are leaves (their stamp
        // paths never take the state lock around them), so locking
        // them under the state lock is inversion-free.
        let cell_age = |cell: &crate::session::SharedLastActivity| {
            cell.lock()
                .unwrap_or_else(|p| p.into_inner())
                .map(|t| now.saturating_duration_since(t).as_secs_f64())
        };
        // The RAW done-report cell, not `reported_done()`: the
        // superseded-by-later-input rule stays derived on BOTH sides
        // from `last_input_at`, so carrying the raw cell + the input
        // age reproduces exactly the pre-swap answer.
        let done_report = s
            .done_report
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .map(|r| reexec_manifest::DoneReportRecord {
                at_unix: r.at_unix,
                age_s: now
                    .saturating_duration_since(r.at_instant)
                    .as_secs_f64(),
                reason: r.reason,
            });
        sessions.push(SessionRecord {
            uid: uid.clone(),
            generation: s.generation,
            transcript_id: s
                .transcript_path
                .as_deref()
                .and_then(crate::session::transcript_id_from_path),
            transcript_path: s.transcript_path.clone(),
            session_type: s.session_type.clone(),
            title: s.title.clone(),
            workspace_id: s.workspace_id.clone(),
            task_id: s.task_id.clone(),
            managed_by_uid: s.managed_by_uid.clone(),
            workflow_run_id: s.workflow_run_id.clone(),
            workflow_role: s.workflow_role.clone(),
            continuous_task_id: s.continuous_task_id.clone(),
            global_perms: s.global_perms,
            memory_cap_soft_bytes: s.memory_cap_soft_bytes,
            memory_cap_hard_bytes: s.memory_cap_hard_bytes,
            last_activity_age_s: cell_age(&s.last_activity_at),
            last_input_age_s: cell_age(&s.last_input_at),
            last_operator_input_age_s: cell_age(&s.last_operator_input_at),
            last_turn_end_age_s: cell_age(&s.last_turn_end_at),
            done_report,
            child_pid: s.pid,
            child_start_time,
            pty_master_fd,
            pidfd,
            cgroup_prefix: s
                .cgroup_prefix
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
            // Phase 4d (R12): capped sessions checkpoint their
            // watcher's live policy state (protected set, breach
            // watermark, watched cgroup path) plus the session-side
            // spawn-time kill-log baseline. For the REAL build the
            // watcher is PARKED at its no-signal safe point — the
            // quiesce barrier completed before exec_stage ran — so
            // the cell is stable; the dry preflight build reads a
            // LIVE cell, which is fine (per-snapshot atomic under
            // the cell mutex, and the dry manifest is discarded).
            // Lock order: the watcher-state cell is a LEAF mutex
            // like the status cells above — the watcher thread locks
            // it holding nothing, and nothing takes the state lock
            // or a gate while holding it — so locking it under this
            // state-lock hold is inversion-free. Serialization is
            // infallible for this shape in practice; a failure rides
            // as None (logged), degrading the re-adoption to a fresh
            // watcher rather than failing the restart.
            watcher_checkpoint: s.watcher_state.as_ref().and_then(|cell| {
                let ws = cell.lock().unwrap_or_else(|p| p.into_inner());
                let cp = ws.checkpoint(s.last_exit.kills_baseline());
                match serde_json::to_value(&cp) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        eprintln!(
                            "cm-daemon: session '{}': watcher checkpoint \
                             failed to serialize ({}) — riding None; the \
                             new image will re-adopt with a FRESH watcher \
                             (policy reset)",
                            uid, e,
                        );
                        None
                    }
                }
            }),
        });
    }
    Ok(ReexecManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        // Written as 0 and read back by the new image; the rollback
        // attempt state machine that ACTS on it is phase-4 scope.
        attempt: 0,
        // Phase 6 (schema v3): the generation this handoff BECOMES if
        // it commits — our own lineage + 1. Stamped onto the new
        // image's state by the rehydrate commit; a rollback exec
        // carries it unchanged (one restart attempt = one generation).
        reexec_generation: st.reexec_generation + 1,
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
// The rehydrate side (new image) — escrow, transaction, ladder
// ============================================================

/// The failure-injection knob for the phase-4a e2e:
/// `CM_RESTART_TEST_FAIL_REHYDRATE`.
///
/// Deliberately **NOT** `CM_REEXEC_*`-prefixed: [`do_execveat`]
/// strips that prefix from the envp it builds (the manifest pointer
/// is a bootstrap, not a trust surface — R8/R9), so a prefixed knob
/// would die at the first exec. This one must survive INTO the new
/// image (and through the rollback exec into the rollback image),
/// because the failure it injects happens there.
///
/// Value semantics (parsed by [`test_fail_requested`]):
/// - a comma-separated list of attempt numbers — the knob names the
///   attempts to fail, so `"0"` fails only the attempt-0 image (the
///   rollback image at attempt 1 rehydrates cleanly: the rollback
///   round-trip e2e), and `"0,1"` fails both (driving rollback and
///   then the terminal fallback: the terminal e2e);
/// - `"terminal"` — fail EVERY attempt, regardless of number.
///
/// **Honored ONLY when the daemon started with the `CM_REEXEC=1` dev
/// flag.** A production daemon (no dev flag) ignores the knob
/// entirely — it logs the refusal once and proceeds; no environment
/// variable may be able to make a production rehydrate fail on
/// purpose.
pub const ENV_TEST_FAIL_REHYDRATE: &str = "CM_RESTART_TEST_FAIL_REHYDRATE";

/// Where [`append_crash_note`] writes: `$HOME/.cm/reexec-crash-note.log`.
const CRASH_NOTE_FILENAME: &str = "reexec-crash-note.log";

// ------------------------------------------------------------
// The attempt state machine (design → "Failure classes")
// ------------------------------------------------------------

/// What to do with a RECEIVED manifest, keyed purely off its sealed
/// `attempt` counter. One half of the phase-4a attempt state machine
/// (the other is [`on_rehydrate_failure`]); a small explicit function
/// so the ladder is testable, not scattered ifs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptDecision {
    /// attempt 0 (a fresh handoff) or attempt 1 (this IS the rollback
    /// image rehydrating): run the rehydrate transaction.
    Rehydrate,
    /// attempt ≥ 2: a manifest claiming this means the machine
    /// already failed twice — never trust a third exec; go straight
    /// to the terminal fallback.
    TerminalFallback,
}

/// See [`AttemptDecision`].
pub fn attempt_decision(attempt: u8) -> AttemptDecision {
    if attempt >= 2 {
        AttemptDecision::TerminalFallback
    } else {
        AttemptDecision::Rehydrate
    }
}

/// What to do after the rehydrate transaction FAILED at `attempt`.
/// The other half of the attempt state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureNext {
    /// attempt 0 failed: exec the pinned rollback binary with the
    /// manifest's counter bumped to `next_attempt` (always 1).
    RollbackExec { next_attempt: u8 },
    /// attempt 1 failed (the ROLLBACK image failed too — the attempt
    /// counter would reach 2): terminal fallback. Also the total-
    /// function answer for attempt ≥ 2, which [`attempt_decision`]
    /// already diverted before any transaction could run.
    TerminalFallback,
}

/// See [`FailureNext`].
pub fn on_rehydrate_failure(attempt: u8) -> FailureNext {
    match attempt {
        0 => FailureNext::RollbackExec { next_attempt: 1 },
        _ => FailureNext::TerminalFallback,
    }
}

/// Parse [`ENV_TEST_FAIL_REHYDRATE`] against this image's `attempt`.
/// Returns the artificial failure reason when the knob names this
/// attempt AND the dev flag is set; `None` otherwise. See the
/// constant's doc for the loud production gate.
fn test_fail_requested(reexec_enabled: bool, attempt: u8) -> Option<String> {
    let val = std::env::var(ENV_TEST_FAIL_REHYDRATE).ok()?;
    let val = val.trim().to_string();
    if val.is_empty() {
        return None;
    }
    if !reexec_enabled {
        eprintln!(
            "cm-daemon: {}={} is set but CM_REEXEC=1 is not — IGNORING the \
             failure-injection knob (a production daemon never honors it)",
            ENV_TEST_FAIL_REHYDRATE, val,
        );
        return None;
    }
    let hit = val == "terminal"
        || val
            .split(',')
            .any(|p| p.trim().parse::<u8>().map_or(false, |n| n == attempt));
    if hit {
        Some(format!(
            "{}={} matched attempt {} — artificial rehydrate failure injected \
             after validation, before commit (dev knob; see reexec.rs)",
            ENV_TEST_FAIL_REHYDRATE, val, attempt,
        ))
    } else {
        None
    }
}

// ------------------------------------------------------------
// Escrow (R5's full form)
// ------------------------------------------------------------

/// The escrowed kernel handles for one manifest session record —
/// the ORIGINAL inherited fds, untouched.
struct EscrowedSessionFds {
    pidfd: OwnedFd,
    master: OwnedFd,
}

/// Escrow table for one handoff: owns every ORIGINAL inherited fd
/// the manifest names — untouched until the transaction's outcome is
/// known (R5's full form: "rollback must use the original escrow
/// FDs, not partially consumed objects"). Built by [`detect_handoff`]
/// the moment the manifest validates; consumed by exactly one of
/// three exits:
///
/// - **Commit** ([`complete_handoff`], transaction succeeded): the
///   registry sessions were armed on CLOEXEC *dups*, so dropping the
///   escrow closes the now-redundant originals plus the manifest and
///   rollback fds (no rollback needed past the commit gate).
/// - **Rollback exec** ([`rollback_exec`]): the intact originals are
///   re-listed (same fd numbers) in a NEW sealed manifest and cross
///   the exec again.
/// - **Terminal fallback** ([`terminal_fallback`]): each child is
///   deliberately SIGKILLed + reaped through its escrowed pidfd, then
///   everything closes.
///
/// The control-socket LISTENER is deliberately NOT escrowed here:
/// `run()` adopts it as its accept listener immediately at startup
/// (the daemon must come back serving on every outcome, terminal
/// fallback included), so its ownership lives in `run()`'s
/// `UnixListener`; the manifest's `listener_fd` NUMBER is all the
/// rollback exec needs (the fd stays open either way).
///
/// Every escrowed fd gets CLOEXEC re-set at construction (R9): they
/// crossed the exec cleared by necessity, and startup work between
/// detection and rehydrate spawns subprocesses (the MCP preflight),
/// which must not inherit another session's master. (3b set the flag
/// only at rehydrate time — this closes that transient window.)
pub struct HandoffEscrow {
    manifest: ReexecManifest,
    manifest_fd: OwnedFd,
    rollback_fd: OwnedFd,
    tls_listener_fd: Option<OwnedFd>,
    /// Parallel to `manifest.sessions` (index i ↔ record i).
    sessions: Vec<EscrowedSessionFds>,
}

impl HandoffEscrow {
    /// The validated manifest (read-only — `run()` needs
    /// `listener_fd` for the early listener adoption).
    pub fn manifest(&self) -> &ReexecManifest {
        &self.manifest
    }
}

/// Detect a re-exec handoff and build the escrow. Called by `run()`
/// during single-threaded startup, right after the identity scrub,
/// BEFORE `bind_socket` (normal startup's stale-socket probe would
/// connect to its own inherited listener and refuse with AddrInUse)
/// and BEFORE `restore_sessions` (legacy restore must never run on a
/// handoff — it would spawn `--resume` duplicates of children that
/// are still alive; R13).
///
/// `consume_env` scrubs `CM_REEXEC_MANIFEST_FD` unconditionally, so
/// on ANY validation failure the daemon boots fresh with nothing
/// left to bequeath to children — and on failure NO fd is touched,
/// including the claimed manifest fd itself: a leaked env var (the
/// 2026-08-18 pattern) may point at an unrelated inherited fd that
/// belongs to something else. Sequencing per the manifest module's
/// corrupt-manifest rule: integrity/structure first (`read_manifest`
/// touches only the manifest fd), then — and only then — the
/// escrow-fd role probes (`validate_fd_roles`, read-only,
/// side-effect-free).
///
/// `None` means "not a handoff — boot fresh" (today's fresh boot):
/// the var was absent, or validation failed and was logged.
pub fn detect_handoff() -> Option<HandoffEscrow> {
    let fd = reexec_manifest::consume_env()?;
    // SAFETY: validation-only borrow of the claimed fd number;
    // ownership is taken ONLY after the manifest verifies (below),
    // and never on failure.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let manifest = match reexec_manifest::read_manifest(borrowed).and_then(|m| {
        reexec_manifest::validate_fd_roles(&m)?;
        Ok(m)
    }) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "cm-daemon: {}={} was present but manifest validation \
                 FAILED: {} — booting FRESH (env already scrubbed; no \
                 inherited fd touched)",
                reexec_manifest::ENV_MANIFEST_FD, fd, e,
            );
            return None;
        }
    };
    eprintln!(
        "cm-daemon: re-exec handoff manifest validated (fd {}, {} \
         session(s), attempt {})",
        fd,
        manifest.sessions.len(),
        manifest.attempt,
    );

    // Ownership transfer into escrow: the handoff path owns every
    // inherited fd from the moment the manifest validated.
    // SAFETY: fd numbers come from a sealed, role-validated manifest
    // written by the previous image; each appears in exactly one slot
    // (duplicate-fd validation), so single ownership holds. The
    // listener fd is deliberately NOT taken — `run()` owns it (see
    // the type doc).
    let manifest_fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let rollback_fd = unsafe { OwnedFd::from_raw_fd(manifest.rollback_bin_fd) };
    let tls_listener_fd = manifest
        .tls_listener_fd
        .map(|raw| unsafe { OwnedFd::from_raw_fd(raw) });
    let sessions: Vec<EscrowedSessionFds> = manifest
        .sessions
        .iter()
        .map(|rec| EscrowedSessionFds {
            pidfd: unsafe { OwnedFd::from_raw_fd(rec.pidfd) },
            master: unsafe { OwnedFd::from_raw_fd(rec.pty_master_fd) },
        })
        .collect();

    // R9 hygiene, at the earliest possible moment: everything in
    // escrow crossed the exec CLOEXEC-cleared by necessity; re-set
    // the flag so no subprocess spawned between now and the
    // transaction's outcome (MCP preflight, launcher refresh) can
    // inherit a session's master or pidfd. Best-effort per fd — a
    // failure here is not worth failing the handoff over (the fds
    // provably exist; the flag write is a plain fcntl).
    let _ = set_fd_flags(manifest_fd.as_raw_fd(), libc::FD_CLOEXEC);
    let _ = set_fd_flags(rollback_fd.as_raw_fd(), libc::FD_CLOEXEC);
    if let Some(fd) = &tls_listener_fd {
        let _ = set_fd_flags(fd.as_raw_fd(), libc::FD_CLOEXEC);
    }
    for s in &sessions {
        let _ = set_fd_flags(s.pidfd.as_raw_fd(), libc::FD_CLOEXEC);
        let _ = set_fd_flags(s.master.as_raw_fd(), libc::FD_CLOEXEC);
    }

    Some(HandoffEscrow {
        manifest,
        manifest_fd,
        rollback_fd,
        tls_listener_fd,
        sessions,
    })
}

// ------------------------------------------------------------
// complete_handoff — ladder wiring around the transaction
// ------------------------------------------------------------

/// How a handoff ended, for `run()`'s thin wiring. The third design
/// outcome — *Fresh* (no/invalid manifest, today's fresh boot) — is
/// [`detect_handoff`] returning `None`; by the time an escrow exists
/// the boot is a handoff and only these two ends remain.
pub enum HandoffOutcome {
    /// The rehydrate transaction committed: sessions are in the
    /// registry, swap-dead records are reaped + tombstoned (4b),
    /// escrow is released. `run()` SKIPS legacy `restore_sessions`
    /// (R13) but runs its NON-SPAWNING half
    /// (`control::methods::rehydrate_derived_state` — phase 4g, audit
    /// gap 2) so bindings / task-tree edges / the agent_task_edges
    /// overlay / daemon-registered workspace rows come back from
    /// `daemon-sessions.json` without spawning anything.
    Adopted {
        adopted: usize,
        /// Records whose child exited during the swap — reaped and
        /// recorded as exited tombstones (4b).
        tombstoned: usize,
        /// Manifest-listed total (adopted + tombstoned + per-record
        /// identity-mismatch skips).
        total: usize,
    },
    /// The terminal fallback ran: every manifest-listed child was
    /// deliberately SIGKILLed + reaped (or verified already
    /// dead/mismatched and left unsignaled), and every escrow fd is
    /// closed. The inherited LISTENER is untouched — `run()` adopted
    /// it at startup, so the daemon comes back serving through it —
    /// and `run()` now proceeds with NORMAL startup semantics: legacy
    /// `restore_sessions` (resume from the H1-pinned stamps).
    TerminalFallback { killed: usize, total: usize },
}

/// Run the attempt ladder around the rehydrate transaction.
/// DESIGN_SEAMLESS_RESTART phase 4a — the "Failure classes,
/// exhaustively" section, made real:
///
/// - attempt ≥ 2 in the received manifest → [`terminal_fallback`]
///   immediately (the machine already failed twice — never trust a
///   third exec).
/// - attempt 0/1 → [`rehydrate_transaction`] under `catch_unwind`
///   (the top-level recovery guard: a panic while escrow FDs are
///   live must reach the ladder, not the process's unwind-to-exit).
/// - transaction failed at attempt 0 → crash note + [`rollback_exec`]
///   with attempt 1 (returns only if the exec itself failed, in which
///   case: terminal — never exec-loop blindly).
/// - transaction failed at attempt 1 → crash note + terminal.
///
/// On SUCCESS the escrow drops here: the registry sessions were
/// armed on dups, so the originals (plus the manifest fd and the
/// pinned rollback fd — no rollback needed past the commit gate)
/// close, and R9's "close every inherited fd you didn't adopt" holds.
pub fn complete_handoff(
    state: &Arc<Mutex<DaemonState>>,
    escrow: HandoffEscrow,
    reexec_enabled: bool,
) -> HandoffOutcome {
    let attempt = escrow.manifest.attempt;
    let total = escrow.manifest.sessions.len();

    if attempt_decision(attempt) == AttemptDecision::TerminalFallback {
        let what = format!(
            "received manifest claims attempt {} (≥ 2): the restart machine \
             already failed twice — refusing a third exec",
            attempt,
        );
        eprintln!("cm-daemon: {}", what);
        append_crash_note(attempt, &what, "terminal fallback");
        return terminal_fallback(escrow);
    }

    // The transaction, under the recovery guard. It borrows the
    // escrow (validation candidates are CLOEXEC dups; commit arms on
    // those dups too), so on ANY failure — Err or panic — the escrow
    // is still intact for the ladder below.
    let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
        rehydrate_transaction(state, &escrow, reexec_enabled)
    }));
    let failure: String = match outcome {
        Ok(Ok(stats)) => {
            eprintln!(
                "cm-daemon: re-exec rehydrate committed at attempt {} \
                 ({} adopted, {} tombstoned, {} skipped) — releasing \
                 escrow: closing the original session fds (registry \
                 sessions run on dups), the manifest fd, and the pinned \
                 rollback binary fd {} (no rollback needed past the commit \
                 gate){}",
                attempt,
                stats.adopted,
                stats.tombstoned,
                stats.skipped,
                escrow.rollback_fd.as_raw_fd(),
                match &escrow.tls_listener_fd {
                    Some(fd) => format!(
                        "; closing unadopted TLS listener fd {} (TLS handoff \
                         is out of scope)",
                        fd.as_raw_fd()
                    ),
                    None => String::new(),
                },
            );
            drop(escrow);
            // Phase 4f gap closure (wired at orchestrator integration):
            // re-exec adoption bypasses the `start_session` spawn
            // funnel where the codex rollout watch is normally armed
            // (see `transcript_detect::spawn_codex_rollout_watch`), so
            // arm it here for every adopted codex session — otherwise
            // a compacted codex session's resume key would go silently
            // stale again after a swap, exactly the lineage gap 4f
            // closed for spawned sessions. Pre-serving, the registry
            // holds exactly the sessions this transaction adopted;
            // uids are collected under the lock and the detached
            // watch threads spawn after it drops.
            let codex_uids: Vec<String> = {
                let st = state.lock().unwrap_or_else(|p| p.into_inner());
                st.sessions
                    .iter()
                    .filter(|(_, s)| s.session_type == "codex")
                    .map(|(uid, _)| uid.clone())
                    .collect()
            };
            for uid in codex_uids {
                eprintln!(
                    "cm-daemon: re-arming codex rollout watch for adopted \
                     session '{}'",
                    uid,
                );
                crate::transcript_detect::spawn_codex_rollout_watch(
                    Arc::clone(state),
                    uid,
                );
            }
            return HandoffOutcome::Adopted {
                adopted: stats.adopted,
                tombstoned: stats.tombstoned,
                total,
            };
        }
        Ok(Err(msg)) => msg,
        Err(panic) => format!(
            "PANIC during rehydrate (caught by the recovery guard): {}",
            panic_message(&panic),
        ),
    };

    // Failure. Unbuffered stderr first (we may be about to exec),
    // then disarm anything a mid-commit failure left in the registry
    // WITHOUT signaling, then the ladder.
    eprintln!(
        "cm-daemon: re-exec rehydrate FAILED at attempt {}: {} — children \
         are alive and unsignaled (candidates are non-killing by \
         construction); escrow originals intact",
        attempt, failure,
    );
    disarm_partial_commit(state);

    match on_rehydrate_failure(attempt) {
        FailureNext::RollbackExec { next_attempt } => {
            append_crash_note(
                attempt,
                &failure,
                &format!(
                    "exec pinned rollback binary with attempt {}",
                    next_attempt
                ),
            );
            let exec_err = rollback_exec(&escrow, next_attempt);
            // rollback_exec returns ONLY on failure: fall through to
            // the terminal fallback — never exec-loop blindly.
            let what = format!(
                "rollback execveat itself failed after the attempt-{} \
                 rehydrate failure: {}",
                attempt, exec_err,
            );
            eprintln!("cm-daemon: {}", what);
            append_crash_note(attempt, &what, "terminal fallback");
            terminal_fallback(escrow)
        }
        FailureNext::TerminalFallback => {
            append_crash_note(attempt, &failure, "terminal fallback");
            terminal_fallback(escrow)
        }
    }
}

/// Best-effort text out of a `catch_unwind` payload.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

/// Disarm anything a mid-commit failure left in the registry WITHOUT
/// signaling: drain and `mem::forget` every registered session, so
/// no `DaemonSession::Drop` can SIGKILL a child the ladder is about
/// to hand back to the rollback image (or that the terminal fallback
/// will kill deliberately, on its own identity-verified terms).
///
/// Sound because this runs during pre-serving startup: the accept
/// loop hasn't started, so the ONLY possible registry entries are
/// the ones this handoff's commit phase inserted. The forgotten
/// sessions' reader/reaper threads keep running until the imminent
/// rollback exec destroys them (all threads die at exec); on the
/// terminal path they leak deliberately — their reaper may even race
/// the fallback's reap of the same child (both target the task
/// through pidfds; the loser gets ECHILD, which both handle). Logged
/// per session because a leak, even a deliberate one, must be
/// findable.
fn disarm_partial_commit(state: &Arc<Mutex<DaemonState>>) {
    let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
    if st.sessions.is_empty() {
        return;
    }
    let drained: Vec<(String, DaemonSession)> = st.sessions.drain().collect();
    for (uid, sess) in drained {
        eprintln!(
            "cm-daemon: disarming partially-committed session '{}' without \
             signaling (mem::forget — threads/fds reclaimed at the imminent \
             exec, or deliberately leaked on the terminal path)",
            uid,
        );
        std::mem::forget(sess);
    }
}

// ------------------------------------------------------------
// The rehydrate transaction (validate on dups → commit gate)
// ------------------------------------------------------------

struct AdoptStats {
    adopted: usize,
    /// Records whose child exited during the swap: reaped
    /// (`waitid(P_PIDFD)`) and recorded as exited tombstones (4b).
    tombstoned: usize,
    /// Start-time-mismatch / broken-probe records: unsignaled,
    /// unreaped, untombstoned — unknown identity (4a semantics,
    /// kept).
    skipped: usize,
}

/// Reconstruct a monotonic cell from its manifest age: `anchor -
/// age`. `None` when the age can't be represented (rejected-as-
/// corrupt values never get here — structural validation enforces
/// finite ≥ 0 — but `checked_sub` can still underflow the clock's
/// epoch for an age larger than the boot's monotonic range; honesty
/// over guessing). One `anchor` per transaction, mirroring the write
/// side's one-anchor rule, so cell ORDER is preserved exactly.
fn instant_from_age(anchor: Instant, age_s: f64) -> Option<Instant> {
    anchor.checked_sub(Duration::try_from_secs_f64(age_s).ok()?)
}

/// Project a v2 manifest record into the [`AdoptedSessionMeta`] the
/// build stage consumes: identity verbatim, cells reconstructed
/// against `anchor`, and the kills_dir wired by the same rule
/// `start_session` uses (soft cap present → real kills_dir), so an
/// adopted capped session's cap kill is attributed instead of
/// reading as a plain signal-9 exit.
///
/// `watcher_checkpoint` (phase 4d, R12) is the record's PARSED
/// watcher checkpoint (parsed once per record by the caller — the
/// commit phase reuses the same parse for the watcher respawn):
/// its `kills_baseline` restores the spawn-time kill-log offset so
/// `build_adopted` doesn't recapture a fresh one that would lose a
/// swap-window cap kill's attribution.
fn adopted_meta_from_record(
    rec: &SessionRecord,
    anchor: Instant,
    watcher_checkpoint: Option<&crate::session_watch::WatcherCheckpoint>,
) -> AdoptedSessionMeta {
    let cell =
        |age: Option<f64>| age.and_then(|a| instant_from_age(anchor, a));
    let done_report = rec.done_report.as_ref().and_then(|dr| {
        match instant_from_age(anchor, dr.age_s) {
            Some(at_instant) => Some(ReportedDone {
                at_unix: dr.at_unix,
                at_instant,
                reason: dr.reason.clone(),
            }),
            None => {
                eprintln!(
                    "cm-daemon: handoff session '{}': done_report age {}s \
                     is not reconstructible on this clock — dropping the \
                     marker (logged, never guessed)",
                    rec.uid, dr.age_s,
                );
                None
            }
        }
    });
    AdoptedSessionMeta {
        title: rec.title.clone(),
        session_type: rec.session_type.clone(),
        workspace_id: rec.workspace_id.clone(),
        managed_by_uid: rec.managed_by_uid.clone(),
        task_id: rec.task_id.clone(),
        transcript_path: rec.transcript_path.clone(),
        memory_cap_soft_bytes: rec.memory_cap_soft_bytes,
        memory_cap_hard_bytes: rec.memory_cap_hard_bytes,
        cgroup_prefix: rec.cgroup_prefix.clone().map(PathBuf::from),
        workflow_run_id: rec.workflow_run_id.clone(),
        workflow_role: rec.workflow_role.clone(),
        continuous_task_id: rec.continuous_task_id.clone(),
        global_perms: rec.global_perms,
        generation: rec.generation,
        last_activity_at: cell(rec.last_activity_age_s),
        last_input_at: cell(rec.last_input_age_s),
        last_operator_input_at: cell(rec.last_operator_input_age_s),
        last_turn_end_at: cell(rec.last_turn_end_age_s),
        done_report,
        kills_dir: if rec.memory_cap_soft_bytes.is_some() {
            crate::path::default_kills_dir()
        } else {
            None
        },
        kills_baseline: watcher_checkpoint.map(|cp| cp.kills_baseline),
    }
}

/// Re-arm the memory-cap watcher for an adopted CAPPED session —
/// DESIGN_SEAMLESS_RESTART phase 4d (R12), closing audit gap 1
/// (pre-4d, `arm` left `watcher_handle: None` and nothing on the
/// adoption path called `spawn_watcher`: the cgroup scope survived
/// the swap but nothing watched `memory.events` — the cap was
/// silently UNENFORCED).
///
/// Two arms, checkpoint-first:
///
/// - **Checkpointed** (the normal swap): the watcher is seeded FROM
///   the checkpoint — watched cgroup path as discovered by the OLD
///   image, `last_high` watermark (a breach that landed mid-swap
///   fires on the first poll instead of being re-baselined away),
///   and the finalized protected set — `(pid, starttime)` pairs
///   since checkpoint v2 (S13), verified both-halves on use —
///   (stabilize/followup SKIPPED — recomputing from the current
///   cgroup would newly protect late workers the pre-swap policy
///   left killable). A checkpoint taken
///   mid-stabilize carries `protected: None`, and the restored
///   watcher re-runs the phases — what the old one would have done.
/// - **No checkpoint** (pre-4d manifest, parse failure — already
///   logged by `parse_watcher_checkpoint` — or a capped session
///   whose watcher never spawned): degrade LOUDLY to a fresh
///   watcher — re-discover the cgroup from the live child's
///   `/proc/<pid>/cgroup` (the same never-trust-a-stored-path rule
///   as `start_session`) and re-seed the watermark from the current
///   counter. A policy reset, logged as one; strictly better than
///   no watcher.
///
/// **Never fails the adoption.** Every degradation — no kills dir,
/// discovery failure, thread-spawn failure — logs the cap as
/// UNENFORCED and returns `None`: trading a live adopted session (or
/// the whole transaction) for its watcher would invert the design's
/// priorities — the cap can be re-imposed; the child cannot. (This
/// deliberately diverges from `start_session`, which refuses to
/// RETURN a new capped session without a producer — there the child
/// is ours to abort; here it predates us.)
///
/// Runs under the commit phase's state-lock hold: `spawn_watcher`
/// only creates the cell and spawns a thread (same weight as `arm`'s
/// spawns beside it); the watcher registers its safe point on its
/// OWN thread, so no coordinator lock is taken under the state lock.
fn respawn_adopted_watcher(
    rec: &SessionRecord,
    checkpoint: Option<&crate::session_watch::WatcherCheckpoint>,
    coordinator: std::sync::Weak<crate::restart_coordinator::RestartCoordinator>,
) -> Option<crate::session_watch::SpawnedWatcher> {
    let soft_cap_bytes = rec.memory_cap_soft_bytes?;
    let hard_cap_bytes = crate::control::methods::resolve_watcher_hard_cap_bytes(
        rec.memory_cap_hard_bytes,
    );
    let Some(kills_dir) = crate::path::default_kills_dir() else {
        eprintln!(
            "cm-daemon: adopted capped session '{}': no kills dir resolves \
             on this host — memory cap UNENFORCED post-swap (no watcher, \
             no kill-log producer); adoption proceeds",
            rec.uid,
        );
        return None;
    };
    let (cgroup_path, initial_high, restored_protected) = match checkpoint {
        Some(cp) => (
            PathBuf::from(&cp.cgroup_path),
            cp.last_high,
            // (pid, starttime) pairs since checkpoint v2 (S13) —
            // carried through unchanged; the watcher's membership
            // check verifies both halves on use.
            cp.protected
                .as_ref()
                .map(|v| v.iter().copied().collect::<HashSet<(u32, u64)>>()),
        ),
        None => {
            eprintln!(
                "cm-daemon: adopted capped session '{}': no usable watcher \
                 checkpoint — POLICY RESET: fresh watcher (cgroup \
                 re-discovered from /proc/{}/cgroup, protected set \
                 recomputed, breach watermark re-anchored at the current \
                 counter; a breach that landed during the swap is \
                 unattributable)",
                rec.uid, rec.child_pid,
            );
            let discovered = match crate::path::discover_session_cgroup_path(
                rec.child_pid as u32,
                Duration::from_millis(500),
            ) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "cm-daemon: adopted capped session '{}': cgroup \
                         re-discovery failed ({}) — memory cap UNENFORCED \
                         post-swap; adoption proceeds",
                        rec.uid, e,
                    );
                    return None;
                }
            };
            let high =
                crate::session_watch::read_memory_events_high(&discovered);
            (discovered, high, None)
        }
    };
    match crate::session_watch::spawn_watcher(
        rec.uid.clone(),
        cgroup_path.clone(),
        soft_cap_bytes,
        hard_cap_bytes,
        kills_dir,
        initial_high,
        crate::session_watch::default_watcher_spawn_fn(),
        Some(coordinator),
        restored_protected.clone(),
    ) {
        Ok(w) => {
            eprintln!(
                "cm-daemon: adopted capped session '{}': memory-cap watcher \
                 re-armed on {} ({}; last_high {}, protected {})",
                rec.uid,
                cgroup_path.display(),
                if checkpoint.is_some() {
                    "restored from checkpoint"
                } else {
                    "fresh policy"
                },
                initial_high,
                match &restored_protected {
                    Some(set) => {
                        format!("{} (pid, starttime) pair(s) restored", set.len())
                    }
                    None => "to be computed".to_string(),
                },
            );
            Some(w)
        }
        Err(e) => {
            eprintln!(
                "cm-daemon: adopted capped session '{}': watcher thread \
                 spawn failed ({}) — memory cap UNENFORCED post-swap; \
                 adoption proceeds (never trade a live child for its \
                 watcher)",
                rec.uid, e,
            );
            None
        }
    }
}

/// Build the exited tombstone for a record whose child exited during
/// the swap — the phase-4b honest-tombstone path, mirroring the
/// provenance shape `handle_session_exit` records for live-session
/// exits. Deliberately NOT a kill: `killed: false, killed_by: None`
/// — nothing signaled this child; it exited on its own while the
/// daemon was between images (the log line is what says "during the
/// re-exec swap"). The done-report marker carries over under the
/// same superseded rule the live cell derives (input newer than the
/// report supersedes it — strict, matching
/// `DaemonSession::reported_done`).
fn swap_exit_tombstone(
    rec: &SessionRecord,
    worktree_path: Option<String>,
    exited_at: f64,
) -> crate::state::ExitedTombstone {
    let report = rec.done_report.as_ref().filter(|dr| {
        match rec.last_input_age_s {
            // input_age < done age ⇔ input is NEWER than the report
            // ⇔ superseded (strict, like the live `>` comparison).
            Some(input_age) => input_age >= dr.age_s,
            None => true,
        }
    });
    crate::state::ExitedTombstone {
        session_uid: rec.uid.clone(),
        transcript_path: rec.transcript_path.clone(),
        generation: rec.generation,
        session_type: rec.session_type.clone(),
        workspace_id: rec.workspace_id.clone(),
        task_id: rec.task_id.clone(),
        managed_by_uid: rec.managed_by_uid.clone(),
        label: rec.title.clone(),
        workflow_run_id: rec.workflow_run_id.clone(),
        workflow_role: rec.workflow_role.clone(),
        worktree_path,
        global_perms: rec.global_perms,
        exited_at,
        killed: false,
        killed_by: None,
        reported_done_at: report.map(|r| r.at_unix),
        report_reason: report.and_then(|r| r.reason.clone()),
    }
}

/// The continuous run-close flip for a record whose child exited
/// during the swap — DESIGN_SEAMLESS_RESTART phase 4g (audit gap 7).
///
/// A live continuous tick's exit reaches
/// `control::methods::handle_session_exit`, which flips the task's
/// ACTIVE run `Running → Done/Failed`; the swap-dead path records its
/// tombstone directly (above), bypassing that callback — pre-4g the
/// run stayed `Running` until the scheduler's `reconcile_orphans`
/// misclassified the outcome `Orphaned`, and
/// `consecutive_wedge_closes` was never reset. Apply the SAME flip
/// here, through the shared
/// [`crate::control::methods::close_continuous_run_for_exit`] (whose
/// docs carry the double-guard and lock-safety arguments; its
/// per-task flock IO is safe under the commit's pre-serving state-lock
/// hold for the same reason it is under the reaper's).
///
/// Terminal-status classification mirrors `handle_session_exit`'s:
/// a non-zero exit code is a genuine FAILURE; a clean exit (code 0)
/// or a bare signal (code None) finishes the run `Done` — matching
/// the live path's treatment of signal kills, and honest for the swap
/// window, where a memory-cap kill cannot be attributed anyway (the
/// classification needs gap 5's operator-kill flag; see the
/// `memory_cap_kill` rationale at the tombstone site in the commit
/// phase).
fn swap_dead_continuous_close(
    rec: &SessionRecord,
    status: &crate::session::DaemonExitStatus,
) {
    let Some(ct_id) = rec.continuous_task_id.as_deref() else {
        return;
    };
    let terminal = if matches!(status.code, Some(c) if c != 0) {
        crate::continuous::task::RunStatus::Failed
    } else {
        crate::continuous::task::RunStatus::Done
    };
    eprintln!(
        "cm-daemon: handoff session '{}' was continuous task '{}''s tick — \
         closing its run {:?} (exited during the swap; exit code {:?}, \
         signal {:?})",
        rec.uid, ct_id, terminal, status.code, status.signal,
    );
    crate::control::methods::close_continuous_run_for_exit(
        ct_id,
        &rec.uid,
        terminal,
        format!(
            "session exited during the re-exec swap (exit code {:?}, signal \
             {:?}) — run closed by the handoff tombstone path",
            status.code, status.signal
        ),
    );
}

/// The escrow/commit-gate transaction (design step 6, R5/R6; phase
/// 4b restructured it into validate → build-all → reap-dead →
/// commit).
///
/// **Validation phase** — no promotion, nothing armed, nothing
/// signaled: every record gets a [`SessionCandidate`] built from
/// CLOEXEC **dups** (`OwnedFd::try_clone`) of its escrowed fds, then
/// the non-consuming probes run: pidfd liveness and the R6
/// start-time cross-check. A record whose child EXITED during the
/// swap is set aside for the commit phase's reap+tombstone (4b);
/// one whose start-time mismatches or whose probe breaks is SKIPPED
/// — logged, never signaled, never reaped, never tombstoned
/// (unknown identity — 4a semantics, kept). Unexpected errors fail
/// the transaction with `Err`: candidates drop (dups close), and
/// the caller's escrow originals are untouched for the rollback
/// path.
///
/// The failure-injection knob ([`ENV_TEST_FAIL_REHYDRATE`], dev-flag
/// gated) fires HERE — after validation, before anything is built,
/// reaped, or committed — so the e2e drives the real rollback path
/// through a fully-intact escrow (zombies included).
///
/// **Build phase** (4b — closes the P4a byte-loss corner): every
/// survivor is promoted and run through
/// [`DaemonSession::build_adopted`] — all the realistically-fallible
/// construction (reader clone, poll-fd dup, `take_writer`, reaper
/// pidfd dup, kills-baseline capture) with **no thread started**. A
/// failure here drops the builds (non-killing, dups close) and
/// rolls back with ZERO bytes consumed — pre-4b, earlier records'
/// readers were already draining kernel-buffered PTY bytes into
/// fanout rings that die at the rollback exec.
///
/// **Reap phase** (4b): each swap-dead record's zombie is reaped via
/// `waitid(P_PIDFD)` through its candidate's dup'd pidfd, under a
/// reap-gate read permit (taken BEFORE the state lock, preserving
/// the process-wide gate → state order). Past this point the
/// transaction is morally committed — the only remaining failures
/// are the arm stage's thread spawns.
///
/// **Commit phase** — the gate: under ONE continuous state-lock hold
/// (the `start_session` fast-exit discipline: a reaper's `on_exit`
/// blocks on this lock, so every insert is visible before any exit
/// callback can run): tombstone the reaped dead records (mirroring
/// `handle_session_exit`'s provenance shape — an exit during the
/// swap, not a kill — including its continuous run-close flip for
/// dead records carrying a `continuous_task_id`; phase 4g, audit gap
/// 7, see [`swap_dead_continuous_close`]), then arm
/// ([`AdoptedSessionBuild::arm`]) and insert each survivor. Kill-on-drop begins at arm — that is the
/// commit. A thread-spawn failure mid-arm still disarms the
/// already-inserted sessions inside the same lock hold (drain +
/// `mem::forget` — never Drop, which SIGKILLs) and returns `Err`
/// with escrow intact; that vanishing residual (already-armed
/// readers may have drained bytes) is the one corner the build/arm
/// split cannot close in-process.
///
/// **Why commit on the dups** (the design offered two honest
/// shapes): arming on the ORIGINALS would consume escrow record by
/// record, so a failure on session N+1 would leave a rollback with a
/// partially-consumed escrow — exactly what R5 forbids. Arming on
/// the dups keeps the originals intact in escrow until the WHOLE
/// transaction has succeeded (a PTY dup shares the open file
/// description, so the armed session's handles are functionally
/// identical); the caller then closes the redundant originals in one
/// place. Single-ownership stays honest: originals never leave
/// escrow, dups are owned by candidate → parts → build → armed
/// session.
fn rehydrate_transaction(
    state_arc: &Arc<Mutex<DaemonState>>,
    escrow: &HandoffEscrow,
    reexec_enabled: bool,
) -> Result<AdoptStats, String> {
    let manifest = &escrow.manifest;
    // ONE reconstruction anchor for every age in this manifest —
    // the read-side twin of build_manifest's single capture anchor,
    // which is what keeps cell ORDER exact across the swap.
    let anchor = Instant::now();

    // ---- Validation phase (dups only, no promotion) ----
    let mut seen_uids: HashSet<&str> = HashSet::new();
    let mut survivors: Vec<(usize, SessionCandidate)> = Vec::new();
    let mut dead: Vec<(usize, SessionCandidate)> = Vec::new();
    let mut skipped = 0usize;
    for (i, rec) in manifest.sessions.iter().enumerate() {
        if !seen_uids.insert(rec.uid.as_str()) {
            return Err(format!(
                "manifest carries duplicate session uid '{}' — corrupt \
                 manifest shape; failing the transaction",
                rec.uid,
            ));
        }
        let fds = &escrow.sessions[i];
        let pidfd_dup = fds.pidfd.try_clone().map_err(|e| {
            format!(
                "dup escrowed pidfd for session '{}' (fd {}): {}",
                rec.uid,
                fds.pidfd.as_raw_fd(),
                e
            )
        })?;
        let master_dup = fds.master.try_clone().map_err(|e| {
            format!(
                "dup escrowed PTY master for session '{}' (fd {}): {}",
                rec.uid,
                fds.master.as_raw_fd(),
                e
            )
        })?;
        let candidate = SessionCandidate::from_raw_parts(
            rec.uid.clone(),
            rec.child_pid,
            pidfd_dup,
            master_dup,
        );

        // Probe 1: liveness (non-consuming pidfd poll). An exited
        // child parks as an unreaped zombie — set aside; the COMMIT
        // phase reaps + tombstones it (4b), so a pre-commit failure
        // still rolls back with the zombie intact for the rollback
        // image's own honest probe.
        match candidate.child_alive() {
            Ok(true) => {}
            Ok(false) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): child exited \
                     during the swap — will reap + tombstone at commit \
                     (never adopted, never signaled)",
                    rec.uid, rec.child_pid,
                );
                dead.push((i, candidate));
                continue;
            }
            Err(e) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): pidfd probe \
                     failed ({}) — skipping adoption, never signaling",
                    rec.uid, rec.child_pid, e,
                );
                skipped += 1;
                continue;
            }
        }
        // Probe 2: R6 identity cross-check — the recorded spawn-time
        // starttime must match the live read. The pidfd already makes
        // signaling pid-reuse-proof; this catches a corrupt/mismatched
        // record before we build anything around it. Mismatches stay
        // exactly as 4a left them: unsignaled, unreaped, untombstoned
        // — never fabricate provenance for an unknown identity.
        match candidate.child_start_time() {
            Ok(t) if t == rec.child_start_time => {}
            Ok(t) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): start-time \
                     mismatch (manifest {}, /proc {}) — skipping adoption, \
                     never signaling",
                    rec.uid, rec.child_pid, rec.child_start_time, t,
                );
                skipped += 1;
                continue;
            }
            Err(e) => {
                eprintln!(
                    "cm-daemon: handoff session '{}' (pid {}): start-time \
                     read failed ({}) — skipping adoption, never signaling",
                    rec.uid, rec.child_pid, e,
                );
                skipped += 1;
                continue;
            }
        }
        survivors.push((i, candidate));
    }

    // ---- Failure injection (dev-flag gated; see the const doc) ----
    if let Some(reason) = test_fail_requested(reexec_enabled, manifest.attempt)
    {
        eprintln!("cm-daemon: {}", reason);
        return Err(reason);
    }

    // ---- Build phase (4b): everything fallible, zero threads. ----
    // The watcher checkpoint (phase 4d, R12) is parsed ONCE here per
    // record — a parse failure logs the policy reset (see
    // `session_watch::parse_watcher_checkpoint`) and degrades that
    // record to fresh watcher policy, NEVER failing the transaction —
    // and the parsed value is threaded to the commit phase's watcher
    // respawn so the parse (and its logging) never runs twice.
    let mut builds: Vec<(
        usize,
        AdoptedSessionBuild,
        Option<crate::session_watch::WatcherCheckpoint>,
    )> = Vec::with_capacity(survivors.len());
    for (i, candidate) in survivors {
        let rec = &manifest.sessions[i];
        let checkpoint = rec.watcher_checkpoint.as_ref().and_then(|v| {
            crate::session_watch::parse_watcher_checkpoint(&rec.uid, v)
        });
        let meta = adopted_meta_from_record(rec, anchor, checkpoint.as_ref());
        let build = DaemonSession::build_adopted(candidate.promote(), meta)
            .map_err(|e| {
                format!(
                    "build_adopted failed for session '{}': {} — \
                     transaction aborted before any thread started \
                     (escrow originals intact, nothing consumed)",
                    rec.uid, e,
                )
            })?;
        builds.push((i, build, checkpoint));
    }

    // ---- Reap phase (4b): consume the swap-dead zombies. ----
    // Permit taken and released BEFORE the state lock below — the
    // process-wide lock order is gate → state (see the module docs'
    // ordering argument), and holding the state lock while blocking
    // on the gate would invert it. waitid(P_PIDFD) through the
    // candidate's dup'd pidfd targets the fd's bound task only, so
    // the numeric pid is diagnostic, never an identity source.
    let mut reaped: Vec<(usize, crate::session::DaemonExitStatus)> =
        Vec::with_capacity(dead.len());
    if !dead.is_empty() {
        let _permit = reap_gate::read_permit();
        for (i, candidate) in dead {
            let rec = &manifest.sessions[i];
            let status = crate::session::consume_exit_status(
                candidate.pidfd(),
                rec.child_pid,
            );
            eprintln!(
                "cm-daemon: handoff session '{}' (pid {}): exited during \
                 the re-exec swap — reaped (exit code {:?}, signal {:?}); \
                 tombstone lands at commit",
                rec.uid, rec.child_pid, status.code, status.signal,
            );
            reaped.push((i, status));
            // candidate drops here: closes this record's dup'd pidfd
            // + master; the escrow originals close with the escrow
            // when the caller releases it post-commit.
        }
    }

    // ---- Commit phase (the gate) ----
    let mut st = state_arc.lock().unwrap_or_else(|p| p.into_inner());

    // Registry-collision pre-check across the WHOLE batch before
    // anything is inserted. Impossible pre-serving (the registry
    // starts empty and only this transaction inserts) — a collision
    // means a program bug or a hostile manifest; nothing to disarm
    // because nothing has been inserted yet.
    for (i, _, _) in &builds {
        let uid = &manifest.sessions[*i].uid;
        if st.sessions.contains_key(uid) {
            return Err(format!(
                "session uid '{}' already present in the registry at commit \
                 — refusing a duplicate adoption",
                uid,
            ));
        }
    }

    // Tombstone the reaped dead records — the same recently-exited
    // state `handle_session_exit` records for live exits, so
    // `list_sessions(include_exited)` / read-after-exit answer for
    // them post-swap. No `manifest.watch` broadcast: pre-serving,
    // no subscriber can exist yet, and attach clients treat the
    // generation bump as a new stream anyway. `killed: false` —
    // provenance is an exit during the swap, not a kill.
    let exited_at = crate::control::methods::now_unix_f64();
    let tombstoned = reaped.len();
    for (i, status) in reaped {
        let rec = &manifest.sessions[i];
        let worktree_path = st
            .workspaces
            .get(&rec.workspace_id)
            .and_then(|w| w.worktree_path.as_ref())
            .map(|p| p.to_string_lossy().into_owned());
        // Mirror handle_session_exit's workspace-entry mutation so
        // the TUI's manifest view agrees with the tombstone.
        // `memory_cap_kill: false` stays the default for SWAP-DEAD
        // records even now that 4d carries the kill-log baseline in
        // the watcher checkpoint: classifying honestly also needs the
        // operator-kill flag (`is_cap_kill`'s conservative override),
        // which is exactly gap 5's unserialized in-flight kill
        // attribution — probing with `operator_kill: false` would
        // bias an operator kill landing just before the swap into a
        // false "memory-cap" label, the misattribution watcher-fix
        // #4/#5 exist to prevent. Wired together with gap 5.
        if let Some(mw) = st.workspaces.get_mut(&rec.workspace_id) {
            if let Some(entry) =
                mw.sessions.iter_mut().find(|e| e.uid == rec.uid)
            {
                entry.last_exit = Some(crate::manifest::LastExit {
                    code: status.code,
                    memory_cap_kill: false,
                    kills_file_offset: None,
                    exited_at,
                });
            }
        }
        st.record_exited(swap_exit_tombstone(rec, worktree_path, exited_at));
        // Phase 4g (audit gap 7): a swap-dead CONTINUOUS tick gets the
        // same run-close flip `handle_session_exit` gives a live exit,
        // instead of leaving its run `Running` for `reconcile_orphans`
        // to misclassify `Orphaned`.
        swap_dead_continuous_close(rec, &status);
        eprintln!(
            "cm-daemon: handoff session '{}' tombstoned (exited during the \
             re-exec swap; exit code {:?}, signal {:?}, reaped — no zombie)",
            rec.uid, status.code, status.signal,
        );
    }

    // Arm + insert, all under this one continuous lock hold.
    // Phase 4d (R12): the coordinator handle for the re-armed
    // watchers' safe-point registration — the NEW image's coordinator
    // (born fresh, no pause in effect), downgraded so a watcher can
    // never keep daemon state alive.
    let coordinator_weak = Arc::downgrade(&st.restart_coordinator);
    let mut adopted = 0usize;
    for (i, build, checkpoint) in builds {
        let rec = &manifest.sessions[i];
        let state_for_cleanup = Arc::clone(state_arc);
        let uid_for_cleanup = rec.uid.clone();
        let on_exit: crate::session::OnExitCallback = Box::new(move |_status| {
            let mut s = state_for_cleanup
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            crate::control::methods::handle_session_exit(&mut s, &uid_for_cleanup);
        });
        match build.arm(Some(on_exit), crate::session::ExitAuthority::Reap) {
            Ok(mut sess) => {
                // Phase 4d (R12): re-arm the memory-cap watcher for a
                // capped record, seeded from the checkpoint (never
                // recomputed when one exists). Mirrors
                // `start_session`'s two-step stash. Runs AFTER arm
                // succeeded and never fails the adoption — see
                // `respawn_adopted_watcher`.
                if rec.memory_cap_soft_bytes.is_some() {
                    if let Some(w) = respawn_adopted_watcher(
                        rec,
                        checkpoint.as_ref(),
                        coordinator_weak.clone(),
                    ) {
                        sess.watcher_handle = Some(w.handle);
                        sess.watcher_state = Some(w.state);
                    }
                }
                st.sessions.insert(rec.uid.clone(), sess);
                adopted += 1;
                eprintln!(
                    "cm-daemon: handoff session '{}' adopted (pid {}, \
                     type {}, generation {}, task {:?}, global_perms {})",
                    rec.uid,
                    rec.child_pid,
                    rec.session_type,
                    rec.generation,
                    rec.task_id,
                    rec.global_perms,
                );
            }
            Err(e) => {
                // Arm (thread-spawn) failure — vanishingly rare:
                // disarm the sessions already inserted INSIDE this
                // same lock hold (their on_exit callbacks stay
                // blocked on the lock until the registry is clean),
                // then fail the transaction. The failed record's
                // DUPS died with its build; the escrow ORIGINALS are
                // intact for the rollback.
                disarm_registry_locked(&mut st);
                return Err(format!(
                    "arm failed at commit for session '{}': {} — \
                     transaction aborted (escrow originals intact)",
                    rec.uid, e,
                ));
            }
        }
    }
    // Phase 6 (schema v3): the handoff COMMITTED — stamp the
    // manifest's lineage counter onto this image's state, where
    // `daemon.health` serves it as `reexec_generation`. Inside the
    // same lock hold as the arms above, so no health poll can observe
    // adopted sessions with a stale generation. Every failure path
    // returned before this line, and the terminal fallback never runs
    // this transaction — a fallback boot stays generation 0 (it IS a
    // fresh boot: children killed, legacy restore).
    st.reexec_generation = manifest.reexec_generation;

    Ok(AdoptStats {
        adopted,
        tombstoned,
        skipped,
    })
}

/// [`disarm_partial_commit`]'s under-the-lock twin, for failure paths
/// that already hold the state guard (keeps the reaper `on_exit`
/// callbacks blocked until the registry is clean).
fn disarm_registry_locked(st: &mut DaemonState) {
    let drained: Vec<(String, DaemonSession)> = st.sessions.drain().collect();
    for (uid, sess) in drained {
        eprintln!(
            "cm-daemon: disarming partially-committed session '{}' without \
             signaling (mem::forget)",
            uid,
        );
        std::mem::forget(sess);
    }
}

// ------------------------------------------------------------
// Crash note
// ------------------------------------------------------------

/// Append one unbuffered line to `$HOME/.cm/reexec-crash-note.log` —
/// the durable record of a ladder transition (the daemon's stderr may
/// be a pipe nobody kept). `File::write_all` is a direct `write(2)`
/// (no BufWriter anywhere), so the line is out before the exec / the
/// fallback proceeds. Best-effort: a note that can't be written must
/// never block recovery — the same text also goes to stderr.
fn append_crash_note(attempt: u8, what: &str, action: &str) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!(
        "[{}] cm-daemon re-exec attempt {} failed: {} — action: {}\n",
        ts, attempt, what, action,
    );
    eprint!("cm-daemon: crash note: {}", line);
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let dir = home.join(".cm");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(CRASH_NOTE_FILENAME);
    match std::fs::OpenOptions::new().append(true).create(true).open(&path) {
        Ok(mut f) => {
            let _ = f.write_all(line.as_bytes());
        }
        Err(e) => eprintln!(
            "cm-daemon: could not append crash note to {}: {}",
            path.display(),
            e,
        ),
    }
}

// ------------------------------------------------------------
// Rollback exec
// ------------------------------------------------------------

/// Exec the pinned rollback binary (R7: an inode held since the
/// original restart's step 1, immune to deploys overwriting the
/// path) with the escrow re-manifested at `next_attempt`. Returns
/// ONLY on failure — success is the exec, and the rollback image
/// rehydrates the same escrow fds through the normal handoff path.
///
/// The steps, mirroring the old image's exec side:
///
/// 1. **New sealed manifest** — the old one is sealed/immutable by
///    design (R8), so a fresh memfd re-lists the ORIGINAL escrow fd
///    NUMBERS (escrow holds those exact fds — R5) with `attempt`
///    bumped and the SAME rollback fd. `write_manifest` re-runs the
///    full structural validation, so a corrupt re-list fails here,
///    before any point of no return.
/// 2. **CLOEXEC discipline (R9)** — audit the whole table to CLOEXEC
///    (nothing can fork here: pre-serving startup, poller/scheduler
///    not yet running), then clear the flag on exactly the handed-off
///    set: escrow session fds, the rollback fd, the listener (owned
///    by `run()`'s `UnixListener`; we flip only its flag), the TLS
///    listener when present, and the NEW manifest fd. No
///    snapshot/restore here, unlike the old image's abort path: both
///    continuations from a failure (terminal fallback) want
///    everything CLOEXEC anyway — the failure path below re-sets the
///    flag on the allowlist (the escrow fds are about to close, the
///    listener must go back to CLOEXEC).
/// 3. **Signal bracketing (R13/F8)** — block SIGHUP/SIGTERM; the mask
///    survives the exec, dispositions don't, and SIGHUP's default is
///    terminate. Restored on the failure path (run()'s unblock
///    already ran, so nothing downstream would).
/// 4. **`execveat(rollback_fd, "", AT_EMPTY_PATH)`** via the shared
///    [`do_execveat`] body — same explicit-envp discipline as the
///    forward exec (strips `CM_REEXEC_*`, injects the new manifest
///    pointer; the dev flag and the test knob deliberately survive).
fn rollback_exec(escrow: &HandoffEscrow, next_attempt: u8) -> anyhow::Error {
    let m = &escrow.manifest;

    // ---- 1: new sealed manifest over the ORIGINAL fd numbers. ----
    let new_manifest = ReexecManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        attempt: next_attempt,
        // Phase 6: the generation rides UNCHANGED through a rollback —
        // it counts committed handoffs, not execs, and this restart
        // attempt (however many execs its ladder takes) is one
        // generation. The rollback image's commit stamps the same
        // value the forward image would have.
        reexec_generation: m.reexec_generation,
        rollback_bin_fd: escrow.rollback_fd.as_raw_fd(),
        sessions: m.sessions.clone(),
        listener_fd: m.listener_fd,
        tls_listener_fd: m.tls_listener_fd,
    };
    let new_manifest_fd = match reexec_manifest::write_manifest(&new_manifest) {
        Ok(fd) => fd,
        Err(e) => {
            return anyhow::anyhow!(
                "write rollback manifest memfd (attempt {}): {}",
                next_attempt,
                e
            )
        }
    };

    // ---- 2: CLOEXEC discipline. ----
    if let Err(e) = set_all_cloexec() {
        return anyhow::anyhow!(
            "CLOEXEC audit before rollback exec (close_range / fd walk): {}",
            e
        );
    }
    let mut inherit: Vec<RawFd> = vec![
        new_manifest_fd.as_raw_fd(),
        escrow.rollback_fd.as_raw_fd(),
        m.listener_fd,
    ];
    if let Some(fd) = m.tls_listener_fd {
        inherit.push(fd);
    }
    for s in &escrow.sessions {
        inherit.push(s.master.as_raw_fd());
        inherit.push(s.pidfd.as_raw_fd());
    }
    for &fd in &inherit {
        if let Err(e) = set_fd_flags(fd, 0) {
            // Undo what we cleared so far (the audit direction —
            // everything CLOEXEC — is the safe direction to leave the
            // table in for the terminal fallback).
            for &back in &inherit {
                let _ = set_fd_flags(back, libc::FD_CLOEXEC);
            }
            return anyhow::anyhow!(
                "clear CLOEXEC on handed-off fd {} for rollback exec: {}",
                fd,
                e
            );
        }
    }

    // ---- 3: signal bracketing. ----
    let old_mask = match block_sighup_sigterm() {
        Ok(mask) => mask,
        Err(e) => {
            for &back in &inherit {
                let _ = set_fd_flags(back, libc::FD_CLOEXEC);
            }
            return e;
        }
    };

    // argv[0] hygiene: the rollback inode's recorded pathname (may
    // carry a " (deleted)" suffix after a deploy overwrote the path —
    // honest, and cosmetic only), falling back to our own argv[0].
    let argv0: PathBuf = std::fs::read_link(format!(
        "/proc/self/fd/{}",
        escrow.rollback_fd.as_raw_fd()
    ))
    .unwrap_or_else(|_| {
        std::env::args_os()
            .next()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("cm-daemon"))
    });

    // Unbuffered logging at the exec point (design requirement).
    eprintln!(
        "cm-daemon: ROLLBACK EXEC — execveat'ing pinned rollback fd {} ({}) \
         with attempt {}, {} session(s), listener fd {}, new manifest fd {}",
        escrow.rollback_fd.as_raw_fd(),
        argv0.display(),
        next_attempt,
        m.sessions.len(),
        m.listener_fd,
        new_manifest_fd.as_raw_fd(),
    );

    let err = do_execveat(&escrow.rollback_fd, &argv0, new_manifest_fd.as_raw_fd());

    // ---- execveat returned — failure. Restore for the terminal path. ----
    eprintln!(
        "cm-daemon: ROLLBACK EXEC FAILED ({}) — falling through to the \
         terminal fallback (never exec-loop blindly)",
        err,
    );
    restore_sigmask(&old_mask);
    for &fd in &inherit {
        let _ = set_fd_flags(fd, libc::FD_CLOEXEC);
    }
    // new_manifest_fd drops here (its memfd dies with this scope).
    err
}

// ------------------------------------------------------------
// Terminal fallback
// ------------------------------------------------------------

/// The deliberate-kill terminal fallback (design → "Failure classes":
/// attempt ≥ 2, or a rollback exec that itself failed). For every
/// manifest-listed session: verify identity via the escrowed pidfd +
/// the R6 start-time cross-check, then `pidfd_send_signal(SIGKILL)`
/// and reap via `waitid(P_PIDFD)` — kill-then-reap, logged per
/// session with uid + pid. Mismatched records are logged and left
/// unsignaled; already-dead ones are reaped (zombie) or noted
/// (already reaped) without signaling. Then every escrow fd closes —
/// session fds, manifest fd, rollback fd, TLS fd — and this RETURNS,
/// so `run()` proceeds with NORMAL startup semantics.
///
/// **The listener survives on purpose**: it was never escrowed —
/// `run()` adopted the inherited listener at startup (the socket
/// never unbound), so the daemon comes back SERVING through it, and
/// `run()`'s wiring follows this return with legacy
/// `restore_sessions` (resume from the H1-pinned stamps) INSTEAD of
/// rehydrate. Sessions are killed deliberately, then resumed — never
/// abandoned alive: falling through with live orphans while restore
/// spawns `--resume` duplicates would be two live writers per
/// conversation, the 2026-08-18 split-brain class (the review finding
/// that turned the original spec's "sessions may die" into this
/// function).
fn terminal_fallback(escrow: HandoffEscrow) -> HandoffOutcome {
    let HandoffEscrow {
        manifest,
        manifest_fd,
        rollback_fd,
        tls_listener_fd,
        sessions,
    } = escrow;
    let total = manifest.sessions.len();
    eprintln!(
        "cm-daemon: TERMINAL FALLBACK — deliberately killing and reaping \
         {} manifest-listed session child(ren) before legacy restore; the \
         inherited control listener stays adopted, so the daemon comes back \
         serving",
        total,
    );

    let mut killed = 0usize;
    for (rec, fds) in manifest.sessions.iter().zip(sessions) {
        // A candidate over the ORIGINALS: its probes are the same
        // non-consuming pidfd poll + /proc starttime read the
        // transaction uses, and its end-of-iteration drop is the
        // close-everything step this path wants anyway.
        let candidate = SessionCandidate::from_raw_parts(
            rec.uid.clone(),
            rec.child_pid,
            fds.pidfd,
            fds.master,
        );
        match candidate.child_alive() {
            Ok(true) => {
                match candidate.child_start_time() {
                    Ok(t) if t == rec.child_start_time => {
                        // Identity verified: deliberate kill-then-reap
                        // through the escrowed pidfd (pid-reuse-proof
                        // by construction; the starttime check catches
                        // a corrupt record, not a recycled pid).
                        if let Err(e) = crate::session::send_sigkill_via_pidfd(
                            candidate.pidfd(),
                        ) {
                            eprintln!(
                                "cm-daemon: terminal fallback: SIGKILL via \
                                 pidfd failed for session '{}' (pid {}): {} \
                                 — NOT reaping (a reap would block on a live \
                                 child)",
                                rec.uid, rec.child_pid, e,
                            );
                            continue;
                        }
                        // Reap under the gate discipline every other
                        // status consumer follows. Blocks only for the
                        // SIGKILL to land (unblockable, prompt).
                        let _permit = reap_gate::read_permit();
                        let status = crate::session::consume_exit_status(
                            candidate.pidfd(),
                            rec.child_pid,
                        );
                        killed += 1;
                        eprintln!(
                            "cm-daemon: terminal fallback: deliberately \
                             SIGKILLed and reaped session '{}' child pid {} \
                             (exit code {:?}, signal {:?})",
                            rec.uid, rec.child_pid, status.code, status.signal,
                        );
                    }
                    Ok(t) => {
                        eprintln!(
                            "cm-daemon: terminal fallback: session '{}' (pid \
                             {}) start-time mismatch (manifest {}, /proc {}) \
                             — corrupt record; left UNSIGNALED",
                            rec.uid, rec.child_pid, rec.child_start_time, t,
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "cm-daemon: terminal fallback: session '{}' (pid \
                             {}) start-time read failed ({}) — left \
                             UNSIGNALED",
                            rec.uid, rec.child_pid, e,
                        );
                    }
                }
            }
            Ok(false) => {
                // Already exited during the swap: reap the zombie so
                // nothing lingers (waitid(P_PIDFD) targets the bound
                // task only; if something else already consumed the
                // status it answers ECHILD, decoded as unknown).
                let _permit = reap_gate::read_permit();
                let status = crate::session::consume_exit_status(
                    candidate.pidfd(),
                    rec.child_pid,
                );
                eprintln!(
                    "cm-daemon: terminal fallback: session '{}' child pid {} \
                     already exited — reaped without signaling (exit code \
                     {:?}, signal {:?})",
                    rec.uid, rec.child_pid, status.code, status.signal,
                );
            }
            Err(e) => {
                eprintln!(
                    "cm-daemon: terminal fallback: session '{}' (pid {}) \
                     pidfd probe failed ({}) — left untouched",
                    rec.uid, rec.child_pid, e,
                );
            }
        }
        // candidate drops here: closes this record's original pidfd +
        // master. Closing the master after the deliberate kill is
        // plain fd teardown (the child is already dead/reaped on the
        // signaled path; on the mismatch path the close may SIGHUP
        // whatever holds the slave — fd lifecycle, not a signal we
        // send).
    }

    // Close the remaining escrow fds. NOT the listener — never
    // escrowed; run() owns it and keeps serving through it.
    eprintln!(
        "cm-daemon: terminal fallback: closing manifest fd {}, rollback fd \
         {}{} — returning to normal startup (legacy restore_sessions runs \
         next)",
        manifest_fd.as_raw_fd(),
        rollback_fd.as_raw_fd(),
        match &tls_listener_fd {
            Some(fd) => format!(", TLS listener fd {}", fd.as_raw_fd()),
            None => String::new(),
        },
    );
    drop(manifest_fd);
    drop(rollback_fd);
    drop(tls_listener_fd);

    HandoffOutcome::TerminalFallback { killed, total }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;

    /// The receive-side ladder: attempts 0 and 1 rehydrate; a
    /// manifest CLAIMING attempt ≥ 2 is never trusted with a third
    /// exec — terminal immediately.
    #[test]
    fn attempt_decision_ladder() {
        assert_eq!(attempt_decision(0), AttemptDecision::Rehydrate);
        assert_eq!(attempt_decision(1), AttemptDecision::Rehydrate);
        assert_eq!(attempt_decision(2), AttemptDecision::TerminalFallback);
        assert_eq!(attempt_decision(3), AttemptDecision::TerminalFallback);
        assert_eq!(attempt_decision(u8::MAX), AttemptDecision::TerminalFallback);
    }

    /// The failure-side ladder: attempt 0 → rollback exec with
    /// attempt 1; attempt 1 (the rollback image failing) → terminal;
    /// ≥ 2 stays terminal (total function — [`attempt_decision`]
    /// already diverted those before any transaction ran).
    #[test]
    fn on_rehydrate_failure_ladder() {
        assert_eq!(
            on_rehydrate_failure(0),
            FailureNext::RollbackExec { next_attempt: 1 }
        );
        assert_eq!(on_rehydrate_failure(1), FailureNext::TerminalFallback);
        assert_eq!(on_rehydrate_failure(2), FailureNext::TerminalFallback);
        assert_eq!(on_rehydrate_failure(u8::MAX), FailureNext::TerminalFallback);
    }

    struct KnobGuard {
        prev: Option<std::ffi::OsString>,
    }
    impl Drop for KnobGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var(ENV_TEST_FAIL_REHYDRATE, v),
                None => std::env::remove_var(ENV_TEST_FAIL_REHYDRATE),
            }
        }
    }
    fn knob_guard() -> KnobGuard {
        KnobGuard {
            prev: std::env::var_os(ENV_TEST_FAIL_REHYDRATE),
        }
    }

    /// The knob names the attempts to fail: "0" hits only attempt 0
    /// (the rollback image at attempt 1 rehydrates cleanly), "0,1"
    /// hits both, "terminal" hits everything, garbage hits nothing.
    #[test]
    fn test_fail_knob_names_attempts() {
        let _g = env_lock();
        let _restore = knob_guard();

        std::env::remove_var(ENV_TEST_FAIL_REHYDRATE);
        assert!(test_fail_requested(true, 0).is_none(), "absent var");

        std::env::set_var(ENV_TEST_FAIL_REHYDRATE, "0");
        assert!(test_fail_requested(true, 0).is_some());
        assert!(
            test_fail_requested(true, 1).is_none(),
            "'0' must not fail the attempt-1 (rollback) image"
        );

        std::env::set_var(ENV_TEST_FAIL_REHYDRATE, "0,1");
        assert!(test_fail_requested(true, 0).is_some());
        assert!(test_fail_requested(true, 1).is_some());

        std::env::set_var(ENV_TEST_FAIL_REHYDRATE, "1");
        assert!(test_fail_requested(true, 0).is_none());
        assert!(test_fail_requested(true, 1).is_some());

        std::env::set_var(ENV_TEST_FAIL_REHYDRATE, "terminal");
        for attempt in [0u8, 1, 2, 200] {
            assert!(
                test_fail_requested(true, attempt).is_some(),
                "'terminal' must hit attempt {}",
                attempt
            );
        }

        for garbage in ["", "  ", "abc", "-1", "0x1"] {
            std::env::set_var(ENV_TEST_FAIL_REHYDRATE, garbage);
            assert!(
                test_fail_requested(true, 0).is_none(),
                "{:?} must hit nothing",
                garbage
            );
        }
    }

    /// The production gate: without the CM_REEXEC=1 dev flag
    /// (`reexec_enabled == false`) the knob is ignored entirely, no
    /// matter its value.
    #[test]
    fn test_fail_knob_requires_dev_flag() {
        let _g = env_lock();
        let _restore = knob_guard();
        for val in ["0", "0,1", "terminal"] {
            std::env::set_var(ENV_TEST_FAIL_REHYDRATE, val);
            assert!(
                test_fail_requested(false, 0).is_none(),
                "a production daemon (no dev flag) must ignore {}={}",
                ENV_TEST_FAIL_REHYDRATE,
                val
            );
            assert!(test_fail_requested(false, 1).is_none());
        }
    }

    // ---------------------------------------------------------------
    // Phase 4b: the status-cell age round trip (R11)
    // ---------------------------------------------------------------

    /// Write-side math (`elapsed` against one anchor) then read-side
    /// math ([`instant_from_age`] against another anchor) recovers
    /// the cell to within the anchor gap — the "sub-second swap
    /// skew" the design accepts. Same-process here, so the recovered
    /// instant must land within a tight epsilon of the original.
    #[test]
    fn age_round_trip_recovers_cells_within_epsilon() {
        let stamped = Instant::now() - Duration::from_secs_f64(123.456);
        // Write side (build_manifest's cell_age).
        let write_anchor = Instant::now();
        let age_s =
            write_anchor.saturating_duration_since(stamped).as_secs_f64();
        assert!(age_s >= 123.0 && age_s < 124.0, "age_s = {}", age_s);
        // Read side.
        let read_anchor = Instant::now();
        let recovered = instant_from_age(read_anchor, age_s)
            .expect("a real elapsed age reconstructs");
        let drift = if recovered > stamped {
            recovered - stamped
        } else {
            stamped - recovered
        };
        assert!(
            drift < Duration::from_millis(250),
            "same-process round trip drifted {:?}",
            drift
        );
    }

    /// Relative ORDER of cells is preserved exactly through the
    /// round trip when both sides use one anchor — the property the
    /// done-report superseded rule and `semantic_idle` depend on
    /// (they compare cells against each other, never against wall
    /// clock).
    #[test]
    fn age_round_trip_preserves_cell_order() {
        let input_at = Instant::now() - Duration::from_secs_f64(60.0);
        let report_at = Instant::now() - Duration::from_secs_f64(10.0);
        assert!(report_at > input_at, "report postdates input by setup");

        let write_anchor = Instant::now();
        let input_age =
            write_anchor.saturating_duration_since(input_at).as_secs_f64();
        let report_age =
            write_anchor.saturating_duration_since(report_at).as_secs_f64();
        // Newer cell ⇒ smaller age.
        assert!(report_age < input_age);

        let read_anchor = Instant::now();
        let input_rec = instant_from_age(read_anchor, input_age).unwrap();
        let report_rec = instant_from_age(read_anchor, report_age).unwrap();
        // The live superseded rule is `last_input > at_instant ⇒
        // superseded`; input predates the report here, so the report
        // must remain CURRENT after reconstruction.
        assert!(
            !(input_rec > report_rec),
            "reconstruction inverted the input/report order — a current \
             done report would read superseded post-swap"
        );

        // And the tombstone-side pure-age form of the same rule
        // (swap_exit_tombstone): input_age >= report_age ⇔ current.
        assert!(input_age >= report_age);
    }

    /// [`instant_from_age`] refuses what it can't represent instead
    /// of guessing: NaN / negative (already rejected by manifest
    /// validation, double-guarded here) and ages past the platform
    /// clock's representable range. (Linux `Instant`s carry i64
    /// seconds, so merely-beyond-boot ages like 1e18 still
    /// reconstruct — to a pre-boot instant, harmless for the
    /// ordering comparisons these cells feed; only past ~9.2e18
    /// seconds does `checked_sub` underflow.)
    #[test]
    fn instant_from_age_refuses_unrepresentable() {
        let now = Instant::now();
        assert!(instant_from_age(now, f64::NAN).is_none());
        assert!(instant_from_age(now, -1.0).is_none());
        // Past the i64-seconds range: checked_sub underflows.
        assert!(instant_from_age(now, 1.0e19).is_none());
        // A sane age works.
        assert!(instant_from_age(now, 0.0).is_some());
        assert!(instant_from_age(now, 3600.0).is_some());
    }

    /// [`adopted_meta_from_record`] maps the record verbatim: the
    /// identity fields land unchanged, `None` cells stay `None`, the
    /// done report reconstructs, and kills_dir is wired iff the
    /// record carries a soft memory cap (mirroring `start_session`).
    #[test]
    fn adopted_meta_maps_record_honestly() {
        let anchor = Instant::now();
        let rec = SessionRecord {
            uid: "ts-meta-1".into(),
            generation: 4,
            transcript_id: Some("tid".into()),
            transcript_path: Some("/tmp/tid.jsonl".into()),
            session_type: "codex".into(),
            title: "my worker".into(),
            workspace_id: "ws-9".into(),
            task_id: Some("task-1".into()),
            managed_by_uid: Some("ts-parent".into()),
            workflow_run_id: Some("run-1".into()),
            workflow_role: Some("reviewer".into()),
            continuous_task_id: Some("ct-2".into()),
            global_perms: true,
            memory_cap_soft_bytes: Some(1 << 30),
            memory_cap_hard_bytes: Some(2 << 30),
            last_activity_age_s: Some(5.0),
            last_input_age_s: Some(50.0),
            last_operator_input_age_s: None,
            last_turn_end_age_s: Some(6.5),
            done_report: Some(reexec_manifest::DoneReportRecord {
                at_unix: 1_700_000_000.0,
                age_s: 7.0,
                reason: Some("done".into()),
            }),
            child_pid: 1234,
            child_start_time: 42,
            pty_master_fd: 30,
            pidfd: 31,
            cgroup_prefix: Some("/sys/fs/cgroup/x".into()),
            watcher_checkpoint: None,
        };
        let meta = adopted_meta_from_record(&rec, anchor, None);
        assert_eq!(meta.title, "my worker");
        assert_eq!(meta.session_type, "codex");
        assert_eq!(meta.workspace_id, "ws-9");
        assert_eq!(meta.task_id.as_deref(), Some("task-1"));
        assert_eq!(meta.managed_by_uid.as_deref(), Some("ts-parent"));
        assert_eq!(meta.transcript_path.as_deref(), Some("/tmp/tid.jsonl"));
        assert_eq!(meta.workflow_run_id.as_deref(), Some("run-1"));
        assert_eq!(meta.workflow_role.as_deref(), Some("reviewer"));
        assert_eq!(meta.continuous_task_id.as_deref(), Some("ct-2"));
        assert!(meta.global_perms);
        assert_eq!(meta.generation, 4);
        assert_eq!(meta.memory_cap_soft_bytes, Some(1 << 30));
        assert_eq!(meta.memory_cap_hard_bytes, Some(2 << 30));
        assert!(meta.last_activity_at.is_some());
        assert!(meta.last_operator_input_at.is_none(), "None stays None");
        let report = meta.done_report.expect("done report reconstructs");
        assert_eq!(report.at_unix, 1_700_000_000.0);
        assert_eq!(report.reason.as_deref(), Some("done"));
        // Report (age 7) postdates turn-end input (age 50) — order
        // preserved through reconstruction.
        assert!(report.at_instant > meta.last_input_at.unwrap());
        // Capped record wires the kill-log probe (real HOME present
        // in test env ⇒ Some; the rule is presence-of-soft-cap).
        assert_eq!(
            meta.kills_dir.is_some(),
            crate::path::default_kills_dir().is_some()
        );
        // No checkpoint → no carried baseline: build_adopted captures
        // a fresh one (the 4b behavior).
        assert_eq!(meta.kills_baseline, None);

        // With a checkpoint (phase 4d, R12): the spawn-time kill-log
        // baseline is carried into the meta, so the adopted probe
        // attributes against the OLD image's window.
        let cp = crate::session_watch::WatcherCheckpoint {
            version: crate::session_watch::WATCHER_CHECKPOINT_VERSION,
            cgroup_path: "/sys/fs/cgroup/x/cm-sess-1.scope".into(),
            protected: Some(vec![(1234, 42)]),
            last_high: 3,
            kills_baseline: 777,
        };
        let meta = adopted_meta_from_record(&rec, anchor, Some(&cp));
        assert_eq!(meta.kills_baseline, Some(777));

        // Uncapped record: no kills_dir, no probe.
        let mut uncapped = rec.clone();
        uncapped.memory_cap_soft_bytes = None;
        uncapped.memory_cap_hard_bytes = None;
        let meta = adopted_meta_from_record(&uncapped, anchor, None);
        assert!(meta.kills_dir.is_none());
    }

    /// [`swap_exit_tombstone`] provenance: an exit during the swap
    /// is NOT a kill; the done report rides onto the tombstone under
    /// the superseded rule (input newer than the report drops it).
    #[test]
    fn swap_exit_tombstone_provenance_and_report_carry() {
        let mut rec = SessionRecord {
            uid: "ts-dead-1".into(),
            generation: 2,
            transcript_id: None,
            transcript_path: Some("/tmp/t.jsonl".into()),
            session_type: "bash".into(),
            title: "doomed".into(),
            workspace_id: "ws-x".into(),
            task_id: Some("task-d".into()),
            managed_by_uid: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            global_perms: false,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            last_activity_age_s: Some(1.0),
            last_input_age_s: Some(30.0),
            last_operator_input_age_s: None,
            last_turn_end_age_s: None,
            done_report: Some(reexec_manifest::DoneReportRecord {
                at_unix: 1_700_000_123.0,
                age_s: 5.0,
                reason: Some("finished".into()),
            }),
            child_pid: 999,
            child_start_time: 7,
            pty_master_fd: 40,
            pidfd: 41,
            cgroup_prefix: None,
            watcher_checkpoint: None,
        };

        // Report (age 5) is newer than the last input (age 30):
        // current → carried.
        let tomb =
            swap_exit_tombstone(&rec, Some("/wt/path".into()), 1_700_000_200.0);
        assert_eq!(tomb.session_uid, "ts-dead-1");
        assert_eq!(tomb.label, "doomed");
        assert_eq!(tomb.session_type, "bash");
        assert_eq!(tomb.task_id.as_deref(), Some("task-d"));
        assert_eq!(tomb.transcript_path.as_deref(), Some("/tmp/t.jsonl"));
        assert_eq!(tomb.worktree_path.as_deref(), Some("/wt/path"));
        assert!(!tomb.killed, "a swap exit is not a kill");
        assert!(tomb.killed_by.is_none(), "no fabricated kill provenance");
        assert_eq!(tomb.reported_done_at, Some(1_700_000_123.0));
        assert_eq!(tomb.report_reason.as_deref(), Some("finished"));

        // Input newer than the report (input age 2 < report age 5):
        // superseded → not carried.
        rec.last_input_age_s = Some(2.0);
        let tomb = swap_exit_tombstone(&rec, None, 1_700_000_200.0);
        assert_eq!(tomb.reported_done_at, None, "superseded report dropped");
        assert_eq!(tomb.report_reason, None);
    }

    // ---------------------------------------------------------------
    // Phase 4c (R15): checked persistence pass + post-freeze
    // revalidation
    // ---------------------------------------------------------------

    /// RAII HOME override for the checked-persist tests —
    /// `persist_all_checked` walks `runs_dir()` / `tasks_dir()`, which
    /// key off HOME, so the tests must never point at the real `~/.cm`.
    /// Held together with `env_lock` (env is process-global).
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }
    fn set_home(dir: &Path) -> HomeGuard {
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", dir);
        HomeGuard { prev }
    }

    /// [`persist_all_checked`] RETURNS an injected write failure
    /// (unwritable directory) instead of swallowing it the way the
    /// best-effort lifecycle persist does — the restart must abort on
    /// a snapshot that didn't land.
    #[test]
    fn persist_all_checked_surfaces_write_failure() {
        use std::os::unix::fs::PermissionsExt as _;
        let _g = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = set_home(dir.path());

        let ro = dir.path().join("ro");
        std::fs::create_dir_all(&ro).expect("mk ro dir");
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o500))
            .expect("chmod ro");

        let state = Arc::new(Mutex::new(DaemonState::new()));
        state
            .lock()
            .unwrap()
            .daemon_sessions_path = Some(ro.join("daemon-sessions.json"));

        let err = persist_all_checked(&state)
            .expect_err("unwritable dir must surface as Err");
        assert!(
            err.to_string()
                .contains("checked persist of daemon sessions"),
            "error must name the failing pass: {}",
            err
        );

        // Restore perms so the tempdir cleans up.
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700))
            .expect("chmod ro back");
    }

    /// The disk-authoritative half: an unreadable `state.json` under
    /// the workflow-runs tree fails the fsync walk with an error
    /// naming the file (never a silent skip).
    #[test]
    fn persist_all_checked_surfaces_fsync_tree_failure() {
        use std::os::unix::fs::PermissionsExt as _;
        let _g = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = set_home(dir.path());

        let run_dir = dir.path().join(".cm/workflow-runs/wf-broken");
        std::fs::create_dir_all(&run_dir).expect("mk run dir");
        let state_file = run_dir.join("state.json");
        std::fs::write(&state_file, "{}").expect("write state.json");
        std::fs::set_permissions(
            &state_file,
            std::fs::Permissions::from_mode(0o000),
        )
        .expect("chmod state.json");

        // No daemon_sessions_path → the session half is skipped and
        // the tree walk is what fails.
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let err = persist_all_checked(&state)
            .expect_err("unreadable state.json must surface as Err");
        assert!(
            err.to_string().contains("state.json"),
            "error must name the file: {}",
            err
        );

        std::fs::set_permissions(
            &state_file,
            std::fs::Permissions::from_mode(0o600),
        )
        .expect("chmod back");
    }

    /// Happy path + the read-back check: the checked persist lands an
    /// fsynced snapshot, [`post_freeze_revalidation`] accepts it, and
    /// then refuses a corrupted / missing file and a drifted schema
    /// version — each with a distinct diagnosis.
    #[test]
    fn checked_persist_then_revalidation_round_trip() {
        let _g = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = set_home(dir.path());

        let sessions_path = dir.path().join(".cm/daemon-sessions.json");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        state.lock().unwrap().daemon_sessions_path =
            Some(sessions_path.clone());

        persist_all_checked(&state).expect("checked persist lands");
        assert!(sessions_path.exists(), "snapshot written");
        post_freeze_revalidation(&state, Some(MANIFEST_SCHEMA_VERSION))
            .expect("fresh snapshot revalidates");

        // Schema drift between the dry run and the real build.
        let err =
            post_freeze_revalidation(&state, Some(MANIFEST_SCHEMA_VERSION + 1))
                .expect_err("drifted schema version must refuse");
        assert!(
            err.to_string().contains("schema version"),
            "diagnosis names the schema: {}",
            err
        );

        // Corrupt read-back.
        std::fs::write(&sessions_path, "{ not json").expect("corrupt file");
        let err = post_freeze_revalidation(&state, Some(MANIFEST_SCHEMA_VERSION))
            .expect_err("corrupt snapshot must refuse");
        assert!(
            err.to_string().contains("does not re-parse"),
            "diagnosis names the re-parse: {}",
            err
        );

        // Missing read-back (the write vanished).
        std::fs::remove_file(&sessions_path).expect("remove file");
        let err = post_freeze_revalidation(&state, Some(MANIFEST_SCHEMA_VERSION))
            .expect_err("missing snapshot must refuse");
        assert!(
            err.to_string().contains("missing at read-back"),
            "diagnosis names the missing file: {}",
            err
        );
    }

    // ---------------------------------------------------------------
    // Phase 4g (audit gap 8): refuse re-exec under [tls]
    // ---------------------------------------------------------------

    /// A `[tls]`-configured daemon must REFUSE `perform_reexec` up
    /// front — before the pin, the preflight, and every side effect —
    /// naming the unimplemented TLS handoff. Pre-4g the only guard was
    /// the module doc's advisory; a swap would drop every TLS client,
    /// and a TLS re-bind failure in the new image is startup-fatal
    /// BEFORE `complete_handoff` (crash-class, escrow unresolved).
    #[test]
    fn perform_reexec_refuses_tls_configured_daemon() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        state.lock().unwrap().config.tls = Some(crate::config::TlsConfig {
            cert_path: "/nonexistent/cert.pem".into(),
            key_path: "/nonexistent/key.pem".into(),
            listen_addr: "127.0.0.1:18443".into(),
        });

        // The target path deliberately does NOT exist: if the TLS
        // guard weren't first, the call would fail at the executable
        // pin with a different message — the assertion below proves
        // the refusal is the TLS one, i.e. nothing before it ran.
        let err = perform_reexec(
            &state,
            Path::new("/nonexistent/cm-daemon-target"),
            true,
        );
        let msg = err.to_string();
        assert!(
            msg.contains("re-exec REFUSED") && msg.contains("[tls]"),
            "refusal must name the [tls] limitation: {}",
            msg
        );
        assert!(
            msg.contains("TLS listener handoff is unimplemented"),
            "refusal must say WHY: {}",
            msg
        );
        assert!(
            msg.contains("127.0.0.1:18443"),
            "refusal names the configured listener: {}",
            msg
        );

        // Daemon state untouched: no drain latched, no restart in
        // flight, registry empty — the refusal happened before the
        // quiesce barrier ever began.
        let st = state.lock().unwrap();
        assert!(!st.draining, "refusal must not leave the daemon draining");
        assert!(!st.restarting, "refusal must not latch `restarting`");
        assert!(st.sessions.is_empty());
    }

    // ---------------------------------------------------------------
    // Phase 4h (R10, audit gap 6): writer-gate wiring
    // ---------------------------------------------------------------

    /// `perform_reexec` takes the writer-gate freeze after
    /// `wait_quiesced` and ABORTS `restart_busy` — with full state
    /// restore — when a PTY-write unit never finishes (the wedged
    /// `write_all` case the bounded freeze exists for).
    ///
    /// Safety of running this in-binary: with the unit permit held,
    /// the writer freeze times out (cfg(test)-shrunk
    /// `WRITER_FREEZE_TIMEOUT`) BEFORE `exec_stage` runs, so no exec
    /// can happen; and even if a bug let the freeze through, this
    /// state has no `listener_raw_fd`, so `build_manifest` errors
    /// long before `execveat`.
    #[test]
    fn perform_reexec_aborts_restart_busy_on_wedged_writer_unit() {
        let _serial = crate::writer_gate::TEST_SERIAL
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let state = Arc::new(Mutex::new(DaemonState::new()));

        // A delivery unit that never finishes — the stand-in for a
        // `write_all` wedged against a full PTY input buffer.
        let wedged_unit = crate::writer_gate::unit_permit();

        // Target must PIN successfully (the refusal under test is the
        // writer freeze, several steps later); /bin/true is a real
        // executable everywhere this suite runs. skip_preflight: the
        // preflight is 4c's concern and needs a real cm-daemon target.
        let err = perform_reexec(&state, Path::new("/bin/true"), true);
        let msg = err.to_string();
        assert!(
            msg.contains("restart_busy"),
            "wedged writer must abort as restart_busy: {}",
            msg
        );
        assert!(
            msg.contains("writer") || msg.contains("delivery"),
            "the abort must name the writer gate as the holdout: {}",
            msg
        );

        // Full restore: drain + restarting cleared, and the writer
        // pause released (a leaked pause would park every future
        // prompt delivery at the door forever).
        {
            let st = state.lock().unwrap();
            assert!(!st.draining, "abort must un-drain");
            assert!(!st.restarting, "abort must clear `restarting`");
        }
        assert!(
            !crate::writer_gate::pause_requested(),
            "abort must drop the writer pause (door reopened)"
        );

        // The gate itself is unharmed: with the wedged unit released,
        // a fresh pause + freeze succeeds.
        drop(wedged_unit);
        let pause = crate::writer_gate::request_pause();
        let frozen =
            crate::writer_gate::freeze(&pause, Duration::from_secs(5))
                .expect("gate healthy after the aborted restart");
        drop(frozen);
        drop(pause);
    }

    // ---------------------------------------------------------------
    // Phase 4g (audit gap 7): swap-dead continuous run-close flip
    // ---------------------------------------------------------------

    /// Build a continuous task on disk with an ACTIVE run bound to
    /// `session_uid`, so the flip under test has real state to close.
    fn save_running_continuous(task_id: &str, session_uid: &str) {
        use crate::continuous::task::{
            self, ContinuousTask, Engine, RunMode, RunRecord, RunStatus,
            Schedule,
        };
        let mut t = ContinuousTask::new(
            task_id.into(),
            "gap7".into(),
            "ws-gap7".into(),
            "/tmp/gap7-wt".into(),
            Engine::Claude,
            RunMode::Fresh,
            Schedule::OnDemand,
            "go".into(),
        );
        t.consecutive_wedge_closes = 3;
        t.last_run = Some(RunRecord {
            seq: 7,
            fire_token: "ft-gap7".into(),
            started_at: 1,
            finished_at: None,
            session_uid: Some(session_uid.into()),
            status: RunStatus::Running,
            trigger_source: "test".into(),
        });
        task::save(&t).expect("save continuous task");
    }

    /// A crafted swap-dead record for the gap-7 tests.
    fn continuous_dead_record(uid: &str, ct_id: Option<&str>) -> SessionRecord {
        SessionRecord {
            uid: uid.into(),
            generation: 1,
            transcript_id: None,
            transcript_path: None,
            session_type: "claude-code".into(),
            title: "gap7 tick".into(),
            workspace_id: "ws-gap7".into(),
            task_id: None,
            managed_by_uid: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: ct_id.map(str::to_string),
            global_perms: false,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            last_activity_age_s: None,
            last_input_age_s: None,
            last_operator_input_age_s: None,
            last_turn_end_age_s: None,
            done_report: None,
            child_pid: 4242,
            child_start_time: 1,
            pty_master_fd: 50,
            pidfd: 51,
            cgroup_prefix: None,
            watcher_checkpoint: None,
        }
    }

    /// The gap-7 regression: a continuous tick whose child exited
    /// during the swap gets the SAME run-close flip a live exit gets
    /// from `handle_session_exit` — clean exit → `Done` (+ wedge-streak
    /// reset), non-zero exit → `Failed` (streak preserved) — instead of
    /// leaving the run `Running` for the scheduler's
    /// `reconcile_orphans` to misclassify `Orphaned`.
    #[test]
    fn swap_dead_continuous_close_flips_running_run() {
        use crate::continuous::task::{self, RunStatus};
        let _g = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = set_home(dir.path());

        // Clean exit (code 0) → Done, finished_at stamped, wedge
        // streak reset.
        save_running_continuous("ct-gap7-done", "ts-gap7-dead");
        let rec = continuous_dead_record("ts-gap7-dead", Some("ct-gap7-done"));
        swap_dead_continuous_close(
            &rec,
            &crate::session::DaemonExitStatus {
                code: Some(0),
                signal: None,
            },
        );
        let t = task::load_one("ct-gap7-done").expect("task on disk");
        let run = t.last_run.expect("run present");
        assert_eq!(run.status, RunStatus::Done, "clean swap exit closes Done");
        assert!(run.finished_at.is_some(), "finished_at stamped");
        assert_eq!(
            t.consecutive_wedge_closes, 0,
            "a Done close ends the wedge streak (was 3)"
        );

        // Non-zero exit → Failed; the wedge streak is NOT a clean
        // completion's to reset.
        save_running_continuous("ct-gap7-fail", "ts-gap7-dead");
        swap_dead_continuous_close(
            &continuous_dead_record("ts-gap7-dead", Some("ct-gap7-fail")),
            &crate::session::DaemonExitStatus {
                code: Some(2),
                signal: None,
            },
        );
        let t = task::load_one("ct-gap7-fail").expect("task on disk");
        assert_eq!(
            t.last_run.as_ref().unwrap().status,
            RunStatus::Failed,
            "non-zero swap exit is a genuine failure"
        );
        assert_eq!(
            t.consecutive_wedge_closes, 3,
            "Failed close preserves the wedge streak"
        );

        // Bare signal (code None) → Done, matching handle_session_exit's
        // treatment of signal kills (and honest for the swap window,
        // where a cap kill can't be attributed).
        save_running_continuous("ct-gap7-sig", "ts-gap7-dead");
        swap_dead_continuous_close(
            &continuous_dead_record("ts-gap7-dead", Some("ct-gap7-sig")),
            &crate::session::DaemonExitStatus {
                code: None,
                signal: Some(9),
            },
        );
        assert_eq!(
            task::load_one("ct-gap7-sig")
                .unwrap()
                .last_run
                .unwrap()
                .status,
            RunStatus::Done,
            "bare-signal swap exit mirrors the live path's Done"
        );
    }

    /// The flip's guards hold on the swap-dead path too: a record with
    /// no `continuous_task_id` is a no-op; a run bound to a DIFFERENT
    /// session uid is untouched (the double guard — never close another
    /// tick's run); a run that already left `Running` (e.g. the
    /// watchdog escalated it `Stuck` pre-swap) is preserved.
    #[test]
    fn swap_dead_continuous_close_respects_guards() {
        use crate::continuous::task::{self, RunStatus};
        let _g = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = set_home(dir.path());
        let status = crate::session::DaemonExitStatus {
            code: Some(0),
            signal: None,
        };

        // No continuous tag → nothing written anywhere (no task dir
        // appears).
        swap_dead_continuous_close(
            &continuous_dead_record("ts-gap7-plain", None),
            &status,
        );
        assert!(
            !crate::continuous::task::tasks_dir().exists(),
            "an untagged record must not touch continuous state"
        );

        // Uid mismatch → run untouched.
        save_running_continuous("ct-gap7-other", "ts-gap7-OTHER");
        swap_dead_continuous_close(
            &continuous_dead_record("ts-gap7-dead", Some("ct-gap7-other")),
            &status,
        );
        let t = task::load_one("ct-gap7-other").expect("task on disk");
        assert_eq!(
            t.last_run.as_ref().unwrap().status,
            RunStatus::Running,
            "another session's run must never be closed"
        );

        // Already-escalated run (Stuck) → preserved.
        save_running_continuous("ct-gap7-stuck", "ts-gap7-dead");
        let _ = task::modify("ct-gap7-stuck", |t| {
            t.last_run.as_mut().unwrap().status = RunStatus::Stuck;
        });
        swap_dead_continuous_close(
            &continuous_dead_record("ts-gap7-dead", Some("ct-gap7-stuck")),
            &status,
        );
        assert_eq!(
            task::load_one("ct-gap7-stuck")
                .unwrap()
                .last_run
                .unwrap()
                .status,
            RunStatus::Stuck,
            "a pre-swap Stuck escalation must survive the flip"
        );
    }

    // ---------------------------------------------------------------
    // Phase 4d (R12, audit gap 1): watcher re-adoption from the
    // manifest checkpoint
    // ---------------------------------------------------------------

    /// A crafted CAPPED record for the 4d respawn tests.
    fn capped_record(
        uid: &str,
        child_pid: i32,
        watcher_checkpoint: Option<serde_json::Value>,
    ) -> SessionRecord {
        SessionRecord {
            uid: uid.into(),
            generation: 1,
            transcript_id: None,
            transcript_path: None,
            session_type: "claude-code".into(),
            title: "capped".into(),
            workspace_id: "ws-4d".into(),
            task_id: None,
            managed_by_uid: None,
            workflow_run_id: None,
            workflow_role: None,
            continuous_task_id: None,
            global_perms: false,
            memory_cap_soft_bytes: Some(64 * 1024 * 1024),
            memory_cap_hard_bytes: Some(128 * 1024 * 1024),
            last_activity_age_s: None,
            last_input_age_s: None,
            last_operator_input_age_s: None,
            last_turn_end_age_s: None,
            done_report: None,
            child_pid,
            child_start_time: 1,
            pty_master_fd: 60,
            pidfd: 61,
            cgroup_prefix: Some("cm-cap-4d".into()),
            watcher_checkpoint,
        }
    }

    /// The gap-1 regression, seeding half: a capped record with a
    /// checkpoint gets a watcher whose live state is the CHECKPOINTED
    /// policy — restored protected set, restored `last_high`
    /// watermark, the OLD image's discovered cgroup path — never a
    /// recomputation from the current cgroup. (The kill-decision
    /// divergence of that policy is pinned end-to-end in
    /// `session_watch::tests::restored_watcher_preserves_protected_set_and_last_high`.)
    #[test]
    fn respawn_adopted_watcher_seeds_from_checkpoint() {
        let _g = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = set_home(dir.path());

        // Synthetic watched cgroup, alive so the watcher enters its
        // main loop rather than exiting at the appear deadline.
        let cgroup = dir.path().join("cm-sess-4d.scope");
        std::fs::create_dir_all(&cgroup).unwrap();
        std::fs::write(cgroup.join("cgroup.procs"), b"").unwrap();
        std::fs::write(cgroup.join("memory.events"), b"high 4\nmax 0\n")
            .unwrap();

        let cp = crate::session_watch::WatcherCheckpoint {
            version: crate::session_watch::WATCHER_CHECKPOINT_VERSION,
            cgroup_path: cgroup.to_string_lossy().into_owned(),
            protected: Some(vec![(41, 410), (42, 420)]),
            last_high: 4,
            kills_baseline: 99,
        };
        let rec = capped_record(
            "ts-4d-restore",
            4242,
            Some(serde_json::to_value(&cp).unwrap()),
        );
        let parsed = rec
            .watcher_checkpoint
            .as_ref()
            .and_then(|v| {
                crate::session_watch::parse_watcher_checkpoint(&rec.uid, v)
            })
            .expect("checkpoint parses");

        let spawned = respawn_adopted_watcher(
            &rec,
            Some(&parsed),
            std::sync::Weak::new(), // no coordinator — registration skipped
        )
        .expect("capped record with a checkpoint must get a watcher");

        {
            let st = spawned.state.lock().unwrap();
            assert_eq!(st.cgroup_path, cgroup, "watches the OLD image's path");
            assert_eq!(
                st.protected
                    .as_ref()
                    .map(|s| {
                        let mut v: Vec<(u32, u64)> =
                            s.iter().copied().collect();
                        v.sort_unstable();
                        v
                    }),
                Some(vec![(41, 410), (42, 420)]),
                "protected pairs restored, not recomputed"
            );
            assert_eq!(st.last_high, 4, "breach watermark restored");
        }

        // Teardown: cgroup vanishes → the watcher's loop exits.
        std::fs::remove_dir_all(&cgroup).unwrap();
        spawned.handle.join().expect("watcher exits");
    }

    /// The degrade ladder never fails the adoption: a capped record
    /// with NO checkpoint falls to the fresh path, whose cgroup
    /// re-discovery (from `/proc/<pid>/cgroup` — the pid here does
    /// not exist) fails → `None`, loudly, with the caller adopting
    /// the session anyway (the commit loop just leaves
    /// `watcher_handle`/`watcher_state` as `None`). Same for a
    /// checkpoint that fails to parse — `parse_watcher_checkpoint`
    /// already returned `None` upstream, making the two paths one.
    #[test]
    fn respawn_adopted_watcher_degrades_to_none_without_usable_state() {
        let _g = env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let _home = set_home(dir.path());

        // A pid that cannot exist → /proc/<pid>/cgroup reads ENOENT
        // and discovery returns Err immediately (the retry loop only
        // spins on parse errors, not read errors).
        let rec = capped_record("ts-4d-fresh-fail", i32::MAX - 1, None);
        assert!(
            respawn_adopted_watcher(&rec, None, std::sync::Weak::new())
                .is_none(),
            "fresh path with failed discovery degrades to no watcher — \
             never an Err, never a panic"
        );

        // Unparseable checkpoint value → the shared parse helper
        // (exercised via the same route the build phase uses) yields
        // None, landing in the same fresh-or-nothing ladder.
        let garbage = capped_record(
            "ts-4d-garbage",
            i32::MAX - 1,
            Some(serde_json::json!({"version": 999, "nonsense": true})),
        );
        let parsed = garbage.watcher_checkpoint.as_ref().and_then(|v| {
            crate::session_watch::parse_watcher_checkpoint(&garbage.uid, v)
        });
        assert!(parsed.is_none(), "garbage checkpoint refuses to parse");
        assert!(respawn_adopted_watcher(
            &garbage,
            parsed.as_ref(),
            std::sync::Weak::new()
        )
        .is_none());
    }
}
