//! Durable record + persistence for a continuous task.
//!
//! A continuous task is a long-lived, manually-triggerable unit of work pinned
//! to one durable worktree. Each task lives in `~/.cm/continuous-tasks/<id>/` as:
//!   - `state.json`   — the [`ContinuousTask`] struct below
//!   - `runs.jsonl`   — append-only audit of fires (see [`super::runlog`])
//!
//! This module is the DATA layer for Continuous Tasks Phase 2 (the trigger
//! funnel, manual-only). It is a structural TWIN of
//! `daemon/src/workflow/run.rs`: the same flock-on-a-sentinel persistence
//! contract (`save` / `load_one` / `load_all` / `modify` / `try_modify`), the
//! same containment-safe id validation, plus an idempotent first-write
//! [`create`] the `continuous.create` handler keys its record-level
//! `created=false` collision-reuse off of.
//!
//! The scheduler (Phase 3), persistent executor / supervision (Phase 3), the
//! queue / Consumer layer (Phase 4) and the downstream fan-out (Phase 6) all
//! consume this contract but live in later slices. Fields those phases need
//! (`next_fire_at`, `downstream`, `retention`, the `Schedule::Periodic` /
//! `Consumer` / `Cron` variants, `RunMode::Persistent`, …) are DEFINED here so
//! older `state.json` stays forward-compatible when they land, but they carry
//! no behavior in Phase 2.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Agent engine for a continuous task. Wire vocab is
/// `"claude"|"codex"|"bash"` — it matches `metadata.continuous.engine` in the
/// planning row (DESIGN_CONTINUOUS_TASKS.md §6). This is NOT the `session_type`
/// vocab `compose_daemon_spawn_params` / `build_args` expect; use
/// [`Engine::as_session_type`] at the spawn boundary to map it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Claude,
    Codex,
    Bash,
}

impl Engine {
    /// Map the wire vocab (`claude`/`codex`/`bash`) to the `session_type` vocab
    /// (`claude-code`/`codex`/`bash`) that `compose_daemon_spawn_params` /
    /// `build_args` / `is_valid_session_type` require. Phase 2 targets claude.
    pub fn as_session_type(&self) -> &'static str {
        match self {
            Engine::Claude => "claude-code",
            Engine::Codex => "codex",
            Engine::Bash => "bash",
        }
    }
}

/// How a fire runs. `fresh` spawns a NEW session per trigger and leaves prior
/// sessions idle (Phase 2); `persistent` reuses one supervised session
/// (Phase 3 — `trigger` returns `persistent_not_yet_implemented` until then).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    #[default]
    Fresh,
    Persistent,
}

/// When a task fires.
///
/// **Wire shape (stable):** internally tagged on `kind` with `snake_case`
/// variant names — same convention as `workflow::run::TriggerKind`. Phase 2
/// only ever constructs [`Schedule::OnDemand`]; the periodic / consumer / cron
/// variants are scheduler/queue territory (Phases 3/4) and are defined here
/// only so their `state.json` shape is pinned ahead of time.
///
/// NOTE: `Cron` is a struct variant `Cron { expr }`, never `Cron(String)` —
/// serde internally-tagged enums reject a tuple/newtype-of-scalar variant at
/// (de)serialize time.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    #[default]
    OnDemand,
    Periodic {
        every_secs: u64,
    },
    Consumer {
        queue: String,
        batch_max: u32,
        window_secs: u64,
        depth_threshold: u32,
    },
    Cron {
        expr: String,
    },
}

/// Lifecycle status of a single run (the [`RunRecord`] / runs.jsonl status;
/// DESIGN_CONTINUOUS_TASKS.md §6). Distinct type from
/// `workflow::run::RunStatus` — same identifier, different module path, no real
/// collision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Idle,
    Done,
    Failed,
    Stuck,
    Orphaned,
}

/// Spawn-window guard recorded at the start of `trigger` and cleared when the
/// trigger call returns (Phase 2 semantics — NOT whole-run tracking). A
/// concurrent trigger while this is `Some(_)` is rejected with
/// `{fired:false, reason:"busy"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InFlight {
    pub fire_token: String,
    pub session_uid: String,
    pub started_at: u64,
}

