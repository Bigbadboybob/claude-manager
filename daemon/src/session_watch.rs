//! Daemon-side cgroup-OOM watcher. Slice 10d-memory-cap-relocation.
//!
//! ## Where this fits
//!
//! Pre-10d, the TUI's `tui/src/session_watch.rs` watched the
//! cgroup for both local-spawn AND daemon-attached sessions —
//! but the daemon owns the cgroup for daemon-spawned children,
//! so the TUI had no way to observe them. Daemon-spawned cap
//! kills surfaced as `memory_cap_kill: false` (silent violation
//! of the memory-cap criterion).
//!
//! Post-10d, the producer side moves here. Per daemon-spawned
//! capped session, the daemon spawns a watcher thread that:
//!   1. Waits for the cgroup at `cgroup_path` to materialize
//!      (post-`systemd-run` lag).
//!   2. Stabilization phase: snapshots the initial PID set as
//!      the *protected* set (agent + its initial children).
//!   3. Follow-up phase: admits late-forking workers if their
//!      ppid is already protected.
//!   4. Main loop: polls `memory.events` for `high` counter
//!      increments. On breach, picks the highest-RSS
//!      non-protected PID, SIGTERMs + SIGKILLs it, and writes
//!      a JSONL record to `~/.cm/memory_kills/<uid>.jsonl`.
//!
//! The JSONL record is what the daemon's existing reaper-side
//! consumer reads (slice 10c-e-2 review-6 lazy classification
//! in `session.rs::LastExitProbe::snapshot`). End frame's
//! `memory_cap_kill: true` → AttachedPty's latched
//! `Arc<AtomicBool>` → TUI exit handler → cap-kill toast via
//! activity log. The chain wired by fix4a + fix4b now has its
//! producer.
//!
//! ## Why not share code with `tui/src/session_watch.rs`
//!
//! The TUI watcher is the local-spawn producer; it ALSO sends
//! `MemoryKillEvent` over an mpsc channel that App's drainer
//! consumes for the local-spawn toast. The daemon doesn't need
//! that channel — its consumer is the kill-log itself
//! (read lazily by the reaper). Some duplication of low-level
//! /proc readers + kill module is the cost; clean separation
//! is the win. A future refactor could extract a shared
//! `cm-cgroup-watcher` crate; out of scope for this slice.
//!
//! Linux-only by way of the crate-root gate on `lib.rs`
//! (slice 10d watcher-fix #2). No per-module portability gates
//! here — the crate as a whole doesn't build on non-Linux.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::restart_coordinator::RestartCoordinator;

// --- Sanitizer ---------------------------------------------------------

/// Sanitize an untrusted byte string for safe embedding in the
/// JSONL log. Mirrors `tui/src/session_watch.rs::sanitize` so
/// daemon-side records match the local-spawn format byte-for-byte.
/// (A future refactor could share this; out of scope for 10d.)
pub fn sanitize(input: &[u8], max_chars: usize) -> String {
    let cleaned_bytes: Vec<u8> = input
        .iter()
        .map(|&b| if b < 0x20 || b == 0x7f { b'?' } else { b })
        .collect();
    let s_utf8 = match std::str::from_utf8(&cleaned_bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(&cleaned_bytes)
            .chars()
            .map(|c| if c == '\u{FFFD}' { '?' } else { c })
            .collect(),
    };
    let filtered: String = s_utf8
        .chars()
        .map(|c| if is_hostile_codepoint(c) { '?' } else { c })
        .collect();
    if filtered.chars().count() > max_chars {
        let truncated: String = filtered.chars().take(max_chars - 1).collect();
        format!("{}\u{2026}", truncated)
    } else {
        filtered
    }
}

fn is_hostile_codepoint(c: char) -> bool {
    let n = c as u32;
    if (0x80..=0x9F).contains(&n) {
        return true;
    }
    if matches!(n, 0x202A..=0x202E | 0x2066..=0x2069) {
        return true;
    }
    if matches!(n, 0x200B | 0x200C | 0x200D | 0xFEFF) {
        return true;
    }
    if matches!(n, 0x2028 | 0x2029) {
        return true;
    }
    false
}

// --- /proc readers -----------------------------------------------------

fn read_cgroup_procs(cgroup_path: &Path) -> Vec<u32> {
    let s = match std::fs::read_to_string(cgroup_path.join("cgroup.procs")) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    s.lines().filter_map(|l| l.trim().parse().ok()).collect()
}

fn read_ppid(pid: u32) -> Option<u32> {
    let s = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    parse_stat_field(&s, 4)
}

fn read_starttime(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    parse_stat_field(&s, 22)
}

fn parse_stat_field<T: std::str::FromStr>(stat: &str, field: usize) -> Option<T> {
    let close = stat.rfind(')')?;
    let after = stat.get(close + 1..)?.trim_start();
    let parts: Vec<&str> = after.split_ascii_whitespace().collect();
    if field < 3 {
        return None;
    }
    parts.get(field - 3)?.parse().ok()
}

fn read_rss_kb(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            for tok in rest.split_ascii_whitespace() {
                if let Ok(n) = tok.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn read_comm(pid: u32) -> Option<Vec<u8>> {
    let mut bytes = std::fs::read(format!("/proc/{}/comm", pid)).ok()?;
    while matches!(bytes.last(), Some(b'\n') | Some(b'\r') | Some(0)) {
        bytes.pop();
    }
    Some(bytes)
}

fn read_cmdline(pid: u32) -> Option<Vec<Vec<u8>>> {
    let bytes = std::fs::read(format!("/proc/{}/cmdline", pid)).ok()?;
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    let mut parts: Vec<Vec<u8>> = bytes.split(|&b| b == 0).map(|s| s.to_vec()).collect();
    while parts.last().map_or(false, |s| s.is_empty()) {
        parts.pop();
    }
    Some(parts)
}

/// Read the `memory.events` `high` counter from a cgroup-v2
/// `cgroup_path`. Slice 10d watcher-fix #6 (startup race): the
/// daemon's `start_session` calls this *immediately after cgroup
/// discovery* — before `spawn_watcher` — and passes the value as
/// the watcher's seed baseline. Pre-fix-#6 the watcher seeded at
/// first poll, so any breach that landed during the ~1s window
/// between child-spawn and watcher-init was silently baselined
/// away. Public so the call site in `methods.rs::start_session`
/// can read once and hand off; the watcher uses the same reader
/// internally during its poll loop.
pub fn read_memory_events_high(cgroup_path: &Path) -> u64 {
    read_memory_high_count(cgroup_path)
}

fn read_memory_high_count(cgroup_path: &Path) -> u64 {
    let s = match std::fs::read_to_string(cgroup_path.join("memory.events")) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("high ") {
            return rest.trim().parse().unwrap_or(0);
        }
    }
    0
}

// --- Kill path ---------------------------------------------------------

mod kill {
    use super::*;
    use std::os::raw::c_int;

    pub enum KillTarget {
        Pidfd { fd: c_int },
        Fallback {
            pid: u32,
            starttime: u64,
            cgroup_path: PathBuf,
        },
    }

    pub fn open(pid: u32, cgroup_path: &Path) -> Option<KillTarget> {
        let fd = unsafe {
            libc::syscall(libc::SYS_pidfd_open, pid as c_int, 0 as c_int) as c_int
        };
        if fd >= 0 {
            return Some(KillTarget::Pidfd { fd });
        }
        let starttime = read_starttime(pid)?;
        Some(KillTarget::Fallback {
            pid,
            starttime,
            cgroup_path: cgroup_path.to_path_buf(),
        })
    }

    pub fn send_signal(target: &KillTarget, sig: c_int) -> bool {
        match target {
            KillTarget::Pidfd { fd } => {
                let ret = unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        *fd,
                        sig,
                        std::ptr::null::<libc::siginfo_t>(),
                        0 as c_int,
                    )
                };
                ret == 0
            }
            KillTarget::Fallback {
                pid,
                starttime,
                cgroup_path,
            } => {
                let cur = match read_starttime(*pid) {
                    Some(s) => s,
                    None => return false,
                };
                if cur != *starttime {
                    return false;
                }
                let in_cgroup = read_cgroup_procs(cgroup_path).contains(pid);
                if !in_cgroup {
                    return false;
                }
                let ret = unsafe { libc::kill(*pid as libc::pid_t, sig) };
                ret == 0
            }
        }
    }

    pub fn close(target: KillTarget) {
        if let KillTarget::Pidfd { fd } = target {
            unsafe {
                libc::close(fd);
            }
        }
    }
}

// --- Kill log ----------------------------------------------------------

/// Write one JSONL kill record into `dir`. Format matches the
/// TUI's `tui/src/session_watch.rs::write_kill_log_to` so the
/// daemon's `LastExitProbe::snapshot` reads the same shape from
/// both producers.
///
/// Best-effort: errors are swallowed. If we can't write the
/// record, the kernel still killed the offender — we just lose
/// forensic data. The reaper's End-frame attribution depends on
/// the file's existence past the per-spawn baseline, so a
/// missed write means a missed cap-kill attribution; the
/// session still exits, just without the toast.
/// Forensic enum classifying the watcher's response to a memory
/// breach. Slice 10d watcher-fix #2: pre-fix the watcher returned
/// silently when there was nothing it could (or should) kill —
/// empty cgroup, all-protected PIDs, target exited before signal
/// landed. Then the kernel's `MemoryMax` eventually OOM-killed the
/// agent itself, and `LastExitProbe::snapshot` saw no record past
/// baseline → `memory_cap_kill: false`. Silent violation of the
/// named acceptance criterion.
///
/// Post-fix the watcher writes a JSONL record on EVERY breach
/// observation, with `kill_status` distinguishing the four cases.
/// `LastExitProbe::snapshot` reads record-existence-past-baseline
/// (not the field's value), so any of these statuses correctly
/// flags `memory_cap_kill: true` — the eventual kernel kill is
/// attributed regardless of whether the watcher was the one that
/// did the killing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KillStatus {
    /// The watcher successfully SIGTERM'd or SIGKILL'd a
    /// non-protected PID. PID + comm + argv hash + rss are
    /// authoritative — this is the kill the watcher executed.
    KilledByUs,
    /// Breach observed but every PID in `cgroup.procs` was on
    /// the protected list (agent's own process tree). The
    /// watcher refused to self-immolate; the kernel's
    /// `MemoryMax` enforcement is the only remaining killer.
    /// PID / RSS fields surface the highest-RSS protected PID
    /// for forensic correlation.
    Protected,
    /// Breach observed, target PID picked, but the kernel
    /// returned `ESRCH` from both SIGTERM and SIGKILL — the
    /// target exited before we could signal. The kill we
    /// would have attributed is instead "already dead by
    /// kernel OOM or self-exit". PID is the target we tried.
    AlreadyDead,
    /// Breach observed but `cgroup.procs` was empty (e.g.
    /// transient state during scope teardown). Nothing to
    /// kill. PID / RSS fields are 0; this is the most
    /// forensic-poor case.
    NoPids,
}

