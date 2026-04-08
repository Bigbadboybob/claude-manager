use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::term::TermMode;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::api::Task;
use crate::backend::{BackendEvent, BackendHandle};
use crate::config::Config;
use crate::input;
use crate::session::Session;
use crate::terminal_widget::TerminalWidget;
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
}

/// Interval between filesystem checks for session_id detection.
const SESSION_ID_CHECK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct ManifestEntry {
    label: String,
    session_type: String,
    session_id: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
struct Manifest {
    #[serde(default)]
    sessions: HashMap<String, Vec<ManifestEntry>>,
    #[serde(default)]
    view: Option<String>,
}

pub struct TaskEntry {
    pub task_id: Option<String>,
    pub name: String,
    pub api_status: TaskStatus,
    pub repo_url: Option<String>,
    pub prompt: Option<String>,
    pub wip_branch: Option<String>,
    pub session_id: Option<String>,
    pub blocked_at: Option<String>,
    pub worker_vm: Option<String>,
    pub worker_zone: Option<String>,
    pub worktree_path: Option<PathBuf>,
    pub main_repo_path: Option<PathBuf>,
    pub sessions: Vec<TerminalSession>,
}

impl TaskEntry {
    pub fn status(&self) -> TaskStatus {
        if self.sessions.iter().any(|s| s.status == SessionStatus::Running) {
            TaskStatus::Running
        } else if self.sessions.iter().any(|s| s.status == SessionStatus::Idle) {
            TaskStatus::Blocked
        } else if self.worker_vm.is_some() {
            self.api_status.clone()
        } else {
            // No local sessions and no cloud worker — show as waiting.
            TaskStatus::Blocked
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cursor {
    Task(usize),
    Session(usize, usize),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SidebarView {
    Status,
    Task,
}

#[derive(Clone, Debug)]
enum VisualItem {
    TaskHeader(usize),
    Session(usize, usize),
    Separator,
}

/// Modal input state.
enum InputMode {
    /// Normal operation — keys go to terminal or app navigation.
    Normal,
    /// Typing a name/label for a new local session.
    NewSession {
        label_text: String,
        branch_text: String,
        repo_url: String,
        /// 0 = name field, 1 = branch field
        active_field: u8,
    },
    /// Picking a session type to add to a task.
    NewTerminalSession {
        task_index: usize,
        session_type: String,
    },
    /// Renaming the focused session's label.
    RenameSession {
        task_index: usize,
        session_index: usize,
        text: String,
    },
}

pub struct App {
    pub tasks: Vec<TaskEntry>,
    pub cursor: Cursor,
    pub sidebar_view: SidebarView,
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
}

impl App {
    pub fn new(config: Config) -> Self {
        let backend = BackendHandle::spawn(&config);
        let manifest = Self::load_manifest();
        let sidebar_view = match manifest.view.as_deref() {
            Some("task") => SidebarView::Task,
            _ => SidebarView::Status,
        };
        App {
            tasks: Vec::new(),
            cursor: Cursor::Task(0),
            sidebar_view,
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

    /// Path to the session manifest file.
    fn manifest_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".cm/tui-sessions.json")
    }

    /// Save session manifest to disk.
    fn save_session_manifest(&self) {
        let mut sessions: HashMap<String, Vec<ManifestEntry>> = HashMap::new();
        for task in &self.tasks {
            let key = match &task.worktree_path {
                Some(p) => match p.to_str() {
                    Some(s) => s.to_string(),
                    None => continue,
                },
                None => continue,
            };
            if task.sessions.is_empty() {
                continue;
            }
            let entries: Vec<ManifestEntry> = task
                .sessions
                .iter()
                .map(|ts| {
                    ManifestEntry {
                        label: ts.label.clone(),
                        session_type: ts.session_type.clone(),
                        session_id: ts.session_id.clone(),
                    }
                })
                .collect();
            sessions.insert(key, entries);
        }

        let view = match self.sidebar_view {
            SidebarView::Status => "status",
            SidebarView::Task => "task",
        };
        let manifest = Manifest {
            sessions,
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

    /// Restore sessions from the manifest after tasks are populated.
    fn restore_sessions(&mut self) {
        let manifest = Self::load_manifest();
        if manifest.sessions.is_empty() {
            return;
        }

        let (cols, rows) = self.last_term_size;

        for task in &mut self.tasks {
            // Skip tasks that already have sessions.
            if !task.sessions.is_empty() {
                continue;
            }
            let wt = match &task.worktree_path {
                Some(p) => p.clone(),
                None => continue,
            };
            let key = match wt.to_str() {
                Some(s) => s.to_string(),
                None => continue,
            };
            let entries = match manifest.sessions.get(&key) {
                Some(e) => e.clone(),
                None => continue,
            };

            for entry in entries {
                let result = match entry.session_type.as_str() {
                    "claude" => {
                        let mut args = vec!["--dangerously-skip-permissions".to_string()];
                        if let Some(ref sid) = entry.session_id {
                            args.push("--resume".to_string());
                            args.push(sid.clone());
                        }
                        Session::new(
                            "claude",
                            &args,
                            cols,
                            rows,
                            Some(wt.clone()),
                            Default::default(),
                        )
                    }
                    _ => Session::new(
                        "/bin/bash",
                        &[],
                        cols,
                        rows,
                        Some(wt.clone()),
                        Default::default(),
                    ),
                };

                if let Ok(s) = result {
                    let ts = TerminalSession {
                        label: entry.label.clone(),
                        session_type: entry.session_type.clone(),
                        session: s,
                        status: SessionStatus::Running,
                        last_write_at: None,
                        session_id: entry.session_id.clone(),
                        pending_jsonl_files: None,
                    };
                    task.sessions.push(ts);
                }
            }
        }

        // If we restored sessions, put cursor on the first session found.
        for (ti, task) in self.tasks.iter().enumerate() {
            if !task.sessions.is_empty() {
                self.cursor = Cursor::Session(ti, 0);
                break;
            }
        }

        // Clear the manifest file since we've restored.
        // It will be re-saved on quit or changes.
    }

    /// Enter rename mode for the focused session.
    fn start_rename_session(&mut self) {
        match self.cursor {
            Cursor::Session(ti, si) => {
                if let Some(task) = self.tasks.get(ti) {
                    if let Some(ts) = task.sessions.get(si) {
                        self.input_mode = InputMode::RenameSession {
                            task_index: ti,
                            session_index: si,
                            text: ts.label.clone(),
                        };
                    }
                }
            }
            Cursor::Task(ti) => {
                // If exactly one session, rename it.
                if let Some(task) = self.tasks.get(ti) {
                    if task.sessions.len() == 1 {
                        self.input_mode = InputMode::RenameSession {
                            task_index: ti,
                            session_index: 0,
                            text: task.sessions[0].label.clone(),
                        };
                    }
                }
            }
        }
    }

    // ── Cursor helpers ──────────────────────────────────────────────

    /// Return the task index the cursor is currently on.
    fn active_task_index(&self) -> Option<usize> {
        if self.tasks.is_empty() {
            return None;
        }
        match self.cursor {
            Cursor::Task(ti) => {
                if ti < self.tasks.len() {
                    Some(ti)
                } else {
                    None
                }
            }
            Cursor::Session(ti, _) => {
                if ti < self.tasks.len() {
                    Some(ti)
                } else {
                    None
                }
            }
        }
    }

    /// Return a reference to the active terminal session (task + session).
    fn active_session(&self) -> Option<(&TaskEntry, &TerminalSession)> {
        match self.cursor {
            Cursor::Session(ti, si) => {
                let task = self.tasks.get(ti)?;
                let ts = task.sessions.get(si)?;
                Some((task, ts))
            }
            Cursor::Task(ti) => {
                // If the task has exactly one session, return it.
                let task = self.tasks.get(ti)?;
                if task.sessions.len() == 1 {
                    Some((task, &task.sessions[0]))
                } else {
                    None
                }
            }
        }
    }

    /// Return a mutable reference to the active terminal session.
    fn active_session_mut(&mut self) -> Option<&mut TerminalSession> {
        match self.cursor {
            Cursor::Session(ti, si) => {
                let task = self.tasks.get_mut(ti)?;
                task.sessions.get_mut(si)
            }
            Cursor::Task(ti) => {
                let task = self.tasks.get_mut(ti)?;
                if task.sessions.len() == 1 {
                    Some(&mut task.sessions[0])
                } else {
                    None
                }
            }
        }
    }

    /// Clamp cursor so it points to a valid item.
    fn clamp_cursor(&mut self) {
        if self.tasks.is_empty() {
            self.cursor = Cursor::Task(0);
            return;
        }
        match self.cursor {
            Cursor::Task(ti) => {
                if ti >= self.tasks.len() {
                    self.cursor = Cursor::Task(self.tasks.len() - 1);
                }
            }
            Cursor::Session(ti, si) => {
                if ti >= self.tasks.len() {
                    self.cursor = Cursor::Task(self.tasks.len() - 1);
                } else if self.tasks[ti].sessions.is_empty() {
                    self.cursor = Cursor::Task(ti);
                } else if si >= self.tasks[ti].sessions.len() {
                    self.cursor =
                        Cursor::Session(ti, self.tasks[ti].sessions.len() - 1);
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
    /// Running sessions first, then idle, then tasks with no sessions.
    fn visual_items_status(&self) -> Vec<VisualItem> {
        let mut running: Vec<VisualItem> = Vec::new();
        let mut idle: Vec<VisualItem> = Vec::new();
        let mut no_session: Vec<VisualItem> = Vec::new();

        for (ti, task) in self.tasks.iter().enumerate() {
            if task.sessions.is_empty() {
                no_session.push(VisualItem::TaskHeader(ti));
            } else {
                for (si, ts) in task.sessions.iter().enumerate() {
                    let item = VisualItem::Session(ti, si);
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

    /// Task view: tasks as headers with sessions indented underneath.
    /// Within a task, running sessions first.
    fn visual_items_task(&self) -> Vec<VisualItem> {
        let mut items = Vec::new();
        for (ti, task) in self.tasks.iter().enumerate() {
            if ti > 0 {
                items.push(VisualItem::Separator);
            }
            items.push(VisualItem::TaskHeader(ti));
            // Running sessions first within the task.
            let mut running_indices: Vec<usize> = Vec::new();
            let mut other_indices: Vec<usize> = Vec::new();
            for (si, ts) in task.sessions.iter().enumerate() {
                if ts.status == SessionStatus::Running {
                    running_indices.push(si);
                } else {
                    other_indices.push(si);
                }
            }
            for si in running_indices {
                items.push(VisualItem::Session(ti, si));
            }
            for si in other_indices {
                items.push(VisualItem::Session(ti, si));
            }
        }
        items
    }

    /// Navigate the cursor up or down. +1 = down, -1 = up.
    /// Skips non-selectable items (Separators, TaskHeaders in task view).
    fn navigate(&mut self, direction: i32) {
        let items = self.visual_items();
        if items.is_empty() {
            return;
        }

        // TaskHeaders are selectable only if the task has no sessions (so you can still interact).
        let is_selectable = |item: &VisualItem| match item {
            VisualItem::Session(_, _) => true,
            VisualItem::TaskHeader(ti) => self.tasks.get(*ti).map_or(false, |t| t.sessions.is_empty()),
            VisualItem::Separator => false,
        };

        // If nothing is selectable, bail.
        if !items.iter().any(is_selectable) {
            return;
        }

        // Find current position in visual items.
        let cur_pos = items
            .iter()
            .position(|item| match (&self.cursor, item) {
                (Cursor::Task(ti), VisualItem::TaskHeader(vti)) => ti == vti,
                (Cursor::Session(ti, si), VisualItem::Session(vti, vsi)) => {
                    ti == vti && si == vsi
                }
                _ => false,
            })
            .unwrap_or(0);

        // Move in the given direction, skipping non-selectable items.
        let len = items.len() as i32;
        let mut next = cur_pos as i32;
        for _ in 0..items.len() {
            next = (next + direction).rem_euclid(len);
            if is_selectable(&items[next as usize]) {
                break;
            }
        }

        match &items[next as usize] {
            VisualItem::Session(ti, si) => self.cursor = Cursor::Session(*ti, *si),
            VisualItem::TaskHeader(ti) => self.cursor = Cursor::Task(*ti),
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
        for task in &mut self.tasks {
            let worktree_path = task.worktree_path.clone();
            for ts in &mut task.sessions {
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
                        _ => {}
                    }
                }

                // Prune old wakeups outside the rolling window.
                ts.session
                    .wakeup_times
                    .retain(|t| now.duration_since(*t) < WAKEUP_WINDOW);

                // Detect idle/active for sessions with a local terminal.
                // Freeze while user is typing to avoid flicker from echo.
                if !ts.session.exited {
                    let user_typing = ts
                        .last_write_at
                        .map_or(false, |t| now.duration_since(t) < WAKEUP_WINDOW);
                    if !user_typing {
                        let burst =
                            ts.session.wakeup_times.len() >= WAKEUP_BURST_THRESHOLD;
                        let quiet = ts.session.wakeup_times.is_empty();
                        if quiet && ts.status == SessionStatus::Running {
                            ts.status = SessionStatus::Idle;
                        } else if burst && ts.status != SessionStatus::Running {
                            ts.status = SessionStatus::Running;
                        }
                    }
                }

                // Detect session_id for claude sessions that don't have one yet.
                if should_check_session_ids
                    && ts.session_id.is_none()
                    && ts.pending_jsonl_files.is_some()
                {
                    if let Some(ref wt) = worktree_path {
                        let existing = ts.pending_jsonl_files.as_ref().unwrap();
                        if let Some(sid) = Self::detect_session_id(wt, existing) {
                            ts.session_id = Some(sid);
                            ts.pending_jsonl_files = None;
                        }
                    }
                }
            }
        }

        if should_check_session_ids {
            self.last_session_id_check = now;
        }
        if had_event {
            self.needs_redraw = true;
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
                BackendEvent::TaskCreated { name, task_id } => {
                    // Find the local entry by name and assign the DB task_id.
                    if let Some(task) = self
                        .tasks
                        .iter_mut()
                        .find(|t| t.task_id.is_none() && t.name == name)
                    {
                        task.task_id = Some(task_id);
                    }
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
            }
        }
    }

    /// Reconcile API tasks with local task entries.
    fn reconcile_tasks(&mut self, tasks: Vec<Task>) {
        // Save cursor context for restoration.
        let saved_task_id = match &self.cursor {
            Cursor::Task(ti) => self.tasks.get(*ti).and_then(|t| t.task_id.clone()),
            Cursor::Session(ti, _) => self.tasks.get(*ti).and_then(|t| t.task_id.clone()),
        };
        let saved_session_label = match &self.cursor {
            Cursor::Session(ti, si) => self
                .tasks
                .get(*ti)
                .and_then(|t| t.sessions.get(*si))
                .map(|s| s.label.clone()),
            _ => None,
        };

        let mut seen_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for task in &tasks {
            if task.status == "done" {
                continue;
            }
            seen_ids.insert(task.id.clone());

            // Display name: prefer name field, fall back to prompt, then id prefix.
            let display_name = task
                .name
                .as_deref()
                .or(task.prompt.as_deref())
                .unwrap_or(&task.id[..8.min(task.id.len())])
                .chars()
                .take(60)
                .collect::<String>();

            // Detect local worktree path for tasks without a VM.
            let is_local = task.worker_vm.is_none()
                && task
                    .wip_branch
                    .as_ref()
                    .map_or(false, |b| b.starts_with("cm/"));

            if let Some(entry) = self
                .tasks
                .iter_mut()
                .find(|e| e.task_id.as_deref() == Some(&task.id))
            {
                // Update API fields. DON'T touch sessions.
                entry.name = display_name;
                entry.api_status = TaskStatus::from_api(&task.status);
                entry.worker_vm = task.worker_vm.clone();
                entry.worker_zone = task.worker_zone.clone();
                entry.repo_url = Some(task.repo_url.clone());
                entry.prompt = task.prompt.clone();
                entry.wip_branch = task.wip_branch.clone();
                entry.session_id = task.session_id.clone();
                entry.blocked_at = task.blocked_at.clone();
            } else {
                // For local tasks, try to find the worktree on disk.
                let worktree_path = if is_local {
                    if let Some(ref branch) = task.wip_branch {
                        let slug = branch.strip_prefix("cm/").unwrap_or(branch);
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
                        if path.exists() {
                            Some(path)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let main_repo_path = if worktree_path.is_some() {
                    worktree::find_local_repo(&task.repo_url)
                } else {
                    None
                };

                self.tasks.push(TaskEntry {
                    task_id: Some(task.id.clone()),
                    name: display_name,
                    api_status: TaskStatus::from_api(&task.status),
                    repo_url: Some(task.repo_url.clone()),
                    prompt: task.prompt.clone(),
                    wip_branch: task.wip_branch.clone(),
                    session_id: task.session_id.clone(),
                    blocked_at: task.blocked_at.clone(),
                    worker_vm: task.worker_vm.clone(),
                    worker_zone: task.worker_zone.clone(),
                    worktree_path,
                    main_repo_path,
                    sessions: vec![],
                });
            }
        }

        // Retain: keep tasks in seen_ids OR that have sessions.
        self.tasks.retain(|t| {
            if t.api_status == TaskStatus::Done && t.sessions.is_empty() {
                return false;
            }
            match &t.task_id {
                Some(id) => seen_ids.contains(id) || !t.sessions.is_empty(),
                None => true,
            }
        });

        // Sort tasks: Running first, then Blocked, Backlog, Done.
        self.tasks.sort_by(|a, b| {
            fn status_rank(s: &TaskStatus) -> u8 {
                match s {
                    TaskStatus::Running => 0,
                    TaskStatus::Blocked => 1,
                    TaskStatus::Backlog => 2,
                    TaskStatus::Done => 3,
                }
            }
            status_rank(&a.status()).cmp(&status_rank(&b.status()))
        });

        // Restore cursor.
        if let Some(ref id) = saved_task_id {
            if let Some(ti) = self
                .tasks
                .iter()
                .position(|t| t.task_id.as_deref() == Some(id))
            {
                if let Some(ref label) = saved_session_label {
                    if let Some(si) = self.tasks[ti]
                        .sessions
                        .iter()
                        .position(|s| &s.label == label)
                    {
                        self.cursor = Cursor::Session(ti, si);
                    } else {
                        self.cursor = Cursor::Task(ti);
                    }
                } else {
                    self.cursor = Cursor::Task(ti);
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
                        self.start_rename_session();
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

    /// Handle events while in input mode.
    fn handle_input_event(&mut self, event: &CrosstermEvent) -> bool {
        if let CrosstermEvent::Key(key) = event {
            match &mut self.input_mode {
                InputMode::Normal => return false,
                InputMode::NewSession {
                    label_text,
                    branch_text,
                    repo_url,
                    active_field,
                } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Tab | KeyCode::BackTab => {
                        *active_field = if *active_field == 0 { 1 } else { 0 };
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
                            self.input_mode = InputMode::Normal;
                            self.create_local_session(
                                &repo,
                                &label,
                                branch.as_deref(),
                            );
                        }
                        return true;
                    }
                    KeyCode::Backspace => {
                        if *active_field == 0 {
                            label_text.pop();
                        } else {
                            branch_text.pop();
                        }
                        return true;
                    }
                    KeyCode::Char(c) => {
                        if *active_field == 0 {
                            label_text.push(c);
                        } else {
                            branch_text.push(c);
                        }
                        return true;
                    }
                    _ => return true,
                },
                InputMode::NewTerminalSession {
                    task_index,
                    session_type,
                } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Char('j') | KeyCode::Char('k') | KeyCode::Tab | KeyCode::BackTab => {
                        *session_type = if session_type == "claude" {
                            "bash".to_string()
                        } else {
                            "claude".to_string()
                        };
                        return true;
                    }
                    KeyCode::Enter => {
                        let ti = *task_index;
                        let st = session_type.clone();
                        self.input_mode = InputMode::Normal;
                        self.spawn_session_on_task(ti, &st);
                        return true;
                    }
                    _ => return true,
                },
                InputMode::RenameSession {
                    task_index,
                    session_index,
                    text,
                } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Enter => {
                        let ti = *task_index;
                        let si = *session_index;
                        let new_label = text.clone();
                        self.input_mode = InputMode::Normal;
                        if !new_label.trim().is_empty() {
                            if let Some(task) = self.tasks.get_mut(ti) {
                                if let Some(ts) = task.sessions.get_mut(si) {
                                    ts.label = new_label;
                                }
                            }
                            self.save_session_manifest();
                            self.set_status_msg("Session renamed");
                        }
                        return true;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                        return true;
                    }
                    KeyCode::Char(c) => {
                        text.push(c);
                        return true;
                    }
                    _ => return true,
                },
            }
        }
        true
    }

    // ── Session management ──────────────────────────────────────────

    /// Enter input mode to create a new local session.
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
            repo_url,
            active_field: 0,
        };
    }

    /// Enter input mode to add a terminal session to the active task.
    fn start_new_terminal_session(&mut self) {
        let ti = match self.active_task_index() {
            Some(ti) => ti,
            None => {
                self.set_status_msg("No task selected");
                return;
            }
        };
        self.input_mode = InputMode::NewTerminalSession {
            task_index: ti,
            session_type: "claude".to_string(),
        };
    }

    /// Close the current session (remove from task.sessions).
    fn close_active_session(&mut self) {
        match self.cursor.clone() {
            Cursor::Session(ti, si) => {
                if let Some(task) = self.tasks.get_mut(ti) {
                    if si < task.sessions.len() {
                        task.sessions.remove(si);
                        // Move cursor to task header or previous session.
                        if task.sessions.is_empty() {
                            self.cursor = Cursor::Task(ti);
                        } else {
                            let new_si = si.min(task.sessions.len() - 1);
                            self.cursor = Cursor::Session(ti, new_si);
                        }
                        self.save_session_manifest();
                        self.set_status_msg("Session closed");
                    }
                }
            }
            Cursor::Task(ti) => {
                // If task has one session, close it.
                if let Some(task) = self.tasks.get_mut(ti) {
                    if task.sessions.len() == 1 {
                        task.sessions.remove(0);
                        self.cursor = Cursor::Task(ti);
                        self.save_session_manifest();
                        self.set_status_msg("Session closed");
                    }
                }
            }
        }
    }

    /// Create a local Claude session in a worktree (new task).
    fn create_local_session(
        &mut self,
        repo_url: &str,
        label: &str,
        start_branch: Option<&str>,
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

        let worktree_path =
            match worktree::create_worktree(&main_repo, &slug, start_branch) {
                Ok(p) => p,
                Err(e) => {
                    self.set_status_msg(&format!("Worktree: {}", e));
                    return;
                }
            };

        worktree::setup_worktree(&main_repo, &worktree_path);

        // Launch Claude Code fresh (no --resume for new tasks).
        let (cols, rows) = self.last_term_size;
        let args = vec!["--dangerously-skip-permissions".to_string()];
        let pending = Self::list_jsonl_files(&worktree_path);

        let session = Session::new(
            "claude",
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        );

        let branch = format!("cm/{}", slug);

        match session {
            Ok(s) => {
                let ts = TerminalSession {
                    label: "claude".to_string(),
                    session_type: "claude".to_string(),
                    session: s,
                    status: SessionStatus::Running,
                    last_write_at: None,
                    session_id: None,
                    pending_jsonl_files: Some(pending),
                };
                let new_ti = self.tasks.len();
                self.tasks.push(TaskEntry {
                    task_id: None,
                    name: label.to_string(),
                    api_status: TaskStatus::Running,
                    repo_url: Some(repo_url.to_string()),
                    prompt: None,
                    wip_branch: Some(branch.clone()),
                    session_id: None,
                    blocked_at: None,
                    worker_vm: None,
                    worker_zone: None,
                    worktree_path: Some(worktree_path),
                    main_repo_path: Some(main_repo),
                    sessions: vec![ts],
                });
                self.cursor = Cursor::Session(new_ti, 0);

                // Create task in DB (async, background).
                self.backend.create_task(
                    label.to_string(),
                    repo_url.to_string(),
                    branch,
                );
                self.save_session_manifest();
                self.set_status_msg("Local session started");
            }
            Err(e) => {
                self.set_status_msg(&format!("Spawn: {}", e));
            }
        }
    }

    /// Attach to the active task (SSH for cloud, claude for local worktree, bash fallback).
    fn attach_active(&mut self) {
        let ti = match self.active_task_index() {
            Some(ti) => ti,
            None => return,
        };

        let (cols, rows) = self.last_term_size;

        // If task already has sessions, just report.
        if !self.tasks[ti].sessions.is_empty() {
            self.set_status_msg("Task already has sessions");
            return;
        }

        if let Some(vm) = self.tasks[ti].worker_vm.clone() {
            // Cloud: SSH into the worker VM.
            let zone = self.tasks[ti]
                .worker_zone
                .clone()
                .unwrap_or_else(|| self.config.gcp_zone.clone());
            let args = vec![
                "compute".to_string(),
                "ssh".to_string(),
                vm.clone(),
                format!("--zone={}", zone),
                format!("--project={}", self.config.gcp_project),
                "--".to_string(),
                "-t".to_string(),
                "TERM=xterm-256color sudo su - worker -c 'tmux attach -t claude'"
                    .to_string(),
            ];
            if let Ok(s) =
                Session::new("gcloud", &args, cols, rows, None, Default::default())
            {
                let ts = TerminalSession {
                    label: "ssh".to_string(),
                    session_type: "bash".to_string(),
                    session: s,
                    status: SessionStatus::Running,
                    last_write_at: None,
                    session_id: None,
                    pending_jsonl_files: None,
                };
                let si = self.tasks[ti].sessions.len();
                self.tasks[ti].sessions.push(ts);
                self.cursor = Cursor::Session(ti, si);
            }
        } else if let Some(wt) = self.tasks[ti].worktree_path.clone() {
            // Local worktree: launch Claude Code fresh.
            let args = vec!["--dangerously-skip-permissions".to_string()];
            let pending = Self::list_jsonl_files(&wt);
            if let Ok(s) = Session::new(
                "claude",
                &args,
                cols,
                rows,
                Some(wt.clone()),
                Default::default(),
            ) {
                let ts = TerminalSession {
                    label: "claude".to_string(),
                    session_type: "claude".to_string(),
                    session: s,
                    status: SessionStatus::Running,
                    last_write_at: None,
                    session_id: None,
                    pending_jsonl_files: Some(pending),
                };
                let si = self.tasks[ti].sessions.len();
                self.tasks[ti].sessions.push(ts);
                self.cursor = Cursor::Session(ti, si);
            }
        } else {
            // Fallback: bash shell.
            if let Ok(s) =
                Session::new("/bin/bash", &[], cols, rows, None, Default::default())
            {
                let ts = TerminalSession {
                    label: "bash".to_string(),
                    session_type: "bash".to_string(),
                    session: s,
                    status: SessionStatus::Running,
                    last_write_at: None,
                    session_id: None,
                    pending_jsonl_files: None,
                };
                let si = self.tasks[ti].sessions.len();
                self.tasks[ti].sessions.push(ts);
                self.cursor = Cursor::Session(ti, si);
            }
        }
    }

    /// Spawn a session on an existing task by type ("claude" or "bash").
    fn spawn_session_on_task(&mut self, task_index: usize, session_type: &str) {
        if task_index >= self.tasks.len() {
            return;
        }

        let (cols, rows) = self.last_term_size;
        let wt = self.tasks[task_index].worktree_path.clone();

        // Record existing .jsonl files before spawning claude (for session_id detection).
        let pending = if session_type == "claude" {
            wt.as_ref().map(|p| Self::list_jsonl_files(p))
        } else {
            None
        };

        let result = match session_type {
            "claude" => {
                // Always start fresh — no --resume.
                let args = vec!["--dangerously-skip-permissions".to_string()];
                Session::new("claude", &args, cols, rows, wt, Default::default())
            }
            _ => Session::new("/bin/bash", &[], cols, rows, wt, Default::default()),
        };

        match result {
            Ok(s) => {
                let ts = TerminalSession {
                    label: session_type.to_string(),
                    session_type: session_type.to_string(),
                    session: s,
                    status: SessionStatus::Running,
                    last_write_at: None,
                    session_id: None,
                    pending_jsonl_files: pending,
                };
                let si = self.tasks[task_index].sessions.len();
                self.tasks[task_index].sessions.push(ts);
                self.cursor = Cursor::Session(task_index, si);
                self.save_session_manifest();
                self.set_status_msg(&format!("Started {} session", session_type));
            }
            Err(e) => {
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
                let ts = TerminalSession {
                    label: "claude".to_string(),
                    session_type: "claude".to_string(),
                    session: s,
                    status: SessionStatus::Running,
                    last_write_at: None,
                    session_id: Some(session_id.clone()),
                    pending_jsonl_files: None,
                };

                // Find existing task by task_id or create new.
                let ti = if let Some(ref id) = task_id {
                    self.tasks
                        .iter()
                        .position(|t| t.task_id.as_deref() == Some(id))
                } else {
                    None
                };

                if let Some(ti) = ti {
                    let si = self.tasks[ti].sessions.len();
                    self.tasks[ti].sessions.push(ts);
                    self.tasks[ti].worktree_path = Some(worktree_path);
                    self.tasks[ti].main_repo_path = Some(main_repo);
                    self.cursor = Cursor::Session(ti, si);
                } else {
                    let display_name: String =
                        prompt.chars().take(60).collect();
                    let new_ti = self.tasks.len();
                    self.tasks.push(TaskEntry {
                        task_id,
                        name: display_name,
                        api_status: TaskStatus::Running,
                        repo_url: Some(repo_url),
                        prompt: Some(prompt),
                        wip_branch: None,
                        session_id: Some(session_id),
                        blocked_at: None,
                        worker_vm: None,
                        worker_zone: None,
                        worktree_path: Some(worktree_path),
                        main_repo_path: Some(main_repo),
                        sessions: vec![ts],
                    });
                    self.cursor = Cursor::Session(new_ti, 0);
                }
                self.save_session_manifest();
                self.set_status_msg("Resumed locally");
            }
            Err(e) => {
                self.set_status_msg(&format!("Resume failed: {}", e));
            }
        }
    }

    /// Mark the active task as done via the API.
    fn mark_active_done(&mut self) {
        let ti = match self.active_task_index() {
            Some(ti) => ti,
            None => return,
        };

        // Drop all sessions.
        self.tasks[ti].sessions.clear();

        if let Some(ref id) = self.tasks[ti].task_id {
            let mut fields = HashMap::new();
            fields.insert(
                "status".to_string(),
                serde_json::Value::String("done".to_string()),
            );
            self.backend.update_task(id.clone(), fields);
        }
        self.tasks[ti].api_status = TaskStatus::Done;
        self.cursor = Cursor::Task(ti);
        self.clamp_cursor();
        self.set_status_msg("Marked done");
    }

    /// Delete the active task: close sessions, remove worktree + branch, delete from API.
    fn delete_active(&mut self) {
        let ti = match self.active_task_index() {
            Some(ti) => ti,
            None => return,
        };

        let task_id = self.tasks[ti].task_id.clone();
        let worktree_path = self.tasks[ti].worktree_path.clone();
        let main_repo = self.tasks[ti].main_repo_path.clone();
        let wip_branch = self.tasks[ti].wip_branch.clone();

        // Delete worktree.
        if let (Some(ref wt), Some(ref repo)) = (&worktree_path, &main_repo) {
            worktree::remove_worktree(repo, wt);
        }

        // Delete branch locally. Only delete remote if it was pushed.
        if let (Some(ref branch), Some(ref repo)) = (&wip_branch, &main_repo) {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["branch", "-D", branch])
                .output();
            // Only delete remote branch if the task was pushed to cloud.
            if task_id.is_some() {
                let _ = std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(["push", "origin", "--delete", branch])
                    .output();
            }
        }

        // Delete from API.
        if let Some(ref id) = task_id {
            self.backend.delete_task(id.clone());
        }

        // Remove from local list.
        self.tasks.remove(ti);
        if !self.tasks.is_empty() {
            self.cursor = Cursor::Task(ti.min(self.tasks.len() - 1));
        } else {
            self.cursor = Cursor::Task(0);
        }
        self.set_status_msg("Deleted");
    }

    /// Push the active local session to the cloud.
    fn push_active(&mut self) {
        let ti = match self.active_task_index() {
            Some(ti) => ti,
            None => return,
        };

        if self.tasks[ti].worker_vm.is_some() {
            self.set_status_msg("Can only push local sessions");
            return;
        }
        let worktree_path = match &self.tasks[ti].worktree_path {
            Some(p) => p.clone(),
            None => {
                self.set_status_msg("No worktree to push");
                return;
            }
        };
        let repo_url = match &self.tasks[ti].repo_url {
            Some(u) => u.clone(),
            None => {
                self.set_status_msg("No repo URL");
                return;
            }
        };
        let name = self.tasks[ti]
            .prompt
            .clone()
            .unwrap_or_else(|| self.tasks[ti].name.clone());
        let task_id = self.tasks[ti].task_id.clone();

        self.backend.push(worktree_path, repo_url, name);

        // Clear sessions, mark done.
        self.tasks[ti].sessions.clear();
        self.tasks[ti].api_status = TaskStatus::Done;
        if let Some(ref id) = task_id {
            let mut fields = HashMap::new();
            fields.insert(
                "status".to_string(),
                serde_json::Value::String("done".to_string()),
            );
            self.backend.update_task(id.clone(), fields);
        }
        self.cursor = Cursor::Task(ti);
        self.set_status_msg("Pushing to cloud...");
    }

    /// Pull the active cloud task to local.
    fn pull_active(&mut self) {
        let ti = match self.active_task_index() {
            Some(ti) => ti,
            None => return,
        };

        let task_id = match &self.tasks[ti].task_id {
            Some(id) => id.clone(),
            None => {
                self.set_status_msg("Can only pull cloud tasks");
                return;
            }
        };
        let repo_url = match &self.tasks[ti].repo_url {
            Some(u) => u.clone(),
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

    /// Handle terminal resize.
    pub fn resize_terminals(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
        for task in &mut self.tasks {
            for ts in &mut task.sessions {
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

        let cols =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(30)])
                .split(content_area);

        self.draw_terminal(frame, cols[0]);
        self.draw_session_list(frame, cols[1]);
        self.draw_status_bar(frame, bar_area);

        // Draw input overlay if active.
        match &self.input_mode {
            InputMode::NewSession {
                label_text,
                branch_text,
                repo_url,
                active_field,
            } => {
                self.draw_input_dialog(
                    frame,
                    area,
                    label_text,
                    branch_text,
                    repo_url,
                    *active_field,
                );
            }
            InputMode::NewTerminalSession {
                task_index,
                session_type,
            } => {
                self.draw_new_terminal_dialog(
                    frame,
                    area,
                    *task_index,
                    session_type,
                );
            }
            InputMode::RenameSession { text, .. } => {
                self.draw_rename_dialog(frame, area, text);
            }
            InputMode::Normal => {}
        }
    }

    fn draw_input_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        label_text: &str,
        branch_text: &str,
        repo_url: &str,
        active_field: u8,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 9u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " New Local Session ",
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
        let name_cursor = if active_field == 0 { cursor } else { "" };
        let branch_cursor = if active_field == 1 { cursor } else { "" };

        let branch_hint = if branch_text.is_empty() && active_field != 1 {
            "main"
        } else {
            ""
        };

        let lines = vec![
            Line::from(vec![
                Span::styled(
                    "  Repo: ",
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(repo_name, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  Name: ",
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(label_text, Style::default().fg(Color::White)),
                Span::styled(name_cursor, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled(
                    "Branch: ",
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(branch_text, Style::default().fg(Color::White)),
                Span::styled(
                    branch_cursor,
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    branch_hint,
                    Style::default().fg(Color::DarkGray),
                ),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Tab switch field \u{00b7} Enter start \u{00b7} Esc cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_new_terminal_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        task_index: usize,
        session_type: &str,
    ) {
        let width = 50u16.min(area.width.saturating_sub(4));
        let height = 8u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let task_name = self
            .tasks
            .get(task_index)
            .map(|t| t.name.as_str())
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

        let claude_indicator = if session_type == "claude" {
            ">"
        } else {
            " "
        };
        let bash_indicator = if session_type == "bash" { ">" } else { " " };

        let claude_style = if session_type == "claude" {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        let bash_style = if session_type == "bash" {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };

        let max_name = (width as usize).saturating_sub(8);
        let display_name: String = task_name.chars().take(max_name).collect();

        let lines = vec![
            Line::from(vec![
                Span::styled("  Task: ", Style::default().fg(Color::DarkGray)),
                Span::styled(display_name, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    format!("  {} ", claude_indicator),
                    claude_style,
                ),
                Span::styled("claude", claude_style),
            ]),
            Line::from(vec![
                Span::styled(format!("  {} ", bash_indicator), bash_style),
                Span::styled("bash", bash_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "j/k toggle \u{00b7} Enter start \u{00b7} Esc cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_rename_dialog(&self, frame: &mut Frame, area: Rect, text: &str) {
        let width = 50u16.min(area.width.saturating_sub(4));
        let height = 5u16;
        let x = (area.width.saturating_sub(width)) / 2;
        let y = (area.height.saturating_sub(height)) / 2;
        let dialog_area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " Rename Session ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ));

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let cursor = "\u{2588}";
        let lines = vec![
            Line::from(vec![
                Span::styled("  Name: ", Style::default().fg(Color::DarkGray)),
                Span::styled(text, Style::default().fg(Color::White)),
                Span::styled(cursor, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Enter confirm \u{00b7} Esc cancel",
                Style::default().fg(Color::DarkGray),
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
        } else if let Some(ti) = self.active_task_index() {
            let task = &self.tasks[ti];
            let mut lines = vec![];
            if let Some(ref prompt) = task.prompt {
                lines.push(Line::from(Span::styled(
                    prompt.as_str(),
                    Style::default().fg(Color::White),
                )));
                lines.push(Line::from(""));
            }
            if let Some(ref repo) = task.repo_url {
                lines.push(Line::from(Span::styled(
                    format!("Repo: {}", repo),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if let Some(ref vm) = task.worker_vm {
                lines.push(Line::from(Span::styled(
                    format!("VM: {}", vm),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                if task.worker_vm.is_some() {
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
                VisualItem::TaskHeader(ti) => {
                    let task = &self.tasks[*ti];
                    let is_selected = match &self.cursor {
                        Cursor::Task(cti) => cti == ti,
                        _ => false,
                    };

                    let status = task.status();
                    let (indicator, indicator_style) = match status {
                        TaskStatus::Running => {
                            (spinner, Style::default().fg(Color::Green))
                        }
                        TaskStatus::Blocked => {
                            ("\u{25cf}", Style::default().fg(Color::White))
                        }
                        TaskStatus::Backlog => {
                            ("\u{25cb}", Style::default().fg(Color::DarkGray))
                        }
                        TaskStatus::Done => {
                            ("\u{2713}", Style::default().fg(Color::DarkGray))
                        }
                    };

                    let max_name = (inner.width as usize).saturating_sub(4);
                    let name = if task.name.len() > max_name {
                        format!(
                            "{}...",
                            &task.name[..max_name.saturating_sub(3)]
                        )
                    } else {
                        task.name.clone()
                    };

                    let line = Line::from(vec![
                        Span::styled(
                            format!(" {} ", indicator),
                            indicator_style,
                        ),
                        Span::raw(name),
                    ]);

                    let base_style = if is_selected {
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    items.push(ListItem::new(line).style(base_style));
                }
                VisualItem::Session(ti, si) => {
                    let task = &self.tasks[*ti];
                    let ts = &task.sessions[*si];
                    let is_selected = match &self.cursor {
                        Cursor::Session(cti, csi) => cti == ti && csi == si,
                        _ => false,
                    };

                    let (indicator, indicator_style) = match ts.status {
                        SessionStatus::Running => {
                            (spinner, Style::default().fg(Color::Green))
                        }
                        SessionStatus::Idle => {
                            ("\u{25cf}", Style::default().fg(Color::White))
                        }
                    };

                    let display = match self.sidebar_view {
                        SidebarView::Status => {
                            // "taskname / label"
                            let max_name =
                                (inner.width as usize).saturating_sub(8);
                            let full =
                                format!("{} / {}", task.name, ts.label);
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

                    let line = Line::from(vec![
                        Span::styled(
                            format!(" {} ", indicator),
                            indicator_style,
                        ),
                        Span::raw(display),
                    ]);

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
        let help_rows = 7u16;
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

        let help_lines: Vec<(&str, &str)> = vec![
            ("A-j/k  nav", "A-d  done"),
            ("A-a    attach", "A-x  delete"),
            ("A-n    new", "A-p  push"),
            ("A-s    +session", "A-l  pull"),
            ("A-w    close", "A-v  view"),
            ("A-e    rename", "A-r  refresh"),
        ];

        let mut lines = vec![sep];
        for (left, right) in &help_lines {
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
            .filter(|t| matches!(t.status(), TaskStatus::Running))
            .count();
        let blocked = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status(), TaskStatus::Blocked))
            .count();
        let backlog = self
            .tasks
            .iter()
            .filter(|t| matches!(t.status(), TaskStatus::Backlog))
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
        if let Some((task, ts)) = self.active_session() {
            format!(" {} / {} ", task.name, ts.label)
        } else if let Some(ti) = self.active_task_index() {
            format!(" {} ", self.tasks[ti].name)
        } else {
            " Terminal ".to_string()
        }
    }
}