/// The most recent run's durable summary. `last_run.fire_token` is what
/// `trigger`'s idempotency check compares an incoming `fire_token` against.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub seq: u64,
    pub fire_token: String,
    pub started_at: u64,
    #[serde(default)]
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub session_uid: Option<String>,
    pub status: RunStatus,
    pub trigger_source: String,
}

/// Retention policy. `keep_sessions = None` means keep-all (Phase 2 default;
/// the prune is a later, default-off addition).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default)]
    pub keep_sessions: Option<u32>,
    #[serde(default = "default_transcript_days")]
    pub transcript_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            keep_sessions: None,
            transcript_days: 30,
        }
    }
}

fn default_transcript_days() -> u32 {
    30
}

/// A named prompt preset (`modes["nightly"]`, …). `args` is a free-form blob
/// the daemon does not parse.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModePreset {
    pub prompt: String,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

/// The durable record of a continuous task. Persisted at
/// `~/.cm/continuous-tasks/<task_id>/state.json`.
///
/// Field groups (and Phase provenance): IDENTITY + CONFIG + RUNTIME are the
/// Phase-2 surface; everything under LATER-PHASE is defined-but-inert (all
/// `#[serde(default)]` so older records deserialize). All fields are `pub` —
/// mirroring `WorkflowRun` — so the `continuous.create` handler can assign
/// optional config after [`ContinuousTask::new`].
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContinuousTask {
    // ----- identity (Phase 2) -----
    pub task_id: String,
    /// Optional backing planning-task UUID. When set, the spawned session's
    /// `task_id` is this UUID instead of the continuous `task_id` slug, so a
    /// subtask-spawning orchestrator (e.g. bug-triage) has a real planning
    /// parent: `create_subtask` resolves `caller.task_id` via the planning API
    /// (`tasks.id` is a UUID PK; `parent_task_id` is a UUID FK), and the
    /// spawned subtasks nest under this row on the planning board. `None`
    /// (default) keeps the legacy behavior (session `task_id` = the slug).
    #[serde(default)]
    pub planning_task_id: Option<String>,
    pub label: String,
    #[serde(default)]
    pub project: Option<String>,
    pub host_id: String,
    #[serde(default)]
    pub repo: Option<String>,
    pub workspace_id: String,
    /// The durable pinned worktree as a String (mirrors `WorkflowRun::task_key`).
    pub worktree_path: String,
    // ----- config (Phase 2) -----
    pub engine: Engine,
    pub run_mode: RunMode,
    pub schedule: Schedule,
    #[serde(default)]
    pub skill: Option<String>,
    pub default_prompt: String,
    #[serde(default)]
    pub modes: BTreeMap<String, ModePreset>,
    // ----- runtime (Phase 2) -----
    #[serde(default)]
    pub current_session_uid: Option<String>,
    #[serde(default)]
    pub run_count: u32,
    pub enabled: bool,
    pub paused: bool,
    #[serde(default)]
    pub in_flight: Option<InFlight>,
    #[serde(default)]
    pub last_run: Option<RunRecord>,
    pub started_at: u64,
    // ----- later-phase, defined-but-inert -----
    /// Phase 3 (scheduler): next periodic fire deadline.
    #[serde(default)]
    pub next_fire_at: u64,
    /// Phase 3 (scheduler): last periodic fire timestamp.
    #[serde(default)]
    pub last_fired_at: u64,
    /// Phase 3 (persistent): auto-`/compact` cadence.
    #[serde(default)]
    pub compact_every: Option<u32>,
    /// Phase 3 (persistent): supervision + watchdog opt-in.
    #[serde(default)]
    pub supervise: bool,
    /// Phase 3 (watchdog): per-run runtime ceiling.
    #[serde(default)]
    pub max_runtime_secs: Option<u32>,
    /// Phase 3 (memory cap): per-task override for the spawn's memory-cap
    /// triple, in bytes. `None` (default) falls back to the daemon's
    /// `[scheduler] default_cap`; `Some(0)` opts out (uncapped).
    #[serde(default)]
    pub mem_cap_bytes: Option<u64>,
    /// Phase 3 (backoff): consecutive-failure counter.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Phase 3b (stuck-story watchdog): how many investigators the watchdog has
    /// spawned for the CURRENT fresh run. Reset to `0` on every new fresh fire;
    /// once it reaches `[scheduler] max_investigations` the watchdog
    /// auto-escalates (last_run → Stuck) instead of spawning another.
    #[serde(default)]
    pub investigation_count: u32,
    /// Phase 3b (stuck-story watchdog): the session_uid of the live investigator
    /// the watchdog spawned for the current stuck run, if any. `Some(_)` means
    /// an investigation is in flight (the watchdog does nothing while it is
    /// alive); cleared back to `None` on a new fresh fire and after every
    /// `resolve_stuck` action.
    #[serde(default)]
    pub investigator_uid: Option<String>,
    /// Wall-clock (unix secs) the current investigator was spawned — set WITH
    /// `investigator_uid`. Lets the watchdog bound the investigator's OWN
    /// runtime (`[scheduler] default_investigator_runtime_secs`) so a wedged-
    /// but-alive investigator is abandoned rather than pinning the stuck run
    /// forever. Only read while `investigator_uid` is `Some` (always set
    /// together on spawn), so a stale value left after a clear is inert.
    #[serde(default)]
    pub investigator_started_at: Option<u64>,
    /// Phase 6 (fan-out): downstream task-id allowlist.
    #[serde(default)]
    pub downstream: Vec<String>,
    /// Phase 4 (chaining): queue to enqueue into on completion.
    #[serde(default)]
    pub enqueue_to: Option<String>,
    /// Later (prune): retention policy.
    #[serde(default)]
    pub retention: Retention,
    /// UX hint: where to surface this task for review.
    #[serde(default)]
    pub review_surface: Option<String>,
    /// Review-discovery marker: whether the `/triage-review` skill can review
    /// this task, and how. `"fix_first"` (bug/perf/scraper — the review queue is
    /// mostly mergeable code fixes) | `"investigate_first"` (behavior/api-update
    /// — mostly investigate-only proposals awaiting an operator decision) |
    /// `None` (not reviewable via triage-review). Surfaced by `continuous.list`
    /// so review discovery is config-driven, not hardcoded per skill.
    #[serde(default)]
    pub review_kind: Option<String>,
}

