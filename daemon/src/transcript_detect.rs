//! Daemon-side transcript-path detector (sub-2b-3 review-2 #2).
//!
//! Claude/Codex transcript files are born AFTER the agent process
//! is spawned — the path isn't known to the daemon at
//! `mcp_start_session` return time. Pre-fix, the daemon depended
//! on the TUI's `detect_session_id` heuristic + the
//! `session.set_transcript_path` RPC to fill in the path post-
//! spawn. For TUI-spawned sessions that flow still works (the TUI
//! attaches and detects). For sessions spawned by an MCP-calling
//! agent via `mcp_start_session` there is no TUI participant in
//! the spawn — so the daemon's `resolve_authorized_session` keeps
//! returning `state="pending"` forever, breaking the named
//! acceptance criterion "an MCP agent inside a daemon-spawned
//! session can call start_session / read_session_output".
//!
//! This module ports the TUI's heuristic into a daemon-internal
//! detector. It's scoped to `mcp_start_session`-spawned sessions
//! only — TUI-spawned sessions continue to route discovery
//! through the TUI side. We do NOT relocate the TUI-wide
//! detector here; that's 10e/10f territory.
//!
//! ## Heuristic (mirrors `tui/src/app.rs::detect_session_id`
//! and `tui/src/app.rs::detect_codex_session_id`)
//!
//! - **Claude Code**: each session writes
//!   `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`, where
//!   `encoded-cwd` is the working directory with `/` and `.`
//!   replaced by `-`. The detector snapshots the set of existing
//!   `*.jsonl` stems BEFORE the spawn, then polls the directory
//!   for new stems. The newest unfamiliar one wins.
//!
//! - **Codex**: each session writes
//!   `~/.codex/sessions/YYYY/MM/DD/<uuid>.jsonl`. The first line
//!   is a JSON record whose `payload.cwd` carries the working
//!   directory and whose `payload.id` is the session UUID. The
//!   detector walks the date-bucketed tree, parses just the first
//!   line of each candidate file, and matches on the cwd.
//!
//! ## Lifecycle
//!
//! [`spawn_detector`] launches a per-session thread that:
//!   1. Polls at [`POLL_INTERVAL`] up to [`MAX_DURATION`].
//!   2. On match, briefly locks state, calls the same
//!      mutate-and-bump logic as `methods::set_transcript_path`
//!      (set + bump-on-change), then exits.
//!   3. On session removal mid-poll (kill/exit), exits silently.
//!   4. On timeout, logs a warning and exits — the agent can
//!      still call `set_transcript_path` later via an RPC.
//!
//! The thread is detached (no join). It self-terminates on
//! match / timeout / session-vanish, all bounded.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::state::DaemonState;
use crate::workflow::transcript::read_first_line;

/// Detector poll interval for the first [`FAST_POLL_WINDOW`].
/// 500ms matches the TUI's drain loop cadence; Claude/Codex
/// usually write their first transcript line within 1-3s of
/// spawn so a tight poll binds the common case immediately.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// After [`FAST_POLL_WINDOW`] without a transcript, back off to
/// this slower cadence. Codex (0.132+) writes its rollout only
/// once the first user turn is recorded, which can be well past
/// the 1-3s typical case if the injected prompt is slow to submit
/// or the agent cold-starts after a version update. We keep
/// polling (cheaply) rather than giving up — a session that
/// binds late should still bind, not wedge in `pending` forever.
pub const SLOW_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// How long to poll at the fast [`POLL_INTERVAL`] before backing
/// off to [`SLOW_POLL_INTERVAL`].
pub const FAST_POLL_WINDOW: Duration = Duration::from_secs(60);

/// Backstop ceiling. The detector keeps polling (with backoff)
/// until it binds, the session vanishes, or this elapses. It is
/// deliberately generous: the old 60s ceiling permanently wedged
/// any session whose engine wrote its transcript late (slow
/// cold-start, deferred-until-first-turn rollout), since the
/// detector never retried after giving up. The only sessions that
/// reach this backstop are ones that never produce a transcript at
/// all (e.g. a `bash` session, or an agent that started but never
/// ran a turn) — for those, polling longer wouldn't help, so we
/// exit and log. A caller can still push a path explicitly via
/// `session.set_transcript_path`.
pub const MAX_DURATION: Duration = Duration::from_secs(60 * 60);

/// Snapshot the set of `*.jsonl` stems currently in the Claude
/// transcript directory for `worktree_path`. Returns an empty
/// vector if the directory doesn't exist (the agent hasn't been
/// run from this worktree before — the directory is created
/// lazily on first transcript write).
pub fn snapshot_claude_transcript_ids(worktree_path: &Path) -> Vec<String> {
    let Some(dir) = claude_transcript_dir(worktree_path) else {
        return Vec::new();
    };
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out
}

/// Detect a Claude session id under `worktree_path` that is not
/// in `existing`. Returns the newest by mtime so a race between
/// multiple recent spawns picks the latest one.
pub fn detect_new_claude_id(
    worktree_path: &Path,
    existing: &[String],
) -> Option<String> {
    let dir = claude_transcript_dir(worktree_path)?;
    if !dir.is_dir() {
        return None;
    }
    let mut newest: Option<(SystemTime, String)> = None;
    for entry in std::fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if existing.iter().any(|e| e == stem) {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        if newest.as_ref().map_or(true, |(t, _)| modified > *t) {
            newest = Some((modified, stem.to_string()));
        }
    }
    newest.map(|(_, id)| id)
}

/// Build the absolute Claude transcript file path for a known id.
pub fn claude_transcript_path(
    worktree_path: &Path,
    transcript_id: &str,
) -> Option<PathBuf> {
    let dir = claude_transcript_dir(worktree_path)?;
    Some(dir.join(format!("{}.jsonl", transcript_id)))
}

/// Public view of [`claude_transcript_dir`] — the directory Claude Code
/// writes this worktree's transcripts into. Used by `session.turn_ended` to
/// bound which file a session may claim as its own (fix-stale-resume).
pub fn claude_transcript_dir_for(worktree_path: &Path) -> Option<PathBuf> {
    claude_transcript_dir(worktree_path)
}

fn claude_transcript_dir(worktree_path: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path_str = worktree_path.to_str()?;
    let encoded = path_str.replace('/', "-").replace('.', "-");
    Some(home.join(format!(".claude/projects/{}", encoded)))
}

