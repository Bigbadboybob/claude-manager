use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::term::TermMode;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::api::Task;
use crate::backend::{BackendEvent, BackendHandle};
use crate::config::Config;
use crate::input;
use crate::planning::{PlanAction, PlanningView, WorkspaceCandidate};
use crate::session::Session;
use crate::terminal_widget::TerminalWidget;
use crate::workflow::run::MessageBaseline;
use crate::workflow::{self, toml_schema::Engine, RoleBinding, TriggerKind, Workflow, WorkflowRun};
use crate::worktree;

mod dirs {
    use std::path::PathBuf;
    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL_MS: u128 = 80;
/// Duration of the rolling window for Wakeup burst detection.
const WAKEUP_WINDOW: Duration = Duration::from_secs(2);
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
    pub label: String,
    pub session_type: String, // "claude" or "bash" — immutable, survives renames
    pub session: Session,
    pub status: SessionStatus,
    pub last_write_at: Option<Instant>,
    pub session_id: Option<String>,
    pub pending_jsonl_files: Option<Vec<String>>,
    pub hidden: bool,
    /// Seconds of quiet before marking idle. 0 = use global default.
    pub idle_timeout_secs: u16,
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
    /// First ~120 chars of the most recent prompt we delivered via
    /// `deliver_pending_write`, along with its delivery timestamp in unix ms.
    /// Used to correlate a fresh claude workflow session with its new
    /// sessionId in `~/.claude/history.jsonl`: when the same text shows up
    /// in a history entry with project==worktree, the entry's sessionId is
    /// ours. Cleared once sid has been bound.
    pub last_delivery: Option<(String, u64)>,
}

const DEFAULT_IDLE_TIMEOUT_SECS: u16 = 2;

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

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct ManifestEntry {
    label: String,
    session_type: String,
    session_id: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    idle_timeout_secs: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workflow_role: Option<String>,
}

/// Persisted workspace metadata. Lives in `Manifest::workspaces` keyed by the
/// workspace's stable id (or, for legacy manifests, by worktree path / task:id
/// before being re-keyed on load).
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
    // Legacy v2 fields — read for migration into the v3 `bindings` map, never
    // written back.
    #[serde(default, skip_serializing)]
    extra_bound_tasks: Vec<BoundTaskLegacy>,
}

#[derive(serde::Deserialize, Clone, Debug, Default)]
struct BoundTaskLegacy {
    #[serde(default)]
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    title: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct Manifest {
    /// v1 legacy shape — flat session list keyed by worktree path / task:id.
    /// Read for migration; never written.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    sessions: HashMap<String, Vec<ManifestEntry>>,
    /// Workspaces keyed by stable workspace id (v3) or legacy key (v1/v2,
    /// rekeyed on load).
    #[serde(default)]
    workspaces: HashMap<String, ManifestWorkspace>,
    /// `task_id` → `workspace_id` bindings. A task present here is bound to
    /// the referenced workspace (primary or extra — no distinction).
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
}

/// A task tracked in the planning/API layer. Pure metadata — no execution
/// state. `workspace_id` points at the Workspace this task has been launched
/// into (None when still in backlog / never launched).
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
}

/// Build a TerminalSession wrapping a freshly-spawned PTY with default state.
/// Used by attach/spawn flows that don't need pending prompts or workflow tags.
fn make_simple_session(
    label: &str,
    session_type: &str,
    session: Session,
    pending_jsonl_files: Option<Vec<String>>,
) -> TerminalSession {
    TerminalSession {
        label: label.to_string(),
        session_type: session_type.to_string(),
        session,
        status: SessionStatus::Running,
        last_write_at: None,
        session_id: None,
        pending_jsonl_files,
        hidden: false,
        idle_timeout_secs: 0,
        pending_prompt: None,
        pending_clear: None,
        workflow_run_id: None,
        workflow_role: None,
        last_delivery: None,
    }
}

/// Generate a fresh workspace id. Not cryptographic — just collision-avoidance
/// across the user's manifest via nanosecond timestamp.
fn new_workspace_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ws-{:x}", nanos)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cursor {
    /// Cursor is on a workspace header (by workspace index).
    Workspace(usize),
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
        /// 0 = name, 1 = branch, 2 = idle timeout
        active_field: u8,
    },
    /// Picking a session type to add to a workspace.
    NewTerminalSession {
        ws_index: usize,
        session_type: String,
    },
    /// Editing session settings.
    SessionSettings {
        ws_index: usize,
        session_index: usize,
        name: String,
        idle_timeout: String,
        hidden: bool,
        /// 0 = name, 1 = idle timeout
        active_field: u8,
    },
    /// Renaming a workspace. Only the display label changes — the branch
    /// and worktree path stay the same.
    WorkspaceSettings {
        ws_index: usize,
        name: String,
    },
    /// Confirming launch of a workflow on a workspace.
    WorkflowLaunchConfirm {
        ws_index: usize,
        workflow_name: String,
        /// One slot per role, in presentation order.
        slots: Vec<WorkflowSlotChoice>,
        /// Index of the slot whose option can currently be cycled.
        active_slot: usize,
    },
    /// Showing a workflow run's history.
    WorkflowHistory {
        run_id: String,
    },
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
    last_session_id_check: Instant,
    /// Workflow definitions loaded from `workflows/*.toml` at startup.
    pub workflows: HashMap<String, Workflow>,
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
}