impl KillStatus {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::KilledByUs => "killed_by_us",
            Self::Protected => "protected",
            Self::AlreadyDead => "already_dead",
            Self::NoPids => "no_pids",
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn write_kill_log_to(
    dir: &Path,
    session_uid: &str,
    ts: SystemTime,
    pid: u32,
    comm_sanitized: &str,
    argc: usize,
    argv_sha256_prefix: &str,
    rss_kb: u64,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kill_status: KillStatus,
) {
    // Route through the shared helper so the TUI's kill-log
    // writer (for local-spawn) and the daemon's writer (for
    // daemon-spawn) can't drift on directory permissions.
    if crate::path::ensure_dot_cm_subdir(dir).is_err() {
        return;
    }

    let path = dir.join(format!("{}.jsonl", session_uid));
    let ts_secs = ts
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let comm_json = serde_json::Value::String(comm_sanitized.to_string()).to_string();
    let line = format!(
        "{{\"ts\":{ts},\"session_uid\":\"{uid}\",\"pid\":{pid},\"comm\":{comm},\"argc\":{argc},\"argv_sha256_prefix\":\"{sha}\",\"rss_kb\":{rss},\"soft_cap_bytes\":{soft},\"hard_cap_bytes\":{hard},\"kill_status\":\"{status}\"}}\n",
        ts = ts_secs,
        uid = session_uid,
        pid = pid,
        comm = comm_json,
        argc = argc,
        sha = argv_sha256_prefix,
        rss = rss_kb,
        soft = soft_cap_bytes,
        hard = hard_cap_bytes,
        status = kill_status.as_wire(),
    );
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true).mode(0o600);
    if let Ok(mut f) = opts.open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn argv_sha256_prefix(argv: &[Vec<u8>]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut first = true;
    for arg in argv {
        if !first {
            hasher.update([0u8]);
        }
        hasher.update(arg);
        first = false;
    }
    let result = hasher.finalize();
    let mut s = String::with_capacity(8);
    for b in result.iter().take(4) {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

// --- Re-exec checkpoint (DESIGN_SEAMLESS_RESTART phase 4d, R12) --------

/// Version of the [`WatcherCheckpoint`] JSON shape. The re-exec
/// manifest carries the checkpoint **opaquely**
/// (`SessionRecord.watcher_checkpoint: Option<serde_json::Value>`), so
/// this version is the checkpoint's OWN compatibility gate — bumping
/// it never bumps the manifest schema. A checkpoint whose version
/// isn't this one is treated exactly like an unparseable one: the
/// re-adopted watcher degrades LOUDLY to fresh policy
/// (see [`parse_watcher_checkpoint`]), never guesses.
pub const WATCHER_CHECKPOINT_VERSION: u32 = 1;

/// The watcher's policy state, checkpointed across a re-exec swap
/// (R12). Everything a freshly-recomputed watcher would get WRONG:
///
/// - `protected`: the stabilize+followup-finalized PID set. A fresh
///   watcher re-snapshots the CURRENT cgroup, newly protecting late
///   workers the pre-swap policy deliberately left killable. `None`
///   means the pre-swap watcher had not finished stabilize/followup
///   when the checkpoint was taken (a capped session spawned seconds
///   before the restart) — the re-adopted watcher re-runs those
///   phases, which is exactly what the old watcher would have done.
/// - `last_high`: the `memory.events` `high` watermark already
///   accounted for. A fresh watcher re-seeds from the current
///   counter, silently baselining away a breach that landed
///   mid-swap.
/// - `kills_baseline`: the SPAWN-TIME kill-log byte offset
///   (`LastExitProbe.kills_baseline`, `reaper::capture_baseline_for_spawn`).
///   Pre-4d the adopted probe captured a FRESH baseline, so a cap
///   kill whose record landed in the swap window was unattributable
///   (the state-inventory audit's `last_exit` row).
/// - `cgroup_path`: the DISCOVERED scope dir the watcher polls (not
///   the `cgroup_prefix` the manifest record carries — discovery is
///   `/proc/<pid>/cgroup`-based and need not be repeated when the
///   old image already did it against this same live child).
///
/// The watcher's remaining inputs (uid, soft/hard cap bytes) already
/// ride the manifest record itself and are deliberately not
/// duplicated here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatcherCheckpoint {
    /// [`WATCHER_CHECKPOINT_VERSION`].
    pub version: u32,
    /// The watched cgroup scope directory.
    pub cgroup_path: String,
    /// Finalized protected PID set (sorted for determinism); `None`
    /// when stabilize/followup had not completed at checkpoint time.
    pub protected: Option<Vec<u32>>,
    /// `memory.events high` watermark already accounted for.
    pub last_high: u64,
    /// Spawn-time kill-log baseline byte offset.
    pub kills_baseline: u64,
}

/// The watcher's live policy state, shared between the watcher thread
/// (writer, at its safe points) and `reexec::build_manifest` (reader,
/// under the state lock while the watcher is parked). A **leaf
/// mutex**, like the session status cells: the watcher thread locks
/// it holding nothing else, and no reader takes the state lock or a
/// gate while holding it — so locking it under the state lock in
/// `build_manifest` is inversion-free (same argument as the
/// `cell_age` closure there).
#[derive(Debug)]
pub struct WatcherLiveState {
    /// The watched cgroup scope dir (immutable for the watcher's
    /// life; carried so the checkpoint composer doesn't need a
    /// side channel).
    pub cgroup_path: PathBuf,
    /// `None` until stabilize+followup finalize the set; `Some`
    /// thereafter. A RESTORED watcher is born `Some` (the set was
    /// finalized pre-swap).
    pub protected: Option<HashSet<u32>>,
    /// The breach watermark; updated by the watcher before each
    /// `handle_breach`.
    pub last_high: u64,
}

impl WatcherLiveState {
    /// Compose the serializable checkpoint. `kills_baseline` comes
    /// from the session's `LastExitProbe` (session-side state the
    /// watcher never holds — the caller joins the two).
    pub fn checkpoint(&self, kills_baseline: u64) -> WatcherCheckpoint {
        let protected = self.protected.as_ref().map(|set| {
            let mut v: Vec<u32> = set.iter().copied().collect();
            v.sort_unstable();
            v
        });
        WatcherCheckpoint {
            version: WATCHER_CHECKPOINT_VERSION,
            cgroup_path: self.cgroup_path.to_string_lossy().into_owned(),
            protected,
            last_high: self.last_high,
            kills_baseline,
        }
    }
}

/// Shared handle to [`WatcherLiveState`]. `DaemonSession.watcher_state`
/// holds one clone; the watcher thread holds the other.
pub type SharedWatcherState = Arc<Mutex<WatcherLiveState>>;

/// What [`spawn_watcher`] returns: the thread handle (stashed on
/// `DaemonSession.watcher_handle`, drop = detach as before) plus the
/// shared policy-state cell (stashed on
/// `DaemonSession.watcher_state` for checkpoint capture).
#[derive(Debug)]
pub struct SpawnedWatcher {
    pub handle: std::thread::JoinHandle<()>,
    pub state: SharedWatcherState,
}

/// Parse an opaque manifest `watcher_checkpoint` value back into the
/// typed checkpoint. Returns `None` — LOUDLY, naming the session and
/// the reason — for an unparseable value or a version this binary
/// doesn't understand; the caller then re-adopts with a FRESH watcher
/// (recomputed policy), which is the honest floor: strictly better
/// than no watcher, honestly worse than the checkpointed one.
pub fn parse_watcher_checkpoint(
    session_uid: &str,
    value: &serde_json::Value,
) -> Option<WatcherCheckpoint> {
    match serde_json::from_value::<WatcherCheckpoint>(value.clone()) {
        Ok(cp) if cp.version == WATCHER_CHECKPOINT_VERSION => Some(cp),
        Ok(cp) => {
            eprintln!(
                "cm-daemon: session '{}': watcher checkpoint version {} \
                 (this binary understands {}) — POLICY RESET: re-adopting \
                 with a fresh watcher (protected set recomputed, breach \
                 watermark re-anchored)",
                session_uid, cp.version, WATCHER_CHECKPOINT_VERSION,
            );
            None
        }
        Err(e) => {
            eprintln!(
                "cm-daemon: session '{}': watcher checkpoint failed to \
                 parse ({}) — POLICY RESET: re-adopting with a fresh \
                 watcher (protected set recomputed, breach watermark \
                 re-anchored)",
                session_uid, e,
            );
            None
        }
    }
}

// --- Watcher thread ----------------------------------------------------

const STABILIZE_MS: u64 = 750;
const FOLLOWUP_MS: u64 = 2000;
const POLL_INTERVAL_MS: u64 = 1000;
const SIGTERM_GRACE_MS: u64 = 500;

/// Boxed thread-spawn factory. Production passes a closure
/// that calls `std::thread::Builder::new().name(name).spawn(body)`;
/// tests pass a closure that returns `Err` to exercise the
/// resource-exhaustion path.
///
/// Why a boxed factory and not a generic `F: FnOnce`: `spawn_watcher`
/// is called from `start_session` (production) and from the
/// failure-injection test in the same crate. Boxing keeps a single
/// callable type in `start_session`'s signature space without
/// monomorphising the whole control-plane module by spawn-fn type.
pub type WatcherSpawnFn = Box<
    dyn FnOnce(
            String,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>
        + Send,
>;

/// Production spawn factory. Wraps
/// `std::thread::Builder::new().name(name).spawn(body)`, which
/// returns `io::Error` (kind = `WouldBlock` on ulimit hit, etc.)
/// rather than panicking — that's the underlying mechanism that
/// makes the whole fix possible.
pub fn default_watcher_spawn_fn() -> WatcherSpawnFn {
    Box::new(|name, body| {
        std::thread::Builder::new()
            .name(name)
            .spawn(body)
    })
}

/// Spawn a watcher thread for a daemon-attached capped session.
///
/// Returns `Err` when the underlying thread spawn fails (e.g.
/// ulimit on threads hit, resource exhaustion). The caller is
/// responsible for cleaning up the partially-constructed session
/// — typically by dropping a `PendingSession` whose `Drop` pidfd-
/// SIGKILLs the still-alive child.
///
/// Pre-slice-10d-fix the function used `.expect("spawn watcher thread")`
/// which would panic *after* `start_session` had already inserted
/// the session into the registry — silent half-broken state: the
/// session runs without a kill-log producer, the client sees a
/// broken connection, no kill-log records ever appear past the
/// per-spawn baseline so cap-kill attribution is impossible. Same
/// bug class as the prior fix2/fix3/fix5/fix6 silent-degrade fixes
/// in this chain.
///
/// The watcher exits on its own when the cgroup vanishes (which
/// happens after the session's child exits and systemd cleans up
/// the scope). The returned `JoinHandle` is stored on
/// `DaemonSession.watcher_handle`; on session-removal paths it's
/// dropped (detach), which is correct because the thread self-
/// terminates and best-effort joining can't add meaningful safety
/// without blocking the reaper-cleanup path.
///
/// Phase 4d (R12) additions:
///
/// - `coordinator`: the restart coordinator to register a
///   [`crate::restart_coordinator::SafePointHandle`] with
///   (`"memcap-watcher:<uid>"`), so a re-exec quiesce waits for this
///   watcher to reach an acknowledged NO-SIGNAL safe point before the
///   exec. `Weak` — the watcher must never keep daemon state alive;
///   `None` for tests that don't exercise the barrier.
/// - `restored_protected`: `Some(set)` re-adopts a checkpointed
///   protected set — stabilize/followup are SKIPPED (recomputing
///   would newly protect late workers the pre-swap policy left
///   killable). `None` is the fresh-spawn path (and the honest
///   restore path for a checkpoint taken mid-stabilize).
///
/// The returned [`SpawnedWatcher`] carries the shared
/// [`WatcherLiveState`] cell the watcher publishes its policy state
/// into; `reexec::build_manifest` snapshots it into the manifest's
/// `watcher_checkpoint` slot.
#[allow(clippy::too_many_arguments)]
pub fn spawn_watcher(
    session_uid: String,
    cgroup_path: PathBuf,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kills_dir: PathBuf,
    initial_high: u64,
    spawn_fn: WatcherSpawnFn,
    coordinator: Option<Weak<RestartCoordinator>>,
    restored_protected: Option<HashSet<u32>>,
) -> std::io::Result<SpawnedWatcher> {
    // The cell is created HERE (not in the thread body) so the caller
    // holds a valid handle from the moment the spawn returns — a
    // checkpoint taken before the thread was ever scheduled reads the
    // honest initial state (restored set or not-yet-finalized None,
    // plus the seed watermark).
    let state: SharedWatcherState = Arc::new(Mutex::new(WatcherLiveState {
        cgroup_path: cgroup_path.clone(),
        protected: restored_protected.clone(),
        last_high: initial_high,
    }));
    let state_for_thread = Arc::clone(&state);
    let thread_name = format!("cm-daemon-watcher-{}", session_uid);
    let body: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
        run_watcher(
            session_uid,
            cgroup_path,
            soft_cap_bytes,
            hard_cap_bytes,
            kills_dir,
            initial_high,
            state_for_thread,
            coordinator,
            restored_protected,
        );
    });
    let handle = spawn_fn(thread_name, body)?;
    Ok(SpawnedWatcher { handle, state })
}

#[allow(clippy::too_many_arguments)]
fn run_watcher(
    session_uid: String,
    cgroup_path: PathBuf,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kills_dir: PathBuf,
    initial_high: u64,
    state: SharedWatcherState,
    coordinator: Option<Weak<RestartCoordinator>>,
    restored_protected: Option<HashSet<u32>>,
) {
    // Phase 4d (R12): register with the quiescence barrier FIRST,
    // before any waiting, so a restart that begins during our
    // stabilize window still fences us (a handle born mid-quiesce
    // inherits the in-effect pause request — see
    // `restart_coordinator::register_subsystem`). The handle lives on
    // this thread's stack and drops with it (registry prunes dead
    // handles), so an exiting watcher can never wedge a restart.
    //
    // Residual window, honestly: a watcher whose thread was spawned
    // but not yet scheduled when `wait_quiesced` evaluates the
    // registry is invisible to the barrier — but it cannot reach a
    // signal for ≥ STABILIZE+FOLLOWUP+POLL (~3.75s of sleeps, each
    // preceded by a park point), so it can never hold a half-done
    // kill across the exec; the exec simply destroys it.
    //
    // SAFE-POINT PLACEMENT: `park()` is called ONLY at loop tops —
    // points where no signal has been issued and every completed
    // policy decision is already published to `state`. It is NEVER
    // called inside `handle_breach`, whose SIGTERM → 500ms grace →
    // SIGKILL sequence is the one window where parking (and an exec
    // landing while parked) would leave a half-killed process — the
    // exact R12 hazard. `handle_breach` is bounded (~500ms + a few
    // syscalls), so letting it finish before the next park keeps the
    // quiesce ack latency well under the 10s barrier timeout
    // (worst case ≈ poll sleep 1s + grace 0.5s).
    let safe_point = coordinator.as_ref().and_then(|w| w.upgrade()).map(|c| {
        RestartCoordinator::register_subsystem(
            &c,
            &format!("memcap-watcher:{}", session_uid),
        )
    });
    let park = || {
        if let Some(h) = &safe_point {
            h.park_if_requested();
        }
    };
    // Wait for the cgroup to actually appear. The daemon's
    // start_session already verifies the cgroup is active
    // (slice 10c-e-3b-fix4a) before we get here, so this is a
    // belt-and-suspenders short wait — typically the cgroup is
    // there on first read.
    let appear_deadline = Instant::now() + Duration::from_secs(3);
    while !cgroup_path.exists() {
        park();
        if Instant::now() >= appear_deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Slice 10d watcher-fix #3 + #6: the breach baseline is
    // seeded by the CALLER (start_session) from a `memory.events
    // high` read taken immediately after cgroup discovery —
    // before the watcher is even spawned. Pre-#3 the watcher
    // captured `last_high` only after stabilize+followup (~2.75s),
    // losing any breach during that window. #3 moved the read
    // to the TOP of `run_watcher`, but the gap between
    // start_session's cgroup discovery and `spawn_watcher`
    // returning (~1s of pidfd-open + arm_reaper + insert) was
    // still uncovered. #6 anchors the seed at the caller's
    // read-time so the only remaining window is the kernel-
    // level "child spawned but not yet in cgroup" sub-window
    // (microseconds; inherent and inactionable).
    //
    // Stabilize / followup phases stay — they exist to suppress
    // ACTING on the noisy startup phase (don't SIGKILL a PID
    // that's only briefly in the cgroup during scope setup) —
    // but the baseline they're stabilizing against is now
    // anchored at start_session-cgroup-discovery time, even
    // tighter than #3's watcher-start anchor.

    // Phase 4d (R12): a RESTORED watcher skips stabilize/followup —
    // its protected set was finalized by the pre-swap watcher, and
    // recomputing from the CURRENT cgroup would newly protect late
    // workers the pre-swap policy deliberately left killable (the
    // kill-decision divergence the checkpoint exists to prevent). A
    // checkpoint taken mid-stabilize carries `protected: None` and
    // lands in the fresh arm below — honest: the pre-swap watcher
    // hadn't finalized a set either.
    let protected: HashSet<u32> = match restored_protected {
        Some(set) => set,
        None => {
            let started = Instant::now();
            let stabilize_until = started + Duration::from_millis(STABILIZE_MS);
            let followup_until = started + Duration::from_millis(FOLLOWUP_MS);

            while Instant::now() < stabilize_until {
                park();
                if !cgroup_path.exists() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            let mut protected: HashSet<u32> =
                read_cgroup_procs(&cgroup_path).into_iter().collect();

            while Instant::now() < followup_until {
                park();
                std::thread::sleep(Duration::from_millis(100));
                if !cgroup_path.exists() {
                    return;
                }
                let current = read_cgroup_procs(&cgroup_path);
                for pid in current {
                    if protected.contains(&pid) {
                        continue;
                    }
                    if let Some(ppid) = read_ppid(pid) {
                        if protected.contains(&ppid) {
                            protected.insert(pid);
                        }
                    }
                }
            }
            protected
        }
    };
    // Publish the FINALIZED set (4d): from here a checkpoint restores
    // this exact policy. Mid-followup partials are deliberately never
    // published — a checkpoint that races finalization reads `None`
    // and the re-adopted watcher re-runs the phases.
    {
        let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
        st.protected = Some(protected.clone());
    }

    // First post-stabilize poll: if memory.events.high grew
    // DURING stabilize+followup (an early-OOM scenario), this
    // catches it because `last_high` is the watcher-start value,
    // not the post-stabilize one.
    let mut last_high = initial_high;
    loop {
        // Safe point: no signal in flight, `state` reflects every
        // completed decision (see the placement note at the top).
        park();
        std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        if !cgroup_path.exists() {
            return;
        }
        let current_high = read_memory_high_count(&cgroup_path);
        if current_high <= last_high {
            continue;
        }
        last_high = current_high;
        // Publish the watermark BEFORE acting on the breach, so by
        // the time the barrier's park-ack lets a checkpoint be taken
        // the cell and the kill log agree.
        {
            let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
            st.last_high = last_high;
        }
        handle_breach(
            &session_uid,
            &cgroup_path,
            &protected,
            soft_cap_bytes,
            hard_cap_bytes,
            &kills_dir,
        );
    }
}

/// Watcher's per-breach handler: identify the highest-RSS
/// unprotected PID in the cgroup, snapshot its identity for
/// forensics, write a `killed_by_us` record to the kill log
/// (10e-a r1 F1: BEFORE the signals — see comment inside),
/// then issue SIGTERM/grace/SIGKILL via the pidfd helpers.
///
/// `pub(crate)` so the 10e-a integration tests in
/// `crate::control::methods::tests` can drive the production
/// path end-to-end — the cap-kill scenario needs a real watcher
/// invocation against a known cgroup membership so the
/// write-before-kill ordering is exercised exactly as production
/// would.
pub(crate) fn handle_breach(
    session_uid: &str,
    cgroup_path: &Path,
    protected: &HashSet<u32>,
    soft_cap_bytes: u64,
    hard_cap_bytes: u64,
    kills_dir: &Path,
) {
    // Slice 10d watcher-fix #2: write a JSONL record on EVERY
    // breach observation, even when we can't kill anything.
    // Pre-fix the watcher returned silently in three cases —
    // empty cgroup, all-protected PIDs, target-already-dead —
    // and the kernel's eventual `MemoryMax` OOM-kill of the
    // agent was misattributed as `memory_cap_kill: false`. The
    // `kill_status` field distinguishes the four cases for
    // forensic analysis; `LastExitProbe::snapshot` looks at
    // record-existence-past-baseline (not the field's value),
    // so any status correctly flags the eventual exit.

    let pids = read_cgroup_procs(cgroup_path);

    // Case 1: empty cgroup. Record `no_pids`.
    if pids.is_empty() {
        write_kill_log_to(
            kills_dir,
            session_uid,
            SystemTime::now(),
            0,
            "",
            0,
            "00000000",
            0,
            soft_cap_bytes,
            hard_cap_bytes,
            KillStatus::NoPids,
        );
        return;
    }

    // Find the highest-RSS PID, and a separate highest-RSS
    // protected PID for forensic logging when the
    // unprotected-set is empty.
    let mut best_unprotected: Option<(u32, u64)> = None;
    let mut best_protected: Option<(u32, u64)> = None;
    let mut any_unprotected = false;
    for pid in &pids {
        let rss = read_rss_kb(*pid).unwrap_or(0);
        if protected.contains(pid) {
            match best_protected {
                Some((_, br)) if br >= rss => {}
                _ => best_protected = Some((*pid, rss)),
            }
        } else {
            any_unprotected = true;
            match best_unprotected {
                Some((_, br)) if br >= rss => {}
                _ => best_unprotected = Some((*pid, rss)),
            }
        }
    }

    // Case 2: all PIDs protected (agent's own process tree).
    // The watcher refuses to self-immolate; record `protected`
    // so the eventual kernel `MemoryMax` kill of the agent
    // gets correctly attributed. PID + RSS surface the
    // highest-RSS protected PID for forensic correlation.
    if !any_unprotected {
        let (pid, rss_kb) = best_protected.unwrap_or((0, 0));
        let comm_raw = read_comm(pid).unwrap_or_default();
        let comm_sanitized = sanitize(&comm_raw, 16);
        let argv = read_cmdline(pid).unwrap_or_default();
        let argc = argv.len();
        let sha = argv_sha256_prefix(&argv);
        write_kill_log_to(
            kills_dir,
            session_uid,
            SystemTime::now(),
            pid,
            &comm_sanitized,
            argc,
            &sha,
            rss_kb,
            soft_cap_bytes,
            hard_cap_bytes,
            KillStatus::Protected,
        );
        return;
    }

    // Case 3 / 4: we have an unprotected target. Try to kill.
    let (pid, rss_kb) = best_unprotected.expect("any_unprotected => Some");
    let target = match kill::open(pid, cgroup_path) {
        Some(t) => t,
        None => {
            // pidfd_open / fallback both failed — target
            // already exited between the procs read and the
            // open. Record `already_dead`.
            write_kill_log_to(
                kills_dir,
                session_uid,
                SystemTime::now(),
                pid,
                "?",
                0,
                "00000000",
                rss_kb,
                soft_cap_bytes,
                hard_cap_bytes,
                KillStatus::AlreadyDead,
            );
            return;
        }
    };

    // Snapshot pre-kill so the record reflects the target's
    // identity at the moment of decision, not after the SIGKILL.
    let comm_raw = read_comm(pid).unwrap_or_default();
    let comm_sanitized = sanitize(&comm_raw, 16);
    let argv = read_cmdline(pid).unwrap_or_default();
    let argc = argv.len();
    let sha = argv_sha256_prefix(&argv);

    // 10e-a r1 F1: write the `KilledByUs` record BEFORE issuing
    // signals. This closes the watcher-vs-reaper race that the
    // manifest.watch exit-diff broadcast surfaced. Pre-r1 the
    // order was signals-then-write; the reaper's `on_exit`
    // callback (running on the per-session reaper thread) fires
    // when `waitpid` returns, which can race ahead of this
    // function's `write_kill_log_to` call. The on_exit path
    // reads the kill log to classify `memory_cap_kill`, so a
    // missed record → wrong `false` broadcast → no toast.
    //
    // The attach-stream path (round-7 lazy classification)
    // avoided this naturally because End-frame consume time is
    // well past the watcher's write. The manifest.watch diff is
    // eager (broadcast at on_exit time), so we need the writer
    // to commit BEFORE the kill instead.
    //
    // Tradeoff: if both SIGTERM/SIGKILL ESRCH (target died
    // independently between open and signal — voluntary clean
    // exit or kernel OOM-kill beat us), the record says
    // "killed_by_us" but no signal from us actually landed.
    // Acceptable per the round-1 plan: the record means
    // "watcher committed to cap-kill at this breach baseline,"
    // which is the semantic the user wants. A clean voluntary
    // exit by a process at hard memory cap is vanishingly rare;
    // a kernel OOM-kill IS a cap kill (just a different
    // killer), so attribution stays correct in spirit.
    //
    // No post-signal write: the pre-write is canonical. The
    // priority-table in `kill_status_priority` would pick
    // `killed_by_us` over a later `already_dead` anyway, so a
    // second write would just bloat the file without changing
    // the consumer's classification.
    write_kill_log_to(
        kills_dir,
        session_uid,
        SystemTime::now(),
        pid,
        &comm_sanitized,
        argc,
        &sha,
        rss_kb,
        soft_cap_bytes,
        hard_cap_bytes,
        KillStatus::KilledByUs,
    );

    // Issue the signals. Outcomes are ignored — the pre-write
    // above is the canonical record. SIGTERM-grace-SIGKILL
    // sequence stays as-is for any process that handles SIGTERM
    // (the cooperative shutdown path).
    let _term_ok = kill::send_signal(&target, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(SIGTERM_GRACE_MS));
    let _kill_ok = kill::send_signal(&target, libc::SIGKILL);
    kill::close(target);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Fresh live-state cell for driving `run_watcher` directly, the
    /// way `spawn_watcher` would build it for a fresh (non-restored)
    /// watcher.
    fn test_cell(cgroup_path: &Path, initial_high: u64) -> SharedWatcherState {
        Arc::new(Mutex::new(WatcherLiveState {
            cgroup_path: cgroup_path.to_path_buf(),
            protected: None,
            last_high: initial_high,
        }))
    }

    #[test]
    fn sanitize_passes_through_safe_ascii() {
        assert_eq!(sanitize(b"hello", 32), "hello");
        assert_eq!(sanitize(b"python3", 32), "python3");
    }

    #[test]
    fn sanitize_replaces_c0_controls() {
        assert_eq!(sanitize(b"hi\x01there", 32), "hi?there");
        assert_eq!(sanitize(b"\x7fend", 32), "?end");
    }

    #[test]
    fn sanitize_truncates_with_ellipsis() {
        let out = sanitize(b"aaaaaaaaaa", 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn write_kill_log_creates_jsonl_record() {
        let tmp = TempDir::new().unwrap();
        write_kill_log_to(
            tmp.path(),
            "ts-test",
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            42,
            "python",
            3,
            "deadbeef",
            123456,
            6 * 1024 * 1024 * 1024,
            10 * 1024 * 1024 * 1024,
            KillStatus::KilledByUs,
        );
        let path = tmp.path().join("ts-test.jsonl");
        let content = std::fs::read_to_string(&path).expect("kill log written");
        // Format must match the TUI writer byte-for-byte so
        // the daemon's LastExitProbe::snapshot consumer reads
        // them interchangeably. Parse as JSON to verify.
        let line = content.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(v["ts"], 1_700_000_000);
        assert_eq!(v["session_uid"], "ts-test");
        assert_eq!(v["pid"], 42);
        assert_eq!(v["comm"], "python");
        assert_eq!(v["argc"], 3);
        assert_eq!(v["argv_sha256_prefix"], "deadbeef");
        assert_eq!(v["rss_kb"], 123456);
        assert_eq!(v["soft_cap_bytes"], 6u64 * 1024 * 1024 * 1024);
        assert_eq!(v["hard_cap_bytes"], 10u64 * 1024 * 1024 * 1024);
        // Slice 10d watcher-fix #2: the forensic kill_status
        // field is part of the canonical record shape.
        assert_eq!(v["kill_status"], "killed_by_us");
    }

    #[test]
    fn write_kill_log_appends_multiple_records() {
        let tmp = TempDir::new().unwrap();
        for pid in &[10u32, 11, 12] {
            write_kill_log_to(
                tmp.path(),
                "ts-multi",
                std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + *pid as u64),
                *pid,
                "x",
                1,
                "abcd1234",
                100,
                1024,
                2048,
                KillStatus::KilledByUs,
            );
        }
        let path = tmp.path().join("ts-multi.jsonl");
        let content = std::fs::read_to_string(&path).expect("kill log written");
        assert_eq!(content.lines().count(), 3);
    }

    #[test]
    fn argv_sha256_prefix_is_deterministic_and_short() {
        let prefix1 = argv_sha256_prefix(&[b"python".to_vec(), b"foo.py".to_vec()]);
        let prefix2 = argv_sha256_prefix(&[b"python".to_vec(), b"foo.py".to_vec()]);
        assert_eq!(prefix1, prefix2);
        assert_eq!(prefix1.len(), 8); // 4 bytes × 2 hex chars
        // Different argv → different prefix (overwhelmingly likely).
        let other = argv_sha256_prefix(&[b"python".to_vec(), b"bar.py".to_vec()]);
        assert_ne!(prefix1, other);
    }

    /// End-to-end producer test. Drives the full watcher loop
    /// against a real spawned child + a synthetic cgroup
    /// directory. This is the "producer through consumer chain"
    /// proof that slice 10d-memory-cap-relocation calls for:
    ///
    /// 1. (Producer) Bump synthetic `memory.events`'s `high`
    ///    counter past baseline.
    /// 2. (Producer) Watcher polls, detects breach, opens a
    ///    pidfd against the real child, sends SIGTERM + SIGKILL.
    /// 3. (Producer) Watcher writes a JSONL kill record to
    ///    `<kills_dir>/<uid>.jsonl`.
    /// 4. (Acceptance) The real child is dead by the end of
    ///    the test (proves the kill path landed, not just a
    ///    no-op).
    /// 5. (Acceptance) The kill-log has at least one record
    ///    past the baseline offset 0, with the right uid + pid.
    ///
    /// Can't drive systemd-run from a unit test, so this
    /// substitutes a synthetic cgroup directory. The watcher
    /// doesn't care that it's not a real cgroupfs — it reads
    /// `cgroup.procs` and `memory.events` as plain files and
    /// signals PIDs via standard kill APIs. Enough surface to
    /// exercise every line of the producer code path EXCEPT the
    /// cgroupfs-specific kernel bits.
    ///
    /// **Timing**: stabilize (750ms) + followup (2s) +
    /// poll-interval (1s) = ~4s baseline. Tagged `#[ignore]` for
    /// normal runs (would slow the suite). Invoke with
    /// `cargo test -p cm-daemon producer_end_to_end -- --ignored`.
    ///
    /// Note: this test calls `run_watcher` directly rather than
    /// going through `spawn_watcher` because it doesn't need the
    /// spawn-injection surface — its job is to prove the *loop
    /// body* (cgroup poll → kill → JSONL write) on a real child.
    /// The spawn-failure path has its own focused test below
    /// (`spawn_watcher_propagates_thread_spawn_error`).
    #[test]
    #[ignore]
    fn producer_end_to_end_kills_and_writes_record() {
        let cgroup_dir = TempDir::new().expect("tempdir cgroup");
        let kills_dir = TempDir::new().expect("tempdir kills");
        let cgroup_path = cgroup_dir.path().to_path_buf();

        // Initial cgroup state: empty procs, high counter at 0.
        // Empty `cgroup.procs` at stabilize-time gives us an
        // empty `protected` set — PIDs added later are fair game.
        std::fs::write(cgroup_path.join("cgroup.procs"), b"")
            .expect("seed cgroup.procs");
        std::fs::write(cgroup_path.join("memory.events"), b"high 0\nmax 0\n")
            .expect("seed memory.events");

        let session_uid = "ts-prod-test-aaaa".to_string();
        let kills_dir_path = kills_dir.path().to_path_buf();
        let cgroup_path_thread = cgroup_path.clone();
        let uid_thread = session_uid.clone();
        let cell = test_cell(&cgroup_path, 0);
        let watcher = std::thread::spawn(move || {
            run_watcher(
                uid_thread,
                cgroup_path_thread,
                64 * 1024 * 1024,
                128 * 1024 * 1024,
                kills_dir_path,
                0, // initial_high seed (no prior breaches in synthetic cgroup)
                cell,
                None, // no restart coordinator in this test
                None, // fresh spawn — stabilize/followup run
            );
        });

        // Wait for stabilize (750ms) + followup (2s) to
        // complete. After this, `protected` is the procs
        // snapshot at stabilize-end (empty) plus followup-
        // admitted descendants (none).
        std::thread::sleep(Duration::from_millis(3000));

        // Spawn a real child the watcher can kill.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        let child_pid = child.id();

        // Add the child to cgroup.procs and bump the high
        // counter. Order matters: PID must be IN cgroup.procs
        // BEFORE the watcher's next poll, else handle_breach
        // finds no unprotected PIDs and bails.
        std::fs::write(
            cgroup_path.join("cgroup.procs"),
            format!("{}\n", child_pid).as_bytes(),
        )
        .expect("update cgroup.procs");
        std::fs::write(cgroup_path.join("memory.events"), b"high 1\nmax 0\n")
            .expect("bump high");

        // Worst case: 1s poll + 500ms SIGTERM grace + kill =
        // ~1.5s. Give it 4s slack for CI scheduling jitter.
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut child_dead = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            match child.try_wait() {
                Ok(Some(_status)) => {
                    child_dead = true;
                    break;
                }
                Ok(None) => continue,
                Err(_) => break,
            }
        }
        if !child_dead {
            let _ = child.kill();
            let _ = child.wait();
        }

        // Tear down the cgroup dir so the watcher exits its
        // loop (checks `cgroup_path.exists()` each iteration).
        drop(cgroup_dir);
        std::thread::sleep(Duration::from_millis(2500));
        let _ = watcher; // detach; loop exit happens on its own

        assert!(
            child_dead,
            "watcher failed to kill the offender PID {} within 4s — \
             producer path didn't fire (check memory.events polling \
             or kill::open pidfd permissions)",
            child_pid,
        );

        let log_path = kills_dir.path().join(format!("{}.jsonl", session_uid));
        let content = std::fs::read_to_string(&log_path)
            .expect("kill-log file should have been written by watcher");
        let lines: Vec<&str> = content.lines().collect();
        assert!(
            !lines.is_empty(),
            "kill-log written but empty — watcher's finalize_kill_outcome \
             gate (signal-actually-landed) rejected the kill unexpectedly",
        );
        let v: serde_json::Value =
            serde_json::from_str(lines[0]).expect("first kill-log line is valid JSON");
        assert_eq!(v["session_uid"], session_uid);
        assert_eq!(v["pid"], child_pid as u64);
        assert_eq!(v["soft_cap_bytes"], 64u64 * 1024 * 1024);
        // Sub-2b-3 review-10: the daemon-spawn watcher
        // startup site (methods.rs) used to pass literal
        // `0` for hard_cap_bytes, diverging from the
        // TUI-local watcher's actual MemoryMax. This
        // assertion pins that the JSONL record now carries
        // the value passed to `spawn_watcher` end-to-end —
        // a regression at the methods.rs site (or in the
        // watcher's plumbing of hard_cap_bytes through
        // run_watcher → write_kill_log_to) would surface
        // here as a 0 instead of 128 MiB.
        assert_eq!(
            v["hard_cap_bytes"], 128u64 * 1024 * 1024,
            "hard_cap_bytes in the JSONL must match what was \
             passed to spawn_watcher (review-10): got {}",
            v["hard_cap_bytes"],
        );
    }

    /// Failure-injection test for the spawn path. Pre-fix
    /// `spawn_watcher` called `.expect("spawn watcher thread")`
    /// which would PANIC on thread-spawn failure (e.g. ulimit
    /// on threads hit). The fix replaced that with explicit
    /// propagation via `WatcherSpawnFn`. This test pins the
    /// propagation: a spawn_fn returning `Err` must produce an
    /// `Err` from `spawn_watcher`, NOT a panic.
    ///
    /// Critical because `start_session` calls `spawn_watcher`
    /// just before the lock-held registry insert (see
    /// `daemon/src/control/methods.rs`). If the spawn panics,
    /// the panic unwinds with state in an inconsistent shape
    /// — same silent-degrade bug class as the prior fix2 /
    /// fix3 / fix5 / fix6 fixes.
    #[test]
    fn spawn_watcher_propagates_thread_spawn_error() {
        // Spawn fn that always fails. The body closure is moved
        // in but never called.
        let failing: WatcherSpawnFn = Box::new(|_name, _body| {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "injected-spawn-failure",
            ))
        });
        let result = spawn_watcher(
            "ts-spawn-fail-test".into(),
            std::path::PathBuf::from("/tmp/not-a-real-cgroup"),
            64 * 1024 * 1024,
            128 * 1024 * 1024,
            std::path::PathBuf::from("/tmp/not-a-real-kills-dir"),
            0, // initial_high seed
            failing,
            None, // no restart coordinator
            None, // fresh spawn
        );
        let err = result.expect_err(
            "spawn_watcher must return Err on spawn-fn failure — not panic",
        );
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::WouldBlock,
            "kind must propagate from the injected error so callers can \
             distinguish resource exhaustion from other failures: {:?}",
            err.kind(),
        );
        assert!(
            err.to_string().contains("injected-spawn-failure"),
            "error message must propagate from the spawn_fn: {}",
            err,
        );
    }