/// Snapshot the set of Codex session ids whose payload.cwd
/// matches `worktree_path`. Walks `~/.codex/sessions/YYYY/MM/DD/`
/// once; the result is what the detector will diff against to
/// find a new session.
pub fn snapshot_codex_transcript_ids(worktree_path: &Path) -> Vec<String> {
    let Some(sessions) = codex_sessions_dir() else {
        return Vec::new();
    };
    if !sessions.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<(SystemTime, String)> = Vec::new();
    let Some(wt_str) = worktree_path.to_str() else {
        return Vec::new();
    };
    for root in codex_scan_roots(&sessions) {
        walk_codex(&root, wt_str, &mut out);
    }
    out.into_iter().map(|(_, id)| id).collect()
}

/// Detect a Codex session id under `worktree_path` that is not
/// in `existing`. Returns the newest by mtime.
pub fn detect_new_codex_id(
    worktree_path: &Path,
    existing: &[String],
) -> Option<String> {
    let sessions = codex_sessions_dir()?;
    if !sessions.is_dir() {
        return None;
    }
    let wt_str = worktree_path.to_str()?;
    let mut found: Vec<(SystemTime, String)> = Vec::new();
    for root in codex_scan_roots(&sessions) {
        walk_codex(&root, wt_str, &mut found);
    }
    found
        .into_iter()
        .filter(|(_, id)| !existing.iter().any(|e| e == id))
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, id)| id)
}

/// Resolve the absolute Codex transcript file path for a known
/// id by walking the date-bucketed tree.
pub fn codex_transcript_path(transcript_id: &str) -> Option<PathBuf> {
    let sessions = codex_sessions_dir()?;
    if !sessions.is_dir() {
        return None;
    }
    // Common case (an active session): the id is in a recent date bucket.
    for root in recent_codex_date_dirs(&sessions, RECENT_CODEX_BUCKETS) {
        if let Some(hit) = find_codex_file(&root, transcript_id) {
            return Some(hit);
        }
    }
    // Not in a recent bucket (an older session, or an unrecognized layout) —
    // fall back to a full walk so a known id is never left unresolved.
    find_codex_file(&sessions, transcript_id)
}

fn codex_sessions_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".codex/sessions"))
}

/// How many of the most-recent `YYYY/MM/DD` codex date buckets the detector
/// scans per poll. >1 so a session spawned just before midnight whose rollout
/// lands in the next day's bucket is still found within the ≤1h poll window
/// (the buckets are re-resolved each poll). Scanning recent buckets instead of
/// the entire `~/.codex/sessions` history bounds the per-poll cost: the old walk
/// opened and read the first line of EVERY rollout file in all of codex history,
/// once or twice per poll (~120–840 times across a session's detect window).
const RECENT_CODEX_BUCKETS: usize = 3;

/// The most-recent codex `YYYY/MM/DD` date-bucket dirs (newest last), capped at
/// `keep`. Returns an empty vec when the layout under `sessions` isn't the
/// expected numeric 3-level nesting — the caller then falls back to a full walk
/// (correctness over the optimization). Only reads directory structure, never
/// opens a rollout file, so it's cheap.
fn recent_codex_date_dirs(sessions: &Path, keep: usize) -> Vec<PathBuf> {
    fn numeric_subdirs(dir: &Path) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).map_or(false, |s| {
                    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
                })
            })
            .collect()
    }
    let mut buckets: Vec<PathBuf> = Vec::new();
    for y in numeric_subdirs(sessions) {
        for m in numeric_subdirs(&y) {
            for d in numeric_subdirs(&m) {
                buckets.push(d);
            }
        }
    }
    // Lexicographic sort == chronological for zero-padded YYYY/MM/DD paths.
    buckets.sort();
    if buckets.len() > keep {
        buckets.drain(0..buckets.len() - keep);
    }
    buckets
}

/// Roots to scan for codex detection: the recent date buckets, or the whole
/// `sessions` dir when the date layout isn't recognized (correctness fallback).
fn codex_scan_roots(sessions: &Path) -> Vec<PathBuf> {
    let buckets = recent_codex_date_dirs(sessions, RECENT_CODEX_BUCKETS);
    if buckets.is_empty() {
        vec![sessions.to_path_buf()]
    } else {
        buckets
    }
}

fn walk_codex(dir: &Path, wt_str: &str, out: &mut Vec<(SystemTime, String)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_codex(&path, wt_str, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let Some(first) = read_first_line(&path) else {
                continue;
            };
            let Ok(val) = serde_json::from_str::<serde_json::Value>(first.trim()) else {
                continue;
            };
            if val.pointer("/payload/cwd").and_then(|v| v.as_str()) != Some(wt_str) {
                continue;
            }
            if let Some(id) = val.pointer("/payload/id").and_then(|v| v.as_str()) {
                let modified = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                out.push((modified, id.to_string()));
            }
        }
    }
}

fn find_codex_file(dir: &Path, transcript_id: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(hit) = find_codex_file(&path, transcript_id) {
                return Some(hit);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            let Some(first) = read_first_line(&path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(first.trim()) else {
                continue;
            };
            if v.pointer("/payload/id").and_then(|v| v.as_str()) == Some(transcript_id) {
                return Some(path);
            }
        }
    }
    None
}

// ============================================================
// Codex live-rollout observation (DESIGN_SEAMLESS_RESTART phase
// 4f — codex lineage)
// ============================================================
//
// Claude sessions are resumable-from-birth (H1's spawn-time pin)
// and never more than one turn stale (the cm Stop hook forwards
// the live transcript path through `session.turn_ended` every
// turn). Codex has neither: rollout ids rotate on `/compact`,
// codex runs no cm hook, and the daemon's detector binds only
// the FIRST rollout — so every hard-restart path (drain+legacy,
// machine reboot, re-exec's deliberate-kill terminal fallback)
// resumed a compacted codex session from PRE-compact history.
//
// The fix is daemon-side observation of ground truth: the codex
// process holds its live rollout file OPEN, so
// `/proc/<pid>/fd/*` names it. A per-session watch thread
// (armed for every codex session at the `start_session` spawn
// funnel) re-reads that on a timer and, on rotation, re-stamps
// the resume identity through the SAME `set_transcript_path`
// flow the detector and the TUI use (generation bump on change +
// `persist_sessions_best_effort`), so a hard restart resumes the
// post-compact lineage.
//
// Why a sibling loop rather than one of the existing cadences:
//   - `session_watch` is the memory-cap watcher — armed only for
//     capped sessions, and its loop is signal-capable (wrong
//     blast radius for an observation-only concern).
//   - the MCP wait/monitor idle loops are MCP-server-resident
//     (Python), not daemon-side, and only run while a caller is
//     watching.
//   - THIS module's detector retry loop is the model (same
//     per-session detached-thread shape, same state-lock
//     discipline, same slow cadence) but it exits at first bind
//     and is not armed at all for TUI-detected spawns — while
//     rotation tracking must outlive the bind and cover every
//     codex session the daemon hosts.
// So the watch reuses the detector's *pattern* and constants but
// arms once, at the one spawn funnel all paths share
// (`control::methods::start_session`). Known gap: the re-exec
// adoption path (reexec.rs) promotes sessions without passing
// through `start_session`, so rehydrated codex sessions need the
// watch re-armed post-commit — phase-4 integration, noted there.