impl ContinuousTask {
    /// Seed a new task with Phase-2 defaults. The `continuous.create` handler
    /// assigns optional config (project, repo, skill, modes, …) on the returned
    /// value before persisting. Mirror of `WorkflowRun::new`.
    pub fn new(
        task_id: String,
        label: String,
        workspace_id: String,
        worktree_path: String,
        engine: Engine,
        run_mode: RunMode,
        schedule: Schedule,
        default_prompt: String,
    ) -> Self {
        ContinuousTask {
            task_id,
            planning_task_id: None,
            label,
            project: None,
            host_id: "local".into(),
            repo: None,
            workspace_id,
            worktree_path,
            engine,
            run_mode,
            schedule,
            skill: None,
            default_prompt,
            modes: BTreeMap::new(),
            current_session_uid: None,
            run_count: 0,
            enabled: true,
            paused: false,
            in_flight: None,
            last_run: None,
            started_at: now_unix(),
            next_fire_at: 0,
            last_fired_at: 0,
            compact_every: None,
            supervise: false,
            max_runtime_secs: None,
            mem_cap_bytes: None,
            consecutive_failures: 0,
            investigation_count: 0,
            investigator_uid: None,
            investigator_started_at: None,
            downstream: Vec::new(),
            enqueue_to: None,
            retention: Retention::default(),
            review_surface: None,
            review_kind: None,
        }
    }
}

// ------------------- Persistence -------------------

pub fn tasks_dir() -> PathBuf {
    // Single source of truth for the .cm root — keeps the symlink-rejection
    // walk in `runlog::ContinuousRunLog` aligned with whatever fallback
    // (`$HOME` or `/tmp`) resolved here, so a no-HOME container env doesn't
    // break continuous tasks.
    crate::path::dot_cm_dir().join("continuous-tasks")
}