    /// Symmetric happy-path test: real `default_watcher_spawn_fn`
    /// must produce a runnable thread. Body is a quick exit so
    /// the test doesn't park a thread.
    #[test]
    fn spawn_watcher_with_default_fn_returns_runnable_handle() {
        // We can't realistically run the watcher loop here
        // (would need a real cgroup), but we can prove the
        // factory wiring by passing a body that immediately
        // exits and verifying the handle is joinable.
        let body_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let body_ran_c = body_ran.clone();

        // We bypass the full spawn_watcher signature (which
        // builds the run_watcher body) by hand-rolling against
        // default_watcher_spawn_fn so we can supply a trivial
        // body. The contract under test is: the default factory
        // produces real threads.
        let spawn_fn = default_watcher_spawn_fn();
        let handle = spawn_fn(
            "test-default-spawn".into(),
            Box::new(move || {
                body_ran_c.store(true, std::sync::atomic::Ordering::SeqCst);
            }),
        )
        .expect("default spawn fn should succeed under normal conditions");
        handle.join().expect("thread should join cleanly");
        assert!(body_ran.load(std::sync::atomic::Ordering::SeqCst));
    }

    // ============================================================
    // Slice 10d watcher-fix #2: record-on-every-breach. The
    // watcher must write a JSONL record even when it can't kill
    // anything (empty cgroup / all PIDs protected / target
    // already dead) so the kernel's eventual `MemoryMax` kill
    // of the agent is correctly attributed via
    // `LastExitProbe::snapshot`'s record-existence-past-baseline
    // check. Pre-fix the watcher returned silently → silent
    // `memory_cap_kill: false`.
    //
    // We test by directly driving `handle_breach` against a
    // synthetic cgroup directory. This bypasses the
    // memory.events polling loop (covered by the e2e test) and
    // focuses on the breach-handling decision tree.
    // ============================================================