impl App {
    pub fn new(config: Config) -> Self {
        let backend = BackendHandle::spawn(&config);
        let manifest = Self::load_manifest();
        let sidebar_view = match manifest.view.as_deref() {
            Some("task") => SidebarView::Task,
            _ => SidebarView::Status,
        };
        let (workflows, _errs) =
            workflow::toml_schema::load_all(&workflow::toml_schema::workflows_dir());
        let workflow_runs = workflow::run::load_all()
            .into_iter()
            .filter(|r| r.is_active())
            .collect();
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
            last_session_id_check: Instant::now(),
            workflows,
            workflow_runs,
            history_watcher: workflow::history::HistoryWatcher::new(),
            pending_rotations: Vec::new(),
        }
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
    fn list_jsonl_files(worktree_path: &Path) -> Vec<String> {
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
    fn detect_session_id(worktree_path: &Path, existing_files: &[String]) -> Option<String> {
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
    fn list_codex_sessions(worktree_path: &Path) -> Vec<String> {
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

    fn walk_codex_sessions(dir: &Path, wt_str: &str, ids: &mut Vec<String>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk_codex_sessions(&path, wt_str, ids);
            } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                // Read first line to check cwd.
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Some(first_line) = content.lines().next() {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(first_line) {
                            if let Some(cwd) = val.pointer("/payload/cwd").and_then(|v| v.as_str()) {
                                if cwd == wt_str {
                                    if let Some(id) = val.pointer("/payload/id").and_then(|v| v.as_str()) {
                                        ids.push(id.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Detect a new codex session_id by comparing against known IDs. Uses the
    /// user's default codex home.
    fn detect_codex_session_id(worktree_path: &Path, existing_ids: &[String]) -> Option<String> {
        let current = Self::list_codex_sessions(worktree_path);
        current.into_iter().find(|id| !existing_ids.contains(id))
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
    /// IMPORTANT: we sleep briefly between the body write and the enter write
    /// so the receiving agent sees them as two separate keystroke events
    /// rather than a single paste. Without this, codex treats the whole
    /// sequence (body + \r) as pasted content — literal text including the
    /// \r character — and never submits.
    fn deliver_pending_write(ts: &mut TerminalSession, pw: &PendingWrite, kind: &str) {
        let body = pw.text.trim_end_matches(['\r', '\n']);
        let enter = enter_bytes_for(&ts.session);
        let kitty = enter != b"\r";
        let exited = ts.session.exited;
        ts.session.write(body.as_bytes());
        if pw.submit {
            // Gap between body and Enter so codex classifies Enter as a
            // keystroke, not the tail of a paste. 50ms was enough for small
            // prompts but fails for multi-KB ones where codex is still
            // absorbing the body when Enter lands. 2s is well above the
            // observed threshold and trivial against the workflow cycle.
            std::thread::sleep(Duration::from_millis(2000));
            ts.session.write(enter);
        }
        // Remember the first chunk of the delivered text + delivery time so
        // an unbound workflow session can be correlated to its new sid in
        // ~/.claude/history.jsonl. Only record for workflow sessions that
        // still need binding.
        if ts.workflow_run_id.is_some() && ts.session_id.is_none() {
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
                    "delivered {}: {} body bytes + submit={} to session '{}' role='{}' exited={} kitty_enter={}",
                    kind,
                    body.len(),
                    pw.submit,
                    ts.label,
                    ts.workflow_role.as_deref().unwrap_or("?"),
                    exited,
                    kitty,
                ),
            );
        }
    }

    /// Path to the session manifest file.
    fn manifest_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".cm/tui-sessions.json")
    }

    /// Save session manifest to disk.
    fn save_session_manifest(&self) {
        let mut workspaces: HashMap<String, ManifestWorkspace> = HashMap::new();
        for ws in &self.workspaces {
            let entries: Vec<ManifestEntry> = ws
                .sessions
                .iter()
                .map(|ts| ManifestEntry {
                    label: ts.label.clone(),
                    session_type: ts.session_type.clone(),
                    session_id: ts.session_id.clone(),
                    hidden: ts.hidden,
                    idle_timeout_secs: ts.idle_timeout_secs,
                    workflow_run_id: ts.workflow_run_id.clone(),
                    workflow_role: ts.workflow_role.clone(),
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
                    extra_bound_tasks: Vec::new(),
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
            sessions: HashMap::new(),
            workspaces,
            bindings,
            view: Some(view.to_string()),
        };

        let path = Self::manifest_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&manifest) {
            let _ = std::fs::write(&path, json);
        }
    }

    /// Load session manifest from disk.
    fn load_manifest() -> Manifest {
        let path = Self::manifest_path();
        match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Manifest::default(),
        }
    }

    /// Normalize a manifest across format versions:
    ///   - v1: flat `sessions` map, keyed by worktree/task:id, no workspaces.
    ///   - v2: `workspaces` keyed by worktree/task:id, with extras + is_standalone.
    ///   - v3: `workspaces` keyed by workspace_id, plus `bindings` map.
    ///
    /// After migration: each workspace entry is keyed by its stable id; legacy
    /// `extra_bound_tasks` get folded into the `bindings` map.
    fn migrate_manifest(mut m: Manifest) -> Manifest {
        // v1 → v2: legacy sessions map → synthetic workspace entries.
        for (key, sessions) in m.sessions.drain() {
            m.workspaces
                .entry(key)
                .or_insert_with(ManifestWorkspace::default)
                .sessions = sessions;
        }

        // v1/v2 → v3: rekey by workspace_id (assigning one if missing) and
        // fold extras into the bindings map.
        let mut rekeyed: HashMap<String, ManifestWorkspace> = HashMap::new();
        for (legacy_key, mut mw) in m.workspaces.drain() {
            if mw.id.is_empty() {
                mw.id = new_workspace_id();
            }
            // Seed worktree_path / is_cloud from the legacy key if they're
            // missing — this matters for v1 manifests that only had the key.
            if mw.worktree_path.is_none() && !legacy_key.starts_with("task:") {
                mw.worktree_path = Some(PathBuf::from(&legacy_key));
            }
            if legacy_key.starts_with("task:") {
                mw.is_cloud = true;
            }
            for bt in mw.extra_bound_tasks.drain(..) {
                if !bt.id.is_empty() {
                    m.bindings.insert(bt.id, mw.id.clone());
                }
            }
            // For v2 cloud workspaces keyed by task:{id}, that task_id is
            // also a binding (it was the "primary" in v2).
            if let Some(task_id) = legacy_key.strip_prefix("task:") {
                m.bindings
                    .entry(task_id.to_string())
                    .or_insert_with(|| mw.id.clone());
            }
            rekeyed.insert(mw.id.clone(), mw);
        }
        m.workspaces = rekeyed;
        m
    }

    /// Restore workspaces + sessions from the manifest. Runs after an
    /// initial API tasks fetch so `bindings` can be cross-referenced with
    /// real tasks, but also works standalone (workspaces without any bound
    /// tasks are legal).
    fn restore_sessions(&mut self) {
        let manifest = Self::migrate_manifest(Self::load_manifest());
        if manifest.workspaces.is_empty() && manifest.bindings.is_empty() {
            return;
        }

        let (cols, rows) = self.last_term_size;

        // Rebuild self.workspaces from the manifest. Closed workspaces are
        // loaded with empty sessions (their PTY state is gone anyway).
        for (_, mw) in manifest.workspaces.iter() {
            let already = self.workspaces.iter().any(|w| w.id == mw.id);
            if already {
                continue;
            }
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
            };
            if !ws.is_closed {
                for entry in &mw.sessions {
                    let ts = Self::spawn_restored_session(
                        entry,
                        &ws,
                        (cols, rows),
                        &self.config,
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
    ) -> Option<TerminalSession> {
        let cloud_vm = ws.worker_vm.as_deref().filter(|s| !s.is_empty());
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
            Session::new("gcloud", &args, cols, rows, None, Default::default())
        } else if entry.session_type == "claude" {
            let wt = ws.worktree_path.clone();
            let mut args = vec!["--dangerously-skip-permissions".to_string()];
            if let Some(ref sid) = entry.session_id {
                args.push("--resume".to_string());
                args.push(sid.clone());
            }
            Session::new("claude", &args, cols, rows, wt, Default::default())
        } else if entry.session_type == "codex" {
            let wt = ws.worktree_path.clone();
            let mut args = vec!["--yolo".to_string()];
            if let Some(ref sid) = entry.session_id {
                args.push("resume".to_string());
                args.push(sid.clone());
            }
            Session::new("codex", &args, cols, rows, wt, Default::default())
        } else {
            let wt = ws.worktree_path.clone();
            Session::new("/bin/bash", &[], cols, rows, wt, Default::default())
        };
        let s = result.ok()?;
        let pending = if entry.session_id.is_some() {
            None
        } else if matches!(entry.session_type.as_str(), "claude" | "codex") {
            Some(Vec::new())
        } else {
            None
        };
        Some(TerminalSession {
            label: entry.label.clone(),
            session_type: entry.session_type.clone(),
            session: s,
            status: SessionStatus::Running,
            last_write_at: None,
            session_id: entry.session_id.clone(),
            pending_jsonl_files: pending,
            hidden: entry.hidden,
            idle_timeout_secs: entry.idle_timeout_secs,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: entry.workflow_run_id.clone(),
            workflow_role: entry.workflow_role.clone(),
            last_delivery: None,
        })
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
                        self.input_mode = InputMode::SessionSettings {
                            ws_index: wi,
                            session_index: si,
                            name: ts.label.clone(),
                            idle_timeout: if timeout == 0 {
                                DEFAULT_IDLE_TIMEOUT_SECS.to_string()
                            } else {
                                timeout.to_string()
                            },
                            hidden: ts.hidden,
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
        }
    }

    /// Soft-close the workspace under the cursor: kill its session PTYs
    /// and hide from the sidebar. Worktree stays on disk; bindings persist.
    fn close_active_workspace(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        if let Some(ws) = self.workspaces.get_mut(wi) {
            for ts in &mut ws.sessions {
                ts.session.exited = true;
            }
            ws.sessions.clear();
            ws.is_closed = true;
        }
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
        let (wi, si) = match self.cursor {
            Cursor::Session(wi, si) => (wi, si),
            Cursor::Workspace(wi) => {
                if self.workspaces.get(wi).map_or(false, |w| w.sessions.len() == 1) {
                    (wi, 0)
                } else {
                    return;
                }
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
        let wi = match self.cursor {
            Cursor::Workspace(wi) => wi,
            Cursor::Session(wi, _) => wi,
        };
        (wi < self.workspaces.len()).then_some(wi)
    }

    /// Return a reference to the active terminal session (workspace + session).
    fn active_session(&self) -> Option<(&Workspace, &TerminalSession)> {
        match self.cursor {
            Cursor::Session(wi, si) => {
                let ws = self.workspaces.get(wi)?;
                let ts = ws.sessions.get(si)?;
                Some((ws, ts))
            }
            Cursor::Workspace(wi) => {
                let ws = self.workspaces.get(wi)?;
                if ws.sessions.len() == 1 {
                    Some((ws, &ws.sessions[0]))
                } else {
                    None
                }
            }
        }
    }

    /// Return a mutable reference to the active terminal session.
    fn active_session_mut(&mut self) -> Option<&mut TerminalSession> {
        match self.cursor {
            Cursor::Session(wi, si) => {
                let ws = self.workspaces.get_mut(wi)?;
                ws.sessions.get_mut(si)
            }
            Cursor::Workspace(wi) => {
                let ws = self.workspaces.get_mut(wi)?;
                if ws.sessions.len() == 1 {
                    Some(&mut ws.sessions[0])
                } else {
                    None
                }
            }
        }
    }

    // ── Workspace / task lookup helpers ─────────────────────────────

    fn workspace_index_by_id(&self, id: &str) -> Option<usize> {
        self.workspaces.iter().position(|w| w.id == id)
    }

    /// First task bound to the given workspace, if any. Used by push/pull
    /// (which need *a* representative task) and the detail panel (shows one
    /// prompt). Multi-task workspaces have no canonical ordering; first-
    /// insertion-wins.
    fn first_task_for_ws(&self, ws_id: &str) -> Option<&TaskEntry> {
        self.tasks
            .iter()
            .find(|t| t.workspace_id.as_deref() == Some(ws_id))
    }

    /// All task names bound to the given workspace, in binding order.
    /// Used for the subtitle line under the workspace header.
    fn task_names_for_ws(&self, ws_id: &str) -> Vec<String> {
        self.tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(ws_id))
            .map(|t| t.name.clone())
            .collect()
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
        match self.cursor {
            Cursor::Workspace(wi) => {
                if wi > max {
                    self.cursor = Cursor::Workspace(max);
                }
            }
            Cursor::Session(wi, si) => {
                if wi > max {
                    self.cursor = Cursor::Workspace(max);
                } else if self.workspaces[wi].sessions.is_empty() {
                    self.cursor = Cursor::Workspace(wi);
                } else if si >= self.workspaces[wi].sessions.len() {
                    self.cursor =
                        Cursor::Session(wi, self.workspaces[wi].sessions.len() - 1);
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
    fn visual_items_status(&self) -> Vec<VisualItem> {
        let mut running: Vec<VisualItem> = Vec::new();
        let mut idle: Vec<VisualItem> = Vec::new();
        let mut no_session: Vec<VisualItem> = Vec::new();

        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed {
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
    /// subheader. Standalone sessions render first; each workflow group follows.
    fn visual_items_task(&self) -> Vec<VisualItem> {
        let mut items = Vec::new();
        let mut first = true;
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed {
                continue;
            }
            if !first {
                items.push(VisualItem::Separator);
            }
            first = false;
            items.push(VisualItem::WorkspaceHeader(wi));

            // Partition sessions: those in workflow groups vs. standalone.
            let mut standalone: Vec<usize> = Vec::new();
            let mut groups: std::collections::BTreeMap<String, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (si, ts) in ws.sessions.iter().enumerate() {
                match &ts.workflow_run_id {
                    Some(run_id) => groups.entry(run_id.clone()).or_default().push(si),
                    None => standalone.push(si),
                }
            }

            // Standalone: running first, then idle.
            let (standalone_running, standalone_other): (Vec<_>, Vec<_>) = standalone
                .into_iter()
                .partition(|si| ws.sessions[*si].status == SessionStatus::Running);
            for si in standalone_running {
                items.push(VisualItem::Session(wi, si));
            }
            for si in standalone_other {
                items.push(VisualItem::Session(wi, si));
            }

            // Workflow groups: header + sessions in role-order from the workflow def.
            for (run_id, session_indices) in groups {
                let is_active_run = self.workflow_runs.iter().any(|r| r.run_id == run_id);
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
        let is_selectable = |item: &VisualItem| match item {
            VisualItem::Session(_, _) => true,
            VisualItem::WorkspaceHeader(wi) => self
                .workspaces
                .get(*wi)
                .map_or(false, |w| w.sessions.is_empty()),
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
        // Collected during the loop: (ws_id, session_id) pairs for sessions
        // whose session_id was freshly detected. Resolved to task_id after
        // the loop so we don't double-borrow self.
        let mut ws_sid_updates: Vec<(String, String)> = Vec::new();
        for ws in &mut self.workspaces {
            let ws_id_here = ws.id.clone();
            let worktree_path = ws.worktree_path.clone();
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
                                    ts.session.write(response.as_bytes());
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
                        let burst = recent_count >= WAKEUP_BURST_THRESHOLD;
                        // Quiet = no wakeups at all in the full idle window → mark idle.
                        let quiet = ts.session.wakeup_times.is_empty();
                        if quiet && ts.status == SessionStatus::Running {
                            ts.status = SessionStatus::Idle;
                        } else if burst && ts.status != SessionStatus::Running {
                            ts.status = SessionStatus::Running;
                        }
                    }
                }

                // Deliver queued `/clear` first once the PTY is quiet (or
                // the hard deadline hits). Sequenced before pending_prompt so
                // the prompt always lands AFTER /clear has been processed.
                if let Some(clear) = &ts.pending_clear {
                    if Self::ready_for_write(&ts.session, clear, now) {
                        let pw = ts.pending_clear.take().unwrap();
                        Self::deliver_pending_write(ts, &pw, "pending_clear");
                    }
                }

                // Only deliver the prompt once the /clear (if any) is gone.
                if ts.pending_clear.is_none() {
                    if let Some(prompt) = &ts.pending_prompt {
                        if Self::ready_for_write(&ts.session, prompt, now) {
                            let pw = ts.pending_prompt.take().unwrap();
                            Self::deliver_pending_write(ts, &pw, "pending_prompt");
                        }
                    }
                }

                // Detect session_id for claude/codex sessions that don't have one yet.
                //
                // Skip claude WORKFLOW sessions — the "newest new .jsonl"
                // heuristic is unreliable when multiple claude processes
                // share a project directory (another process's /clear
                // rotation can produce a new .jsonl right when we're
                // looking). For those we use history.jsonl correlation
                // via `resolve_pending_deliveries` instead.
                let skip_workflow_claude =
                    ts.session_type == "claude" && ts.workflow_run_id.is_some();
                if should_check_session_ids
                    && !skip_workflow_claude
                    && (ts.session_type == "claude" || ts.session_type == "codex")
                    && ts.session_id.is_none()
                    && ts.pending_jsonl_files.is_some()
                {
                    if let Some(ref wt) = worktree_path {
                        let existing = ts.pending_jsonl_files.as_ref().unwrap();
                        let sid = if ts.session_type == "codex" {
                            Self::detect_codex_session_id(wt, existing)
                        } else {
                            Self::detect_session_id(wt, existing)
                        };
                        if let Some(sid) = sid {
                            ts.session_id = Some(sid.clone());
                            ts.pending_jsonl_files = None;
                            ws_sid_updates.push((ws_id_here.clone(), sid));
                        }
                    }
                }

            }
        }

        // Sync any newly detected session_ids to the DB. Resolve each ws_id
        // to bound tasks and push an update per bound task.
        for (ws_id, sid) in ws_sid_updates {
            for task in &self.tasks {
                if task.workspace_id.as_deref() != Some(&ws_id) {
                    continue;
                }
                let Some(task_id) = task.task_id.clone() else {
                    continue;
                };
                let mut fields = HashMap::new();
                fields.insert(
                    "session_id".to_string(),
                    serde_json::Value::String(sid.clone()),
                );
                self.backend.update_task(task_id, fields);
            }
        }

        if should_check_session_ids {
            self.last_session_id_check = now;
        }
        if had_event {
            self.needs_redraw = true;
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
        // Build (sid → (run_id, role, worktree)) lookup for active claude roles.
        let mut bindings: HashMap<String, (String, String, std::path::PathBuf)> = HashMap::new();
        for run in &self.workflow_runs {
            if !run.is_active() {
                continue;
            }
            for (role, binding) in &run.role_sessions {
                let Some(sid) = &binding.current_session_id else {
                    continue;
                };
                let Some((wi, si)) = self.locate_workflow_session(&run.run_id, role) else {
                    continue;
                };
                if self.workspaces[wi].sessions[si].session_type != "claude" {
                    continue;
                }
                let Some(wt) = self.workspaces[wi].worktree_path.clone() else {
                    continue;
                };
                bindings.insert(sid.clone(), (run.run_id.clone(), role.clone(), wt));
            }
        }
        // Walk pending queue; resolve what we can, drop stale ones.
        let now = Instant::now();
        let max_age = Duration::from_secs(30);
        let mut resolved: Vec<(String, String, String, String)> = Vec::new();
        self.pending_rotations.retain(|(old_sid, ts_ms, first_seen)| {
            if now.duration_since(*first_seen) > max_age {
                return false;
            }
            let Some((run_id, role, wt)) = bindings.get(old_sid) else {
                return true;
            };
            let Some(new_sid) = workflow::history::find_post_rotation_sid(wt, *ts_ms) else {
                return true;
            };
            if &new_sid == old_sid {
                return false;
            }
            resolved.push((
                run_id.clone(),
                role.clone(),
                old_sid.clone(),
                new_sid,
            ));
            false
        });
        for (run_id, role, old_sid, new_sid) in &resolved {
            let Some((wi, si)) = self.locate_workflow_session(run_id, role) else {
                continue;
            };
            self.workspaces[wi].sessions[si].session_id = Some(new_sid.clone());
            let Some(run) = self.workflow_runs.iter_mut().find(|r| &r.run_id == run_id)
            else {
                continue;
            };
            if let Some(b) = run.role_sessions.get_mut(role) {
                b.current_session_id = Some(new_sid.clone());
            }
            run.role_baselines
                .insert(role.clone(), workflow::run::MessageBaseline::default());
            if run.active_role.as_deref() == Some(role.as_str()) {
                if let Some(h) = run.history.last_mut() {
                    h.assistant_count_at_start = 0;
                    h.session_id = Some(new_sid.clone());
                }
            }
            let _ = workflow::run::save(run);
            log_tick(
                run_id,
                &format!(
                    "history-rotation: role={} {} -> {}",
                    role, old_sid, new_sid
                ),
            );
        }
        if !resolved.is_empty() {
            self.save_session_manifest();
            self.set_status_msg("Workflow: session rotated (/clear or /compact)");
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

    /// Process planning editor events (non-blocking).
    pub fn drain_planning_events(&mut self) {
        if let Some(action) = self.planning.drain_editor_events() {
            match action {
                PlanAction::UpdateTask { id, fields } => {
                    self.backend.update_plan_task(id, fields);
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
                    || ts.session_id.is_some()
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
                    let content_matches = e.display.starts_with(prefix.as_str())
                        || e.paste_content.starts_with(prefix.as_str());
                    if !content_matches {
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
            ts.session_id = Some(sid.clone());
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
        };
        let saved_session_label = match &self.cursor {
            Cursor::Session(wi, si) => self
                .workspaces
                .get(*wi)
                .and_then(|w| w.sessions.get(*si))
                .map(|s| s.label.clone()),
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
            let is_local = !is_cloud
                && task
                    .wip_branch
                    .as_ref()
                    .map_or(false, |b| b.starts_with("cm/"));

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

            let (worktree_path, main_repo_path) = if is_local {
                let wt = task
                    .wip_branch
                    .as_ref()
                    .and_then(|b| {
                        let slug = b.strip_prefix("cm/").unwrap_or(b);
                        let repo_name = task
                            .repo_url
                            .trim_end_matches('/')
                            .trim_end_matches(".git")
                            .rsplit('/')
                            .next()
                            .unwrap_or("repo");
                        let path = dirs::home_dir()
                            .unwrap_or_default()
                            .join(format!(".cm/worktrees/{}-{}", repo_name, slug));
                        path.exists().then_some(path)
                    });
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
                if let Some(ref label) = saved_session_label {
                    if let Some(si) = self.workspaces[wi]
                        .sessions
                        .iter()
                        .position(|s| &s.label == label)
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
                    prompt,
                } => {
                    self.launch_into_workspace(
                        &workspace_id,
                        &task_id,
                        &task_title,
                        &task_repo_url,
                        &prompt,
                    );
                    return true;
                }
                PlanAction::UnbindTask { task_id } => {
                    self.unbind_task_from_workspace(&task_id);
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
                PlanAction::CreateTask { project, repo_url, name, description, status } => {
                    self.backend.create_plan_task(project, repo_url, name, description, status);
                    return true;
                }
                PlanAction::UpdateTask { id, fields } => {
                    self.backend.update_plan_task(id, fields);
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
                        self.mark_active_done();
                        return true;
                    }
                    KeyCode::Char('x') => {
                        self.delete_active();
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
                    KeyCode::Char('o') => {
                        self.stop_workflow_for_cursor();
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
                    ts.session.write(&data);
                    ts.last_write_at = Some(Instant::now());
                    return true;
                }
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
        if let Some(ts) = self.active_session_mut() {
            if !ts.session.exited {
                // Auto-scroll to bottom on any input so the cursor stays visible.
                {
                    use alacritty_terminal::grid::Scroll;
                    ts.session.term.lock().scroll_display(Scroll::Bottom);
                }
                let term_mode = *ts.session.term.lock().mode();
                if let Some(bytes) = input::event_to_bytes(event, &term_mode) {
                    ts.session.write(&bytes);
                    ts.last_write_at = Some(Instant::now());
                }
                return true;
            }
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
        if let CrosstermEvent::Key(key) = event {
            match &mut self.input_mode {
                InputMode::Normal => return false,
                InputMode::NewSession {
                    label_text,
                    branch_text,
                    idle_timeout_text,
                    repo_url,
                    active_field,
                } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Tab => {
                        *active_field = (*active_field + 1) % 3;
                        return true;
                    }
                    KeyCode::BackTab => {
                        *active_field = if *active_field == 0 { 2 } else { *active_field - 1 };
                        return true;
                    }
                    KeyCode::Enter => {
                        if !label_text.trim().is_empty() {
                            let label = label_text.clone();
                            let repo = repo_url.clone();
                            let branch = if branch_text.trim().is_empty() {
                                None
                            } else {
                                Some(branch_text.clone())
                            };
                            let timeout = idle_timeout_text.parse::<u16>().unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
                            self.input_mode = InputMode::Normal;
                            self.create_local_session(
                                &repo,
                                &label,
                                branch.as_deref(),
                                timeout,
                            );
                        }
                        return true;
                    }
                    KeyCode::Backspace => {
                        match *active_field {
                            0 => { label_text.pop(); }
                            1 => { branch_text.pop(); }
                            2 => { idle_timeout_text.pop(); }
                            _ => {}
                        }
                        return true;
                    }
                    KeyCode::Char(c) => {
                        match *active_field {
                            0 => label_text.push(c),
                            1 => branch_text.push(c),
                            2 => { if c.is_ascii_digit() { idle_timeout_text.push(c); } }
                            _ => {}
                        }
                        return true;
                    }
                    _ => return true,
                },
                InputMode::NewTerminalSession {
                    ws_index,
                    session_type,
                } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Char('j') | KeyCode::Tab | KeyCode::Down => {
                        *session_type = match session_type.as_str() {
                            "claude" => "codex".to_string(),
                            "codex" => "bash".to_string(),
                            _ => "claude".to_string(),
                        };
                        return true;
                    }
                    KeyCode::Char('k') | KeyCode::BackTab | KeyCode::Up => {
                        *session_type = match session_type.as_str() {
                            "claude" => "bash".to_string(),
                            "bash" => "codex".to_string(),
                            _ => "claude".to_string(),
                        };
                        return true;
                    }
                    KeyCode::Enter => {
                        let wi = *ws_index;
                        let st = session_type.clone();
                        self.input_mode = InputMode::Normal;
                        self.spawn_session_on_workspace(wi, &st);
                        return true;
                    }
                    _ => return true,
                },
                InputMode::SessionSettings {
                    ws_index,
                    session_index,
                    name,
                    idle_timeout,
                    hidden,
                    active_field,
                } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        *active_field = (*active_field + 1) % 3;
                        return true;
                    }
                    KeyCode::Char(' ') if *active_field == 2 => {
                        *hidden = !*hidden;
                        return true;
                    }
                    KeyCode::Enter => {
                        let wi = *ws_index;
                        let si = *session_index;
                        let new_name = name.clone();
                        let new_timeout = idle_timeout.parse::<u16>().unwrap_or(DEFAULT_IDLE_TIMEOUT_SECS);
                        let new_hidden = *hidden;
                        self.input_mode = InputMode::Normal;
                        if let Some(ws) = self.workspaces.get_mut(wi) {
                            if let Some(ts) = ws.sessions.get_mut(si) {
                                if !new_name.trim().is_empty() {
                                    ts.label = new_name;
                                }
                                ts.idle_timeout_secs = new_timeout;
                                ts.hidden = new_hidden;
                            }
                        }
                        self.save_session_manifest();
                        self.set_status_msg("Settings saved");
                        return true;
                    }
                    KeyCode::Backspace => {
                        match *active_field {
                            0 => { name.pop(); }
                            1 => { idle_timeout.pop(); }
                            _ => {}
                        }
                        return true;
                    }
                    KeyCode::Char(c) => {
                        match *active_field {
                            0 => name.push(c),
                            1 => {
                                if c.is_ascii_digit() { idle_timeout.push(c); }
                            }
                            _ => {}
                        }
                        return true;
                    }
                    _ => return true,
                },
                InputMode::WorkspaceSettings { ws_index, name } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Enter => {
                        let wi = *ws_index;
                        let new_name = name.trim().to_string();
                        self.input_mode = InputMode::Normal;
                        if !new_name.is_empty() {
                            if let Some(ws) = self.workspaces.get_mut(wi) {
                                ws.name = new_name;
                            }
                            self.save_session_manifest();
                            self.set_status_msg("Workspace renamed");
                        }
                        return true;
                    }
                    KeyCode::Backspace => {
                        name.pop();
                        return true;
                    }
                    KeyCode::Char(c) => {
                        name.push(c);
                        return true;
                    }
                    _ => return true,
                },
                InputMode::WorkflowLaunchConfirm { ws_index, workflow_name, slots, active_slot } => {
                    match key.code {
                        KeyCode::Esc => {
                            self.input_mode = InputMode::Normal;
                            return true;
                        }
                        KeyCode::Enter => {
                            let wi = *ws_index;
                            let wf_name = workflow_name.clone();
                            let slots_owned = slots.clone();
                            self.input_mode = InputMode::Normal;
                            self.launch_workflow(wi, &wf_name, slots_owned);
                            return true;
                        }
                        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                            if !slots.is_empty() {
                                *active_slot = (*active_slot + 1) % slots.len();
                            }
                            return true;
                        }
                        KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                            if !slots.is_empty() {
                                *active_slot = if *active_slot == 0 {
                                    slots.len() - 1
                                } else {
                                    *active_slot - 1
                                };
                            }
                            return true;
                        }
                        KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                            if let Some(slot) = slots.get_mut(*active_slot) {
                                slot.cycle(1);
                            }
                            return true;
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            if let Some(slot) = slots.get_mut(*active_slot) {
                                slot.cycle(-1);
                            }
                            return true;
                        }
                        _ => return true,
                    }
                }
                InputMode::WorkflowHistory { run_id: _ } => match key.code {
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    _ => return true,
                },
            }
        }
        true
    }

    // ── Session management ──────────────────────────────────────────

    /// Enter input mode to create a new workspace (empty, no task binding).
    fn start_new_session(&mut self) {
        // Use the first repo from config.
        let (_repo_name, repo_url) = match self.config.repos.iter().next() {
            Some((name, url)) => (name.clone(), url.clone()),
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
            active_field: 0,
        };
    }

    /// Enter input mode to add a terminal session to the active workspace.
    fn start_new_terminal_session(&mut self) {
        let wi = match self.active_workspace_index() {
            Some(wi) => wi,
            None => {
                self.set_status_msg("No workspace selected");
                return;
            }
        };
        self.input_mode = InputMode::NewTerminalSession {
            ws_index: wi,
            session_type: "claude".to_string(),
        };
    }

    /// Close the current session (remove from workspace.sessions).
    fn close_active_session(&mut self) {
        match self.cursor.clone() {
            Cursor::Session(wi, si) => {
                if let Some(ws) = self.workspaces.get_mut(wi) {
                    if si < ws.sessions.len() {
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
                        ws.sessions.remove(0);
                        self.cursor = Cursor::Workspace(wi);
                        self.save_session_manifest();
                        self.set_status_msg("Session closed");
                    }
                }
            }
        }
    }

    /// Create a fresh standalone workspace — A-n flow. No task binding.
    fn create_local_session(
        &mut self,
        repo_url: &str,
        label: &str,
        start_branch: Option<&str>,
        idle_timeout_secs: u16,
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

        let worktree_path = match worktree::create_worktree(&main_repo, &slug, start_branch) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_msg(&format!("Worktree: {}", e));
                return;
            }
        };
        worktree::setup_worktree(&main_repo, &worktree_path);

        let (cols, rows) = self.last_term_size;
        let args = vec!["--dangerously-skip-permissions".to_string()];
        let pending = Self::list_jsonl_files(&worktree_path);

        let Ok(s) = Session::new(
            "claude",
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        ) else {
            self.set_status_msg("Spawn failed");
            return;
        };

        let ts = TerminalSession {
            label: "claude".to_string(),
            session_type: "claude".to_string(),
            session: s,
            status: SessionStatus::Running,
            last_write_at: None,
            session_id: None,
            pending_jsonl_files: Some(pending),
            hidden: false,
            idle_timeout_secs,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: None,
            workflow_role: None,
            last_delivery: None,
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
            Session::new("gcloud", &args, cols, rows, None, Default::default())
                .ok()
                .map(|s| make_simple_session("ssh", "bash", s, None))
        } else if let Some(wt) = ws.worktree_path.clone() {
            let args = vec!["--dangerously-skip-permissions".to_string()];
            let pending = Self::list_jsonl_files(&wt);
            Session::new("claude", &args, cols, rows, Some(wt), Default::default())
                .ok()
                .map(|s| make_simple_session("claude", "claude", s, Some(pending)))
        } else {
            Session::new("/bin/bash", &[], cols, rows, None, Default::default())
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
    fn spawn_session_on_workspace(&mut self, ws_index: usize, session_type: &str) {
        if ws_index >= self.workspaces.len() {
            return;
        }
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
                match Session::new("gcloud", &args, cols, rows, None, Default::default()) {
                    Ok(s) => {
                        let ts = make_simple_session(&tmux_name, "bash", s, None);
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
        let pending = match session_type {
            "claude" => wt.as_ref().map(|p| Self::list_jsonl_files(p)),
            "codex" => wt.as_ref().map(|p| Self::list_codex_sessions(p)),
            _ => None,
        };
        let result = match session_type {
            "claude" => {
                let args = vec!["--dangerously-skip-permissions".to_string()];
                Session::new("claude", &args, cols, rows, wt, Default::default())
            }
            "codex" => {
                let args = vec!["--yolo".to_string()];
                Session::new("codex", &args, cols, rows, wt, Default::default())
            }
            _ => Session::new("/bin/bash", &[], cols, rows, wt, Default::default()),
        };
        match result {
            Ok(s) => {
                let ts = make_simple_session(session_type, session_type, s, pending);
                let si = self.workspaces[ws_index].sessions.len();
                self.workspaces[ws_index].sessions.push(ts);
                self.cursor = Cursor::Session(ws_index, si);
                self.save_session_manifest();
                self.set_status_msg(&format!("Started {} session", session_type));
            }
            Err(e) => self.set_status_msg(&format!("Spawn: {}", e)),
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
        let args = vec![
            "--dangerously-skip-permissions".to_string(),
            "--resume".to_string(),
            session_id.clone(),
        ];

        match Session::new(
            "claude",
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        ) {
            Ok(s) => {
                let mut ts = make_simple_session("claude", "claude", s, None);
                ts.session_id = Some(session_id.clone());

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
        let ws_id = self.workspaces[wi].id.clone();
        let bound: Vec<String> = self
            .tasks
            .iter()
            .filter(|t| t.workspace_id.as_deref() == Some(&ws_id))
            .filter_map(|t| t.task_id.clone())
            .collect();
        if bound.len() > 1 {
            self.set_status_msg("Multiple tasks bound — use planning view");
            return;
        }
        if let Some(tid) = bound.first().cloned() {
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
        }
        // Drop sessions and leave workspace alive; user can explicitly close
        // via A-W if desired.
        self.workspaces[wi].sessions.clear();
        self.cursor = Cursor::Workspace(wi);
        self.clamp_cursor();
        self.set_status_msg("Marked done");
    }

    /// Delete the active workspace: close sessions, remove worktree + branch,
    /// delete any bound tasks from the API, drop the workspace.
    fn delete_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
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

        if let (Some(ref wt), Some(ref repo)) = (&worktree_path, &main_repo_path) {
            worktree::remove_worktree(repo, wt);
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
    fn push_active(&mut self) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        if self.workspaces[wi].is_cloud {
            self.set_status_msg("Can only push local workspaces");
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

        self.backend.push(worktree_path, repo_url, name, task_id);

        // Clear local sessions and worktree; mark workspace as cloud.
        self.workspaces[wi].sessions.clear();
        self.workspaces[wi].worktree_path = None;
        self.workspaces[wi].is_cloud = true;
        if let Some(task) = self
            .tasks
            .iter_mut()
            .find(|t| t.workspace_id.as_deref() == Some(&ws_id))
        {
            task.is_cloud = true;
        }
        self.cursor = Cursor::Workspace(wi);
        self.set_status_msg("Pushing to cloud...");
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
        let args = vec!["--dangerously-skip-permissions".to_string()];
        let pending = Self::list_jsonl_files(&worktree_path);

        match Session::new(
            "claude",
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        ) {
            Ok(s) => {
                let branch = format!("cm/{}", slug);
                let mut ts = make_simple_session("claude", "claude", s, Some(pending));
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
        let args = vec!["--dangerously-skip-permissions".to_string()];
        let pending = Self::list_jsonl_files(&worktree_path);
        match Session::new(
            "claude",
            &args,
            cols,
            rows,
            Some(worktree_path),
            Default::default(),
        ) {
            Ok(s) => {
                let mut ts = make_simple_session("claude", "claude", s, Some(pending));
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

        let rows =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
                .split(area);

        let content_area = rows[0];
        let bar_area = rows[1];

        match self.view_mode {
            ViewMode::Sessions => {
                let cols =
                    Layout::horizontal([Constraint::Min(40), Constraint::Length(30)])
                        .split(content_area);

                self.draw_terminal(frame, cols[0]);
                self.draw_session_list(frame, cols[1]);
            }
            ViewMode::Planning => {
                self.planning.draw(frame, content_area);
            }
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
                    active_field,
                } => {
                    self.draw_input_dialog(
                        frame,
                        area,
                        label_text,
                        branch_text,
                        idle_timeout_text,
                        repo_url,
                        *active_field,
                    );
                }
                InputMode::NewTerminalSession {
                    ws_index,
                    session_type,
                } => {
                    self.draw_new_terminal_dialog(
                        frame,
                        area,
                        *ws_index,
                        session_type,
                    );
                }
                InputMode::SessionSettings { name, idle_timeout, hidden, active_field, .. } => {
                    self.draw_session_settings(frame, area, name, idle_timeout, *hidden, *active_field);
                }
                InputMode::WorkspaceSettings { name, .. } => {
                    self.draw_workspace_settings(frame, area, name);
                }
                InputMode::WorkflowLaunchConfirm { ws_index, workflow_name, slots, active_slot } => {
                    self.draw_workflow_launch(
                        frame,
                        area,
                        *ws_index,
                        workflow_name,
                        slots,
                        *active_slot,
                    );
                }
                InputMode::WorkflowHistory { run_id } => {
                    self.draw_workflow_history(frame, area, run_id);
                }
                InputMode::Normal => {}
            }
        }
    }

    fn draw_input_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        label_text: &str,
        branch_text: &str,
        idle_timeout_text: &str,
        repo_url: &str,
        active_field: u8,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 11u16;
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

        let name_cursor = if active_field == 0 { cursor } else { "" };
        let branch_cursor = if active_field == 1 { cursor } else { "" };
        let timeout_cursor = if active_field == 2 { cursor } else { "" };

        let branch_hint = if branch_text.is_empty() && active_field != 1 {
            "main"
        } else {
            ""
        };

        let lines = vec![
            Line::from(vec![
                Span::styled("    Repo: ", dim),
                Span::styled(repo_name, white),
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
        ws_index: usize,
        session_type: &str,
    ) {
        let width = 50u16.min(area.width.saturating_sub(4));
        let height = 9u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let ws_name = self
            .workspaces
            .get(ws_index)
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

        let mut lines = vec![
            Line::from(vec![
                Span::styled("  Task: ", Style::default().fg(Color::DarkGray)),
                Span::styled(display_name, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
        ];
        for opt in &options {
            let ind = if session_type == *opt { ">" } else { " " };
            let st = if session_type == *opt {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            lines.push(Line::from(Span::styled(format!("  {} {}", ind, opt), st)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "j/k select \u{00b7} Enter start \u{00b7} Esc cancel",
            Style::default().fg(Color::DarkGray),
        )));

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_session_settings(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &str,
        idle_timeout: &str,
        hidden: bool,
        active_field: u8,
    ) {
        let width = 55u16.min(area.width.saturating_sub(4));
        let height = 11u16;
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
        let hidden_marker = if hidden { "[x]" } else { "[ ]" };
        let hidden_style = if active_field == 2 { white } else { dim };

        let lines = vec![
            Line::from(vec![
                Span::styled("      Name: ", dim),
                Span::styled(name, white),
                Span::styled(name_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Idle (s): ", dim),
                Span::styled(idle_timeout, white),
                Span::styled(timeout_cursor, white),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("    Hidden: ", dim),
                Span::styled(hidden_marker, hidden_style),
                Span::styled(if active_field == 2 { "  Space to toggle" } else { "" }, dim),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Tab next field \u{00b7} Enter save \u{00b7} Esc cancel",
                dim,
            )),
        ];

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
        let list_height = inner.height.saturating_sub(8);
        let dim = Style::default().fg(Color::DarkGray);

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

                    // If any task is bound, show titles as a dim subtitle.
                    let bound = self.task_names_for_ws(&ws.id);
                    if !bound.is_empty() && self.sidebar_view == SidebarView::Task {
                        let joined = bound.join(" \u{00b7} ");
                        let budget = (inner.width as usize).saturating_sub(5);
                        let subtitle = if joined.len() > budget {
                            format!(" \u{00b7} {}...", &joined[..budget.saturating_sub(6)])
                        } else {
                            format!(" \u{00b7} {}", joined)
                        };
                        let sub_line = Line::from(Span::styled(
                            format!("   {}", subtitle),
                            Style::default().fg(Color::DarkGray),
                        ));
                        items.push(
                            ListItem::new(vec![header_line, sub_line]).style(base_style),
                        );
                    } else {
                        items.push(ListItem::new(header_line).style(base_style));
                    }
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

                    // Role badge for workflow-participant sessions, e.g. "[W]".
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
                            let ch = role
                                .chars()
                                .next()
                                .map(|c| c.to_ascii_uppercase())
                                .unwrap_or('?');
                            Some((format!("[{}] ", ch), style))
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
                            // "  label" indented
                            format!("  {}", ts.label)
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

        // Help text — two columns.
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
            ("A-o    stop wf", ""),
            ("PgUp   scroll up", ""),
            ("PgDn   scroll dn", ""),
            ("A-Ent  newline", ""),
        ];
        let help_rows = help_entries.len() as u16;
        let help_y = inner.y + inner.height.saturating_sub(help_rows + 1);
        let help_area = Rect {
            x: inner.x,
            y: help_y,
            width: inner.width,
            height: help_rows + 1,
        };

        let dim = Style::default().fg(Color::DarkGray);
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

        let right = format!(" {}r {}b {}q ", running, blocked, backlog);

        let right_width = right.chars().count() as u16;
        let center_width = center.len() as u16;
        let left_used = 18u16; // " ● claude-manager "
        let pad = area
            .width
            .saturating_sub(left_used + right_width + center_width);
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

/// Bridges the template engine to a workflow run's roles.
///
/// For a given role name, looks up the engine (from `Workflow`) and session id
/// (from `WorkflowRun.role_sessions`), then reads that session's JSONL transcript
/// on demand. Fresh-context roles naturally expose only their current activation's
/// messages — past activations wrote to a different session id and are no longer
/// pointed at.
struct WorkflowResolver<'a> {
    wf: &'a Workflow,
    run: &'a WorkflowRun,
    worktree_path: Option<&'a Path>,
}

impl<'a> WorkflowResolver<'a> {
    fn lookup(&self, role: &str) -> Option<(&'a workflow::toml_schema::Engine, &'a Path, &'a str)> {
        let role_spec = self.wf.roles.get(role)?;
        let binding = self.run.role_sessions.get(role)?;
        let session_id = binding.current_session_id.as_deref()?;
        let worktree = self.worktree_path?;
        Some((&role_spec.engine, worktree, session_id))
    }
}

impl<'a> workflow::template::RoleResolver for WorkflowResolver<'a> {
    fn user_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let offset = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.user_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(engine, wt, sid, workflow::transcript::MessageKind::User)
            .into_iter()
            .skip(offset)
            .collect()
    }

    fn assistant_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let offset = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.assistant_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(
            engine,
            wt,
            sid,
            workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .skip(offset)
        .collect()
    }

    fn prior_user_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let baseline = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.user_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(engine, wt, sid, workflow::transcript::MessageKind::User)
            .into_iter()
            .take(baseline)
            .collect()
    }

    fn prior_assistant_messages(&self, role: &str) -> Vec<String> {
        let Some((engine, wt, sid)) = self.lookup(role) else {
            return Vec::new();
        };
        let baseline = self
            .run
            .role_baselines
            .get(role)
            .map(|b| b.assistant_count)
            .unwrap_or(0);
        workflow::transcript::list_messages(
            engine,
            wt,
            sid,
            workflow::transcript::MessageKind::Assistant,
        )
        .into_iter()
        .take(baseline)
        .collect()
    }

    fn latest_plan(&self, role: &str) -> Option<String> {
        let (engine, wt, sid) = self.lookup(role)?;
        workflow::transcript::latest_plan(engine, wt, sid)
    }
}

impl App {
    /// The stable key a workflow run stores to refer back to its workspace.
    /// With the v3 data model this is the workspace id directly — no more
    /// worktree-path / `task:{id}` special-casing. Name retained for
    /// compatibility with the `WorkflowRun::task_key` field on disk.
    fn workspace_key(ws: &Workspace) -> String {
        ws.id.clone()
    }

    /// Locate the `(task_index, session_index)` of the session that's tagged
    /// as `role` for workflow run `run_id`. Searches across ALL tasks — the
    /// workflow's stored `task_key` can drift away from reality (sessions can
    /// move, or the workflow may have been launched with a stale task key),
    /// and the tags on the session itself are the source of truth.
    fn locate_workflow_session(&self, run_id: &str, role: &str) -> Option<(usize, usize)> {
        for (wi, ws) in self.workspaces.iter().enumerate() {
            for (si, ts) in ws.sessions.iter().enumerate() {
                if ts.workflow_run_id.as_deref() == Some(run_id)
                    && ts.workflow_role.as_deref() == Some(role)
                {
                    return Some((wi, si));
                }
            }
        }
        None
    }

    /// Open the launch modal for a workflow, prefilled for the focused session.
    fn open_workflow_launch(&mut self) {
        let (wi, focused_si) = match self.cursor.clone() {
            Cursor::Session(wi, si) => (wi, Some(si)),
            Cursor::Workspace(wi) => (wi, None),
        };
        if wi >= self.workspaces.len() {
            self.set_status_msg("No workspace selected");
            return;
        }
        let wf_name = "feedback".to_string();
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
        };
    }

    /// Actually launch a workflow given a workspace and resolved slot choices.
    fn launch_workflow(
        &mut self,
        ws_index: usize,
        workflow_name: &str,
        slots: Vec<WorkflowSlotChoice>,
    ) {
        let Some(wf) = self.workflows.get(workflow_name).cloned() else {
            self.set_status_msg("Workflow not found");
            return;
        };
        if ws_index >= self.workspaces.len() {
            return;
        }

        // Validate: `fresh` slots cannot use existing sessions. Also reject
        // duplicate existing-session assignments across slots.
        let mut existing_seen: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for slot in &slots {
            let role = match wf.roles.get(&slot.role) {
                Some(r) => r,
                None => {
                    self.set_status_msg(&format!("Unknown role: {}", slot.role));
                    return;
                }
            };
            if let WorkflowSlotSource::Existing(si) = slot.source() {
                if matches!(role.context, workflow::toml_schema::Context::Fresh) {
                    self.set_status_msg(&format!(
                        "Role '{}' has fresh context; must use a new session",
                        slot.role
                    ));
                    return;
                }
                if !existing_seen.insert(*si) {
                    self.set_status_msg(
                        "Two roles can't share the same existing session",
                    );
                    return;
                }
            }
        }

        let task_key = Self::workspace_key(&self.workspaces[ws_index]);
        let run_id = workflow::run::new_run_id();
        let worktree_path = self.workspaces[ws_index].worktree_path.clone();

        // Spawn / bind sessions for each slot and build role_sessions.
        // For existing sessions we also snapshot the current user/assistant counts
        // so that templates like `{{ roles.worker.initial_prompt }}` point at the
        // first message *after* this launch, not the first message ever.
        let mut role_sessions: std::collections::BTreeMap<String, RoleBinding> =
            std::collections::BTreeMap::new();
        let mut role_baselines: std::collections::BTreeMap<String, MessageBaseline> =
            std::collections::BTreeMap::new();
        for slot in &slots {
            let role = &wf.roles[&slot.role];
            let (session_label, session_id, effective_engine) = match slot.source() {
                WorkflowSlotSource::Existing(si) => {
                    // Tag with workflow metadata, and if sid isn't known yet,
                    // try to detect it NOW (newest JSONL heuristic) so the
                    // baseline below is computed from the actual transcript.
                    let worktree_for_detect = self.workspaces[ws_index].worktree_path.clone();
                    let ts = match self.workspaces[ws_index].sessions.get_mut(*si) {
                        Some(s) => s,
                        None => continue,
                    };
                    ts.workflow_run_id = Some(run_id.clone());
                    ts.workflow_role = Some(slot.role.clone());
                    if ts.session_id.is_none() {
                        if let Some(wt) = worktree_for_detect.as_deref() {
                            // Use the session's own pending list (pre-launch
                            // snapshot) if available so detection picks this
                            // session's new JSONL rather than some other
                            // session's in the same worktree. Empty-list
                            // fallback picks the newest overall.
                            let existing: Vec<String> =
                                ts.pending_jsonl_files.clone().unwrap_or_default();
                            let detected = match ts.session_type.as_str() {
                                "claude" => Self::detect_session_id(wt, &existing),
                                "codex" => Self::detect_codex_session_id(wt, &existing),
                                _ => None,
                            };
                            if let Some(sid) = detected {
                                ts.session_id = Some(sid);
                                ts.pending_jsonl_files = None;
                            }
                        }
                    }
                    let eng = match ts.session_type.as_str() {
                        "codex" => Engine::Codex,
                        _ => Engine::ClaudeCode,
                    };
                    (ts.label.clone(), ts.session_id.clone(), eng)
                }
                WorkflowSlotSource::New(engine) => {
                    match self.spawn_workflow_session(ws_index, &slot.role, engine, &run_id) {
                        Some((label, sid)) => (label, sid, engine.clone()),
                        None => {
                            self.set_status_msg(&format!("Failed to spawn {}", slot.role));
                            return;
                        }
                    }
                }
            };
            // Compute baseline now, before the session does any new work.
            // Use count_messages (counts any turn) for assistant_count so the
            // idle gate sees a consistent picture later — it compares current
            // count against baseline.assistant_count at start. user_count
            // still uses list_messages (template slice uses text messages).
            let baseline = match (worktree_path.as_deref(), session_id.as_deref()) {
                (Some(wt), Some(sid)) => MessageBaseline {
                    user_count: workflow::transcript::list_messages(
                        &effective_engine,
                        wt,
                        sid,
                        workflow::transcript::MessageKind::User,
                    )
                    .len(),
                    assistant_count: workflow::transcript::count_messages(
                        &effective_engine,
                        wt,
                        sid,
                        workflow::transcript::MessageKind::Assistant,
                    ),
                },
                _ => MessageBaseline::default(),
            };
            let _ = role;
            role_baselines.insert(slot.role.clone(), baseline);
            role_sessions.insert(
                slot.role.clone(),
                RoleBinding {
                    session_label,
                    current_session_id: session_id,
                },
            );
        }

        // Initial active role = first in role_order.
        let initial_role = wf.role_order.first().cloned().unwrap_or_else(|| "worker".into());
        let run = WorkflowRun::new(
            run_id.clone(),
            workflow_name.to_string(),
            task_key,
            role_sessions,
            initial_role.clone(),
            role_baselines,
        );
        let _ = workflow::run::save(&run);
        self.workflow_runs.push(run);
        self.save_session_manifest();
        self.set_status_msg(&format!(
            "Launched {} ({} roles, initial: {})",
            workflow_name,
            wf.role_order.len(),
            initial_role
        ));
    }

    /// Keep role_sessions.current_session_id aligned with the live
    /// TerminalSession.session_id. Nothing else.
    fn sync_role_session_ids(&mut self) {
        let run_count = self.workflow_runs.len();
        for idx in 0..run_count {
            if !self.workflow_runs[idx].is_active() {
                continue;
            }
            let run_id = self.workflow_runs[idx].run_id.clone();
            let role_names: Vec<String> = self.workflow_runs[idx]
                .role_sessions
                .keys()
                .cloned()
                .collect();
            let mut changed = false;
            for role in role_names {
                let Some((ti, si)) = self.locate_workflow_session(&run_id, &role) else {
                    continue;
                };

                let live = self.workspaces[ti].sessions[si].session_id.clone();
                let binding_sid = self
                    .workflow_runs[idx]
                    .role_sessions
                    .get(&role)
                    .and_then(|b| b.current_session_id.clone());
                if live != binding_sid {
                    if let Some(b) = self.workflow_runs[idx].role_sessions.get_mut(&role) {
                        b.current_session_id = live;
                    }
                    changed = true;
                }
            }
            if changed {
                let _ = workflow::run::save(&self.workflow_runs[idx]);
            }
        }
    }

    /// Spawn a new TerminalSession for a workflow role, returning (label, session_id).
    /// The session_id is usually None immediately — it's detected later via JSONL scan.
    fn spawn_workflow_session(
        &mut self,
        ws_index: usize,
        role_name: &str,
        engine: &Engine,
        run_id: &str,
    ) -> Option<(String, Option<String>)> {
        let worktree_path = self.workspaces[ws_index].worktree_path.clone()?;
        let (cols, rows) = self.last_term_size;
        let (program, args) = match workflow::spawn::build_args(engine, run_id, role_name, None) {
            Ok(v) => v,
            Err(e) => {
                self.set_status_msg(&format!("spawn args: {}", e));
                return None;
            }
        };
        let pending = Some(match engine {
            Engine::ClaudeCode => Self::list_jsonl_files(&worktree_path),
            Engine::Codex => Self::list_codex_sessions(&worktree_path),
        });
        let sess = Session::new(
            &program,
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        )
        .ok()?;
        let session_type = engine.as_session_type().to_string();
        let label = role_name.to_string();
        let ts = TerminalSession {
            label: label.clone(),
            session_type,
            session: sess,
            // Start Idle — PTY startup noise isn't "work". Wakeup-burst
            // detection will flip to Running when the agent actually responds.
            status: SessionStatus::Idle,
            last_write_at: None,
            session_id: None,
            pending_jsonl_files: pending,
            // Participants default hidden — the workflow header carries the
            // aggregate indicator. Toggle per session with A-h.
            hidden: true,
            idle_timeout_secs: 0,
            pending_prompt: None,
            pending_clear: None,
            workflow_run_id: Some(run_id.to_string()),
            workflow_role: Some(role_name.to_string()),
            last_delivery: None,
        };
        self.workspaces[ws_index].sessions.push(ts);
        Some((label, None))
    }

    /// Called once per main loop iteration. Drives transitions for each active run:
    ///   1. Read new events from events.jsonl; fire dynamic transitions / done.
    ///   2. Check if the active role's session went idle; fire static transition.
    ///   3. Auto-pause runs when the user types into a workflow session.
    pub fn tick_workflows(&mut self) {
        if self.workflow_runs.is_empty() {
            return;
        }

        // Keep role_sessions.current_session_id in sync with whatever the
        // live TerminalSession.session_id is. Needed because templating
        // (WorkflowResolver) reads from role_sessions, and sessions may get
        // their sid detected asynchronously (5-second poll) after launch.
        // This is a pure sync — no baseline / start_count mutation, which
        // would shift gates unpredictably.
        self.sync_role_session_ids();

        // Collect decisions first, then apply. (Avoids borrow issues with mutable
        // access to both self.workflow_runs and self.tasks.)
        #[derive(Debug)]
        enum Decision {
            ActivateStatic { run_id: String, to: String, from: String },
            ActivateDynamic {
                run_id: String,
                to: String,
                from: String,
                prompt: String,
                event_id: String,
            },
            Done { run_id: String, reason: String },
        }
        let mut decisions: Vec<Decision> = Vec::new();

        // Snapshot run states.
        let run_snapshots: Vec<(usize, String, u64, Option<String>, bool)> = self
            .workflow_runs
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_active())
            .map(|(i, r)| {
                (
                    i,
                    r.run_id.clone(),
                    r.events_offset,
                    r.active_role.clone(),
                    r.paused,
                )
            })
            .collect();

        for (idx, run_id, offset, active_role, paused) in run_snapshots {
            // Log per-session status so we can tell at a glance whether each
            // role ever reaches Running. Rate-limited by log_tick so this
            // doesn't flood. Now locates sessions by their workflow tags
            // (run_id+role), which is the source of truth — the workflow's
            // stored task_key can drift.
            {
                let role_names: Vec<String> = self.workflow_runs[idx]
                    .role_sessions
                    .keys()
                    .cloned()
                    .collect();
                let mut parts = Vec::new();
                for role in &role_names {
                    let status = match self.locate_workflow_session(&run_id, role) {
                        Some((ti, si)) => {
                            let ts = &self.workspaces[ti].sessions[si];
                            format!(
                                "{:?}{}",
                                ts.status,
                                if ts.session.exited { "(exited)" } else { "" }
                            )
                        }
                        None => "<no session>".to_string(),
                    };
                    parts.push(format!("{}={}", role, status));
                }
                log_tick(
                    &run_id,
                    &format!(
                        "statuses: active={} [{}]",
                        active_role.as_deref().unwrap_or("?"),
                        parts.join(", ")
                    ),
                );
            }

            // Read new events regardless of paused state so the log stays in sync;
            // events are still recorded in history but not fired while paused.
            let (events, new_offset) = workflow::events::read_new(&run_id, offset);
            self.workflow_runs[idx].events_offset = new_offset;

            if paused {
                continue;
            }

            for ev in &events {
                match ev.kind() {
                    workflow::events::EventKind::Transition { to, prompt } => {
                        if let Some(from) = active_role.clone() {
                            decisions.push(Decision::ActivateDynamic {
                                run_id: run_id.clone(),
                                to,
                                from,
                                prompt,
                                event_id: ev.id.clone(),
                            });
                        }
                    }
                    workflow::events::EventKind::Done { reason } => {
                        decisions.push(Decision::Done {
                            run_id: run_id.clone(),
                            reason,
                        });
                    }
                    workflow::events::EventKind::Unknown => {}
                }
            }

            // If no dynamic event fired, check for static idle transition.
            if events.is_empty() {
                let Some(active) = active_role.as_deref() else { continue };
                let wf = self
                    .workflows
                    .get(&self.workflow_runs[idx].workflow_name)
                    .cloned();
                let Some(wf) = wf else { continue };
                // Locate by workflow tags — not by task_key + session_label,
                // which can drift.
                let Some((ti, si)) = self.locate_workflow_session(&run_id, active) else {
                    continue;
                };
                let session_idle = matches!(
                    self.workspaces[ti].sessions[si].status,
                    SessionStatus::Idle
                );
                if session_idle {
                    // Only fire the static transition if the outgoing role has
                    // actually taken a NEW turn since its current activation.
                    // Use `count_messages` (counts any assistant JSONL entry —
                    // including thinking-only / tool-use-only turns) rather
                    // than `list_messages` which skips non-text content and
                    // would undercount real turns.
                    let start_count = self.workflow_runs[idx]
                        .active_assistant_start_count()
                        .unwrap_or(0);
                    let current_sid = self.workspaces[ti].sessions[si].session_id.clone();
                    let current_count = match (
                        self.workspaces[ti].worktree_path.as_deref(),
                        current_sid.as_deref(),
                    ) {
                        (Some(wt), Some(sid)) => workflow::transcript::count_messages(
                            &wf.roles[active].engine,
                            wt,
                            sid,
                            workflow::transcript::MessageKind::Assistant,
                        ),
                        _ => 0,
                    };
                    log_tick(
                        &run_id,
                        &format!(
                            "idle check: role={} sid={:?} start={} current={} will_fire={}",
                            active,
                            current_sid.as_deref().unwrap_or("<none>"),
                            start_count,
                            current_count,
                            current_count > start_count,
                        ),
                    );
                    if current_count > start_count {
                        if let Some(t) = wf.static_transition_on_idle(active) {
                            decisions.push(Decision::ActivateStatic {
                                run_id: run_id.clone(),
                                to: t.to.clone(),
                                from: active.to_string(),
                            });
                        }
                    }
                }
            }
        }

        for d in decisions {
            match d {
                Decision::ActivateStatic { run_id, to, from } => {
                    self.fire_transition(
                        &run_id,
                        &to,
                        TriggerKind::StaticIdle { from_role: from },
                        None,
                    );
                }
                Decision::ActivateDynamic { run_id, to, from, prompt, event_id } => {
                    self.fire_transition(
                        &run_id,
                        &to,
                        TriggerKind::McpTransition { from_role: from, prompt: prompt.clone(), event_id },
                        Some(prompt),
                    );
                }
                Decision::Done { run_id, reason } => {
                    self.finish_run(&run_id, reason);
                }
            }
        }
    }

    /// Execute a role transition: capture outgoing role's last message, render the
    /// target role's prompt, deliver it into the PTY (respawning first if fresh).
    fn fire_transition(
        &mut self,
        run_id: &str,
        to_role: &str,
        trigger: TriggerKind,
        supplied_prompt: Option<String>,
    ) {
        let run_idx = match self.workflow_runs.iter().position(|r| r.run_id == run_id) {
            Some(i) => i,
            None => return,
        };
        let wf_name = self.workflow_runs[run_idx].workflow_name.clone();
        let wf = match self.workflows.get(&wf_name).cloned() {
            Some(w) => w,
            None => return,
        };

        // Locate target role's session by workflow tags (source of truth).
        let Some((ti, si)) = self.locate_workflow_session(run_id, to_role) else {
            return;
        };

        // Capture outgoing role's last assistant message for history.
        let from_role = self.workflow_runs[run_idx].active_role.clone();
        let captured = if let Some(from) = &from_role {
            if let Some((fti, fsi)) = self.locate_workflow_session(run_id, from) {
                let from_role_spec = wf.roles.get(from).cloned();
                let fsid = self.workspaces[fti].sessions[fsi].session_id.clone();
                let fwt = self.workspaces[fti].worktree_path.clone();
                if let (Some(spec), Some(sid), Some(wt)) = (from_role_spec, fsid, fwt) {
                    workflow::transcript::last_message(&spec.engine, &wt, &sid)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        self.workflow_runs[run_idx].close_active_role(captured);

        // Render prompt for target role.
        let target_role_spec = match wf.roles.get(to_role).cloned() {
            Some(r) => r,
            None => return,
        };
        let template_source = supplied_prompt
            .or_else(|| target_role_spec.activation_prompt.clone())
            .unwrap_or_default();
        let worktree_ref = self.workspaces[ti].worktree_path.as_deref();
        let resolver = WorkflowResolver {
            wf: &wf,
            run: &self.workflow_runs[run_idx],
            worktree_path: worktree_ref,
        };
        let rendered = workflow::template::render(&template_source, &resolver);

        if matches!(target_role_spec.context, workflow::toml_schema::Context::Fresh) {
            self.reset_fresh_session(run_id, ti, si);
        }

        // Update role_sessions with (possibly new) session_id from the session.
        let current_sid = self.workspaces[ti].sessions[si].session_id.clone();
        if let Some(b) = self.workflow_runs[run_idx].role_sessions.get_mut(to_role) {
            b.current_session_id = current_sid;
        }

        // Snapshot the target role's current assistant TURN count at activation.
        // Uses `count_messages` (any assistant JSONL entry counts) so that
        // downstream the idle gate compares turn-to-turn regardless of whether
        // the agent's reply contains text, thinking, or tool_use content.
        let start_count = {
            let current_sid = self.workspaces[ti].sessions[si].session_id.clone();
            match (self.workspaces[ti].worktree_path.as_deref(), current_sid.as_deref()) {
                (Some(wt), Some(sid)) => workflow::transcript::count_messages(
                    &target_role_spec.engine,
                    wt,
                    sid,
                    workflow::transcript::MessageKind::Assistant,
                ),
                _ => 0,
            }
        };

        self.workflow_runs[run_idx].activate_role(to_role.to_string(), trigger, start_count);
        let _ = workflow::run::save(&self.workflow_runs[run_idx]);
        let from_label = from_role.as_deref().unwrap_or("?");
        self.set_status_msg(&format!("Workflow: {} → {}", from_label, to_role));

        // Deliver prompt. Trim trailing whitespace first so our explicit "\r"
        // submit lands on non-newline text — otherwise a trailing "\n" in the
        // TOML multiline string gets typed into the input box and the "\r"
        // then only adds another newline instead of submitting. Longer delay
        // for fresh-context roles because they just received a `/clear` and
        // need a beat to reset internal state.
        if !rendered.trim().is_empty() {
            // Queue the prompt to fire at the first moment of PTY quiet.
            // Delivery is sequenced AFTER pending_clear (if any) in the
            // drain loop, so we don't need to pre-compute a "start after
            // clear" time here.
            let pw = PendingWrite::wait_for_quiet(
                rendered.trim_end().to_string(),
                true,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(180),
            );
            let label = self.workspaces[ti].sessions[si].label.clone();
            log_tick(
                run_id,
                &format!(
                    "fire_transition: activated '{}' queued prompt ({} bytes, fires on quiet PTY) on session '{}'",
                    to_role,
                    pw.text.len(),
                    label,
                ),
            );
            self.workspaces[ti].sessions[si].pending_prompt = Some(pw);
        } else {
            log_tick(
                run_id,
                &format!(
                    "fire_transition: activated '{}' but rendered prompt was EMPTY — nothing to deliver",
                    to_role
                ),
            );
        }
        self.save_session_manifest();
    }

    /// Queue `/clear` to reset a fresh-context role's agent. Delivery is
    /// gated on PTY quiet (see `PendingWrite`) so we don't try to type the
    /// command while the agent is still painting its startup UI — that's
    /// when `\r` gets buffered into the input box instead of interpreted
    /// as submit.
    ///
    /// Also invalidates the session's bound sid and role baseline because
    /// claude rotates its transcript file on `/clear`; the new file's sid
    /// is picked up later by the history.jsonl correlator.
    fn reset_fresh_session(&mut self, run_id: &str, ti: usize, si: usize) -> bool {
        let wt = self.workspaces[ti].worktree_path.as_deref().map(|p| p.to_path_buf());
        let label = self.workspaces[ti].sessions[si].label.clone();
        let role_label = label.clone();
        let ts = &mut self.workspaces[ti].sessions[si];
        if ts.session.exited {
            log_tick(run_id, &format!("reset_fresh: session '{}' already exited", label));
            return false;
        }
        // Queue /clear to fire when the PTY first goes quiet. Floor of 1s so
        // we don't fire during the PTY startup noise. Hard deadline 120s in
        // case the agent never goes quiet.
        ts.pending_clear = Some(PendingWrite::wait_for_quiet(
            "/clear".to_string(),
            true,
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(120),
        ));
        ts.status = SessionStatus::Idle;
        // Refresh the pending-jsonl baseline to the current files so the new
        // file created by /clear shows up as new, and clear session_id so the
        // detection poll rebinds to it. Without this the detector treats the
        // pre-/clear file as still bound.
        ts.pending_jsonl_files = match (ts.session_type.as_str(), wt.as_deref()) {
            ("claude", Some(wt)) => Some(Self::list_jsonl_files(wt)),
            ("codex", Some(wt)) => Some(Self::list_codex_sessions(wt)),
            _ => None,
        };
        ts.session_id = None;
        ts.pending_prompt = None;
        // Old file's turn counts no longer apply to the new file — reset the
        // role's message baseline so templates slice from 0 post-/clear.
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            run.role_baselines.insert(
                role_label.clone(),
                workflow::run::MessageBaseline::default(),
            );
            if let Some(b) = run.role_sessions.get_mut(&role_label) {
                b.current_session_id = None;
            }
        }
        self.save_session_manifest();
        log_tick(
            run_id,
            &format!(
                "reset_fresh: queued /clear for '{}' (fires on first quiet PTY)",
                label
            ),
        );
        true
    }

    fn finish_run(&mut self, run_id: &str, reason: String) {
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            run.mark_done(reason.clone());
            let _ = workflow::run::save(run);
        }
        self.set_status_msg(&format!("Workflow done: {}", reason));
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

    /// Stop the workflow the focused session belongs to.
    ///
    /// The workflow run is marked detached (no more transitions will fire) and
    /// the participating sessions have their workflow tags cleared so they
    /// behave like normal standalone sessions from here on. The sessions
    /// themselves stay open and their transcripts are preserved.
    fn stop_workflow_for_cursor(&mut self) {
        let run_id = match self.focused_session_run_id() {
            Some(id) => id,
            None => {
                self.set_status_msg("Focused session is not in a workflow");
                return;
            }
        };
        if let Some(run) = self.workflow_runs.iter_mut().find(|r| r.run_id == run_id) {
            run.mark_detached();
            let _ = workflow::run::save(run);
        }
        // Clear workflow tags from participating sessions so they behave like
        // normal standalone sessions. Also un-hide their per-session indicators
        // (hidden on launch since the workflow header carries the aggregate).
        for ws in &mut self.workspaces {
            for ts in &mut ws.sessions {
                if ts.workflow_run_id.as_deref() == Some(&run_id) {
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
        let (wi, si) = match self.cursor.clone() {
            Cursor::Session(wi, si) => (wi, si),
            _ => return None,
        };
        self.workspaces
            .get(wi)
            .and_then(|w| w.sessions.get(si))
            .and_then(|s| s.workflow_run_id.clone())
    }

    /// Role name for a session, if it's a workflow participant.
    #[allow(dead_code)]
    pub fn session_workflow_role(&self, wi: usize, si: usize) -> Option<&str> {
        self.workspaces
            .get(wi)?
            .sessions
            .get(si)?
            .workflow_role
            .as_deref()
    }

    /// Run associated with a workspace, if any is active.
    #[allow(dead_code)]
    pub fn workspace_workflow_run(&self, wi: usize) -> Option<&WorkflowRun> {
        let key = Self::workspace_key(self.workspaces.get(wi)?);
        self.workflow_runs
            .iter()
            .find(|r| r.task_key == key && r.is_active())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//                         Workflow modal rendering
// ═══════════════════════════════════════════════════════════════════════════

impl App {
    pub fn draw_workflow_launch(
        &self,
        frame: &mut Frame,
        area: Rect,
        ws_index: usize,
        workflow_name: &str,
        slots: &[WorkflowSlotChoice],
        active_slot: usize,
    ) {
        let width = area.width.min(72).max(44);
        let height = (slots.len() as u16 + 8).min(area.height.saturating_sub(2));
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
        lines.push(Line::from(Span::styled(
            "\u{2191}\u{2193} slot   \u{2190}\u{2192} choice   Enter: launch   Esc: cancel",
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
    let mode = *session.term.lock().mode();
    if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        // Kitty: Enter = CSI 13 u
        b"\x1b[13u"
    } else {
        b"\r"
    }
}

/// Append a diagnostic line for a workflow run to its `tick.log`.
///
/// Lives in `~/.cm/workflow-runs/<run_id>/tick.log`. Rate-limited to at most
/// one distinct message per run per second to avoid spamming the file on every
/// tick of the main loop. Best-effort — ignores all I/O errors.
fn log_tick(run_id: &str, msg: &str) {
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
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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