pub fn task_dir(task_id: &str) -> PathBuf {
    tasks_dir().join(task_id)
}

pub fn runs_log_path(task_id: &str) -> PathBuf {
    task_dir(task_id).join("runs.jsonl")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug)]
pub enum PersistError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl From<io::Error> for PersistError {
    fn from(e: io::Error) -> Self {
        PersistError::Io(e)
    }
}
impl From<serde_json::Error> for PersistError {
    fn from(e: serde_json::Error) -> Self {
        PersistError::Json(e)
    }
}

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PersistError::Io(e) => write!(f, "io: {}", e),
            PersistError::Json(e) => write!(f, "json: {}", e),
        }
    }
}

/// Containment-safe `task_id` validation (twin of
/// `workflow::run::validate_run_id`). Reject anything that could escape the
/// `~/.cm/continuous-tasks/` directory via path-traversal (`/`, `..`, `.`) or
/// otherwise confuse the filesystem (empty, oversize, non-allowlist chars).
/// task_ids are planning slugs like `bug-triage`, which fit the conservative
/// `[A-Za-z0-9_-]` allowlist.
///
/// EVERY filesystem-touching entry point in this module (`save`, `create`,
/// `load_one`, `modify`, `try_modify`) and the runs.jsonl append validates via
/// this single helper, BEFORE constructing any path — so a future caller that
/// builds a path from an untrusted task_id can't bypass validation.
pub fn validate_task_id(id: &str) -> io::Result<()> {
    const MAX_LEN: usize = 128;
    if id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "task_id is empty",
        ));
    }
    if id.len() > MAX_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("task_id too long: {} > {}", id.len(), MAX_LEN),
        ));
    }
    if id == "." || id == ".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("task_id is a path-traversal token: {:?}", id),
        ));
    }
    for c in id.chars() {
        if !(c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("disallowed character in task_id {:?}: {:?}", id, c),
            ));
        }
    }
    Ok(())
}

/// Save atomically: write to state.json.tmp then rename.
///
/// The write happens under an exclusive `flock(2)` on a dedicated lock file
/// (`state.json.lock`) so concurrent readers/writers across processes
/// serialize cleanly. The lock is on a SEPARATE file from `state.json` itself
/// because the atomic rename used here unlinks the original — invalidating any
/// flock held on it. The `.lock` file is a sentinel; only its inode matters.
pub fn save(task: &ContinuousTask) -> Result<(), PersistError> {
    // Validate BEFORE any path construction.
    validate_task_id(&task.task_id)?;
    let dir = task_dir(&task.task_id);
    fs::create_dir_all(&dir)?;
    let _guard = LockGuard::exclusive(&dir)?;
    let tmp = dir.join("state.json.tmp");
    let final_path = dir.join("state.json");
    let json = serde_json::to_string_pretty(task)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &final_path)?;
    Ok(())
}

/// Idempotent first-write (no `workflow::run` twin). Returns `Ok(true)` if this
/// call created the record, `Ok(false)` if a record already existed (collision
/// — the `continuous.create` handler reuses it, mirroring `create_worktree`'s
/// `created=false`).
///
/// The `state.json.exists()` check happens UNDER the exclusive flock — not
/// before — so two concurrent `continuous.create` calls for the same id can't
/// both think they created it.
pub fn create(task: &ContinuousTask) -> Result<bool, PersistError> {
    // Validate BEFORE any path construction.
    validate_task_id(&task.task_id)?;
    let dir = task_dir(&task.task_id);
    fs::create_dir_all(&dir)?;
    let _guard = LockGuard::exclusive(&dir)?;
    let final_path = dir.join("state.json");
    if final_path.exists() {
        // Collision: an earlier create won. Reuse the existing record.
        return Ok(false);
    }
    let tmp = dir.join("state.json.tmp");
    let json = serde_json::to_string_pretty(task)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &final_path)?;
    Ok(true)
}

