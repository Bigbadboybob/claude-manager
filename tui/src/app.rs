use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::term::TermMode;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::agent;
use crate::agent_memory;
use crate::api::Task;
use crate::backend::{BackendEvent, BackendHandle};
use crate::config::Config;
use crate::input;
use crate::planning::{PlanAction, PlanningView, WorkspaceCandidate};
use crate::session::Session;
use crate::terminal_widget::TerminalWidget;
use crate::workflow::{self, toml_schema::Engine, Workflow, WorkflowRun};
use crate::worktree;

mod dirs {
    use std::path::PathBuf;
    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL_MS: u128 = 80;
/// Width of the Sessions-view sidebar in cells. The terminal panel takes the
/// remaining width minus its own border (see `SIDEBAR_WIDTH + 2` in main.rs
/// when sizing the PTY).
pub const SIDEBAR_WIDTH: u16 = 36;
/// Minimum Wakeups within the window to consider a session actively working.
const WAKEUP_BURST_THRESHOLD: usize = 5;

#[derive(Clone, Debug, PartialEq)]
pub enum TaskStatus {
    Running,
    Blocked,
    Backlog,
    Done,
}

impl TaskStatus {
    fn from_api(s: &str) -> Self {
        match s {
            "running" => TaskStatus::Running,
            "blocked" => TaskStatus::Blocked,
            "backlog" => TaskStatus::Backlog,
            "done" => TaskStatus::Done,
            _ => TaskStatus::Backlog,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SessionStatus {
    Running,
    Idle,
}

pub struct TerminalSession {
    /// Stable per-session id, generated at creation. Used to track the cursor
    /// across reconciles so duplicate labels don't confuse restoration.
    /// In-memory only; not persisted.
    pub uid: String,
    pub label: String,
    pub session_type: String, // "claude" or "bash" — immutable, survives renames
    pub session: Session,
    pub status: SessionStatus,
    pub last_write_at: Option<Instant>,
    /// Current transcript file UUID (the JSONL filename). Unstable: `None`
    /// until the detector binds to a fresh file, and reset to `None` on
    /// `/clear` while the detector rebinds. For stable identity, use `uid`.
    pub transcript_id: Option<String>,
    /// Bumps every time `transcript_id` rebinds (e.g. on `/clear`). Used to
    /// detect cursor-vs-file mismatch when reading transcripts. In-memory
    /// only for now; Phase 2 will mirror it into the manifest.
    pub generation: u64,
    pub pending_jsonl_files: Option<Vec<String>>,
    pub hidden: bool,
    /// Seconds of quiet before marking idle. 0 = use global default.
    pub idle_timeout_secs: u16,
    /// Wakeups required within the 2s activity window to flip Idle → Running.
    /// 0 = use global default (`WAKEUP_BURST_THRESHOLD`). Lower = more sensitive,
    /// useful for slow-streaming bash scripts that produce one line at a time.
    pub burst_threshold: u16,
    /// Prompt text to deliver to the session once it's actually ready to
    /// receive input (see `PendingWrite`).
    pub pending_prompt: Option<PendingWrite>,
    /// Pending `/clear` command to send before `pending_prompt`. Sequenced:
    /// the prompt only delivers after the clear has either been delivered or
    /// hit its deadline.
    pub pending_clear: Option<PendingWrite>,
    /// If this session is a workflow participant, the run it belongs to.
    pub workflow_run_id: Option<String>,
    /// Role name within that workflow (e.g. "worker", "reviewer", "manager").
    pub workflow_role: Option<String>,
    /// Task this session belongs to. None = workspace-level (ad-hoc) session
    /// not tied to any task. Sessions launched from the planning view (A-f / A-l)
    /// inherit the launched task's id; manually added sessions (A-s) inherit
    /// the task_id of the cursor's current task scope, or None.
    pub task_id: Option<String>,
    /// First ~120 chars of the most recent prompt we delivered via
    /// `deliver_pending_write`, along with its delivery timestamp in unix ms.
    /// Used to correlate a fresh claude workflow session with its new
    /// sessionId in `~/.claude/history.jsonl`: when the same text shows up
    /// in a history entry with project==worktree, the entry's sessionId is
    /// ours. Cleared once sid has been bound.
    pub last_delivery: Option<(String, u64)>,
    /// If true, fire a desktop notification each time this session
    /// transitions Running → Idle. Off by default; toggled in A-e settings.
    pub notify_on_idle: bool,
    /// Marker that an Enter keystroke is queued to be written to the PTY at
    /// or after `fire_at`. Used to introduce a deliberate gap between the body
    /// of a multi-KB workflow prompt and the trailing Enter so the receiving
    /// agent (notably codex) classifies Enter as a fresh keystroke rather than
    /// the tail of a paste. Done as a deferred write rather than
    /// `thread::sleep` so the UI thread keeps draining events.
    ///
    /// We deliberately do NOT store the Enter bytes — the encoding (raw `\r`
    /// vs Kitty `\x1b[13u`) depends on the terminal mode at submit time. The
    /// agent often enables Kitty mode AFTER we've written the body but BEFORE
    /// the deferred Enter fires; if we used bytes captured at body-write time
    /// we'd send the wrong encoding and the prompt wouldn't submit.
    pub pending_enter: Option<PendingEnter>,
    /// Wall-clock instant when this TerminalSession was constructed.
    /// Used as a tie-breaker in session_id detection so when two sessions in
    /// the same worktree race for a newly-written transcript, the one that
    /// has been waiting longer (and is therefore more likely to actually own
    /// the file) gets first pick.
    pub created_at: Instant,
    /// UID of the agent session that spawned/owns this one. Set when a
    /// session is spawned via the agent-orchestration MCP tools; None
    /// for user-created sessions. Persisted across TUI restart.
    pub managed_by_uid: Option<String>,
    /// Name of the agent-memory snapshot this session was cloned from, if
    /// any. Informational provenance only — surfaces in session info as
    /// "Seeded from: <name>". Persisted via `ManifestEntry`. See
    /// DESIGN_AGENT_MEMORIES.md.
    pub seeded_from_snapshot: Option<String>,
}

impl TerminalSession {
    /// Rebind the session to a new transcript file (or `None` to mark the
    /// session as transcript-less while a fresh one is being detected).
    /// Bumps `generation` so any reader holding a cursor against the old
    /// transcript detects the rebind and restarts at offset 0 of the new
    /// file. Use at every site that mutates `transcript_id` AFTER an
    /// initial bind — the initial `None → Some(...)` transition has no
    /// prior readers and skips the bump.
    pub fn rebind_transcript(&mut self, new_sid: Option<String>) {
        self.transcript_id = new_sid;
        self.generation = self.generation.saturating_add(1);
    }
}

fn note_workflow_transcript_binding(
    runs: &mut [WorkflowRun],
    run_id: &str,
    role: &str,
    old_sid: Option<&str>,
    new_sid: &str,
) -> bool {
    let Some(run) = runs.iter_mut().find(|run| run.run_id == run_id) else {
        return false;
    };

    let mut changed = false;
    if let Some(binding) = run.role_sessions.get_mut(role) {
        if binding.current_session_id.as_deref() != Some(new_sid) {
            binding.current_session_id = Some(new_sid.to_string());
            changed = true;
        }
    }

    let rebound = old_sid.is_some() && old_sid != Some(new_sid);
    if rebound {
        run.role_baselines
            .insert(role.to_string(), workflow::run::MessageBaseline::default());
        changed = true;
    }

    if run.active_role.as_deref() == Some(role) {
        if let Some(entry) = run.history.last_mut() {
            if entry.role == role {
                if entry.session_id.as_deref() != Some(new_sid) {
                    entry.session_id = Some(new_sid.to_string());
                    changed = true;
                }
                if rebound && entry.assistant_count_at_start != 0 {
                    entry.assistant_count_at_start = 0;
                    changed = true;
                }
            }
        }
    }

    changed
}

/// Marker that an Enter keystroke is queued to fire at or after `fire_at`. The
/// actual bytes are computed from the current terminal mode at submit time.
pub struct PendingEnter {
    pub fire_at: Instant,
}

const DEFAULT_IDLE_TIMEOUT_SECS: u16 = 2;
const CODEX_RESUME_REBIND_WINDOW: Duration = Duration::from_secs(120);

/// A byte sequence queued to be written to a session's PTY once the session
/// is "ready" to receive input. Readiness is determined by PTY quietness —
/// absence of wakeup events for a minimum window — which adapts to however
/// long the underlying agent takes to finish starting up, connecting to MCP
/// servers, rendering its banner, etc.
///
/// Two knobs:
/// - `earliest_deliver_at`: floor (don't deliver before this time regardless
///   of quietness). Used to give the user a chance to notice what's happening,
///   and to debounce brief quiet windows during startup.
/// - `hard_deadline`: ceiling. If the agent NEVER goes quiet (e.g. a pathological
///   ticking spinner), deliver anyway so the workflow doesn't hang forever.
///
/// Between the floor and deadline, delivery fires at the first moment of
/// `require_quiet` of uninterrupted silence.
///
/// `text` is the payload; if `submit` is true we append an Enter keystroke
/// (encoded for the session's current mode) at delivery time.
pub struct PendingWrite {
    pub text: String,
    pub submit: bool,
    pub earliest_deliver_at: Instant,
    pub require_quiet: Duration,
    pub hard_deadline: Instant,
}

impl PendingWrite {
    /// A write that fires at the first moment of PTY quiet (>= `quiet`
    /// without any wakeup), bounded by `floor` (earliest) and `deadline`
    /// (latest) from now.
    pub fn wait_for_quiet(text: String, submit: bool, floor: Duration, quiet: Duration, deadline: Duration) -> Self {
        let now = Instant::now();
        PendingWrite {
            text,
            submit,
            earliest_deliver_at: now + floor,
            require_quiet: quiet,
            hard_deadline: now + deadline,
        }
    }
}

/// Interval between filesystem checks for session_id detection.
const SESSION_ID_CHECK_INTERVAL: Duration = Duration::from_secs(5);

/// Per-file cap on `~/.cm/workflow-runs/<run-id>/tick.log`. When `log_tick`
/// is about to write and the file is at or over this size, it truncates and
/// starts fresh (with a marker line). Generous because these logs are useful
/// debugging artifacts; this exists only to bound runaway growth from
/// pathologically chatty runs.
const TICK_LOG_MAX_BYTES: u64 = 500 * 1024 * 1024;

/// Gap between writing a workflow prompt body and the trailing Enter. Implemented
/// as a deferred write (not `thread::sleep`) so the UI thread keeps draining
/// events. Generous to leave codex's PTY paste detector no doubt about Enter
/// being a fresh keystroke.
const ENTER_GAP: Duration = Duration::from_secs(10);

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct ManifestEntry {
    /// Stable per-session UID generated at creation. Persisted (Phase 2a)
    /// so MCP env's `CM_TUI_SESSION_ID` survives TUI restart and the
    /// agent's tool calls keep authorizing. Backfill rule: missing on
    /// load → generate fresh and re-save.
    #[serde(default)]
    uid: String,
    /// UID of the agent session that spawned/owns this one. Used by
    /// the descendant-only auth check in Phase 3 and by sidebar
    /// "managed-by" markers later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    managed_by_uid: Option<String>,
    /// Bumps every time `transcript_id` rebinds. Persisted so a
    /// pre-restart cursor against an old transcript correctly mismatches
    /// the post-restart generation and resets to offset 0.
    #[serde(default)]
    generation: u64,
    label: String,
    session_type: String,
    /// Current transcript file UUID. Older manifests stored this as
    /// `session_id`; the alias keeps backfill correct across upgrade.
    #[serde(alias = "session_id")]
    transcript_id: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    idle_timeout_secs: u16,
    #[serde(default)]
    burst_threshold: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
    #[serde(default)]
    notify_on_idle: bool,
    /// Name of the agent-memory snapshot this session was cloned from, if
    /// any. Informational provenance only — used to surface "Seeded from:
    /// <name>" in session info. See DESIGN_AGENT_MEMORIES.md.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seeded_from_snapshot: Option<String>,
}

/// Persisted workspace metadata. Lives in `Manifest::workspaces` keyed by the
/// workspace's stable id.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct ManifestWorkspace {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    is_closed: bool,
    #[serde(default)]
    is_cloud: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worktree_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    main_repo_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    repo_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_vm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    worker_zone: Option<String>,
    #[serde(default)]
    sessions: Vec<ManifestEntry>,
    /// Recently-closed sessions kept around so `read_session_output` can
    /// resolve a transcript path even after the session is gone. Pruned
    /// on TUI startup; see `TOMBSTONE_RETENTION_SECS`.
    #[serde(default)]
    tombstones: Vec<SessionTombstone>,
}

/// Lightweight record of a session that's been closed. Holds only what
/// the resolver and sidebar need; the live `TerminalSession` (which owns
/// PTY resources) is dropped at close time. Keeping the full struct
/// alive after exit would leak the PTY writer file descriptor.
///
/// **Self-contained**: every field needed to resolve `transcript_path`
/// for an `exited`-state read is on the tombstone itself, not on the
/// workspace. This matters because workspace state mutates after a
/// session closes (e.g. `push_active` clears `worktree_path` when a
/// local workspace gets uploaded to cloud). If resolution depended on
/// the workspace's *current* `worktree_path`, those tombstones would
/// silently stop resolving even though the on-disk transcript file
/// still exists at the path captured at exit time.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct SessionTombstone {
    pub uid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_by_uid: Option<String>,
    pub label: String,
    /// "claude" / "codex" / "bash"
    pub session_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Last transcript file UUID this session was bound to. Used by the
    /// resolver to compute a `transcript_path` for `state: "exited"`
    /// reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transcript_id: Option<String>,
    /// Worktree path captured at exit time. Snapshot, not a live
    /// reference — survives subsequent mutations of the workspace's
    /// `worktree_path`. Required to compute Claude Code transcript
    /// paths (Codex's path scheme is per-user-and-date and ignores it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<PathBuf>,
    pub generation: u64,
    /// Unix-timestamp seconds when the session exited. Used for the
    /// retention prune.
    pub exited_at: f64,
}

/// How long tombstones live before the startup prune drops them. 30 days
/// is generous because the data is small and these are exactly the
/// records an agent might want to look at later.
const TOMBSTONE_RETENTION_SECS: f64 = 30.0 * 24.0 * 60.0 * 60.0;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct Manifest {
    /// Workspaces keyed by stable workspace id.
    #[serde(default)]
    workspaces: HashMap<String, ManifestWorkspace>,
    /// `task_id` → `workspace_id` bindings. A task present here is bound to
    /// the referenced workspace.
    #[serde(default)]
    bindings: HashMap<String, String>,
    #[serde(default)]
    view: Option<String>,
}

/// An execution context: a worktree (local) or cloud worker (remote) plus
/// the sessions running in it. Any number of `TaskEntry`s can point at a
/// workspace via `TaskEntry::workspace_id`; none is also valid (standalone
/// workspace created via A-n).
pub struct Workspace {
    pub id: String,
    pub name: String,
    pub is_closed: bool,
    pub is_cloud: bool,
    pub repo_url: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub main_repo_path: Option<PathBuf>,
    pub worker_vm: Option<String>,
    pub worker_zone: Option<String>,
    pub sessions: Vec<TerminalSession>,
    /// Records of closed sessions in this workspace. Resolver consults
    /// these (after `sessions`) to answer `read_session_output` for an
    /// already-exited session — its last-known transcript file remains
    /// on disk and the `last_transcript_id` field gives us the path.
    pub tombstones: Vec<SessionTombstone>,
    /// True between `push_active` and the matching `PushComplete` /
    /// `PushFailed` event from the backend. Transient — not persisted
    /// in `ManifestWorkspace`, so a TUI restart mid-push surfaces as
    /// "not pushing" rather than wedging on a stuck flag (the user can
    /// retry; the worst case is a duplicate `cm/push-*` branch).
    pub is_pushing: bool,
}

/// A task tracked in the planning/API layer. Pure metadata — no execution
/// state. `workspace_id` points at the Workspace this task has been launched
/// into (None when still in backlog / never launched).
#[derive(Clone)]
pub struct TaskEntry {
    pub task_id: Option<String>,
    pub name: String,
    pub api_status: TaskStatus,
    pub repo_url: Option<String>,
    pub prompt: Option<String>,
    pub wip_branch: Option<String>,
    pub session_id: Option<String>,
    pub blocked_at: Option<String>,
    pub is_cloud: bool,
    /// FK to `App.workspaces`. None = task in backlog, not bound yet.
    pub workspace_id: Option<String>,
    /// Planning project this task belongs to. Read from the API row;
    /// `backend.rs::filter_project_tasks` filters tasks with `None` out
    /// of the planning view, so subtasks need this populated to be
    /// visible after a planning refresh.
    pub project: Option<String>,
    /// FK to another `TaskEntry` (by `task_id`). None = top-level task.
    /// Phase 5 uses this for the subtask MCP tools and the planning-view
    /// tree. Set when an agent calls `create_subtask` or when the user
    /// creates a subtask in the planning view.
    pub parent_task_id: Option<String>,
    /// Worktree behavior for subtasks. Only meaningful when
    /// `parent_task_id` is set:
    ///   - "inherit" (default): sessions spawn in the parent's worktree.
    ///   - "branch": a new worktree is created off the parent's branch
    ///     with name `cm-sub/<slug-chain>-<short_id>`.
    pub worktree_mode: WorktreeMode,
}

/// How a subtask's worktree relates to its parent. Default = `Inherit`
/// per the design discussion (the common case is "go do this side thing
/// in the same codebase").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorktreeMode {
    #[default]
    Inherit,
    Branch,
}

/// Parse the API's `worktree_mode` string into the enum. Unknown values
/// fall through to `Inherit` — the safer default if the server sends
/// something we don't recognize (e.g. a future variant we haven't
/// shipped yet).
pub fn parse_worktree_mode(s: &str) -> WorktreeMode {
    match s {
        "branch" => WorktreeMode::Branch,
        _ => WorktreeMode::Inherit,
    }
}

impl WorktreeMode {
    pub fn as_wire(&self) -> &'static str {
        match self {
            WorktreeMode::Inherit => "inherit",
            WorktreeMode::Branch => "branch",
        }
    }
}

/// Build a TerminalSession wrapping a freshly-spawned PTY with default state.
/// Used by attach/spawn flows that don't need pending prompts or workflow tags.
fn make_simple_session(
    label: &str,
    session_type: &str,
    session: Session,
    pending_jsonl_files: Option<Vec<String>>,
) -> TerminalSession {
    make_simple_session_with_uid(new_session_uid(), label, session_type, session, pending_jsonl_files)
}

/// Variant for spawn paths that pre-generate the uid so they can wire it
/// into MCP config env (`CM_TUI_SESSION_ID` must match `ts.uid`). Without
/// matching values, the agent's tool calls fail authorization.
fn make_simple_session_with_uid(
    uid: String,
    label: &str,
    session_type: &str,
    session: Session,
    pending_jsonl_files: Option<Vec<String>>,
) -> TerminalSession {
    TerminalSession {
        uid,
        label: label.to_string(),
        session_type: session_type.to_string(),
        session,
        status: SessionStatus::Running,
        last_write_at: None,
        transcript_id: None,
        generation: 0,
        pending_jsonl_files,
        hidden: false,
        idle_timeout_secs: 0,
        burst_threshold: 0,
        pending_prompt: None,
        pending_clear: None,
        workflow_run_id: None,
        workflow_role: None,
        last_delivery: None,
        task_id: None,
        notify_on_idle: false,
        pending_enter: None,
        created_at: Instant::now(),
        managed_by_uid: None,
        seeded_from_snapshot: None,
    }
}

/// Fire a desktop notification announcing that a session went idle. Spawned
/// onto a detached thread so a slow/blocked dbus call can't stall the UI loop.
/// Errors are intentionally swallowed — a missing notification daemon is not
/// a reason to surface anything to the user.
fn notify_session_idle(label: &str) {
    let label = label.to_string();
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .summary("Claude Manager")
            .body(&format!("Session idle: {}", label))
            .show();
    });
}

/// Generate a fresh workspace id. Not cryptographic — just collision-avoidance
/// across the user's manifest via nanosecond timestamp.
pub(crate) fn new_workspace_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ws-{:x}", nanos)
}

/// Repo URLs in deterministic order (sorted by repo name). Used by the New
/// Workspace picker so the dropdown is stable across launches and the ←/→
/// cycle order matches what the user sees. Free function (vs `&self` method)
/// so it composes with split borrows when callers already hold `&mut
/// self.input_mode`.
fn sorted_repo_urls(repos: &HashMap<String, String>) -> Vec<String> {
    let mut entries: Vec<(&String, &String)> = repos.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries.into_iter().map(|(_, url)| url.clone()).collect()
}

/// Map a TerminalSession's `session_type` ("claude" | "codex" | "bash") to the
/// `Engine` used by transcript readers. Workflow code must derive engine from
/// the actual bound session (not from the workflow TOML role spec) since the
/// user can bind, e.g., a codex session into a role declared as "claude-code".
/// Per-session info needed to resolve a `/clear` or `/compact` rotation
/// detected via `~/.claude/history.jsonl`. Built fresh each tick from
/// the current workspace state.
pub(crate) struct RotationBinding {
    pub wi: usize,
    pub si: usize,
    /// `(run_id, role)` when the session is a workflow participant,
    /// `None` otherwise (regular `A-n` / planning panes). The fix was
    /// to also include non-workflow sessions — they need rotation
    /// rebinds too so `read_session_output` doesn't stall on the
    /// pre-rotation transcript file.
    pub workflow: Option<(String, String)>,
    pub worktree: std::path::PathBuf,
}

/// Build the (sid → RotationBinding) map for every live Claude session.
/// Extracted from the rotation-resolve loop in `drain_terminal_events`
/// so it can be tested without spinning up an App. Skips sessions
/// without a `transcript_id` (nothing to rebind), without a worktree
/// (no project dir to scan), or whose `session_type` isn't `"claude"`
/// (Codex doesn't go through the history.jsonl rotation path).
pub(crate) fn collect_rotation_bindings(
    workspaces: &[Workspace],
) -> HashMap<String, RotationBinding> {
    let mut out: HashMap<String, RotationBinding> = HashMap::new();
    for (wi, ws) in workspaces.iter().enumerate() {
        let Some(wt) = ws.worktree_path.clone() else {
            continue;
        };
        for (si, ts) in ws.sessions.iter().enumerate() {
            if ts.session_type != "claude" {
                continue;
            }
            let Some(sid) = &ts.transcript_id else {
                continue;
            };
            let workflow = match (
                ts.workflow_run_id.as_deref(),
                ts.workflow_role.as_deref(),
            ) {
                (Some(rid), Some(role)) => Some((rid.to_string(), role.to_string())),
                _ => None,
            };
            out.insert(
                sid.clone(),
                RotationBinding {
                    wi,
                    si,
                    workflow,
                    worktree: wt.clone(),
                },
            );
        }
    }
    out
}

pub(crate) fn engine_for_session_type(session_type: &str) -> workflow::toml_schema::Engine {
    match session_type {
        "codex" => workflow::toml_schema::Engine::Codex,
        _ => workflow::toml_schema::Engine::ClaudeCode,
    }
}

/// Decide what prompt to deliver to a workflow's initial role and
/// produce its final string form. Invoked only by the MCP launch
/// path (`start_workflow_run`).
///
/// Two delivery paths, picked in order:
///   1. Role's `activation_prompt` template — passed through `render`
///      so workflow context (`{{ goal }}`, `{{ roles.X.last_message }}`,
///      etc.) gets resolved.
///   2. The run's `goal`, used **verbatim**. Critically, this bypasses
///      `render` — a goal containing literal `{{` (Mustache examples,
///      JSON template fragments, code samples) would otherwise be
///      mangled. Both shipped workflows (feedback, review) leave the
///      initial role's `activation_prompt` unset, so the goal path is
///      the common case.
///
/// `render` is `FnOnce` so callers can build a resolver lazily — the
/// goal-only path doesn't need one and shouldn't pay for it.
///
/// Empty-after-trim inputs are treated as not set; returns `None`
/// when neither path yields content.
pub(crate) fn prepare_initial_prompt<F>(
    activation_prompt: Option<&str>,
    goal: Option<&str>,
    render: F,
) -> Option<String>
where
    F: FnOnce(&str) -> String,
{
    if let Some(template) = activation_prompt.filter(|s| !s.trim().is_empty()) {
        let rendered = render(template);
        if rendered.trim().is_empty() {
            None
        } else {
            Some(rendered)
        }
    } else if let Some(goal_text) = goal.filter(|s| !s.trim().is_empty()) {
        Some(goal_text.to_string())
    } else {
        None
    }
}

/// Kill the agent process inside `ts` and respawn it under the same PTY slot
/// with workflow MCP config + resume args, so the role can call
/// `workflow_transition` / `workflow_done`. The user-launched session was
/// started without `--mcp-config`; this is what gives it the workflow tools.
///
/// Returns `None` on success, `Some(message)` on any failure (caller decides
/// whether to surface to the status bar). On failure the existing `Session`
/// is left untouched — we only swap if `Session::new` succeeds.
pub(crate) fn respawn_existing_with_workflow_mcp(
    ts: &mut TerminalSession,
    engine: &workflow::toml_schema::Engine,
    run_id: &str,
    role: &str,
    session_id: Option<&str>,
    worktree: Option<&Path>,
    cols: u16,
    rows: u16,
    config: &Config,
    cap_status: &crate::memory_cap::MemoryCapAvailability,
    kill_tx: &std::sync::mpsc::Sender<crate::session_watch::MemoryKillEvent>,
) -> Option<String> {
    let (sid, wt) = match (session_id, worktree) {
        (Some(sid), Some(wt)) => (sid, wt),
        _ => {
            return Some(format!(
                "Skipping reload of {}: session_id not detected — workflow MCP tools unavailable",
                role
            ));
        }
    };
    let workflow_meta = crate::mcp_config::WorkflowMeta { run_id, role };
    let codex_resume_baseline = if matches!(engine, workflow::toml_schema::Engine::Codex) {
        Some(App::list_codex_sessions(wt))
    } else {
        None
    };
    let (program, args) = match crate::mcp_config::build_args(
        engine,
        &ts.uid,
        Some(workflow_meta),
        Some(sid),
    ) {
        Ok(v) => v,
        Err(e) => {
            return Some(format!(
                "MCP config build failed for {}: {} — workflow MCP tools unavailable",
                role, e
            ));
        }
    };
    let session_type = engine.as_session_type();
    let new_sess = match crate::session::spawn_agent_session(
        session_type,
        &ts.uid,
        &program,
        &args,
        cols,
        rows,
        Some(wt.to_path_buf()),
        Default::default(),
        config,
        cap_status,
        kill_tx,
    ) {
        Ok(s) => s,
        Err(e) => {
            return Some(format!(
                "Reload failed for {}: {} — workflow MCP tools unavailable",
                role, e
            ));
        }
    };
    // Swap the Session: dropping the old one closes its PTY which reaps the
    // old agent process. Claude resumes in-place; modern Codex writes a new
    // rollout id for `codex resume <sid>`, so keep the old sid bound until the
    // detector sees the post-resume file and rebinds the role.
    ts.session = new_sess;
    ts.transcript_id = Some(sid.to_string());
    ts.pending_jsonl_files = codex_resume_baseline;
    ts.pending_prompt = None;
    ts.pending_clear = None;
    ts.pending_enter = None;
    ts.last_delivery = None;
    ts.status = SessionStatus::Idle;
    None
}

/// True if `entry` plausibly corresponds to a prompt delivery whose first
/// 120 chars equal `prefix`. Three matching modes:
///
/// 1. **Plain typed input** — `display` carries the raw text directly.
/// 2. **Legacy paste schema** — `pastedContents.<k>.content` holds the raw
///    text; the parser concatenated those into `paste_content`.
/// 3. **Post-2025 paste schema** — `pastedContents.<k>.contentHash` (a
///    redacted reference) replaces `.content`. There's no text to match,
///    but `display` becomes the placeholder `"[Pasted text #N +M lines]"`
///    which is an unambiguous "something was pasted here" signal. The
///    caller already constrains the entry to a specific project (worktree)
///    and a 2-second window around the delivery timestamp, so accepting
///    the placeholder still uniquely identifies the right session in
///    practice.
///
/// Mode 3 is the fix for the overnight-cleanup workflow stall: every goal
/// >5 lines triggered paste-redaction, the prefix never matched, and all
/// 5 workflows got stuck on iteration 1 because their workers never got
/// bound to their transcripts.
pub(crate) fn entry_matches_delivery(
    entry: &workflow::history::HistoryEntry,
    prefix: &str,
) -> bool {
    if prefix.is_empty() {
        return false;
    }
    entry.display.starts_with(prefix)
        || entry.paste_content.starts_with(prefix)
        || workflow::history::is_paste_placeholder(&entry.display)
}

/// Phase 6: format a `SystemTime` as `HH:MM:SS` in UTC. Used by the
/// activity-feed renderer; UTC keeps the implementation tiny (no chrono /
/// libc dep) and the absolute ordering of entries is what matters for
/// the feed, not local-clock alignment.
fn format_utc_hms(ts: std::time::SystemTime) -> String {
    let secs = ts
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

/// Phase 6: build a one-line summary for the activity feed from a
/// completed control-socket method call. Returns `Some(_)` only for
/// mutating methods; read-only methods (list_*, get_*, ping,
/// resolve_authorized_session, read_session_output) return `None` so
/// they don't pollute the feed.
///
/// Each summary is at-a-glance and includes the most relevant arg(s) —
/// session uid prefixes are shortened to 8 chars (uids are ASCII so
/// byte slicing is safe), text payloads are truncated to ~40 chars
/// with an ellipsis, and the result's primary id (e.g. new task_id
/// from create_subtask) is appended where useful.
fn activity_summary_for(
    method: &str,
    params: &serde_json::Value,
    result_value: &serde_json::Value,
) -> Option<String> {
    use serde_json::Value as V;
    /// Truncate a session uid / task id to the first 8 ASCII chars.
    fn short(s: &str) -> String {
        s.chars().take(8).collect()
    }
    /// Compact text snippet for send_input: first ~40 chars + ellipsis.
    fn snippet(s: &str) -> String {
        let trimmed: String = s.chars().take(40).collect();
        if s.chars().count() > 40 {
            format!("{}…", trimmed)
        } else {
            trimmed
        }
    }
    match method {
        "send_input" => {
            let target = params.get("session_uid").and_then(V::as_str).unwrap_or("?");
            let text = params.get("text").and_then(V::as_str).unwrap_or("");
            Some(format!("send_input({}, {:?})", short(target), snippet(text)))
        }
        "kill_session" => {
            let target = params.get("session_uid").and_then(V::as_str).unwrap_or("?");
            Some(format!("kill_session({})", short(target)))
        }
        "start_session" => {
            let label = params.get("label").and_then(V::as_str).unwrap_or("?");
            let typ = params.get("type").and_then(V::as_str).unwrap_or("?");
            Some(format!("start_session({}, {})", label, typ))
        }
        "start_workflow" => {
            let name = params.get("workflow_name").and_then(V::as_str).unwrap_or("?");
            let task = params.get("task_id").and_then(V::as_str).unwrap_or("?");
            Some(format!("start_workflow({}, task={})", name, short(task)))
        }
        "stop_workflow" => {
            let run = params.get("run_id").and_then(V::as_str).unwrap_or("?");
            // Run ids are `wf_<hex>`; show the wf_ prefix + 8 chars of
            // hex so they're distinguishable from task ids.
            Some(format!("stop_workflow({})", run.chars().take(15).collect::<String>()))
        }
        "create_subtask" => {
            let name = params.get("name").and_then(V::as_str).unwrap_or("?");
            let mode = params
                .get("worktree_mode")
                .and_then(V::as_str)
                .unwrap_or("inherit");
            let new_id = result_value
                .get("task_id")
                .and_then(V::as_str)
                .unwrap_or("?");
            Some(format!(
                "create_subtask({}, {}) → {}",
                name,
                mode,
                short(new_id)
            ))
        }
        "mark_subtask_done" => {
            let task = params.get("task_id").and_then(V::as_str).unwrap_or("?");
            let close = params
                .get("close_worktree")
                .and_then(V::as_bool)
                .unwrap_or(true);
            Some(format!(
                "mark_subtask_done({}, close_worktree={})",
                short(task),
                close
            ))
        }
        // Read-only — explicitly NOT logged. List intentional so adding
        // a new method without thinking about it (the default arm below)
        // ALSO doesn't get logged accidentally; if you add a mutating
        // method, add a branch for it here.
        "ping"
        | "resolve_authorized_session"
        | "list_sessions"
        | "list_workflows"
        | "list_subtasks"
        | "get_workflow_state" => None,
        // Default: don't log unknown methods. New mutating methods must
        // be explicitly added above to surface in the feed.
        _ => None,
    }
}

pub(crate) fn new_session_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("ts-{:x}-{:x}", nanos, n)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cursor {
    /// Cursor is on a workspace header (by workspace index).
    Workspace(usize),
    /// Cursor is on a task subheader within a workspace. Identified by
    /// workspace index plus task_id (tasks can move / be renumbered, so an
    /// index wouldn't be stable).
    Task { ws_idx: usize, task_id: String },
    /// Cursor is on a session within a workspace (workspace index, session index).
    Session(usize, usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SidebarView {
    Status,
    Task,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ViewMode {
    Sessions,
    Planning,
}

#[derive(Clone, Debug)]
enum VisualItem {
    WorkspaceHeader(usize),
    /// Subheader for a task inside a workspace. Sessions tagged with this
    /// task_id follow immediately after.
    TaskHeader { ws_idx: usize, task_id: String },
    Session(usize, usize),
    Separator,
    /// Header row for a workflow grouping, followed by its participant Sessions.
    WorkflowHeader { ws_idx: usize, run_id: String },
}

/// Modal input state.
enum InputMode {
    /// Normal operation — keys go to terminal or app navigation.
    Normal,
    /// Typing a name/label for a new local session.
    NewSession {
        label_text: String,
        branch_text: String,
        idle_timeout_text: String,
        repo_url: String,
        /// `Some(snapshot_name)` when the session should be cloned from
        /// that agent-memory snapshot. Filled in via the picker invoked
        /// from field 4. See chunk 5 in DESIGN_AGENT_MEMORIES.md.
        seed_from: Option<String>,
        /// 0 = repo (←/→ to cycle), 1 = name, 2 = branch, 3 = idle timeout,
        /// 4 = seed-from (Enter opens snapshot picker, Esc clears)
        active_field: u8,
    },
    /// Picking a session type to add to a workspace.
    ///
    /// Target identified by stable `workspace_id` (NOT positional index).
    /// Backend events can fire `reconcile_tasks` while the form is open,
    /// which sorts `self.workspaces`. A stored index would silently point
    /// at the wrong workspace by submit time — the seeded clone could
    /// land in the wrong worktree. See chunk-3's SaveSnapshot fix for
    /// the same pattern.
    NewTerminalSession {
        workspace_id: String,
        session_type: String,
        /// Task scope inherited from the cursor when A-s was pressed. None =
        /// workspace-level session (no task subheader).
        task_id: Option<String>,
        /// `Some(snapshot_name)` when the session should be cloned from
        /// that agent-memory snapshot. Cleared whenever `session_type`
        /// changes — the picker filtered to a specific engine, and we
        /// can't carry a Codex snapshot across to a Claude session.
        seed_from: Option<String>,
        /// 0 = session type (j/k cycles), 1 = seed-from (Enter picks).
        active_field: u8,
    },
    /// Editing session settings.
    SessionSettings {
        ws_index: usize,
        session_index: usize,
        name: String,
        idle_timeout: String,
        burst_threshold: String,
        hidden: bool,
        notify_on_idle: bool,
        /// Read-only provenance: name of the agent-memory snapshot this
        /// session was cloned from, if any. Surfaced at the bottom of the
        /// dialog when `Some`. Not editable from settings.
        seeded_from_snapshot: Option<String>,
        /// 0 = name, 1 = idle timeout, 2 = burst threshold, 3 = hidden, 4 = notify on idle
        active_field: u8,
    },
    /// Renaming a workspace. Only the display label changes — the branch
    /// and worktree path stay the same.
    WorkspaceSettings {
        ws_index: usize,
        name: String,
    },
    /// Save the focused session as a named agent-memory snapshot. Opened
    /// via A-b in Sessions view; see `DESIGN_AGENT_MEMORIES.md` (chunk 3).
    ///
    /// Target identified by stable IDs (workspace id + session uid) — NOT
    /// by indices. Backend events can reorder `App.workspaces` while the
    /// modal is open, so indices would silently point at the wrong session
    /// (or fall out of bounds) by submit time.
    ///
    /// `error` is set when a submit fails so the modal can stay open and
    /// surface the reason inline (e.g. name conflict, no transcript yet).
    SaveSnapshot {
        workspace_id: String,
        session_uid: String,
        name_text: String,
        description_text: String,
        /// 0 = name, 1 = description.
        active_field: u8,
        error: Option<String>,
    },
    /// Browse / detail / rename / delete the agent-memory snapshot catalog.
    /// Opened via A-z in Sessions view. `mode` tracks the active sub-view
    /// (the catalog modal stays one InputMode variant rather than a family
    /// of variants, so the snapshots list is loaded once on open and shared
    /// across all sub-modes — list re-reads only happen after a rename or
    /// delete commits).
    ///
    /// `is_picker` toggles select-and-return semantics for chunk 5's
    /// seed-from-snapshot field: when true, Enter on a row would return
    /// the snapshot name to the caller instead of opening Detail, and
    /// rename / delete are disabled. Currently unused — chunk 4 only opens
    /// the catalog in browse mode (`is_picker = false`).
    SnapshotCatalog {
        snapshots: Vec<agent_memory::Snapshot>,
        selected: usize,
        mode: CatalogMode,
        /// `Some` when the catalog was opened in picker mode from a parent
        /// form — Enter on a row submits `SnapshotPicked` and the dispatch
        /// returns to that form with `seed_from` set. `None` for the
        /// stand-alone catalog opened via A-z.
        picker_target: Option<PickerTarget>,
        /// Transient error/status string surfaced at the bottom of the
        /// catalog (e.g. "Delete failed: …"). Cleared on the next user
        /// input so it doesn't linger across mode transitions.
        status_msg: Option<String>,
    },
    /// Renaming a task (updates the `name` field via the planning API and
    /// updates the local TaskEntry so the sidebar subheader refreshes).
    TaskSettings {
        task_id: String,
        name: String,
    },
    /// Picking which workflow to launch when more than one is defined.
    WorkflowPicker {
        ws_index: usize,
        focused_si: Option<usize>,
        names: Vec<String>,
        selected: usize,
    },
    /// Confirming launch of a workflow on a workspace.
    WorkflowLaunchConfirm {
        ws_index: usize,
        workflow_name: String,
        /// One slot per role, in presentation order.
        slots: Vec<WorkflowSlotChoice>,
        /// Index of the slot whose option can currently be cycled. When equal
        /// to `slots.len()`, focus is on the goal text field.
        active_slot: usize,
        /// Optional run-level goal typed by the user. Persists on the run so
        /// templates' `{{ goal }}` expands to it across restarts.
        goal: String,
    },
    /// Showing a workflow run's history.
    WorkflowHistory {
        run_id: String,
    },
    /// Picker over past workspaces (closed or all-tasks-done) so the user
    /// can reopen one without cluttering the sidebar. Opened via A-O from
    /// Sessions view. Carries the candidate list up-front instead of
    /// recomputing on every input event.
    PastWorkspacePicker {
        candidates: Vec<PastCandidate>,
        selected: usize,
    },
    /// Generic y/N confirmation overlay. The action runs only on `y`/`Y`/Enter;
    /// `n`/`N`/Esc cancels. Used to gate destructive keys (A-d, A-x).
    Confirm {
        prompt: String,
        action: ConfirmAction,
    },
}

/// Snapshot of a past workspace surfaced in the A-O picker. `worktree_exists`
/// is checked at modal-open time so the row can be greyed/disabled when the
/// directory has been removed since close.
#[derive(Clone, Debug)]
pub struct PastCandidate {
    pub ws_id: String,
    pub display: String,
    pub worktree_path: Option<std::path::PathBuf>,
    pub worktree_exists: bool,
    /// Latest tombstone `exited_at` if any — used to sort most-recent first.
    pub last_exited_at: f64,
}

#[derive(Clone, Debug)]
pub enum ConfirmAction {
    MarkDone,
    Delete,
    StopWorkflow { run_id: String },
}

/// Per-role slot in the launch modal. The user cycles through `options` with
/// left/right; `option_index` points at the currently-selected one.
#[derive(Clone, Debug)]
pub struct WorkflowSlotChoice {
    pub role: String,
    pub options: Vec<WorkflowSlotSource>,
    pub option_index: usize,
}

impl WorkflowSlotChoice {
    pub fn source(&self) -> &WorkflowSlotSource {
        &self.options[self.option_index]
    }
    pub fn cycle(&mut self, delta: i32) {
        if self.options.is_empty() {
            return;
        }
        let len = self.options.len() as i32;
        let next = ((self.option_index as i32 + delta).rem_euclid(len)) as usize;
        self.option_index = next;
    }
}

#[derive(Clone, Debug)]
pub enum WorkflowSlotSource {
    /// Use an existing session on the workspace, by index within `ws.sessions`.
    Existing(usize),
    /// Spawn a new session with the given engine.
    New(Engine),
}

// ── Input handler extraction ────────────────────────────────────────
//
// The per-mode arms of `handle_input_event` are implemented as free
// functions (`handle_<mode>`) so each modal can be unit-tested without
// booting an `App`. The functions:
//   - take a `<Mode>Mut<'_>` bag of refs into the `InputMode` variant
//     payload (so they can mutate cursor position, type characters, etc.),
//   - take an `InputCtx<'_>` for the read-only context that's needed
//     across more than one mode (currently just the repo URL list),
//   - return an `InputOutcome` describing the post-condition.
// The dispatcher (`handle_input_event`) translates the outcome into
// app-level state changes (mode swap, side-effect dispatch).

/// Read-only context handlers may need to make decisions. The whole-App
/// reference is too coarse — only fields actually needed by some handler
/// land here. The dispatcher builds this fresh per call.
pub(crate) struct InputCtx<'a> {
    /// Repo URLs in the user's config, sorted by repo name. Used by
    /// `handle_new_session` to cycle the repo picker (←/→).
    pub repo_urls: &'a [String],
}

/// Post-condition signal from a per-mode handler back to the dispatcher.
/// Handlers stay pure-ish: they mutate their own mode payload in place
/// (typing characters, cycling fields) and surface app-level transitions
/// through this enum.
#[derive(Debug, Clone)]
pub(crate) enum InputOutcome {
    /// Event handled by the modal; no app-level state change.
    Consumed,
    /// Event was not for any modal — fall through to terminal/app keys.
    /// Only `InputMode::Normal` returns this.
    Ignored,
    /// Reset `input_mode` to `Normal`. Used by Esc / explicit cancels.
    Cancel,
    /// Reset `input_mode` to `Normal` AND fire a side effect.
    Submit(SubmitAction),
}

/// Side effects requested by a `Submit` outcome. The dispatcher matches
/// on this and invokes the relevant `App` method; the handlers
/// themselves never see `&mut App`.
#[derive(Debug, Clone)]
pub(crate) enum SubmitAction {
    /// Submit attempted but the inputs produced no work to do (e.g.
    /// empty workspace name, no workflow selected). Modal still closes.
    None,
    CreateLocalSession {
        repo_url: String,
        label: String,
        branch: Option<String>,
        idle_timeout_secs: u16,
        seed_from: Option<String>,
    },
    SpawnSessionOnWorkspace {
        workspace_id: String,
        session_type: String,
        task_id: Option<String>,
        seed_from: Option<String>,
    },
    /// Open the snapshot catalog in picker mode from the A-n form. Carries
    /// the form state so the catalog can re-open the form (with seed_from
    /// set on pick or unchanged on cancel) on submit / cancel.
    OpenSnapshotPickerForNewSession {
        label_text: String,
        branch_text: String,
        idle_timeout_text: String,
        repo_url: String,
        existing_seed_from: Option<String>,
    },
    /// Open the snapshot catalog in picker mode from the A-s form.
    OpenSnapshotPickerForNewTerminalSession {
        workspace_id: String,
        session_type: String,
        task_id: Option<String>,
        existing_seed_from: Option<String>,
    },
    SaveSessionSettings {
        ws_index: usize,
        session_index: usize,
        name: String,
        idle_timeout: u16,
        burst_threshold: u16,
        hidden: bool,
        notify_on_idle: bool,
    },
    SaveWorkspaceName {
        ws_index: usize,
        name: String,
    },
    SaveSnapshot {
        workspace_id: String,
        session_uid: String,
        name: String,
        description: String,
    },
    /// Emitted when the catalog is opened in picker mode and the user
    /// presses Enter on a row. Chunk 4 doesn't open the catalog in picker
    /// mode anywhere — chunk 5's seed-from-snapshot field will. The arm
    /// in `apply_submit_action` is a no-op today so the variant exists
    /// to be wired up later.
    SnapshotPicked {
        name: String,
    },
    SaveTaskName {
        task_id: String,
        name: String,
    },
    EnterWorkflowLaunchConfirm {
        ws_index: usize,
        focused_si: Option<usize>,
        workflow_name: String,
    },
    LaunchWorkflow {
        ws_index: usize,
        workflow_name: String,
        slots: Vec<WorkflowSlotChoice>,
        goal: Option<String>,
    },
    MarkActiveDone,
    DeleteActive,
    StopWorkflow {
        run_id: String,
    },
    /// Picker chose a past workspace to reopen.
    ReopenPastWorkspace {
        ws_id: String,
    },
}

// Per-mode mutable-ref bags. Each handler takes its own bag so the
// dispatcher can split the borrow on `&mut self.input_mode` and pass
// only the variant payload through.

pub(crate) struct NewSessionMut<'a> {
    pub label_text: &'a mut String,
    pub branch_text: &'a mut String,
    pub idle_timeout_text: &'a mut String,
    pub repo_url: &'a mut String,
    pub seed_from: &'a mut Option<String>,
    pub active_field: &'a mut u8,
}

pub(crate) struct NewTerminalSessionMut<'a> {
    pub workspace_id: &'a str,
    pub session_type: &'a mut String,
    pub task_id: &'a Option<String>,
    pub seed_from: &'a mut Option<String>,
    pub active_field: &'a mut u8,
}

pub(crate) struct SessionSettingsMut<'a> {
    pub ws_index: usize,
    pub session_index: usize,
    pub name: &'a mut String,
    pub idle_timeout: &'a mut String,
    pub burst_threshold: &'a mut String,
    pub hidden: &'a mut bool,
    pub notify_on_idle: &'a mut bool,
    pub active_field: &'a mut u8,
}

pub(crate) struct WorkspaceSettingsMut<'a> {
    pub ws_index: usize,
    pub name: &'a mut String,
}

pub(crate) struct SaveSnapshotMut<'a> {
    pub workspace_id: &'a str,
    pub session_uid: &'a str,
    pub name_text: &'a mut String,
    pub description_text: &'a mut String,
    pub active_field: &'a mut u8,
    /// Cleared on the next user input so the previous error doesn't
    /// linger after the user starts correcting the form.
    pub error: &'a mut Option<String>,
}

/// What to return to after the catalog is opened in picker mode. Carries
/// the form state captured at picker-open time so that Esc (cancel) and
/// Enter (select) can both re-open the parent form with the user's typed
/// input intact. See chunk 5 in DESIGN_AGENT_MEMORIES.md.
///
/// `existing_seed_from` is the form's `seed_from` value at the moment the
/// picker was opened. Picker-cancel restores it (don't silently wipe a
/// previously-picked snapshot just because the user opened the picker to
/// look); picker-select overwrites it with the chosen name.
#[derive(Debug, Clone)]
pub enum PickerTarget {
    NewSession {
        label_text: String,
        branch_text: String,
        idle_timeout_text: String,
        repo_url: String,
        existing_seed_from: Option<String>,
    },
    NewTerminalSession {
        workspace_id: String,
        session_type: String,
        task_id: Option<String>,
        existing_seed_from: Option<String>,
    },
}

/// Compute the next `InputMode` and optional status toast for an
/// `open_snapshot_catalog` call given the result of
/// `agent_memory::list()`. Pure function so the failure path —
/// especially the recovery that re-opens the parent form when called
/// from a picker — is unit-testable without standing up an `App`.
///
/// - `Ok(snapshots)`: filter by the picker target's engine (if any),
///   build the `SnapshotCatalog` input mode. No status message.
/// - `Err(e)` with `picker_target = Some(t)`: re-open the parent form
///   via `rebuild_form_from_picker` so the user's typed input survives
///   the list failure. Status message describes the error.
/// - `Err(e)` with `picker_target = None`: drop back to `Normal` and
///   surface the error.
fn catalog_open_outcome(
    list_result: Result<Vec<agent_memory::Snapshot>, agent_memory::SnapshotError>,
    picker_target: Option<PickerTarget>,
) -> (InputMode, Option<String>) {
    match list_result {
        Ok(snapshots) => {
            let filtered = match &picker_target {
                Some(t) => {
                    let want = picker_target_engine(t);
                    snapshots
                        .into_iter()
                        .filter(|s| Some(&s.manifest.engine) == want.as_ref())
                        .collect()
                }
                None => snapshots,
            };
            (
                InputMode::SnapshotCatalog {
                    snapshots: filtered,
                    selected: 0,
                    mode: CatalogMode::Browse,
                    picker_target,
                    status_msg: None,
                },
                None,
            )
        }
        Err(e) => {
            let msg = format!("Could not list snapshots: {e}");
            let mode = match picker_target {
                Some(t) => rebuild_form_from_picker(t, None),
                None => InputMode::Normal,
            };
            (mode, Some(msg))
        }
    }
}

/// Rebuild the parent input form after a snapshot picker round-trip.
///
/// - `name = Some(picked)`: overwrite `seed_from` with the picked
///   snapshot (used by `SubmitAction::SnapshotPicked`).
/// - `name = None`: preserve `existing_seed_from` from the target so
///   picker-cancel doesn't silently wipe a previously-picked snapshot
///   (the user opening the picker to look at options should be a no-op
///   if they bail).
///
/// Extracted as a free function so the round-trip is unit-testable
/// without constructing an `App`.
fn rebuild_form_from_picker(target: PickerTarget, name: Option<String>) -> InputMode {
    match target {
        PickerTarget::NewSession {
            label_text,
            branch_text,
            idle_timeout_text,
            repo_url,
            existing_seed_from,
        } => InputMode::NewSession {
            label_text,
            branch_text,
            idle_timeout_text,
            repo_url,
            seed_from: name.or(existing_seed_from),
            // Keep the picker field selected so the user sees the
            // result of their pick (or non-pick) land in context.
            active_field: 4,
        },
        PickerTarget::NewTerminalSession {
            workspace_id,
            session_type,
            task_id,
            existing_seed_from,
        } => InputMode::NewTerminalSession {
            workspace_id,
            session_type,
            task_id,
            seed_from: name.or(existing_seed_from),
            active_field: 1,
        },
    }
}

/// Engine constraint the catalog enforces when opened in picker mode.
/// `NewSession` always spawns a Claude Code session, so the filter is
/// always `ClaudeCode`. `NewTerminalSession` filters to whichever engine
/// the user selected on the form (no filter for bash — that path
/// doesn't reach the picker).
fn picker_target_engine(t: &PickerTarget) -> Option<Engine> {
    match t {
        PickerTarget::NewSession { .. } => Some(Engine::ClaudeCode),
        PickerTarget::NewTerminalSession { session_type, .. } => {
            match session_type.as_str() {
                "claude" => Some(Engine::ClaudeCode),
                "codex" => Some(Engine::Codex),
                _ => None,
            }
        }
    }
}

/// Cheap pre-flight check that the named snapshot loads cleanly. Used
/// by `create_local_session` to fail-fast BEFORE creating a git worktree
/// — otherwise a seeded A-n with a bad snapshot name would leave an
/// orphan worktree + branch on disk and block retries on the same label.
///
/// The later `clone_snapshot_for_spawn` re-loads the snapshot to do the
/// actual materialization; this just rejects names that would never
/// succeed. Free function so tests can drive it against an isolated
/// HOME without standing up an `App`.
fn validate_seed_loadable(name: &str) -> std::result::Result<(), String> {
    agent_memory::load(name).map(|_| ()).map_err(|e| format!("Snapshot load failed: {e}"))
}

/// Resolve a stable `workspace_id` to its current position in
/// `App.workspaces`. Returns `None` if the workspace has been removed
/// since the id was captured (e.g. user deleted it, or reconcile_tasks
/// dropped a cloud workspace). Free function so it's unit-testable
/// against a hand-rolled `&[Workspace]`.
fn resolve_workspace_by_id(workspaces: &[Workspace], workspace_id: &str) -> Option<usize> {
    workspaces.iter().position(|w| w.id == workspace_id)
}

/// Snapshot-catalog sub-mode. Tracks what the user is currently doing
/// inside the catalog modal — all sub-modes share the same list of
/// snapshots and selection cursor.
#[derive(Debug, Clone)]
pub enum CatalogMode {
    /// Default list view. j/k navigate, Enter→Detail, r→Rename, d→ConfirmDelete.
    Browse,
    /// Read-only manifest + transcript head/tail preview. Esc/Enter→Browse.
    /// `head` and `tail` are loaded once on transition and cached so
    /// rendering doesn't hit disk every frame.
    Detail {
        head: Vec<String>,
        tail: Vec<String>,
    },
    /// In-line rename of the selected snapshot. Enter commits via
    /// `agent_memory::rename`; Esc returns to Browse without changes.
    Rename {
        text: String,
        error: Option<String>,
    },
    /// "Delete snapshot `<name>`? (y/n)" confirm prompt. y/Enter commits
    /// via `agent_memory::delete`; n/Esc returns to Browse.
    ConfirmDelete,
}

pub(crate) struct SnapshotCatalogMut<'a> {
    pub snapshots: &'a mut Vec<agent_memory::Snapshot>,
    pub selected: &'a mut usize,
    pub mode: &'a mut CatalogMode,
    pub picker_target: Option<&'a PickerTarget>,
    pub status_msg: &'a mut Option<String>,
}

impl SnapshotCatalogMut<'_> {
    fn is_picker(&self) -> bool {
        self.picker_target.is_some()
    }
}

pub(crate) struct TaskSettingsMut<'a> {
    pub task_id: &'a str,
    pub name: &'a mut String,
}

pub(crate) struct WorkflowLaunchConfirmMut<'a> {
    pub ws_index: usize,
    pub workflow_name: &'a str,
    pub slots: &'a mut Vec<WorkflowSlotChoice>,
    pub active_slot: &'a mut usize,
    pub goal: &'a mut String,
}

pub(crate) struct WorkflowPickerMut<'a> {
    pub ws_index: usize,
    pub focused_si: Option<usize>,
    pub names: &'a [String],
    pub selected: &'a mut usize,
}

pub(crate) fn handle_new_session(
    state: NewSessionMut<'_>,
    ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    const FIELD_COUNT: u8 = 5; // repo, label, branch, idle, seed-from
    match key.code {
        KeyCode::Esc => {
            // Esc on the seed-from field with a value clears the
            // selection instead of cancelling the whole form — matches
            // the design: "Esc clears the selection back to [none]".
            if *state.active_field == 4 && state.seed_from.is_some() {
                *state.seed_from = None;
                InputOutcome::Consumed
            } else {
                InputOutcome::Cancel
            }
        }
        KeyCode::Tab => {
            *state.active_field = (*state.active_field + 1) % FIELD_COUNT;
            InputOutcome::Consumed
        }
        KeyCode::BackTab => {
            *state.active_field = if *state.active_field == 0 {
                FIELD_COUNT - 1
            } else {
                *state.active_field - 1
            };
            InputOutcome::Consumed
        }
        KeyCode::Left | KeyCode::Right if *state.active_field == 0 => {
            if let Some(cur) = ctx.repo_urls.iter().position(|u| u == state.repo_url) {
                let n = ctx.repo_urls.len();
                let next = if matches!(key.code, KeyCode::Right) {
                    (cur + 1) % n
                } else {
                    (cur + n - 1) % n
                };
                *state.repo_url = ctx.repo_urls[next].clone();
            }
            InputOutcome::Consumed
        }
        KeyCode::Enter if *state.active_field == 4 => {
            // Open the snapshot catalog in picker mode. The dispatcher
            // stashes the form state on the submit action so it can
            // re-open this exact form after the user picks (or cancels).
            // existing_seed_from is captured so picker-cancel doesn't
            // wipe a previously-picked snapshot.
            InputOutcome::Submit(SubmitAction::OpenSnapshotPickerForNewSession {
                label_text: state.label_text.clone(),
                branch_text: state.branch_text.clone(),
                idle_timeout_text: state.idle_timeout_text.clone(),
                repo_url: state.repo_url.clone(),
                existing_seed_from: state.seed_from.clone(),
            })
        }
        KeyCode::Enter => {
            if !state.label_text.trim().is_empty() {
                let branch = if state.branch_text.trim().is_empty() {
                    None
                } else {
                    Some(state.branch_text.clone())
                };
                let timeout = state
                    .idle_timeout_text
                    .parse::<u16>()
                    .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
                InputOutcome::Submit(SubmitAction::CreateLocalSession {
                    repo_url: state.repo_url.clone(),
                    label: state.label_text.clone(),
                    branch,
                    idle_timeout_secs: timeout,
                    seed_from: state.seed_from.clone(),
                })
            } else {
                InputOutcome::Consumed
            }
        }
        KeyCode::Backspace => {
            match *state.active_field {
                1 => {
                    state.label_text.pop();
                }
                2 => {
                    state.branch_text.pop();
                }
                3 => {
                    state.idle_timeout_text.pop();
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            match *state.active_field {
                1 => state.label_text.push(c),
                2 => state.branch_text.push(c),
                3 => {
                    if c.is_ascii_digit() {
                        state.idle_timeout_text.push(c);
                    }
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_new_terminal_session(
    state: NewTerminalSessionMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    // Two fields: 0 = session type, 1 = seed-from. Tab/BackTab cycle
    // between them; j/k cycle the session type when field 0 is active
    // (within-field). j/k on the seed-from field are no-ops (would
    // conflict with the picker selection later).
    match (key.code, *state.active_field) {
        (KeyCode::Esc, 1) if state.seed_from.is_some() => {
            *state.seed_from = None;
            InputOutcome::Consumed
        }
        (KeyCode::Esc, _) => InputOutcome::Cancel,
        (KeyCode::Tab, _) | (KeyCode::BackTab, _) => {
            *state.active_field = (*state.active_field + 1) % 2;
            InputOutcome::Consumed
        }
        (KeyCode::Char('j') | KeyCode::Down, 0) => {
            *state.session_type = match state.session_type.as_str() {
                "claude" => "codex".to_string(),
                "codex" => "bash".to_string(),
                _ => "claude".to_string(),
            };
            // Engine changed — any previously-picked snapshot was
            // engine-filtered for the OLD value and no longer applies.
            *state.seed_from = None;
            InputOutcome::Consumed
        }
        (KeyCode::Char('k') | KeyCode::Up, 0) => {
            *state.session_type = match state.session_type.as_str() {
                "claude" => "bash".to_string(),
                "bash" => "codex".to_string(),
                _ => "claude".to_string(),
            };
            *state.seed_from = None;
            InputOutcome::Consumed
        }
        (KeyCode::Enter, 1) => {
            // Bash doesn't have transcripts and so isn't pickable; bounce
            // the user back to field 0 with a no-op rather than opening
            // an empty picker.
            if state.session_type == "bash" {
                return InputOutcome::Consumed;
            }
            InputOutcome::Submit(
                SubmitAction::OpenSnapshotPickerForNewTerminalSession {
                    workspace_id: state.workspace_id.to_string(),
                    session_type: state.session_type.clone(),
                    task_id: state.task_id.clone(),
                    existing_seed_from: state.seed_from.clone(),
                },
            )
        }
        (KeyCode::Enter, _) => {
            InputOutcome::Submit(SubmitAction::SpawnSessionOnWorkspace {
                workspace_id: state.workspace_id.to_string(),
                session_type: state.session_type.clone(),
                task_id: state.task_id.clone(),
                seed_from: state.seed_from.clone(),
            })
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_session_settings(
    state: SessionSettingsMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Tab | KeyCode::BackTab => {
            *state.active_field = (*state.active_field + 1) % 5;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') if *state.active_field == 3 => {
            *state.hidden = !*state.hidden;
            InputOutcome::Consumed
        }
        KeyCode::Char(' ') if *state.active_field == 4 => {
            *state.notify_on_idle = !*state.notify_on_idle;
            InputOutcome::Consumed
        }
        KeyCode::Enter => {
            let new_timeout = state
                .idle_timeout
                .parse::<u16>()
                .unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
            let new_burst = state
                .burst_threshold
                .parse::<u16>()
                .unwrap_or(WAKEUP_BURST_THRESHOLD as u16)
                .max(1);
            InputOutcome::Submit(SubmitAction::SaveSessionSettings {
                ws_index: state.ws_index,
                session_index: state.session_index,
                name: state.name.clone(),
                idle_timeout: new_timeout,
                burst_threshold: new_burst,
                hidden: *state.hidden,
                notify_on_idle: *state.notify_on_idle,
            })
        }
        KeyCode::Backspace => {
            match *state.active_field {
                0 => {
                    state.name.pop();
                }
                1 => {
                    state.idle_timeout.pop();
                }
                2 => {
                    state.burst_threshold.pop();
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            match *state.active_field {
                0 => state.name.push(c),
                1 => {
                    if c.is_ascii_digit() {
                        state.idle_timeout.push(c);
                    }
                }
                2 => {
                    if c.is_ascii_digit() {
                        state.burst_threshold.push(c);
                    }
                }
                _ => {}
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workspace_settings(
    state: WorkspaceSettingsMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => InputOutcome::Submit(SubmitAction::SaveWorkspaceName {
            ws_index: state.ws_index,
            name: state.name.trim().to_string(),
        }),
        KeyCode::Backspace => {
            state.name.pop();
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            state.name.push(c);
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_snapshot_catalog(
    state: SnapshotCatalogMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };

    // Alt+z anywhere closes the catalog (matches the open binding so it
    // toggles). Esc behaves contextually inside each sub-mode below.
    if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('z') {
        return InputOutcome::Cancel;
    }

    // Clear any transient status message ("Delete failed: …" etc.) on the
    // next keystroke so it doesn't linger across unrelated interactions.
    // Sub-handlers can re-set it for the current event if needed.
    *state.status_msg = None;

    match state.mode.clone() {
        CatalogMode::Browse => handle_catalog_browse(state, key),
        CatalogMode::Detail { .. } => handle_catalog_detail(state, key),
        CatalogMode::Rename { text, error } => {
            handle_catalog_rename(state, key, text, error)
        }
        CatalogMode::ConfirmDelete => handle_catalog_delete(state, key),
    }
}

fn handle_catalog_browse(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
) -> InputOutcome {
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Down | KeyCode::Char('j') => {
            if !state.snapshots.is_empty() {
                *state.selected = (*state.selected + 1) % state.snapshots.len();
            }
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if !state.snapshots.is_empty() {
                *state.selected = if *state.selected == 0 {
                    state.snapshots.len() - 1
                } else {
                    *state.selected - 1
                };
            }
            InputOutcome::Consumed
        }
        KeyCode::Enter => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                return InputOutcome::Consumed;
            };
            if state.is_picker() {
                return InputOutcome::Submit(SubmitAction::SnapshotPicked {
                    name: snap.name.clone(),
                });
            }
            let (head, tail) = read_transcript_head_tail(&snap.dir, 5);
            *state.mode = CatalogMode::Detail { head, tail };
            InputOutcome::Consumed
        }
        KeyCode::Char('r') if !state.is_picker() => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                return InputOutcome::Consumed;
            };
            *state.mode = CatalogMode::Rename {
                text: snap.name.clone(),
                error: None,
            };
            InputOutcome::Consumed
        }
        KeyCode::Char('d') if !state.is_picker() => {
            if state.snapshots.get(*state.selected).is_some() {
                *state.mode = CatalogMode::ConfirmDelete;
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

fn handle_catalog_detail(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
) -> InputOutcome {
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
            *state.mode = CatalogMode::Browse;
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

fn handle_catalog_rename(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
    mut text: String,
    error: Option<String>,
) -> InputOutcome {
    match key.code {
        KeyCode::Esc => {
            *state.mode = CatalogMode::Browse;
            InputOutcome::Consumed
        }
        KeyCode::Enter => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                *state.mode = CatalogMode::Browse;
                return InputOutcome::Consumed;
            };
            let old = snap.name.clone();
            let new = text.trim().to_string();
            if new == old {
                *state.mode = CatalogMode::Browse;
                return InputOutcome::Consumed;
            }
            match agent_memory::rename(&old, &new) {
                Ok(()) => match agent_memory::list() {
                    Ok(fresh) => {
                        // Move selection to the renamed entry so the
                        // cursor doesn't appear to jump arbitrarily.
                        let new_idx = fresh
                            .iter()
                            .position(|s| s.name == new)
                            .unwrap_or(0);
                        *state.snapshots = fresh;
                        *state.selected = new_idx;
                        *state.mode = CatalogMode::Browse;
                        InputOutcome::Consumed
                    }
                    Err(e) => {
                        *state.mode = CatalogMode::Rename {
                            text: new,
                            error: Some(format!("rename succeeded but reload failed: {e}")),
                        };
                        InputOutcome::Consumed
                    }
                },
                Err(e) => {
                    *state.mode = CatalogMode::Rename {
                        text: new,
                        error: Some(e.to_string()),
                    };
                    InputOutcome::Consumed
                }
            }
        }
        KeyCode::Backspace => {
            text.pop();
            *state.mode = CatalogMode::Rename { text, error: None };
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            // Drop the prior error so the user sees their typing land
            // before any new validation runs at submit.
            let _ = error;
            text.push(c);
            *state.mode = CatalogMode::Rename { text, error: None };
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

fn handle_catalog_delete(
    state: SnapshotCatalogMut<'_>,
    key: &crossterm::event::KeyEvent,
) -> InputOutcome {
    match key.code {
        KeyCode::Char('y') | KeyCode::Enter => {
            let Some(snap) = state.snapshots.get(*state.selected) else {
                *state.mode = CatalogMode::Browse;
                return InputOutcome::Consumed;
            };
            let name = snap.name.clone();
            match agent_memory::delete(&name) {
                Err(e) => {
                    // Keep list + selection as-is and surface the error.
                    // The previous code discarded the result and then
                    // potentially blanked the list via list().unwrap_or_default(),
                    // which silently dropped state on failures like a
                    // permission-denied rmdir.
                    *state.status_msg = Some(format!("Delete failed: {e}"));
                    *state.mode = CatalogMode::Browse;
                }
                Ok(()) => match agent_memory::list() {
                    Ok(fresh) => {
                        let len = fresh.len();
                        *state.snapshots = fresh;
                        *state.selected = if len == 0 {
                            0
                        } else {
                            (*state.selected).min(len - 1)
                        };
                        *state.mode = CatalogMode::Browse;
                    }
                    Err(e) => {
                        // Disk delete succeeded but we can't refresh the
                        // list. Keep the in-memory list intact (slightly
                        // stale) — better than blanking it. The user can
                        // close + reopen the catalog to retry the list.
                        *state.status_msg = Some(format!(
                            "Deleted, but reload failed: {e}"
                        ));
                        *state.mode = CatalogMode::Browse;
                    }
                },
            }
            InputOutcome::Consumed
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            *state.mode = CatalogMode::Browse;
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

/// Read up to `n` lines from the start and the end of `transcript.jsonl`
/// inside a snapshot dir. Cached in `CatalogMode::Detail` so the read
/// happens once per transition, not every frame.
///
/// Memory is O(n) regardless of file size: head is a `Vec<String>` capped at
/// `n`, tail is a `VecDeque<String>` ring buffer of capacity `n` that the
/// rest of the iterator drains into. Earlier implementation slurped the
/// whole transcript into memory just to take 5 lines from each end —
/// for multi-MB transcripts that stalled the UI on Detail open.
fn read_transcript_head_tail(
    snapshot_dir: &Path,
    n: usize,
) -> (Vec<String>, Vec<String>) {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader};

    let path = snapshot_dir.join("transcript.jsonl");
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let reader = BufReader::new(file);

    let mut head: Vec<String> = Vec::with_capacity(n);
    let mut tail: VecDeque<String> = VecDeque::with_capacity(n.saturating_add(1));

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            // Skip unreadable lines (e.g. invalid UTF-8 in the middle) but
            // keep the loop going so we still get a usable head/tail.
            Err(_) => continue,
        };
        if head.len() < n {
            head.push(line);
            continue;
        }
        if n == 0 {
            break;
        }
        if tail.len() == n {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    (head, tail.into_iter().collect())
}

/// Build the body lines of the catalog Rename overlay. Pure function so
/// unit tests can drive it without spinning up a Terminal/TestBackend.
/// Both `text` and `error` (validation messages can quote control bytes)
/// are sanitized before being placed into spans.
fn rename_overlay_lines(text: &str, error: Option<&str>) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let white = Style::default().fg(Color::White);
    let red = Style::default().fg(Color::Red);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("New name: ", dim),
            // Defensive: paste into the buffer could carry control
            // bytes — sanitize on render so the rename modal can't be
            // weaponized via clipboard injection.
            Span::styled(sanitize_for_display(text), white),
            Span::styled("\u{2588}", white),
        ]),
        Line::from(""),
    ];
    if let Some(msg) = error {
        // Validation errors (`validate_name`) quote the offending
        // character — including potentially an ESC. Sanitize on render
        // so the error message can't drive the terminal.
        lines.push(Line::from(Span::styled(sanitize_for_display(msg), red)));
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "Enter rename \u{00b7} Esc cancel",
        dim,
    )));
    lines
}

/// Compute the visible row range for a scrollable list given the current
/// selection. Keeps `selected` on-screen by centering it within the window
/// when possible and clamping to the top/bottom edges otherwise. Returns
/// `(start, end)` such that `start <= selected < end` and `end - start <=
/// visible` and `end <= total`. Both bounds are 0 when there's nothing to
/// show.
fn visible_range(selected: usize, total: usize, visible: usize) -> (usize, usize) {
    if total == 0 || visible == 0 {
        return (0, 0);
    }
    let visible = visible.min(total);
    let half = visible / 2;
    // Center the selection, clamping at the top.
    let mut start = selected.saturating_sub(half);
    // Pull start in so end never overshoots `total`.
    if start + visible > total {
        start = total - visible;
    }
    let end = start + visible;
    (start, end)
}

/// Strip bytes that would let an untrusted string drive the user's terminal
/// — ESC (0x1B), DEL (0x7F), and other C0 control characters except `\t`.
/// Snapshot manifests and transcript lines are user-authored or
/// agent-authored content; if one contains a stray `\x1b[2J` it would clear
/// the screen the moment the catalog opens. Apply this to every string
/// that came from a snapshot before handing it to ratatui.
///
/// `\n` is also stripped here because the catalog/detail renderers handle
/// line breaks themselves via separate `Line`s — letting a raw newline
/// through would visually merge a field with the following one.
fn sanitize_for_display(s: &str) -> String {
    s.chars()
        .filter(|c| {
            if *c == '\t' {
                return true;
            }
            !c.is_control()
        })
        .collect()
}

/// Truncate `s` to at most `max` characters (counting chars, not bytes).
/// Used for one-line previews of transcript text in the detail pane.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

/// Format a unix-seconds timestamp relative to `now_secs` as "just now",
/// "5m ago", "2h ago", "3d ago", etc. Saturates at "1y+ ago" so very old
/// timestamps don't render an awkwardly large number.
fn format_relative_time(then_secs: u64, now_secs: u64) -> String {
    let delta = now_secs.saturating_sub(then_secs);
    if delta < 60 {
        return "just now".to_string();
    }
    let minutes = delta / 60;
    if minutes < 60 {
        return format!("{minutes}m ago");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 365 {
        return format!("{days}d ago");
    }
    "1y+ ago".to_string()
}

pub(crate) fn handle_save_snapshot(
    state: SaveSnapshotMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Tab | KeyCode::BackTab => {
            *state.active_field = (*state.active_field + 1) % 2;
            *state.error = None;
            InputOutcome::Consumed
        }
        KeyCode::Enter => InputOutcome::Submit(SubmitAction::SaveSnapshot {
            workspace_id: state.workspace_id.to_string(),
            session_uid: state.session_uid.to_string(),
            name: state.name_text.trim().to_string(),
            description: state.description_text.trim().to_string(),
        }),
        KeyCode::Backspace => {
            let buf = if *state.active_field == 0 {
                &mut *state.name_text
            } else {
                &mut *state.description_text
            };
            buf.pop();
            *state.error = None;
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            let buf = if *state.active_field == 0 {
                &mut *state.name_text
            } else {
                &mut *state.description_text
            };
            buf.push(c);
            *state.error = None;
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_task_settings(
    state: TaskSettingsMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => InputOutcome::Submit(SubmitAction::SaveTaskName {
            task_id: state.task_id.to_string(),
            name: state.name.trim().to_string(),
        }),
        KeyCode::Backspace => {
            state.name.pop();
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            state.name.push(c);
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workflow_launch_confirm(
    state: WorkflowLaunchConfirmMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    // `active_slot == slots.len()` means the goal text field is focused;
    // typing goes there instead of cycling slots.
    let goal_focused = *state.active_slot == state.slots.len();
    let positions = state.slots.len() + 1;
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => {
            let goal_owned = state.goal.trim().to_string();
            let goal_opt = if goal_owned.is_empty() {
                None
            } else {
                Some(goal_owned)
            };
            InputOutcome::Submit(SubmitAction::LaunchWorkflow {
                ws_index: state.ws_index,
                workflow_name: state.workflow_name.to_string(),
                slots: state.slots.clone(),
                goal: goal_opt,
            })
        }
        KeyCode::Down | KeyCode::Tab => {
            *state.active_slot = (*state.active_slot + 1) % positions;
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::BackTab => {
            *state.active_slot = if *state.active_slot == 0 {
                positions - 1
            } else {
                *state.active_slot - 1
            };
            InputOutcome::Consumed
        }
        KeyCode::Right => {
            if !goal_focused {
                if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                    slot.cycle(1);
                }
            }
            InputOutcome::Consumed
        }
        KeyCode::Left => {
            if !goal_focused {
                if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                    slot.cycle(-1);
                }
            }
            InputOutcome::Consumed
        }
        KeyCode::Backspace => {
            if goal_focused {
                state.goal.pop();
            }
            InputOutcome::Consumed
        }
        KeyCode::Char(c) => {
            if goal_focused {
                state.goal.push(c);
            } else {
                // Slot navigation shorthands (only when the goal field
                // isn't focused, so characters in the goal don't get
                // consumed as commands).
                match c {
                    'j' => *state.active_slot = (*state.active_slot + 1) % positions,
                    'k' => {
                        *state.active_slot = if *state.active_slot == 0 {
                            positions - 1
                        } else {
                            *state.active_slot - 1
                        };
                    }
                    'l' | ' ' => {
                        if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                            slot.cycle(1);
                        }
                    }
                    'h' => {
                        if let Some(slot) = state.slots.get_mut(*state.active_slot) {
                            slot.cycle(-1);
                        }
                    }
                    _ => {}
                }
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workflow_picker(
    state: WorkflowPickerMut<'_>,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc => InputOutcome::Cancel,
        KeyCode::Enter => match state.names.get(*state.selected).cloned() {
            Some(wf_name) => {
                InputOutcome::Submit(SubmitAction::EnterWorkflowLaunchConfirm {
                    ws_index: state.ws_index,
                    focused_si: state.focused_si,
                    workflow_name: wf_name,
                })
            }
            None => InputOutcome::Submit(SubmitAction::None),
        },
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
            if !state.names.is_empty() {
                *state.selected = (*state.selected + 1) % state.names.len();
            }
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
            if !state.names.is_empty() {
                *state.selected = if *state.selected == 0 {
                    state.names.len() - 1
                } else {
                    *state.selected - 1
                };
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_past_workspace_picker(
    candidates: &[PastCandidate],
    selected: &mut usize,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => InputOutcome::Cancel,
        KeyCode::Enter => match candidates.get(*selected) {
            Some(c) => InputOutcome::Submit(SubmitAction::ReopenPastWorkspace {
                ws_id: c.ws_id.clone(),
            }),
            None => InputOutcome::Cancel,
        },
        KeyCode::Down | KeyCode::Tab | KeyCode::Char('j') => {
            if !candidates.is_empty() {
                *selected = (*selected + 1) % candidates.len();
            }
            InputOutcome::Consumed
        }
        KeyCode::Up | KeyCode::BackTab | KeyCode::Char('k') => {
            if !candidates.is_empty() {
                *selected = if *selected == 0 {
                    candidates.len() - 1
                } else {
                    *selected - 1
                };
            }
            InputOutcome::Consumed
        }
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_workflow_history(
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => InputOutcome::Cancel,
        _ => InputOutcome::Consumed,
    }
}

pub(crate) fn handle_confirm(
    action: &ConfirmAction,
    _ctx: InputCtx<'_>,
    event: &CrosstermEvent,
) -> InputOutcome {
    let CrosstermEvent::Key(key) = event else {
        return InputOutcome::Consumed;
    };
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            let submit = match action.clone() {
                ConfirmAction::MarkDone => SubmitAction::MarkActiveDone,
                ConfirmAction::Delete => SubmitAction::DeleteActive,
                ConfirmAction::StopWorkflow { run_id } => SubmitAction::StopWorkflow { run_id },
            };
            InputOutcome::Submit(submit)
        }
        KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => InputOutcome::Cancel,
        _ => InputOutcome::Consumed,
    }
}

pub struct App {
    pub tasks: Vec<TaskEntry>,
    /// Execution contexts. Sidebar rendering iterates workspaces, not tasks.
    pub workspaces: Vec<Workspace>,
    pub cursor: Cursor,
    pub sidebar_view: SidebarView,
    pub view_mode: ViewMode,
    pub planning: PlanningView,
    pub should_quit: bool,
    pub last_term_size: (u16, u16),
    pub config: Config,
    pub backend: BackendHandle,
    pub connected: bool,
    pub status_msg: Option<(String, Instant)>,
    pub needs_redraw: bool,
    input_mode: InputMode,
    start_time: Instant,
    sessions_restored: bool,
    /// Task→workspace bindings loaded from the manifest at startup. Consulted
    /// by reconcile_tasks before auto-provisioning so tasks that were already
    /// bound to a workspace don't spawn orphan duplicates when reconcile runs
    /// before restore_sessions populates self.workspaces.
    manifest_bindings: HashMap<String, String>,
    last_session_id_check: Instant,
    /// Workflow definitions loaded from `workflows/*.toml` at startup.
    pub workflows: HashMap<String, Workflow>,
    /// Files in the workflows directory that failed to parse or validate at
    /// startup. Surfaced in the workflow picker so a typo in a TOML doesn't
    /// silently make a workflow disappear without a hint.
    pub workflow_load_errors: Vec<(PathBuf, String)>,
    /// Active + recent workflow runs (persisted per run at ~/.cm/workflow-runs/).
    pub workflow_runs: Vec<WorkflowRun>,
    /// Tails `~/.claude/history.jsonl` for `/clear` and `/compact` events so
    /// we can detect when a bound workflow session rotates its transcript
    /// file. `None` if the history file couldn't be located at startup.
    history_watcher: Option<workflow::history::HistoryWatcher>,
    /// Rotation-trigger entries we've seen but haven't resolved yet because
    /// the new transcript file hadn't been created when we polled. Retry
    /// each tick until resolved or aged out.
    /// Each: (old_sid, timestamp_ms, first_seen_at).
    pending_rotations: Vec<(String, u64, Instant)>,
    /// Mouse capture state. When false, `DisableMouseCapture` has been sent so
    /// the user can use the terminal's native selection (including block-select
    /// chords). Toggle with Alt+m.
    pub mouse_capture_enabled: bool,
    /// Pending requests from the control socket. Drained each tick by the
    /// main loop and dispatched to method handlers. The server thread
    /// pushes; the main loop pops + replies. See `tui/src/control/`.
    control_queue: crate::control::queue::Queue,
    /// Phase 6 activity feed: ring buffer of agent-initiated mutations
    /// surfaced over the MCP control socket. Read-only methods (list_*,
    /// get_*, ping, read_session_output) are intentionally excluded —
    /// they're high-frequency and uninteresting in a feed. Capped at
    /// `ACTIVITY_LOG_CAP` entries (oldest evicted).
    pub activity_log: VecDeque<ActivityEntry>,
    /// Toggle for the bottom-of-screen activity strip. Off by default;
    /// `Alt-,` flips it.
    pub activity_visible: bool,
    /// Result of the startup memory-cap preflight probe. Cached for
    /// the lifetime of the run; consulted in `spawn_agent_session`
    /// to decide whether to wrap a spawn. See DESIGN_MEMORY_CAP.md
    /// § Components / Preflight.
    pub memory_cap_status: crate::memory_cap::MemoryCapAvailability,
    /// Channel watcher threads use to push `MemoryKillEvent`s back
    /// to the main loop. The receiver is drained each tick by
    /// `drain_memory_kill_events`. The sender is cloned into each
    /// capped session's watcher thread.
    pub memory_kill_tx: std::sync::mpsc::Sender<crate::session_watch::MemoryKillEvent>,
    pub memory_kill_rx: std::sync::mpsc::Receiver<crate::session_watch::MemoryKillEvent>,
}

/// Phase 6 activity-feed entry. Logged from each mutating control-socket
/// method handler via `App::log_activity`. Each entry is one observable
/// mutation (start_session, send_input, kill_session, start/stop_workflow,
/// create_subtask, mark_subtask_done, propose_task).
#[derive(Clone, Debug)]
pub struct ActivityEntry {
    /// Wall-clock timestamp the mutation landed (used for the leading
    /// `HH:MM:SS` column in the rendered strip).
    pub ts: std::time::SystemTime,
    /// Human-friendly caller label. For workflow participants this is
    /// the role name (`worker`/`reviewer`/`manager`); otherwise the
    /// session's sidebar label (e.g. `survey-claude`). Falls back to
    /// the caller's session_uid prefix if neither is available.
    pub caller_label: String,
    /// Compact one-line summary of the mutation, formatted by the
    /// caller. Example: `start_session(refactor-helpers, codex)` or
    /// `mark_subtask_done(b4264d86, close_worktree=true)`.
    pub summary: String,
}

/// How many activity entries to retain. ~50 covers a few minutes of busy
/// orchestration while keeping the buffer cheap. The strip itself only
/// renders the last few; the rest exist for a future scrollable view.
const ACTIVITY_LOG_CAP: usize = 50;

impl App {
    pub fn new(config: Config) -> Self {
        let backend = BackendHandle::spawn(&config);
        let manifest = Self::load_manifest();
        let sidebar_view = match manifest.view.as_deref() {
            Some("task") => SidebarView::Task,
            _ => SidebarView::Status,
        };
        // Only keep bindings whose target workspace still exists in the
        // manifest — otherwise we'd set workspace_id to a dangling id that
        // nothing resolves to.
        let known_ws_ids: HashSet<&String> = manifest.workspaces.keys().collect();
        let manifest_bindings: HashMap<String, String> = manifest
            .bindings
            .iter()
            .filter(|(_, ws_id)| known_ws_ids.contains(ws_id))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let workflows_dir = workflow::toml_schema::workflows_dir();
        let (workflows, load_errs) = workflow::toml_schema::load_all(&workflows_dir);
        let workflow_load_errors = filter_real_workflow_load_errors(&workflows_dir, load_errs);
        for (path, err) in &workflow_load_errors {
            eprintln!("workflow load failed: {}: {}", path.display(), err);
        }
        let workflow_runs = workflow::run::load_all()
            .into_iter()
            .filter(|r| r.is_active())
            .collect();
        // Start the control socket. Failures aren't fatal — the TUI runs
        // fine without it; only MCP-driven control becomes unavailable.
        let control_queue = crate::control::queue::Queue::new();
        match crate::control::server::start(control_queue.clone()) {
            Ok(path) => {
                eprintln!("control socket bound at {}", path.display());
            }
            Err(e) => {
                eprintln!("control socket failed to start: {}", e);
            }
        }
        // Memory-cap preflight: run once at startup, cache the result.
        // Subsequent `spawn_agent_session` calls consult this synchronously
        // — no per-spawn probing.
        let memory_cap_status = crate::preflight::probe();
        let mut activity_log: VecDeque<ActivityEntry> = VecDeque::new();
        if let crate::memory_cap::MemoryCapAvailability::Unavailable { reason } = &memory_cap_status
        {
            activity_log.push_back(ActivityEntry {
                ts: std::time::SystemTime::now(),
                caller_label: "preflight".into(),
                summary: format!("memory cap disabled: {}", reason),
            });
        }
        let (memory_kill_tx, memory_kill_rx) = std::sync::mpsc::channel();

        App {
            tasks: Vec::new(),
            workspaces: Vec::new(),
            cursor: Cursor::Workspace(0),
            sidebar_view,
            view_mode: ViewMode::Sessions,
            planning: PlanningView::new(),
            should_quit: false,
            last_term_size: (80, 24),
            config,
            backend,
            connected: false,
            status_msg: None,
            needs_redraw: true,
            input_mode: InputMode::Normal,
            start_time: Instant::now(),
            sessions_restored: false,
            manifest_bindings,
            last_session_id_check: Instant::now(),
            workflows,
            workflow_load_errors,
            workflow_runs,
            history_watcher: workflow::history::HistoryWatcher::new(),
            pending_rotations: Vec::new(),
            mouse_capture_enabled: true,
            control_queue,
            activity_log,
            activity_visible: false,
            memory_cap_status,
            memory_kill_tx,
            memory_kill_rx,
        }
    }

    /// Spawn an agent session through the cap-aware helper. Single
    /// entry point for every agent (`claude`/`codex`) PTY spawn — owns
    /// the `CM_TUI_SESSION_ID` env-population, the cap lookup, and
    /// the watcher thread. See `session::spawn_agent_session` for
    /// the full contract. Test/infra spawns (`/bin/true`, `gcloud`,
    /// `/bin/bash`) keep calling `Session::new` directly.
    pub fn spawn_agent_session(
        &self,
        session_type: &str,
        session_uid: &str,
        program: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        working_dir: Option<PathBuf>,
        env: HashMap<String, String>,
    ) -> anyhow::Result<Session> {
        crate::session::spawn_agent_session(
            session_type,
            session_uid,
            program,
            args,
            cols,
            rows,
            working_dir,
            env,
            &self.config,
            &self.memory_cap_status,
            &self.memory_kill_tx,
        )
    }

    /// Append a Phase 6 activity-feed entry. `caller_uid` is resolved to
    /// a friendly label (workflow role, else session label, else uid
    /// prefix). Capped at `ACTIVITY_LOG_CAP` — oldest entry evicted on
    /// overflow. Called by mutating control-socket method handlers.
    /// Read-only methods MUST NOT call this.
    pub fn log_activity(&mut self, caller_uid: &str, summary: String) {
        let caller_label = self.resolve_activity_caller_label(caller_uid);
        if self.activity_log.len() >= ACTIVITY_LOG_CAP {
            self.activity_log.pop_front();
        }
        self.activity_log.push_back(ActivityEntry {
            ts: std::time::SystemTime::now(),
            caller_label,
            summary,
        });
        self.needs_redraw = true;
    }

    fn resolve_activity_caller_label(&self, caller_uid: &str) -> String {
        for ws in &self.workspaces {
            for ts in &ws.sessions {
                if ts.uid == caller_uid {
                    if let Some(role) = &ts.workflow_role {
                        return role.clone();
                    }
                    return ts.label.clone();
                }
            }
            for tomb in &ws.tombstones {
                if tomb.uid == caller_uid {
                    return tomb.label.clone();
                }
            }
        }
        // Unknown caller — fall back to a uid prefix so the feed still
        // renders something searchable rather than the full opaque uid.
        caller_uid.chars().take(12).collect()
    }

    fn spinner_frame(&self) -> &'static str {
        let elapsed = self.start_time.elapsed().as_millis();
        let idx = (elapsed / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len();
        SPINNER_FRAMES[idx]
    }

    pub fn is_input_mode(&self) -> bool {
        !matches!(self.input_mode, InputMode::Normal)
    }



    /// List all .jsonl file stems in the Claude project directory for a worktree.
    pub(crate) fn list_jsonl_files(worktree_path: &Path) -> Vec<String> {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return Vec::new(),
        };
        let path_str = match worktree_path.to_str() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let encoded = path_str.replace('/', "-").replace('.', "-");
        let session_dir = home.join(format!(".claude/projects/{}", encoded));
        if !session_dir.is_dir() {
            return Vec::new();
        }
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&session_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        files.push(stem.to_string());
                    }
                }
            }
        }
        files
    }

    /// Detect a new session_id by finding .jsonl files that weren't in the existing list.
    /// Returns the newest new file's stem.
    pub(crate) fn detect_session_id(worktree_path: &Path, existing_files: &[String]) -> Option<String> {
        let home = dirs::home_dir()?;
        let path_str = worktree_path.to_str()?;
        let encoded = path_str.replace('/', "-").replace('.', "-");
        let session_dir = home.join(format!(".claude/projects/{}", encoded));
        if !session_dir.is_dir() {
            return None;
        }
        let mut newest: Option<(std::time::SystemTime, String)> = None;
        for entry in std::fs::read_dir(&session_dir).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if !existing_files.contains(&stem.to_string()) {
                        if let Ok(meta) = entry.metadata() {
                            if let Ok(modified) = meta.modified() {
                                if newest.as_ref().map_or(true, |(t, _)| modified > *t) {
                                    newest = Some((modified, stem.to_string()));
                                }
                            }
                        }
                    }
                }
            }
        }
        newest.map(|(_, id)| id)
    }

    /// List codex session IDs (UUIDs) that were started in the given worktree.
    pub(crate) fn list_codex_sessions(worktree_path: &Path) -> Vec<String> {
        Self::list_codex_sessions_with_mtime(worktree_path)
            .into_iter()
            .map(|(_, id)| id)
            .collect()
    }

    fn list_codex_sessions_with_mtime(worktree_path: &Path) -> Vec<(SystemTime, String)> {
        let home = match dirs::home_dir() {
            Some(h) => h,
            None => return Vec::new(),
        };
        let sessions_dir = home.join(".codex/sessions");
        if !sessions_dir.is_dir() {
            return Vec::new();
        }
        let wt_str = match worktree_path.to_str() {
            Some(s) => s.to_string(),
            None => return Vec::new(),
        };
        let mut ids = Vec::new();
        Self::walk_codex_sessions(&sessions_dir, &wt_str, &mut ids);
        ids
    }

    fn walk_codex_sessions(dir: &Path, wt_str: &str, ids: &mut Vec<(SystemTime, String)>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk_codex_sessions(&path, wt_str, ids);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                // Read just the first line — the JSONL files grow into the
                // megabytes and there are hundreds of them.
                let Some(first) = workflow::transcript::read_first_line(&path) else { continue };
                let Ok(val) = serde_json::from_str::<serde_json::Value>(first.trim()) else { continue };
                if val.pointer("/payload/cwd").and_then(|v| v.as_str()) != Some(wt_str) {
                    continue;
                }
                if let Some(id) = val.pointer("/payload/id").and_then(|v| v.as_str()) {
                    let modified = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(UNIX_EPOCH);
                    ids.push((modified, id.to_string()));
                }
            }
        }
    }

    /// Detect a new codex session_id by comparing against known IDs. Uses the
    /// user's default codex home.
    pub(crate) fn detect_codex_session_id(worktree_path: &Path, existing_ids: &[String]) -> Option<String> {
        Self::list_codex_sessions_with_mtime(worktree_path)
            .into_iter()
            .filter(|(_, id)| !existing_ids.contains(id))
            .max_by_key(|(modified, _)| *modified)
            .map(|(_, id)| id)
    }

    /// True if the session is ready to receive a queued write. Ready means
    /// either we've hit the hard deadline (deliver anyway), or:
    ///   1. We've passed the earliest-deliver floor, AND
    ///   2. The PTY has been quiet for `require_quiet` (no wakeups in that window).
    fn ready_for_write(session: &Session, pw: &PendingWrite, now: Instant) -> bool {
        pending_write_ready(&session.wakeup_times, pw, now)
    }

    /// Write a PendingWrite's bytes (plus correctly-encoded Enter if submit)
    /// to the session's PTY and log the outcome.
    ///
    /// IMPORTANT: a deliberate gap separates the body write from the Enter
    /// write so the receiving agent sees them as two separate keystroke
    /// events rather than a single paste. Without this, codex treats the
    /// whole sequence (body + \r) as pasted content — literal text including
    /// the \r character — and never submits. The gap is implemented by
    /// queueing the Enter into `ts.pending_enter` and letting the main drain
    /// loop fire it after `fire_at`. We MUST NOT block the UI thread here.
    fn deliver_pending_write(
        ts: &mut TerminalSession,
        pw: &PendingWrite,
        kind: &str,
    ) -> std::io::Result<()> {
        let body = pw.text.trim_end_matches(['\r', '\n']);
        let enter = enter_bytes_for(&ts.session);
        let kitty = enter != b"\r";
        let exited = ts.session.exited;
        let term_mode = *ts.session.term.lock().mode();
        let payload = format_body_for_delivery(body, term_mode);
        let bracketed = payload.len() != body.len();
        let write_result = ts.session.write(&payload);
        // Only queue the trailing Enter once the body has fully landed —
        // otherwise we'd submit a half-written prompt to the agent.
        if write_result.is_ok() && pw.submit {
            ts.pending_enter = Some(PendingEnter {
                fire_at: Instant::now() + ENTER_GAP,
            });
        }
        // Remember the first chunk of the delivered text + delivery time so
        // an unbound workflow session can be correlated to its new sid in
        // ~/.claude/history.jsonl. Only record for workflow sessions that
        // still need binding — and only when the body write actually
        // succeeded in full. A failed/partial body never lands in
        // history.jsonl, so recording it would just leave the detector
        // permanently bypassed for this session.
        if write_result.is_ok()
            && ts.workflow_run_id.is_some()
            && ts.transcript_id.is_none()
        {
            let prefix: String = body.chars().take(120).collect();
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            ts.last_delivery = Some((prefix, now_ms));
        }
        if let Some(run_id) = ts.workflow_run_id.clone() {
            log_tick(
                &run_id,
                &format!(
                    "delivered {}: {} body bytes + submit={} to session '{}' role='{}' exited={} kitty_enter={} bracketed={} write_ok={}",
                    kind,
                    body.len(),
                    pw.submit,
                    ts.label,
                    ts.workflow_role.as_deref().unwrap_or("?"),
                    exited,
                    kitty,
                    bracketed,
                    write_result.is_ok(),
                ),
            );
        }
        write_result
    }

    /// Path to the session manifest file.
    fn manifest_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".cm/tui-sessions.json")
    }

    /// Crash-safe write: stage to a sibling `.tmp`, fsync, then rename.
    /// On Linux, rename is atomic across the same filesystem, so a reader
    /// either sees the old complete file or the new complete file — never
    /// a truncated/partial one. The fsync before rename ensures the new
    /// content has hit disk before the directory entry flips.
    fn atomic_write_manifest(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        let tmp = match path.file_name() {
            Some(name) => {
                let mut s = name.to_os_string();
                s.push(".tmp");
                path.with_file_name(s)
            }
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "manifest path has no file name",
                ));
            }
        };
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// Save session manifest to disk.
    pub(crate) fn save_session_manifest(&self) {
        let mut workspaces: HashMap<String, ManifestWorkspace> = HashMap::new();
        for ws in &self.workspaces {
            let entries: Vec<ManifestEntry> = ws
                .sessions
                .iter()
                .map(|ts| ManifestEntry {
                    uid: ts.uid.clone(),
                    managed_by_uid: ts.managed_by_uid.clone(),
                    generation: ts.generation,
                    label: ts.label.clone(),
                    session_type: ts.session_type.clone(),
                    transcript_id: ts.transcript_id.clone(),
                    hidden: ts.hidden,
                    idle_timeout_secs: ts.idle_timeout_secs,
                    burst_threshold: ts.burst_threshold,
                    workflow_run_id: ts.workflow_run_id.clone(),
                    workflow_role: ts.workflow_role.clone(),
                    task_id: ts.task_id.clone(),
                    notify_on_idle: ts.notify_on_idle,
                    seeded_from_snapshot: ts.seeded_from_snapshot.clone(),
                })
                .collect();
            workspaces.insert(
                ws.id.clone(),
                ManifestWorkspace {
                    id: ws.id.clone(),
                    name: ws.name.clone(),
                    is_closed: ws.is_closed,
                    is_cloud: ws.is_cloud,
                    worktree_path: ws.worktree_path.clone(),
                    main_repo_path: ws.main_repo_path.clone(),
                    repo_url: ws.repo_url.clone(),
                    worker_vm: ws.worker_vm.clone(),
                    worker_zone: ws.worker_zone.clone(),
                    sessions: entries,
                    tombstones: ws.tombstones.clone(),
                },
            );
        }

        let mut bindings: HashMap<String, String> = HashMap::new();
        for task in &self.tasks {
            if let (Some(tid), Some(wsid)) = (&task.task_id, &task.workspace_id) {
                bindings.insert(tid.clone(), wsid.clone());
            }
        }

        let view = match self.sidebar_view {
            SidebarView::Status => "status",
            SidebarView::Task => "task",
        };
        let manifest = Manifest {
            workspaces,
            bindings,
            view: Some(view.to_string()),
        };

        let path = Self::manifest_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&manifest) {
            if let Err(e) = Self::atomic_write_manifest(&path, json.as_bytes()) {
                eprintln!(
                    "failed to write session manifest at {}: {}",
                    path.display(),
                    e
                );
            }
        }
    }

    /// Load session manifest from disk. On parse failure, the corrupt file is
    /// preserved at `<path>.corrupt-<unix_ts>` so the user can recover state.
    fn load_manifest() -> Manifest {
        let path = Self::manifest_path();
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Manifest::default(),
        };
        match serde_json::from_str(&contents) {
            Ok(m) => m,
            Err(e) => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let backup = path.with_extension(format!("json.corrupt-{}", ts));
                let backup_msg = match std::fs::rename(&path, &backup) {
                    Ok(()) => format!("backed up to {}", backup.display()),
                    Err(rename_err) => match std::fs::write(&backup, &contents) {
                        Ok(()) => format!(
                            "rename failed ({}); copied to {}",
                            rename_err,
                            backup.display()
                        ),
                        Err(copy_err) => format!(
                            "could not preserve corrupt file (rename: {}; copy: {})",
                            rename_err, copy_err
                        ),
                    },
                };
                eprintln!(
                    "session manifest at {} failed to parse ({}); {}. Starting with empty state.",
                    path.display(),
                    e,
                    backup_msg
                );
                Manifest::default()
            }
        }
    }

    /// Restore workspaces + sessions from the manifest. Runs after an
    /// initial API tasks fetch so `bindings` can be cross-referenced with
    /// real tasks, but also works standalone (workspaces without any bound
    /// tasks are legal).
    fn restore_sessions(&mut self) {
        let manifest = Self::load_manifest();
        if manifest.workspaces.is_empty() && manifest.bindings.is_empty() {
            return;
        }

        let (cols, rows) = self.last_term_size;

        // Identify worktree paths that are "covered" by a useful workspace —
        // one with sessions or referenced in bindings. We use this to drop
        // orphan-duplicate empty workspaces that accumulated from the pre-fix
        // auto-provision-before-restore bug.
        let bound_ws_ids: HashSet<&String> = manifest.bindings.values().collect();
        let useful_worktree_paths: HashSet<PathBuf> = manifest
            .workspaces
            .values()
            .filter(|w| !w.sessions.is_empty() || bound_ws_ids.contains(&w.id))
            .filter_map(|w| w.worktree_path.clone())
            .collect();

        // Rebuild self.workspaces from the manifest. Closed workspaces are
        // loaded with empty sessions (their PTY state is gone anyway).
        for (_, mw) in manifest.workspaces.iter() {
            let already = self.workspaces.iter().any(|w| w.id == mw.id);
            if already {
                continue;
            }
            // Skip orphan-duplicate: empty, open, not in bindings, and shares a
            // worktree_path with a useful sibling. User-closed workspaces are
            // preserved (is_closed=true) since closing is an explicit action.
            if !mw.is_closed
                && mw.sessions.is_empty()
                && !bound_ws_ids.contains(&mw.id)
                && mw
                    .worktree_path
                    .as_ref()
                    .map_or(false, |p| useful_worktree_paths.contains(p))
            {
                continue;
            }
            // Prune tombstones older than the retention window before
            // copying them into the live workspace. Cheap — these lists
            // stay small in normal use.
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let restored_tombstones: Vec<SessionTombstone> = mw
                .tombstones
                .iter()
                .filter(|t| now_secs - t.exited_at < TOMBSTONE_RETENTION_SECS)
                .cloned()
                .collect();
            let mut ws = Workspace {
                id: mw.id.clone(),
                name: if mw.name.is_empty() {
                    mw.worktree_path
                        .as_ref()
                        .and_then(|p| p.file_name())
                        .and_then(|n| n.to_str())
                        .unwrap_or("workspace")
                        .to_string()
                } else {
                    mw.name.clone()
                },
                is_closed: mw.is_closed,
                is_cloud: mw.is_cloud,
                repo_url: mw.repo_url.clone(),
                worktree_path: mw.worktree_path.clone(),
                main_repo_path: mw.main_repo_path.clone(),
                worker_vm: mw.worker_vm.clone(),
                worker_zone: mw.worker_zone.clone(),
                sessions: vec![],
                tombstones: restored_tombstones,
                is_pushing: false,
            };
            if !ws.is_closed {
                for entry in &mw.sessions {
                    let ts = Self::spawn_restored_session(
                        entry,
                        &ws,
                        (cols, rows),
                        &self.config,
                        &self.memory_cap_status,
                        &self.memory_kill_tx,
                    );
                    if let Some(ts) = ts {
                        ws.sessions.push(ts);
                    }
                }
            }
            self.workspaces.push(ws);
        }

        // Apply task bindings onto any existing TaskEntries (from the API
        // fetch). Tasks that aren't in self.tasks yet (task still backlog
        // or API hasn't come back) will get their workspace_id set later
        // in reconcile_tasks when they arrive.
        for (task_id, ws_id) in &manifest.bindings {
            if let Some(task) = self
                .tasks
                .iter_mut()
                .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
            {
                task.workspace_id = Some(ws_id.clone());
            }
        }

        // If we restored sessions, put cursor on the first workspace with one.
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if !ws.sessions.is_empty() {
                self.cursor = Cursor::Session(wi, 0);
                break;
            }
        }
    }

    /// Spawn a session from a ManifestEntry within a Workspace context.
    /// Extracted so both restore + manual creation paths can share it.
    fn spawn_restored_session(
        entry: &ManifestEntry,
        ws: &Workspace,
        (cols, rows): (u16, u16),
        config: &Config,
        cap_status: &crate::memory_cap::MemoryCapAvailability,
        kill_tx: &std::sync::mpsc::Sender<crate::session_watch::MemoryKillEvent>,
    ) -> Option<TerminalSession> {
        // Resolve the UID ONCE here so the MCP config and the
        // TerminalSession agree. Earlier this had two separate
        // `new_session_uid()` calls — they generated different values
        // for legacy manifests (no `entry.uid`), which made the agent's
        // env-supplied CM_TUI_SESSION_ID never match `ts.uid` and every
        // tool call from a restored session failed `caller_ctx`.
        let restored_uid = if entry.uid.is_empty() {
            new_session_uid()
        } else {
            entry.uid.clone()
        };
        let cloud_vm = ws.worker_vm.as_deref().filter(|s| !s.is_empty());
        let codex_resume_baseline =
            if entry.session_type == "codex" && entry.transcript_id.is_some() {
                ws.worktree_path
                    .as_ref()
                    .map(|p| Self::list_codex_sessions(p))
            } else {
                None
            };
        let result = if cloud_vm.is_some() && entry.session_type == "bash" {
            let vm = cloud_vm.unwrap().to_string();
            let zone = ws
                .worker_zone
                .clone()
                .unwrap_or_else(|| config.gcp_zone.clone());
            let tmux_name = &entry.label;
            let args = vec![
                "compute".to_string(),
                "ssh".to_string(),
                vm,
                format!("--zone={}", zone),
                format!("--project={}", config.gcp_project),
                "--".to_string(),
                "-t".to_string(),
                format!(
                    "TERM=xterm-256color sudo su - worker -c 'cd /workspace && tmux new-session -As {}'",
                    tmux_name
                ),
            ];
            Session::new("gcloud", &args, cols, rows, None, Default::default(), None)
        } else if matches!(entry.session_type.as_str(), "claude" | "codex") {
            let wt = ws.worktree_path.clone();
            let engine = if entry.session_type == "codex" {
                workflow::toml_schema::Engine::Codex
            } else {
                workflow::toml_schema::Engine::ClaudeCode
            };
            // Use the resolved-up-front UID so the MCP env's
            // CM_TUI_SESSION_ID matches what the TerminalSession will hold.
            let session_uid_for_mcp = restored_uid.clone();
            let workflow_meta = match (
                entry.workflow_run_id.as_deref(),
                entry.workflow_role.as_deref(),
            ) {
                (Some(run_id), Some(role)) => {
                    Some(crate::mcp_config::WorkflowMeta { run_id, role })
                }
                _ => None,
            };
            match crate::mcp_config::build_args(
                &engine,
                &session_uid_for_mcp,
                workflow_meta,
                entry.transcript_id.as_deref(),
            ) {
                Ok((program, args)) => crate::session::spawn_agent_session(
                    &entry.session_type,
                    &session_uid_for_mcp,
                    &program,
                    &args,
                    cols,
                    rows,
                    wt,
                    Default::default(),
                    config,
                    cap_status,
                    kill_tx,
                ),
                Err(_) => {
                    // Fallback: spawn without MCP. Agent loses access to
                    // workflow + control-socket tools but the session
                    // still runs.
                    let program = if entry.session_type == "codex" {
                        "codex"
                    } else {
                        "claude"
                    };
                    let mut args: Vec<String> = if entry.session_type == "codex" {
                        vec!["--yolo".into()]
                    } else {
                        vec!["--dangerously-skip-permissions".into()]
                    };
                    if let Some(ref sid) = entry.transcript_id {
                        if entry.session_type == "codex" {
                            args.push("resume".into());
                        } else {
                            args.push("--resume".into());
                        }
                        args.push(sid.clone());
                    }
                    crate::session::spawn_agent_session(
                        &entry.session_type,
                        &session_uid_for_mcp,
                        program,
                        &args,
                        cols,
                        rows,
                        wt,
                        Default::default(),
                        config,
                        cap_status,
                        kill_tx,
                    )
                }
            }
        } else {
            let wt = ws.worktree_path.clone();
            Session::new("/bin/bash", &[], cols, rows, wt, Default::default(), None)
        };
        let s = result.ok()?;
        let pending = if entry.transcript_id.is_some() {
            codex_resume_baseline
        } else if matches!(entry.session_type.as_str(), "claude" | "codex") {
            Some(Vec::new())
        } else {
            None
        };
        // `restored_uid` was computed at the top of this function — same
        // value used in `session_uid_for_mcp` above. Don't generate a
        // fresh one here.
        Some(TerminalSession {
            uid: restored_uid,
            label: entry.label.clone(),
            session_type: entry.session_type.clone(),
            session: s,
            status: SessionStatus::Running,
            last_write_at: None,
            transcript_id: entry.transcript_id.clone(),
            generation: entry.generation,
            pending_jsonl_files: pending,
            hidden: entry.hidden,
            idle_timeout_secs: entry.idle_timeout_secs,
            burst_threshold: entry.burst_threshold,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: entry.workflow_run_id.clone(),
            workflow_role: entry.workflow_role.clone(),
            last_delivery: None,
            task_id: entry.task_id.clone(),
            notify_on_idle: entry.notify_on_idle,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: entry.managed_by_uid.clone(),
            seeded_from_snapshot: entry.seeded_from_snapshot.clone(),
        })
    }

    /// Body of `SubmitAction::SaveSnapshot`. Resolves the focused session's
    /// transcript path via the agent strategy, builds a `SaveSpec`, and
    /// either toasts on success or re-opens the modal with the error
    /// surfaced inline.
    fn handle_save_snapshot_submit(
        &mut self,
        workspace_id: String,
        session_uid: String,
        name: String,
        description: String,
    ) {
        // Reopen the modal carrying `name` / `description` so the user
        // doesn't lose what they typed. Called via the early-return closure
        // pattern below — used for any failure that's recoverable in-form
        // (validation, name conflict, missing transcript). Stable IDs are
        // re-stored so a subsequent reorder still resolves correctly.
        let reopen = |this: &mut Self, err: String| {
            this.input_mode = InputMode::SaveSnapshot {
                workspace_id: workspace_id.clone(),
                session_uid: session_uid.clone(),
                name_text: name.clone(),
                description_text: description.clone(),
                active_field: 0,
                error: Some(err),
            };
        };

        // Resolve stable IDs → current indices. Backend events can reorder
        // workspaces while the modal is open, so the IDs are the source of
        // truth, not the indices that were captured at open time.
        let Some((_wi, _si)) = resolve_session_by_ids(
            &self.workspaces,
            &workspace_id,
            &session_uid,
        ) else {
            self.set_status_msg(
                "Snapshot cancelled — the target session is no longer available",
            );
            return;
        };

        // Pull immutable refs anew now that we have current indices.
        let ws = &self.workspaces[_wi];
        let ts = &ws.sessions[_si];

        let engine = match ts.session_type.as_str() {
            "claude" => Engine::ClaudeCode,
            "codex" => Engine::Codex,
            _ => {
                reopen(
                    self,
                    "Snapshots only supported for Claude Code / Codex sessions"
                        .into(),
                );
                return;
            }
        };

        let Some(source_cwd) = ws.worktree_path.clone() else {
            reopen(self, "Session has no worktree path".into());
            return;
        };

        let source_session_uid = ts.uid.clone();
        let Some(source_transcript_id) = ts.transcript_id.clone() else {
            reopen(
                self,
                "No transcript yet — let the session produce at least one message first"
                    .into(),
            );
            return;
        };

        // Resolve the transcript on disk via the agent strategy (walks
        // ~/.codex/sessions for codex; encoded-cwd join for claude).
        let agent = agent::agent_for(&ts.session_type);
        let ctx = agent::AgentCtx {
            ts,
            worktree_path: &source_cwd,
        };
        let Some(source_transcript_path) = agent.transcript_path(ctx) else {
            reopen(
                self,
                "Could not resolve transcript path for this session".into(),
            );
            return;
        };

        // Claude has a per-cwd memory dir; codex does not.
        let memory_dir = match engine {
            Engine::ClaudeCode => agent_memory::claude_memory_dir(&source_cwd),
            Engine::Codex => None,
        };

        let spec = agent_memory::SaveSpec {
            name: name.as_str(),
            description: description.as_str(),
            engine: engine.clone(),
            source_session_uid: source_session_uid.as_str(),
            source_transcript_id: source_transcript_id.as_str(),
            source_cwd: &source_cwd,
            source_transcript_path: &source_transcript_path,
            source_memory_dir: memory_dir.as_deref(),
        };

        match agent_memory::save(spec) {
            Ok(snap) => {
                self.set_status_msg(&format!("Snapshot saved: {}", snap.name));
            }
            Err(e) => {
                reopen(self, e.to_string());
            }
        }
    }

    /// Open the save-snapshot modal for the focused session. Surface the
    /// design's two upfront error toasts (engine not supported, no
    /// transcript yet) so the modal only opens when a save can plausibly
    /// succeed. See `DESIGN_AGENT_MEMORIES.md` save flow.
    /// Load all saved snapshots and open the catalog modal in browse
    /// mode. Toasts if the on-disk list can't be read; otherwise opens
    /// even when there are zero snapshots (the empty-state render tells
    /// the user how to create one).
    ///
    /// `picker_target` is `None` for the stand-alone A-z catalog. Pass
    /// `Some(target)` when invoking from a parent form so the catalog
    /// can re-open it (with seed_from set or unchanged) on submit /
    /// cancel — including when `list()` fails before the catalog can
    /// even render. Without that restoration, a list() error would
    /// silently drop the user's typed form state.
    fn open_snapshot_catalog(&mut self, picker_target: Option<PickerTarget>) {
        let (mode, status) =
            catalog_open_outcome(agent_memory::list(), picker_target);
        self.input_mode = mode;
        if let Some(msg) = status {
            self.set_status_msg(&msg);
        }
    }

    fn open_save_snapshot(&mut self) {
        let (wi, si) = match self.cursor {
            Cursor::Session(wi, si) => (wi, si),
            _ => {
                self.set_status_msg("Snapshot: focus a session first");
                return;
            }
        };
        let Some(ws) = self.workspaces.get(wi) else {
            return;
        };
        let Some(ts) = ws.sessions.get(si) else {
            return;
        };
        if !matches!(ts.session_type.as_str(), "claude" | "codex") {
            self.set_status_msg(
                "Snapshots only supported for Claude Code / Codex sessions",
            );
            return;
        }
        if ts.transcript_id.is_none() {
            self.set_status_msg(
                "No transcript yet — let the session produce at least one message first",
            );
            return;
        }
        // Capture stable IDs so a backend-driven workspace reorder while
        // the modal is open doesn't make the indices stale.
        self.input_mode = InputMode::SaveSnapshot {
            workspace_id: ws.id.clone(),
            session_uid: ts.uid.clone(),
            name_text: String::new(),
            description_text: String::new(),
            active_field: 0,
            error: None,
        };
    }

    /// Open settings for whatever the cursor is focused on — a workspace
    /// (rename) when on a header, a session (label / idle / hidden) when on
    /// a specific session.
    fn open_session_settings(&mut self) {
        match self.cursor.clone() {
            Cursor::Session(wi, si) => {
                if let Some(ws) = self.workspaces.get(wi) {
                    if let Some(ts) = ws.sessions.get(si) {
                        let timeout = ts.idle_timeout_secs;
                        let burst = ts.burst_threshold;
                        self.input_mode = InputMode::SessionSettings {
                            ws_index: wi,
                            session_index: si,
                            name: ts.label.clone(),
                            idle_timeout: if timeout == 0 {
                                DEFAULT_IDLE_TIMEOUT_SECS.to_string()
                            } else {
                                timeout.to_string()
                            },
                            burst_threshold: if burst == 0 {
                                WAKEUP_BURST_THRESHOLD.to_string()
                            } else {
                                burst.to_string()
                            },
                            hidden: ts.hidden,
                            notify_on_idle: ts.notify_on_idle,
                            seeded_from_snapshot: ts.seeded_from_snapshot.clone(),
                            active_field: 0,
                        };
                    }
                }
            }
            Cursor::Workspace(wi) => {
                if let Some(ws) = self.workspaces.get(wi) {
                    self.input_mode = InputMode::WorkspaceSettings {
                        ws_index: wi,
                        name: ws.name.clone(),
                    };
                }
            }
            Cursor::Task { task_id, .. } => {
                let current_name = self
                    .tasks
                    .iter()
                    .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
                    .map(|t| t.name.clone())
                    .unwrap_or_default();
                self.input_mode = InputMode::TaskSettings {
                    task_id,
                    name: current_name,
                };
            }
        }
    }

    /// Build the candidate list for the A-O picker and open it. Shows a
    /// status message when no past workspaces exist instead of an empty
    /// modal. Most-recent (latest tombstone) first.
    fn open_past_workspace_picker(&mut self) {
        let mut candidates: Vec<PastCandidate> = self
            .workspaces
            .iter()
            .enumerate()
            .filter(|(wi, _)| self.is_past_workspace(*wi))
            .map(|(_, ws)| {
                let last_exited_at = ws
                    .tombstones
                    .iter()
                    .map(|t| t.exited_at)
                    .fold(0.0f64, f64::max);
                let worktree_exists = ws
                    .worktree_path
                    .as_ref()
                    .map_or(false, |p| p.exists());
                PastCandidate {
                    ws_id: ws.id.clone(),
                    display: ws.name.clone(),
                    worktree_path: ws.worktree_path.clone(),
                    worktree_exists,
                    last_exited_at,
                }
            })
            .collect();
        if candidates.is_empty() {
            self.set_status_msg("No past workspaces");
            return;
        }
        candidates.sort_by(|a, b| {
            b.last_exited_at
                .partial_cmp(&a.last_exited_at)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.input_mode = InputMode::PastWorkspacePicker {
            candidates,
            selected: 0,
        };
    }

    /// Reopen a past workspace by id: flip `is_closed` back to false and
    /// PATCH any bound done tasks back to `running` so the workspace
    /// re-enters the active sidebar. Refuses gracefully when the worktree
    /// directory is gone (manually deleted or `git worktree remove`'d).
    /// Returns true on success — callers in modal mode use it to close
    /// the picker only when the reopen actually went through.
    fn reopen_workspace_by_id(&mut self, ws_id: &str) -> bool {
        let Some(wi) = self.workspaces.iter().position(|w| w.id == ws_id) else {
            self.set_status_msg("Workspace no longer in manifest");
            return false;
        };
        let worktree_path = self.workspaces[wi].worktree_path.clone();
        match worktree_path.as_deref() {
            Some(p) if p.exists() => {}
            Some(p) => {
                self.set_status_msg(&format!(
                    "Worktree gone: {} — can't reopen",
                    p.display()
                ));
                return false;
            }
            None => {
                self.set_status_msg("Workspace has no worktree to reopen");
                return false;
            }
        }

        self.workspaces[wi].is_closed = false;

        let bound_done: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(ws_id))
            .filter(|t| matches!(t.api_status, TaskStatus::Done))
            .filter_map(|t| t.task_id.clone())
            .collect();
        for tid in &bound_done {
            let mut fields = HashMap::new();
            fields.insert(
                "status".to_string(),
                serde_json::Value::String("running".to_string()),
            );
            self.backend.update_task(tid.clone(), fields);
        }
        for tid in &bound_done {
            if let Some(entry) = self
                .tasks
                .iter_mut()
                .find(|t| t.task_id.as_deref() == Some(tid.as_str()))
            {
                entry.api_status = TaskStatus::Running;
            }
            self.planning.mark_task_running_by_id(tid);
        }

        self.save_session_manifest();
        self.cursor = Cursor::Workspace(wi);
        self.clamp_cursor();
        self.set_status_msg("Workspace reopened — A-s to add session");
        true
    }

    /// Soft-close the workspace under the cursor: kill its session PTYs
    /// and hide from the sidebar. Worktree stays on disk; bindings persist.
    /// Each closed session leaves behind a `SessionTombstone` so the
    /// resolver can still answer `read_session_output` for it.
    fn close_active_workspace(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        // Tombstone every session before dropping. Helper persists the
        // manifest as a side-effect so a TUI crash mid-close doesn't
        // resurrect tombstoned sessions.
        self.tombstone_and_remove(wi, |_| true);
        if let Some(ws) = self.workspaces.get_mut(wi) {
            ws.is_closed = true;
        }
        // Persist again in case `is_closed` flipped after the helper
        // already saved (cheap, just rewrites the same JSON).
        self.save_session_manifest();
        if let Some((nwi, _)) = self
            .workspaces
            .iter()
            .enumerate()
            .find(|(_, w)| !w.is_closed)
        {
            self.cursor = Cursor::Workspace(nwi);
        }
        self.clamp_cursor();
        self.set_status_msg("Workspace closed");
    }

    fn toggle_session_hidden(&mut self) {
        let (wi, si) = match &self.cursor {
            Cursor::Session(wi, si) => (*wi, *si),
            Cursor::Workspace(wi) => {
                let wi = *wi;
                if self.workspaces.get(wi).map_or(false, |w| w.sessions.len() == 1) {
                    (wi, 0)
                } else {
                    return;
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                // Toggle hidden on every session belonging to the task.
                // Uses the majority-hidden state as the "current" so one
                // keypress always flips everything in unison.
                let wi = *ws_idx;
                let tid = task_id.clone();
                let Some(ws) = self.workspaces.get_mut(wi) else {
                    return;
                };
                let matching: Vec<&mut TerminalSession> = ws
                    .sessions
                    .iter_mut()
                    .filter(|ts| ts.task_id.as_deref() == Some(tid.as_str()))
                    .collect();
                if matching.is_empty() {
                    return;
                }
                let hidden_count = matching.iter().filter(|ts| ts.hidden).count();
                let new_hidden = hidden_count * 2 < matching.len();
                for ts in matching {
                    ts.hidden = new_hidden;
                }
                self.save_session_manifest();
                self.needs_redraw = true;
                return;
            }
        };
        if let Some(ts) = self
            .workspaces
            .get_mut(wi)
            .and_then(|w| w.sessions.get_mut(si))
        {
            ts.hidden = !ts.hidden;
            self.save_session_manifest();
            self.needs_redraw = true;
        }
    }

    // ── Cursor helpers ──────────────────────────────────────────────

    /// Return the workspace index the cursor is currently on.
    fn active_workspace_index(&self) -> Option<usize> {
        if self.workspaces.is_empty() {
            return None;
        }
        let wi = match &self.cursor {
            Cursor::Workspace(wi) => *wi,
            Cursor::Task { ws_idx, .. } => *ws_idx,
            Cursor::Session(wi, _) => *wi,
        };
        (wi < self.workspaces.len()).then_some(wi)
    }

    /// Return the task_id for the cursor's current task scope, if any:
    ///   - Cursor::Task → the task_id on the cursor
    ///   - Cursor::Session → the session's task_id (may be None)
    ///   - Cursor::Workspace → None (not task-scoped, even if one task is bound)
    fn cursor_task_id(&self) -> Option<String> {
        match &self.cursor {
            Cursor::Task { task_id, .. } => Some(task_id.clone()),
            Cursor::Session(wi, si) => self
                .workspaces
                .get(*wi)
                .and_then(|w| w.sessions.get(*si))
                .and_then(|ts| ts.task_id.clone()),
            Cursor::Workspace(_) => None,
        }
    }

    /// Return a reference to the active terminal session (workspace + session).
    fn active_session(&self) -> Option<(&Workspace, &TerminalSession)> {
        match &self.cursor {
            Cursor::Session(wi, si) => {
                let ws = self.workspaces.get(*wi)?;
                let ts = ws.sessions.get(*si)?;
                Some((ws, ts))
            }
            Cursor::Workspace(wi) => {
                let ws = self.workspaces.get(*wi)?;
                if ws.sessions.len() == 1 {
                    Some((ws, &ws.sessions[0]))
                } else {
                    None
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                let ws = self.workspaces.get(*ws_idx)?;
                let matches: Vec<&TerminalSession> = ws
                    .sessions
                    .iter()
                    .filter(|ts| ts.task_id.as_deref() == Some(task_id.as_str()))
                    .collect();
                if matches.len() == 1 {
                    Some((ws, matches[0]))
                } else {
                    None
                }
            }
        }
    }

    /// Return a mutable reference to the active terminal session.
    fn active_session_mut(&mut self) -> Option<&mut TerminalSession> {
        match &self.cursor {
            Cursor::Session(wi, si) => {
                let ws = self.workspaces.get_mut(*wi)?;
                ws.sessions.get_mut(*si)
            }
            Cursor::Workspace(wi) => {
                let ws = self.workspaces.get_mut(*wi)?;
                if ws.sessions.len() == 1 {
                    Some(&mut ws.sessions[0])
                } else {
                    None
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                let task_id = task_id.clone();
                let ws = self.workspaces.get_mut(*ws_idx)?;
                let mut found_idx = None;
                let mut count = 0;
                for (i, ts) in ws.sessions.iter().enumerate() {
                    if ts.task_id.as_deref() == Some(task_id.as_str()) {
                        count += 1;
                        if count == 1 {
                            found_idx = Some(i);
                        } else {
                            return None;
                        }
                    }
                }
                ws.sessions.get_mut(found_idx?)
            }
        }
    }

    // ── Workspace / task lookup helpers ─────────────────────────────

    fn workspace_index_by_id(&self, id: &str) -> Option<usize> {
        self.workspaces.iter().position(|w| w.id == id)
    }
}

/// Resolve a `(workspace_id, session_uid)` pair to current `(ws_index,
/// session_index)`. Returns `None` if either has been removed since the
/// IDs were captured. Free function so it can be unit-tested against a
/// hand-rolled `&[Workspace]` without building a full `App`.
fn resolve_session_by_ids(
    workspaces: &[Workspace],
    workspace_id: &str,
    session_uid: &str,
) -> Option<(usize, usize)> {
    let wi = workspaces.iter().position(|w| w.id == workspace_id)?;
    let si = workspaces[wi]
        .sessions
        .iter()
        .position(|s| s.uid == session_uid)?;
    Some((wi, si))
}

impl App {

    /// First task bound to the given workspace, if any. Used by push/pull
    /// (which need *a* representative task) and the detail panel (shows one
    /// prompt). Multi-task workspaces have no canonical ordering; first-
    /// insertion-wins.
    fn first_task_for_ws(&self, ws_id: &str) -> Option<&TaskEntry> {
        self.tasks
            .iter()
            .find(|t| t.workspace_id.as_deref() == Some(ws_id))
    }

    /// A workspace is "past" if it's been put away but its worktree may still
    /// be on disk. Cloud workspaces never qualify — there's no worktree to
    /// reopen and the VM may be gone. Local workspaces qualify when either:
    ///   - `is_closed = true` (explicit A-W close), or
    ///   - they have no live sessions AND every bound task is done.
    /// An unbound, sessionless, open workspace is NOT past — it's a fresh
    /// workspace waiting for sessions.
    fn is_past_workspace(&self, wi: usize) -> bool {
        let Some(ws) = self.workspaces.get(wi) else {
            return false;
        };
        if ws.is_cloud {
            return false;
        }
        if ws.is_closed {
            return true;
        }
        if !ws.sessions.is_empty() {
            return false;
        }
        let bound: Vec<&TaskEntry> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(&ws.id))
            .collect();
        !bound.is_empty()
            && bound
                .iter()
                .all(|t| matches!(t.api_status, TaskStatus::Done))
    }

    /// Compute effective task status: derived from the workspace's sessions
    /// if bound, otherwise falls back to api_status.
    fn task_status(&self, task: &TaskEntry) -> TaskStatus {
        if let Some(ws) = task
            .workspace_id
            .as_deref()
            .and_then(|id| self.workspaces.iter().find(|w| w.id == id))
        {
            if ws.sessions.iter().any(|s| s.status == SessionStatus::Running) {
                return TaskStatus::Running;
            }
            if ws.sessions.iter().any(|s| s.status == SessionStatus::Idle) {
                return TaskStatus::Blocked;
            }
            if ws.worker_vm.as_deref().is_some_and(|s| !s.is_empty()) {
                return task.api_status.clone();
            }
        }
        task.api_status.clone()
    }

    /// Clamp cursor so it points to a valid item.
    fn clamp_cursor(&mut self) {
        if self.workspaces.is_empty() {
            self.cursor = Cursor::Workspace(0);
            return;
        }
        let max = self.workspaces.len() - 1;
        match &self.cursor {
            Cursor::Workspace(wi) => {
                if *wi > max {
                    self.cursor = Cursor::Workspace(max);
                }
            }
            Cursor::Session(wi, si) => {
                let wi = *wi;
                let si = *si;
                if wi > max {
                    self.cursor = Cursor::Workspace(max);
                } else if self.workspaces[wi].sessions.is_empty() {
                    self.cursor = Cursor::Workspace(wi);
                } else if si >= self.workspaces[wi].sessions.len() {
                    self.cursor =
                        Cursor::Session(wi, self.workspaces[wi].sessions.len() - 1);
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                let wi = *ws_idx;
                let tid = task_id.clone();
                if wi > max {
                    self.cursor = Cursor::Workspace(max);
                } else if !self
                    .tasks
                    .iter()
                    .any(|t| t.task_id.as_deref() == Some(tid.as_str()))
                {
                    // Task disappeared — fall back to workspace.
                    self.cursor = Cursor::Workspace(wi);
                }
            }
        }
    }

    /// Build visual items for the current sidebar view.
    fn visual_items(&self) -> Vec<VisualItem> {
        match self.sidebar_view {
            SidebarView::Status => self.visual_items_status(),
            SidebarView::Task => self.visual_items_task(),
        }
    }

    /// Status view: flat list of sessions grouped by status.
    /// Running sessions first, then idle, then workspaces with no sessions.
    /// Past workspaces (closed / all-tasks-done) are hidden — open the
    /// A-O picker to reach them.
    fn visual_items_status(&self) -> Vec<VisualItem> {
        let mut running: Vec<VisualItem> = Vec::new();
        let mut idle: Vec<VisualItem> = Vec::new();
        let mut no_session: Vec<VisualItem> = Vec::new();

        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed || self.is_past_workspace(wi) {
                continue;
            }
            if ws.sessions.is_empty() {
                no_session.push(VisualItem::WorkspaceHeader(wi));
            } else {
                for (si, ts) in ws.sessions.iter().enumerate() {
                    let item = VisualItem::Session(wi, si);
                    match ts.status {
                        SessionStatus::Running => running.push(item),
                        SessionStatus::Idle => idle.push(item),
                    }
                }
            }
        }

        let mut items = Vec::new();
        items.extend(running);
        if !items.is_empty() && (!idle.is_empty() || !no_session.is_empty()) {
            items.push(VisualItem::Separator);
        }
        items.extend(idle);
        if !items.is_empty() && !no_session.is_empty() {
            if !matches!(items.last(), Some(VisualItem::Separator)) {
                items.push(VisualItem::Separator);
            }
        }
        items.extend(no_session);
        items
    }

    /// Task view: workspace headers with sessions indented underneath.
    /// Sessions grouped by workflow run appear contiguously under a workflow
    /// subheader. Standalone sessions render first; each workflow group
    /// follows. Past workspaces are hidden — reachable via the A-O picker.
    fn visual_items_task(&self) -> Vec<VisualItem> {
        let mut items = Vec::new();
        let mut first = true;
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed || self.is_past_workspace(wi) {
                continue;
            }
            if !first {
                items.push(VisualItem::Separator);
            }
            first = false;
            items.push(VisualItem::WorkspaceHeader(wi));

            // Partition sessions by task_id bucket. Unbound sessions live in
            // the `None` bucket and render at workspace level (no subheader).
            let mut by_task: std::collections::BTreeMap<Option<String>, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (si, ts) in ws.sessions.iter().enumerate() {
                by_task.entry(ts.task_id.clone()).or_default().push(si);
            }

            // Render buckets: unbound first, then task buckets in binding order.
            let task_order: Vec<Option<String>> = {
                let mut ordered: Vec<Option<String>> = Vec::new();
                if by_task.contains_key(&None) {
                    ordered.push(None);
                }
                // Tasks bound to this workspace, in insertion order of self.tasks.
                for task in &self.tasks {
                    if task.workspace_id.as_deref() != Some(ws.id.as_str()) {
                        continue;
                    }
                    let Some(tid) = task.task_id.as_deref() else { continue };
                    let key = Some(tid.to_string());
                    if by_task.contains_key(&key) && !ordered.contains(&key) {
                        ordered.push(key);
                    }
                }
                // Catch any orphaned task_ids tagged on sessions but not in
                // self.tasks (stale API state) — render them too so they don't
                // silently disappear.
                let mut tail: Vec<Option<String>> = by_task
                    .keys()
                    .filter(|k| k.is_some() && !ordered.contains(k))
                    .cloned()
                    .collect();
                tail.sort();
                ordered.extend(tail);
                ordered
            };

            for bucket_key in task_order {
                // Emit task subheader for task-scoped buckets.
                if let Some(tid) = bucket_key.as_deref() {
                    items.push(VisualItem::TaskHeader {
                        ws_idx: wi,
                        task_id: tid.to_string(),
                    });
                }
                let indices = by_task.remove(&bucket_key).unwrap_or_default();

                // Split each bucket into standalone + workflow groups.
                let mut standalone: Vec<usize> = Vec::new();
                let mut groups: std::collections::BTreeMap<String, Vec<usize>> =
                    std::collections::BTreeMap::new();
                for si in &indices {
                    match &ws.sessions[*si].workflow_run_id {
                        Some(run_id) => groups.entry(run_id.clone()).or_default().push(*si),
                        None => standalone.push(*si),
                    }
                }
                // Standalone: running first, then idle.
                let (run_, other): (Vec<_>, Vec<_>) = standalone
                    .into_iter()
                    .partition(|si| ws.sessions[*si].status == SessionStatus::Running);
                for si in run_ {
                    items.push(VisualItem::Session(wi, si));
                }
                for si in other {
                    items.push(VisualItem::Session(wi, si));
                }
                // Workflow groups.
                for (run_id, session_indices) in groups {
                    let is_active_run =
                        self.workflow_runs.iter().any(|r| r.run_id == run_id);
                    if !is_active_run {
                        for si in session_indices {
                            items.push(VisualItem::Session(wi, si));
                        }
                        continue;
                    }
                    items.push(VisualItem::WorkflowHeader {
                        ws_idx: wi,
                        run_id: run_id.clone(),
                    });
                    let role_order: Vec<String> = self
                        .workflow_runs
                        .iter()
                        .find(|r| r.run_id == run_id)
                        .and_then(|r| self.workflows.get(&r.workflow_name))
                        .map(|wf| wf.role_order.clone())
                        .unwrap_or_default();
                    let mut ordered = session_indices.clone();
                    ordered.sort_by_key(|si| {
                        let role = ws.sessions[*si].workflow_role.as_deref().unwrap_or("");
                        role_order.iter().position(|r| r == role).unwrap_or(usize::MAX)
                    });
                    for si in ordered {
                        items.push(VisualItem::Session(wi, si));
                    }
                }
            }
        }
        items
    }

    /// Navigate the cursor up or down. +1 = down, -1 = up.
    /// Skips non-selectable items (Separators, headers with sessions).
    fn navigate(&mut self, direction: i32) {
        let items = self.visual_items();
        if items.is_empty() {
            return;
        }

        // Workspace headers are selectable only when the workspace has no
        // sessions (otherwise the cursor lives on a child session).
        // Task headers are always selectable — they support A-d / A-x / A-e
        // etc. even when they have sessions underneath.
        let is_selectable = |item: &VisualItem| match item {
            VisualItem::Session(_, _) => true,
            VisualItem::WorkspaceHeader(wi) => self
                .workspaces
                .get(*wi)
                .map_or(false, |w| w.sessions.is_empty()),
            VisualItem::TaskHeader { .. } => true,
            VisualItem::Separator => false,
            VisualItem::WorkflowHeader { .. } => false,
        };

        if !items.iter().any(is_selectable) {
            return;
        }

        let cur_pos = items
            .iter()
            .position(|item| match (&self.cursor, item) {
                (Cursor::Workspace(wi), VisualItem::WorkspaceHeader(vwi)) => wi == vwi,
                (Cursor::Session(wi, si), VisualItem::Session(vwi, vsi)) => {
                    wi == vwi && si == vsi
                }
                (
                    Cursor::Task { ws_idx, task_id },
                    VisualItem::TaskHeader { ws_idx: vwi, task_id: vtid },
                ) => ws_idx == vwi && task_id == vtid,
                _ => false,
            })
            .unwrap_or(0);

        let len = items.len() as i32;
        let mut next = cur_pos as i32;
        for _ in 0..items.len() {
            next = (next + direction).rem_euclid(len);
            if is_selectable(&items[next as usize]) {
                break;
            }
        }

        match &items[next as usize] {
            VisualItem::Session(wi, si) => self.cursor = Cursor::Session(*wi, *si),
            VisualItem::WorkspaceHeader(wi) => self.cursor = Cursor::Workspace(*wi),
            VisualItem::TaskHeader { ws_idx, task_id } => {
                self.cursor = Cursor::Task {
                    ws_idx: *ws_idx,
                    task_id: task_id.clone(),
                };
            }
            _ => {}
        }
    }

    // ── Event processing ────────────────────────────────────────────

    /// Process all pending terminal events (non-blocking).
    pub fn drain_terminal_events(&mut self) {
        let now = Instant::now();
        let should_check_session_ids =
            now.duration_since(self.last_session_id_check) >= SESSION_ID_CHECK_INTERVAL;

        let mut had_event = false;
        struct DetectedSid {
            ws_id: String,
            sid: String,
            workflow: Option<(String, String)>,
            old_sid: Option<String>,
        }
        let mut sid_detections: Vec<DetectedSid> = Vec::new();
        let mut manifest_needs_save = false;
        // Sids already bound to some live session in this TUI. The detector
        // must exclude these so two sessions sharing a worktree (e.g. a
        // workflow reviewer + a regular codex pane) can't both pick the
        // same newly-written transcript file. Updated as the loop binds new
        // sids so later iterations see them too.
        let mut bound_sids: std::collections::HashSet<String> = self
            .workspaces
            .iter()
            .flat_map(|w| w.sessions.iter())
            .filter_map(|s| s.transcript_id.clone())
            .collect();
        // Status-bar notes for write failures encountered during this drain.
        // Collected here and applied after the loop because we cannot borrow
        // `&mut self.status_msg` while iterating `&mut self.workspaces`.
        let mut write_failure_notes: Vec<String> = Vec::new();
        for ws in &mut self.workspaces {
            for ts in &mut ws.sessions {
                while let Ok(event) = ts.session.event_rx.try_recv() {
                    had_event = true;
                    match event {
                        TermEvent::Exit | TermEvent::ChildExit(_) => {
                            ts.session.exited = true;
                        }
                        TermEvent::Title(title) => {
                            ts.session.title = title;
                        }
                        TermEvent::Wakeup => {
                            ts.session.wakeup_times.push(now);
                        }
                        TermEvent::ClipboardStore(_, text) => {
                            // Forward OSC 52 clipboard store to the outer terminal.
                            use base64::Engine;
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&text);
                            let osc = format!("\x1b]52;c;{}\x07", b64);
                            let _ = std::io::Write::write_all(
                                &mut std::io::stdout(),
                                osc.as_bytes(),
                            );
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        }
                        TermEvent::ClipboardLoad(_, formatter) => {
                            // Read clipboard via OSC 52 is unreliable; try xclip/xsel.
                            if let Ok(output) = std::process::Command::new("xclip")
                                .args(["-selection", "clipboard", "-o"])
                                .output()
                            {
                                if output.status.success() {
                                    let text = String::from_utf8_lossy(&output.stdout);
                                    let response = formatter(&text);
                                    let _ = ts.session.write(response.as_bytes());
                                }
                            }
                        }
                        _ => {}
                    }
                }

                // Two windows: a short one for detecting activity bursts (idle→running),
                // and the per-session timeout for detecting quiet (running→idle).
                let activity_window = Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS as u64);
                let idle_secs = if ts.idle_timeout_secs > 0 {
                    ts.idle_timeout_secs as u64
                } else {
                    DEFAULT_IDLE_TIMEOUT_SECS as u64
                };
                let idle_window = Duration::from_secs(idle_secs);

                // Prune old wakeups outside the longer window.
                ts.session
                    .wakeup_times
                    .retain(|t| now.duration_since(*t) < idle_window);

                // Detect idle/active for sessions with a local terminal.
                // Freeze while user is typing to avoid flicker from echo.
                if !ts.session.exited {
                    let user_typing = ts
                        .last_write_at
                        .map_or(false, |t| now.duration_since(t) < activity_window);
                    if !user_typing {
                        // Burst = recent wakeups in the short activity window → mark running.
                        let recent_count = ts.session.wakeup_times.iter()
                            .filter(|t| now.duration_since(**t) < activity_window)
                            .count();
                        let burst_threshold = if ts.burst_threshold > 0 {
                            ts.burst_threshold as usize
                        } else {
                            WAKEUP_BURST_THRESHOLD
                        };
                        let burst = recent_count >= burst_threshold;
                        // Quiet = no wakeups at all in the full idle window → mark idle.
                        let quiet = ts.session.wakeup_times.is_empty();
                        if quiet && ts.status == SessionStatus::Running {
                            ts.status = SessionStatus::Idle;
                            if ts.notify_on_idle {
                                notify_session_idle(&ts.label);
                            }
                        } else if burst && ts.status != SessionStatus::Running {
                            ts.status = SessionStatus::Running;
                        }
                    }
                }

                // Deliver queued `/clear` first once the PTY is quiet (or
                // the hard deadline hits). Sequenced before pending_prompt so
                // the prompt always lands AFTER /clear has been processed.
                //
                // On write failure, the pending slot has already been taken,
                // so we don't retry the same payload — preventing an infinite
                // loop against a wedged PTY. We surface the timeout to the
                // status bar so the user knows to investigate.
                if let Some(clear) = &ts.pending_clear {
                    if Self::ready_for_write(&ts.session, clear, now) {
                        let pw = ts.pending_clear.take().unwrap();
                        if let Err(e) = Self::deliver_pending_write(ts, &pw, "pending_clear") {
                            // A partial /clear in the PTY buffer can't be
                            // recovered: any follow-up prompt would land on
                            // top of the truncated slash-command and produce
                            // malformed input. Tear down the whole queued
                            // sequence so the user can re-issue it cleanly.
                            ts.pending_prompt = None;
                            ts.pending_enter = None;
                            write_failure_notes.push(format!(
                                "write to {}: {}",
                                ts.label, e
                            ));
                        }
                    }
                }

                // Only deliver the prompt once the /clear (if any) is gone
                // AND its trailing Enter has fired. Without the pending_enter
                // gate, the prompt's deliver_pending_write call would overwrite
                // ts.pending_enter before the clear's Enter ever fires —
                // codex then sees `/clearCan you review unstaged changes...\r`
                // as a single slash command and rejects it.
                if ts.pending_clear.is_none() && ts.pending_enter.is_none() {
                    if let Some(prompt) = &ts.pending_prompt {
                        if Self::ready_for_write(&ts.session, prompt, now) {
                            let pw = ts.pending_prompt.take().unwrap();
                            if let Err(e) =
                                Self::deliver_pending_write(ts, &pw, "pending_prompt")
                            {
                                write_failure_notes.push(format!(
                                    "write to {}: {}",
                                    ts.label, e
                                ));
                            }
                        }
                    }
                }

                // Fire any deferred Enter that's reached its `fire_at`. The
                // body of the prompt has already gone to the PTY; this writes
                // just the Enter keystroke separately so codex doesn't classify
                // it as paste tail. See `deliver_pending_write` for context.
                //
                // Encoding is recomputed here (not snapshotted at body-write
                // time) because the agent often flips to Kitty keyboard mode
                // during the gap; the Enter keystroke must match the mode in
                // effect right now or the agent treats it as a literal `\r`.
                if let Some(pe) = &ts.pending_enter {
                    if now >= pe.fire_at {
                        ts.pending_enter = None;
                        let enter = enter_bytes_for(&ts.session);
                        let mode_label = if enter == b"\r" { "raw" } else { "kitty" };
                        if let Err(e) = ts.session.write(enter) {
                            // Without a successful Enter the agent never
                            // submits the body, so nothing will land in
                            // history.jsonl for the correlator to match.
                            // Clear `last_delivery` so the listing-based
                            // detector isn't permanently bypassed for this
                            // session.
                            ts.last_delivery = None;
                            // If a `pending_prompt` is still queued at this
                            // point, this Enter belonged to a `/clear` body
                            // (the prompt only fires after pending_enter
                            // clears). The /clear didn't submit, so a prompt
                            // landing on top would concatenate with the
                            // half-applied slash-command — drop it.
                            ts.pending_prompt = None;
                            write_failure_notes.push(format!(
                                "enter to {}: {}",
                                ts.label, e
                            ));
                        }
                        if let Some(run_id) = ts.workflow_run_id.clone() {
                            log_tick(
                                &run_id,
                                &format!(
                                    "enter_fired mode={} session='{}' role='{}'",
                                    mode_label,
                                    ts.label,
                                    ts.workflow_role.as_deref().unwrap_or("?"),
                                ),
                            );
                        }
                    }
                }

                // Session-id detection is run in a separate ordered pass
                // after this loop (see below) so older sessions get first
                // pick when two sessions in the same worktree race for the
                // same newly-written transcript file.

            }
        }

        // Session-id detection: oldest-first, so when two sessions in the
        // same worktree race for the same newly-written transcript, the one
        // that's been waiting longer (and is therefore more likely to
        // actually own the file) wins. `bound_sids` is also extended as we
        // go so once a sid is taken, no other session can claim it on this
        // tick.
        //
        // Skip claude WORKFLOW sessions when there's a pending delivery —
        // those rely on history.jsonl correlation via
        // `resolve_pending_deliveries`, which is more reliable than the
        // listing heuristic when /clear rotations or other claude processes
        // are racing the project directory. After a workflow-launch respawn
        // (Existing claude slot) we don't deliver an activation prompt, so
        // there's nothing for the correlator to match — fall back to the
        // listing detector in that case.
        if should_check_session_ids {
            let mut detection_order: Vec<(usize, usize, Instant)> = Vec::new();
            for (wi, ws) in self.workspaces.iter().enumerate() {
                for (si, ts) in ws.sessions.iter().enumerate() {
                    let skip_workflow_claude = ts.session_type == "claude"
                        && ts.workflow_run_id.is_some()
                        && ts.last_delivery.is_some();
                    if skip_workflow_claude {
                        continue;
                    }
                    if !matches!(ts.session_type.as_str(), "claude" | "codex") {
                        continue;
                    }
                    let pending_detection = ts.pending_jsonl_files.is_some();
                    let initial_bind = ts.transcript_id.is_none() && pending_detection;
                    let codex_resume_rebind = ts.session_type == "codex"
                        && ts.transcript_id.is_some()
                        && pending_detection
                        && now.duration_since(ts.created_at) <= CODEX_RESUME_REBIND_WINDOW;
                    if !(initial_bind || codex_resume_rebind) {
                        continue;
                    }
                    detection_order.push((wi, si, ts.created_at));
                }
            }
            detection_order.sort_by_key(|t| t.2);
            for (wi, si, _) in detection_order {
                let Some(ws) = self.workspaces.get(wi) else { continue };
                let ws_id_here = ws.id.clone();
                let Some(wt) = ws.worktree_path.clone() else { continue };
                let Some(ts) = self.workspaces[wi].sessions.get_mut(si) else { continue };
                let mut existing: Vec<String> =
                    ts.pending_jsonl_files.as_ref().cloned().unwrap_or_default();
                existing.extend(bound_sids.iter().cloned());
                let sid = if ts.session_type == "codex" {
                    Self::detect_codex_session_id(&wt, &existing)
                } else {
                    Self::detect_session_id(&wt, &existing)
                };
                if let Some(sid) = sid {
                    let old_sid = ts.transcript_id.clone();
                    if old_sid.is_some() {
                        ts.rebind_transcript(Some(sid.clone()));
                    } else {
                        ts.transcript_id = Some(sid.clone());
                    }
                    ts.pending_jsonl_files = None;
                    let workflow = match (ts.workflow_run_id.clone(), ts.workflow_role.clone()) {
                        (Some(run_id), Some(role)) => Some((run_id, role)),
                        _ => None,
                    };
                    bound_sids.insert(sid.clone());
                    sid_detections.push(DetectedSid {
                        ws_id: ws_id_here,
                        sid,
                        workflow,
                        old_sid,
                    });
                    manifest_needs_save = true;
                }
            }
        }

        // Sync any newly detected session_ids to the DB. Resolve each ws_id
        // to bound tasks and push an update per bound task.
        for detected in &sid_detections {
            for task in &self.tasks {
                if task.workspace_id.as_deref() != Some(&detected.ws_id) {
                    continue;
                }
                let Some(task_id) = task.task_id.clone() else {
                    continue;
                };
                let mut fields = HashMap::new();
                fields.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(detected.sid.clone()),
                );
                self.backend.update_task(task_id, fields);
            }
            if let Some((run_id, role)) = &detected.workflow {
                if note_workflow_transcript_binding(
                    &mut self.workflow_runs,
                    run_id,
                    role,
                    detected.old_sid.as_deref(),
                    &detected.sid,
                ) {
                    if let Some(run) = self.workflow_runs.iter().find(|r| &r.run_id == run_id) {
                        let _ = workflow::run::save(run);
                    }
                    if let Some(old_sid) = detected.old_sid.as_deref() {
                        log_tick(
                            run_id,
                            &format!(
                                "codex-resume-rebind: role={} {} -> {}",
                                role, old_sid, detected.sid
                            ),
                        );
                    }
                }
            }
        }
        if manifest_needs_save {
            self.save_session_manifest();
        }

        if should_check_session_ids {
            self.last_session_id_check = now;
        }
        if had_event {
            self.needs_redraw = true;
        }

        // Surface any write timeouts collected during the per-session loop.
        // Last note wins (status_msg holds a single string), which is fine —
        // a stalled PTY tends to fail repeatedly and the user just needs to
        // see *something*, not every individual failure.
        if let Some(note) = write_failure_notes.into_iter().next_back() {
            self.set_status_msg(&note);
        }

        // Poll `~/.claude/history.jsonl` for `/clear` and `/compact` events
        // targeting any active workflow role's bound session, and migrate
        // to the new transcript file.
        self.apply_history_rotations();

        // Drive workflow transitions after per-session bookkeeping — this way
        // any session state changes above (idle detection, new session_id) are
        // visible to the workflow engine.
        self.tick_workflows();
    }

    /// Drain new entries from `~/.claude/history.jsonl`. For each rotation-
    /// trigger entry (`/clear`, `/compact`) whose `sessionId` matches the
    /// bound sid of an active claude workflow role, find the new transcript
    /// file that was produced and rebind the role to it.
    fn apply_history_rotations(&mut self) {
        // Drain new history.jsonl entries. Route rotation triggers to the
        // pending queue, and feed every entry to the sid-correlation step
        // for claude workflow sessions that haven't been bound yet.
        let mut new_entries: Vec<workflow::history::HistoryEntry> = Vec::new();
        if let Some(watcher) = self.history_watcher.as_mut() {
            new_entries = watcher.poll();
            let now = Instant::now();
            for entry in &new_entries {
                if workflow::history::is_rotation_trigger(&entry.display) {
                    self.pending_rotations
                        .push((entry.session_id.clone(), entry.timestamp_ms, now));
                }
            }
        }
        self.resolve_pending_deliveries(&new_entries);
        if self.pending_rotations.is_empty() {
            return;
        }
        // Build (sid → binding) lookup for every claude session — both
        // workflow participants AND regular `A-n` / planning panes. The
        // pre-fix version only included workflow roles, which meant a
        // regular pane running `/clear` or `/compact` kept resolving to
        // the *old* transcript file forever and `read_session_output`
        // returned stale data on what looked like a healthy session.
        let bindings = collect_rotation_bindings(&self.workspaces);
        // Walk pending queue; resolve what we can, drop stale ones.
        let now = Instant::now();
        let max_age = Duration::from_secs(30);
        struct ResolvedRotation {
            wi: usize,
            si: usize,
            workflow: Option<(String, String)>,
            old_sid: String,
            new_sid: String,
        }
        let mut resolved: Vec<ResolvedRotation> = Vec::new();
        self.pending_rotations.retain(|(old_sid, ts_ms, first_seen)| {
            if now.duration_since(*first_seen) > max_age {
                return false;
            }
            let Some(binding) = bindings.get(old_sid) else {
                return true;
            };
            let Some(new_sid) =
                workflow::history::find_post_rotation_sid(&binding.worktree, *ts_ms)
            else {
                return true;
            };
            if &new_sid == old_sid {
                return false;
            }
            resolved.push(ResolvedRotation {
                wi: binding.wi,
                si: binding.si,
                workflow: binding.workflow.clone(),
                old_sid: old_sid.clone(),
                new_sid,
            });
            false
        });
        for r in &resolved {
            // Rebind via the helper so `generation` bumps. Without this,
            // a reader holding a cursor for the pre-rotation transcript
            // would skip messages in the new file (cursor offset N from
            // the old file applied to the new file).
            self.workspaces[r.wi].sessions[r.si]
                .rebind_transcript(Some(r.new_sid.clone()));
            // Workflow-specific bookkeeping only when the session is a
            // workflow participant — non-workflow rebinds just need the
            // transcript_id swap + generation bump above.
            if let Some((run_id, role)) = &r.workflow {
                if let Some(run) =
                    self.workflow_runs.iter_mut().find(|run| &run.run_id == run_id)
                {
                    if let Some(b) = run.role_sessions.get_mut(role) {
                        b.current_session_id = Some(r.new_sid.clone());
                    }
                    run.role_baselines
                        .insert(role.clone(), workflow::run::MessageBaseline::default());
                    if run.active_role.as_deref() == Some(role.as_str()) {
                        if let Some(h) = run.history.last_mut() {
                            h.assistant_count_at_start = 0;
                            h.session_id = Some(r.new_sid.clone());
                        }
                    }
                    let _ = workflow::run::save(run);
                    log_tick(
                        run_id,
                        &format!(
                            "history-rotation: role={} {} -> {}",
                            role, r.old_sid, r.new_sid
                        ),
                    );
                }
            }
        }
        if !resolved.is_empty() {
            self.save_session_manifest();
            self.set_status_msg("Session rotated (/clear or /compact)");
        }
    }

    /// Process all pending backend events (non-blocking).
    pub fn drain_backend_events(&mut self) {
        while let Ok(event) = self.backend.event_rx.try_recv() {
            self.needs_redraw = true;
            match event {
                BackendEvent::TasksUpdated(tasks) => {
                    self.reconcile_tasks(tasks);
                    if !self.sessions_restored {
                        self.sessions_restored = true;
                        self.restore_sessions();
                    }
                }
                BackendEvent::Connected => {
                    self.connected = true;
                    self.set_status_msg("Connected to API");
                    // Restore sessions from manifest on first connect
                    // (tasks may not be populated yet, but they will be
                    // after TasksUpdated fires — see below).
                }
                BackendEvent::Disconnected => {
                    self.connected = false;
                }
                BackendEvent::ApiError(msg) => {
                    self.set_status_msg(&format!("API: {}", msg));
                }
                BackendEvent::Progress(msg) => {
                    self.set_status_msg(&msg);
                }
                BackendEvent::PullComplete {
                    task_id,
                    worktree_path,
                    main_repo,
                    session_id,
                    repo_url,
                    prompt,
                } => {
                    self.spawn_resumed_session(
                        Some(task_id),
                        worktree_path,
                        main_repo,
                        session_id,
                        repo_url,
                        prompt,
                    );
                }
                BackendEvent::PushComplete {
                    workspace_id,
                    task_id,
                } => {
                    // Local mutation gated on PushComplete: see
                    // `push_active` for the invariant. Reaching here
                    // means git push + GCS upload + API write all
                    // succeeded, so it's now safe to drop the local
                    // worktree state and flip to cloud.
                    self.finish_push(&workspace_id, task_id);
                }
                BackendEvent::PushFailed {
                    workspace_id,
                    error,
                } => {
                    if let Some(ws) = self
                        .workspaces
                        .iter_mut()
                        .find(|w| w.id == workspace_id)
                    {
                        ws.is_pushing = false;
                    }
                    self.set_status_msg(&format!("Push failed: {}", error));
                }
                BackendEvent::PlanTasksUpdated(tasks) => {
                    self.planning.update_from_api(tasks);
                }
                BackendEvent::PlanTaskUpdated(task) => {
                    self.planning.on_task_updated(task);
                }
                BackendEvent::PlanTaskCreated(task) => {
                    self.planning.on_task_created(task);
                }
                BackendEvent::PlanTaskDeleted(id) => {
                    self.planning.on_task_deleted(&id);
                }
            }
        }
    }

    /// Process pending control-socket requests. The socket server thread
    /// pushes (Request, reply_tx) tuples onto a shared queue; we pop each
    /// one, dispatch to a method handler, and send the Response back.
    /// Handlers run on the main loop so they have free `&mut self` access
    /// to App state without any extra locking.
    pub fn drain_control_events(&mut self) {
        let pending = self.control_queue.drain();
        if pending.is_empty() {
            return;
        }
        for entry in pending {
            let resp = self.dispatch_control(&entry.request);
            let _ = entry.reply.send(resp);
        }
        self.needs_redraw = true;
    }

    /// Dispatch a single control-socket request to its method handler.
    /// New handlers are added here as Phases 1+3 fill out the surface.
    ///
    /// **Persistence invariant**: any handler that mutates state which
    /// lives in `ManifestEntry` / `Workspace.tombstones` MUST call
    /// `self.save_session_manifest()` before returning Ok. A TUI crash
    /// between the mutation and the next unrelated save would otherwise
    /// lose the change — most painfully for tombstones, where a killed
    /// session would restore as live on next boot. Handlers that only
    /// touch in-memory state (`pending_prompt`, runtime status) don't
    /// need the save.
    fn dispatch_control(
        &mut self,
        req: &crate::control::protocol::Request,
    ) -> crate::control::protocol::Response {
        use crate::control::methods;
        use crate::control::protocol::{ErrorCode, Response};
        let caller = req.caller.session_uid.as_str();
        let result: methods::MethodResult = match req.method.as_str() {
            "ping" => Ok(serde_json::json!({
                "pong": true,
                "uid": req.caller.session_uid,
            })),
            "resolve_authorized_session" => {
                methods::resolve_authorized_session(self, caller, &req.params)
            }
            "list_sessions" => methods::list_sessions(self, caller, &req.params),
            "send_input" => methods::send_input(self, caller, &req.params),
            "kill_session" => methods::kill_session(self, caller, &req.params),
            "start_session" => methods::start_session(self, caller, &req.params),
            "start_workflow" => methods::start_workflow(self, caller, &req.params),
            "stop_workflow" => methods::stop_workflow(self, caller, &req.params),
            "get_workflow_state" => methods::get_workflow_state(self, caller, &req.params),
            "list_workflows" => methods::list_workflows(self, caller, &req.params),
            "create_subtask" => methods::create_subtask(self, caller, &req.params),
            "list_subtasks" => methods::list_subtasks(self, caller, &req.params),
            "mark_subtask_done" => methods::mark_subtask_done(self, caller, &req.params),
            other => Err((
                ErrorCode::UnknownMethod,
                format!("unknown method: {}", other),
            )),
        };
        match result {
            Ok(value) => {
                // Phase 6 activity feed. Only mutating methods land here;
                // read-only ones (`list_*`, `get_*`, `ping`,
                // `resolve_authorized_session`) are intentionally skipped.
                if let Some(summary) =
                    activity_summary_for(req.method.as_str(), &req.params, &value)
                {
                    self.log_activity(caller, summary);
                }
                Response::ok(req.id.clone(), value)
            }
            Err((code, msg)) => Response::err(req.id.clone(), code, msg),
        }
    }

    /// Spawn a new agent session in the given workspace, owned by the
    /// caller (managed_by_uid recorded). Used by the `start_session`
    /// MCP tool. Returns the new session's UID.
    pub fn spawn_managed_session(
        &mut self,
        ws_index: usize,
        caller_uid: &str,
        type_: &str,
        label: &str,
        task_id: Option<String>,
        prompt: Option<&str>,
    ) -> std::io::Result<String> {
        let worktree_path = self.workspaces[ws_index]
            .worktree_path
            .clone()
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "workspace has no worktree",
                )
            })?;
        let (cols, rows) = self.last_term_size;
        let session_uid = new_session_uid();
        // Bash gets a raw PTY shell — no MCP injection, no transcript
        // tracking. `idle` still flips correctly via the burst detector
        // (PTY-activity-based), and `send_input` works because it queues
        // raw bytes + Enter at the PTY layer. Useful when the caller
        // wants a shell the user can also drive interactively.
        let (session, session_type, pending) = if type_ == "bash" {
            let s = Session::new(
                "/bin/bash",
                &[],
                cols,
                rows,
                Some(worktree_path.clone()),
                Default::default(),
                None,
            )
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            (s, "bash".to_string(), None)
        } else {
            let engine = match type_ {
                "codex" => workflow::toml_schema::Engine::Codex,
                _ => workflow::toml_schema::Engine::ClaudeCode,
            };
            let (program, args) =
                crate::mcp_config::build_args(&engine, &session_uid, None, None)?;
            let pending = match engine {
                workflow::toml_schema::Engine::ClaudeCode => Self::list_jsonl_files(&worktree_path),
                workflow::toml_schema::Engine::Codex => Self::list_codex_sessions(&worktree_path),
            };
            let session_type = engine.as_session_type().to_string();
            let s = self
                .spawn_agent_session(
                    &session_type,
                    &session_uid,
                    &program,
                    &args,
                    cols,
                    rows,
                    Some(worktree_path),
                    Default::default(),
                )
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
            (s, session_type, Some(pending))
        };
        let mut pending_prompt = None;
        if let Some(text) = prompt {
            if !text.trim().is_empty() {
                pending_prompt = Some(PendingWrite::wait_for_quiet(
                    text.trim_end().to_string(),
                    true,
                    Duration::from_secs(1),
                    Duration::from_secs(2),
                    Duration::from_secs(180),
                ));
            }
        }
        let ts = TerminalSession {
            uid: session_uid.clone(),
            label: label.to_string(),
            session_type,
            session,
            status: SessionStatus::Running,
            last_write_at: None,
            transcript_id: None,
            generation: 0,
            pending_jsonl_files: pending,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            last_delivery: None,
            task_id,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: Some(caller_uid.to_string()),
            seeded_from_snapshot: None,
        };
        self.workspaces[ws_index].sessions.push(ts);
        self.save_session_manifest();
        Ok(session_uid)
    }

    /// Process planning editor events (non-blocking).
    /// Drain pending `MemoryKillEvent`s pushed by per-session watcher
    /// threads into the activity feed. Called once per main-loop tick
    /// alongside the other `drain_*` methods.
    pub fn drain_memory_kill_events(&mut self) {
        loop {
            let evt = match self.memory_kill_rx.try_recv() {
                Ok(e) => e,
                Err(_) => return,
            };
            let (caller, summary) = match evt {
                crate::session_watch::MemoryKillEvent::Killed {
                    session_uid,
                    pid,
                    comm,
                    argc,
                    argv_sha256_prefix,
                    rss_kb,
                    soft_cap_bytes,
                    ..
                } => {
                    // `comm` arrives sanitized, but re-escape at the
                    // render boundary (defense-in-depth — the writer
                    // is in another module and could regress).
                    let safe_comm = crate::session_watch::sanitize(comm.as_bytes(), 16);
                    let summary = format!(
                        "killed PID {} comm={} argc={} sha={} — {:.1} GiB RSS, soft cap {:.0} GiB",
                        pid,
                        safe_comm,
                        argc,
                        argv_sha256_prefix,
                        rss_kb as f64 / (1024.0 * 1024.0),
                        soft_cap_bytes as f64 / (1024.0 * 1024.0 * 1024.0),
                    );
                    (session_uid, summary)
                }
                crate::session_watch::MemoryKillEvent::KillFailed {
                    session_uid,
                    reason,
                    ..
                } => (session_uid, format!("memory cap kill failed: {}", reason)),
            };
            self.log_activity(&caller, summary);
        }
    }

    pub fn drain_planning_events(&mut self) {
        if let Some(action) = self.planning.drain_editor_events() {
            match action {
                PlanAction::UpdateTask { id, fields, status_msg } => {
                    self.backend.update_plan_task(id, fields);
                    if let Some(msg) = status_msg {
                        self.set_status_msg(&msg);
                    }
                }
                _ => {}
            }
            self.needs_redraw = true;
        }
        if self.planning.needs_redraw {
            self.needs_redraw = true;
            self.planning.needs_redraw = false;
        }
    }

    /// Bind freshly-spawned claude workflow sessions to their real sessionId
    /// by matching `last_delivery` prefix against new `history.jsonl` entries.
    ///
    /// The "newest new .jsonl" heuristic we rely on elsewhere can race when
    /// multiple claude processes share a project dir — another process's
    /// `/clear` rotation can produce a new file right when we're looking.
    /// Instead, we correlate: when we delivered a prompt whose text starts
    /// with P to an unbound session, claude later writes a history entry
    /// whose content starts with P; that entry's `sessionId` is ours.
    ///
    /// See [`entry_matches_delivery`] for the per-entry decision; that helper
    /// is the unit-testable witness for the post-2025 paste-redaction case
    /// that took down the overnight cleanup orchestration.
    fn resolve_pending_deliveries(&mut self, entries: &[workflow::history::HistoryEntry]) {
        if entries.is_empty() {
            return;
        }
        // Collect sids already claimed by any active workflow role so we
        // don't re-bind a session to a sid already in use.
        let mut claimed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for run in &self.workflow_runs {
            if !run.is_active() {
                continue;
            }
            for b in run.role_sessions.values() {
                if let Some(sid) = &b.current_session_id {
                    claimed.insert(sid.clone());
                }
            }
        }
        let mut to_bind: Vec<(usize, usize, String)> = Vec::new();
        for (wi, ws) in self.workspaces.iter().enumerate() {
            let Some(wt_str) = ws.worktree_path.as_deref().and_then(|p| p.to_str()) else {
                continue;
            };
            for (si, ts) in ws.sessions.iter().enumerate() {
                if ts.session_type != "claude"
                    || ts.workflow_run_id.is_none()
                    || ts.transcript_id.is_some()
                {
                    continue;
                }
                let Some((prefix, delivered_ms)) = ts.last_delivery.as_ref() else {
                    continue;
                };
                if prefix.is_empty() {
                    continue;
                }
                let mut best: Option<(u64, String)> = None;
                for e in entries {
                    if e.project != wt_str {
                        continue;
                    }
                    if e.timestamp_ms + 2000 < *delivered_ms {
                        continue;
                    }
                    if claimed.contains(&e.session_id) {
                        continue;
                    }
                    if !entry_matches_delivery(e, prefix) {
                        continue;
                    }
                    if best.as_ref().map_or(true, |(t, _)| e.timestamp_ms < *t) {
                        best = Some((e.timestamp_ms, e.session_id.clone()));
                    }
                }
                if let Some((_, sid)) = best {
                    to_bind.push((wi, si, sid));
                }
            }
        }
        for (wi, si, sid) in to_bind {
            let Some(ts) = self
                .workspaces
                .get_mut(wi)
                .and_then(|w| w.sessions.get_mut(si))
            else {
                continue;
            };
            let run_id = ts.workflow_run_id.clone();
            let role = ts.workflow_role.clone();
            ts.transcript_id = Some(sid.clone());
            ts.pending_jsonl_files = None;
            ts.last_delivery = None;
            if let (Some(run_id), Some(role)) = (run_id, role) {
                if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
                    if let Some(b) = run.role_sessions.get_mut(&role) {
                        b.current_session_id = Some(sid.clone());
                    }
                    if run.active_role.as_deref() == Some(role.as_str()) {
                        if let Some(h) = run.history.last_mut() {
                            h.session_id = Some(sid.clone());
                        }
                    }
                    let _ = workflow::run::save(run);
                    log_tick(
                        &run_id,
                        &format!("delivery-correlated: role={} sid={}", role, sid),
                    );
                }
            }
        }
    }

    /// Reconcile API tasks with local task entries + auto-provision a
    /// Workspace for each running/blocked task that doesn't have one bound.
    fn reconcile_tasks(&mut self, tasks: Vec<Task>) {
        // Save cursor context for restoration: remember the workspace id and
        // session label the cursor was on.
        let saved_ws_id = match &self.cursor {
            Cursor::Workspace(wi) => self.workspaces.get(*wi).map(|w| w.id.clone()),
            Cursor::Session(wi, _) => self.workspaces.get(*wi).map(|w| w.id.clone()),
            Cursor::Task { ws_idx, .. } => self.workspaces.get(*ws_idx).map(|w| w.id.clone()),
        };
        let saved_session_uid = match &self.cursor {
            Cursor::Session(wi, si) => self
                .workspaces
                .get(*wi)
                .and_then(|w| w.sessions.get(*si))
                .map(|s| s.uid.clone()),
            _ => None,
        };

        let mut seen_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for task in &tasks {
            // Only show active tasks in the sessions view; backlog/draft/done
            // stay in the planning view.
            match task.status.as_str() {
                "running" | "blocked" => {}
                _ => continue,
            }
            seen_ids.insert(task.id.clone());

            let display_name = task
                .name
                .as_deref()
                .or(task.prompt.as_deref())
                .unwrap_or(&task.id[..8.min(task.id.len())])
                .chars()
                .take(60)
                .collect::<String>();

            let is_cloud = task.is_cloud;
            // Local recovery covers both top-level CM branches (`cm/...`)
            // AND subtask branches (`cm-sub/...`). Pre-fix, `cm-sub/`
            // tasks reloaded with `workspace_id = None` after a manifest
            // loss because reconcile only matched `cm/` — leaving
            // start_session, workflow launch, and cleanup unable to
            // find the workspace.
            let is_local = !is_cloud
                && task.wip_branch.as_ref().map_or(false, |b| {
                    b.starts_with("cm/") || b.starts_with("cm-sub/")
                });

            // Upsert TaskEntry.
            if let Some(entry) = self
                .tasks
                .iter_mut()
                .find(|e| e.task_id.as_deref() == Some(&task.id))
            {
                entry.name = display_name.clone();
                entry.api_status = TaskStatus::from_api(&task.status);
                entry.repo_url = Some(task.repo_url.clone());
                entry.prompt = task.prompt.clone();
                entry.wip_branch = task.wip_branch.clone();
                entry.session_id = task.session_id.clone();
                entry.blocked_at = task.blocked_at.clone();
                entry.is_cloud = is_cloud;
                entry.project = task.project.clone();
                entry.parent_task_id = task.parent_task_id.clone();
                entry.worktree_mode = parse_worktree_mode(&task.worktree_mode);
            } else {
                self.tasks.push(TaskEntry {
                    task_id: Some(task.id.clone()),
                    name: display_name.clone(),
                    api_status: TaskStatus::from_api(&task.status),
                    repo_url: Some(task.repo_url.clone()),
                    prompt: task.prompt.clone(),
                    wip_branch: task.wip_branch.clone(),
                    session_id: task.session_id.clone(),
                    blocked_at: task.blocked_at.clone(),
                    is_cloud,
                    workspace_id: None,
                    project: task.project.clone(),
                    parent_task_id: task.parent_task_id.clone(),
                    worktree_mode: parse_worktree_mode(&task.worktree_mode),
                });
            }

            // Link (or create) a Workspace for this task if it doesn't already
            // have one. Multi-task workspaces: users explicitly bind via the
            // launch-into-workspace picker, so we only auto-bind when the
            // task's own worktree (local) or VM (cloud) matches.
            let task_idx = self
                .tasks
                .iter()
                .position(|t| t.task_id.as_deref() == Some(&task.id))
                .expect("just inserted");
            if self.tasks[task_idx].workspace_id.is_some() {
                continue;
            }

            // Honor manifest binding before auto-provisioning. On the first
            // reconcile tick self.workspaces is still empty (restore_sessions
            // runs right after), so without this we'd spawn an orphan that
            // restore_sessions later supersedes via bindings.
            if let Some(ws_id) = self.manifest_bindings.get(&task.id) {
                self.tasks[task_idx].workspace_id = Some(ws_id.clone());
                continue;
            }

            let (worktree_path, main_repo_path) = if is_local {
                // Single resolver handles both `cm/<slug>` and
                // `cm-sub/<chain>-<short>` layouts. See
                // `worktree::recover_worktree_path`.
                let wt = task
                    .wip_branch
                    .as_ref()
                    .and_then(|b| worktree::recover_worktree_path(&task.repo_url, b));
                let main = wt.is_some().then(|| worktree::find_local_repo(&task.repo_url)).flatten();
                (wt, main)
            } else {
                (None, None)
            };

            // Match an existing workspace:
            //   - local: same worktree_path
            //   - cloud: same worker_vm (VM uniquely identifies the cloud workspace)
            let existing_ws_idx = if is_cloud {
                task.worker_vm.as_deref().filter(|s| !s.is_empty()).and_then(|vm| {
                    self.workspaces
                        .iter()
                        .position(|w| w.is_cloud && w.worker_vm.as_deref() == Some(vm))
                })
            } else {
                worktree_path.as_ref().and_then(|wt| {
                    self.workspaces
                        .iter()
                        .position(|w| w.worktree_path.as_deref() == Some(wt.as_path()))
                })
            };

            let ws_id = if let Some(wi) = existing_ws_idx {
                self.workspaces[wi].id.clone()
            } else if is_cloud || worktree_path.is_some() {
                // Auto-provision a workspace so this task gets a sidebar row.
                let ws = Workspace {
                    id: new_workspace_id(),
                    name: display_name.clone(),
                    is_closed: false,
                    is_cloud,
                    repo_url: Some(task.repo_url.clone()),
                    worktree_path,
                    main_repo_path,
                    worker_vm: task.worker_vm.clone(),
                    worker_zone: task.worker_zone.clone(),
                    sessions: vec![],
                    tombstones: Vec::new(),
                    is_pushing: false,
                };
                let id = ws.id.clone();
                self.workspaces.push(ws);
                id
            } else {
                continue;
            };
            self.tasks[task_idx].workspace_id = Some(ws_id);
        }

        // Retain tasks: keep those still seen by the API, plus anything still
        // referenced by a workspace (in case a bound task temporarily vanished
        // from the API — unlikely but defensive).
        let ws_bound_task_ids: std::collections::HashSet<String> = self
            .workspaces
            .iter()
            .flat_map(|w| {
                self.tasks
                    .iter()
                    .filter(move |t| t.workspace_id.as_deref() == Some(&w.id))
                    .filter_map(|t| t.task_id.clone())
            })
            .collect();
        self.tasks.retain(|t| {
            if t.api_status == TaskStatus::Done {
                return false;
            }
            match &t.task_id {
                Some(id) => {
                    seen_ids.contains(id)
                        || ws_bound_task_ids.contains(id)
                }
                None => false,
            }
        });

        // Also GC workspaces whose worker_vm-based cloud task is gone.
        // Keep local workspaces always (they survive task lifecycle).
        self.workspaces.retain(|w| {
            if !w.is_cloud {
                return true;
            }
            let vm = match w.worker_vm.as_deref() {
                Some(vm) if !vm.is_empty() => vm,
                _ => return true,
            };
            tasks.iter().any(|t| {
                t.is_cloud
                    && t.worker_vm.as_deref() == Some(vm)
                    && matches!(t.status.as_str(), "running" | "blocked")
            })
        });

        // Sort workspaces by effective status (via their first bound task if
        // any). No bound task → put last.
        let status_rank = |s: &TaskStatus| -> u8 {
            match s {
                TaskStatus::Running => 0,
                TaskStatus::Blocked => 1,
                TaskStatus::Backlog => 2,
                TaskStatus::Done => 3,
            }
        };
        let workspace_rank: Vec<(String, u8)> = self
            .workspaces
            .iter()
            .map(|w| {
                let rank = self
                    .first_task_for_ws(&w.id)
                    .map(|t| status_rank(&self.task_status(t)))
                    .unwrap_or(4);
                (w.id.clone(), rank)
            })
            .collect();
        let rank_of = |id: &str| -> u8 {
            workspace_rank
                .iter()
                .find(|(i, _)| i == id)
                .map(|(_, r)| *r)
                .unwrap_or(4)
        };
        self.workspaces.sort_by_key(|w| rank_of(&w.id));

        // Restore cursor by workspace id.
        if let Some(ref id) = saved_ws_id {
            if let Some(wi) = self.workspaces.iter().position(|w| &w.id == id) {
                if let Some(ref uid) = saved_session_uid {
                    if let Some(si) = self.workspaces[wi]
                        .sessions
                        .iter()
                        .position(|s| &s.uid == uid)
                    {
                        self.cursor = Cursor::Session(wi, si);
                    } else {
                        self.cursor = Cursor::Workspace(wi);
                    }
                } else {
                    self.cursor = Cursor::Workspace(wi);
                }
            }
        }
        self.clamp_cursor();
    }

    fn set_status_msg(&mut self, msg: &str) {
        self.status_msg = Some((msg.to_string(), Instant::now()));
    }

    /// Toggle terminal mouse capture. When off, mouse events go to the
    /// terminal emulator instead of the TUI, so the user can use native
    /// selection (including block-select chords like Ctrl+Shift+drag).
    fn toggle_mouse_capture(&mut self) {
        use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
        use crossterm::execute;
        let mut stdout = std::io::stdout();
        if self.mouse_capture_enabled {
            let _ = execute!(stdout, DisableMouseCapture);
            self.mouse_capture_enabled = false;
            self.set_status_msg("Mouse capture OFF — use terminal's native selection (Alt+m to re-enable)");
        } else {
            let _ = execute!(stdout, EnableMouseCapture);
            self.mouse_capture_enabled = true;
            self.set_status_msg("Mouse capture ON");
        }
    }

    // ── Input handling ──────────────────────────────────────────────

    /// Handle a crossterm event. Returns true if consumed.
    pub fn handle_event(&mut self, event: &CrosstermEvent) -> bool {
        // Drop key release events — we only care about presses/repeats.
        if let CrosstermEvent::Key(key) = event {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return false;
            }
        }

        self.needs_redraw = true;

        // Alt+t toggles between Sessions and Planning view.
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('t') {
                self.view_mode = match self.view_mode {
                    ViewMode::Sessions => {
                        // Refresh planning tasks when switching to planning view.
                        self.backend.refresh_plan_tasks();
                        ViewMode::Planning
                    }
                    ViewMode::Planning => ViewMode::Sessions,
                };
                return true;
            }
        }

        // Alt+m toggles mouse capture so the user can use their terminal's
        // native selection (including block-select chords).
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char('m') {
                self.toggle_mouse_capture();
                return true;
            }
            // Phase 6: Alt+, toggles the activity feed strip.
            if key.modifiers.contains(KeyModifiers::ALT) && key.code == KeyCode::Char(',') {
                self.activity_visible = !self.activity_visible;
                self.needs_redraw = true;
                return true;
            }
        }

        // Delegate to planning view when in Planning mode.
        if self.view_mode == ViewMode::Planning {
            // Keep planning's workspace picker in sync with the current
            // set of open workspaces before it sees the event.
            let candidates = self.collect_workspace_candidates();
            self.planning.set_workspace_candidates(candidates);
            let action = self.planning.handle_event(event);
            match action {
                PlanAction::Consumed => return true,
                PlanAction::Ignored => return false,
                PlanAction::LaunchTask {
                    project,
                    slug,
                    prompt,
                    branch,
                    autostart,
                    task_id,
                } => {
                    self.launch_from_plan(&project, &slug, &prompt, branch.as_deref(), autostart, &task_id);
                    return true;
                }
                PlanAction::LaunchTaskIntoWorkspace {
                    workspace_id,
                    task_id,
                    task_title,
                    task_repo_url,
                    project,
                    prompt,
                } => {
                    self.launch_into_workspace(
                        &workspace_id,
                        &task_id,
                        &task_title,
                        &task_repo_url,
                        &project,
                        &prompt,
                    );
                    return true;
                }
                PlanAction::UnbindTask { task_id } => {
                    self.unbind_task_from_workspace(&task_id);
                    return true;
                }
                PlanAction::UnlaunchTask { task_id } => {
                    self.unlaunch_task(&task_id);
                    return true;
                }
                PlanAction::ReopenTask { task_id } => {
                    self.reopen_task_from_planning(&task_id);
                    return true;
                }
                PlanAction::SwitchToSessions => {
                    self.view_mode = ViewMode::Sessions;
                    return true;
                }
                PlanAction::Quit => {
                    self.save_session_manifest();
                    self.should_quit = true;
                    return true;
                }
                PlanAction::CreateTask {
                    project,
                    repo_url,
                    name,
                    description,
                    status,
                    parent_task_id,
                    worktree_mode,
                } => {
                    self.backend.create_plan_task(
                        project,
                        repo_url,
                        name,
                        description,
                        status,
                        parent_task_id,
                        worktree_mode,
                    );
                    return true;
                }
                PlanAction::UpdateTask { id, fields, status_msg } => {
                    self.backend.update_plan_task(id, fields);
                    if let Some(msg) = status_msg {
                        self.set_status_msg(&msg);
                    }
                    return true;
                }
                PlanAction::BulkUpdateTasks { ids, fields } => {
                    for id in ids {
                        self.backend.update_plan_task(id, fields.clone());
                    }
                    return true;
                }
                PlanAction::DeleteTask { id } => {
                    self.backend.delete_plan_task(id);
                    return true;
                }
                PlanAction::RefreshTasks => {
                    self.backend.refresh_plan_tasks();
                    return true;
                }
            }
        }

        // If in input mode, handle input events.
        if self.is_input_mode() {
            return self.handle_input_event(event);
        }

        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) {
                match key.code {
                    KeyCode::Char('q') => {
                        self.save_session_manifest();
                        self.should_quit = true;
                        return true;
                    }
                    KeyCode::Char('j') => {
                        self.navigate(1);
                        return true;
                    }
                    KeyCode::Char('k') => {
                        self.navigate(-1);
                        return true;
                    }
                    KeyCode::Char('v') => {
                        self.sidebar_view = match self.sidebar_view {
                            SidebarView::Status => SidebarView::Task,
                            SidebarView::Task => SidebarView::Status,
                        };
                        self.save_session_manifest();
                        return true;
                    }
                    KeyCode::Char('s') => {
                        self.start_new_terminal_session();
                        return true;
                    }
                    // A-W (close workspace) vs A-w (close session). Terminals
                    // differ on whether Shift is baked into the char case or
                    // reported as a modifier — accept both forms.
                    KeyCode::Char('W') => {
                        self.close_active_workspace();
                        return true;
                    }
                    KeyCode::Char('w')
                        if key.modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        self.close_active_workspace();
                        return true;
                    }
                    KeyCode::Char('w') => {
                        self.close_active_session();
                        return true;
                    }
                    KeyCode::Char('a') => {
                        self.attach_active();
                        return true;
                    }
                    KeyCode::Char('d') => {
                        self.input_mode = InputMode::Confirm {
                            prompt: "Mark task done? Sessions for this task will close.".to_string(),
                            action: ConfirmAction::MarkDone,
                        };
                        return true;
                    }
                    KeyCode::Char('x') => {
                        let prompt = if self.cursor_task_id().is_some() {
                            "Delete this task and close its sessions?".to_string()
                        } else {
                            "Delete this workspace? Worktree, branch, and any bound tasks will be removed.".to_string()
                        };
                        self.input_mode = InputMode::Confirm {
                            prompt,
                            action: ConfirmAction::Delete,
                        };
                        return true;
                    }
                    KeyCode::Char('r') => {
                        self.backend.refresh();
                        self.set_status_msg("Refreshing...");
                        return true;
                    }
                    KeyCode::Char('e') => {
                        self.open_session_settings();
                        return true;
                    }
                    KeyCode::Char('h') => {
                        self.toggle_session_hidden();
                        return true;
                    }
                    KeyCode::Char('b') => {
                        self.open_save_snapshot();
                        return true;
                    }
                    KeyCode::Char('z') => {
                        self.open_snapshot_catalog(None);
                        return true;
                    }
                    KeyCode::Char('n') => {
                        self.start_new_session();
                        return true;
                    }
                    KeyCode::Char('p') => {
                        self.push_active();
                        return true;
                    }
                    KeyCode::Char('l') => {
                        self.pull_active();
                        return true;
                    }
                    KeyCode::Char('f') => {
                        self.open_workflow_launch();
                        return true;
                    }
                    KeyCode::Char('u') => {
                        self.resume_workflow_for_cursor();
                        return true;
                    }
                    // A-O (Alt+Shift+O): open the past-workspaces picker.
                    // Past workspaces never appear in the sidebar — this is
                    // the only path to find and reopen them. Terminals
                    // differ on whether Shift is folded into the case of
                    // the char or reported as a modifier — accept both.
                    KeyCode::Char('O') => {
                        self.open_past_workspace_picker();
                        return true;
                    }
                    KeyCode::Char('o')
                        if key.modifiers.contains(KeyModifiers::SHIFT) =>
                    {
                        self.open_past_workspace_picker();
                        return true;
                    }
                    KeyCode::Char('o') => {
                        if let Some(run_id) = self.focused_session_run_id() {
                            self.input_mode = InputMode::Confirm {
                                prompt: "Stop workflow? Sessions stay open; run can't resume.".to_string(),
                                action: ConfirmAction::StopWorkflow { run_id },
                            };
                        } else {
                            self.set_status_msg("Focused session is not in a workflow");
                        }
                        return true;
                    }
                    KeyCode::Char('y') => {
                        self.open_workflow_history();
                        return true;
                    }
                    _ => {}
                }
            }
        }

        // Handle scroll in terminal.
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                match key.code {
                    KeyCode::PageUp => {
                        if let Some((_, ts)) = self.active_session() {
                            use alacritty_terminal::grid::Scroll;
                            ts.session.term.lock().scroll_display(Scroll::PageUp);
                        }
                        return true;
                    }
                    KeyCode::PageDown => {
                        if let Some((_, ts)) = self.active_session() {
                            use alacritty_terminal::grid::Scroll;
                            ts.session.term.lock().scroll_display(Scroll::PageDown);
                        }
                        return true;
                    }
                    _ => {}
                }
            }
            // Plain PageUp/PageDown also scroll (Shift not required).
            match key.code {
                KeyCode::PageUp if key.modifiers.is_empty() => {
                    if let Some((_, ts)) = self.active_session() {
                        use alacritty_terminal::grid::Scroll;
                        ts.session.term.lock().scroll_display(Scroll::PageUp);
                    }
                    return true;
                }
                KeyCode::PageDown if key.modifiers.is_empty() => {
                    if let Some((_, ts)) = self.active_session() {
                        use alacritty_terminal::grid::Scroll;
                        ts.session.term.lock().scroll_display(Scroll::PageDown);
                    }
                    return true;
                }
                _ => {}
            }
        }

        // Handle mouse events over the terminal pane: scroll wheel + click-drag selection.
        // Always consume — un-consumed mouse events would fall through to the terminal
        // forwarder below, which both snaps scroll to bottom and writes ANSI bytes to the PTY.
        if let CrosstermEvent::Mouse(me) = event {
            self.handle_terminal_mouse(me);
            return true;
        }

        // Handle bracketed paste — send entire text at once, wrapped in
        // bracket escapes if the inner program has enabled bracketed paste mode.
        if let CrosstermEvent::Paste(text) = event {
            let mut paste_err: Option<(String, std::io::Error)> = None;
            let mut handled = false;
            if let Some(ts) = self.active_session_mut() {
                if !ts.session.exited {
                    use alacritty_terminal::grid::Scroll;
                    ts.session.term.lock().scroll_display(Scroll::Bottom);

                    let term_mode = *ts.session.term.lock().mode();
                    let data = if term_mode.contains(TermMode::BRACKETED_PASTE) {
                        format!("\x1b[200~{}\x1b[201~", text).into_bytes()
                    } else {
                        text.as_bytes().to_vec()
                    };
                    if let Err(e) = ts.session.write(&data) {
                        paste_err = Some((ts.label.clone(), e));
                    }
                    ts.last_write_at = Some(Instant::now());
                    handled = true;
                }
            }
            if let Some((label, e)) = paste_err {
                self.set_status_msg(&format!("paste to {}: {}", label, e));
            }
            if handled {
                return true;
            }
        }

        // If the focused session is part of a running workflow and the user
        // hit Ctrl-C, pause the run. We do not swallow the keystroke — it's
        // still forwarded below so the agent sees the interrupt as usual.
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('c')
            {
                self.pause_focused_workflow();
            }
        }

        // Forward to active terminal.
        let mut input_err: Option<(String, std::io::Error)> = None;
        let mut handled = false;
        if let Some(ts) = self.active_session_mut() {
            if !ts.session.exited {
                // Auto-scroll to bottom on any input so the cursor stays visible.
                {
                    use alacritty_terminal::grid::Scroll;
                    ts.session.term.lock().scroll_display(Scroll::Bottom);
                }
                let term_mode = *ts.session.term.lock().mode();
                if let Some(bytes) = input::event_to_bytes(event, &term_mode) {
                    if let Err(e) = ts.session.write(&bytes) {
                        input_err = Some((ts.label.clone(), e));
                    }
                    ts.last_write_at = Some(Instant::now());
                }
                handled = true;
            }
        }
        if let Some((label, e)) = input_err {
            self.set_status_msg(&format!("input to {}: {}", label, e));
        }
        if handled {
            return true;
        }

        false
    }

    /// Handle a mouse event over the terminal pane.
    /// Returns true if the event was consumed.
    fn handle_terminal_mouse(&mut self, me: &crossterm::event::MouseEvent) -> bool {
        if !matches!(self.view_mode, ViewMode::Sessions) {
            return false;
        }
        // Terminal inner rect (after border) sits at (1,1) with last_term_size dims.
        let (term_cols, term_rows) = self.last_term_size;
        if me.column < 1 || me.row < 1
            || me.column > term_cols
            || me.row > term_rows
        {
            return false;
        }
        let grid_col = (me.column - 1) as usize;
        let viewport_row = (me.row - 1) as usize;

        let Some(ts) = self.active_session_mut() else { return false; };

        use alacritty_terminal::grid::Scroll;
        use alacritty_terminal::index::{Column, Point as GridPoint, Side};
        use alacritty_terminal::selection::{Selection, SelectionType};
        use alacritty_terminal::term::viewport_to_point;

        match me.kind {
            MouseEventKind::ScrollUp => {
                ts.session.term.lock().scroll_display(Scroll::Delta(3));
                true
            }
            MouseEventKind::ScrollDown => {
                ts.session.term.lock().scroll_display(Scroll::Delta(-3));
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let mut term = ts.session.term.lock();
                let display_offset = term.grid().display_offset();
                let point = viewport_to_point(
                    display_offset,
                    GridPoint::new(viewport_row, Column(grid_col)),
                );
                let ty = if me.modifiers.contains(KeyModifiers::ALT) {
                    SelectionType::Block
                } else {
                    SelectionType::Simple
                };
                term.selection = Some(Selection::new(ty, point, Side::Left));
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let mut term = ts.session.term.lock();
                let display_offset = term.grid().display_offset();
                let point = viewport_to_point(
                    display_offset,
                    GridPoint::new(viewport_row, Column(grid_col)),
                );
                if let Some(sel) = term.selection.as_mut() {
                    sel.update(point, Side::Right);
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let text = ts.session.term.lock().selection_to_string();
                if let Some(text) = text {
                    if !text.is_empty() {
                        copy_to_clipboard(&text);
                        self.set_status_msg(&format!("Copied {} chars", text.len()));
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Handle events while in input mode.
    fn handle_input_event(&mut self, event: &CrosstermEvent) -> bool {
        // Non-Key events (resize, focus, etc.) match pre-extraction
        // behavior: while in any non-Normal input mode, the event is
        // absorbed by the modal.
        if !matches!(event, CrosstermEvent::Key(_)) {
            return true;
        }
        let urls = sorted_repo_urls(&self.config.repos);
        let outcome = match &mut self.input_mode {
            InputMode::Normal => InputOutcome::Ignored,
            InputMode::NewSession {
                label_text,
                branch_text,
                idle_timeout_text,
                repo_url,
                seed_from,
                active_field,
            } => handle_new_session(
                NewSessionMut {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    seed_from,
                    active_field,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::NewTerminalSession {
                workspace_id,
                session_type,
                task_id,
                seed_from,
                active_field,
            } => handle_new_terminal_session(
                NewTerminalSessionMut {
                    workspace_id: workspace_id.as_str(),
                    session_type,
                    task_id,
                    seed_from,
                    active_field,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::SessionSettings {
                ws_index,
                session_index,
                name,
                idle_timeout,
                burst_threshold,
                hidden,
                notify_on_idle,
                seeded_from_snapshot: _,
                active_field,
            } => handle_session_settings(
                SessionSettingsMut {
                    ws_index: *ws_index,
                    session_index: *session_index,
                    name,
                    idle_timeout,
                    burst_threshold,
                    hidden,
                    notify_on_idle,
                    active_field,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::WorkspaceSettings { ws_index, name } => handle_workspace_settings(
                WorkspaceSettingsMut {
                    ws_index: *ws_index,
                    name,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::SaveSnapshot {
                workspace_id,
                session_uid,
                name_text,
                description_text,
                active_field,
                error,
            } => handle_save_snapshot(
                SaveSnapshotMut {
                    workspace_id: workspace_id.as_str(),
                    session_uid: session_uid.as_str(),
                    name_text,
                    description_text,
                    active_field,
                    error,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::SnapshotCatalog {
                snapshots,
                selected,
                mode,
                picker_target,
                status_msg,
            } => handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots,
                    selected,
                    mode,
                    picker_target: picker_target.as_ref(),
                    status_msg,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::TaskSettings { task_id, name } => handle_task_settings(
                TaskSettingsMut {
                    task_id: task_id.as_str(),
                    name,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::WorkflowLaunchConfirm {
                ws_index,
                workflow_name,
                slots,
                active_slot,
                goal,
            } => handle_workflow_launch_confirm(
                WorkflowLaunchConfirmMut {
                    ws_index: *ws_index,
                    workflow_name: workflow_name.as_str(),
                    slots,
                    active_slot,
                    goal,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::WorkflowPicker {
                ws_index,
                focused_si,
                names,
                selected,
            } => handle_workflow_picker(
                WorkflowPickerMut {
                    ws_index: *ws_index,
                    focused_si: *focused_si,
                    names,
                    selected,
                },
                InputCtx { repo_urls: &urls },
                event,
            ),
            InputMode::WorkflowHistory { run_id: _ } => {
                handle_workflow_history(InputCtx { repo_urls: &urls }, event)
            }
            InputMode::PastWorkspacePicker { candidates, selected } => {
                handle_past_workspace_picker(
                    candidates,
                    selected,
                    InputCtx { repo_urls: &urls },
                    event,
                )
            }
            InputMode::Confirm { action, .. } => {
                handle_confirm(action, InputCtx { repo_urls: &urls }, event)
            }
        };
        self.apply_input_outcome(outcome)
    }

    /// Apply a handler's outcome to App state. Returns whether the event
    /// was consumed by an input modal (matches the legacy `bool` return
    /// of `handle_input_event` — `false` means "fall through to terminal/
    /// app keybindings", which only happens in `InputMode::Normal`).
    fn apply_input_outcome(&mut self, outcome: InputOutcome) -> bool {
        match outcome {
            InputOutcome::Ignored => false,
            InputOutcome::Consumed => true,
            InputOutcome::Cancel => {
                // If the cancelled modal was a snapshot picker invoked
                // from a parent form, re-open the form with seed_from
                // unchanged (None on first open). Otherwise the user's
                // form input would be silently lost on a picker Esc.
                let old = std::mem::replace(&mut self.input_mode, InputMode::Normal);
                if let InputMode::SnapshotCatalog {
                    picker_target: Some(target),
                    ..
                } = old
                {
                    self.reopen_form_from_picker(target, None);
                }
                true
            }
            InputOutcome::Submit(action) => {
                // Special case: picker-mode SnapshotPicked submission
                // returns to the parent form with seed_from set to the
                // chosen name. Other submits (and a no-target catalog)
                // go through the normal path.
                let old = std::mem::replace(&mut self.input_mode, InputMode::Normal);
                if let InputMode::SnapshotCatalog {
                    picker_target: Some(target),
                    ..
                } = old
                {
                    if let SubmitAction::SnapshotPicked { name } = action {
                        self.reopen_form_from_picker(target, Some(name));
                        return true;
                    }
                    // Some other submit emerged from picker mode
                    // (shouldn't happen given the catalog handler, but
                    // safe-fall back to reopening the form unchanged).
                    self.reopen_form_from_picker(target, None);
                    return true;
                }
                self.apply_submit_action(action);
                true
            }
        }
    }

    /// Restore the parent form that opened the snapshot picker.
    fn reopen_form_from_picker(
        &mut self,
        target: PickerTarget,
        name: Option<String>,
    ) {
        self.input_mode = rebuild_form_from_picker(target, name);
    }

    fn apply_submit_action(&mut self, action: SubmitAction) {
        match action {
            SubmitAction::None => {}
            SubmitAction::CreateLocalSession {
                repo_url,
                label,
                branch,
                idle_timeout_secs,
                seed_from,
            } => {
                self.create_local_session(
                    &repo_url,
                    &label,
                    branch.as_deref(),
                    idle_timeout_secs,
                    seed_from.as_deref(),
                );
            }
            SubmitAction::SpawnSessionOnWorkspace {
                workspace_id,
                session_type,
                task_id,
                seed_from,
            } => {
                self.spawn_session_on_workspace(
                    &workspace_id,
                    &session_type,
                    task_id,
                    seed_from.as_deref(),
                );
            }
            SubmitAction::OpenSnapshotPickerForNewSession {
                label_text,
                branch_text,
                idle_timeout_text,
                repo_url,
                existing_seed_from,
            } => {
                self.open_snapshot_catalog(Some(PickerTarget::NewSession {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    existing_seed_from,
                }));
            }
            SubmitAction::OpenSnapshotPickerForNewTerminalSession {
                workspace_id,
                session_type,
                task_id,
                existing_seed_from,
            } => {
                self.open_snapshot_catalog(Some(
                    PickerTarget::NewTerminalSession {
                        workspace_id,
                        session_type,
                        task_id,
                        existing_seed_from,
                    },
                ));
            }
            SubmitAction::SaveSessionSettings {
                ws_index,
                session_index,
                name,
                idle_timeout,
                burst_threshold,
                hidden,
                notify_on_idle,
            } => {
                if let Some(ws) = self.workspaces.get_mut(ws_index) {
                    if let Some(ts) = ws.sessions.get_mut(session_index) {
                        if !name.trim().is_empty() {
                            ts.label = name;
                        }
                        ts.idle_timeout_secs = idle_timeout;
                        ts.burst_threshold = burst_threshold;
                        ts.hidden = hidden;
                        ts.notify_on_idle = notify_on_idle;
                    }
                }
                self.save_session_manifest();
                self.set_status_msg("Settings saved");
            }
            SubmitAction::SaveWorkspaceName { ws_index, name } => {
                if !name.is_empty() {
                    if let Some(ws) = self.workspaces.get_mut(ws_index) {
                        ws.name = name;
                    }
                    self.save_session_manifest();
                    self.set_status_msg("Workspace renamed");
                }
            }
            SubmitAction::SaveSnapshot {
                workspace_id,
                session_uid,
                name,
                description,
            } => {
                self.handle_save_snapshot_submit(
                    workspace_id,
                    session_uid,
                    name,
                    description,
                );
            }
            SubmitAction::SnapshotPicked { name } => {
                // Picker-mode result. Chunk 4 never opens the catalog in
                // picker mode; chunk 5 will wire this to the seed-from
                // field on the new-session form. For now we just toast so
                // the variant has somewhere to land if a misbehaving caller
                // emits it.
                let _ = name;
            }
            SubmitAction::SaveTaskName { task_id, name } => {
                if !name.is_empty() {
                    if let Some(task) = self
                        .tasks
                        .iter_mut()
                        .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
                    {
                        task.name = name.clone();
                    }
                    let mut fields = HashMap::new();
                    fields.insert("name".to_string(), serde_json::Value::String(name));
                    self.backend.update_plan_task(task_id, fields);
                    self.set_status_msg("Task renamed");
                }
            }
            SubmitAction::EnterWorkflowLaunchConfirm {
                ws_index,
                focused_si,
                workflow_name,
            } => {
                self.enter_workflow_launch_confirm(ws_index, focused_si, workflow_name);
            }
            SubmitAction::LaunchWorkflow {
                ws_index,
                workflow_name,
                slots,
                goal,
            } => {
                self.launch_workflow(ws_index, &workflow_name, slots, goal);
            }
            SubmitAction::MarkActiveDone => self.mark_active_done(),
            SubmitAction::DeleteActive => self.delete_active(),
            SubmitAction::StopWorkflow { run_id } => self.stop_workflow_run(&run_id),
            SubmitAction::ReopenPastWorkspace { ws_id } => {
                self.reopen_workspace_by_id(&ws_id);
            }
        }
    }

    // ── Session management ──────────────────────────────────────────

    /// Enter input mode to create a new workspace (empty, no task binding).
    fn start_new_session(&mut self) {
        // Seed with the first repo from config, sorted by name so the picker
        // is deterministic. ←/→ cycles through the rest.
        let repo_url = match sorted_repo_urls(&self.config.repos).first() {
            Some(url) => url.clone(),
            None => {
                self.set_status_msg("No repos configured");
                return;
            }
        };

        self.input_mode = InputMode::NewSession {
            label_text: String::new(),
            branch_text: String::new(),
            idle_timeout_text: DEFAULT_IDLE_TIMEOUT_SECS.to_string(),
            repo_url,
            seed_from: None,
            active_field: 0,
        };
    }


    /// Enter input mode to add a terminal session to the active workspace.
    /// If the cursor is inside a task scope, the new session inherits that
    /// task_id so it appears under the task subheader.
    fn start_new_terminal_session(&mut self) {
        let wi = match self.active_workspace_index() {
            Some(wi) => wi,
            None => {
                self.set_status_msg("No workspace selected");
                return;
            }
        };
        // A push in flight will tombstone every live session on the
        // workspace when `PushComplete` lands, so a session added now
        // would silently disappear seconds later — confusing enough
        // that we bounce the user with an explicit message instead.
        if self.workspaces[wi].is_pushing {
            self.set_status_msg("Workspace is being pushed to cloud, retry after");
            return;
        }
        let task_id = self.cursor_task_id();
        // Capture workspace_id (stable) instead of the index — backend
        // events fired while the form is open can reorder workspaces,
        // and a stored index would silently target the wrong workspace
        // by submit time.
        let workspace_id = self.workspaces[wi].id.clone();
        self.input_mode = InputMode::NewTerminalSession {
            workspace_id,
            session_type: "claude".to_string(),
            task_id,
            seed_from: None,
            active_field: 0,
        };
    }

    /// Public wrapper exposed to the control-socket method handlers
    /// (which live in `crate::control::methods`).
    pub(crate) fn tombstone_session_pub(ws: &mut Workspace, si: usize) {
        Self::tombstone_session(ws, si);
    }

    /// Bulk session removal that preserves the tombstone invariant.
    /// Walks `ws.sessions`, tombstones each entry where `should_drop`
    /// returns true, marks the PTY exited, and removes it. Use this
    /// instead of `ws.sessions.retain(...)` or `ws.sessions.clear()` —
    /// otherwise `read_session_output` for the closed sessions returns
    /// `not_found` instead of `state: "exited"`.
    ///
    /// **Persists the manifest before returning** when anything was
    /// removed. This is deliberate — every previous round of review
    /// found another caller that forgot to persist, breaking Phase 2b
    /// across TUI crashes. Pushing the save into the helper makes it
    /// impossible to forget. Callers can ignore the return value if
    /// they don't need the count; the persist is unconditional.
    pub(crate) fn tombstone_and_remove(
        &mut self,
        ws_index: usize,
        mut should_drop: impl FnMut(&TerminalSession) -> bool,
    ) -> usize {
        let Some(ws) = self.workspaces.get_mut(ws_index) else {
            return 0;
        };
        let mut removed = 0;
        let mut i = 0;
        while i < ws.sessions.len() {
            if should_drop(&ws.sessions[i]) {
                Self::tombstone_session(ws, i);
                ws.sessions[i].session.exited = true;
                ws.sessions.remove(i);
                removed += 1;
            } else {
                i += 1;
            }
        }
        if removed > 0 {
            self.save_session_manifest();
        }
        removed
    }

    /// Build a tombstone from `ws.sessions[si]` and push it onto
    /// `ws.tombstones`. Doesn't remove the session — caller does that
    /// to keep the borrow flow simple. Snapshots the workspace's
    /// `worktree_path` into the tombstone so post-close mutations of
    /// the workspace (e.g. `push_active` clearing the path) don't
    /// silently break `read_session_output`.
    fn tombstone_session(ws: &mut Workspace, si: usize) {
        let Some(ts) = ws.sessions.get(si) else {
            return;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let worktree_snapshot = ws.worktree_path.clone();
        ws.tombstones.push(SessionTombstone {
            uid: ts.uid.clone(),
            managed_by_uid: ts.managed_by_uid.clone(),
            label: ts.label.clone(),
            session_type: ts.session_type.clone(),
            task_id: ts.task_id.clone(),
            last_transcript_id: ts.transcript_id.clone(),
            worktree_path: worktree_snapshot,
            generation: ts.generation,
            exited_at: now,
        });
    }

    /// Close the current session: extract a `SessionTombstone` from its
    /// metadata, push it onto the workspace's tombstone list, then drop
    /// the live entry (which tears down the PTY). The resolver can still
    /// answer `read_session_output` for the closed session via the
    /// tombstone.
    fn close_active_session(&mut self) {
        match self.cursor.clone() {
            Cursor::Session(wi, si) => {
                if let Some(ws) = self.workspaces.get_mut(wi) {
                    if si < ws.sessions.len() {
                        Self::tombstone_session(ws, si);
                        ws.sessions.remove(si);
                        if ws.sessions.is_empty() {
                            self.cursor = Cursor::Workspace(wi);
                        } else {
                            let new_si = si.min(ws.sessions.len() - 1);
                            self.cursor = Cursor::Session(wi, new_si);
                        }
                        self.save_session_manifest();
                        self.set_status_msg("Session closed");
                    }
                }
            }
            Cursor::Workspace(wi) => {
                if let Some(ws) = self.workspaces.get_mut(wi) {
                    if ws.sessions.len() == 1 {
                        Self::tombstone_session(ws, 0);
                        ws.sessions.remove(0);
                        self.cursor = Cursor::Workspace(wi);
                        self.save_session_manifest();
                        self.set_status_msg("Session closed");
                    }
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                // Close every session belonging to the task. The task remains
                // in the sidebar (as an empty subheader) until A-x removes it.
                // Tombstone each so `read_session_output` keeps working.
                if ws_idx < self.workspaces.len() {
                    let target = task_id.clone();
                    let removed = self.tombstone_and_remove(ws_idx, |ts| {
                        ts.task_id.as_deref() == Some(target.as_str())
                    });
                    if removed > 0 {
                        // Helper already saved the manifest.
                        self.set_status_msg(&format!("Closed {} session(s)", removed));
                    }
                }
            }
        }
    }

    /// Create a fresh standalone workspace — A-n flow. No task binding.
    /// Load the named snapshot and materialize it into `worktree_path`'s
    /// expected on-disk locations. Returns the full `ClonedSession` so
    /// the caller can pass `transcript_id` as `resume_session_id` to
    /// `build_args` / `codex_args` AND, if a later step (build_args,
    /// spawn) fails, remove the cloned `transcript_path` to keep retries
    /// unblocked. On clone/load error, toasts and returns `None`.
    ///
    /// **Engine-asymmetric integration** (see ClonedSession rustdoc):
    /// - Claude Code: returned id IS the live transcript id; caller sets
    ///   `ts.transcript_id = Some(id)`, `pending_jsonl_files = None`.
    /// - Codex: returned id is a *resume-source* id only — `codex resume`
    ///   reads our seed file once, then mints a fresh rollout. Caller
    ///   leaves `ts.transcript_id = None` and primes
    ///   `pending_jsonl_files` AFTER this call so the detector picks up
    ///   the new rollout (not the seed file).
    fn clone_snapshot_for_spawn(
        &mut self,
        name: &str,
        engine: Engine,
        worktree_path: &Path,
    ) -> Option<agent_memory::ClonedSession> {
        let snap = match agent_memory::load(name) {
            Ok(s) => s,
            Err(e) => {
                self.set_status_msg(&format!("Snapshot load failed: {e}"));
                return None;
            }
        };
        if snap.manifest.engine != engine {
            // Picker filters by engine but a hand-crafted manifest or a
            // stale form could still reach this branch — refuse rather
            // than producing an incoherent clone.
            self.set_status_msg(
                "Snapshot engine doesn't match this session type",
            );
            return None;
        }
        match agent_memory::clone_into_session(&snap, worktree_path) {
            Ok(cloned) => Some(cloned),
            Err(e) => {
                self.set_status_msg(&format!("Snapshot clone failed: {e}"));
                None
            }
        }
    }

    /// Undo a snapshot clone when a later step (build_args, PTY spawn)
    /// fails. Removes the transcript AND restores every merged memory
    /// file to its pre-clone state — otherwise a subsequent unseeded
    /// Claude session in the same worktree would silently inherit the
    /// snapshot's memory entries (the merge wrote them to disk and only
    /// the transcript would have been removed by a partial cleanup).
    fn cleanup_failed_clone(cloned: &agent_memory::ClonedSession) {
        agent_memory::cleanup_clone(cloned);
    }

    fn create_local_session(
        &mut self,
        repo_url: &str,
        label: &str,
        start_branch: Option<&str>,
        idle_timeout_secs: u16,
        seed_from: Option<&str>,
    ) {
        let main_repo = match worktree::find_local_repo(repo_url) {
            Some(p) => p,
            None => {
                self.set_status_msg("Repo not found locally");
                return;
            }
        };

        let slug = worktree::slugify(label);
        if slug.is_empty() {
            self.set_status_msg("Invalid name");
            return;
        }

        // Fail-fast on snapshot load BEFORE touching git. Without this,
        // a non-existent / corrupt snapshot would leave a freshly-created
        // worktree + branch orphaned on disk, and a retry would fail
        // because the worktree path is taken. The later
        // `clone_snapshot_for_spawn` re-validates (load is idempotent
        // and cheap) — this early check just keeps git off the failure
        // path.
        if let Some(name) = seed_from {
            if let Err(e) = validate_seed_loadable(name) {
                self.set_status_msg(&e);
                return;
            }
        }

        let worktree_path = match worktree::create_worktree(&main_repo, &slug, start_branch) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_msg(&format!("Worktree: {}", e));
                return;
            }
        };
        worktree::setup_worktree(&main_repo, &worktree_path);

        // If the user picked a snapshot, materialize it into the new
        // worktree's expected paths before we spawn. The returned id is
        // what `claude --resume <id>` reads — for Claude this is also
        // the live transcript id (post-resume Claude keeps writing to
        // the same file), so we set it directly on `ts` below.
        let cloned: Option<agent_memory::ClonedSession> = match seed_from {
            Some(name) => match self.clone_snapshot_for_spawn(
                name,
                Engine::ClaudeCode,
                &worktree_path,
            ) {
                Some(c) => Some(c),
                None => return, // clone failure already toasted
            },
            None => None,
        };

        let (cols, rows) = self.last_term_size;
        // Generate uid first so the MCP config carries the matching
        // CM_TUI_SESSION_ID. A-n sessions are taskless — pass None for
        // workflow meta.
        let session_uid = new_session_uid();
        let cloned_transcript_id = cloned.as_ref().map(|c| c.transcript_id.clone());
        let (program, args) = match crate::mcp_config::build_args(
            &workflow::toml_schema::Engine::ClaudeCode,
            &session_uid,
            None,
            cloned_transcript_id.as_deref(),
        ) {
            Ok(v) => v,
            Err(e) => {
                if let Some(c) = cloned.as_ref() {
                    // A seeded launch CANNOT fall back to plain `claude`
                    // (no `--resume`) — that would leave the TUI bound
                    // to the seed transcript while the live agent runs
                    // with none of the seeded context. Cleanup + fail.
                    Self::cleanup_failed_clone(c);
                    self.set_status_msg(&format!(
                        "Seeded launch aborted (could not configure agent): {e}"
                    ));
                    return;
                }
                // Unseeded fallback preserved — original behavior when
                // MCP config writing fails (agent runs without MCP).
                (
                    "claude".to_string(),
                    vec!["--dangerously-skip-permissions".to_string()],
                )
            }
        };
        // For a seeded Claude session, the JSONL is already on disk and
        // `--resume` keeps writing to it — there's no "new file" for the
        // detector to find, so leave `pending_jsonl_files = None`
        // (matches the resumed-Claude pattern at app.rs:5512).
        let pending = if cloned.is_some() {
            None
        } else {
            Some(Self::list_jsonl_files(&worktree_path))
        };

        let s = match self.spawn_agent_session(
            "claude",
            &session_uid,
            &program,
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        ) {
            Ok(s) => s,
            Err(_) => {
                if let Some(c) = cloned.as_ref() {
                    Self::cleanup_failed_clone(c);
                }
                self.set_status_msg("Spawn failed");
                return;
            }
        };

        let ts = TerminalSession {
            uid: session_uid,
            label: "claude".to_string(),
            session_type: "claude".to_string(),
            session: s,
            status: SessionStatus::Running,
            last_write_at: None,
            transcript_id: cloned_transcript_id.clone(),
            generation: 0,
            pending_jsonl_files: pending,
            hidden: false,
            idle_timeout_secs,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            last_delivery: None,
            task_id: None,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: seed_from.map(str::to_string),
        };
        let ws = Workspace {
            id: new_workspace_id(),
            name: label.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: Some(repo_url.to_string()),
            worktree_path: Some(worktree_path),
            main_repo_path: Some(main_repo),
            worker_vm: None,
            worker_zone: None,
            sessions: vec![ts],
            tombstones: Vec::new(),
            is_pushing: false,
        };
        let new_wi = self.workspaces.len();
        self.workspaces.push(ws);
        self.cursor = Cursor::Session(new_wi, 0);
        self.save_session_manifest();
        self.set_status_msg("Workspace created");
    }

    /// Attach to the active workspace (SSH for cloud, claude for local, bash fallback).
    fn attach_active(&mut self) {
        let wi = match self.active_workspace_index() {
            Some(wi) => wi,
            None => return,
        };
        let (cols, rows) = self.last_term_size;
        let ws = &self.workspaces[wi];

        if !ws.sessions.is_empty() {
            self.set_status_msg("Workspace already has sessions");
            return;
        }
        if ws.is_cloud && ws.worker_vm.is_none() {
            self.set_status_msg("Waiting for cloud VM assignment...");
            return;
        }

        let ts = if let Some(vm) = ws.worker_vm.clone().filter(|s| !s.is_empty()) {
            let zone = ws
                .worker_zone
                .clone()
                .unwrap_or_else(|| self.config.gcp_zone.clone());
            let args = vec![
                "compute".to_string(),
                "ssh".to_string(),
                vm,
                format!("--zone={}", zone),
                format!("--project={}", self.config.gcp_project),
                "--".to_string(),
                "-t".to_string(),
                "TERM=xterm-256color sudo su - worker -c 'tmux attach -t claude'".to_string(),
            ];
            Session::new("gcloud", &args, cols, rows, None, Default::default(), None)
                .ok()
                .map(|s| make_simple_session("ssh", "bash", s, None))
        } else if let Some(wt) = ws.worktree_path.clone() {
            let session_uid = new_session_uid();
            let (program, args) = crate::mcp_config::build_args(
                &workflow::toml_schema::Engine::ClaudeCode,
                &session_uid,
                None,
                None,
            )
            .unwrap_or_else(|_| (
                "claude".to_string(),
                vec!["--dangerously-skip-permissions".to_string()],
            ));
            let pending = Self::list_jsonl_files(&wt);
            self.spawn_agent_session(
                "claude",
                &session_uid,
                &program,
                &args,
                cols,
                rows,
                Some(wt),
                Default::default(),
            )
            .ok()
            .map(|s| make_simple_session_with_uid(session_uid, "claude", "claude", s, Some(pending)))
        } else {
            Session::new("/bin/bash", &[], cols, rows, None, Default::default(), None)
                .ok()
                .map(|s| make_simple_session("bash", "bash", s, None))
        };

        if let Some(ts) = ts {
            let si = self.workspaces[wi].sessions.len();
            self.workspaces[wi].sessions.push(ts);
            self.cursor = Cursor::Session(wi, si);
        }
    }

    /// Spawn a session on an existing workspace by type ("claude" / "codex" / "bash").
    /// If `task_id` is Some, the new session is tagged with that task so it
    /// appears under the corresponding task subheader.
    fn spawn_session_on_workspace(
        &mut self,
        workspace_id: &str,
        session_type: &str,
        task_id: Option<String>,
        seed_from: Option<&str>,
    ) {
        // Resolve workspace_id → current index. If the workspace
        // disappeared while the form was open (delete, reconcile drop,
        // etc.), bail cleanly rather than spawning into an unrelated
        // workspace at whatever happens to sit at the stale index now.
        let ws_index = match resolve_workspace_by_id(&self.workspaces, workspace_id) {
            Some(i) => i,
            None => {
                self.set_status_msg(
                    "Workspace no longer exists — session not started",
                );
                return;
            }
        };
        if self.workspaces[ws_index].is_cloud && self.workspaces[ws_index].worker_vm.is_none() {
            self.set_status_msg("Waiting for cloud VM assignment...");
            return;
        }

        let (cols, rows) = self.last_term_size;

        // Cloud workspace + bash session type → SSH into the VM.
        if let Some(vm) = self.workspaces[ws_index].worker_vm.clone().filter(|s| !s.is_empty()) {
            if session_type == "bash" {
                let zone = self.workspaces[ws_index]
                    .worker_zone
                    .clone()
                    .unwrap_or_else(|| self.config.gcp_zone.clone());
                let si = self.workspaces[ws_index].sessions.len();
                let tmux_name = format!("bash-{}", si);
                let args = vec![
                    "compute".to_string(),
                    "ssh".to_string(),
                    vm,
                    format!("--zone={}", zone),
                    format!("--project={}", self.config.gcp_project),
                    "--".to_string(),
                    "-t".to_string(),
                    format!(
                        "TERM=xterm-256color sudo su - worker -c 'cd /workspace && tmux new-session -As {}'",
                        tmux_name
                    ),
                ];
                match Session::new("gcloud", &args, cols, rows, None, Default::default(), None) {
                    Ok(s) => {
                        let mut ts = make_simple_session(&tmux_name, "bash", s, None);
                        ts.task_id = task_id.clone();
                        let si = self.workspaces[ws_index].sessions.len();
                        self.workspaces[ws_index].sessions.push(ts);
                        self.cursor = Cursor::Session(ws_index, si);
                        self.save_session_manifest();
                        self.set_status_msg("Started SSH bash session");
                    }
                    Err(e) => self.set_status_msg(&format!("Spawn: {}", e)),
                }
                return;
            }
        }

        let wt = self.workspaces[ws_index].worktree_path.clone();

        // Clone the seed snapshot BEFORE computing the baseline (Codex)
        // or building args (both engines). For Codex, the cloned seed
        // file must be in the baseline so the post-spawn detector picks
        // the new rollout id rather than rebinding to the seed file.
        let cloned: Option<agent_memory::ClonedSession> =
            match (seed_from, session_type, wt.as_ref()) {
                (Some(name), "claude", Some(p)) => {
                    match self.clone_snapshot_for_spawn(name, Engine::ClaudeCode, p) {
                        Some(c) => Some(c),
                        None => return,
                    }
                }
                (Some(name), "codex", Some(p)) => {
                    match self.clone_snapshot_for_spawn(name, Engine::Codex, p) {
                        Some(c) => Some(c),
                        None => return,
                    }
                }
                (Some(_), _, _) => {
                    // Bash or no worktree — seed_from is meaningless.
                    // The form prevents this combination but defend
                    // against it.
                    self.set_status_msg(
                        "Snapshots only apply to claude / codex sessions",
                    );
                    return;
                }
                (None, _, _) => None,
            };

        // For Claude with a clone, the JSONL is already on disk and
        // --resume keeps writing to it, so pending_jsonl_files = None
        // (detector path isn't used). For Codex, baseline is taken AFTER
        // the clone so the seed file is excluded and the detector picks
        // the freshly-minted rollout id post-resume.
        let pending = match (session_type, cloned.is_some()) {
            ("claude", true) => None,
            ("claude", false) => wt.as_ref().map(|p| Self::list_jsonl_files(p)),
            ("codex", _) => wt.as_ref().map(|p| Self::list_codex_sessions(p)),
            _ => None,
        };
        // Pre-generate uid so MCP env carries the same CM_TUI_SESSION_ID
        // the TerminalSession will hold. Sessions added on a workspace
        // are taskless from MCP's POV (they inherit a task_id below for
        // sidebar grouping but no workflow context).
        let session_uid_pre = new_session_uid();
        let cloned_transcript_id = cloned.as_ref().map(|c| c.transcript_id.clone());

        // Build args, refusing to fall back to plain claude/codex
        // (without `--resume`/`resume`) for seeded launches — the
        // resume flag IS the wiring that connects the seed transcript
        // to the live agent. Without it the TUI binds to the seed file
        // while the agent has none of the context.
        let build = |engine: Engine, fallback_prog: &str, fallback_args: Vec<String>| {
            crate::mcp_config::build_args(
                &engine,
                &session_uid_pre,
                None,
                cloned_transcript_id.as_deref(),
            )
            .or_else(|e| {
                if cloned.is_some() {
                    Err(e)
                } else {
                    Ok((fallback_prog.to_string(), fallback_args))
                }
            })
        };
        let result = match session_type {
            "claude" => match build(
                Engine::ClaudeCode,
                "claude",
                vec!["--dangerously-skip-permissions".to_string()],
            ) {
                Ok((program, args)) => self.spawn_agent_session(
                    "claude",
                    &session_uid_pre,
                    &program,
                    &args,
                    cols,
                    rows,
                    wt,
                    Default::default(),
                ),
                Err(e) => {
                    if let Some(c) = cloned.as_ref() {
                        Self::cleanup_failed_clone(c);
                    }
                    self.set_status_msg(&format!(
                        "Seeded launch aborted (could not configure agent): {e}"
                    ));
                    return;
                }
            },
            "codex" => match build(
                Engine::Codex,
                "codex",
                vec!["--yolo".to_string()],
            ) {
                Ok((program, args)) => self.spawn_agent_session(
                    "codex",
                    &session_uid_pre,
                    &program,
                    &args,
                    cols,
                    rows,
                    wt,
                    Default::default(),
                ),
                Err(e) => {
                    if let Some(c) = cloned.as_ref() {
                        Self::cleanup_failed_clone(c);
                    }
                    self.set_status_msg(&format!(
                        "Seeded launch aborted (could not configure agent): {e}"
                    ));
                    return;
                }
            },
            _ => Session::new("/bin/bash", &[], cols, rows, wt, Default::default(), None),
        };
        match result {
            Ok(s) => {
                // Use the same uid we baked into MCP env for claude/codex.
                // bash sessions don't have MCP config and the uid is just
                // for sidebar tracking — but we still use the pre-gen one
                // for consistency.
                let mut ts = make_simple_session_with_uid(
                    session_uid_pre,
                    session_type,
                    session_type,
                    s,
                    pending,
                );
                ts.task_id = task_id;
                ts.seeded_from_snapshot = seed_from.map(str::to_string);
                // Engine-asymmetric transcript_id wiring — see
                // `ClonedSession` rustdoc. For Claude the cloned id IS
                // the live transcript id; for Codex it's a seed-file id
                // and the live id is filled in by detection.
                if session_type == "claude" {
                    ts.transcript_id = cloned_transcript_id;
                }
                let si = self.workspaces[ws_index].sessions.len();
                self.workspaces[ws_index].sessions.push(ts);
                self.cursor = Cursor::Session(ws_index, si);
                self.save_session_manifest();
                self.set_status_msg(&format!("Started {} session", session_type));
            }
            Err(e) => {
                if let Some(c) = cloned.as_ref() {
                    Self::cleanup_failed_clone(c);
                }
                self.set_status_msg(&format!("Spawn: {}", e));
            }
        }
    }

    /// Spawn a local claude --resume session after a pull completes.
    fn spawn_resumed_session(
        &mut self,
        task_id: Option<String>,
        worktree_path: PathBuf,
        main_repo: PathBuf,
        session_id: String,
        repo_url: String,
        prompt: String,
    ) {
        let (cols, rows) = self.last_term_size;
        // Pre-generate the session UID so the per-session MCP config
        // bakes the matching CM_TUI_SESSION_ID. Without this, a pulled
        // session can spawn but its agent has no MCP config and any
        // tool call would fail auth as `not_found`.
        let session_uid = new_session_uid();
        let (program, args) = match crate::mcp_config::build_args(
            &workflow::toml_schema::Engine::ClaudeCode,
            &session_uid,
            None,
            Some(session_id.as_str()),
        ) {
            Ok(v) => v,
            Err(_) => (
                "claude".to_string(),
                vec![
                    "--dangerously-skip-permissions".to_string(),
                    "--resume".to_string(),
                    session_id.clone(),
                ],
            ),
        };

        match self.spawn_agent_session(
            "claude",
            &session_uid,
            &program,
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        ) {
            Ok(s) => {
                let mut ts = make_simple_session_with_uid(
                    session_uid,
                    "claude",
                    "claude",
                    s,
                    None,
                );
                ts.transcript_id = Some(session_id.clone());
                ts.task_id = task_id.clone();

                // If we have a task_id, find the TaskEntry and its (cloud)
                // workspace; replace that workspace with a local one.
                let target_ti = task_id
                    .as_ref()
                    .and_then(|id| {
                        self.tasks
                            .iter()
                            .position(|t| t.task_id.as_deref() == Some(id))
                    });

                let local_ws = Workspace {
                    id: new_workspace_id(),
                    name: task_id
                        .as_deref()
                        .and_then(|id| {
                            self.tasks
                                .iter()
                                .find(|t| t.task_id.as_deref() == Some(id))
                                .map(|t| t.name.clone())
                        })
                        .unwrap_or_else(|| prompt.chars().take(60).collect()),
                    is_closed: false,
                    is_cloud: false,
                    repo_url: Some(repo_url.clone()),
                    worktree_path: Some(worktree_path.clone()),
                    main_repo_path: Some(main_repo.clone()),
                    worker_vm: None,
                    worker_zone: None,
                    sessions: vec![ts],
                    tombstones: Vec::new(),
                    is_pushing: false,
                };
                let ws_id = local_ws.id.clone();

                if let Some(ti) = target_ti {
                    // Remove the old (cloud) workspace if one was linked.
                    if let Some(old_id) = self.tasks[ti].workspace_id.clone() {
                        self.workspaces.retain(|w| w.id != old_id);
                    }
                    self.tasks[ti].is_cloud = false;
                    self.tasks[ti].session_id = Some(session_id);
                    self.tasks[ti].workspace_id = Some(ws_id.clone());
                } else {
                    // No matching task — create one.
                    self.tasks.push(TaskEntry {
                        task_id,
                        name: local_ws.name.clone(),
                        api_status: TaskStatus::Running,
                        repo_url: Some(repo_url),
                        prompt: Some(prompt),
                        wip_branch: None,
                        session_id: Some(session_id),
                        blocked_at: None,
                        is_cloud: false,
                        workspace_id: Some(ws_id.clone()),
                        project: None,
                        parent_task_id: None,
                        worktree_mode: WorktreeMode::Inherit,
                    });
                }
                self.workspaces.push(local_ws);
                let new_wi = self.workspaces.len() - 1;
                self.cursor = Cursor::Session(new_wi, 0);
                self.save_session_manifest();
                self.set_status_msg("Resumed locally");
            }
            Err(e) => {
                self.set_status_msg(&format!("Resume failed: {}", e));
            }
        }
    }

    /// Mark the first task bound to the active workspace as done via the API.
    /// Does nothing if the workspace has zero or multiple bound tasks (ambiguous).
    fn mark_active_done(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };

        // Task-scoped: if the cursor is on a task header, or on a session
        // tagged with a task, mark THAT task done (and close only its
        // sessions). If the cursor is workspace-scoped, fall back to the
        // old "single bound task" logic.
        let scoped_tid = self.cursor_task_id();

        let ws_id = self.workspaces[wi].id.clone();
        let tid = match scoped_tid {
            Some(t) => t,
            None => {
                let bound: Vec<String> = self
                    .tasks
                    .iter()
                    .filter(|t| t.workspace_id.as_deref() == Some(&ws_id))
                    .filter_map(|t| t.task_id.clone())
                    .collect();
                if bound.len() > 1 {
                    self.set_status_msg("Multiple tasks bound — pick one (A-d on its header)");
                    return;
                }
                match bound.into_iter().next() {
                    Some(t) => t,
                    None => {
                        // No task — soft-close every session in the
                        // workspace. Tombstone each so the resolver
                        // can still answer `read_session_output`.
                        // Helper persists the manifest internally.
                        self.tombstone_and_remove(wi, |_| true);
                        self.cursor = Cursor::Workspace(wi);
                        self.clamp_cursor();
                        self.set_status_msg("Cleared sessions");
                        return;
                    }
                }
            }
        };

        let mut fields = HashMap::new();
        fields.insert(
            "status".to_string(),
            serde_json::Value::String("done".to_string()),
        );
        self.backend.update_task(tid.clone(), fields);
        self.planning.mark_task_done_by_id(&tid);
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(&tid))
        {
            task.api_status = TaskStatus::Done;
        }
        // Drop sessions tagged with this task. Other task-scoped and
        // workspace-level sessions in the same workspace stay running.
        // Tombstone each so post-done `read_session_output` keeps working.
        // Helper persists the manifest before returning.
        let target = tid.clone();
        self.tombstone_and_remove(wi, |ts| {
            ts.task_id.as_deref() == Some(target.as_str())
        });
        self.cursor = Cursor::Workspace(wi);
        self.clamp_cursor();
        self.set_status_msg("Marked done");
    }

    /// Delete whatever the cursor resolves to:
    ///   - Cursor::Task → delete just that task (close its sessions, remove
    ///     from backend + local TaskEntry). The workspace, worktree, and any
    ///     other tasks / workspace-level sessions survive.
    ///   - Cursor::Session on a task-tagged session → same as Cursor::Task.
    ///   - Otherwise → delete the whole workspace: close sessions, remove
    ///     worktree + branch, delete any bound tasks from the API.
    fn delete_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };

        // Task-scoped delete path.
        if let Some(tid) = self.cursor_task_id() {
            // Tombstone-then-drop the task's sessions so the resolver
            // can still answer for them post-delete. Helper persists.
            let target = tid.clone();
            self.tombstone_and_remove(wi, |ts| {
                ts.task_id.as_deref() == Some(target.as_str())
            });
            self.backend.delete_task(tid.clone());
            self.tasks.retain(|t| t.task_id.as_deref() != Some(tid.as_str()));
            self.cursor = Cursor::Workspace(wi);
            self.clamp_cursor();
            self.set_status_msg("Task deleted");
            self.save_session_manifest();
            return;
        }

        let ws_id = self.workspaces[wi].id.clone();
        let worktree_path = self.workspaces[wi].worktree_path.clone();
        let main_repo_path = self.workspaces[wi].main_repo_path.clone();
        let bound_task_ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(&ws_id))
            .filter_map(|t| t.task_id.clone())
            .collect();

        // Determine the branch to delete from any bound task's wip_branch.
        let wip_branch = self
            .tasks
            .iter()
            .find(|t| t.workspace_id.as_deref() == Some(&ws_id))
            .and_then(|t| t.wip_branch.clone());

        // Worktree removal is the gate to the rest of the destructive
        // cleanup. If `git worktree remove` fails, branches and API
        // tasks should NOT get deleted — better to leave the user
        // with a recoverable state (worktree still on disk, branches
        // intact, API rows intact) than to half-cleanup. The status
        // bar shows the git error so the user knows what's wrong.
        if let (Some(ref wt), Some(ref repo)) = (&worktree_path, &main_repo_path) {
            if let Err(e) = worktree::remove_worktree(repo, wt) {
                self.set_status_msg(&format!(
                    "Workspace delete aborted: git worktree remove failed: {}",
                    e
                ));
                return;
            }
        }
        if let (Some(ref branch), Some(ref repo)) = (&wip_branch, &main_repo_path) {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["branch", "-D", branch])
                .output();
            if !bound_task_ids.is_empty() {
                let _ = std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(["push", "origin", "--delete", branch])
                    .output();
            }
        }

        for tid in &bound_task_ids {
            self.backend.delete_task(tid.clone());
        }
        self.tasks.retain(|t| !bound_task_ids.iter().any(|id| t.task_id.as_deref() == Some(id)));
        self.workspaces.remove(wi);
        self.cursor = Cursor::Workspace(wi.min(self.workspaces.len().saturating_sub(1)));
        self.set_status_msg("Deleted");
    }

    /// Push the active local workspace to the cloud. If a task is bound to
    /// the workspace, its id is included so the cloud side can reuse it;
    /// otherwise a new cloud task is created from the workspace's name.
    ///
    /// **Invariant**: this function does NOT mutate local workspace state
    /// (no tombstones, no clearing `worktree_path`, no flipping
    /// `is_cloud`). All destructive cleanup is deferred to
    /// `BackendEvent::PushComplete` in `drain_backend_events`. A failed
    /// push (`PushFailed`) just clears `is_pushing` and surfaces the
    /// error — the user can retry without reconstructing the worktree.
    fn push_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        if self.workspaces[wi].is_cloud {
            self.set_status_msg("Can only push local workspaces");
            return;
        }
        if self.workspaces[wi].is_pushing {
            self.set_status_msg("Push already in progress");
            return;
        }
        let worktree_path = match &self.workspaces[wi].worktree_path {
            Some(p) => p.clone(),
            None => {
                self.set_status_msg("No worktree to push");
                return;
            }
        };
        let repo_url = match &self.workspaces[wi].repo_url {
            Some(u) => u.clone(),
            None => {
                self.set_status_msg("No repo URL");
                return;
            }
        };
        let ws_id = self.workspaces[wi].id.clone();
        let ws_name = self.workspaces[wi].name.clone();
        let first = self.first_task_for_ws(&ws_id);
        let name = first.and_then(|t| t.prompt.clone()).unwrap_or(ws_name);
        let task_id = first.and_then(|t| t.task_id.clone());

        self.workspaces[wi].is_pushing = true;
        self.backend.push(worktree_path, repo_url, name, task_id, ws_id);
        self.cursor = Cursor::Workspace(wi);
        self.set_status_msg("Pushing to cloud...");
    }

    /// Apply the destructive local-cleanup half of a push, gated on a
    /// `PushComplete` event from the backend. Tombstones live sessions,
    /// drops `worktree_path`, flips `is_cloud` on the workspace and any
    /// bound task, and persists the new state. If `cloud_task_id` was
    /// returned (always set for now, but kept Optional in the event),
    /// no extra task binding work is done — `do_refresh` will pull the
    /// authoritative cloud row in the next refresh tick.
    fn finish_push(&mut self, workspace_id: &str, _cloud_task_id: Option<String>) {
        let Some(wi) = self.workspaces.iter().position(|w| w.id == workspace_id) else {
            return;
        };
        // Tombstone first — the helper saves the manifest with each
        // tombstone's `worktree_path` snapshotted at the current value.
        // We then mutate workspace + task state and save AGAIN so the
        // post-push state (no worktree, is_cloud=true) is durable too.
        // Without the second save, a crash here would leave the manifest
        // with valid tombstones but the workspace still flagged local
        // with a stale `worktree_path` — the worst kind of half-state
        // because it looks valid on restart.
        self.tombstone_and_remove(wi, |_| true);
        let ws_id = self.workspaces[wi].id.clone();
        self.workspaces[wi].worktree_path = None;
        self.workspaces[wi].is_cloud = true;
        self.workspaces[wi].is_pushing = false;
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.workspace_id.as_deref() == Some(&ws_id))
        {
            task.is_cloud = true;
        }
        self.save_session_manifest();
    }

    /// Pull the active cloud workspace to local (uses the first bound task).
    fn pull_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        let ws_id = self.workspaces[wi].id.clone();
        let Some(task) = self
            .tasks
            .iter()
            .find(|t| t.workspace_id.as_deref() == Some(&ws_id))
        else {
            self.set_status_msg("No task bound to pull");
            return;
        };
        let task_id = match task.task_id.clone() {
            Some(id) => id,
            None => {
                self.set_status_msg("Task has no id");
                return;
            }
        };
        let repo_url = match task.repo_url.clone() {
            Some(u) => u,
            None => {
                self.set_status_msg("No repo URL on task");
                return;
            }
        };
        let main_repo = match worktree::find_local_repo(&repo_url) {
            Some(p) => p,
            None => {
                self.set_status_msg("Repo not found locally");
                return;
            }
        };
        self.backend.pull(task_id, main_repo);
        self.set_status_msg("Pulling to local...");
    }

    /// Launch a task from the planning view.
    fn launch_from_plan(
        &mut self,
        project: &str,
        slug: &str,
        prompt: &str,
        start_branch: Option<&str>,
        _autostart: bool,
        task_id: &str,
    ) {
        let repo_url = match self.config.repos.get(project) {
            Some(url) => url.clone(),
            None => {
                self.set_status_msg(&format!("No repo configured for '{}'", project));
                return;
            }
        };

        let main_repo = match worktree::find_local_repo(&repo_url) {
            Some(p) => p,
            None => {
                self.set_status_msg("Repo not found locally");
                return;
            }
        };

        let worktree_path = match worktree::create_worktree(&main_repo, slug, start_branch) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_msg(&format!("Worktree: {}", e));
                return;
            }
        };

        worktree::setup_worktree(&main_repo, &worktree_path);

        let (cols, rows) = self.last_term_size;
        // Pre-generate UID + route through the shared MCP config helper
        // so the planning-launched agent gets `--mcp-config` + matching
        // CM_TUI_SESSION_ID — the Phase 1 "MCP-everywhere" invariant.
        let session_uid = new_session_uid();
        let (program, args) = crate::mcp_config::build_args(
            &workflow::toml_schema::Engine::ClaudeCode,
            &session_uid,
            None,
            None,
        )
        .unwrap_or_else(|_| (
            "claude".to_string(),
            vec!["--dangerously-skip-permissions".to_string()],
        ));
        let pending = Self::list_jsonl_files(&worktree_path);

        match self.spawn_agent_session(
            "claude",
            &session_uid,
            &program,
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        ) {
            Ok(s) => {
                let branch = format!("cm/{}", slug);
                let mut ts = make_simple_session_with_uid(
                    session_uid,
                    slug,
                    "claude",
                    s,
                    Some(pending),
                );
                ts.task_id = Some(task_id.to_string());
                if !prompt.trim().is_empty() {
                    ts.pending_prompt = Some(PendingWrite::wait_for_quiet(
                        prompt.to_string(),
                        false,
                        Duration::from_secs(1),
                        Duration::from_secs(2),
                        Duration::from_secs(60),
                    ));
                }

                let ws = Workspace {
                    id: new_workspace_id(),
                    name: slug.to_string(),
                    is_closed: false,
                    is_cloud: false,
                    repo_url: Some(repo_url.clone()),
                    worktree_path: Some(worktree_path),
                    main_repo_path: Some(main_repo),
                    worker_vm: None,
                    worker_zone: None,
                    sessions: vec![ts],
                    tombstones: Vec::new(),
                    is_pushing: false,
                };
                let ws_id = ws.id.clone();
                self.workspaces.push(ws);
                let new_wi = self.workspaces.len() - 1;

                self.tasks.push(TaskEntry {
                    task_id: Some(task_id.to_string()),
                    name: slug.to_string(),
                    api_status: TaskStatus::Running,
                    repo_url: Some(repo_url.clone()),
                    prompt: Some(prompt.to_string()),
                    wip_branch: Some(branch.clone()),
                    session_id: None,
                    blocked_at: None,
                    is_cloud: false,
                    workspace_id: Some(ws_id),
                    // Pin project synchronously so subtask inheritance
                    // works before the next reconcile pass — without
                    // this, an agent calling `create_subtask` in the
                    // first second sees `project: None` on the parent
                    // and writes `project = NULL` to the API, which
                    // the planning refresh then filters out.
                    project: Some(project.to_string()),
                    parent_task_id: None,
                    worktree_mode: WorktreeMode::Inherit,
                });

                self.cursor = Cursor::Session(new_wi, 0);
                self.view_mode = ViewMode::Sessions;

                let mut fields = std::collections::HashMap::new();
                fields.insert("status".to_string(), serde_json::Value::String("running".to_string()));
                fields.insert("wip_branch".to_string(), serde_json::Value::String(branch));
                self.backend.update_plan_task(task_id.to_string(), fields);
                self.save_session_manifest();
                self.set_status_msg("Task launched");
            }
            Err(e) => {
                self.set_status_msg(&format!("Launch: {}", e));
            }
        }
    }

    /// Open workspaces the planning picker can target. Skips closed workspaces
    /// and cloud workspaces (those have no worktree to share).
    fn collect_workspace_candidates(&self) -> Vec<WorkspaceCandidate> {
        self.workspaces
            .iter()
            .filter(|w| !w.is_closed && w.worktree_path.is_some())
            .map(|w| WorkspaceCandidate {
                workspace_id: w.id.clone(),
                name: w.name.clone(),
                repo_url: w.repo_url.clone(),
            })
            .collect()
    }

    /// Spawn a new Claude session in an existing workspace and bind the
    /// given task to it. No new worktree — the workspace already has one.
    fn launch_into_workspace(
        &mut self,
        workspace_id: &str,
        task_id: &str,
        task_title: &str,
        task_repo_url: &str,
        project: &str,
        prompt: &str,
    ) {
        let Some(wi) = self.workspace_index_by_id(workspace_id) else {
            self.set_status_msg("Workspace no longer exists");
            return;
        };
        let Some(worktree_path) = self.workspaces[wi].worktree_path.clone() else {
            self.set_status_msg("Workspace has no worktree");
            return;
        };

        let (cols, rows) = self.last_term_size;
        // Pre-generate UID so the per-session MCP config carries the
        // matching CM_TUI_SESSION_ID. Phase 1 "MCP-everywhere" — without
        // this, a session launched into an existing workspace can't call
        // any orchestration tool.
        let session_uid = new_session_uid();
        let (program, args) = crate::mcp_config::build_args(
            &workflow::toml_schema::Engine::ClaudeCode,
            &session_uid,
            None,
            None,
        )
        .unwrap_or_else(|_| (
            "claude".to_string(),
            vec!["--dangerously-skip-permissions".to_string()],
        ));
        let pending = Self::list_jsonl_files(&worktree_path);
        match self.spawn_agent_session(
            "claude",
            &session_uid,
            &program,
            &args,
            cols,
            rows,
            Some(worktree_path),
            Default::default(),
        ) {
            Ok(s) => {
                let mut ts = make_simple_session_with_uid(
                    session_uid,
                    task_title,
                    "claude",
                    s,
                    Some(pending),
                );
                ts.task_id = Some(task_id.to_string());
                if !prompt.trim().is_empty() {
                    ts.pending_prompt = Some(PendingWrite::wait_for_quiet(
                        prompt.to_string(),
                        false,
                        Duration::from_secs(1),
                        Duration::from_secs(2),
                        Duration::from_secs(60),
                    ));
                }
                let si = self.workspaces[wi].sessions.len();
                self.workspaces[wi].sessions.push(ts);

                // The task may be in backlog (not yet in self.tasks because
                // reconcile only pulls running/blocked). Upsert a stub with
                // the workspace binding set; a later reconcile will fill in
                // the remaining API fields without clobbering workspace_id.
                if let Some(task) = self
                    .tasks
                    .iter_mut()
                    .find(|t| t.task_id.as_deref() == Some(task_id))
                {
                    task.workspace_id = Some(workspace_id.to_string());
                } else {
                    self.tasks.push(TaskEntry {
                        task_id: Some(task_id.to_string()),
                        name: task_title.to_string(),
                        api_status: TaskStatus::Running,
                        repo_url: Some(task_repo_url.to_string()),
                        prompt: Some(prompt.to_string()),
                        wip_branch: None,
                        session_id: None,
                        blocked_at: None,
                        is_cloud: false,
                        workspace_id: Some(workspace_id.to_string()),
                        // Same race fix as `launch_from_plan` — pin the
                        // project synchronously from the planning row so
                        // an early `create_subtask` inherits it.
                        project: Some(project.to_string()),
                        parent_task_id: None,
                        worktree_mode: WorktreeMode::Inherit,
                    });
                }
                self.cursor = Cursor::Session(wi, si);
                self.view_mode = ViewMode::Sessions;

                let mut fields = std::collections::HashMap::new();
                fields.insert(
                    "status".to_string(),
                    serde_json::Value::String("running".to_string()),
                );
                self.backend
                    .update_plan_task(task_id.to_string(), fields);
                self.save_session_manifest();
                self.set_status_msg("Task launched into workspace");
            }
            Err(e) => {
                self.set_status_msg(&format!("Launch: {}", e));
            }
        }
    }

    /// Clear a task's workspace binding. Task status is left alone.
    fn unbind_task_from_workspace(&mut self, task_id: &str) {
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))
        {
            if task.workspace_id.is_some() {
                task.workspace_id = None;
                self.save_session_manifest();
                self.set_status_msg("Task unbound from workspace");
            }
        }
    }

    /// Planning-view counterpart to `reopen_active_workspace`. Resolves the
    /// task's worktree via either its existing workspace binding, an in-memory
    /// workspace matching by recovered path, or a filesystem scan. Refuses
    /// when the worktree directory is gone. Otherwise PATCHes the task back
    /// to `running`, un-archives (or provisions) the workspace, switches to
    /// the Sessions view, and leaves the cursor on the reopened workspace.
    fn reopen_task_from_planning(&mut self, task_id: &str) {
        let entry = self
            .tasks
            .iter()
            .find(|t| t.task_id.as_deref() == Some(task_id));
        let (repo_url, wip_branch, workspace_id, is_cloud, name) = match entry {
            Some(t) => (
                t.repo_url.clone(),
                t.wip_branch.clone(),
                t.workspace_id.clone(),
                t.is_cloud,
                t.name.clone(),
            ),
            None => {
                self.set_status_msg("Task not found locally");
                return;
            }
        };
        if is_cloud {
            self.set_status_msg("Cloud tasks aren't reopenable from past");
            return;
        }

        // Locate an existing workspace: by id, else by recovered worktree_path.
        let recovered_path = wip_branch
            .as_deref()
            .zip(repo_url.as_deref())
            .and_then(|(b, r)| worktree::recover_worktree_path(r, b));
        let mut wi: Option<usize> = workspace_id
            .as_deref()
            .and_then(|id| self.workspaces.iter().position(|w| w.id == id));
        if wi.is_none() {
            if let Some(ref wt) = recovered_path {
                wi = self
                    .workspaces
                    .iter()
                    .position(|w| w.worktree_path.as_deref() == Some(wt.as_path()));
            }
        }

        // Resolve the worktree path we'll validate. Prefer the existing
        // workspace's path (might differ from recovered if branch was renamed),
        // fall back to filesystem recovery.
        let worktree_path = wi
            .and_then(|i| self.workspaces[i].worktree_path.clone())
            .or(recovered_path);
        let worktree_path = match worktree_path {
            Some(p) if p.exists() => p,
            Some(p) => {
                self.set_status_msg(&format!(
                    "Worktree gone: {} — task can't be reopened",
                    p.display()
                ));
                return;
            }
            None => {
                self.set_status_msg("Worktree not found on disk — task can't be reopened");
                return;
            }
        };

        // PATCH task → running and update optimistic in-memory state.
        let mut fields = HashMap::new();
        fields.insert(
            "status".to_string(),
            serde_json::Value::String("running".to_string()),
        );
        self.backend.update_task(task_id.to_string(), fields);
        if let Some(entry) = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))
        {
            entry.api_status = TaskStatus::Running;
        }
        self.planning.mark_task_running_by_id(task_id);

        // Provision a workspace if none exists yet — the manifest may have
        // dropped the binding when the task last reconciled as done (the
        // reconcile loop skips non-running/blocked tasks).
        let final_wi = match wi {
            Some(i) => i,
            None => {
                let main_repo = repo_url.as_deref().and_then(worktree::find_local_repo);
                let ws = Workspace {
                    id: new_workspace_id(),
                    name,
                    is_closed: false,
                    is_cloud: false,
                    repo_url: repo_url.clone(),
                    worktree_path: Some(worktree_path),
                    main_repo_path: main_repo,
                    worker_vm: None,
                    worker_zone: None,
                    sessions: vec![],
                    tombstones: Vec::new(),
                    is_pushing: false,
                };
                let new_ws_id = ws.id.clone();
                self.workspaces.push(ws);
                let idx = self.workspaces.len() - 1;
                if let Some(entry) = self
                    .tasks
                    .iter_mut()
                    .find(|t| t.task_id.as_deref() == Some(task_id))
                {
                    entry.workspace_id = Some(new_ws_id);
                }
                idx
            }
        };

        self.workspaces[final_wi].is_closed = false;
        self.cursor = Cursor::Workspace(final_wi);
        self.save_session_manifest();
        self.view_mode = ViewMode::Sessions;
        self.clamp_cursor();
        self.set_status_msg("Reopened — A-s to add session");
    }

    fn unlaunch_task(&mut self, task_id: &str) {
        let mut fields = std::collections::HashMap::new();
        fields.insert("status".to_string(), serde_json::Value::String("backlog".to_string()));
        self.backend.update_plan_task(task_id.to_string(), fields);

        let ws_id = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))
            .and_then(|t| {
                t.api_status = TaskStatus::Backlog;
                t.workspace_id.take()
            });

        if let Some(ws_id) = ws_id {
            if let Some(wi) = self.workspaces.iter().position(|w| w.id == ws_id) {
                // Tombstone each session before dropping so post-unlaunch
                // `read_session_output` works for the closed sessions.
                // Helper persists the manifest internally.
                self.tombstone_and_remove(wi, |_| true);
                if let Some(ws) = self.workspaces.get_mut(wi) {
                    ws.is_closed = true;
                }
            }
        }
        self.save_session_manifest();
        self.clamp_cursor();
        self.set_status_msg("Task unlaunched \u{2192} backlog");
    }

    /// Handle terminal resize.
    pub fn resize_terminals(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
        for ws in &mut self.workspaces {
            for ts in &mut ws.sessions {
                ts.session.resize(cols, rows);
            }
        }
    }

    // ── Drawing ──────────────────────────────────────────────────────

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();

        // Phase 6: bottom layout — content / [activity strip] / status bar.
        // Activity strip renders only when toggled on (Alt-,) and we have
        // entries to show; fixed at 5 lines so it doesn't dominate the
        // screen but shows enough recent context to be useful.
        let activity_height: u16 = if self.activity_visible
            && !self.activity_log.is_empty()
            && area.height >= 8
        {
            5
        } else {
            0
        };
        let rows = if activity_height > 0 {
            Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(activity_height),
                Constraint::Length(1),
            ])
            .split(area)
        } else {
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area)
        };

        let content_area = rows[0];
        let bar_area = if activity_height > 0 { rows[2] } else { rows[1] };
        let activity_area = if activity_height > 0 { Some(rows[1]) } else { None };

        // Wipe the content area first so stale cells from a previous frame's
        // wider/taller widgets don't bleed through when a new panel renders
        // less content. Ratatui only diffs touched cells; without this, the
        // user sees artifacts in the gaps after switching views/panels (the
        // status bar is fully painted by draw_status_bar so it doesn't need
        // clearing here).
        frame.render_widget(Clear, content_area);

        match self.view_mode {
            ViewMode::Sessions => {
                let cols =
                    Layout::horizontal([Constraint::Min(40), Constraint::Length(SIDEBAR_WIDTH)])
                        .split(content_area);

                self.draw_terminal(frame, cols[0]);
                self.draw_session_list(frame, cols[1]);
            }
            ViewMode::Planning => {
                self.planning.draw(frame, content_area);
            }
        }

        if let Some(act_area) = activity_area {
            self.draw_activity_feed(frame, act_area);
        }
        self.draw_status_bar(frame, bar_area);

        // Draw input overlay if active (sessions mode only).
        if matches!(self.view_mode, ViewMode::Sessions) {
            match &self.input_mode {
                InputMode::NewSession {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    seed_from,
                    active_field,
                } => {
                    self.draw_input_dialog(
                        frame,
                        area,
                        label_text,
                        branch_text,
                        idle_timeout_text,
                        repo_url,
                        seed_from.as_deref(),
                        *active_field,
                    );
                }
                InputMode::NewTerminalSession {
                    workspace_id,
                    session_type,
                    seed_from,
                    active_field,
                    ..
                } => {
                    self.draw_new_terminal_dialog(
                        frame,
                        area,
                        workspace_id,
                        session_type,
                        seed_from.as_deref(),
                        *active_field,
                    );
                }
                InputMode::SessionSettings { name, idle_timeout, burst_threshold, hidden, notify_on_idle, seeded_from_snapshot, active_field, .. } => {
                    self.draw_session_settings(
                        frame,
                        area,
                        name,
                        idle_timeout,
                        burst_threshold,
                        *hidden,
                        *notify_on_idle,
                        seeded_from_snapshot.as_deref(),
                        *active_field,
                    );
                }
                InputMode::WorkspaceSettings { name, .. } => {
                    self.draw_workspace_settings(frame, area, name);
                }
                InputMode::SaveSnapshot {
                    name_text,
                    description_text,
                    active_field,
                    error,
                    ..
                } => {
                    self.draw_save_snapshot(
                        frame,
                        area,
                        name_text,
                        description_text,
                        *active_field,
                        error.as_deref(),
                    );
                }
                InputMode::SnapshotCatalog {
                    snapshots,
                    selected,
                    mode,
                    picker_target,
                    status_msg,
                } => {
                    self.draw_snapshot_catalog(
                        frame,
                        area,
                        snapshots,
                        *selected,
                        mode,
                        picker_target.is_some(),
                        status_msg.as_deref(),
                    );
                }
                InputMode::WorkflowPicker { names, selected, .. } => {
                    self.draw_workflow_picker(frame, area, names, *selected);
                }
                InputMode::WorkflowLaunchConfirm { ws_index, workflow_name, slots, active_slot, goal } => {
                    self.draw_workflow_launch(
                        frame,
                        area,
                        *ws_index,
                        workflow_name,
                        slots,
                        *active_slot,
                        goal,
                    );
                }
                InputMode::TaskSettings { name, .. } => {
                    self.draw_task_settings(frame, area, name);
                }
                InputMode::WorkflowHistory { run_id } => {
                    self.draw_workflow_history(frame, area, run_id);
                }
                InputMode::PastWorkspacePicker { candidates, selected } => {
                    self.draw_past_workspace_picker(frame, area, candidates, *selected);
                }
                InputMode::Confirm { prompt, .. } => {
                    self.draw_confirm(frame, area, prompt);
                }
                InputMode::Normal => {}
            }
        }
    }

    /// Minimal dialog for renaming a task from the sidebar.
    fn draw_task_settings(&self, frame: &mut Frame, area: Rect, name: &str) {
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 5u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(" Rename task ");
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let para = Paragraph::new(format!("Name: {}_", name))
            .style(Style::default().fg(Color::White));
        frame.render_widget(para, inner);
    }

    fn draw_confirm(&self, frame: &mut Frame, area: Rect, prompt: &str) {
        let width = 70u16.min(area.width.saturating_sub(4));
        let height = 5u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Confirm ");
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);
        let lines = vec![
            Line::from(Span::styled(prompt.to_string(), white)),
            Line::from(""),
            Line::from(Span::styled("y/Enter confirm \u{00b7} n/Esc cancel", dim)),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), inner);
    }

    fn draw_input_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        label_text: &str,
        branch_text: &str,
        idle_timeout_text: &str,
        repo_url: &str,
        seed_from: Option<&str>,
        active_field: u8,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        // +2 rows for the seed-from line (one separator + the field).
        let height = 13u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " New Workspace ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let repo_name = repo_url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or(repo_url);

        let cursor = "\u{2588}";
        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);
        let highlight = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);

        let repo_style = if active_field == 0 { highlight } else { white };
        let repo_hint = if active_field == 0 && self.config.repos.len() > 1 {
            "  \u{2190}/\u{2192} change"
        } else {
            ""
        };
        let name_cursor = if active_field == 1 { cursor } else { "" };
        let branch_cursor = if active_field == 2 { cursor } else { "" };
        let timeout_cursor = if active_field == 3 { cursor } else { "" };

        let branch_hint = if branch_text.is_empty() && active_field != 2 {
            "main"
        } else {
            ""
        };

        let seed_label = sanitize_for_display(seed_from.unwrap_or("[none]"));
        let seed_style = if active_field == 4 { highlight } else { white };
        let seed_hint = match (active_field == 4, seed_from.is_some()) {
            (true, true) => "  Esc clear",
            (true, false) => "  Enter pick",
            _ => "",
        };

        let lines = vec![
            Line::from(vec![
                Span::styled("    Repo: ", dim),
                Span::styled(repo_name, repo_style),
                Span::styled(repo_hint, dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Name: ", dim),
                Span::styled(label_text, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(vec![
                Span::styled("  Branch: ", dim),
                Span::styled(branch_text, white),
                Span::styled(branch_cursor, white),
                Span::styled(branch_hint, dim),
            ]),
            Line::from(vec![
                Span::styled("Idle (s): ", dim),
                Span::styled(idle_timeout_text, white),
                Span::styled(timeout_cursor, white),
            ]),
            Line::from(vec![
                Span::styled("    Seed: ", dim),
                Span::styled(seed_label, seed_style),
                Span::styled(seed_hint, dim),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Tab switch field \u{00b7} Enter start \u{00b7} Esc cancel",
                dim,
            )),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_new_terminal_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        workspace_id: &str,
        session_type: &str,
        seed_from: Option<&str>,
        active_field: u8,
    ) {
        let width = 50u16.min(area.width.saturating_sub(4));
        // +2 rows for the seed-from line.
        let height = 11u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        // Resolve workspace name from the stable id at render time —
        // tolerates a reorder while the form is open.
        let ws_name = self
            .workspaces
            .iter()
            .find(|w| w.id == workspace_id)
            .map(|w| w.name.as_str())
            .unwrap_or("?");

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " Add Session ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let options = ["claude", "codex", "bash"];
        let max_name = (width as usize).saturating_sub(8);
        let display_name: String = ws_name.chars().take(max_name).collect();

        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);
        let highlight = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);

        let mut lines = vec![
            Line::from(vec![
                Span::styled("  Task: ", dim),
                Span::styled(display_name, white),
            ]),
            Line::from(""),
        ];
        // The session-type rows are field 0 and j/k cycle them in place.
        // A `▸` marker on the active row group makes it easy to see which
        // field has focus once the seed-from line is in play below.
        let type_marker = if active_field == 0 { "▸ " } else { "  " };
        for opt in &options {
            let ind = if session_type == *opt { ">" } else { " " };
            let st = if session_type == *opt {
                if active_field == 0 { highlight } else { white }
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled(
                format!("{}{} {}", type_marker, ind, opt),
                st,
            )));
        }
        lines.push(Line::from(""));
        let seed_label = sanitize_for_display(seed_from.unwrap_or(
            if session_type == "bash" { "[N/A]" } else { "[none]" },
        ));
        let seed_style = if active_field == 1 { highlight } else { white };
        let seed_hint = match (active_field == 1, seed_from.is_some(), session_type) {
            (true, _, "bash") => "  not pickable",
            (true, true, _) => "  Esc clear",
            (true, false, _) => "  Enter pick",
            _ => "",
        };
        lines.push(Line::from(vec![
            Span::styled("  Seed: ", dim),
            Span::styled(seed_label, seed_style),
            Span::styled(seed_hint, dim),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tab field \u{00b7} j/k type \u{00b7} Enter start \u{00b7} Esc cancel",
            dim,
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_session_settings(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &str,
        idle_timeout: &str,
        burst_threshold: &str,
        hidden: bool,
        notify_on_idle: bool,
        seeded_from_snapshot: Option<&str>,
        active_field: u8,
    ) {
        let width = 55u16.min(area.width.saturating_sub(4));
        // Seeded-from line is a 2-line block (blank + "Seeded from: <name>")
        // only when the field is set; otherwise the dialog keeps its old
        // size so the unrelated common case doesn't grow.
        let height = if seeded_from_snapshot.is_some() { 17u16 } else { 15u16 };
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " Session Settings ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let cursor = "\u{2588}";
        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);

        let name_cursor = if active_field == 0 { cursor } else { "" };
        let timeout_cursor = if active_field == 1 { cursor } else { "" };
        let burst_cursor = if active_field == 2 { cursor } else { "" };
        let hidden_marker = if hidden { "[x]" } else { "[ ]" };
        let hidden_style = if active_field == 3 { white } else { dim };
        let notify_marker = if notify_on_idle { "[x]" } else { "[ ]" };
        let notify_style = if active_field == 4 { white } else { dim };

        let mut lines = vec![
            Line::from(vec![
                Span::styled("           Name: ", dim),
                Span::styled(name, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("       Idle (s): ", dim),
                Span::styled(idle_timeout, white),
                Span::styled(timeout_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Burst (wakeups): ", dim),
                Span::styled(burst_threshold, white),
                Span::styled(burst_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("         Hidden: ", dim),
                Span::styled(hidden_marker, hidden_style),
                Span::styled(if active_field == 3 { "  Space to toggle" } else { "" }, dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Notify on idle: ", dim),
                Span::styled(notify_marker, notify_style),
                Span::styled(if active_field == 4 { "  Space to toggle" } else { "" }, dim),
            ]),
        ];

        if let Some(snap) = seeded_from_snapshot {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("   Seeded from: ", dim),
                Span::styled(snap, white),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tab next field \u{00b7} Enter save \u{00b7} Esc cancel",
            dim,
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_save_snapshot(
        &self,
        frame: &mut Frame,
        area: Rect,
        name_text: &str,
        description_text: &str,
        active_field: u8,
        error: Option<&str>,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        // Base = 11 rows (title, name, blank, description, blank, blank,
        // hint). +2 when an error is being shown.
        let height = if error.is_some() { 13u16 } else { 11u16 };
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " Save Snapshot ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let cursor = "\u{2588}";
        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);
        let red = Style::default().fg(Color::Red);

        let name_cursor = if active_field == 0 { cursor } else { "" };
        let desc_cursor = if active_field == 1 { cursor } else { "" };

        let mut lines = vec![
            Line::from(vec![
                Span::styled("        Name: ", dim),
                Span::styled(name_text, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Description: ", dim),
                Span::styled(description_text, white),
                Span::styled(desc_cursor, white),
            ]),
            Line::from(""),
        ];

        if let Some(msg) = error {
            // Validation errors that quote an offending character can
            // include ESC / control bytes — sanitize on render so they
            // don't drive the terminal.
            lines.push(Line::from(Span::styled(sanitize_for_display(msg), red)));
            lines.push(Line::from(""));
        }

        lines.push(Line::from(Span::styled(
            "Tab switch field \u{00b7} Enter save \u{00b7} Esc cancel",
            dim,
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_workspace_settings(&self, frame: &mut Frame, area: Rect, name: &str) {
        let width = 55u16.min(area.width.saturating_sub(4));
        let height = 7u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " Rename Workspace ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);
        let lines = vec![
            Line::from(vec![
                Span::styled("  Name: ", dim),
                Span::styled(name, white),
                Span::styled("\u{2588}", white),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Enter save \u{00b7} Esc cancel  (branch name unchanged)",
                dim,
            )),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_terminal(&self, frame: &mut Frame, area: Rect) {
        let has_session = self.active_session().is_some();

        let title_style = if has_session {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(self.active_title(), title_style));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some((_, ts)) = self.active_session() {
            let widget = TerminalWidget::new(&ts.session.term, true);
            frame.render_widget(widget, inner);
        } else if let Some(wi) = self.active_workspace_index() {
            let ws = &self.workspaces[wi];
            let mut lines = vec![];
            // Show prompt + repo from first bound task, if any.
            if let Some(task) = self.first_task_for_ws(&ws.id) {
                if let Some(ref prompt) = task.prompt {
                    lines.push(Line::from(Span::styled(
                        prompt.as_str(),
                        Style::default().fg(Color::White),
                    )));
                    lines.push(Line::from(""));
                }
            }
            if let Some(ref repo) = ws.repo_url {
                lines.push(Line::from(Span::styled(
                    format!("Repo: {}", repo),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if let Some(ref vm) = ws.worker_vm {
                lines.push(Line::from(Span::styled(
                    format!("VM: {}", vm),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                if ws.worker_vm.is_some() {
                    "Press Alt+A to SSH into this session"
                } else {
                    "Press Alt+A to attach"
                },
                Style::default().fg(Color::DarkGray),
            )));

            frame.render_widget(Paragraph::new(lines), inner);
        } else {
            let msg = if self.connected {
                Paragraph::new(
                    "No tasks \u{2014} press Alt+n to start a local session",
                )
                .style(Style::default().fg(Color::DarkGray))
            } else {
                Paragraph::new("Connecting to API...")
                    .style(Style::default().fg(Color::DarkGray))
            };
            frame.render_widget(msg, inner);
        }
    }

    fn draw_session_list(&self, frame: &mut Frame, area: Rect) {
        let view_label = match self.sidebar_view {
            SidebarView::Status => " Sessions ",
            SidebarView::Task => " Tasks ",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                view_label,
                Style::default().fg(Color::White),
            ));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 2 || inner.width < 4 {
            return;
        }

        let spinner = self.spinner_frame();
        let dim = Style::default().fg(Color::DarkGray);

        // Help text — two columns. Defined up here so `list_height` can size
        // itself around the help footer (otherwise the help overdraws the
        // bottom rows of the list and indicators vanish).
        let help_entries: Vec<(&str, &str)> = vec![
            ("A-j/k  nav", "A-d  done"),
            ("A-a    attach", "A-x  delete"),
            ("A-n    new ws", "A-p  push"),
            ("A-s    +session", "A-l  pull"),
            ("A-w    close sess", "A-v  view"),
            ("A-W    close ws", "A-r  refresh"),
            ("A-e    settings", "A-q  quit"),
            ("A-h    hide", "A-y  history"),
            ("A-f    workflow", "A-u  resume"),
            ("A-o    stop wf", "A-,  activity"),
            ("A-b    snapshot", "A-z  catalog"),
            ("A-O    reopen ws", ""),
            ("PgUp   scroll up", ""),
            ("PgDn   scroll dn", ""),
            ("A-Ent  newline", ""),
        ];
        let help_rows = help_entries.len() as u16;
        let list_height = inner.height.saturating_sub(help_rows + 1);

        let visual = self.visual_items();
        let mut items: Vec<ListItem> = Vec::new();
        let max = list_height as usize;

        for vi in &visual {
            if items.len() >= max {
                break;
            }
            match vi {
                VisualItem::WorkspaceHeader(wi) => {
                    let ws = &self.workspaces[*wi];
                    let is_selected = match &self.cursor {
                        Cursor::Workspace(cwi) => cwi == wi,
                        _ => false,
                    };

                    let max_name = (inner.width as usize).saturating_sub(2);
                    let name = if ws.name.len() > max_name {
                        format!("{}...", &ws.name[..max_name.saturating_sub(3)])
                    } else {
                        ws.name.clone()
                    };

                    let header_line = Line::from(vec![
                        Span::raw(" "),
                        Span::raw(name),
                    ]);

                    let base_style = if is_selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };

                    items.push(ListItem::new(header_line).style(base_style));
                }
                VisualItem::Session(wi, si) => {
                    let ws = &self.workspaces[*wi];
                    let ts = &ws.sessions[*si];
                    let is_selected = match &self.cursor {
                        Cursor::Session(cwi, csi) => cwi == wi && csi == si,
                        _ => false,
                    };

                    // Find enclosing workflow run, if any — controls vertical-line
                    // prefix for visual grouping in task view.
                    let in_active_workflow = ts
                        .workflow_run_id
                        .as_deref()
                        .is_some_and(|id| self.workflow_runs.iter().any(|r| r.run_id == id));

                    let (indicator, indicator_style) = if ts.hidden {
                        (" ", Style::default())
                    } else {
                        match ts.status {
                            SessionStatus::Running => {
                                (spinner, Style::default().fg(Color::Green))
                            }
                            SessionStatus::Idle => {
                                ("\u{25cf}", Style::default().fg(Color::White))
                            }
                        }
                    };

                    // Role badge for workflow-participant sessions, e.g.
                    // "[worker] " / "[reviewer] " / "[manager] ". Phase 6
                    // widened the sidebar so the full role name fits;
                    // single-char tags like "[W]" were too cryptic at a
                    // glance once feedback workflows became routine.
                    let wf_badge: Option<(String, Style)> =
                        if let (Some(run_id), Some(role)) =
                            (ts.workflow_run_id.as_deref(), ts.workflow_role.as_deref())
                        {
                            let active = self
                                .workflow_runs
                                .iter()
                                .any(|r| r.run_id == run_id && r.active_role.as_deref() == Some(role));
                            let style = if active {
                                Style::default().fg(Color::Yellow)
                            } else {
                                Style::default().fg(Color::Cyan)
                            };
                            Some((format!("[{}] ", role), style))
                        } else {
                            None
                        };

                    let display = match self.sidebar_view {
                        SidebarView::Status => {
                            let max_name =
                                (inner.width as usize).saturating_sub(8);
                            let full = format!("{} / {}", ws.name, ts.label);
                            if full.len() > max_name {
                                format!(
                                    "{}...",
                                    &full[..max_name.saturating_sub(3)]
                                )
                            } else {
                                full
                            }
                        }
                        SidebarView::Task => {
                            // Indent levels (Phase 6 deepened by 2 cells per tier
                            // so workflow-participant nesting reads cleanly):
                            //   - Workspace-level (no task): 2 spaces.
                            //   - Task-scoped, no workflow:  4 spaces.
                            //   - Workflow participant:      6 spaces, putting
                            //     them visually inside the task they belong to.
                            let in_active_wf = ts
                                .workflow_run_id
                                .as_deref()
                                .is_some_and(|id| {
                                    self.workflow_runs.iter().any(|r| r.run_id == id)
                                });
                            if in_active_wf {
                                format!("      {}", ts.label)
                            } else if ts.task_id.is_some() {
                                format!("    {}", ts.label)
                            } else {
                                format!("  {}", ts.label)
                            }
                        }
                    };

                    let mut spans = vec![Span::styled(
                        format!(" {} ", indicator),
                        indicator_style,
                    )];
                    // Vertical line prefix for sessions inside a workflow group
                    // (only in task view where grouping makes sense visually).
                    if in_active_workflow && self.sidebar_view == SidebarView::Task {
                        spans.push(Span::styled(
                            "\u{2502} ",
                            Style::default().fg(Color::DarkGray),
                        ));
                    }
                    if let Some((badge, style)) = wf_badge {
                        spans.push(Span::styled(badge, style));
                    }
                    spans.push(Span::raw(display));
                    let line = Line::from(spans);

                    let base_style = if is_selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    items.push(ListItem::new(line).style(base_style));
                }
                VisualItem::Separator => {
                    let sep_line = Line::from(Span::styled(
                        format!(
                            " {}",
                            "\u{2500}"
                                .repeat(
                                    inner.width.saturating_sub(2) as usize
                                )
                        ),
                        dim,
                    ));
                    items.push(ListItem::new(sep_line));
                }
                VisualItem::WorkflowHeader { ws_idx, run_id } => {
                    let ws = &self.workspaces[*ws_idx];
                    let run = self.workflow_runs.iter().find(|r| &r.run_id == run_id);
                    let (agg_indicator, agg_style) = match run {
                        Some(r) => aggregate_indicator(r, ws, spinner),
                        None => ("\u{25cf}", Style::default().fg(Color::DarkGray)),
                    };
                    let name = run
                        .map(|r| r.workflow_name.clone())
                        .unwrap_or_else(|| "workflow".into());
                    let paused_suffix = run
                        .map(|r| match r.status {
                            workflow::RunStatus::Paused => " (paused)",
                            workflow::RunStatus::Done => " (done)",
                            _ => "",
                        })
                        .unwrap_or("");
                    let line = Line::from(vec![
                        Span::styled(format!(" {} ", agg_indicator), agg_style),
                        Span::styled(
                            format!("\u{256d}\u{2500} {}{}", name, paused_suffix),
                            Style::default().fg(Color::Cyan),
                        ),
                    ]);
                    items.push(ListItem::new(line));
                }
                VisualItem::TaskHeader { ws_idx, task_id } => {
                    let is_selected = match &self.cursor {
                        Cursor::Task { ws_idx: cwi, task_id: ctid } => {
                            cwi == ws_idx && ctid == task_id
                        }
                        _ => false,
                    };
                    let name = self
                        .tasks
                        .iter()
                        .find(|t| t.task_id.as_deref() == Some(task_id.as_str()))
                        .map(|t| t.name.clone())
                        .unwrap_or_else(|| "task".into());
                    let max_name = (inner.width as usize).saturating_sub(4);
                    let name = if name.len() > max_name {
                        format!("{}...", &name[..max_name.saturating_sub(3)])
                    } else {
                        name
                    };
                    // Style lives on the ListItem so selection highlight can
                    // override. Using Span::styled with a fixed color here
                    // would mask the base_style on selection.
                    let base_style = if is_selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Cyan)
                    };
                    let line = Line::from(vec![
                        Span::raw("  "),
                        Span::raw(name),
                    ]);
                    items.push(ListItem::new(line).style(base_style));
                }
            }
        }

        let list = List::new(items);
        frame.render_widget(
            list,
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: list_height,
            },
        );

        let help_y = inner.y + inner.height.saturating_sub(help_rows + 1);
        let help_area = Rect {
            x: inner.x,
            y: help_y,
            width: inner.width,
            height: help_rows + 1,
        };

        let sep = Line::from(Span::styled(
            "\u{2500}".repeat(inner.width as usize),
            dim,
        ));
        let col = inner.width / 2;

        let mut lines = vec![sep];
        for (left, right) in &help_entries {
            let left_padded = format!("{:<w$}", left, w = col as usize);
            let line = Line::from(vec![
                Span::styled(left_padded, dim),
                Span::styled(*right, dim),
            ]);
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), help_area);
    }

    /// Phase 6: render the activity-feed strip (Alt-, toggle). Shows the
    /// last few `ActivityEntry`s from `self.activity_log` formatted as
    /// `HH:MM:SS  caller → summary`. The strip is bordered so it's
    /// visually distinct from the main content and the status bar.
    fn draw_activity_feed(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Activity (Alt-, to hide) ",
                Style::default().fg(Color::White),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width < 8 {
            return;
        }

        // Render the most recent N entries (where N is the inner height),
        // bottom-anchored so the newest entry is closest to the status bar
        // and older entries scroll upward as new ones arrive.
        let take = inner.height as usize;
        let start = self.activity_log.len().saturating_sub(take);
        let mut lines: Vec<Line> = Vec::with_capacity(take);
        for entry in self.activity_log.iter().skip(start) {
            let ts_str = format_utc_hms(entry.ts);
            // Caller column padded to a stable width so summary text
            // aligns across entries even when caller names differ.
            let caller_col_width = 10;
            let caller_padded = if entry.caller_label.chars().count() >= caller_col_width {
                entry.caller_label.chars().take(caller_col_width).collect::<String>()
            } else {
                let pad = caller_col_width - entry.caller_label.chars().count();
                format!("{}{}", entry.caller_label, " ".repeat(pad))
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", ts_str),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(caller_padded, Style::default().fg(Color::Cyan)),
                Span::raw(" → "),
                Span::raw(entry.summary.clone()),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let running = self
            .tasks
            .iter()
            .filter(|t| matches!(self.task_status(t), TaskStatus::Running))
            .count();
        let blocked = self
            .tasks
            .iter()
            .filter(|t| matches!(self.task_status(t), TaskStatus::Blocked))
            .count();
        let backlog = self
            .tasks
            .iter()
            .filter(|t| matches!(self.task_status(t), TaskStatus::Backlog))
            .count();

        let conn_indicator = if self.connected { "\u{25cf}" } else { "\u{25cb}" };
        let conn_color = if self.connected {
            Color::Green
        } else {
            Color::Red
        };

        let center = if let Some((ref msg, when)) = self.status_msg {
            if when.elapsed().as_secs() < 3 {
                msg.clone()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let mouse_off = !self.mouse_capture_enabled;
        let mouse_indicator = if mouse_off { " [mouse off] " } else { "" };

        let right = format!(" {}r {}b {}q ", running, blocked, backlog);

        let right_width = right.chars().count() as u16;
        let center_width = center.len() as u16;
        let mouse_width = mouse_indicator.chars().count() as u16;
        let left_used = 18u16; // " ● claude-manager "
        let pad = area
            .width
            .saturating_sub(left_used + right_width + center_width + mouse_width);
        let pad_left = pad / 2;
        let pad_right = pad - pad_left;

        let line = Line::from(vec![
            Span::styled(
                format!(" {} ", conn_indicator),
                Style::default().fg(conn_color),
            ),
            Span::styled(
                "claude-manager ",
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                mouse_indicator,
                Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " ".repeat(pad_left as usize),
                Style::default(),
            ),
            Span::styled(center, Style::default().fg(Color::Yellow)),
            Span::styled(
                " ".repeat(pad_right as usize),
                Style::default(),
            ),
            Span::styled(right, Style::default().fg(Color::DarkGray)),
        ]);

        frame.render_widget(Paragraph::new(line), area);
    }

    fn active_title(&self) -> String {
        if let Some((ws, ts)) = self.active_session() {
            format!(" {} / {} ", ws.name, ts.label)
        } else if let Some(wi) = self.active_workspace_index() {
            format!(" {} ", self.workspaces[wi].name)
        } else {
            " Terminal ".to_string()
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//                            Workflow integration
// ═══════════════════════════════════════════════════════════════════════════

impl App {
    /// Open the launch modal for a workflow, prefilled for the focused session.
    fn open_workflow_launch(&mut self) {
        let (wi, focused_si) = match self.cursor.clone() {
            Cursor::Session(wi, si) => (wi, Some(si)),
            Cursor::Workspace(wi) => (wi, None),
            Cursor::Task { ws_idx, task_id } => {
                // Focus on the first running session of the task so the
                // launch modal can prefill a reasonable worker slot.
                let si = self
                    .workspaces
                    .get(ws_idx)
                    .and_then(|w| {
                        w.sessions
                            .iter()
                            .position(|ts| ts.task_id.as_deref() == Some(task_id.as_str()))
                    });
                (ws_idx, si)
            }
        };
        if wi >= self.workspaces.len() {
            self.set_status_msg("No workspace selected");
            return;
        }
        // Same reason as `start_new_terminal_session`: workflow
        // participants are sibling sessions on the workspace, and a
        // push in flight will tombstone them on `PushComplete`.
        if self.workspaces[wi].is_pushing {
            self.set_status_msg("Workspace is being pushed to cloud, retry after");
            return;
        }

        let mut names: Vec<String> = self.workflows.keys().cloned().collect();
        names.sort();
        let has_load_errors = !self.workflow_load_errors.is_empty();
        match route_workflow_launch(names, has_load_errors) {
            WorkflowLaunchRouting::NoWorkflowsFound => {
                self.set_status_msg(&format!(
                    "No workflows found in {}",
                    workflow::toml_schema::workflows_dir().display()
                ));
            }
            WorkflowLaunchRouting::LaunchOnly(only) => {
                self.enter_workflow_launch_confirm(wi, focused_si, only);
            }
            WorkflowLaunchRouting::OpenPicker(names) => {
                self.input_mode = InputMode::WorkflowPicker {
                    ws_index: wi,
                    focused_si,
                    names,
                    selected: 0,
                };
            }
        }
    }

    /// Build the per-role slot list for `wf_name` and enter the launch-confirm
    /// modal. Called both from the single-workflow fast path and after the
    /// user picks a workflow in `WorkflowPicker`.
    fn enter_workflow_launch_confirm(
        &mut self,
        wi: usize,
        focused_si: Option<usize>,
        wf_name: String,
    ) {
        let Some(wf) = self.workflows.get(&wf_name).cloned() else {
            self.set_status_msg(&format!(
                "Workflow '{}' not found (looked in {})",
                wf_name,
                workflow::toml_schema::workflows_dir().display()
            ));
            return;
        };

        let ws = &self.workspaces[wi];
        let mut slots = Vec::new();
        for (idx, role_name) in wf.role_order.iter().enumerate() {
            let role = &wf.roles[role_name];
            let is_fresh = matches!(role.context, workflow::toml_schema::Context::Fresh);

            let mut options: Vec<WorkflowSlotSource> = Vec::new();
            if !is_fresh {
                for si in 0..ws.sessions.len() {
                    let ts = &ws.sessions[si];
                    if ts.workflow_run_id.is_some() {
                        continue;
                    }
                    options.push(WorkflowSlotSource::Existing(si));
                }
            }
            options.push(WorkflowSlotSource::New(Engine::ClaudeCode));
            options.push(WorkflowSlotSource::New(Engine::Codex));

            let initial = if idx == 0
                && focused_si.is_some()
                && !is_fresh
                && options
                    .iter()
                    .any(|o| matches!(o, WorkflowSlotSource::Existing(si) if Some(*si) == focused_si))
            {
                options
                    .iter()
                    .position(|o| matches!(o, WorkflowSlotSource::Existing(si) if Some(*si) == focused_si))
                    .unwrap()
            } else {
                options
                    .iter()
                    .position(|o| matches!(o, WorkflowSlotSource::New(e) if *e == role.engine))
                    .unwrap_or(options.len() - 1)
            };

            slots.push(WorkflowSlotChoice {
                role: role_name.clone(),
                options,
                option_index: initial,
            });
        }
        self.input_mode = InputMode::WorkflowLaunchConfirm {
            ws_index: wi,
            workflow_name: wf_name,
            slots,
            active_slot: 0,
            goal: String::new(),
        };
    }

    /// Actually launch a workflow given a workspace and resolved slot choices.
    /// Real implementation lives in
    /// `workflow::controller::WorkflowControllerCtx::launch_workflow`; this is
    /// a thin dispatcher matching the F7 pattern (build the controller's
    /// reference bag, run it, apply the resulting actions).
    fn launch_workflow(
        &mut self,
        ws_index: usize,
        workflow_name: &str,
        slots: Vec<WorkflowSlotChoice>,
        goal: Option<String>,
    ) {
        self.run_workflow_controller(|ctx| {
            ctx.launch_workflow(ws_index, workflow_name, slots, goal)
        });
    }

    /// Programmatic workflow launch used by the `start_workflow` MCP
    /// tool. Builds default slots (all roles get a freshly-spawned
    /// session per their TOML-declared engine) and routes through the
    /// existing `launch_workflow` UI path. Returns the new run's id.
    ///
    /// The MCP path differs from the UI path in two important ways:
    ///   1. The agent provides the target `task_id`; we set it on the
    ///      new `WorkflowRun` so descendant auth (in `methods.rs`) can
    ///      evaluate against a real task id rather than the workflow's
    ///      `task_key` (which is a workspace key, not a task id).
    ///   2. All slots are fresh-new, so participants would otherwise
    ///      have `task_id: None`. We override each participant's task
    ///      binding to match the launching task — without this, every
    ///      session-management call from the launching agent (list,
    ///      read_session_output, send_input, kill) would fail the
    ///      descendant check on participants and return unauthorized.
    pub fn start_workflow_run(
        &mut self,
        ws_index: usize,
        workflow_name: &str,
        goal: Option<String>,
        task_id: Option<String>,
        existing_role_sessions: std::collections::BTreeMap<String, usize>,
    ) -> Result<String, String> {
        let wf = self
            .workflows
            .get(workflow_name)
            .cloned()
            .ok_or_else(|| format!("workflow not found: {}", workflow_name))?;
        if ws_index >= self.workspaces.len() {
            return Err(format!("workspace index {} out of range", ws_index));
        }
        // Reject any role name in existing_role_sessions that isn't a
        // role of this workflow — surfaces typos at the MCP boundary
        // instead of silently falling back to a fresh spawn.
        for role in existing_role_sessions.keys() {
            if !wf.roles.contains_key(role) {
                return Err(format!(
                    "role '{}' is not declared in workflow '{}'",
                    role, workflow_name
                ));
            }
        }
        let slots: Vec<WorkflowSlotChoice> = wf
            .role_order
            .iter()
            .filter_map(|role_name| {
                let role = wf.roles.get(role_name)?;
                let source = match existing_role_sessions.get(role_name) {
                    Some(si) => WorkflowSlotSource::Existing(*si),
                    None => WorkflowSlotSource::New(role.engine.clone()),
                };
                Some(WorkflowSlotChoice {
                    role: role_name.clone(),
                    options: vec![source],
                    option_index: 0,
                })
            })
            .collect();
        if slots.is_empty() {
            return Err("workflow has no roles".into());
        }
        let count_before = self.workflow_runs.len();
        self.launch_workflow(ws_index, workflow_name, slots, goal);
        if self.workflow_runs.len() == count_before {
            return Err(self
                .status_msg
                .as_ref()
                .map(|(s, _)| s.clone())
                .unwrap_or_else(|| "launch failed".into()));
        }
        // Pull the run we just pushed and stamp it with the launching
        // task_id. Same-pass: also bind every participant session in
        // this run to that task. Both are required for downstream
        // descendant auth.
        let run_idx = self.workflow_runs.len() - 1;
        let run_id = self.workflow_runs[run_idx].run_id.clone();
        if let Some(tid) = task_id.as_deref() {
            self.workflow_runs[run_idx].task_id = Some(tid.to_string());
            // Walk every workspace + session; tag participants of this
            // run so they descend from the launching task.
            for ws in &mut self.workspaces {
                for ts in &mut ws.sessions {
                    if ts.workflow_run_id.as_deref() == Some(run_id.as_str()) {
                        ts.task_id = Some(tid.to_string());
                    }
                }
            }
            let _ = workflow::run::save(&self.workflow_runs[run_idx]);
            self.save_session_manifest();
        }

        // Deliver the initial activation prompt. `launch_workflow`
        // creates the run + spawns participant sessions but never
        // queues an initial prompt — that's by design for UI launches,
        // where the user types into the worker themselves. For MCP
        // launches there's no human typing, so without this the worker
        // sits idle, the on_idle gate never fires, and the workflow
        // does nothing.
        //
        // Strategy: prefer the initial role's `activation_prompt`
        // template (rendered with the workflow context, including
        // `{{ goal }}`) if defined. Otherwise deliver `goal` directly
        // as the worker's first user turn — both shipped workflows
        // (feedback, review) leave the initial role's
        // `activation_prompt` unset because they expect the user to
        // type the goal in.
        if let Some(initial_role) = self.workflow_runs[run_idx].active_role.clone() {
            self.deliver_initial_workflow_prompt(&run_id, &initial_role, ws_index);
        }
        Ok(run_id)
    }

    /// Build a `WorkflowControllerCtx` borrowing this `App`'s workflow
    /// state, run a controller method via `f`, then dispatch the
    /// returned actions back through `App` (status bar, manifest
    /// persistence). Borrows split here so the closure has a `&mut`
    /// view of just the controller-relevant fields; status_msg and
    /// manifest writes happen on the App after the borrow ends.
    fn run_workflow_controller<F>(&mut self, f: F)
    where
        F: FnOnce(&mut workflow::controller::WorkflowControllerCtx<'_>) -> Vec<workflow::controller::WorkflowAction>,
    {
        let last_term_size = self.last_term_size;
        let actions = {
            let mut ctx = workflow::controller::WorkflowControllerCtx {
                workflow_runs: &mut self.workflow_runs,
                workspaces: &mut self.workspaces,
                workflows: &self.workflows,
                last_term_size,
                config: &self.config,
                cap_status: &self.memory_cap_status,
                kill_tx: &self.memory_kill_tx,
            };
            f(&mut ctx)
        };
        self.apply_workflow_actions(actions);
    }

    /// Apply each `WorkflowAction` the controller emitted. One arm per
    /// variant — same one-line dispatcher pattern F4's
    /// `handle_input_event` ended up with.
    fn apply_workflow_actions(&mut self, actions: Vec<workflow::controller::WorkflowAction>) {
        for action in actions {
            match action {
                workflow::controller::WorkflowAction::SaveSessionManifest => {
                    self.save_session_manifest();
                }
                workflow::controller::WorkflowAction::SetStatusMsg(msg) => {
                    self.set_status_msg(&msg);
                }
            }
        }
    }

    /// Deliver the very first activation prompt to the initial role's
    /// session in a freshly-launched workflow. Called only by the MCP
    /// launch path (`start_workflow_run`); UI launches don't need this
    /// because the user types directly into the session.
    fn deliver_initial_workflow_prompt(
        &mut self,
        run_id: &str,
        role_name: &str,
        ws_index: usize,
    ) {
        self.run_workflow_controller(|ctx| {
            ctx.deliver_initial_workflow_prompt(run_id, role_name, ws_index)
        });
    }

    /// Called once per main loop iteration. Drives transitions for each
    /// active workflow run. Real implementation lives in
    /// `workflow::controller::WorkflowControllerCtx::tick`; this is a
    /// thin dispatcher: build the controller's reference bag, run it,
    /// apply the resulting actions.
    pub fn tick_workflows(&mut self) {
        self.run_workflow_controller(|ctx| ctx.tick());
    }

    /// Mark the focused session's workflow run as paused. No-op if the focused
    /// session isn't in a workflow or the run is already paused/done.
    ///
    /// Called when the user hits Ctrl-C on a participant session — the
    /// keystroke itself is still forwarded to the PTY so the agent receives
    /// the interrupt as it would in a normal terminal.
    fn pause_focused_workflow(&mut self) {
        let run_id = match self.focused_session_run_id() {
            Some(id) => id,
            None => return,
        };
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            if matches!(run.status, workflow::RunStatus::Running) {
                run.set_paused(true);
                let _ = workflow::run::save(run);
                self.set_status_msg("Workflow paused (A-u to resume)");
            }
        }
    }

    fn resume_workflow_for_cursor(&mut self) {
        let run_id = match self.focused_session_run_id() {
            Some(id) => id,
            None => {
                self.set_status_msg("Focused session is not in a workflow");
                return;
            }
        };
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            if matches!(run.status, workflow::RunStatus::Paused) {
                run.set_paused(false);
                let _ = workflow::run::save(run);
                self.set_status_msg(&format!("Resumed workflow {}", run_id));
            } else {
                self.set_status_msg("Workflow is not paused");
            }
        }
    }

    /// Stop a workflow by run id.
    ///
    /// The workflow run is marked detached (no more transitions will fire) and
    /// the participating sessions have their workflow tags cleared so they
    /// behave like normal standalone sessions from here on. The sessions
    /// themselves stay open and their transcripts are preserved.
    pub(crate) fn stop_workflow_run(&mut self, run_id: &str) {
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            run.mark_detached();
            let _ = workflow::run::save(run);
        }
        for ws in &mut self.workspaces {
            for ts in &mut ws.sessions {
                if ts.workflow_run_id.as_deref() == Some(run_id) {
                    ts.workflow_run_id = None;
                    ts.workflow_role = None;
                    ts.hidden = false;
                }
            }
        }
        self.workflow_runs.retain(|r| r.run_id != run_id);
        self.save_session_manifest();
        self.set_status_msg("Workflow stopped");
    }

    fn open_workflow_history(&mut self) {
        let run_id = match self.focused_session_run_id() {
            Some(id) => id,
            None => {
                self.set_status_msg("Focused session is not in a workflow");
                return;
            }
        };
        self.input_mode = InputMode::WorkflowHistory { run_id };
    }

    fn focused_session_run_id(&self) -> Option<String> {
        match self.cursor.clone() {
            Cursor::Session(wi, si) => self
                .workspaces
                .get(wi)
                .and_then(|w| w.sessions.get(si))
                .and_then(|s| s.workflow_run_id.clone()),
            Cursor::Task { ws_idx, task_id } => {
                // Return the first workflow run_id seen on any session tagged
                // with this task. Usually unique per task.
                self.workspaces.get(ws_idx).and_then(|w| {
                    w.sessions
                        .iter()
                        .filter(|ts| ts.task_id.as_deref() == Some(task_id.as_str()))
                        .find_map(|ts| ts.workflow_run_id.clone())
                })
            }
            Cursor::Workspace(_) => None,
        }
    }

}

// ═══════════════════════════════════════════════════════════════════════════
//                         Workflow modal rendering
// ═══════════════════════════════════════════════════════════════════════════

/// Title for the workflow picker dialog. Includes a count when any workflow
/// files failed to load, so the user has a hint that some entries are missing
/// from the picker list.
pub(crate) fn workflow_picker_title(error_count: usize) -> String {
    if error_count == 0 {
        " Pick workflow ".to_string()
    } else {
        format!(" Pick workflow ({} failed to load) ", error_count)
    }
}

/// Strip the directory-level `Io(NotFound)` from `load_all`'s error list and
/// stringify the rest. A missing `workflows/` directory (e.g. fresh install)
/// is not a "load failure" — it's an absent surface, already handled by the
/// existing "No workflows found in …" status. Treating it as a load error
/// would push the picker into "(1 failed to load)" mode on every bare repo.
pub(crate) fn filter_real_workflow_load_errors(
    workflows_dir: &Path,
    errs: Vec<(PathBuf, workflow::toml_schema::WorkflowError)>,
) -> Vec<(PathBuf, String)> {
    errs.into_iter()
        .filter(|(p, e)| {
            !(p == workflows_dir
                && matches!(
                    e,
                    workflow::toml_schema::WorkflowError::Io(io)
                        if io.kind() == std::io::ErrorKind::NotFound
                ))
        })
        .map(|(p, e)| (p, e.to_string()))
        .collect()
}

/// Routing decision for the `A-f` launch keybinding. The picker is normally
/// short-circuited when there are 0 or 1 valid workflows, but when load
/// errors exist we force-open it so the user sees which TOML files failed
/// — otherwise a typo silently drops a workflow with no hint.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WorkflowLaunchRouting {
    NoWorkflowsFound,
    LaunchOnly(String),
    OpenPicker(Vec<String>),
}

pub(crate) fn route_workflow_launch(
    valid_names: Vec<String>,
    has_load_errors: bool,
) -> WorkflowLaunchRouting {
    match (valid_names.len(), has_load_errors) {
        (0, false) => WorkflowLaunchRouting::NoWorkflowsFound,
        (1, false) => {
            WorkflowLaunchRouting::LaunchOnly(valid_names.into_iter().next().unwrap())
        }
        _ => WorkflowLaunchRouting::OpenPicker(valid_names),
    }
}

/// One-line summary for a single workflow load failure, rendered as a dim row
/// at the top of the picker dialog. Uses the file's basename to keep the row
/// short; the full path is logged to stderr at startup.
pub(crate) fn format_workflow_load_error(path: &Path, err: &str) -> String {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>");
    let one_line = err.lines().next().unwrap_or("").trim();
    format!("⚠ {}: {}", name, one_line)
}

impl App {
    pub fn draw_workflow_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        names: &[String],
        selected: usize,
    ) {
        let err_rows = self.workflow_load_errors.len() as u16;
        let err_pad = if err_rows > 0 { 1 } else { 0 };
        let width = area.width.min(60).max(36);
        let height = (names.len() as u16 + 5 + err_rows + err_pad)
            .min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let mut lines: Vec<Line> = Vec::new();
        for (path, err) in &self.workflow_load_errors {
            lines.push(Line::from(Span::styled(
                format_workflow_load_error(path, err),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if !self.workflow_load_errors.is_empty() {
            lines.push(Line::from(""));
        }
        for (idx, name) in names.iter().enumerate() {
            let is_active = idx == selected;
            let cursor = if is_active { "▸ " } else { "  " };
            let desc = self
                .workflows
                .get(name)
                .map(|w| w.description.clone())
                .unwrap_or_default();
            let name_style = if is_active {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let desc_style = if is_active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let mut spans = vec![
                Span::raw(cursor),
                Span::styled(format!("{:<12}", name), name_style),
            ];
            if !desc.is_empty() {
                spans.push(Span::styled(desc, desc_style));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "\u{2191}\u{2193} select   Enter: choose   Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(workflow_picker_title(self.workflow_load_errors.len()))
            .style(Style::default().fg(Color::White));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }

    pub fn draw_past_workspace_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        candidates: &[PastCandidate],
        selected: usize,
    ) {
        let rows = candidates.len() as u16;
        let width = area.width.min(80).max(40);
        let height = (rows + 5).min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let mut lines: Vec<Line> = Vec::new();
        if candidates.is_empty() {
            lines.push(Line::from(Span::styled(
                "No past workspaces.",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (idx, cand) in candidates.iter().enumerate() {
                let is_active = idx == selected;
                let cursor = if is_active { "\u{25b8} " } else { "  " };
                let path_repr = cand
                    .worktree_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                let name_style = if !cand.worktree_exists {
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::CROSSED_OUT)
                } else if is_active {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                let path_style = if !cand.worktree_exists {
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::CROSSED_OUT)
                } else if is_active {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let suffix = if cand.worktree_exists {
                    String::new()
                } else {
                    "  (worktree gone)".to_string()
                };
                let mut spans = vec![
                    Span::raw(cursor),
                    Span::styled(cand.display.clone(), name_style),
                ];
                if !path_repr.is_empty() {
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(path_repr.to_string(), path_style));
                }
                if !suffix.is_empty() {
                    spans.push(Span::styled(
                        suffix,
                        Style::default().fg(Color::Red),
                    ));
                }
                lines.push(Line::from(spans));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "\u{2191}\u{2193} select   Enter: reopen   Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Reopen past workspace ")
            .style(Style::default().fg(Color::White));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }

    pub fn draw_snapshot_catalog(
        &self,
        frame: &mut Frame,
        area: Rect,
        snapshots: &[agent_memory::Snapshot],
        selected: usize,
        mode: &CatalogMode,
        is_picker: bool,
        status_msg: Option<&str>,
    ) {
        // Each sub-mode reuses the same outer dialog so transitions feel
        // in-place. Browse renders the list; Detail overlays the manifest
        // and head/tail; Rename overlays an inline editor; ConfirmDelete
        // overlays a y/n prompt.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let width = area.width.min(78).max(40);
        let height = area.height.min(28).max(8);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let title = match (is_picker, mode) {
            (true, _) => " Pick Snapshot ",
            (false, CatalogMode::Detail { .. }) => " Snapshot Detail ",
            (false, CatalogMode::Rename { .. }) => " Rename Snapshot ",
            (false, CatalogMode::ConfirmDelete) => " Delete Snapshot ",
            _ => " Snapshots ",
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(title, Style::default().fg(Color::White)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);
        let cyan = Style::default().fg(Color::Cyan);
        let yellow = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        let red = Style::default().fg(Color::Red);

        if snapshots.is_empty() {
            let mut lines = vec![
                Line::from(""),
                Line::from(Span::styled(
                    "No snapshots saved yet.",
                    Style::default().fg(Color::Gray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Focus a claude / codex session and press A-b to save one.",
                    dim,
                )),
                Line::from(""),
            ];
            if let Some(msg) = status_msg {
                lines.push(Line::from(Span::styled(
                    sanitize_for_display(msg),
                    Style::default().fg(Color::Red),
                )));
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled("Esc / A-z close", dim)));
            frame.render_widget(Paragraph::new(lines), inner);
            return;
        }

        // Reserve rows for the footer (blank + optional status + hint).
        // Cap the visible row count by the inner area so the selection
        // never scrolls off-screen and the footer is always visible —
        // a long list without windowing would silently let the user
        // rename / delete an invisible row.
        let footer_rows: u16 = 1 // blank separator
            + if status_msg.is_some() { 1 } else { 0 }
            + 1; // hint
        let visible_rows = inner
            .height
            .saturating_sub(footer_rows) as usize;
        let (row_start, row_end) =
            visible_range(selected, snapshots.len(), visible_rows);

        let mut lines: Vec<Line> = Vec::new();
        for (idx, snap) in snapshots[row_start..row_end].iter().enumerate() {
            let global_idx = row_start + idx;
            let is_active = global_idx == selected;
            let cursor = if is_active { "▸ " } else { "  " };
            let engine = match snap.manifest.engine {
                Engine::ClaudeCode => "claude-code",
                Engine::Codex => "codex",
            };
            let when =
                format_relative_time(snap.manifest.created_at_unix, now_secs);
            // Sanitize every snapshot-sourced string — manifests come from
            // disk and could include ANSI/OSC bytes that would otherwise
            // execute against the user's terminal on render.
            let safe_name = sanitize_for_display(&snap.name);
            let desc_first = sanitize_for_display(
                snap.manifest.description.lines().next().unwrap_or(""),
            );

            let name_style = if is_active { yellow } else { cyan };
            let meta_style = if is_active { white } else { dim };
            let mut spans = vec![
                Span::raw(cursor),
                Span::styled(format!("{safe_name:<24}"), name_style),
                Span::styled(format!("{engine:<13}"), meta_style),
                Span::styled(format!("{when:<10}"), meta_style),
            ];
            if !desc_first.is_empty() {
                spans.push(Span::styled(desc_first, meta_style));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        if let Some(msg) = status_msg {
            lines.push(Line::from(Span::styled(
                sanitize_for_display(msg),
                Style::default().fg(Color::Red),
            )));
        }
        let total = snapshots.len();
        let hint = if is_picker {
            "j/k select \u{00b7} Enter pick \u{00b7} Esc / A-z cancel"
        } else {
            "j/k select \u{00b7} Enter detail \u{00b7} r rename \u{00b7} d delete \u{00b7} Esc / A-z close"
        };
        // Indicator like " 41/53 " when only a window is visible, so the
        // user can tell their selection is part of a longer list.
        let footer = if row_end - row_start < total {
            format!(
                "{hint}    [{}/{}]",
                selected.saturating_add(1),
                total,
            )
        } else {
            hint.to_string()
        };
        lines.push(Line::from(Span::styled(footer, dim)));

        frame.render_widget(Paragraph::new(lines), inner);

        // Sub-mode overlays.
        match mode {
            CatalogMode::Browse => {}
            CatalogMode::Detail { head, tail } => {
                if let Some(snap) = snapshots.get(selected) {
                    self.draw_snapshot_detail(frame, inner, snap, head, tail);
                }
            }
            CatalogMode::Rename { text, error } => {
                self.draw_snapshot_rename_overlay(
                    frame,
                    inner,
                    text,
                    error.as_deref(),
                );
            }
            CatalogMode::ConfirmDelete => {
                if let Some(snap) = snapshots.get(selected) {
                    self.draw_snapshot_delete_overlay(frame, inner, &snap.name);
                }
            }
        }
        let _ = (white, red);
    }

    fn draw_snapshot_detail(
        &self,
        frame: &mut Frame,
        area: Rect,
        snap: &agent_memory::Snapshot,
        head: &[String],
        tail: &[String],
    ) {
        let width = area.width.saturating_sub(2).min(74).max(30);
        let height = area.height.saturating_sub(2).min(22).max(6);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                format!(" {} ", sanitize_for_display(&snap.name)),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);

        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let engine = match snap.manifest.engine {
            Engine::ClaudeCode => "claude-code",
            Engine::Codex => "codex",
        };

        let mut lines: Vec<Line> = Vec::new();
        // Every snapshot-sourced string is sanitized — manifests live on
        // disk and could carry ANSI/OSC byte sequences that would
        // otherwise be replayed into the user's terminal here.
        let kv = |k: &str, v: &str| {
            Line::from(vec![
                Span::styled(format!("{k:<16}"), dim),
                Span::styled(sanitize_for_display(v), white),
            ])
        };
        lines.push(kv("Engine:", engine));
        lines.push(kv(
            "Created:",
            &format_relative_time(snap.manifest.created_at_unix, now_secs),
        ));
        lines.push(kv(
            "Transcript:",
            &format!("{} bytes", snap.manifest.transcript_bytes),
        ));
        lines.push(kv(
            "Memory files:",
            &snap.manifest.memory_files.to_string(),
        ));
        lines.push(kv("Source UID:", &snap.manifest.source_session_uid));
        lines.push(kv(
            "Source transcript:",
            &snap.manifest.source_transcript_id,
        ));
        lines.push(kv(
            "Source cwd:",
            &snap.manifest.source_cwd.display().to_string(),
        ));
        if !snap.manifest.description.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Description", dim)));
            for ln in snap.manifest.description.lines() {
                lines.push(Line::from(Span::styled(
                    sanitize_for_display(ln),
                    white,
                )));
            }
        }
        if !head.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Transcript head", dim)));
            for ln in head {
                lines.push(Line::from(Span::styled(
                    truncate(&sanitize_for_display(ln), 72),
                    white,
                )));
            }
        }
        if !tail.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled("Transcript tail", dim)));
            for ln in tail {
                lines.push(Line::from(Span::styled(
                    truncate(&sanitize_for_display(ln), 72),
                    white,
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("Esc / Enter back", dim)));

        frame.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }

    fn draw_snapshot_rename_overlay(
        &self,
        frame: &mut Frame,
        area: Rect,
        text: &str,
        error: Option<&str>,
    ) {
        let width = area.width.min(60).max(30);
        let height = if error.is_some() { 9u16 } else { 7u16 };
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" Rename ", Style::default().fg(Color::White)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let lines = rename_overlay_lines(text, error);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_snapshot_delete_overlay(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &str,
    ) {
        let width = area.width.min(60).max(30);
        let height = 5u16;
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(" Confirm ", Style::default().fg(Color::White)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let dim = Style::default().fg(Color::DarkGray);
        let white = Style::default().fg(Color::White);

        let lines = vec![
            Line::from(Span::styled(
                format!("Delete snapshot `{}`?", sanitize_for_display(name)),
                white,
            )),
            Line::from(""),
            Line::from(Span::styled("y / Enter confirm \u{00b7} n / Esc cancel", dim)),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }

    pub fn draw_workflow_launch(
        &self,
        frame: &mut Frame,
        area: Rect,
        ws_index: usize,
        workflow_name: &str,
        slots: &[WorkflowSlotChoice],
        active_slot: usize,
        goal: &str,
    ) {
        let width = area.width.min(72).max(44);
        // +10 leaves room for the goal field row and the hint footer.
        let height = (slots.len() as u16 + 10).min(area.height.saturating_sub(2));
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };

        frame.render_widget(Clear, dialog);

        let title = format!(" Launch workflow: {} ", workflow_name);
        let ws_name = self
            .workspaces
            .get(ws_index)
            .map(|w| w.name.clone())
            .unwrap_or_default();

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(Span::styled(
            format!("Workspace: {}", ws_name),
            Style::default().fg(Color::White),
        )));
        lines.push(Line::from(""));
        for (idx, slot) in slots.iter().enumerate() {
            let is_active = idx == active_slot;
            let src_label = match slot.source() {
                WorkflowSlotSource::Existing(si) => {
                    let label = self
                        .workspaces
                        .get(ws_index)
                        .and_then(|w| w.sessions.get(*si))
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| "?".into());
                    format!("existing ({})", label)
                }
                WorkflowSlotSource::New(Engine::ClaudeCode) => "new claude".into(),
                WorkflowSlotSource::New(Engine::Codex) => "new codex".into(),
            };
            let cursor = if is_active { "▸ " } else { "  " };
            let role_style = if is_active {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let value_style = if is_active {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let decorator = if is_active && slot.options.len() > 1 {
                format!("◂ {} ▸", src_label)
            } else {
                src_label.clone()
            };
            lines.push(Line::from(vec![
                Span::raw(cursor),
                Span::styled(format!("{:<10}", slot.role), role_style),
                Span::styled(decorator, value_style),
            ]));
        }
        lines.push(Line::from(""));
        // Goal field (optional). Focused when `active_slot == slots.len()`.
        let goal_focused = active_slot == slots.len();
        let goal_cursor = if goal_focused { "▸ " } else { "  " };
        let goal_label_style = if goal_focused {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let goal_value_style = if goal_focused {
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let goal_display: String = if goal.is_empty() {
            "(optional — overrides {{ goal }})".into()
        } else if goal_focused {
            format!("{}\u{258f}", goal)
        } else {
            goal.to_string()
        };
        lines.push(Line::from(vec![
            Span::raw(goal_cursor),
            Span::styled(format!("{:<10}", "goal"), goal_label_style),
            Span::styled(goal_display, goal_value_style),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "\u{2191}\u{2193} field   \u{2190}\u{2192} choice   Enter: launch   Esc: cancel",
            Style::default().fg(Color::DarkGray),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .style(Style::default().fg(Color::White));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }

    pub fn draw_workflow_history(&self, frame: &mut Frame, area: Rect, run_id: &str) {
        let width = area.width.saturating_sub(4).min(90);
        let height = area.height.saturating_sub(4);
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect { x, y, width, height };
        frame.render_widget(Clear, dialog);

        let run = self.workflow_runs.iter().find(|r| r.run_id == run_id);
        let mut lines: Vec<Line> = Vec::new();
        if let Some(run) = run {
            lines.push(Line::from(Span::styled(
                format!(
                    "{} • iter {} • status: {:?}",
                    run.workflow_name, run.iteration, run.status
                ),
                Style::default().fg(Color::White),
            )));
            lines.push(Line::from(""));
            for h in &run.history {
                let msg = h
                    .last_message
                    .as_deref()
                    .map(|s| {
                        let first = s.lines().next().unwrap_or("");
                        let trimmed: String = first.chars().take(80).collect();
                        trimmed
                    })
                    .unwrap_or("(active)".into());
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("[{:>3}] {:<10}", h.iteration, h.role),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::raw("  "),
                    Span::styled(msg, Style::default().fg(Color::Gray)),
                ]));
            }
            if let Some(reason) = &run.done_reason {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!("done: {}", reason),
                    Style::default().fg(Color::Green),
                )));
            }
        } else {
            lines.push(Line::from("(run not found)"));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Esc / Enter: close",
            Style::default().fg(Color::DarkGray),
        )));
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" Workflow history • {} ", run_id))
            .style(Style::default().fg(Color::White));
        let paragraph = Paragraph::new(lines).block(block);
        frame.render_widget(paragraph, dialog);
    }
}

/// Compute the workflow-level aggregate indicator.
/// Running = any participant session active; Idle = none active; plus Paused/Done.
/// Core readiness predicate for a queued PendingWrite. Pure over inputs so
/// the semantics can be unit-tested without a real PTY.
fn pending_write_ready(wakeups: &[Instant], pw: &PendingWrite, now: Instant) -> bool {
    if now >= pw.hard_deadline {
        return true;
    }
    if now < pw.earliest_deliver_at {
        return false;
    }
    let window = pw.require_quiet;
    !wakeups.iter().any(|t| now.duration_since(*t) < window)
}

/// Return the byte sequence that means "Enter" to whatever's reading the
/// session's PTY right now. Most modern TUIs (codex, claude code) enable
/// the Kitty keyboard protocol (CSI >1u) at startup, which encodes Enter as
/// `\x1b[13u`, not raw `\r`. A raw `\r` written in that mode gets interpreted
/// as a literal carriage-return character appended to the input box instead
/// of as the Enter keystroke — which matches the "prompt shows up with a
/// newline but isn't submitted" symptom.
fn enter_bytes_for(session: &crate::session::Session) -> &'static [u8] {
    enter_bytes_for_mode(*session.term.lock().mode())
}

/// Pure mode → Enter-encoding mapping. Split out from `enter_bytes_for` so
/// the encoding choice is unit-testable without constructing a real `Term`.
fn enter_bytes_for_mode(mode: TermMode) -> &'static [u8] {
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        // Kitty: Enter = CSI 13 u
        b"\x1b[13u"
    } else {
        b"\r"
    }
}

/// Decide the actual byte sequence to write for a workflow delivery body,
/// given the inner program's current terminal mode.
///
/// When the inner program has enabled bracketed-paste mode (`\x1b[?2004h`)
/// AND the body contains at least one newline, wrap the body in
/// `\x1b[200~ … \x1b[201~`. This matches the wrapping used for user-typed
/// pastes (`CrosstermEvent::Paste` handler) and is what codex's input
/// handler expects for large multi-line input. Without it, codex can wedge
/// in a state where the trailing Enter is ignored — the symptom that
/// motivated this helper (see `wf_69fd318f1ad8c4d0` tick.log).
///
/// Single-line bodies stay raw so slash commands like `/clear` aren't
/// rendered as pasted text — the agent needs to recognise them as typed
/// commands. The newline test is conservative: real activation prompts
/// always span multiple lines.
fn format_body_for_delivery(body: &str, term_mode: TermMode) -> Vec<u8> {
    if body.contains('\n') && term_mode.contains(TermMode::BRACKETED_PASTE) {
        let mut out = Vec::with_capacity(body.len() + 12);
        out.extend_from_slice(b"\x1b[200~");
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        body.as_bytes().to_vec()
    }
}

/// Append a diagnostic line for a workflow run to its `tick.log`.
///
/// Lives in `~/.cm/workflow-runs/<run_id>/tick.log`. Rate-limited to at most
/// one distinct message per run per second to avoid spamming the file on every
/// tick of the main loop. Best-effort — ignores all I/O errors.
pub(crate) fn log_tick(run_id: &str, msg: &str) {
    use std::io::Write as _;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Rate-limit: remember the last (run_id, msg) logged and when. Skip if we
    // logged the same thing within the last second.
    static LAST: std::sync::OnceLock<Mutex<Option<(String, String, u64)>>> =
        std::sync::OnceLock::new();
    let lock = LAST.get_or_init(|| Mutex::new(None));
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    {
        let mut guard = match lock.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if let Some((last_run, last_msg, last_ts)) = guard.as_ref() {
            if last_run == run_id && last_msg == msg && now.saturating_sub(*last_ts) < 1 {
                return;
            }
        }
        *guard = Some((run_id.to_string(), msg.to_string(), now));
    }

    let path = workflow::run::run_dir(run_id).join("tick.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let truncate = std::fs::metadata(&path)
        .map(|m| m.len() >= TICK_LOG_MAX_BYTES)
        .unwrap_or(false);
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true);
    if truncate {
        opts.write(true).truncate(true);
    } else {
        opts.append(true);
    }
    if let Ok(mut f) = opts.open(&path) {
        if truncate {
            let _ = writeln!(
                f,
                "{} (log truncated: cap {} bytes hit)",
                now, TICK_LOG_MAX_BYTES
            );
        }
        let _ = writeln!(f, "{} {}", now, msg);
    }
}

fn aggregate_indicator(
    run: &WorkflowRun,
    ws: &Workspace,
    spinner: &'static str,
) -> (&'static str, Style) {
    match run.status {
        workflow::RunStatus::Done => ("\u{2713}", Style::default().fg(Color::Green)),
        workflow::RunStatus::Paused => ("\u{25cf}", Style::default().fg(Color::Yellow)),
        _ => {
            // Match the per-session indicator logic: active iff any participant
            // session tagged with this run_id is Running and not exited.
            let any_running = ws.sessions.iter().any(|ts| {
                ts.workflow_run_id.as_ref() == Some(&run.run_id)
                    && ts.status == SessionStatus::Running
                    && !ts.session.exited
            });
            if any_running {
                (spinner, Style::default().fg(Color::Green))
            } else {
                ("\u{25cf}", Style::default().fg(Color::White))
            }
        }
    }
}

/// Copy text to the system clipboard via the OSC 52 escape sequence.
/// Supported by most modern terminal emulators (kitty, wezterm, iTerm2, alacritty,
/// xterm, and tmux with `set -g set-clipboard on`).
fn copy_to_clipboard(text: &str) {
    use base64::Engine;
    use std::io::Write;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let seq = format!("\x1b]52;c;{}\x1b\\", encoded);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

#[cfg(test)]
mod manifest_entry_seeded_from_tests {
    //! Lock in the persistence semantics of `ManifestEntry.seeded_from_snapshot`:
    //! the serde-default lets pre-existing manifests load cleanly, and the
    //! skip-if-none keeps the on-disk format quiet when the field is absent.
    use super::*;

    #[test]
    fn seeded_from_snapshot_round_trips() {
        let entry = ManifestEntry {
            uid: "ts-abc".into(),
            managed_by_uid: None,
            generation: 0,
            label: "x".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            task_id: None,
            notify_on_idle: false,
            seeded_from_snapshot: Some("reviewer-strict".into()),
        };
        let bytes = serde_json::to_vec(&entry).unwrap();
        let back: ManifestEntry = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.seeded_from_snapshot.as_deref(), Some("reviewer-strict"));
    }

    #[test]
    fn missing_seeded_from_deserializes_as_none() {
        // Pre-existing manifests written before this field existed must
        // continue to load. `#[serde(default)]` guarantees that.
        let json = r#"{
            "uid": "ts-abc",
            "generation": 0,
            "label": "x",
            "session_type": "claude",
            "hidden": false,
            "idle_timeout_secs": 0,
            "burst_threshold": 0,
            "notify_on_idle": false
        }"#;
        let entry: ManifestEntry = serde_json::from_str(json).unwrap();
        assert!(entry.seeded_from_snapshot.is_none());
    }

    #[test]
    fn none_does_not_serialize() {
        let entry = ManifestEntry {
            uid: "ts-abc".into(),
            managed_by_uid: None,
            generation: 0,
            label: "x".into(),
            session_type: "claude".into(),
            transcript_id: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: None,
            workflow_role: None,
            task_id: None,
            notify_on_idle: false,
            seeded_from_snapshot: None,
        };
        let s = serde_json::to_string(&entry).unwrap();
        assert!(
            !s.contains("seeded_from_snapshot"),
            "None should be skipped, got: {s}"
        );
    }
}

#[cfg(test)]
mod ready_tests {
    use super::*;

    fn pw(floor_secs: u64, quiet_secs: u64, deadline_secs: u64) -> (PendingWrite, Instant) {
        let now = Instant::now();
        (
            PendingWrite {
                text: "hi".into(),
                submit: true,
                earliest_deliver_at: now + Duration::from_secs(floor_secs),
                require_quiet: Duration::from_secs(quiet_secs),
                hard_deadline: now + Duration::from_secs(deadline_secs),
            },
            now,
        )
    }

    #[test]
    fn not_ready_before_floor() {
        let (p, now) = pw(5, 2, 60);
        // Early — floor not reached
        assert!(!pending_write_ready(&[], &p, now));
        // At floor with no wakeups — ready
        assert!(pending_write_ready(&[], &p, now + Duration::from_secs(5)));
    }

    #[test]
    fn not_ready_while_pty_noisy() {
        let (p, now) = pw(1, 2, 60);
        let check_at = now + Duration::from_secs(3);
        // Wakeup 0.5s ago — still within quiet window
        let recent = check_at - Duration::from_millis(500);
        assert!(!pending_write_ready(&[recent], &p, check_at));
    }

    #[test]
    fn ready_after_pty_goes_quiet() {
        let (p, now) = pw(1, 2, 60);
        let check_at = now + Duration::from_secs(10);
        // Last wakeup 5s ago — outside 2s quiet window
        let old = check_at - Duration::from_secs(5);
        assert!(pending_write_ready(&[old], &p, check_at));
    }

    #[test]
    fn deadline_forces_delivery_even_if_noisy() {
        let (p, now) = pw(1, 2, 10);
        let check_at = now + Duration::from_secs(11);
        let recent = check_at - Duration::from_millis(100); // noisy
        assert!(pending_write_ready(&[recent], &p, check_at));
    }

    #[test]
    fn empty_wakeups_is_ready_past_floor() {
        let (p, now) = pw(1, 2, 60);
        assert!(pending_write_ready(&[], &p, now + Duration::from_secs(2)));
    }
}

#[cfg(test)]
mod enter_encoding_tests {
    use super::*;

    #[test]
    fn raw_cr_when_kitty_mode_off() {
        let mode = TermMode::empty();
        assert_eq!(enter_bytes_for_mode(mode), b"\r");
    }

    #[test]
    fn kitty_csi_when_disambiguate_on() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES;
        assert_eq!(enter_bytes_for_mode(mode), b"\x1b[13u");
    }

    #[test]
    fn kitty_csi_when_disambiguate_set_alongside_other_modes() {
        // Real sessions carry many mode bits at once. We only care about the
        // one that drives Enter encoding.
        let mode = TermMode::DISAMBIGUATE_ESC_CODES
            | TermMode::ALT_SCREEN
            | TermMode::BRACKETED_PASTE;
        assert_eq!(enter_bytes_for_mode(mode), b"\x1b[13u");
    }
}

#[cfg(test)]
mod body_delivery_tests {
    //! Pins down the byte-formatting we use when delivering a workflow
    //! activation prompt body to a session's PTY. The hypothesis driving
    //! these tests: codex's input handler wedges on large multi-line raw
    //! writes — the trailing Enter is ignored — but accepts the same content
    //! cleanly when wrapped in bracketed-paste markers (`\x1b[200~ … \x1b[201~`),
    //! the same wrapping we already use for user-typed pastes (see the
    //! `CrosstermEvent::Paste` handler).
    //!
    //! These are unit tests over the byte-formatting helper alone; they
    //! don't validate codex's runtime behavior. Final confirmation is a
    //! manual reproduction in the TUI against a real codex worker session.

    use super::*;

    #[test]
    fn multiline_body_wrapped_when_bracketed_paste_enabled() {
        let mode = TermMode::DISAMBIGUATE_ESC_CODES | TermMode::BRACKETED_PASTE;
        let body = "do thing\n\nstep 1\nstep 2";
        let out = format_body_for_delivery(body, mode);
        assert!(
            out.starts_with(b"\x1b[200~"),
            "multi-line body should start with paste-begin marker: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            out.ends_with(b"\x1b[201~"),
            "multi-line body should end with paste-end marker: {:?}",
            String::from_utf8_lossy(&out)
        );
        let expected = format!("\x1b[200~{}\x1b[201~", body);
        assert_eq!(out, expected.as_bytes());
    }

    #[test]
    fn multiline_body_raw_when_bracketed_paste_disabled() {
        // Older / non-bracket-paste-aware agents see raw bytes. We must not
        // emit paste markers because the agent would render them as literal
        // `[200~`, `[201~` in its input box.
        let mode = TermMode::DISAMBIGUATE_ESC_CODES; // no BRACKETED_PASTE
        let body = "do thing\nstep 1\nstep 2";
        let out = format_body_for_delivery(body, mode);
        assert_eq!(out, body.as_bytes());
    }

    #[test]
    fn single_line_body_stays_raw_even_with_bracketed_paste() {
        // Slash commands (`/clear`, `/compact`, etc.) are always single-line.
        // Wrapping them in paste markers risks the agent treating them as
        // pasted text instead of a typed command. Newline absence is the
        // signal: real activation prompts always span multiple lines.
        let mode = TermMode::BRACKETED_PASTE;
        let body = "/clear";
        let out = format_body_for_delivery(body, mode);
        assert_eq!(out, body.as_bytes());
    }

    #[test]
    fn empty_body_stays_raw() {
        let mode = TermMode::BRACKETED_PASTE;
        let out = format_body_for_delivery("", mode);
        assert!(out.is_empty());
    }

    #[test]
    fn embedded_paste_end_marker_is_preserved_verbatim() {
        // We don't try to escape an embedded \x1b[201~ in the body — if the
        // user really included one in an activation prompt, the agent would
        // see paste-end early. This test pins that we do NOT silently
        // mutate the body; if escaping is ever needed, this test will be
        // the place to revisit.
        let mode = TermMode::BRACKETED_PASTE;
        let body = "line one\nweird \x1b[201~ marker\nline three";
        let out = format_body_for_delivery(body, mode);
        let expected = format!("\x1b[200~{}\x1b[201~", body);
        assert_eq!(out, expected.as_bytes());
    }
}

#[cfg(test)]
mod transcript_rebind_tests {
    //! Pins down the invariant that any rebind of `transcript_id` after
    //! the initial bind bumps `generation`. Without this, a reader holding
    //! a cursor against the pre-rebind file would skip messages in the new
    //! file (cursor offset N applied to a different file).

    use super::*;
    use std::collections::{BTreeMap, HashMap};

    fn make_test_session(transcript_id: Option<&str>, generation: u64) -> TerminalSession {
        // /bin/true exits immediately; the PTY/Session shell is harmless
        // for a value-only test that never reads the PTY.
        let session =
            crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
                .expect("session for test");
        TerminalSession {
            uid: "uid".into(),
            label: "test".into(),
            session_type: "claude".into(),
            session,
            status: SessionStatus::Idle,
            last_write_at: None,
            transcript_id: transcript_id.map(str::to_string),
            generation,
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            task_id: None,
            last_delivery: None,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
        }
    }

    #[test]
    fn rebind_to_new_sid_bumps_generation() {
        let mut ts = make_test_session(Some("old-sid"), 5);
        ts.rebind_transcript(Some("new-sid".into()));
        assert_eq!(ts.transcript_id.as_deref(), Some("new-sid"));
        assert_eq!(ts.generation, 6);
    }

    #[test]
    fn rebind_to_none_bumps_generation() {
        // /clear path: transcript becomes None until the detector picks
        // up the freshly-rotated file. Generation must bump immediately
        // so cursors held by readers are invalidated before the next
        // file binds.
        let mut ts = make_test_session(Some("old-sid"), 1);
        ts.rebind_transcript(None);
        assert!(ts.transcript_id.is_none());
        assert_eq!(ts.generation, 2);
    }

    #[test]
    fn rebind_saturates_at_u64_max() {
        let mut ts = make_test_session(Some("old"), u64::MAX);
        ts.rebind_transcript(Some("new".into()));
        assert_eq!(ts.generation, u64::MAX, "must not panic on overflow");
    }

    #[test]
    fn workflow_rebind_resets_active_role_to_new_sid() {
        let run_id = "wf_rebind";
        let mut role_sessions = BTreeMap::new();
        role_sessions.insert(
            "worker".to_string(),
            workflow::run::RoleBinding {
                session_label: "worker".into(),
                current_session_id: Some("old-sid".into()),
            },
        );
        let mut role_baselines = BTreeMap::new();
        role_baselines.insert(
            "worker".to_string(),
            workflow::run::MessageBaseline {
                user_count: 4,
                assistant_count: 7,
            },
        );
        let mut run = WorkflowRun::new(
            run_id.to_string(),
            "feedback".into(),
            "ws-1".into(),
            role_sessions,
            "worker".into(),
            role_baselines,
            None,
            BTreeMap::new(),
        );
        run.history[0].assistant_count_at_start = 7;
        let mut runs = vec![run];

        assert!(note_workflow_transcript_binding(
            &mut runs,
            run_id,
            "worker",
            Some("old-sid"),
            "new-sid",
        ));

        let run = &runs[0];
        assert_eq!(
            run.role_sessions["worker"].current_session_id.as_deref(),
            Some("new-sid")
        );
        assert_eq!(run.role_baselines["worker"].assistant_count, 0);
        assert_eq!(run.role_baselines["worker"].user_count, 0);
        assert_eq!(run.history[0].session_id.as_deref(), Some("new-sid"));
        assert_eq!(run.history[0].assistant_count_at_start, 0);
    }
}

#[cfg(test)]
mod rotation_binding_tests {
    //! Regression tests for the `/clear` and `/compact` rotation rebind
    //! path. The pre-fix version only rebound workflow roles, so a
    //! regular `A-n` Claude pane that ran `/clear` would keep resolving
    //! to the *old* transcript file forever and `read_session_output`
    //! returned stale data on what looked like a healthy session.

    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::Instant;

    fn dummy_session() -> Session {
        Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
            .expect("dummy session")
    }

    fn ts_with(
        sid: Option<&str>,
        workflow: Option<(&str, &str)>,
    ) -> TerminalSession {
        TerminalSession {
            uid: "uid".into(),
            label: "test".into(),
            session_type: "claude".into(),
            session: dummy_session(),
            status: SessionStatus::Idle,
            last_write_at: None,
            transcript_id: sid.map(str::to_string),
            generation: 0,
            pending_jsonl_files: None,
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: workflow.map(|(r, _)| r.to_string()),
            workflow_role: workflow.map(|(_, role)| role.to_string()),
            task_id: None,
            last_delivery: None,
            notify_on_idle: false,
            pending_enter: None,
            created_at: Instant::now(),
            managed_by_uid: None,
            seeded_from_snapshot: None,
        }
    }

    fn ws_with(sessions: Vec<TerminalSession>) -> Workspace {
        Workspace {
            id: "ws-1".into(),
            name: "ws".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(PathBuf::from("/tmp/ws")),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            sessions,
            tombstones: vec![],
            is_pushing: false,
        }
    }

    #[test]
    fn binding_includes_non_workflow_claude_session() {
        // The fix: a regular pane (no workflow_run_id/role) with a
        // bound transcript_id must still appear in the rotation
        // bindings map. Without this, `/clear` from that pane never
        // rebound and `read_session_output` stalled on the old file.
        let ws = ws_with(vec![ts_with(Some("solo-sid"), None)]);
        let bindings = collect_rotation_bindings(&[ws]);
        assert!(
            bindings.contains_key("solo-sid"),
            "non-workflow session must be tracked for rotation; got keys {:?}",
            bindings.keys().collect::<Vec<_>>(),
        );
        let b = bindings.get("solo-sid").unwrap();
        assert!(b.workflow.is_none());
    }

    #[test]
    fn binding_includes_workflow_claude_session() {
        let ws = ws_with(vec![ts_with(
            Some("worker-sid"),
            Some(("wf_1", "worker")),
        )]);
        let bindings = collect_rotation_bindings(&[ws]);
        let b = bindings
            .get("worker-sid")
            .expect("workflow session must still be tracked");
        assert_eq!(
            b.workflow.as_ref().map(|(r, role)| (r.as_str(), role.as_str())),
            Some(("wf_1", "worker"))
        );
    }

    #[test]
    fn binding_skips_session_without_transcript_id() {
        // No bound transcript yet — nothing to rotate from.
        let ws = ws_with(vec![ts_with(None, None)]);
        assert!(collect_rotation_bindings(&[ws]).is_empty());
    }

    #[test]
    fn binding_skips_codex_session() {
        // Codex doesn't go through history.jsonl rotation — its session
        // metadata is in the rollout file itself. Skip from this map.
        let mut ts = ts_with(Some("codex-sid"), None);
        ts.session_type = "codex".into();
        let ws = ws_with(vec![ts]);
        assert!(collect_rotation_bindings(&[ws]).is_empty());
    }

    #[test]
    fn binding_skips_workspace_without_worktree() {
        // Cloud workspaces clear `worktree_path` after `push_active`.
        // Without the path we can't compute the project dir, so skip.
        let mut ws = ws_with(vec![ts_with(Some("sid"), None)]);
        ws.worktree_path = None;
        assert!(collect_rotation_bindings(&[ws]).is_empty());
    }

    /// End-to-end-ish: write two sequential transcript files for the
    /// same logical session under `~/.claude/projects/<encoded>/`,
    /// confirm `find_post_rotation_sid` picks up the newer one given
    /// a rotation timestamp between the two file timestamps.
    #[test]
    fn find_post_rotation_picks_newer_transcript() {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let worktree = std::path::PathBuf::from("/tmp/myrepo");
        // Encoded path matches the agent module's rule: '/' and '.' → '-'.
        let encoded = "-tmp-myrepo";
        let proj = tmp.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj).unwrap();

        // Old transcript: 2026-01-01T00:00:01Z = 1767225601000 ms.
        let old_line = r#"{"timestamp":"2026-01-01T00:00:01.000Z","type":"user","message":{"role":"user","content":"old"}}"#;
        std::fs::write(proj.join("old-sid.jsonl"), old_line).unwrap();

        // Rotation marker between the two transcripts. With the 2s
        // slack in `find_post_rotation_sid`, the old file's
        // `first_ts + 2000 < after_ms` filter requires after_ms to be
        // strictly greater than 1767225603000.
        let rotation_at = 1767225604000_u64;

        // New transcript: 2026-01-01T00:00:06Z = 1767225606000 ms.
        let new_line = r#"{"timestamp":"2026-01-01T00:00:06.000Z","type":"user","message":{"role":"user","content":"new"}}"#;
        std::fs::write(proj.join("new-sid.jsonl"), new_line).unwrap();

        let found = workflow::history::find_post_rotation_sid(&worktree, rotation_at);

        unsafe {
            if let Some(h) = old_home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }

        assert_eq!(
            found.as_deref(),
            Some("new-sid"),
            "must pick the newer transcript (timestamp >= rotation)",
        );
    }
}

#[cfg(test)]
mod start_workflow_prompt_selection_tests {
    //! Pins down what `deliver_initial_workflow_prompt` will queue for
    //! the initial role under each combination of inputs. Without this
    //! coverage, an `start_workflow` call with a workflow whose initial
    //! role has no activation_prompt and a None goal would silently
    //! ship-idle (the bug Phase 4 review caught), AND a goal containing
    //! literal `{{` would be mangled by the renderer.

    use super::prepare_initial_prompt;
    use std::cell::Cell;

    /// Marker render function — wraps so we can assert the renderer
    /// was (or wasn't) called.
    fn marker_render(template: &str) -> String {
        format!("RENDERED({})", template)
    }

    fn no_render(_template: &str) -> String {
        panic!("renderer must not be called on the goal-only path");
    }

    #[test]
    fn prefers_activation_prompt_when_set() {
        let got = prepare_initial_prompt(
            Some("from-template"),
            Some("goal-text"),
            marker_render,
        );
        // Renderer was called with the template.
        assert_eq!(got.as_deref(), Some("RENDERED(from-template)"));
    }

    #[test]
    fn falls_back_to_goal_when_no_activation_prompt() {
        let got = prepare_initial_prompt(None, Some("goal-text"), no_render);
        assert_eq!(got.as_deref(), Some("goal-text"));
    }

    #[test]
    fn goal_with_mustache_braces_is_not_rendered() {
        // Regression for Phase 4 review: a goal containing literal
        // `{{` (e.g. user pastes a Mustache example, or a code
        // fragment with `{{ x }}`, or a JSON template) was being
        // routed through the renderer in the fallback path and
        // either mangled or emptied. Now the goal path bypasses
        // render entirely.
        let goal = "Implement {{user.name}} substitution in the templating engine.";
        let got = prepare_initial_prompt(None, Some(goal), no_render);
        assert_eq!(
            got.as_deref(),
            Some(goal),
            "goal must pass through verbatim — `{{{{` survived intact"
        );
    }

    #[test]
    fn returns_none_when_both_missing() {
        // Workflow whose initial role has no template AND no goal
        // provided — caller's choice to ship a no-op launch. We
        // surface this as None so the caller can log + skip rather
        // than queueing an empty prompt.
        let got = prepare_initial_prompt(None, None, no_render);
        assert!(got.is_none());
    }

    #[test]
    fn empty_or_whitespace_activation_prompt_falls_through_to_goal() {
        // Treat an empty-after-trim template as "not set" — no point
        // queueing pure whitespace through the renderer either.
        let got = prepare_initial_prompt(Some("   "), Some("real-goal"), no_render);
        assert_eq!(got.as_deref(), Some("real-goal"));
    }

    #[test]
    fn empty_goal_with_no_template_returns_none() {
        let got = prepare_initial_prompt(None, Some("\n\t  "), no_render);
        assert!(got.is_none());
    }

    #[test]
    fn renderer_is_called_at_most_once() {
        // The renderer is `FnOnce` — confirm the implementation
        // doesn't double-invoke it (which would matter if it had
        // side effects like mutating shared state).
        let calls = Cell::new(0);
        let _ = prepare_initial_prompt(
            Some("template"),
            Some("goal"),
            |t| {
                calls.set(calls.get() + 1);
                t.to_string()
            },
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn renderer_is_not_called_when_goal_path_taken() {
        // Confirms the lazy-build property: the renderer closure is
        // never invoked when activation_prompt is absent.
        let calls = Cell::new(0);
        let _ = prepare_initial_prompt(
            None,
            Some("goal"),
            |t| {
                calls.set(calls.get() + 1);
                t.to_string()
            },
        );
        assert_eq!(calls.get(), 0);
    }
}

#[cfg(test)]
mod activity_summary_tests {
    //! Phase 6 activity feed. Pins the read-only/mutating partition (only
    //! mutating methods get logged) and the formatting of the most-used
    //! summary lines. The doc on `activity_summary_for` says new mutating
    //! methods MUST be added explicitly; these tests fail-loud if a
    //! mutating method is added without surfacing in the feed.

    use super::activity_summary_for;
    use serde_json::json;

    #[test]
    fn read_only_methods_are_silent() {
        // None of these may produce a summary — they're high-frequency
        // observability calls and would drown out real mutations.
        for m in [
            "ping",
            "resolve_authorized_session",
            "list_sessions",
            "list_workflows",
            "list_subtasks",
            "get_workflow_state",
        ] {
            assert!(
                activity_summary_for(m, &json!({}), &json!({})).is_none(),
                "{m} must NOT produce an activity-feed entry"
            );
        }
    }

    #[test]
    fn unknown_method_silent_by_default() {
        // Defensive: a method that isn't in the explicit list (e.g. a
        // new control-socket method added without thinking about
        // observability) defaults to no-log. The author has to come
        // here and add a branch — surfacing the omission rather than
        // silently dropping it.
        assert!(
            activity_summary_for("totally_made_up_method", &json!({}), &json!({})).is_none()
        );
    }

    #[test]
    fn send_input_summarizes_target_and_text_snippet() {
        let s = activity_summary_for(
            "send_input",
            &json!({"session_uid": "ts-12345678abcdefXX", "text": "hello world"}),
            &json!({}),
        )
        .expect("send_input is mutating");
        // Truncates uid to 8 chars and quotes the text.
        assert!(s.contains("ts-12345"), "{s}");
        assert!(s.contains("\"hello world\""), "{s}");
    }

    #[test]
    fn send_input_truncates_long_text_with_ellipsis() {
        let long = "x".repeat(200);
        let s = activity_summary_for(
            "send_input",
            &json!({"session_uid": "ts-AAAAAAAA", "text": long}),
            &json!({}),
        )
        .expect("send_input is mutating");
        // Snippet is at most ~40 chars + a "…" suffix.
        assert!(s.contains("…"), "expected truncation marker in {s}");
        // Sanity: the full 200-char run isn't in there.
        assert!(!s.contains(&"x".repeat(200)));
    }

    #[test]
    fn create_subtask_appends_new_task_id() {
        let s = activity_summary_for(
            "create_subtask",
            &json!({"name": "demo", "worktree_mode": "branch"}),
            &json!({"task_id": "abcd1234-deadbeef", "worktree_path": "/tmp/wt"}),
        )
        .expect("create_subtask is mutating");
        // Format is "create_subtask(<name>, <mode>) → <new-id-prefix>".
        assert!(s.starts_with("create_subtask(demo, branch)"), "{s}");
        assert!(s.contains("→"), "{s}");
        assert!(s.contains("abcd1234"), "{s}");
    }

    #[test]
    fn mark_subtask_done_includes_close_worktree_flag() {
        let s = activity_summary_for(
            "mark_subtask_done",
            &json!({"task_id": "task-uuid-v1", "close_worktree": true}),
            &json!({"ok": true, "worktree_removed": true}),
        )
        .expect("mark_subtask_done is mutating");
        assert!(s.contains("close_worktree=true"), "{s}");
    }

    #[test]
    fn start_workflow_truncates_task_id() {
        let s = activity_summary_for(
            "start_workflow",
            &json!({
                "workflow_name": "feedback",
                "task_id": "1914682b-b633-4d15-9df6-20ba036427bc",
                "goal": "anything",
            }),
            &json!({"run_id": "wf_xxx"}),
        )
        .expect("start_workflow is mutating");
        assert!(s.starts_with("start_workflow(feedback"), "{s}");
        assert!(s.contains("task=1914682b"), "{s}");
        // Full UUID must not bleed through — the column would overflow.
        assert!(!s.contains("20ba036427bc"), "{s}");
    }
}

#[cfg(test)]
mod entry_matches_delivery_tests {
    //! Regression coverage for the workflow-binding path that broke
    //! during the overnight cleanup orchestration. Each `parse_entries`
    //! input below was copied verbatim from a real history.jsonl line
    //! produced by the stuck workflow workers — this is the *exact*
    //! data shape the production code has to handle.

    use super::entry_matches_delivery;
    use crate::workflow::history;

    /// First 120 chars of the goal we delivered to the cli-cleanup worker.
    /// That worker's actual transcript ended with `stop_reason: end_turn`
    /// at 2026-05-09T04:36:18 — but the binding never landed because
    /// neither `display` nor `paste_content` on the matching history
    /// entry started with this prefix.
    const CLEANUP_GOAL_PREFIX: &str =
        "Fix two real bugs in the CLI in /home/lucas/.cm/worktrees/cm-sub-allow-claudes-to-spawn-and-manage-tasks-cleanup-cli";

    /// Real production line from ~/.claude/history.jsonl on 2026-05-09 —
    /// the one that left the cli-cleanup worker stuck on iteration 1.
    /// `pastedContents` carries `contentHash` only; there is no `content`
    /// field, so `paste_content` parses to "".
    const REAL_POST_2025_PASTE_LINE: &str = r#"{"display":"[Pasted text #1 +11 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"d07c78137ebcc578"}},"timestamp":1778301350854,"project":"/home/lucas/.cm/worktrees/cm-sub-allow-claudes-to-spawn-and-manage-tasks-cleanup-cli-bucket-and-config-c316cc3","sessionId":"7cc30907-9cfd-458e-a9fa-896745af5b1a"}"#;

    /// Pre-fix simulation: what the matcher used to look at — display +
    /// paste_content prefix only. If THIS evaluates true on
    /// `REAL_POST_2025_PASTE_LINE`, the bug never existed and our fix is
    /// fixing nothing. (It MUST evaluate false to demonstrate the regression.)
    fn pre_fix_match(entry: &history::HistoryEntry, prefix: &str) -> bool {
        !prefix.is_empty()
            && (entry.display.starts_with(prefix)
                || entry.paste_content.starts_with(prefix))
    }

    #[test]
    fn pre_fix_matcher_does_not_recover_post_2025_pastes() {
        let entries = parse(REAL_POST_2025_PASTE_LINE);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];

        // Confirm the parser produces the shape we expect: display is the
        // placeholder, paste_content is empty.
        assert_eq!(e.display, "[Pasted text #1 +11 lines]");
        assert_eq!(e.paste_content, "");

        // Old logic — the regression we're fixing. This MUST be false on
        // the real production line, otherwise our fix is treating a non-bug.
        assert!(
            !pre_fix_match(e, CLEANUP_GOAL_PREFIX),
            "pre-fix matcher should NOT have matched the post-2025 paste \
             entry — that's exactly why all 5 cleanup workflows stalled. \
             If this assertion fires, the diagnosis is wrong."
        );
    }

    #[test]
    fn post_fix_matcher_recovers_post_2025_pastes() {
        let entries = parse(REAL_POST_2025_PASTE_LINE);
        let e = &entries[0];

        // New logic — the production code path. With the placeholder
        // detector, the SAME real-world line now correlates.
        assert!(
            entry_matches_delivery(e, CLEANUP_GOAL_PREFIX),
            "post-fix matcher must accept the post-2025 paste placeholder \
             so resolve_pending_deliveries can bind the session"
        );
    }

    #[test]
    fn legacy_plain_typed_input_still_matches() {
        // Pre-paste-redaction era: short typed prompts log raw text into
        // `display`. Mustn't regress.
        let line = r#"{"display":"Implement the feedback workflow","pastedContents":{},"timestamp":1,"project":"/p","sessionId":"s"}"#;
        let e = &parse(line)[0];
        assert!(entry_matches_delivery(e, "Implement the feedback"));
    }

    #[test]
    fn legacy_paste_with_content_field_still_matches() {
        // Pre-2025 paste schema where the raw text was inlined as
        // `pastedContents.<k>.content`. Parser surfaces it via
        // `paste_content`. Mustn't regress.
        let line = r#"{"display":"[Pasted text #1 +3 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"Fix the broken thing\nin the place\nthat is broken"}},"timestamp":1,"project":"/p","sessionId":"s"}"#;
        let e = &parse(line)[0];
        assert!(entry_matches_delivery(e, "Fix the broken"));
    }

    #[test]
    fn empty_prefix_never_matches() {
        // Defensive: a session with no recorded `last_delivery` (or one
        // whose prefix was somehow trimmed to zero) must not bind to
        // every history entry it sees.
        let e = &parse(REAL_POST_2025_PASTE_LINE)[0];
        assert!(!entry_matches_delivery(e, ""));
    }

    #[test]
    fn typed_input_does_not_false_match_the_placeholder_text() {
        // If a user literally types the placeholder string (improbable
        // but possible), `display` matches by exact prefix, not the
        // placeholder fallback. This covers a corner of the matching
        // priority ordering — verifying both paths agree on this case.
        let line = r#"{"display":"[Pasted text #1 +5 lines] is funny syntax","pastedContents":{},"timestamp":1,"project":"/p","sessionId":"s"}"#;
        let e = &parse(line)[0];
        assert!(entry_matches_delivery(e, "[Pasted text #1"));
        // It also matches as a placeholder, which is fine — both paths
        // agree on this entry. The test is documentation: don't try to
        // "fix" the overlap; the matcher is intentionally OR-shaped.
    }

    /// Test helper: parse via the same parse_entries the production code
    /// uses. We can't import it directly (private to history.rs) but a
    /// single-line input through the public `HistoryWatcher::poll` is
    /// awkward to set up. Instead, exercise the public path by writing
    /// to a tempfile and reading back — but for these correlation tests
    /// we only care about the parsed shape, so reuse the test-only
    /// shim below that round-trips through a one-element vec.
    fn parse(line: &str) -> Vec<history::HistoryEntry> {
        // The simplest in-test parse path: use serde to project the raw
        // JSON onto the same fields parse_entries extracts. We test
        // parse_entries itself in workflow::history::tests; here we
        // just need a constructor.
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        let display = v.get("display").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let timestamp_ms = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
        let project = v.get("project").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let session_id = v
            .get("sessionId")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let mut paste_content = String::new();
        if let Some(map) = v.get("pastedContents").and_then(|x| x.as_object()) {
            for (_, val) in map {
                if let Some(s) = val.get("content").and_then(|x| x.as_str()) {
                    if !paste_content.is_empty() {
                        paste_content.push('\n');
                    }
                    paste_content.push_str(s);
                }
            }
        }
        vec![history::HistoryEntry {
            display,
            timestamp_ms,
            project,
            session_id,
            paste_content,
        }]
    }
}

#[cfg(test)]
mod input_handler_tests {
    //! Per-mode handler tests. Each input mode is exercised through its
    //! free `handle_<mode>` function with synthesized `CrosstermEvent::Key`
    //! events — no `App`, no PTY, no terminal. Behaviors pinned here:
    //!   - ESC → InputOutcome::Cancel (closes the modal),
    //!   - ENTER → InputOutcome::Submit(...) (close + side effect),
    //!   - BACKSPACE → mode-payload buffer mutated in place.
    //! These guard the behavior contract that pre-extraction was only
    //! enforced by the call site of `handle_input_event`.
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    fn ctx_no_repos<'a>() -> InputCtx<'a> {
        InputCtx { repo_urls: &[] }
    }

    fn assert_consumed(o: &InputOutcome) {
        assert!(
            matches!(o, InputOutcome::Consumed),
            "expected Consumed, got {:?}",
            o
        );
    }

    fn assert_cancel(o: &InputOutcome) {
        assert!(
            matches!(o, InputOutcome::Cancel),
            "expected Cancel, got {:?}",
            o
        );
    }

    // ── NewSession ────────────────────────────────────────────────

    fn new_session_state(
        label: &str,
        branch: &str,
        timeout: &str,
        repo: &str,
        active: u8,
    ) -> (String, String, String, String, u8) {
        (
            label.to_string(),
            branch.to_string(),
            timeout.to_string(),
            repo.to_string(),
            active,
        )
    }

    #[test]
    fn new_session_esc_cancels() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("hello", "", "2", "", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
        // Buffers preserved — Cancel doesn't clear input.
        assert_eq!(label, "hello");
    }

    #[test]
    fn new_session_backspace_pops_label_buffer() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("foo", "", "2", "", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(label, "fo");
    }

    #[test]
    fn new_session_enter_with_label_submits_create_local_session() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("my-task", "feat/x", "10", "https://github.com/a/b", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::CreateLocalSession {
                repo_url,
                label,
                branch,
                idle_timeout_secs,
                seed_from,
            }) => {
                assert_eq!(repo_url, "https://github.com/a/b");
                assert_eq!(label, "my-task");
                assert_eq!(branch.as_deref(), Some("feat/x"));
                assert_eq!(idle_timeout_secs, 10);
                assert!(seed_from.is_none());
            }
            other => panic!("expected Submit(CreateLocalSession), got {:?}", other),
        }
    }

    #[test]
    fn new_session_enter_with_blank_label_stays_open() {
        // When the label is empty, Enter is consumed but the modal does
        // NOT close — pre-extraction behavior was `return true` without
        // touching `input_mode`.
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("   ", "", "2", "", 1);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        assert_consumed(&outcome);
    }

    #[test]
    fn new_session_char_appends_only_to_active_field() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("", "", "2", "", 2);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('x')),
        );
        assert_consumed(&outcome);
        assert_eq!(label, "");
        assert_eq!(branch, "x");
    }

    #[test]
    fn new_session_right_cycles_repo_when_field_zero() {
        let urls = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("", "", "2", "b", 0);
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut None,
                active_field: &mut active,
            },
            InputCtx { repo_urls: &urls },
            &key(KeyCode::Right),
        );
        assert_consumed(&outcome);
        assert_eq!(repo, "c");
    }

    // ── NewSession seed-from (chunk 5) ────────────────────────────

    #[test]
    fn new_session_tab_cycles_through_five_fields() {
        // 0 → 1 → 2 → 3 → 4 → 0
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("", "", "", "", 0);
        let mut seed: Option<String> = None;
        for expected in [1, 2, 3, 4, 0] {
            handle_new_session(
                NewSessionMut {
                    label_text: &mut label,
                    branch_text: &mut branch,
                    idle_timeout_text: &mut timeout,
                    repo_url: &mut repo,
                    seed_from: &mut seed,
                    active_field: &mut active,
                },
                ctx_no_repos(),
                &key(KeyCode::Tab),
            );
            assert_eq!(active, expected);
        }
    }

    #[test]
    fn new_session_enter_on_seed_field_opens_picker_with_form_state() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("my-task", "feat/x", "12", "https://github.com/o/r", 4);
        let mut seed: Option<String> = None;
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(
                SubmitAction::OpenSnapshotPickerForNewSession {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    existing_seed_from,
                },
            ) => {
                assert_eq!(label_text, "my-task");
                assert_eq!(branch_text, "feat/x");
                assert_eq!(idle_timeout_text, "12");
                assert_eq!(repo_url, "https://github.com/o/r");
                assert!(existing_seed_from.is_none());
            }
            other => panic!(
                "expected OpenSnapshotPickerForNewSession, got {other:?}"
            ),
        }
    }

    #[test]
    fn new_session_esc_on_seed_field_with_value_clears_seed_only() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("x", "", "2", "", 4);
        let mut seed: Option<String> = Some("reviewer".into());
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_consumed(&outcome);
        assert!(seed.is_none(), "seed_from should have been cleared");
        assert_eq!(label, "x", "other form fields untouched");
    }

    #[test]
    fn new_session_esc_on_other_fields_still_cancels() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("x", "", "2", "", 1);
        let mut seed: Option<String> = None;
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    #[test]
    fn new_session_submit_carries_seed_from_when_set() {
        let (mut label, mut branch, mut timeout, mut repo, mut active) =
            new_session_state("task", "", "2", "https://github.com/a/b", 1);
        let mut seed: Option<String> = Some("reviewer-strict".into());
        let outcome = handle_new_session(
            NewSessionMut {
                label_text: &mut label,
                branch_text: &mut branch,
                idle_timeout_text: &mut timeout,
                repo_url: &mut repo,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::CreateLocalSession {
                seed_from, ..
            }) => assert_eq!(seed_from.as_deref(), Some("reviewer-strict")),
            other => panic!("expected CreateLocalSession, got {other:?}"),
        }
    }

    // ── NewTerminalSession ────────────────────────────────────────

    #[test]
    fn new_terminal_session_j_cycles_type_forward() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed = None;
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-7",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('j')),
        );
        assert_consumed(&outcome);
        assert_eq!(session_type, "codex");
    }

    #[test]
    fn new_terminal_session_enter_submits_with_payload() {
        let mut session_type = "bash".to_string();
        let task_id = Some("t-123".to_string());
        let mut seed = None;
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-4",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SpawnSessionOnWorkspace {
                workspace_id,
                session_type,
                task_id,
                seed_from,
            }) => {
                assert_eq!(workspace_id, "ws-4");
                assert_eq!(session_type, "bash");
                assert_eq!(task_id, Some("t-123".to_string()));
                assert!(seed_from.is_none());
            }
            other => panic!("expected SpawnSessionOnWorkspace, got {:?}", other),
        }
    }

    #[test]
    fn new_terminal_session_esc_cancels() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed = None;
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // ── NewTerminalSession seed-from (chunk 5) ────────────────────

    #[test]
    fn new_terminal_session_j_on_field_0_clears_seed_from() {
        // Engine change invalidates a previously picked snapshot (picker
        // filters by engine). Must clear seed_from to avoid carrying a
        // claude-code snapshot across to a codex session.
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = Some("reviewer".into());
        let mut active = 0u8;
        handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('j')),
        );
        assert_eq!(session_type, "codex");
        assert!(seed.is_none(), "engine change should clear seed_from");
    }

    #[test]
    fn new_terminal_session_tab_cycles_field_0_and_1() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = None;
        let mut active = 0u8;
        for expected in [1u8, 0] {
            handle_new_terminal_session(
                NewTerminalSessionMut {
                    workspace_id: "ws-0",
                    session_type: &mut session_type,
                    task_id: &task_id,
                    seed_from: &mut seed,
                    active_field: &mut active,
                },
                ctx_no_repos(),
                &key(KeyCode::Tab),
            );
            assert_eq!(active, expected);
        }
    }

    #[test]
    fn new_terminal_session_enter_on_seed_field_with_bash_is_noop() {
        let mut session_type = "bash".to_string();
        let task_id = None;
        let mut seed: Option<String> = None;
        let mut active = 1u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        assert_consumed(&outcome);
    }

    #[test]
    fn new_terminal_session_enter_on_seed_field_with_claude_opens_picker() {
        let mut session_type = "claude".to_string();
        let task_id = Some("t-42".to_string());
        let mut seed: Option<String> = None;
        let mut active = 1u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-9",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(
                SubmitAction::OpenSnapshotPickerForNewTerminalSession {
                    workspace_id,
                    session_type,
                    task_id,
                    existing_seed_from,
                },
            ) => {
                assert_eq!(workspace_id, "ws-9");
                assert_eq!(session_type, "claude");
                assert_eq!(task_id.as_deref(), Some("t-42"));
                assert!(existing_seed_from.is_none());
            }
            other => panic!(
                "expected OpenSnapshotPickerForNewTerminalSession, got {other:?}"
            ),
        }
    }

    #[test]
    fn new_terminal_session_esc_on_seed_field_with_value_clears_seed_only() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = Some("reviewer".into());
        let mut active = 1u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-0",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_consumed(&outcome);
        assert!(seed.is_none());
    }

    #[test]
    fn new_terminal_session_submit_carries_seed_from() {
        let mut session_type = "claude".to_string();
        let task_id = None;
        let mut seed: Option<String> = Some("reviewer".into());
        let mut active = 0u8;
        let outcome = handle_new_terminal_session(
            NewTerminalSessionMut {
                workspace_id: "ws-1",
                session_type: &mut session_type,
                task_id: &task_id,
                seed_from: &mut seed,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SpawnSessionOnWorkspace {
                seed_from, ..
            }) => assert_eq!(seed_from.as_deref(), Some("reviewer")),
            other => panic!("expected SpawnSessionOnWorkspace, got {other:?}"),
        }
    }

    #[test]
    fn picker_cancel_preserves_existing_seed_from() {
        // Regression for "Picker cancel clears an existing seed_from".
        // The form had seed_from = Some("A"); the user opens the picker
        // to maybe change it, then Escs out. rebuild_form_from_picker
        // with name=None must restore the captured existing value.
        let target = PickerTarget::NewSession {
            label_text: "task".into(),
            branch_text: "br".into(),
            idle_timeout_text: "30".into(),
            repo_url: "u".into(),
            existing_seed_from: Some("snap-A".into()),
        };
        let mode = super::rebuild_form_from_picker(target, None);
        match mode {
            InputMode::NewSession { seed_from, .. } => {
                assert_eq!(seed_from.as_deref(), Some("snap-A"));
            }
            _ => panic!("expected NewSession"),
        }
    }

    #[test]
    fn picker_select_overwrites_existing_seed_from() {
        let target = PickerTarget::NewTerminalSession {
            workspace_id: "ws-3".into(),
            session_type: "claude".into(),
            task_id: None,
            existing_seed_from: Some("snap-A".into()),
        };
        let mode = super::rebuild_form_from_picker(
            target,
            Some("snap-B".into()),
        );
        match mode {
            InputMode::NewTerminalSession {
                seed_from,
                workspace_id,
                ..
            } => {
                assert_eq!(seed_from.as_deref(), Some("snap-B"));
                assert_eq!(workspace_id, "ws-3");
            }
            _ => panic!("expected NewTerminalSession"),
        }
    }

    #[test]
    fn picker_cancel_with_no_prior_seed_remains_none() {
        // No prior pick → cancel leaves seed_from None (i.e. the new
        // existing-seed_from preservation logic doesn't accidentally
        // inject something).
        let target = PickerTarget::NewSession {
            label_text: String::new(),
            branch_text: String::new(),
            idle_timeout_text: String::new(),
            repo_url: String::new(),
            existing_seed_from: None,
        };
        let mode = super::rebuild_form_from_picker(target, None);
        match mode {
            InputMode::NewSession { seed_from, .. } => {
                assert!(seed_from.is_none())
            }
            _ => panic!("expected NewSession"),
        }
    }

    #[test]
    fn cleanup_failed_clone_removes_transcript_file() {
        // Regression for "Seeded A-s leaves clone artifacts when spawn
        // fails". If a later step (build_args or spawn) errors after
        // clone_into_session has written the seed transcript, the file
        // must be removed so the next retry isn't blocked by
        // `AlreadyExists` from clone_into_session.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, b"seed\n").unwrap();
        let cloned = agent_memory::ClonedSession {
            transcript_id: "tid".into(),
            transcript_path: path.clone(),
            rollback: agent_memory::ClonedRollback::default(),
        };
        assert!(path.exists());
        App::cleanup_failed_clone(&cloned);
        assert!(!path.exists(), "transcript should have been removed");
    }

    #[test]
    fn cleanup_failed_clone_is_idempotent_on_missing_file() {
        // Called twice or against a never-written path: must not panic
        // or surface an error to the user. We're a best-effort cleanup.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-existed.jsonl");
        let cloned = agent_memory::ClonedSession {
            transcript_id: "tid".into(),
            transcript_path: path,
            rollback: agent_memory::ClonedRollback::default(),
        };
        App::cleanup_failed_clone(&cloned); // no panic
    }

    #[test]
    fn cleanup_failed_clone_restores_overwritten_memory_files() {
        // End-to-end rollback for the Claude memory-merge case. Build a
        // fake worktree with a user's MEMORY.md, clone a snapshot whose
        // memory dir contains a same-named file (the merge overwrites),
        // then call cleanup_failed_clone and assert the user's original
        // bytes are restored.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let worktree = fake_home.join("workspace");
        std::fs::create_dir_all(&worktree).unwrap();

        let projects_dir = fake_home.join(".claude/projects").join(
            worktree
                .to_string_lossy()
                .replace('/', "-")
                .replace('.', "-"),
        );
        let memory_dir = projects_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        let user_memory = memory_dir.join("MEMORY.md");
        std::fs::write(&user_memory, b"USER ORIGINAL CONTENT").unwrap();

        // Build a snapshot on disk with a competing MEMORY.md.
        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("reviewer");
        std::fs::create_dir_all(snap_dir.join("memory")).unwrap();
        std::fs::write(
            snap_dir.join("memory/MEMORY.md"),
            b"SNAPSHOT MEMORY",
        )
        .unwrap();
        std::fs::write(snap_dir.join("transcript.jsonl"), b"line\n").unwrap();
        let manifest = serde_json::json!({
            "version": agent_memory::MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-x",
            "source_transcript_id": "tid-1",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 5,
            "memory_files": 1,
        });
        std::fs::write(
            snap_dir.join("manifest.json"),
            manifest.to_string(),
        )
        .unwrap();

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };

        let snap = agent_memory::load("reviewer").unwrap();
        let cloned =
            agent_memory::clone_into_session(&snap, &worktree).unwrap();

        // Sanity: after clone the user's file was overwritten and the
        // transcript exists.
        assert_eq!(
            std::fs::read(&user_memory).unwrap(),
            b"SNAPSHOT MEMORY",
            "clone should have merged snapshot memory over user's file"
        );
        assert!(cloned.transcript_path.exists());

        // Simulate the post-clone failure path.
        App::cleanup_failed_clone(&cloned);

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(!cloned.transcript_path.exists(), "transcript removed");
        assert_eq!(
            std::fs::read(&user_memory).unwrap(),
            b"USER ORIGINAL CONTENT",
            "user's MEMORY.md must be byte-restored after cleanup",
        );
    }

    #[test]
    fn cleanup_failed_clone_removes_newly_created_memory_artifacts() {
        // When the worktree had no MEMORY.md before the clone, the file
        // is "newly created" and rollback should remove it (and the
        // memory dir if we created that too). A subsequent unseeded
        // session would then see no snapshot memory at all.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let worktree = fake_home.join("workspace");
        std::fs::create_dir_all(&worktree).unwrap();

        let projects_dir = fake_home.join(".claude/projects").join(
            worktree
                .to_string_lossy()
                .replace('/', "-")
                .replace('.', "-"),
        );
        // No memory dir/files yet — fresh worktree.

        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("primed");
        std::fs::create_dir_all(snap_dir.join("memory")).unwrap();
        std::fs::write(
            snap_dir.join("memory/MEMORY.md"),
            b"SNAPSHOT MEMORY",
        )
        .unwrap();
        std::fs::write(snap_dir.join("transcript.jsonl"), b"line\n").unwrap();
        let manifest = serde_json::json!({
            "version": agent_memory::MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-y",
            "source_transcript_id": "tid-2",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 5,
            "memory_files": 1,
        });
        std::fs::write(
            snap_dir.join("manifest.json"),
            manifest.to_string(),
        )
        .unwrap();

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };

        let snap = agent_memory::load("primed").unwrap();
        let cloned =
            agent_memory::clone_into_session(&snap, &worktree).unwrap();
        let dst_memory = projects_dir.join("memory/MEMORY.md");
        assert_eq!(std::fs::read(&dst_memory).unwrap(), b"SNAPSHOT MEMORY");

        App::cleanup_failed_clone(&cloned);

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(
            !dst_memory.exists(),
            "newly-created memory file should be removed"
        );
        assert!(
            !projects_dir.join("memory").exists(),
            "newly-created memory dir should be removed after cleanup"
        );
    }

    #[test]
    fn catalog_open_list_failure_with_picker_restores_form() {
        // Regression: list() failure inside open_snapshot_catalog used
        // to set a toast and return — silently dropping the captured
        // PickerTarget form state. The fix routes through
        // rebuild_form_from_picker, preserving every typed field plus
        // any existing seed_from.
        let target = PickerTarget::NewSession {
            label_text: "task".into(),
            branch_text: "feat/x".into(),
            idle_timeout_text: "30".into(),
            repo_url: "https://github.com/o/r".into(),
            existing_seed_from: Some("prior-snap".into()),
        };
        let err = agent_memory::SnapshotError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ));
        let (mode, status) =
            super::catalog_open_outcome(Err(err), Some(target));
        match mode {
            InputMode::NewSession {
                label_text,
                branch_text,
                idle_timeout_text,
                repo_url,
                seed_from,
                ..
            } => {
                assert_eq!(label_text, "task");
                assert_eq!(branch_text, "feat/x");
                assert_eq!(idle_timeout_text, "30");
                assert_eq!(repo_url, "https://github.com/o/r");
                assert_eq!(seed_from.as_deref(), Some("prior-snap"));
            }
            _ => panic!("expected restored NewSession form"),
        }
        let msg = status.expect("error should surface as status_msg");
        assert!(
            msg.contains("Could not list snapshots"),
            "unexpected status: {msg:?}"
        );
    }

    #[test]
    fn catalog_open_list_failure_without_picker_drops_to_normal() {
        let err = agent_memory::SnapshotError::NotFound;
        let (mode, status) = super::catalog_open_outcome(Err(err), None);
        assert!(matches!(mode, InputMode::Normal));
        assert!(status.is_some());
    }

    #[test]
    fn catalog_open_success_filters_by_picker_engine() {
        // Picker for a codex form must only show codex snapshots.
        let snaps = vec![
            agent_memory::Snapshot {
                name: "claude-one".into(),
                dir: std::path::PathBuf::from("/dev/null"),
                manifest: agent_memory::Manifest {
                    version: agent_memory::MANIFEST_VERSION,
                    description: String::new(),
                    engine: Engine::ClaudeCode,
                    source_session_uid: "u".into(),
                    source_transcript_id: "tid".into(),
                    source_cwd: std::path::PathBuf::from("/tmp"),
                    created_at_unix: 0,
                    transcript_bytes: 0,
                    memory_files: 0,
                },
            },
            agent_memory::Snapshot {
                name: "codex-one".into(),
                dir: std::path::PathBuf::from("/dev/null"),
                manifest: agent_memory::Manifest {
                    version: agent_memory::MANIFEST_VERSION,
                    description: String::new(),
                    engine: Engine::Codex,
                    source_session_uid: "u".into(),
                    source_transcript_id: "tid".into(),
                    source_cwd: std::path::PathBuf::from("/tmp"),
                    created_at_unix: 0,
                    transcript_bytes: 0,
                    memory_files: 0,
                },
            },
        ];
        let target = PickerTarget::NewTerminalSession {
            workspace_id: "ws-0".into(),
            session_type: "codex".into(),
            task_id: None,
            existing_seed_from: None,
        };
        let (mode, _) = super::catalog_open_outcome(Ok(snaps), Some(target));
        match mode {
            InputMode::SnapshotCatalog { snapshots, .. } => {
                assert_eq!(snapshots.len(), 1);
                assert_eq!(snapshots[0].name, "codex-one");
            }
            _ => panic!("expected SnapshotCatalog"),
        }
    }

    #[test]
    fn validate_seed_loadable_rejects_nonexistent_snapshot() {
        // Pointed at a fresh HOME with no snapshots; `load` returns
        // NotFound, the helper surfaces it as a user-facing error
        // string. `create_local_session` consults this BEFORE
        // `worktree::create_worktree`, so a bad seed name no longer
        // leaves an orphan worktree + branch on disk.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", dir.path()) };

        let result = super::validate_seed_loadable("ghost-snapshot");

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let msg = result.expect_err("missing snapshot should fail validation");
        assert!(
            msg.starts_with("Snapshot load failed: "),
            "unexpected error message: {msg:?}"
        );
    }

    #[test]
    fn validate_seed_loadable_accepts_real_snapshot() {
        // Counterpart: a real on-disk snapshot returns Ok so the
        // worktree creation proceeds.
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();
        let snap_root = fake_home.join(".cm/agent-memories");
        let snap_dir = snap_root.join("real");
        std::fs::create_dir_all(&snap_dir).unwrap();
        let manifest = serde_json::json!({
            "version": agent_memory::MANIFEST_VERSION,
            "description": "",
            "engine": "claude-code",
            "source_session_uid": "ts-x",
            "source_transcript_id": "tid-1",
            "source_cwd": "/tmp",
            "created_at_unix": 0,
            "transcript_bytes": 0,
            "memory_files": 0,
        });
        std::fs::write(snap_dir.join("manifest.json"), manifest.to_string())
            .unwrap();
        std::fs::write(snap_dir.join("transcript.jsonl"), b"line\n").unwrap();

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };

        let result = super::validate_seed_loadable("real");

        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert!(result.is_ok(), "real snapshot should pass validation: {result:?}");
    }

    #[test]
    fn picker_round_trip_targets_workspace_by_stable_id() {
        // Regression: PickerTarget::NewTerminalSession used to store a
        // positional ws_index. While the picker was open, backend events
        // (reconcile_tasks) could reorder workspaces and the form would
        // reopen pointing at the wrong slot. After the fix the target
        // carries workspace_id, so the form always reopens on the
        // workspace the user actually invoked the picker from.
        let target = PickerTarget::NewTerminalSession {
            workspace_id: "ws-B".into(),
            session_type: "claude".into(),
            task_id: Some("t-1".into()),
            existing_seed_from: None,
        };

        // Simulate: workspaces were [A, B, C] when picker opened; while
        // the picker was open a reconcile reordered them to [C, B, A].
        // The form's workspace_id must still be ws-B (not whatever sits
        // at index 1 now, which happens to be B in this case but would
        // diverge in other orderings).
        let workspaces = vec![
            ws_fixture("ws-C", &[]),
            ws_fixture("ws-B", &[]),
            ws_fixture("ws-A", &[]),
        ];
        let mode = super::rebuild_form_from_picker(target, Some("snap-X".into()));
        let workspace_id = match mode {
            InputMode::NewTerminalSession {
                workspace_id,
                seed_from,
                ..
            } => {
                assert_eq!(seed_from.as_deref(), Some("snap-X"));
                workspace_id
            }
            _ => panic!("expected NewTerminalSession"),
        };
        // The reopened form still references workspace B by id.
        assert_eq!(workspace_id, "ws-B");
        // And resolving against the reordered vec lands on B's current
        // position — index 1 now (because of the swap), but the lookup
        // would still work even if it had moved further.
        let resolved =
            super::resolve_workspace_by_id(&workspaces, &workspace_id);
        assert_eq!(resolved, Some(1));
        assert_eq!(workspaces[resolved.unwrap()].id, "ws-B");
    }

    #[test]
    fn resolve_workspace_by_id_returns_none_when_workspace_removed() {
        // The "workspace vanished between open and submit" path —
        // spawn_session_on_workspace bails on None rather than spawning
        // into whatever (unrelated) workspace happens to share the
        // old index.
        let workspaces = vec![
            ws_fixture("ws-A", &[]),
            ws_fixture("ws-C", &[]),
        ];
        assert!(
            super::resolve_workspace_by_id(&workspaces, "ws-B").is_none(),
            "lookup must return None for a removed workspace"
        );
    }

    #[test]
    fn picker_target_engine_resolves_per_target() {
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewSession {
                label_text: String::new(),
                branch_text: String::new(),
                idle_timeout_text: String::new(),
                repo_url: String::new(),
                existing_seed_from: None,
            }),
            Some(Engine::ClaudeCode)
        );
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewTerminalSession {
                workspace_id: "ws-0".into(),
                session_type: "claude".into(),
                task_id: None,
                existing_seed_from: None,
            }),
            Some(Engine::ClaudeCode)
        );
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewTerminalSession {
                workspace_id: "ws-0".into(),
                session_type: "codex".into(),
                task_id: None,
                existing_seed_from: None,
            }),
            Some(Engine::Codex)
        );
        assert_eq!(
            super::picker_target_engine(&PickerTarget::NewTerminalSession {
                workspace_id: "ws-0".into(),
                session_type: "bash".into(),
                task_id: None,
                existing_seed_from: None,
            }),
            None
        );
    }

    // ── SessionSettings ───────────────────────────────────────────

    fn session_settings_state(
        active: u8,
    ) -> (String, String, String, bool, bool, u8) {
        (
            "label".to_string(),
            "30".to_string(),
            "5".to_string(),
            false,
            false,
            active,
        )
    }

    #[test]
    fn session_settings_tab_cycles_active_field() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(0);
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 1,
                session_index: 2,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Tab),
        );
        assert_consumed(&outcome);
        assert_eq!(active, 1);
    }

    #[test]
    fn session_settings_backspace_pops_name_when_field_zero() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(0);
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 0,
                session_index: 0,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "labe");
    }

    #[test]
    fn session_settings_space_toggles_hidden_when_field_three() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(3);
        hidden = false;
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 0,
                session_index: 0,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Char(' ')),
        );
        assert_consumed(&outcome);
        assert!(hidden);
    }

    #[test]
    fn session_settings_enter_submits_save() {
        let (mut name, mut idle, mut burst, mut hidden, mut notify, mut active) =
            session_settings_state(0);
        let outcome = handle_session_settings(
            SessionSettingsMut {
                ws_index: 4,
                session_index: 9,
                name: &mut name,
                idle_timeout: &mut idle,
                burst_threshold: &mut burst,
                hidden: &mut hidden,
                notify_on_idle: &mut notify,
                active_field: &mut active,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveSessionSettings {
                ws_index,
                session_index,
                name,
                idle_timeout,
                burst_threshold,
                hidden,
                notify_on_idle,
            }) => {
                assert_eq!(ws_index, 4);
                assert_eq!(session_index, 9);
                assert_eq!(name, "label");
                assert_eq!(idle_timeout, 30);
                assert_eq!(burst_threshold, 5);
                assert!(!hidden);
                assert!(!notify_on_idle);
            }
            other => panic!("expected SaveSessionSettings, got {:?}", other),
        }
    }

    // ── WorkspaceSettings ─────────────────────────────────────────

    #[test]
    fn workspace_settings_char_appends() {
        let mut name = "foo".to_string();
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 0,
                name: &mut name,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('x')),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "foox");
    }

    #[test]
    fn workspace_settings_backspace_pops() {
        let mut name = "abc".to_string();
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 0,
                name: &mut name,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "ab");
    }

    #[test]
    fn workspace_settings_enter_submits_trimmed_name() {
        let mut name = "  hello  ".to_string();
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 3,
                name: &mut name,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveWorkspaceName { ws_index, name }) => {
                assert_eq!(ws_index, 3);
                assert_eq!(name, "hello");
            }
            other => panic!("expected SaveWorkspaceName, got {:?}", other),
        }
    }

    #[test]
    fn workspace_settings_esc_cancels() {
        let mut name = "n".to_string();
        let outcome = handle_workspace_settings(
            WorkspaceSettingsMut {
                ws_index: 0,
                name: &mut name,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // ── SaveSnapshot ──────────────────────────────────────────────

    fn save_snapshot_state<'a>(
        name: &'a mut String,
        desc: &'a mut String,
        active: &'a mut u8,
        error: &'a mut Option<String>,
    ) -> SaveSnapshotMut<'a> {
        SaveSnapshotMut {
            workspace_id: "ws-1",
            session_uid: "uid-1",
            name_text: name,
            description_text: desc,
            active_field: active,
            error,
        }
    }

    #[test]
    fn save_snapshot_char_routes_to_active_field() {
        let mut name = "fo".to_string();
        let mut desc = "ba".to_string();
        let mut active = 0u8;
        let mut error = None;

        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Char('o')),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "foo");
        assert_eq!(desc, "ba");

        active = 1;
        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Char('r')),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "foo");
        assert_eq!(desc, "bar");
    }

    #[test]
    fn save_snapshot_tab_cycles_field_and_clears_error() {
        let mut name = "x".to_string();
        let mut desc = String::new();
        let mut active = 0u8;
        let mut error = Some("prior".to_string());

        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Tab),
        );
        assert_consumed(&outcome);
        assert_eq!(active, 1);
        assert!(error.is_none(), "tab should clear stale error");
    }

    #[test]
    fn save_snapshot_typing_clears_error() {
        let mut name = String::new();
        let mut desc = String::new();
        let mut active = 0u8;
        let mut error = Some("prior".to_string());

        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Char('a')),
        );
        assert_consumed(&outcome);
        assert!(error.is_none(), "typing should clear stale error");
    }

    #[test]
    fn save_snapshot_enter_submits_trimmed() {
        let mut name = "  reviewer-strict  ".to_string();
        let mut desc = "  hello  ".to_string();
        let mut active = 0u8;
        let mut error = None;

        let outcome = handle_save_snapshot(
            SaveSnapshotMut {
                workspace_id: "ws-target",
                session_uid: "uid-target",
                name_text: &mut name,
                description_text: &mut desc,
                active_field: &mut active,
                error: &mut error,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveSnapshot {
                workspace_id,
                session_uid,
                name,
                description,
            }) => {
                assert_eq!(workspace_id, "ws-target");
                assert_eq!(session_uid, "uid-target");
                assert_eq!(name, "reviewer-strict");
                assert_eq!(description, "hello");
            }
            other => panic!("expected SaveSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn resolve_session_by_ids_follows_reordered_workspaces() {
        // Regression for stale-index bug: the modal stores (workspace_id,
        // session_uid). Even after the backend reorders self.workspaces,
        // submit-time resolution must still point at the originally-
        // targeted session — not whatever happens to be at the old index.

        // Initial order: [A, B]. Target = ws-A, uid-A2 → (0, 1).
        let mut workspaces = vec![
            ws_fixture("ws-A", &[("uid-A1", "claude"), ("uid-A2", "claude")]),
            ws_fixture("ws-B", &[("uid-B1", "codex")]),
        ];
        assert_eq!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A2"),
            Some((0, 1)),
        );

        // Backend swaps order in place: [B, A]. Same IDs → (1, 1).
        workspaces.swap(0, 1);
        assert_eq!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A2"),
            Some((1, 1)),
        );

        // Target session removed → None (caller toasts and bails).
        let wi_a = workspaces.iter().position(|w| w.id == "ws-A").unwrap();
        workspaces[wi_a].sessions.retain(|s| s.uid != "uid-A2");
        assert!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A2").is_none()
        );

        // Workspace removed → None.
        workspaces.retain(|w| w.id != "ws-A");
        assert!(
            super::resolve_session_by_ids(&workspaces, "ws-A", "uid-A1").is_none()
        );
    }

    /// Build a `Workspace` with the requested `(uid, session_type)` sessions.
    /// Bash sessions are used (no transcript, no PTY work) so the helper
    /// can synthesize them cheaply for resolver-only tests.
    fn ws_fixture(id: &str, sessions: &[(&str, &str)]) -> Workspace {
        use std::collections::HashMap;
        let mut out = Workspace {
            id: id.to_string(),
            name: id.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            sessions: Vec::new(),
            tombstones: Vec::new(),
            is_pushing: false,
        };
        for (uid, ty) in sessions {
            let session = crate::session::Session::new(
                "/bin/true",
                &[],
                80,
                24,
                None,
                HashMap::new(),
                None,
            )
            .expect("dummy session for fixture");
            out.sessions.push(TerminalSession {
                uid: (*uid).into(),
                label: (*uid).into(),
                session_type: (*ty).into(),
                session,
                status: SessionStatus::Idle,
                last_write_at: None,
                transcript_id: None,
                generation: 0,
                pending_jsonl_files: None,
                hidden: false,
                idle_timeout_secs: 0,
                burst_threshold: 0,
                pending_prompt: None,
                pending_clear: None,
                workflow_run_id: None,
                workflow_role: None,
                task_id: None,
                last_delivery: None,
                notify_on_idle: false,
                pending_enter: None,
                created_at: Instant::now(),
                managed_by_uid: None,
                seeded_from_snapshot: None,
            });
        }
        out
    }

    #[test]
    fn save_snapshot_esc_cancels() {
        let mut name = "x".to_string();
        let mut desc = String::new();
        let mut active = 0u8;
        let mut error = None;
        let outcome = handle_save_snapshot(
            save_snapshot_state(&mut name, &mut desc, &mut active, &mut error),
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // ── SnapshotCatalog ───────────────────────────────────────────

    fn fake_snapshot(name: &str) -> agent_memory::Snapshot {
        use agent_memory::{Manifest, Snapshot, MANIFEST_VERSION};
        Snapshot {
            name: name.to_string(),
            dir: std::path::PathBuf::from("/dev/null"),
            manifest: Manifest {
                version: MANIFEST_VERSION,
                description: String::new(),
                engine: Engine::ClaudeCode,
                source_session_uid: "uid".into(),
                source_transcript_id: "tid".into(),
                source_cwd: std::path::PathBuf::from("/tmp"),
                created_at_unix: 0,
                transcript_bytes: 0,
                memory_files: 0,
            },
        }
    }

    fn alt(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            code,
            KeyModifiers::ALT,
        ))
    }

    #[test]
    fn catalog_jk_wraps_around_list() {
        let mut snaps = vec![fake_snapshot("a"), fake_snapshot("b"), fake_snapshot("c")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;

        // j j j → wrap back to 0
        for _ in 0..3 {
            handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots: &mut snaps,
                    selected: &mut selected,
                    mode: &mut mode,
                    picker_target: None,
                    status_msg: &mut None,
                },
                ctx_no_repos(),
                &key(KeyCode::Char('j')),
            );
        }
        assert_eq!(selected, 0);

        // k → wraps to last
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('k')),
        );
        assert_eq!(selected, 2);
    }

    #[test]
    fn catalog_browse_enter_opens_detail() {
        let mut snaps = vec![fake_snapshot("a")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;
        let outcome = handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        assert_consumed(&outcome);
        assert!(matches!(mode, CatalogMode::Detail { .. }));
    }

    #[test]
    fn catalog_picker_enter_submits_snapshot_name() {
        let mut snaps = vec![fake_snapshot("alpha"), fake_snapshot("beta")];
        let mut selected = 1usize;
        let mut mode = CatalogMode::Browse;
        let outcome = handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: Some(&PickerTarget::NewSession {
                    label_text: String::new(),
                    branch_text: String::new(),
                    idle_timeout_text: String::new(),
                    repo_url: String::new(),
                    existing_seed_from: None,
                }),
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SnapshotPicked { name }) => {
                assert_eq!(name, "beta");
            }
            other => panic!("expected SnapshotPicked, got {other:?}"),
        }
    }

    #[test]
    fn catalog_browse_r_opens_rename_with_current_name() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('r')),
        );
        match mode {
            CatalogMode::Rename { text, error } => {
                assert_eq!(text, "alpha");
                assert!(error.is_none());
            }
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn catalog_browse_d_opens_confirm_delete() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Browse;
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('d')),
        );
        assert!(matches!(mode, CatalogMode::ConfirmDelete));
    }

    #[test]
    fn catalog_picker_disables_rename_and_delete() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;

        for ch in ['r', 'd'] {
            let mut mode = CatalogMode::Browse;
            handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots: &mut snaps,
                    selected: &mut selected,
                    mode: &mut mode,
                    picker_target: Some(&PickerTarget::NewSession {
                        label_text: String::new(),
                        branch_text: String::new(),
                        idle_timeout_text: String::new(),
                        repo_url: String::new(),
                        existing_seed_from: None,
                    }),
                    status_msg: &mut None,
                },
                ctx_no_repos(),
                &key(KeyCode::Char(ch)),
            );
            assert!(
                matches!(mode, CatalogMode::Browse),
                "picker mode should ignore `{ch}`, stayed in: {mode:?}"
            );
        }
    }

    #[test]
    fn catalog_alt_z_cancels_from_any_sub_mode() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;

        for start in [
            CatalogMode::Browse,
            CatalogMode::Detail {
                head: Vec::new(),
                tail: Vec::new(),
            },
            CatalogMode::Rename {
                text: "x".into(),
                error: None,
            },
            CatalogMode::ConfirmDelete,
        ] {
            let mut mode = start;
            let outcome = handle_snapshot_catalog(
                SnapshotCatalogMut {
                    snapshots: &mut snaps,
                    selected: &mut selected,
                    mode: &mut mode,
                    picker_target: None,
                    status_msg: &mut None,
                },
                ctx_no_repos(),
                &alt(KeyCode::Char('z')),
            );
            assert_cancel(&outcome);
        }
    }

    #[test]
    fn catalog_detail_esc_returns_to_browse() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Detail {
            head: Vec::new(),
            tail: Vec::new(),
        };
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert!(matches!(mode, CatalogMode::Browse));
    }

    #[test]
    fn catalog_rename_typing_and_backspace() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Rename {
            text: "alp".into(),
            error: Some("prior".into()),
        };
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('x')),
        );
        match &mode {
            CatalogMode::Rename { text, error } => {
                assert_eq!(text, "alpx");
                assert!(error.is_none(), "typing should clear prior error");
            }
            other => panic!("expected Rename, got {other:?}"),
        }

        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        match &mode {
            CatalogMode::Rename { text, .. } => assert_eq!(text, "alp"),
            other => panic!("expected Rename, got {other:?}"),
        }
    }

    #[test]
    fn catalog_rename_esc_returns_to_browse() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::Rename {
            text: "alpha-v2".into(),
            error: None,
        };
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert!(matches!(mode, CatalogMode::Browse));
    }

    #[test]
    fn catalog_confirm_delete_n_returns_to_browse() {
        let mut snaps = vec![fake_snapshot("alpha")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::ConfirmDelete;
        handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut None,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('n')),
        );
        assert!(matches!(mode, CatalogMode::Browse));
        // List untouched on cancel.
        assert_eq!(snaps.len(), 1);
    }

    #[test]
    fn format_relative_time_buckets() {
        // Pick `now` well above one year so the 1y+ case can subtract
        // cleanly without overflow in the literal computation.
        let one_year_secs: u64 = 60 * 60 * 24 * 365;
        let now: u64 = one_year_secs + 10;
        assert_eq!(super::format_relative_time(now, now), "just now");
        assert_eq!(super::format_relative_time(now - 59, now), "just now");
        assert_eq!(super::format_relative_time(now - 60, now), "1m ago");
        assert_eq!(super::format_relative_time(now - 60 * 59, now), "59m ago");
        assert_eq!(super::format_relative_time(now - 60 * 60, now), "1h ago");
        assert_eq!(super::format_relative_time(now - 60 * 60 * 24, now), "1d ago");
        assert_eq!(
            super::format_relative_time(now - one_year_secs, now),
            "1y+ ago"
        );
        // Saturating subtraction so future timestamps don't panic.
        assert_eq!(super::format_relative_time(now + 5, now), "just now");
    }

    #[test]
    fn truncate_keeps_short_strings_intact() {
        assert_eq!(super::truncate("hello", 10), "hello");
        assert_eq!(super::truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_inserts_ellipsis_when_too_long() {
        let out = super::truncate("hello world", 6);
        assert_eq!(out.chars().count(), 6);
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn sanitize_for_display_strips_ansi_and_control_bytes() {
        // ESC + CSI red, then "red", then ESC + CSI reset. All ESCs and
        // control bytes must be stripped — otherwise opening the catalog
        // replays the sequence into the user's terminal.
        let out = super::sanitize_for_display("\x1b[31mred\x1b[0m");
        assert!(!out.contains('\x1b'), "ESC must be stripped: {out:?}");
        assert_eq!(out, "[31mred[0m");

        // C0 controls (other than tab) and DEL are stripped.
        let out = super::sanitize_for_display(
            "a\x00b\x07c\x08d\x0ae\x0df\x7fg",
        );
        assert_eq!(out, "abcdefg");

        // Tab is preserved (it's the one C0 control we let through).
        let out = super::sanitize_for_display("a\tb");
        assert_eq!(out, "a\tb");

        // OSC (ESC ] ... BEL) — ESC and BEL both go, content between
        // remains as inert text. Acceptable: the dangerous part is the
        // ESC that begins the sequence; without it the terminal sees
        // only literal characters.
        let out = super::sanitize_for_display("\x1b]0;title\x07rest");
        assert!(!out.contains('\x1b'));
        assert!(!out.contains('\x07'));
        assert_eq!(out, "]0;titlerest");
    }

    #[test]
    fn read_transcript_head_tail_streams_large_file() {
        // 1000-line transcript. Head should be the first N, tail should
        // be the last N. Function must work without slurping the whole
        // file (covered by code review; the assertion here verifies
        // correctness of the windowing).
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().to_path_buf();
        let path = snap_dir.join("transcript.jsonl");
        let mut buf = String::new();
        for i in 0..1000 {
            buf.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&path, buf).unwrap();

        let (head, tail) = super::read_transcript_head_tail(&snap_dir, 5);
        assert_eq!(head.len(), 5);
        assert_eq!(head[0], "line 0");
        assert_eq!(head[4], "line 4");
        assert_eq!(tail.len(), 5);
        assert_eq!(tail[0], "line 995");
        assert_eq!(tail[4], "line 999");
    }

    #[test]
    fn read_transcript_head_tail_no_overlap_for_small_files() {
        // 8 lines, n=5 → head = 0..4, tail = 5..7. (Tail must not
        // include lines already in head.)
        let dir = tempfile::tempdir().unwrap();
        let snap_dir = dir.path().to_path_buf();
        let path = snap_dir.join("transcript.jsonl");
        let buf: String = (0..8).map(|i| format!("L{i}\n")).collect();
        std::fs::write(&path, buf).unwrap();

        let (head, tail) = super::read_transcript_head_tail(&snap_dir, 5);
        assert_eq!(head, vec!["L0", "L1", "L2", "L3", "L4"]);
        assert_eq!(tail, vec!["L5", "L6", "L7"]);
    }

    #[test]
    fn read_transcript_head_tail_empty_when_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (head, tail) =
            super::read_transcript_head_tail(dir.path(), 5);
        assert!(head.is_empty());
        assert!(tail.is_empty());
    }

    #[test]
    fn snapshot_rename_error_is_sanitized_on_render() {
        // Render the rename overlay with an ANSI-laced validation error
        // into a TestBackend and inspect the buffer. No ESC byte should
        // appear anywhere on screen — otherwise a paste-injected name
        // that fails `validate_name` would echo its bytes back through
        // the terminal when the error renders.
        //
        // Render methods on `App` for this overlay don't dereference
        // `self`, so we can avoid building a full App by using ratatui's
        // pure widget API directly to construct the same lines the
        // method does and asserting on that. (Tested via the line-build
        // helper below — extracted from the render method so the test
        // can drive it without a Terminal.)
        let lines = rename_overlay_lines("newname", Some("invalid character: '\x1b[31m'"));
        // Flatten all spans into a single text dump.
        let dump: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            !dump.contains('\x1b'),
            "ESC byte present in rendered text: {dump:?}"
        );
        // Sanitized text is still visible.
        assert!(
            dump.contains("invalid character"),
            "expected the (sanitized) error text on screen, got: {dump:?}"
        );
    }

    #[test]
    fn visible_range_centers_selection_when_possible() {
        // selected=40, total=50, visible=10 → window around 40 with 5
        // items above the selection (half = 10/2 = 5) and 4 after.
        let (start, end) = super::visible_range(40, 50, 10);
        assert_eq!(end - start, 10, "window must be exactly visible-sized");
        assert!(
            start <= 40 && 40 < end,
            "selected 40 must be within window [{start}, {end})"
        );
        assert_eq!(start, 35);
        assert_eq!(end, 45);
    }

    #[test]
    fn visible_range_clamps_at_top() {
        let (start, end) = super::visible_range(0, 50, 10);
        assert_eq!((start, end), (0, 10));
    }

    #[test]
    fn visible_range_clamps_at_bottom() {
        let (start, end) = super::visible_range(49, 50, 10);
        assert_eq!((start, end), (40, 50));
    }

    #[test]
    fn visible_range_fits_whole_list_when_room_allows() {
        let (start, end) = super::visible_range(2, 5, 10);
        assert_eq!((start, end), (0, 5));
    }

    #[test]
    fn visible_range_handles_empty_or_zero_height() {
        assert_eq!(super::visible_range(0, 0, 10), (0, 0));
        assert_eq!(super::visible_range(3, 10, 0), (0, 0));
    }

    #[test]
    fn catalog_delete_failure_preserves_list_and_surfaces_error() {
        // Construct an in-memory snapshot list whose entries don't exist
        // on disk under HOME. `agent_memory::delete` returns NotFound;
        // the handler must keep the list intact and set status_msg
        // instead of silently blanking the list via list().unwrap_or_default().
        use crate::test_support::home_lock;
        let dir = tempfile::tempdir().unwrap();
        let fake_home = dir.path();

        let mut snaps = vec![fake_snapshot("ghost"), fake_snapshot("other")];
        let mut selected = 0usize;
        let mut mode = CatalogMode::ConfirmDelete;
        let mut status: Option<String> = None;

        let _guard = home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", fake_home) };
        let outcome = handle_snapshot_catalog(
            SnapshotCatalogMut {
                snapshots: &mut snaps,
                selected: &mut selected,
                mode: &mut mode,
                picker_target: None,
                status_msg: &mut status,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('y')),
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        assert_consumed(&outcome);
        assert_eq!(snaps.len(), 2, "list must be preserved on delete failure");
        assert_eq!(snaps[0].name, "ghost");
        assert!(matches!(mode, CatalogMode::Browse));
        let msg = status.expect("status_msg should be set on delete failure");
        assert!(
            msg.starts_with("Delete failed: "),
            "expected delete-failure status, got: {msg:?}"
        );
    }

    // ── TaskSettings ──────────────────────────────────────────────

    #[test]
    fn task_settings_backspace_pops_name() {
        let task_id = "task-id-1".to_string();
        let mut name = "abc".to_string();
        let outcome = handle_task_settings(
            TaskSettingsMut {
                task_id: task_id.as_str(),
                name: &mut name,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(name, "ab");
    }

    #[test]
    fn task_settings_enter_submits_save_task_name() {
        let task_id = "task-id-1".to_string();
        let mut name = " new name ".to_string();
        let outcome = handle_task_settings(
            TaskSettingsMut {
                task_id: task_id.as_str(),
                name: &mut name,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::SaveTaskName { task_id, name }) => {
                assert_eq!(task_id, "task-id-1");
                assert_eq!(name, "new name");
            }
            other => panic!("expected SaveTaskName, got {:?}", other),
        }
    }

    // ── WorkflowLaunchConfirm ─────────────────────────────────────

    fn make_slot() -> WorkflowSlotChoice {
        WorkflowSlotChoice {
            role: "worker".to_string(),
            options: vec![
                WorkflowSlotSource::New(Engine::ClaudeCode),
                WorkflowSlotSource::New(Engine::Codex),
            ],
            option_index: 0,
        }
    }

    #[test]
    fn workflow_launch_down_advances_active_slot() {
        let mut slots = vec![make_slot()];
        let mut active = 0usize;
        let mut goal = String::new();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_index: 1,
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
            },
            ctx_no_repos(),
            &key(KeyCode::Down),
        );
        assert_consumed(&outcome);
        // positions = slots.len() + 1 = 2; from 0 → 1 (the goal field).
        assert_eq!(active, 1);
    }

    #[test]
    fn workflow_launch_backspace_pops_goal_when_focused() {
        let mut slots = vec![make_slot()];
        let mut active = slots.len(); // goal-focused
        let mut goal = "ab".to_string();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_index: 0,
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
            },
            ctx_no_repos(),
            &key(KeyCode::Backspace),
        );
        assert_consumed(&outcome);
        assert_eq!(goal, "a");
    }

    #[test]
    fn workflow_launch_enter_submits_with_optional_goal() {
        let mut slots = vec![make_slot()];
        let mut active = slots.len();
        let mut goal = "  refactor the parser  ".to_string();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_index: 5,
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::LaunchWorkflow {
                ws_index,
                workflow_name,
                slots: launched_slots,
                goal,
            }) => {
                assert_eq!(ws_index, 5);
                assert_eq!(workflow_name, "feedback");
                assert_eq!(launched_slots.len(), 1);
                assert_eq!(goal.as_deref(), Some("refactor the parser"));
            }
            other => panic!("expected LaunchWorkflow, got {:?}", other),
        }
    }

    #[test]
    fn workflow_launch_esc_cancels() {
        let mut slots = vec![make_slot()];
        let mut active = 0usize;
        let mut goal = String::new();
        let outcome = handle_workflow_launch_confirm(
            WorkflowLaunchConfirmMut {
                ws_index: 0,
                workflow_name: "feedback",
                slots: &mut slots,
                active_slot: &mut active,
                goal: &mut goal,
            },
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    // ── WorkflowPicker ────────────────────────────────────────────

    #[test]
    fn workflow_picker_j_advances_selection_with_wraparound() {
        let names = vec!["a".to_string(), "b".to_string()];
        let mut selected = 1usize;
        let outcome = handle_workflow_picker(
            WorkflowPickerMut {
                ws_index: 0,
                focused_si: None,
                names: &names,
                selected: &mut selected,
            },
            ctx_no_repos(),
            &key(KeyCode::Char('j')),
        );
        assert_consumed(&outcome);
        assert_eq!(selected, 0);
    }

    #[test]
    fn workflow_picker_enter_submits_selected_name() {
        let names = vec!["alpha".to_string(), "beta".to_string()];
        let mut selected = 1usize;
        let outcome = handle_workflow_picker(
            WorkflowPickerMut {
                ws_index: 7,
                focused_si: Some(2),
                names: &names,
                selected: &mut selected,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::EnterWorkflowLaunchConfirm {
                ws_index,
                focused_si,
                workflow_name,
            }) => {
                assert_eq!(ws_index, 7);
                assert_eq!(focused_si, Some(2));
                assert_eq!(workflow_name, "beta");
            }
            other => panic!("expected EnterWorkflowLaunchConfirm, got {:?}", other),
        }
    }

    #[test]
    fn workflow_picker_enter_with_empty_names_submits_none() {
        let names: Vec<String> = vec![];
        let mut selected = 0usize;
        let outcome = handle_workflow_picker(
            WorkflowPickerMut {
                ws_index: 0,
                focused_si: None,
                names: &names,
                selected: &mut selected,
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        // Pre-extraction behavior: out-of-bounds selection still closes
        // the modal, just without firing the side effect.
        assert!(matches!(
            outcome,
            InputOutcome::Submit(SubmitAction::None)
        ));
    }

    // ── WorkflowHistory ───────────────────────────────────────────

    #[test]
    fn workflow_history_q_cancels() {
        let outcome = handle_workflow_history(ctx_no_repos(), &key(KeyCode::Char('q')));
        assert_cancel(&outcome);
    }

    #[test]
    fn workflow_history_other_key_consumes() {
        let outcome = handle_workflow_history(ctx_no_repos(), &key(KeyCode::Char('x')));
        assert_consumed(&outcome);
    }

    // ── Confirm ───────────────────────────────────────────────────

    #[test]
    fn confirm_y_submits_mark_done() {
        let outcome = handle_confirm(
            &ConfirmAction::MarkDone,
            ctx_no_repos(),
            &key(KeyCode::Char('y')),
        );
        assert!(matches!(
            outcome,
            InputOutcome::Submit(SubmitAction::MarkActiveDone)
        ));
    }

    #[test]
    fn confirm_capital_y_submits_delete() {
        let outcome = handle_confirm(
            &ConfirmAction::Delete,
            ctx_no_repos(),
            &key(KeyCode::Char('Y')),
        );
        assert!(matches!(
            outcome,
            InputOutcome::Submit(SubmitAction::DeleteActive)
        ));
    }

    #[test]
    fn confirm_n_cancels() {
        let outcome = handle_confirm(
            &ConfirmAction::MarkDone,
            ctx_no_repos(),
            &key(KeyCode::Char('n')),
        );
        assert_cancel(&outcome);
    }

    #[test]
    fn confirm_esc_cancels() {
        let outcome = handle_confirm(
            &ConfirmAction::MarkDone,
            ctx_no_repos(),
            &key(KeyCode::Esc),
        );
        assert_cancel(&outcome);
    }

    #[test]
    fn confirm_enter_routes_stop_workflow_run_id() {
        let outcome = handle_confirm(
            &ConfirmAction::StopWorkflow {
                run_id: "run-abc".to_string(),
            },
            ctx_no_repos(),
            &key(KeyCode::Enter),
        );
        match outcome {
            InputOutcome::Submit(SubmitAction::StopWorkflow { run_id }) => {
                assert_eq!(run_id, "run-abc");
            }
            other => panic!("expected StopWorkflow, got {:?}", other),
        }
    }
}

#[cfg(test)]
mod workflow_load_errors_tests {
    //! Pins down that workflow TOML failures captured by `load_all` flow into
    //! the picker surface. Without this, a typo in `workflows/feedback.toml`
    //! silently makes that workflow disappear from `A-f` with no hint.

    use super::{
        filter_real_workflow_load_errors, format_workflow_load_error, route_workflow_launch,
        workflow_picker_title, WorkflowLaunchRouting,
    };
    use crate::workflow;

    const VALID_TOML: &str = r#"
name = "good"
description = "Loads cleanly"
[roles.solo]
engine = "claude-code"
context = "persistent"
"#;

    /// `load_all` walks a workflows dir and partitions entries into
    /// (parsed, errors). A garbage TOML file lands in the errors bucket
    /// while siblings keep parsing — that's the contract App::new relies
    /// on to populate `workflow_load_errors` without losing valid loads.
    #[test]
    fn load_all_returns_errors_for_invalid_toml_alongside_valid_ones() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("good.toml"), VALID_TOML).unwrap();
        std::fs::write(tmp.path().join("bad.toml"), "not = = valid toml").unwrap();

        let (workflows, errors) = workflow::toml_schema::load_all(tmp.path());

        assert_eq!(workflows.len(), 1, "valid workflow should still parse");
        assert!(workflows.contains_key("good"));
        assert_eq!(errors.len(), 1, "invalid workflow should be reported");
        assert!(
            errors[0].0.file_name().and_then(|s| s.to_str()) == Some("bad.toml"),
            "error tuple should carry the offending file path",
        );
    }

    /// End-to-end: feed real `load_all` output through the same conversion
    /// `App::new` does, then check both surfaces (picker title + per-row
    /// summary) include identifying info. This is the surface promise users
    /// see when they open the picker after a load failure.
    #[test]
    fn captured_errors_render_in_picker_title_and_dim_rows() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("typo.toml"), "name = \n").unwrap();
        let (_workflows, errors) = workflow::toml_schema::load_all(tmp.path());
        assert_eq!(errors.len(), 1);

        // Mirror what App::new stores.
        let app_errors: Vec<(std::path::PathBuf, String)> = errors
            .into_iter()
            .map(|(p, e)| (p, e.to_string()))
            .collect();

        let title = workflow_picker_title(app_errors.len());
        assert_eq!(title, " Pick workflow (1 failed to load) ");

        let row = format_workflow_load_error(&app_errors[0].0, &app_errors[0].1);
        assert!(row.contains("typo.toml"), "row should name the file: {}", row);
        assert!(row.starts_with('⚠'), "row should be visually flagged: {}", row);
    }

    #[test]
    fn workflow_picker_title_omits_count_when_no_errors() {
        assert_eq!(workflow_picker_title(0), " Pick workflow ");
    }

    /// One valid + zero errors: the picker is short-circuited and we go
    /// straight to launch-confirm. This is the existing fast path.
    #[test]
    fn route_one_valid_no_errors_short_circuits_to_launch() {
        let r = route_workflow_launch(vec!["feedback".into()], false);
        assert_eq!(r, WorkflowLaunchRouting::LaunchOnly("feedback".into()));
    }

    /// One valid + one bad TOML: the picker MUST open so the dim error row
    /// is visible. Without this, the user would jump straight to confirm
    /// for the lone valid workflow with no hint that another file failed.
    /// This is the bug the reviewer flagged.
    #[test]
    fn route_one_valid_with_load_errors_forces_picker_open() {
        let r = route_workflow_launch(vec!["feedback".into()], true);
        match r {
            WorkflowLaunchRouting::OpenPicker(names) => {
                assert_eq!(names, vec!["feedback".to_string()]);
            }
            other => panic!("expected OpenPicker, got {:?}", other),
        }
    }

    /// Zero valid + at least one bad TOML: don't show the misleading
    /// "No workflows found in <dir>" status — open the picker so the load
    /// errors are surfaced as the dim rows on top.
    #[test]
    fn route_zero_valid_with_load_errors_opens_empty_picker() {
        let r = route_workflow_launch(Vec::new(), true);
        match r {
            WorkflowLaunchRouting::OpenPicker(names) => {
                assert!(names.is_empty());
            }
            other => panic!("expected OpenPicker, got {:?}", other),
        }
    }

    /// Zero valid + zero errors: the genuine empty-dir case keeps its
    /// original status_msg. We don't want to open an empty picker on a
    /// fresh checkout where no `workflows/` dir exists yet.
    #[test]
    fn route_zero_valid_no_errors_reports_not_found() {
        let r = route_workflow_launch(Vec::new(), false);
        assert_eq!(r, WorkflowLaunchRouting::NoWorkflowsFound);
    }

    /// Missing `workflows/` dir: `load_all` returns `(empty, [(dir, Io(NotFound))])`.
    /// That is NOT a per-file load failure — it's an absent surface — so the
    /// filter must drop it. Otherwise a fresh install gets the misleading
    /// "(1 failed to load)" picker title every time.
    #[test]
    fn filter_drops_directory_level_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let (workflows, errs) = workflow::toml_schema::load_all(&missing);

        assert!(workflows.is_empty());
        assert_eq!(errs.len(), 1, "load_all reports the absent dir");
        let filtered = filter_real_workflow_load_errors(&missing, errs);
        assert!(
            filtered.is_empty(),
            "directory-level NotFound must be filtered: {:?}",
            filtered
        );

        // And the routing then falls through to NoWorkflowsFound, matching
        // the pre-existing "No workflows found in <dir>" UX.
        let r = route_workflow_launch(Vec::new(), !filtered.is_empty());
        assert_eq!(r, WorkflowLaunchRouting::NoWorkflowsFound);
    }

    /// Real per-file failures (parse errors, validation errors) MUST still
    /// pass through the filter — that's the whole point of the surface.
    #[test]
    fn filter_keeps_per_file_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bad.toml"), "not = = valid").unwrap();
        let (_workflows, errs) = workflow::toml_schema::load_all(tmp.path());
        assert_eq!(errs.len(), 1);

        let filtered = filter_real_workflow_load_errors(tmp.path(), errs);
        assert_eq!(filtered.len(), 1, "parse error must survive the filter");
        assert_eq!(
            filtered[0].0.file_name().and_then(|s| s.to_str()),
            Some("bad.toml"),
        );
    }
}
