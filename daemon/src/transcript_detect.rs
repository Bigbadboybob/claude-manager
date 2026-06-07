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

/// Detector poll interval. 500ms matches the TUI's drain loop
/// cadence; Claude/Codex typically write their first transcript
/// line within 1-3s of spawn so a 500ms poll is plenty.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Hard ceiling. If the detector doesn't see a transcript within
/// this window the agent is either not engine-instrumented (bash)
/// or is hung pre-first-write. Either way, polling forever isn't
/// useful — log and exit. The agent / TUI can push a path
/// explicitly later via `session.set_transcript_path`.
pub const MAX_DURATION: Duration = Duration::from_secs(60);

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
    walk_codex(&sessions, wt_str, &mut out);
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
    walk_codex(&sessions, wt_str, &mut found);
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
    find_codex_file(&sessions, transcript_id)
}

fn codex_sessions_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".codex/sessions"))
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
                "cm-daemon: transcript detector for {} ({:?}) timed out after {:?} \
                 — the agent will stay in `pending` until a caller pushes \
                 session.set_transcript_path explicitly",
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
            return DetectorOutcome::Bound;
        }
        std::thread::sleep(POLL_INTERVAL);
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