/// Load a single persisted task by id. Returns `None` if the file is missing or
/// can't be parsed.
///
/// The read happens under a shared `flock(2)` on `state.json.lock` so a
/// concurrent `save` doesn't tear our view. (Atomic rename in `save` already
/// makes torn reads impossible at the byte level, but the shared lock is
/// defense-in-depth for the read-modify-write window that [`modify`] uses.)
pub fn load_one(task_id: &str) -> Option<ContinuousTask> {
    // Validate BEFORE any path construction.
    validate_task_id(task_id).ok()?;
    let dir = task_dir(task_id);
    let _guard = LockGuard::shared(&dir).ok()?;
    let path = dir.join("state.json");
    let contents = fs::read_to_string(&path).ok()?;
    serde_json::from_str::<ContinuousTask>(&contents).ok()
}

/// Atomic load-modify-write under exclusive `flock(2)`. The closure receives a
/// `&mut ContinuousTask` loaded from disk; on closure return, the task is saved
/// back under the SAME lock the load took. This is the right shape for
/// `trigger` recording `last_run` / clearing `in_flight` without racing a
/// concurrent save.
///
/// Returns the post-mutation `ContinuousTask` on success.
pub fn modify<F>(task_id: &str, mutate: F) -> Result<ContinuousTask, PersistError>
where
    F: FnOnce(&mut ContinuousTask),
{
    // Validate BEFORE any path construction.
    validate_task_id(task_id)?;
    let dir = task_dir(task_id);
    fs::create_dir_all(&dir)?;
    let _guard = LockGuard::exclusive(&dir)?;
    let path = dir.join("state.json");
    let contents = fs::read_to_string(&path)?;
    let mut task: ContinuousTask = serde_json::from_str(&contents)?;
    mutate(&mut task);
    let tmp = dir.join("state.json.tmp");
    let json = serde_json::to_string_pretty(&task)?;
    fs::write(&tmp, json)?;
    fs::rename(&tmp, &path)?;
    Ok(task)
}

/// Fallible variant of [`modify`]. Closure returns `Result<(), E>`; on `Err(e)`
/// the save is skipped and the original (unmutated) `Err(e)` surfaces to the
/// caller. Used by `trigger` for the **inside-flock check-and-set**: the
/// closure runs under the exclusive lock, sees the authoritative `in_flight` /
/// `last_run`, and either sets the spawn-window guard or aborts with a
/// busy/duplicate reason — no TOCTOU between the check and the mutation.
pub enum TryModifyOutcome<E> {
    /// Closure ran to `Ok(())`; mutation saved.
    Ok(ContinuousTask),
    /// Closure returned `Err(e)`; mutation rolled back (no save).
    Aborted(E),
    /// Filesystem error during load / save / lock.
    Persist(PersistError),
}

pub fn try_modify<F, E>(task_id: &str, mutate: F) -> TryModifyOutcome<E>
where
    F: FnOnce(&mut ContinuousTask) -> Result<(), E>,
{
    // Validate BEFORE any path construction. A malformed task_id surfaces as
    // `Persist(InvalidInput)` — handlers translate to InvalidParams.
    if let Err(e) = validate_task_id(task_id) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    let dir = task_dir(task_id);
    if let Err(e) = fs::create_dir_all(&dir) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    let _guard = match LockGuard::exclusive(&dir) {
        Ok(g) => g,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Io(e)),
    };
    let path = dir.join("state.json");
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Io(e)),
    };
    let mut task: ContinuousTask = match serde_json::from_str(&contents) {
        Ok(t) => t,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Json(e)),
    };
    if let Err(e) = mutate(&mut task) {
        return TryModifyOutcome::Aborted(e);
    }
    let tmp = dir.join("state.json.tmp");
    let json = match serde_json::to_string_pretty(&task) {
        Ok(s) => s,
        Err(e) => return TryModifyOutcome::Persist(PersistError::Json(e)),
    };
    if let Err(e) = fs::write(&tmp, json) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    if let Err(e) = fs::rename(&tmp, &path) {
        return TryModifyOutcome::Persist(PersistError::Io(e));
    }
    TryModifyOutcome::Ok(task)
}