/// Cadence of the codex rollout watch. Shares the detector's
/// slow-poll rationale ([`SLOW_POLL_INTERVAL`]): a `/proc` fd
/// readdir is cheap, rotation (a `/compact`) is rare, and the
/// only cost of a coarser interval is the residual staleness
/// window documented in DESIGN_SEAMLESS_RESTART's codex section
/// (a hard stop within one interval of a compact can still
/// resume pre-compact history).
pub const ROLLOUT_WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// Bound on how many processes one rollout scan will visit.
/// The real trees are tiny (see [`ROLLOUT_FD_SCAN_MAX_DEPTH`]);
/// the cap exists so a pathological child that forks wildly
/// can't turn the watch into a /proc crawl.
const ROLLOUT_FD_SCAN_MAX_PIDS: usize = 32;

/// Descendant depth of the /proc walk, from the session's
/// recorded child pid. How codex sessions are actually spawned
/// here (`mcp_config::build_args` → `PendingSession::spawn`,
/// optionally wrapped by `wrap_with_systemd_run`):
///
///   - uncapped:  daemon → `codex` (the npm bin is a node JS
///     launcher that `spawn`s the native binary) → codex-native
///     — the fd holder is 1 level down;
///   - capped:    daemon → `systemd-run --user --scope` →
///     node launcher → codex-native — 2 levels down.
///
/// Depth 3 covers both with one level of margin (e.g. a codex
/// build that adds another wrapper). Never deeper: tool
/// subprocesses codex forks live below the native binary and
/// must not be scanned as if they were the session.
const ROLLOUT_FD_SCAN_MAX_DEPTH: usize = 3;

/// Result of one [`scan_live_codex_rollout`] pass. `rollout` is
/// the file the process tree holds open right now (`None`:
/// codex not started yet, exited, mid-rotation, or a codex that
/// doesn't keep the fd open — in which case the watch degrades
/// to never re-stamping, exactly the pre-4f behavior).
/// `permission_denied` surfaces an EACCES on some `/proc/<pid>/fd`
/// honestly instead of silently reporting "no rollout" — it
/// shouldn't happen for same-user children, but a child that
/// execs a setuid/undumpable binary would produce it.
pub struct LiveRolloutScan {
    pub rollout: Option<PathBuf>,
    pub permission_denied: bool,
}

/// Direct children of `pid`, via `/proc/<pid>/task/<tid>/children`.
/// All task (thread) entries are read because the kernel files a
/// child under the THREAD that forked it, not the main thread.
/// Missing/unreadable entries mean the process raced away — an
/// empty vec, never an error (normal churn).
fn direct_children_of(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let task_dir = PathBuf::from(format!("/proc/{}/task", pid));
    let Ok(entries) = std::fs::read_dir(&task_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(contents) = std::fs::read_to_string(entry.path().join("children")) else {
            continue;
        };
        for tok in contents.split_ascii_whitespace() {
            if let Ok(child) = tok.parse::<u32>() {
                out.push(child);
            }
        }
    }
    out
}

/// Does `path` have the codex rollout layout
/// `…/.codex/sessions/YYYY/MM/DD/<name>.jsonl`? Matched on SHAPE
/// (trailing components), never on a literal `$HOME` prefix — the
/// /proc symlink target is the kernel's canonical path, which need
/// not string-match the daemon's `HOME` (symlinked homes), and
/// tests point it at a tempdir. The shape gate is also the
/// self-scope bound on what the watch may stamp (the codex
/// analogue of `session.turn_ended`'s own-transcript-dir check):
/// combined with the fact that the path comes from the session's
/// OWN process tree via /proc — daemon-observed, never
/// caller-reported — a watch can only ever stamp a codex rollout
/// file this session itself is writing. A deleted-but-open target
/// reads as `…jsonl (deleted)` from readlink and fails the
/// extension check, so it is never stamped.
pub fn is_codex_rollout_shaped(path: &Path) -> bool {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(os) => os.to_str(),
            _ => None,
        })
        .collect();
    let n = comps.len();
    if n < 6 {
        return false;
    }
    let (dot_codex, sessions, dates, file) =
        (comps[n - 6], comps[n - 5], &comps[n - 4..n - 1], comps[n - 1]);
    dot_codex == ".codex"
        && sessions == "sessions"
        && dates
            .iter()
            .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        && file.len() > ".jsonl".len()
        && file.ends_with(".jsonl")
}