    #[test]
    fn handle_breach_with_empty_cgroup_writes_no_pids_record() {
        let cgroup_dir = TempDir::new().unwrap();
        let kills_dir = TempDir::new().unwrap();
        // Empty `cgroup.procs` — the kernel has cleaned up
        // all PIDs but the breach was already counted.
        std::fs::write(cgroup_dir.path().join("cgroup.procs"), b"").unwrap();

        handle_breach(
            "ts-empty-test",
            cgroup_dir.path(),
            &HashSet::new(),
            64 * 1024 * 1024,
            0,
            kills_dir.path(),
        );

        let log_path = kills_dir.path().join("ts-empty-test.jsonl");
        let content = std::fs::read_to_string(&log_path)
            .expect("watcher must write a record even when cgroup is empty");
        let line = content.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["kill_status"], "no_pids");
        assert_eq!(v["pid"], 0);
        assert_eq!(v["rss_kb"], 0);
    }

    #[test]
    fn handle_breach_with_all_pids_protected_writes_protected_record() {
        let cgroup_dir = TempDir::new().unwrap();
        let kills_dir = TempDir::new().unwrap();
        // PID list = test process's own PID (guaranteed alive,
        // and reading /proc/<self>/status returns a real RSS).
        let test_pid = std::process::id();
        std::fs::write(
            cgroup_dir.path().join("cgroup.procs"),
            format!("{}\n", test_pid).as_bytes(),
        )
        .unwrap();
        // Mark test_pid as protected (the agent's own process
        // tree).
        let mut protected = HashSet::new();
        protected.insert(test_pid);

        handle_breach(
            "ts-protected-test",
            cgroup_dir.path(),
            &protected,
            64 * 1024 * 1024,
            0,
            kills_dir.path(),
        );

        let log_path = kills_dir.path().join("ts-protected-test.jsonl");
        let content = std::fs::read_to_string(&log_path)
            .expect("watcher must write a record when all PIDs are protected");
        let line = content.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["kill_status"], "protected");
        // PID and RSS surface the protected target for
        // forensic correlation.
        assert_eq!(v["pid"], test_pid as u64);
        // RSS is non-zero (we're a real Rust test process
        // with allocated memory).
        assert!(
            v["rss_kb"].as_u64().unwrap_or(0) > 0,
            "RSS must be non-zero for a live test process",
        );
    }

    /// Slice 10d watcher-fix #2 named acceptance: a Protected
    /// record dropped in the kill-log past the baseline must
    /// cause `LastExitProbe::snapshot` to return
    /// `memory_cap_kill: true`. This proves the eventual kernel
    /// `MemoryMax` kill of the agent (which the watcher refused
    /// to perform) is still correctly attributed.
    ///
    /// We construct the LastExitProbe by hand (using its public
    /// constructor) rather than going through a real session
    /// spawn — the test surface is the consumer's interpretation
    /// of the record file, not the spawn pipeline.
    #[test]
    fn protected_record_past_baseline_flips_memory_cap_kill_to_true() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-attribution-test";

        // Pretend baseline was captured when the file was
        // empty/missing.
        let baseline: u64 = 0;

        // Watcher writes a Protected record (the agent is the
        // sole survivor in the cgroup; we refused to kill it).
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            12345, // some protected PID
            "agent",
            1,
            "feedface",
            500_000,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );

        // Consumer side: build a LastExitProbe with that
        // kills_dir + baseline + a kernel exit (the kernel
        // SIGKILL'd the agent post-MemoryMax).
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            baseline,
        );
        // Kernel signal-killed via MemoryMax: code=None,
        // signal=Some(9). The new schema (slice 10d watcher-
        // fix #1.5) splits code and signal cleanly so
        // `is_cap_kill` can distinguish from a clean exit.
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(9),
        });

        let (code, memory_cap_kill) = probe.snapshot();
        assert_eq!(code, None, "signal-killed exit has no WEXITSTATUS code");
        assert!(
            memory_cap_kill,
            "Protected record past baseline + signal exit must flip \
             memory_cap_kill to TRUE — the watcher observed the breach + \
             kernel did the kill",
        );
    }

    /// Symmetric: NoPids record + signal exit flips
    /// memory_cap_kill to true. Same join via `is_cap_kill`.
    #[test]
    fn no_pids_record_past_baseline_with_signal_flips_memory_cap_kill_to_true() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-no-pids-attribution";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            0,
            "",
            0,
            "00000000",
            0,
            64 * 1024 * 1024,
            0,
            KillStatus::NoPids,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(9),
        });
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(
            memory_cap_kill,
            "NoPids record + signal exit must flag cap-kill",
        );
    }

    /// Slice 10d watcher-fix #1.5 named acceptance — the
    /// transient-soft-limit-recovery case. A `protected`
    /// record past baseline followed by a CLEAN exit must
    /// NOT flag memory_cap_kill. Pre-fix-#1.5 this returned
    /// true (false positive toast); post-fix the
    /// kill_status + signal join correctly returns false.
    #[test]
    fn protected_record_plus_clean_exit_does_not_flip_memory_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-transient-breach";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            12345,
            "agent",
            1,
            "feedface",
            500_000,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        // Clean exit: code=Some(0), signal=None. The
        // process recovered from the soft-limit breach.
        probe.set_kernel(crate::session::KernelExitStatus {
            code: Some(0),
            signal: None,
        });
        let (code, memory_cap_kill) = probe.snapshot();
        assert_eq!(code, Some(0));
        assert!(
            !memory_cap_kill,
            "transient soft-limit breach + clean exit must NOT flag \
             memory_cap_kill — would surface a phantom toast for an exit \
             that today wouldn't have shown one (violates 'indistinguishable \
             from today's signal 9 toast')",
        );
    }

    /// Symmetric: NoPids record + clean exit also returns
    /// false (transient scope-teardown race).
    #[test]
    fn no_pids_record_plus_clean_exit_does_not_flip_memory_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-no-pids-clean";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            0,
            "",
            0,
            "00000000",
            0,
            64 * 1024 * 1024,
            0,
            KillStatus::NoPids,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: Some(0),
            signal: None,
        });
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(!memory_cap_kill);
    }

    // ============================================================
    // Slice 10d watcher-fix #4: operator-kill end-to-end tests
    // through `LastExitProbe`. These exercise the same join
    // logic as the reaper.rs unit tests but through the probe's
    // `operator_kill_requested` flag, which is what production
    // observes.
    // ============================================================

    /// Capped session, transient `protected` record past
    /// baseline, operator A-w → memory_cap_kill: false.
    /// Pre-fix #4 this returned true; the false-positive
    /// toast is what fix #4 closes.
    #[test]
    fn capped_session_protected_record_plus_operator_kill_is_not_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-op-kill-protected";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            12345,
            "agent",
            1,
            "feedface",
            500_000,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(9), // pidfd-SIGKILL from kill_session RPC
        });
        // Operator initiates A-w BEFORE the SIGKILL — same
        // ordering production observes (the `kill_session`
        // handler calls `mark_operator_kill_requested` before
        // dropping the session, which triggers Drop's
        // pidfd-SIGKILL).
        probe.mark_operator_kill_requested();
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(
            !memory_cap_kill,
            "operator A-w on a session with a transient protected breach must \
             NOT render as cap-kill (slice 10d watcher-fix #4)",
        );
    }

    /// Slice 10d watcher-fix #5: capped session,
    /// `killed_by_us` past baseline (the watcher attempted a
    /// SIGKILL), operator A-w fires concurrently → operator
    /// override applies; memory_cap_kill = FALSE.
    ///
    /// The `killed_by_us` record proves the watcher's signal
    /// was issued, not that it landed. In a real race
    /// (watcher's pidfd_send_signal and operator's
    /// pidfd_send_signal both racing toward the same child)
    /// either signal can win, and the operator-initiated one
    /// shouldn't get attributed to the cap. Conservative rule:
    /// when operator_kill_requested is set, never claim
    /// cap-kill.
    ///
    /// Pre-#5 this asserted `true`; rename + invert reflects
    /// the new attribution.
    #[test]
    fn capped_session_killed_by_us_plus_operator_kill_attributes_to_operator() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-cap-race-op";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            12345,
            "agent",
            1,
            "feedface",
            500_000,
            64 * 1024 * 1024,
            0,
            KillStatus::KilledByUs,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(9),
        });
        probe.mark_operator_kill_requested();
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(
            !memory_cap_kill,
            "killed_by_us + operator kill race: operator override wins \
             (watcher's record proves attempt, not delivery; operator's \
              pidfd-SIGKILL may have been the actual proximate cause)",
        );
    }

    /// Slice 10d watcher-fix #5 named acceptance — full E2E
    /// of the race. Watcher writes `killed_by_us` past
    /// baseline (so the watcher believed it killed); operator
    /// then marks the kill-requested flag (simulating the
    /// `kill_session` RPC firing concurrently); kernel exit
    /// reflects the SIGKILL. The probe's `memory_cap_kill`
    /// must be FALSE because operator-driven attribution is
    /// the conservative choice. The race is rare; the
    /// trade-off is documented on `is_cap_kill`.
    #[test]
    fn race_killed_by_us_versus_operator_attributes_to_operator() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-race-named-acceptance";

        // The probe ITSELF survives the session via Arc; this
        // mirrors the production lifetime where `last_exit` on
        // a DaemonSession is `Arc<LastExitProbe>` and the
        // attach-stream End-frame consumer holds the Arc
        // independently.
        let probe = std::sync::Arc::new(crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0, // baseline 0 — fresh session
        ));

        // Step 1: watcher detects breach, writes
        // `killed_by_us` (i.e., the watcher's
        // pidfd_send_signal returned 0; from the watcher's
        // POV the kill went out).
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            12345,
            "agent",
            1,
            "feedface",
            500_000,
            64 * 1024 * 1024,
            0,
            KillStatus::KilledByUs,
        );

        // Step 2: in a concurrent timeline, the operator's
        // kill_session RPC fires (`mark_operator_kill_requested`
        // is what the daemon's `kill_session` handler does
        // BEFORE issuing its own pidfd-SIGKILL).
        probe.mark_operator_kill_requested();

        // Step 3: child dies — SIGKILL exit. From waitpid we
        // can't tell which pidfd_send_signal actually delivered
        // the killing signal (could have been either, or both
        // — pidfd-SIGKILL is idempotent).
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(9),
        });

        // Step 4: End-frame consumer snapshots. Conservative
        // rule: operator override wins.
        let (code, memory_cap_kill) = probe.snapshot();
        assert_eq!(code, None, "signal-killed exit has no WEXITSTATUS");
        assert!(
            !memory_cap_kill,
            "killed_by_us + operator kill race attributes to the operator \
             (slice 10d watcher-fix #5 named acceptance: conservative \
             override avoids surfacing a cap-kill toast on a user-initiated \
             exit, accepting the rare lost-attribution case)",
        );
    }

    /// Capped session, kernel `MemoryMax` SIGKILL with
    /// `protected` record, NO operator kill → memory_cap_kill:
    /// true. The kernel-driven kill case fix #1.5 made true;
    /// fix #4 keeps it that way.
    #[test]
    fn capped_session_kernel_sigkill_no_operator_is_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-kernel-kill";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            12345,
            "agent",
            1,
            "feedface",
            500_000,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(9),
        });
        // No operator A-w — kernel MemoryMax did the kill.
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(memory_cap_kill);
    }

    /// Uncapped session (no kills_dir, so no records can exist
    /// past baseline), operator A-w → memory_cap_kill: false.
    /// The probe's no-kills-dir branch returns false directly;
    /// the operator flag is incidental.
    #[test]
    fn uncapped_session_operator_kill_is_not_cap_kill() {
        let uid = "ts-uncapped";
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            None, // no kills_dir — uncapped session
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(9),
        });
        probe.mark_operator_kill_requested();
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(
            !memory_cap_kill,
            "uncapped session can't be a cap kill, regardless of operator action",
        );
    }

    // ============================================================
    // Slice 10d watcher-fix #3: startup-window — anchor the
    // breach baseline BEFORE stabilize, so an early OOM that
    // increments `memory.events high` during the 2.75s
    // stabilize+followup window is still observed after the
    // watcher exits stabilize.
    // ============================================================

    /// Synthetic cgroup: high=5 at watcher start. While the
    /// watcher is in stabilize/followup, externally bump high
    /// to 6. After stabilize completes, the first poll must
    /// detect the breach (and `handle_breach` write a record
    /// since the cgroup is empty → NoPids status).
    ///
    /// Pre-fix #3 the watcher captured `last_high = 6` AFTER
    /// stabilize, treating the bump as the new baseline and
    /// silently swallowing the early OOM. Post-fix the
    /// `initial_high = 5` anchor at the TOP of run_watcher
    /// means the post-stabilize poll sees current=6 > 5 and
    /// fires.
    ///
    /// Real timing: stabilize=750ms, followup=2000ms, poll
    /// interval=1000ms → first detection lands ~3.75s after
    /// watcher start. Give 5s slack.
    #[test]
    #[ignore]
    fn watcher_detects_breach_during_stabilize_window() {
        let cgroup_dir = TempDir::new().unwrap();
        let kills_dir = TempDir::new().unwrap();
        let cgroup_path = cgroup_dir.path().to_path_buf();

        // Initial state: cgroup empty, high counter already
        // at 5 (simulating prior recorded breaches).
        std::fs::write(cgroup_path.join("cgroup.procs"), b"").unwrap();
        std::fs::write(cgroup_path.join("memory.events"), b"high 5\nmax 0\n").unwrap();

        let uid = "ts-startup-window".to_string();
        let kills_dir_path = kills_dir.path().to_path_buf();
        let cgroup_path_thread = cgroup_path.clone();
        let uid_thread = uid.clone();
        // Slice 10d watcher-fix #6: the caller-supplied seed.
        // For this test the seed equals the pre-bump value (5)
        // so the post-bump value (6) reads as a breach.
        let cell = test_cell(&cgroup_path, 5);
        let watcher = std::thread::spawn(move || {
            run_watcher(
                uid_thread,
                cgroup_path_thread,
                64 * 1024 * 1024,
                0,
                kills_dir_path,
                5, // initial_high seed — matches pre-bump value
                cell,
                None,
                None,
            );
        });

        // During stabilize (within first 750ms), bump high.
        std::thread::sleep(Duration::from_millis(200));
        std::fs::write(cgroup_path.join("memory.events"), b"high 6\nmax 0\n")
            .expect("bump high during stabilize");

        // Wait for stabilize (750ms) + followup (2s) + first
        // poll (1s) + slack.
        std::thread::sleep(Duration::from_millis(5000));

        // Tear down so the watcher exits naturally.
        drop(cgroup_dir);
        std::thread::sleep(Duration::from_millis(2500));
        let _ = watcher;

        // Assert: a record was written (the breach was seen
        // post-stabilize). Pre-fix this assertion would fail
        // because last_high = 6 after stabilize and the next
        // poll's `current_high == last_high` made the breach
        // invisible.
        let log_path = kills_dir.path().join(format!("{}.jsonl", uid));
        let content = std::fs::read_to_string(&log_path)
            .expect("kill-log written for window-internal breach");
        assert!(
            !content.lines().next().unwrap_or("").is_empty(),
            "watcher must observe the during-stabilize high-counter bump",
        );
        let line = content.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        // Cgroup was empty → kill_status: "no_pids".
        assert_eq!(v["kill_status"], "no_pids");
    }

    // ============================================================
    // Slice 10d watcher-fix #6: pre-watcher startup race. The
    // earlier #3 fix anchored the seed at watcher-start, but
    // the ~1s window between cgroup discovery (where the
    // caller `start_session` reads `memory.events.high`) and
    // `spawn_watcher` returning was still uncovered. #6
    // pushes the seed up to the caller's read-time.
    // ============================================================

    /// Synthetic cgroup: `memory.events high=5` at seed-read
    /// time. Caller passes `initial_high=5`. EXTERNALLY bump
    /// the file to high=6 BEFORE `run_watcher` would have done
    /// its own first poll. Watcher first poll should detect
    /// the breach because the seed is 5 (the pre-bump value).
    ///
    /// Pre-fix #6 the watcher would have done its own
    /// `read_memory_high_count` at the top of `run_watcher`,
    /// captured 6, then `current_high (6) <= last_high (6)` →
    /// false negative.
    #[test]
    #[ignore]
    fn watcher_detects_breach_during_pre_watcher_window() {
        let cgroup_dir = TempDir::new().unwrap();
        let kills_dir = TempDir::new().unwrap();
        let cgroup_path = cgroup_dir.path().to_path_buf();

        // Caller's pre-spawn state: high=5.
        std::fs::write(cgroup_path.join("cgroup.procs"), b"").unwrap();
        std::fs::write(cgroup_path.join("memory.events"), b"high 5\nmax 0\n").unwrap();

        // Caller (start_session) reads the seed here, BEFORE
        // spawn_watcher would be invoked in production. This
        // is the load-bearing step #6 introduces.
        let seed_initial_high = read_memory_events_high(&cgroup_path);
        assert_eq!(seed_initial_high, 5);

        // Simulate the pre-watcher window: external bump
        // happens BEFORE spawn_watcher (or rather, before
        // run_watcher does anything). Production analog: child
        // spawn → cgroup discovery → seed read → child OOMs
        // here → spawn_watcher → run_watcher.
        std::fs::write(cgroup_path.join("memory.events"), b"high 6\nmax 0\n")
            .expect("bump high during pre-watcher window");

        let uid = "ts-pre-watcher-window".to_string();
        let kills_dir_path = kills_dir.path().to_path_buf();
        let cgroup_path_thread = cgroup_path.clone();
        let uid_thread = uid.clone();
        let cell = test_cell(&cgroup_path, seed_initial_high);
        let watcher = std::thread::spawn(move || {
            run_watcher(
                uid_thread,
                cgroup_path_thread,
                64 * 1024 * 1024,
                0,
                kills_dir_path,
                seed_initial_high, // = 5, the pre-bump value
                cell,
                None,
                None,
            );
        });

        // Wait for stabilize+followup+first poll + slack.
        std::thread::sleep(Duration::from_millis(5000));

        drop(cgroup_dir);
        std::thread::sleep(Duration::from_millis(2500));
        let _ = watcher;

        let log_path = kills_dir.path().join(format!("{}.jsonl", uid));
        let content = std::fs::read_to_string(&log_path)
            .expect("kill-log written for pre-watcher-window breach");
        assert!(
            !content.lines().next().unwrap_or("").is_empty(),
            "watcher must detect the pre-watcher-window bump (seed=5, file=6)",
        );
        let line = content.lines().next().unwrap();
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["kill_status"], "no_pids");
    }

    // ============================================================
    // Slice 10d watcher-fix #7: SIGKILL specifically. The
    // `protected`/`no_pids`/`already_dead` rows flip only on
    // signal == SIGKILL. Other signals (SIGTERM from
    // operator-friendly cleanup, SIGINT from Ctrl-C, SIGHUP
    // from controlling-terminal close, SIGABRT from
    // panic/assert) are user-driven and must NOT render as
    // cap-kill on a transient breach record.
    // ============================================================

    /// Capped session, transient `protected` record, SIGTERM
    /// exit (operator-friendly cleanup signal, e.g. service
    /// manager wanting graceful shutdown) → memory_cap_kill:
    /// false. Pre-fix #7 this returned true (any-signal rule).
    #[test]
    fn capped_session_protected_record_plus_sigterm_is_not_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-sigterm-not-cap";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            12345,
            "agent",
            1,
            "feedface",
            500_000,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(libc::SIGTERM), // = 15, not SIGKILL
        });
        // No operator_kill_requested — the SIGTERM came from
        // somewhere else (init system, ctrl-C from controlling
        // tty, manual `kill -TERM`, …).
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(
            !memory_cap_kill,
            "SIGTERM is not how the kernel's MemoryMax kills; transient \
             breach + SIGTERM must NOT flag cap-kill (slice 10d watcher-fix #7)",
        );
    }

    /// Symmetric for SIGINT (Ctrl-C analog in a detached
    /// agent).
    #[test]
    fn capped_session_protected_record_plus_sigint_is_not_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-sigint-not-cap";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            1,
            "x",
            0,
            "00000000",
            0,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(libc::SIGINT),
        });
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(!memory_cap_kill);
    }

    /// Symmetric for SIGABRT (panic / assertion failure).
    #[test]
    fn capped_session_protected_record_plus_sigabrt_is_not_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-sigabrt-not-cap";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            1,
            "x",
            0,
            "00000000",
            0,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(libc::SIGABRT),
        });
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(!memory_cap_kill);
    }

    /// SIGKILL is the only signal that flips it (the kernel's
    /// MemoryMax send). Sanity-check the constant resolves.
    #[test]
    fn capped_session_protected_record_plus_sigkill_is_cap_kill() {
        let kills_dir = TempDir::new().unwrap();
        let uid = "ts-sigkill-is-cap";
        write_kill_log_to(
            kills_dir.path(),
            uid,
            std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            1,
            "x",
            0,
            "00000000",
            0,
            64 * 1024 * 1024,
            0,
            KillStatus::Protected,
        );
        let probe = crate::session::LastExitProbe::new(
            uid.into(),
            Some(kills_dir.path().to_path_buf()),
            0,
        );
        probe.set_kernel(crate::session::KernelExitStatus {
            code: None,
            signal: Some(libc::SIGKILL),
        });
        let (_, memory_cap_kill) = probe.snapshot();
        assert!(memory_cap_kill, "SIGKILL on a capped session with transient breach IS cap-kill");
    }

    // ============================================================
    // DESIGN_SEAMLESS_RESTART phase 4d (R12): watcher checkpoints
    // across the re-exec swap — serde round trip, live-state cell,
    // park/ack protocol, and restored-policy kill decisions.
    // ============================================================

    /// The checkpoint round-trips through the manifest's opaque
    /// `serde_json::Value` slot losslessly, the composer sorts the
    /// protected set deterministically, and the parse gate refuses
    /// (loudly → `None`) a wrong version and garbage.
    #[test]
    fn watcher_checkpoint_serde_round_trip() {
        let live = WatcherLiveState {
            cgroup_path: PathBuf::from("/sys/fs/cgroup/u/cm-sess-x.scope"),
            protected: Some([31337u32, 42, 7].into_iter().collect()),
            last_high: 9,
        };
        let cp = live.checkpoint(12_345);
        assert_eq!(cp.version, WATCHER_CHECKPOINT_VERSION);
        assert_eq!(cp.cgroup_path, "/sys/fs/cgroup/u/cm-sess-x.scope");
        assert_eq!(
            cp.protected.as_deref(),
            Some(&[7u32, 42, 31337][..]),
            "protected set serializes sorted (deterministic manifests)"
        );
        assert_eq!(cp.last_high, 9);
        assert_eq!(cp.kills_baseline, 12_345);

        // Through the opaque slot and back.
        let value = serde_json::to_value(&cp).expect("serialize");
        let back = parse_watcher_checkpoint("ts-cp", &value)
            .expect("round trip parses");
        assert_eq!(back, cp);

        // Not-yet-finalized protected set survives as None.
        let live_unfinalized = WatcherLiveState {
            cgroup_path: PathBuf::from("/x"),
            protected: None,
            last_high: 0,
        };
        let cp2 = live_unfinalized.checkpoint(0);
        assert_eq!(cp2.protected, None);
        let v2 = serde_json::to_value(&cp2).expect("serialize");
        assert_eq!(
            parse_watcher_checkpoint("ts-cp", &v2).unwrap().protected,
            None
        );

        // Version gate: a future version is a policy reset, not a guess.
        let mut wrong = serde_json::to_value(&cp).unwrap();
        wrong["version"] = serde_json::json!(WATCHER_CHECKPOINT_VERSION + 1);
        assert!(parse_watcher_checkpoint("ts-cp", &wrong).is_none());

        // Garbage: same.
        assert!(parse_watcher_checkpoint(
            "ts-cp",
            &serde_json::json!({"not": "a checkpoint"})
        )
        .is_none());
        assert!(
            parse_watcher_checkpoint("ts-cp", &serde_json::json!(17)).is_none()
        );
    }

    /// `spawn_watcher` seeds the shared cell BEFORE the thread runs:
    /// a checkpoint taken the instant the spawn returns reads the
    /// honest initial state (restored set / not-yet-finalized None,
    /// plus the seed watermark). Proven with a spawn_fn that never
    /// runs the body.
    #[test]
    fn spawn_watcher_seeds_live_state_cell_at_birth() {
        let inert: WatcherSpawnFn = Box::new(|name, _body| {
            std::thread::Builder::new().name(name).spawn(|| {})
        });
        let restored: HashSet<u32> = [11u32, 22].into_iter().collect();
        let spawned = spawn_watcher(
            "ts-cell-birth".into(),
            PathBuf::from("/tmp/ts-cell-birth-cgroup"),
            64 * 1024 * 1024,
            0,
            PathBuf::from("/tmp/ts-cell-birth-kills"),
            4,
            inert,
            None,
            Some(restored.clone()),
        )
        .expect("spawn");
        let st = spawned.state.lock().unwrap();
        assert_eq!(st.cgroup_path, PathBuf::from("/tmp/ts-cell-birth-cgroup"));
        assert_eq!(st.protected.as_ref(), Some(&restored));
        assert_eq!(st.last_high, 4);
        drop(st);
        spawned.handle.join().expect("inert body joins");

        // Fresh spawn: protected is None until stabilize/followup
        // finalize it.
        let inert: WatcherSpawnFn = Box::new(|name, _body| {
            std::thread::Builder::new().name(name).spawn(|| {})
        });
        let spawned = spawn_watcher(
            "ts-cell-fresh".into(),
            PathBuf::from("/tmp/ts-cell-fresh-cgroup"),
            64 * 1024 * 1024,
            0,
            PathBuf::from("/tmp/ts-cell-fresh-kills"),
            7,
            inert,
            None,
            None,
        )
        .expect("spawn");
        assert!(spawned.state.lock().unwrap().protected.is_none());
        assert_eq!(spawned.state.lock().unwrap().last_high, 7);
        spawned.handle.join().expect("inert body joins");
    }

    /// The park/ack protocol against a REAL running watcher: a
    /// restored watcher registers `memcap-watcher:<uid>` with the
    /// coordinator, inherits a pause requested before it was spawned
    /// (the mid-quiesce registration rule), acks at its main-loop
    /// safe point, and the quiesce barrier completes; abort resumes
    /// it and it exits normally when the cgroup vanishes.
    #[test]
    fn restored_watcher_registers_parks_and_acks_quiesce() {
        let cgroup_dir = TempDir::new().unwrap();
        let kills_dir = TempDir::new().unwrap();
        std::fs::write(cgroup_dir.path().join("cgroup.procs"), b"").unwrap();
        std::fs::write(
            cgroup_dir.path().join("memory.events"),
            b"high 0\nmax 0\n",
        )
        .unwrap();

        let state = Arc::new(Mutex::new(crate::state::DaemonState::new()));
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);

        // Pause FIRST, then spawn: the watcher's handle is born with
        // the request set and parks at its first safe point, so the
        // ack is deterministic (no sleep-length race).
        let quiesce =
            crate::restart_coordinator::begin(&state).expect("begin");

        let spawned = spawn_watcher(
            "ts-park-ack".into(),
            cgroup_dir.path().to_path_buf(),
            64 * 1024 * 1024,
            0,
            kills_dir.path().to_path_buf(),
            0,
            default_watcher_spawn_fn(),
            Some(Arc::downgrade(&coordinator)),
            // Restored ⇒ straight to the main loop's park point.
            Some(HashSet::new()),
        )
        .expect("spawn watcher");

        quiesce
            .wait_quiesced(Duration::from_secs(10))
            .expect("watcher must ack its safe point (park) for the barrier");

        // Abort resumes the watcher; tearing the cgroup down makes it
        // exit its loop, proving resume actually released the park.
        quiesce.abort();
        drop(cgroup_dir);
        spawned
            .handle
            .join()
            .expect("resumed watcher exits once the cgroup vanishes");
    }

    /// Same protocol for a FRESH watcher parked in its stabilize
    /// window — the barrier must not have to wait out the ~2.75s
    /// stabilize+followup phases for an ack.
    #[test]
    fn fresh_watcher_acks_quiesce_during_stabilize() {
        let cgroup_dir = TempDir::new().unwrap();
        let kills_dir = TempDir::new().unwrap();
        std::fs::write(cgroup_dir.path().join("cgroup.procs"), b"").unwrap();
        std::fs::write(
            cgroup_dir.path().join("memory.events"),
            b"high 0\nmax 0\n",
        )
        .unwrap();

        let state = Arc::new(Mutex::new(crate::state::DaemonState::new()));
        let coordinator =
            Arc::clone(&state.lock().unwrap().restart_coordinator);
        let quiesce =
            crate::restart_coordinator::begin(&state).expect("begin");

        let spawned = spawn_watcher(
            "ts-park-stabilize".into(),
            cgroup_dir.path().to_path_buf(),
            64 * 1024 * 1024,
            0,
            kills_dir.path().to_path_buf(),
            0,
            default_watcher_spawn_fn(),
            Some(Arc::downgrade(&coordinator)),
            None, // fresh — parks at the stabilize-loop safe point
        )
        .expect("spawn watcher");

        quiesce
            .wait_quiesced(Duration::from_secs(10))
            .expect("stabilize-phase watcher must still ack promptly");
        quiesce.abort();
        drop(cgroup_dir);
        spawned
            .handle
            .join()
            .expect("watcher exits once the cgroup vanishes");
    }

    /// THE R12 regression: a watcher restored from a checkpoint makes
    /// the PRE-SWAP policy's kill decisions, which DIFFER from a
    /// recomputed watcher's on both axes:
    ///
    /// - **protected set**: the checkpoint protects only the test
    ///   process; a real `sleep` child sits in the current cgroup and
    ///   is NOT in the checkpointed set. A recomputed watcher would
    ///   have snapshot BOTH pids at stabilize time (both were in
    ///   `cgroup.procs`) and refused to kill anything (`protected`
    ///   record, no signal). The restored watcher must kill the child.
    /// - **last_high**: the checkpoint's watermark is 4 while the
    ///   file already reads 5 — a breach that landed mid-swap. A
    ///   recomputed watcher re-seeding from the current counter
    ///   (`read_memory_events_high` → 5) would baseline the breach
    ///   away and never fire; the restored one fires on its FIRST
    ///   poll with no post-restore bump at all.
    ///
    /// Runtime ~2s (1s poll + 500ms SIGTERM grace + margins) — kept
    /// in the normal suite because it pins the slice's core behavior.
    #[test]
    fn restored_watcher_preserves_protected_set_and_last_high() {
        let cgroup_dir = TempDir::new().unwrap();
        let kills_dir = TempDir::new().unwrap();
        let cgroup_path = cgroup_dir.path().to_path_buf();

        // A real child the restored policy is allowed to kill.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        let child_pid = child.id();
        let own_pid = std::process::id();

        // Current cgroup state at adoption: BOTH pids present, and
        // the high counter one past the checkpointed watermark (the
        // mid-swap breach).
        std::fs::write(
            cgroup_path.join("cgroup.procs"),
            format!("{}\n{}\n", own_pid, child_pid).as_bytes(),
        )
        .unwrap();
        std::fs::write(cgroup_path.join("memory.events"), b"high 5\nmax 0\n")
            .unwrap();

        let restored: HashSet<u32> = [own_pid].into_iter().collect();
        let spawned = spawn_watcher(
            "ts-restored-policy".into(),
            cgroup_path.clone(),
            64 * 1024 * 1024,
            128 * 1024 * 1024,
            kills_dir.path().to_path_buf(),
            4, // checkpoint.last_high — one BELOW the file's 5
            default_watcher_spawn_fn(),
            None,
            Some(restored),
        )
        .expect("spawn watcher");

        // First poll lands ~1s in (no stabilize/followup for a
        // restored watcher), SIGTERM grace is 500ms; allow 5s slack.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut child_dead = false;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            match child.try_wait() {
                Ok(Some(_)) => {
                    child_dead = true;
                    break;
                }
                Ok(None) => continue,
                Err(_) => break,
            }
        }
        if !child_dead {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(
            child_dead,
            "restored watcher must kill the divergent pid {} — it is in \
             the CURRENT cgroup but NOT in the checkpointed protected \
             set, and the breach (file high 5 > checkpoint last_high 4) \
             landed mid-swap with no post-restore bump",
            child_pid,
        );

        // The record attributes the restored policy's kill.
        let log_path =
            kills_dir.path().join("ts-restored-policy.jsonl");
        let content =
            std::fs::read_to_string(&log_path).expect("kill log written");
        let v: serde_json::Value =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(v["kill_status"], "killed_by_us");
        assert_eq!(v["pid"], child_pid as u64);
        assert_eq!(v["session_uid"], "ts-restored-policy");

        // The cell published the consumed watermark — the NEXT
        // checkpoint would carry 5, not re-report this breach.
        assert_eq!(spawned.state.lock().unwrap().last_high, 5);
        // And the restored protected set was published as finalized.
        assert_eq!(
            spawned.state.lock().unwrap().protected.as_ref().map(|s| {
                let mut v: Vec<u32> = s.iter().copied().collect();
                v.sort_unstable();
                v
            }),
            Some(vec![own_pid]),
        );

        drop(cgroup_dir);
        spawned
            .handle
            .join()
            .expect("watcher exits once the cgroup vanishes");
    }
}