/// RAII guard around `flock(2)` on `<task_dir>/state.json.lock`. Created via
/// [`LockGuard::exclusive`] / [`LockGuard::shared`]; drop releases the lock.
/// The lock file is created if missing.
///
/// Why a sentinel `.lock` file instead of locking `state.json` directly:
/// `save` uses atomic rename, which unlinks the original file — invalidating
/// any open fd's lock on it. The sentinel survives renames.
struct LockGuard {
    _file: fs::File,
}

impl LockGuard {
    fn exclusive(dir: &std::path::Path) -> Result<Self, std::io::Error> {
        Self::acquire(dir, libc::LOCK_EX)
    }

    fn shared(dir: &std::path::Path) -> Result<Self, std::io::Error> {
        Self::acquire(dir, libc::LOCK_SH)
    }

    fn acquire(dir: &std::path::Path, op: libc::c_int) -> Result<Self, std::io::Error> {
        use std::os::unix::io::AsRawFd;
        let lock_path = dir.join("state.json.lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        // `flock` blocks until the requested mode is available. Per-task write
        // volume is low (a handful of fires per task), so blocking is fine.
        let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(LockGuard { _file: file })
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // Best-effort unlock; on close `flock` is released by the kernel
        // anyway, so an explicit LOCK_UN is belt-and-suspenders. The `_file`
        // field of this struct DOES drop here.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Load all persisted tasks. Invalid/unreadable state.json files are skipped.
/// This is the scheduler's per-tick scan in Phase 3 (cheap empty readdir when
/// no tasks exist).
pub fn load_all() -> Vec<ContinuousTask> {
    let dir = tasks_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let state_file = path.join("state.json");
        if !state_file.exists() {
            continue;
        }
        if let Ok(contents) = fs::read_to_string(&state_file) {
            if let Ok(task) = serde_json::from_str::<ContinuousTask>(&contents) {
                out.push(task);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_home<F: FnOnce()>(f: F) -> tempfile::TempDir {
        // Serialize HOME env mutation across the whole crate — cargo runs
        // tests from different modules in parallel, so a local `static LOCK`
        // would only protect this module's tests.
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().unwrap();
        let orig = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        f();
        if let Some(o) = orig {
            unsafe {
                std::env::set_var("HOME", o);
            }
        }
        tmp
    }

    fn sample_task() -> ContinuousTask {
        ContinuousTask::new(
            "bug-triage".into(),
            "Bug triage".into(),
            "ws-1".into(),
            "/tmp/repo".into(),
            Engine::Claude,
            RunMode::Fresh,
            Schedule::OnDemand,
            "read NOTES.md first".into(),
        )
    }

    #[test]
    fn engine_run_mode_status_round_trip() {
        for e in [Engine::Claude, Engine::Codex, Engine::Bash] {
            let s = serde_json::to_string(&e).unwrap();
            assert_eq!(serde_json::from_str::<Engine>(&s).unwrap(), e);
        }
        for m in [RunMode::Fresh, RunMode::Persistent] {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<RunMode>(&s).unwrap(), m);
        }
        for st in [
            RunStatus::Pending,
            RunStatus::Running,
            RunStatus::Idle,
            RunStatus::Done,
            RunStatus::Failed,
            RunStatus::Stuck,
            RunStatus::Orphaned,
        ] {
            let s = serde_json::to_string(&st).unwrap();
            assert_eq!(serde_json::from_str::<RunStatus>(&s).unwrap(), st);
        }
    }

    /// Pin the wire vocab: engine serializes lowercase ("claude"), and the
    /// session_type mapping diverges ("claude-code").
    #[test]
    fn engine_wire_and_session_type_vocab() {
        assert_eq!(
            serde_json::to_value(Engine::Claude).unwrap(),
            serde_json::json!("claude"),
        );
        assert_eq!(Engine::Claude.as_session_type(), "claude-code");
        assert_eq!(Engine::Codex.as_session_type(), "codex");
        assert_eq!(Engine::Bash.as_session_type(), "bash");
    }

    /// Every `Schedule` variant round-trips, and the internally-tagged wire
    /// shape is pinned. `Cron` must be a struct variant or this errors at
    /// (de)serialize time.
    #[test]
    fn schedule_variants_round_trip() {
        let cases = vec![
            Schedule::OnDemand,
            Schedule::Periodic { every_secs: 60 },
            Schedule::Consumer {
                queue: "q".into(),
                batch_max: 5,
                window_secs: 30,
                depth_threshold: 2,
            },
            Schedule::Cron {
                expr: "0 * * * *".into(),
            },
        ];
        for sc in cases {
            let s = serde_json::to_string(&sc).unwrap();
            let back: Schedule = serde_json::from_str(&s).unwrap();
            // Schedule doesn't impl PartialEq — compare via JSON.
            assert_eq!(
                serde_json::to_value(&sc).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
        }
        assert_eq!(
            serde_json::to_value(Schedule::OnDemand).unwrap(),
            serde_json::json!({"kind": "on_demand"}),
        );
        assert_eq!(
            serde_json::to_value(Schedule::Periodic { every_secs: 60 }).unwrap(),
            serde_json::json!({"kind": "periodic", "every_secs": 60}),
        );
        assert_eq!(
            serde_json::to_value(Schedule::Cron {
                expr: "@daily".into()
            })
            .unwrap(),
            serde_json::json!({"kind": "cron", "expr": "@daily"}),
        );
    }

    #[test]
    fn round_trip_serde() {
        let task = sample_task();
        let s = serde_json::to_string(&task).unwrap();
        let back: ContinuousTask = serde_json::from_str(&s).unwrap();
        assert_eq!(back.task_id, task.task_id);
        assert_eq!(back.engine, Engine::Claude);
        assert_eq!(back.run_mode, RunMode::Fresh);
        assert!(back.enabled);
        assert!(!back.paused);
    }

    /// A minimal `state.json` carrying only the required fields deserializes —
    /// the later-phase fields fall back to their `#[serde(default)]` values, so
    /// records written before those phases land stay forward-compatible.
    #[test]
    fn serde_defaults_fill_later_phase_fields() {
        let json = serde_json::json!({
            "task_id": "bug-triage",
            "label": "Bug triage",
            "host_id": "local",
            "workspace_id": "ws-1",
            "worktree_path": "/tmp/repo",
            "engine": "claude",
            "run_mode": "fresh",
            "schedule": {"kind": "on_demand"},
            "default_prompt": "go",
            "enabled": true,
            "paused": false,
            "started_at": 123
        });
        let task: ContinuousTask = serde_json::from_value(json).unwrap();
        assert_eq!(task.next_fire_at, 0);
        assert_eq!(task.last_fired_at, 0);
        assert_eq!(task.consecutive_failures, 0);
        // Phase 3b watchdog state: a pre-3b state.json fills these from
        // `#[serde(default)]` — no investigations spawned, no live investigator.
        assert_eq!(task.investigation_count, 0);
        assert!(task.investigator_uid.is_none());
        assert!(task.downstream.is_empty());
        assert!(task.enqueue_to.is_none());
        assert!(task.compact_every.is_none());
        assert!(!task.supervise);
        assert!(task.max_runtime_secs.is_none());
        assert!(task.mem_cap_bytes.is_none());
        assert!(task.review_surface.is_none());
        assert!(task.review_kind.is_none());
        assert_eq!(task.run_count, 0);
        assert!(task.modes.is_empty());
        assert!(task.in_flight.is_none());
        assert!(task.last_run.is_none());
        assert!(task.current_session_uid.is_none());
        assert!(task.project.is_none());
        assert!(task.repo.is_none());
        assert!(task.skill.is_none());
        // Retention default: keep-all + 30 days.
        assert!(task.retention.keep_sessions.is_none());
        assert_eq!(task.retention.transcript_days, 30);
    }

    #[test]
    fn validate_task_id_allowlist() {
        assert!(validate_task_id("bug-triage").is_ok());
        assert!(validate_task_id("a").is_ok());
        assert!(validate_task_id("Task_1-2").is_ok());
        assert!(validate_task_id(&"x".repeat(128)).is_ok());

        for bad in [
            "",
            ".",
            "..",
            "../etc",
            "foo/bar",
            "with space",
            "semi;colon",
            "with\0null",
        ] {
            let r = validate_task_id(bad);
            assert!(r.is_err(), "expected reject for {:?}", bad);
            assert_eq!(
                r.unwrap_err().kind(),
                io::ErrorKind::InvalidInput,
                "{:?} must error with InvalidInput",
                bad,
            );
        }
        assert!(validate_task_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn save_then_load_one_round_trip() {
        let _tmp = with_temp_home(|| {
            let mut task = sample_task();
            task.run_count = 7;
            task.modes.insert(
                "nightly".into(),
                ModePreset {
                    prompt: "do the nightly thing".into(),
                    args: None,
                },
            );
            save(&task).expect("save");

            let loaded = load_one("bug-triage").expect("load");
            assert_eq!(loaded.task_id, "bug-triage");
            assert_eq!(loaded.run_count, 7);
            assert_eq!(loaded.engine, Engine::Claude);
            assert_eq!(loaded.modes.len(), 1);
            assert_eq!(loaded.modes["nightly"].prompt, "do the nightly thing");
        });
    }

    /// `load_one` returns `None` for a bad id (validation rejects before any
    /// path is built) and for a missing record.
    #[test]
    fn load_one_none_on_bad_or_missing() {
        let _tmp = with_temp_home(|| {
            assert!(load_one("../escape").is_none());
            assert!(load_one("never-saved").is_none());
        });
    }

    #[test]
    fn create_returns_false_on_collision() {
        let _tmp = with_temp_home(|| {
            let task = sample_task();
            assert!(create(&task).expect("first create"), "first create -> true");
            // A second create for the same id must report the collision so the
            // handler reuses the existing record.
            assert!(
                !create(&task).expect("second create"),
                "collision -> false",
            );
            let loaded = load_one("bug-triage").expect("load");
            assert_eq!(loaded.task_id, "bug-triage");
        });
    }

    #[test]
    fn modify_mutates_and_persists() {
        let _tmp = with_temp_home(|| {
            save(&sample_task()).expect("save");
            let updated = modify("bug-triage", |t| {
                t.run_count += 1;
                t.paused = true;
            })
            .expect("modify");
            assert_eq!(updated.run_count, 1);
            assert!(updated.paused);

            let reloaded = load_one("bug-triage").expect("reload");
            assert!(reloaded.paused);
            assert_eq!(reloaded.run_count, 1);
        });
    }

    #[test]
    fn try_modify_aborts_without_save_and_commits_on_ok() {
        let _tmp = with_temp_home(|| {
            save(&sample_task()).expect("save");

            let aborted: TryModifyOutcome<&str> = try_modify("bug-triage", |t| {
                t.run_count = 99;
                Err("nope")
            });
            match aborted {
                TryModifyOutcome::Aborted(e) => assert_eq!(e, "nope"),
                _ => panic!("expected Aborted"),
            }
            // Roll-back: disk unchanged.
            assert_eq!(load_one("bug-triage").unwrap().run_count, 0);

            let ok: TryModifyOutcome<&str> = try_modify("bug-triage", |t| {
                t.run_count = 5;
                Ok(())
            });
            match ok {
                TryModifyOutcome::Ok(t) => assert_eq!(t.run_count, 5),
                _ => panic!("expected Ok"),
            }
            assert_eq!(load_one("bug-triage").unwrap().run_count, 5);
        });
    }

    #[test]
    fn load_all_collects_saved_tasks() {
        let _tmp = with_temp_home(|| {
            // Empty tasks dir -> empty vec (cheap readdir miss).
            assert!(load_all().is_empty());

            let mut a = sample_task();
            a.task_id = "task-a".into();
            let mut b = sample_task();
            b.task_id = "task-b".into();
            save(&a).expect("save a");
            save(&b).expect("save b");

            let all = load_all();
            let ids: std::collections::HashSet<String> =
                all.iter().map(|t| t.task_id.clone()).collect();
            assert!(ids.contains("task-a"));
            assert!(ids.contains("task-b"));
        });
    }
}