/// Resolve the rollout file the codex session rooted at `root_pid`
/// holds open RIGHT NOW, by walking `/proc/<pid>/fd` symlinks over
/// the (bounded) descendant tree. Multiple distinct matches — a
/// brief rotation overlap where old and new rollout are both open —
/// resolve to the most recently MODIFIED one (the live writer), and
/// the ambiguity is logged so it's visible if it ever stops being
/// transient. Processes that vanish mid-scan are skipped silently;
/// permission errors are reported via
/// [`LiveRolloutScan::permission_denied`].
pub fn scan_live_codex_rollout(root_pid: u32) -> LiveRolloutScan {
    // Bounded BFS over the descendant tree.
    let mut pids: Vec<u32> = vec![root_pid];
    let mut frontier: Vec<u32> = vec![root_pid];
    for _ in 0..ROLLOUT_FD_SCAN_MAX_DEPTH {
        let mut next: Vec<u32> = Vec::new();
        for pid in &frontier {
            for child in direct_children_of(*pid) {
                if pids.len() >= ROLLOUT_FD_SCAN_MAX_PIDS {
                    break;
                }
                if !pids.contains(&child) {
                    pids.push(child);
                    next.push(child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    let mut permission_denied = false;
    let mut candidates: Vec<PathBuf> = Vec::new();
    for pid in &pids {
        let fd_dir = PathBuf::from(format!("/proc/{}/fd", pid));
        match std::fs::read_dir(&fd_dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let Ok(target) = std::fs::read_link(entry.path()) else {
                        continue;
                    };
                    if is_codex_rollout_shaped(&target)
                        && !candidates.contains(&target)
                    {
                        candidates.push(target);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                permission_denied = true;
            }
            // NotFound et al: the process exited between the child
            // walk and the fd read — normal churn, keep scanning.
            Err(_) => {}
        }
    }

    let rollout = match candidates.len() {
        0 => None,
        1 => candidates.pop(),
        n => {
            let newest = candidates
                .iter()
                .max_by_key(|p| {
                    std::fs::metadata(p)
                        .and_then(|m| m.modified())
                        .unwrap_or(UNIX_EPOCH)
                })
                .cloned();
            eprintln!(
                "cm-daemon: codex rollout scan under pid {} found {} open \
                 rollout files (rotation overlap?) — picking most recently \
                 modified: {}",
                root_pid,
                n,
                newest
                    .as_deref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
            );
            newest
        }
    };
    LiveRolloutScan {
        rollout,
        permission_denied,
    }
}

/// Outcome of one [`observe_codex_rollout_once`] pass. Terminal
/// states (`SessionGone` / `NotCodex`) end the watch loop; the
/// rest re-poll on the next tick.
#[derive(Debug, PartialEq, Eq)]
pub enum RolloutObserveOutcome {
    /// Session left the registry (exit / kill) — watch exits.
    SessionGone,
    /// Session isn't `codex` — defensive terminal state; the arm
    /// site already gates on type, so reaching this is a bug.
    NotCodex,
    /// No open rollout observed (codex not started, exited fds,
    /// mid-rotation, or the child respawned under this uid
    /// mid-scan). Current stamp untouched — absence of an open fd
    /// is never evidence of rotation.
    NoRollout,
    /// Live rollout matches the current stamp.
    Unchanged,
    /// Rotation observed and re-stamped (ids are the derived
    /// resume keys; `old` is `None` for a first bind).
    Stamped {
        old: Option<String>,
        new: String,
    },
    /// The stamp RPC flow errored non-NotFound (e.g. a transient
    /// internal error) — logged, retried next tick.
    StampFailed,
}

/// One observation pass of the codex rollout watch: resolve the
/// live rollout for the session's process tree and, if it differs
/// from the stamped resume identity, re-stamp through
/// [`crate::control::methods::set_transcript_path`] — deliberately
/// REUSED (not mirrored) so the bump-generation-on-change +
/// persist semantics can't drift from the RPC path the TUI uses.
/// `warned_permission` dedupes the EACCES log to once per watch
/// lifetime (the condition is stable once present; re-logging at
/// every 5s tick would be noise).
pub fn observe_codex_rollout_once(
    state: &Arc<Mutex<DaemonState>>,
    session_uid: &str,
    warned_permission: &mut bool,
) -> RolloutObserveOutcome {
    let (pid, current) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(sess) = s.sessions.get(session_uid) else {
            return RolloutObserveOutcome::SessionGone;
        };
        if sess.session_type != "codex" {
            return RolloutObserveOutcome::NotCodex;
        }
        (sess.pid as u32, sess.transcript_path.clone())
    };

    // /proc IO runs lock-free.
    let scan = scan_live_codex_rollout(pid);
    if scan.permission_denied && !*warned_permission {
        *warned_permission = true;
        eprintln!(
            "cm-daemon: codex rollout watch for '{}': permission denied \
             reading /proc fd tables under pid {} — rotation tracking is \
             degraded for this session (stale-resume window reopens)",
            session_uid, pid,
        );
    }
    let Some(live) = scan.rollout else {
        return RolloutObserveOutcome::NoRollout;
    };
    let live_str = live.to_string_lossy().into_owned();
    if current.as_deref() == Some(live_str.as_str()) {
        return RolloutObserveOutcome::Unchanged;
    }

    // Re-check the session still exists at the SAME pid before
    // stamping: if the uid was revived/respawned mid-scan, the scan
    // result describes the old child. (A pid reused by an unrelated
    // codex process inside one 5s tick while the registry entry
    // still records it is the residual TOCTOU — vanishingly narrow,
    // and the shape gate still bounds the damage to a real rollout
    // file of the same user.)
    {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(sess) = s.sessions.get(session_uid) else {
            return RolloutObserveOutcome::SessionGone;
        };
        if sess.pid as u32 != pid {
            return RolloutObserveOutcome::NoRollout;
        }
    }

    match crate::control::methods::set_transcript_path(
        state,
        &serde_json::json!({
            "session_uid": session_uid,
            "transcript_path": live_str,
        }),
    ) {
        Ok(_) => {
            let old_id = current
                .as_deref()
                .and_then(crate::session::transcript_id_from_path);
            let new_id = crate::session::transcript_id_from_path(&live_str)
                .unwrap_or_else(|| live_str.clone());
            eprintln!(
                "cm-daemon: codex rollout rotated for '{}': {} -> {}",
                session_uid,
                old_id.as_deref().unwrap_or("(none)"),
                new_id,
            );
            RolloutObserveOutcome::Stamped {
                old: old_id,
                new: new_id,
            }
        }
        Err((crate::control::protocol::ErrorCode::NotFound, _)) => {
            RolloutObserveOutcome::SessionGone
        }
        Err((code, msg)) => {
            eprintln!(
                "cm-daemon: codex rollout re-stamp for '{}' failed \
                 ({:?}): {} — retrying next tick",
                session_uid, code, msg,
            );
            RolloutObserveOutcome::StampFailed
        }
    }
}

/// The watch loop body: observe at [`ROLLOUT_WATCH_INTERVAL`] until
/// the session leaves the registry. Runs for the session's whole
/// lifetime — rotation can happen at any turn, so unlike the
/// detector there is no terminal "bound" state to stop at.
pub fn run_codex_rollout_watch(
    state: Arc<Mutex<DaemonState>>,
    session_uid: String,
) {
    let mut warned_permission = false;
    loop {
        match observe_codex_rollout_once(&state, &session_uid, &mut warned_permission) {
            RolloutObserveOutcome::SessionGone | RolloutObserveOutcome::NotCodex => {
                return;
            }
            _ => {}
        }
        std::thread::sleep(ROLLOUT_WATCH_INTERVAL);
    }
}

/// Arm the per-session codex rollout watch (detached thread, like
/// [`spawn_detector`]). Spawn failure is logged, not fatal: the
/// watch is a resume-identity hardening — a session without it is
/// exactly the pre-4f session (stale-on-compact), while failing the
/// spawn RPC for it would break something that used to work.
pub fn spawn_codex_rollout_watch(
    state: Arc<Mutex<DaemonState>>,
    session_uid: String,
) {
    let name = format!("cm-daemon-codex-rollout-{}", session_uid);
    let uid_for_log = session_uid.clone();
    if let Err(e) = std::thread::Builder::new().name(name).spawn(move || {
        run_codex_rollout_watch(state, session_uid);
    }) {
        eprintln!(
            "cm-daemon: could not start codex rollout watch for '{}' ({}); \
             resume identity will stay at its detector-bound rollout \
             (stale after a /compact)",
            uid_for_log, e,
        );
    }
}

/// Engine discriminator for the detector. Mirrors the
/// `mcp_start_session` `type` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectorEngine {
    ClaudeCode,
    Codex,
}

impl DetectorEngine {
    /// Parse from `mcp_start_session.type`. Returns `None` for
    /// `bash` (no transcript) — callers should skip the
    /// detector entirely in that case.
    pub fn from_session_type(session_type: &str) -> Option<Self> {
        match session_type {
            "claude-code" => Some(Self::ClaudeCode),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// Outcome of [`run_detector_sync`].
#[derive(Debug, PartialEq, Eq)]
pub enum DetectorOutcome {
    /// Detector wrote a transcript_path to the session.
    Bound,
    /// Session was removed from the registry before a transcript
    /// could be found (kill / exit / fast crash).
    SessionGone,
    /// Hit [`MAX_DURATION`] without finding a transcript.
    /// Common for sessions that never write one (`bash`); rare for
    /// engine-instrumented sessions but still bounded.
    Timeout,
}

/// Launch a detector thread for a freshly spawned session. The
/// caller must:
///   1. Snapshot existing transcript ids BEFORE the spawn (via
///      [`snapshot_claude_transcript_ids`] /
///      [`snapshot_codex_transcript_ids`]).
///   2. Call this AFTER the session is registered in
///      `state.sessions` (so the thread can find it).
///
/// The thread runs detached. On success it updates
/// `session.transcript_path` and bumps `generation` if the path
/// changed (same semantics as
/// `methods::set_transcript_path`). On session-vanish or
/// timeout it exits silently / with a log line.
///
/// **`mcp_start_session` callers should prefer
/// [`run_detector_sync`]** under the per-worktree spawn gate to
/// avoid cross-binding between concurrent same-worktree spawns
/// (sub-2b-3 review-4 #2). This helper remains for non-MCP
/// callers (TUI-spawned sessions) where ownership is already
/// guaranteed by other means.
pub fn spawn_detector(
    state: Arc<Mutex<DaemonState>>,
    session_uid: String,
    engine: DetectorEngine,
    worktree_path: PathBuf,
    existing: Vec<String>,
) {
    let _ = std::thread::Builder::new()
        .name(format!("cm-daemon-transcript-detect-{}", session_uid))
        .spawn(move || {
            let _ = run_detector_sync(state, session_uid, engine, worktree_path, existing);
        });
}

/// Synchronous detector. Same semantics as the threaded
/// [`spawn_detector`] but blocks the calling thread until one
/// of the [`DetectorOutcome`] terminal states is reached.
///
/// Sub-2b-3 review-4 #2: `mcp_start_session` calls this
/// while holding the per-worktree spawn gate so concurrent
/// same-worktree spawns can't observe each other's transcript
/// files mid-bind.
pub fn run_detector_sync(
    state: Arc<Mutex<DaemonState>>,
    session_uid: String,
    engine: DetectorEngine,
    worktree_path: PathBuf,
    existing: Vec<String>,
) -> DetectorOutcome {
    run_detector(state, session_uid, engine, worktree_path, existing)
}

/// Sub-2b-3 review-11: boxed thread-spawn factory mirroring
/// [`crate::session_watch::WatcherSpawnFn`]. Production
/// passes [`default_detector_spawn_fn`]; tests can pass a
/// closure that returns `Err` to exercise the fail-closed
/// path.
///
/// The shape matches `WatcherSpawnFn` exactly — same
/// signature, same semantics — so callers that already
/// understand the watcher-spawn injection (e.g.
/// `start_session_with_spawn_fn`) get the same mental
/// model for the detector spawn.
pub type DetectorSpawnFn = Box<
    dyn FnOnce(
            String,
            Box<dyn FnOnce() + Send + 'static>,
        ) -> std::io::Result<std::thread::JoinHandle<()>>
        + Send,
>;

/// Production detector-spawn factory. Wraps
/// `std::thread::Builder::new().name(name).spawn(body)`,
/// which returns `io::Error` (kind = `WouldBlock` on
/// thread/FD ulimit hit, etc.) rather than panicking —
/// that's the underlying mechanism that makes the
/// fail-closed shape possible.
///
/// Sub-2b-3 review-11: when the test override
/// [`set_force_spawn_failure_for_test`] is active, this
/// returns a closure that always fails. The override is
/// `#[cfg(test)]`-only, so production builds compile out
/// the branch and use the real thread spawner.
pub fn default_detector_spawn_fn() -> DetectorSpawnFn {
    #[cfg(test)]
    if FORCE_DETECTOR_SPAWN_FAILURE
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Box::new(|_name, _body| {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "test-forced detector spawn failure (review-11)",
            ))
        });
    }
    Box::new(|name, body| {
        std::thread::Builder::new()
            .name(name)
            .spawn(body)
    })
}

#[cfg(test)]
static FORCE_DETECTOR_SPAWN_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only: force [`default_detector_spawn_fn`] to return
/// a closure that always errors. Returns an RAII guard that
/// restores the previous state on drop. Used by review-11's
/// fail-closed integration test to exercise the
/// `mcp_start_session` cleanup path without actually
/// exhausting threads.
#[cfg(test)]
pub fn set_force_spawn_failure_for_test(force: bool) -> ForceSpawnFailureGuard {
    let prev = FORCE_DETECTOR_SPAWN_FAILURE.swap(force, std::sync::atomic::Ordering::Relaxed);
    ForceSpawnFailureGuard { prev }
}

#[cfg(test)]
pub struct ForceSpawnFailureGuard {
    prev: bool,
}

#[cfg(test)]
impl Drop for ForceSpawnFailureGuard {
    fn drop(&mut self) {
        FORCE_DETECTOR_SPAWN_FAILURE
            .store(self.prev, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Sub-2b-3 review-5 #1 + review-6 #1 + review-7 +
/// review-11: spawn a detector thread that holds an
/// already-acquired `WorktreeSpawnTicket` through detection.
/// Returns the thread's `JoinHandle` on success or the
/// underlying `io::Error` on thread-spawn failure.
///
/// **Important: the caller must have already called
/// `queue.wait_for_turn(ticket.seq())` BEFORE invoking this
/// function.** The slot is held by the ticket until the
/// thread exits and Drop fires `signal_done` — releasing the
/// next waiter in line.
///
/// **Failure path** (review-11): when `spawn_fn` returns
/// `Err`, the body closure (including `ticket`) is dropped
/// inside `spawn_fn` — ticket's `Drop` fires `signal_done`
/// and releases the slot. The CALLER is responsible for
/// removing the just-spawned `DaemonSession` from
/// `state.sessions` so the kernel SIGKILLs the child via
/// pidfd Drop. Pre-fix the failure was silently ignored,
/// leaving the session alive in the registry with no
/// detector — MCP-spawned sessions would stay `pending`
/// forever.
///
/// Pre-review-7 this function called `wait_for_turn` itself,
/// which meant the spawn's pre-snapshot and child creation
/// happened OUTSIDE the slot. Two same-worktree spawns could
/// both create transcripts before either detector polled,
/// allowing cross-binding. Moving `wait_for_turn` to the
/// main thread (BEFORE pre-snapshot + spawn) ensures
/// {pre-snapshot + spawn + detect} all run under the same
/// held slot.
pub fn spawn_queued_detector(
    state: Arc<Mutex<DaemonState>>,
    session_uid: String,
    engine: DetectorEngine,
    worktree_path: PathBuf,
    existing: Vec<String>,
    ticket: Option<crate::state::WorktreeSpawnTicket>,
    spawn_fn: DetectorSpawnFn,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let thread_name = format!("cm-daemon-transcript-detect-{}", session_uid);
    let body: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
        // `ticket` lives until the closure exits — its Drop calls
        // `signal_done(seq)` to release the worktree slot. The caller already
        // called `wait_for_turn` before launching us, so this thread doesn't
        // wait — it runs the detector and lets Drop release the slot. A panic
        // anywhere in the body still releases via Drop.
        //
        // P-B: `ticket` is `Option` so a caller whose `wait_for_turn` TIMED
        // OUT (and already dropped its slot) can still arm the detector
        // UNSERIALIZED with `None`. Dropping `None` is a no-op — the detector
        // runs for liveness (accepting the small cross-bind risk the timeout
        // path already documents) rather than being skipped entirely, which
        // would wedge the role with no transcript_path.
        let _hold_ticket = ticket;
        let _ = run_detector_sync(state, session_uid, engine, worktree_path, existing);
    });
    spawn_fn(thread_name, body)
}

fn run_detector(
    state: Arc<Mutex<DaemonState>>,
    session_uid: String,
    engine: DetectorEngine,
    worktree_path: PathBuf,
    existing: Vec<String>,
) -> DetectorOutcome {
    // Sub-2b-3 review-3 #1: extra IDs to exclude from
    // detection get appended here when a concurrent detector
    // claimed the same candidate first. Mutates across poll
    // iterations; the immutable `existing` snapshot is what was
    // present at spawn time, while `claimed_collisions` grows as
    // we observe other live sessions' bindings.
    let mut claimed_collisions: Vec<String> = Vec::new();
    let started = Instant::now();
    loop {
        if started.elapsed() > MAX_DURATION {
            eprintln!(
                "cm-daemon: transcript detector for {} ({:?}) gave up after {:?} \
                 without ever seeing a transcript — the agent likely never ran a \
                 turn (no rollout written). It will stay `pending` until a caller \
                 pushes session.set_transcript_path explicitly",
                session_uid, engine, MAX_DURATION,
            );
            return DetectorOutcome::Timeout;
        }
        // Bail early if the session was killed / exited before
        // the agent had a chance to write its transcript.
        {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            if !s.sessions.contains_key(&session_uid) {
                return DetectorOutcome::SessionGone;
            }
        }
        let effective_existing: Vec<String> = existing
            .iter()
            .chain(claimed_collisions.iter())
            .cloned()
            .collect();
        let detected: Option<(String, PathBuf)> = match engine {
            DetectorEngine::ClaudeCode => {
                detect_new_claude_id(&worktree_path, &effective_existing)
                    .and_then(|id| {
                        claude_transcript_path(&worktree_path, &id)
                            .map(|p| (id, p))
                    })
            }
            DetectorEngine::Codex => {
                detect_new_codex_id(&worktree_path, &effective_existing)
                    .and_then(|id| codex_transcript_path(&id).map(|p| (id, p)))
            }
        };
        if let Some((id, path)) = detected {
            let path_str = path.to_string_lossy().into_owned();
            let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
            // Sub-2b-3 review-3 #1: claim check held under the
            // same state lock as the write. If any OTHER live
            // session has already bound `path_str`, two
            // detectors raced and the other detector won. Add
            // the colliding id to our exclusion list and
            // re-poll for the next-newest unfamiliar file.
            // Deriving the claimed set on-demand from
            // `state.sessions.values()` auto-cleans on session
            // removal (kill / exit-reap drops the
            // DaemonSession; the lookup naturally stops seeing
            // its path).
            let already_claimed = s.sessions.iter().any(|(other_uid, other)| {
                other_uid != &session_uid
                    && other.transcript_path.as_deref() == Some(path_str.as_str())
            });
            if already_claimed {
                drop(s);
                claimed_collisions.push(id);
                // Tight retry — no sleep — so we can catch the
                // next-newest candidate immediately if it's
                // already on disk (e.g. both engine writes
                // landed before either detector polled).
                continue;
            }
            let Some(session) = s.sessions.get_mut(&session_uid) else {
                return DetectorOutcome::SessionGone;
            };
            // Mirrors `methods::set_transcript_path` mutate-
            // and-bump-on-change semantics. Bump only when the
            // path actually changed so re-runs against a steady
            // path are idempotent and don't invalidate agent
            // cursors.
            let changed = session.transcript_path.as_deref() != Some(path_str.as_str());
            if changed {
                session.generation = session.generation.saturating_add(1);
            }
            session.transcript_path = Some(path_str);
            // P0 session durability (S1): this is the HEADLESS transcript-
            // bind path (no TUI runs `session.set_transcript_path`), so
            // persisting here is what makes a restart able to resume an
            // MCP-spawned subtask session's history. The assignment above
            // is the last use of `session`, so its `&mut s.sessions`
            // borrow has ended — `s` is free for the immutable persist.
            if changed {
                s.persist_sessions_best_effort();
            }
            return DetectorOutcome::Bound;
        }
        // Tight poll for the first minute (binds the common case
        // within a couple seconds), then back off so a long-lived
        // pending session isn't a hot 500ms loop until the backstop.
        let interval = if started.elapsed() < FAST_POLL_WINDOW {
            POLL_INTERVAL
        } else {
            SLOW_POLL_INTERVAL
        };
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_var` / `remove_var` for HOME aren't safe across
    /// threads; serialize against the rest of the daemon test
    /// suite via the shared `test_support::env_lock` (the same
    /// mutex other HOME-touching tests in this binary use). A
    /// separate per-module mutex would race them.
    ///
    /// The lock ALSO covers `tempfile::tempdir()` because
    /// another test in this crate transiently sets `umask(0o177)`
    /// inside its env_lock scope (the daemon socket bind tests).
    /// A tempdir created during that window inherits the
    /// tightened umask and ends up with no execute bits — later
    /// `create_dir_all` inside it fails with PermissionDenied.
    /// So `make_env` MUST take the lock BEFORE creating the
    /// tempdir.
    struct HomeEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev: Option<std::ffi::OsString>,
        pub dir: tempfile::TempDir,
    }
    impl HomeEnv {
        fn make() -> Self {
            let g = crate::test_support::env_lock();
            let dir = tempfile::tempdir().unwrap();
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", dir.path());
            Self { _guard: g, prev, dir }
        }
        fn path(&self) -> &Path {
            self.dir.path()
        }
    }
    impl Drop for HomeEnv {
        fn drop(&mut self) {
            match self.prev.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    #[test]
    fn recent_codex_date_dirs_keeps_newest_and_falls_back_on_odd_layout() {
        let env = HomeEnv::make();
        let sessions = env.path().join(".codex/sessions");
        // Five date buckets across a month boundary; expect the 3 newest.
        for (y, m, d) in [
            ("2026", "05", "30"),
            ("2026", "05", "31"),
            ("2026", "06", "01"),
            ("2026", "06", "10"),
            ("2026", "06", "11"),
        ] {
            std::fs::create_dir_all(sessions.join(y).join(m).join(d)).unwrap();
        }
        let picked = recent_codex_date_dirs(&sessions, 3);
        let names: Vec<String> = picked
            .iter()
            .map(|p| {
                // last three path components: YYYY/MM/DD
                let comps: Vec<_> = p.components().rev().take(3).collect();
                format!(
                    "{}/{}/{}",
                    comps[2].as_os_str().to_string_lossy(),
                    comps[1].as_os_str().to_string_lossy(),
                    comps[0].as_os_str().to_string_lossy(),
                )
            })
            .collect();
        assert_eq!(
            names,
            vec!["2026/06/01", "2026/06/10", "2026/06/11"],
            "must keep the 3 newest date buckets, newest last",
        );

        // Unrecognized (non-numeric) layout → empty, so codex_scan_roots falls
        // back to the full `sessions` walk (correctness over the optimization).
        let odd = env.path().join(".codex-odd");
        std::fs::create_dir_all(odd.join("not-a-date")).unwrap();
        assert!(recent_codex_date_dirs(&odd, 3).is_empty());
        assert_eq!(codex_scan_roots(&odd), vec![odd.clone()]);
    }

    #[test]
    fn claude_snapshot_returns_empty_for_missing_dir() {
        let env = HomeEnv::make();
        let wt = env.path().join("worktree-x");
        std::fs::create_dir_all(&wt).unwrap();
        let ids = snapshot_claude_transcript_ids(&wt);
        assert!(ids.is_empty(), "no transcript dir → empty snapshot");
    }

    #[test]
    fn claude_detect_picks_up_new_file_after_snapshot() {
        let env = HomeEnv::make();
        let wt = env.path().join("some-worktree");
        std::fs::create_dir_all(&wt).unwrap();
        // Build the encoded transcript dir manually — `claude
        // --print` would do this lazily on first session, but
        // the test pre-creates it.
        let encoded = wt.to_str().unwrap().replace('/', "-").replace('.', "-");
        let transcript_dir = env.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&transcript_dir).unwrap();
        // Seed an "existing" transcript that should NOT be
        // picked up.
        std::fs::write(transcript_dir.join("old-uid.jsonl"), b"{}\n").unwrap();
        let snapshot = snapshot_claude_transcript_ids(&wt);
        assert_eq!(snapshot, vec!["old-uid".to_string()]);
        // Simulate a fresh spawn writing a new file.
        std::fs::write(transcript_dir.join("new-uid.jsonl"), b"{}\n").unwrap();
        let detected = detect_new_claude_id(&wt, &snapshot);
        assert_eq!(
            detected.as_deref(),
            Some("new-uid"),
            "detector must surface the new file, not the pre-snapshot one",
        );
        let resolved = claude_transcript_path(&wt, "new-uid").unwrap();
        assert_eq!(resolved, transcript_dir.join("new-uid.jsonl"));
    }

    #[test]
    fn codex_detect_matches_payload_cwd() {
        let env = HomeEnv::make();
        let wt = env.path().join("codex-worktree");
        std::fs::create_dir_all(&wt).unwrap();
        let codex_day = env.path().join(".codex/sessions/2026/01/15");
        std::fs::create_dir_all(&codex_day).unwrap();
        // A pre-existing session in this cwd that should be in
        // the snapshot.
        let old = serde_json::json!({"payload": {"cwd": wt.to_str().unwrap(), "id": "old-codex"}});
        std::fs::write(codex_day.join("old-codex.jsonl"), old.to_string()).unwrap();
        let snapshot = snapshot_codex_transcript_ids(&wt);
        assert_eq!(snapshot, vec!["old-codex".to_string()]);
        // A session in a DIFFERENT cwd; must not show up in
        // detection at all.
        let other = serde_json::json!({"payload": {"cwd": "/other/cwd", "id": "other-codex"}});
        std::fs::write(codex_day.join("other-codex.jsonl"), other.to_string()).unwrap();
        // The fresh spawn we're "detecting".
        let new = serde_json::json!({"payload": {"cwd": wt.to_str().unwrap(), "id": "new-codex"}});
        std::fs::write(codex_day.join("new-codex.jsonl"), new.to_string()).unwrap();
        let detected = detect_new_codex_id(&wt, &snapshot);
        assert_eq!(
            detected.as_deref(),
            Some("new-codex"),
            "detector matches on payload.cwd AND excludes pre-snapshot ids",
        );
        let resolved = codex_transcript_path("new-codex").unwrap();
        assert_eq!(resolved, codex_day.join("new-codex.jsonl"));
    }

    /// Sub-2b-3 review-11: a `DetectorSpawnFn` that returns
    /// `Err` propagates out of `spawn_queued_detector`
    /// instead of being silently dropped. Pre-fix
    /// `Builder::spawn`'s `Err` was ignored, leaving
    /// MCP-spawned sessions detector-less; this test pins
    /// the propagation so future regressions trip the
    /// build.
    ///
    /// Also verifies the ticket Drop fires on the failure
    /// path: when the spawn closure runs, the body
    /// (including ticket) is consumed inside the spawn_fn.
    /// `Err` returns from spawn_fn drop the body, and ticket
    /// Drop calls signal_done — releasing the slot for
    /// subsequent waiters.
    #[test]
    fn spawn_queued_detector_propagates_thread_spawn_error_and_releases_slot() {
        let state = Arc::new(Mutex::new(DaemonState::new()));
        // Insert a placeholder session so the detector's
        // session-vanish branch isn't hit immediately.
        let mut sp = crate::session::SpawnParams::new("ts-spawn-fail", "x", "/bin/sleep");
        sp.args = vec!["30".into()];
        sp.workspace_id = "ws-x".into();
        sp.session_type = "claude-code".to_string();
        let session = crate::session::DaemonSession::spawn(sp).unwrap();
        state.lock().unwrap().sessions.insert("ts-spawn-fail".into(), session);

        let queue = Arc::new(crate::state::WorktreeSpawnQueue::new());
        let ticket = crate::state::WorktreeSpawnTicket::new(queue.clone(), queue.enqueue());
        assert_eq!(ticket.seq(), 0);

        // Failing spawn_fn — body is dropped (ticket goes
        // with it; ticket Drop signals done).
        let failing: DetectorSpawnFn = Box::new(|_name, _body| {
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "injected-spawn-failure-r11",
            ))
        });
        let result = spawn_queued_detector(
            state.clone(),
            "ts-spawn-fail".to_string(),
            DetectorEngine::ClaudeCode,
            std::path::PathBuf::from("/tmp/wt-x"),
            Vec::new(),
            Some(ticket),
            failing,
        );
        let err = result.expect_err("must propagate spawn failure");
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert!(
            err.to_string().contains("injected-spawn-failure-r11"),
            "must propagate underlying error: {}",
            err,
        );
        // Slot released: a fresh ticket gets seq=1 and
        // `wait_for_turn(1)` returns immediately because
        // the prior ticket's Drop signaled done.
        let probe_seq = queue.enqueue();
        assert_eq!(probe_seq, 1);
        queue.wait_for_turn(probe_seq);

        // Cleanup.
        let _release_probe = crate::state::WorktreeSpawnTicket::new(queue.clone(), probe_seq);
        drop(_release_probe);
        state.lock().unwrap().sessions.remove("ts-spawn-fail");
    }

    /// DESIGN_SEAMLESS_RESTART phase 4f (codex lineage): the rollout
    /// shape gate matches on trailing path components, never a
    /// literal HOME prefix, and refuses everything that isn't a
    /// `…/.codex/sessions/YYYY/MM/DD/<name>.jsonl` file — it doubles
    /// as the self-scope bound on what the watch may stamp.
    #[test]
    fn codex_rollout_shape_gate_matches_layout_not_home() {
        let ok = |s: &str| assert!(is_codex_rollout_shaped(Path::new(s)), "{}", s);
        let no = |s: &str| assert!(!is_codex_rollout_shaped(Path::new(s)), "{}", s);
        // Any HOME prefix works, including a tempdir.
        ok("/home/u/.codex/sessions/2026/08/18/rollout-2026-08-18T09-00-00-abc.jsonl");
        ok("/tmp/xyz123/.codex/sessions/2026/08/18/r.jsonl");
        // Date components must be numeric.
        no("/home/u/.codex/sessions/2026/aug/18/r.jsonl");
        // Must sit under `.codex/sessions`.
        no("/home/u/.claude/projects/-x/6a1b2c3d.jsonl");
        no("/home/u/.codex/other/2026/08/18/r.jsonl");
        // Extension is load-bearing (also excludes readlink's
        // `… (deleted)` suffix for unlinked-but-open targets).
        no("/home/u/.codex/sessions/2026/08/18/r.log");
        no("/home/u/.codex/sessions/2026/08/18/r.jsonl (deleted)");
        no("/home/u/.codex/sessions/2026/08/18/.jsonl");
        // Too shallow.
        no("/.codex/sessions/2026/08/r.jsonl");
    }

    #[test]
    fn detector_engine_skips_bash() {
        assert_eq!(
            DetectorEngine::from_session_type("claude-code"),
            Some(DetectorEngine::ClaudeCode),
        );
        assert_eq!(
            DetectorEngine::from_session_type("codex"),
            Some(DetectorEngine::Codex),
        );
        assert_eq!(
            DetectorEngine::from_session_type("bash"),
            None,
            "bash sessions have no transcript — caller skips the detector",
        );
    }
}
