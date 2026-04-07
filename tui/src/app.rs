use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event as TermEvent;
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

/// Modal input state for creating a new local session.
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
}

/// An entry in the session list.
pub struct SessionEntry {
    pub task_id: Option<String>,
    pub name: String,
    pub status: TaskStatus,
    pub session: Option<Session>,
    // Cloud task metadata.
    pub worker_vm: Option<String>,
    pub worker_zone: Option<String>,
    pub repo_url: Option<String>,
    pub prompt: Option<String>,
    pub wip_branch: Option<String>,
    pub session_id: Option<String>,
    pub blocked_at: Option<String>,
    // Local session metadata.
    pub worktree_path: Option<PathBuf>,
    pub main_repo_path: Option<PathBuf>,
    /// Last time user input was written to this session's PTY.
    pub last_write_at: Option<Instant>,
}

pub struct App {
    pub entries: Vec<SessionEntry>,
    pub active: usize,
    pub should_quit: bool,
    pub last_term_size: (u16, u16),
    pub config: Config,
    pub backend: BackendHandle,
    pub connected: bool,
    pub status_msg: Option<(String, Instant)>,
    input_mode: InputMode,
    start_time: Instant,
}

impl App {
    pub fn new(config: Config) -> Self {
        let backend = BackendHandle::spawn(&config);
        App {
            entries: Vec::new(),
            active: 0,
            should_quit: false,
            last_term_size: (80, 24),
            config,
            backend,
            connected: false,
            status_msg: None,
            input_mode: InputMode::Normal,
            start_time: Instant::now(),
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

    /// Visual display order: Running entries first, then the rest.
    /// Returns a vec of indices into self.entries.
    fn visual_order(&self) -> Vec<usize> {
        let mut active: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.status, TaskStatus::Running))
            .map(|(i, _)| i)
            .collect();
        let mut rest: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !matches!(e.status, TaskStatus::Running))
            .map(|(i, _)| i)
            .collect();
        active.append(&mut rest);
        active
    }

    /// Find the most recent Claude session file for a worktree directory.
    /// Returns the session UUID if found.
    fn find_latest_session(worktree_path: &std::path::Path) -> Option<String> {
        let home = dirs::home_dir()?;
        // Claude encodes project paths: '/' and '.' both become '-'.
        let path_str = worktree_path.to_str()?;
        let encoded = path_str.replace('/', "-").replace('.', "-");
        let session_dir = home.join(format!(".claude/projects/{}", encoded));
        if !session_dir.is_dir() {
            return None;
        }
        // Find the most recently modified .jsonl file.
        let mut newest: Option<(std::time::SystemTime, String)> = None;
        for entry in std::fs::read_dir(&session_dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(modified) = meta.modified() {
                        let stem = path.file_stem()?.to_str()?.to_string();
                        if newest.as_ref().map_or(true, |(t, _)| modified > *t) {
                            newest = Some((modified, stem));
                        }
                    }
                }
            }
        }
        newest.map(|(_, id)| id)
    }

    /// Process all pending terminal events (non-blocking).
    pub fn drain_terminal_events(&mut self) {
        let now = Instant::now();

        for entry in &mut self.entries {
            if let Some(ref mut session) = entry.session {
                while let Ok(event) = session.event_rx.try_recv() {
                    match event {
                        TermEvent::Exit | TermEvent::ChildExit(_) => {
                            session.exited = true;
                        }
                        TermEvent::Title(title) => {
                            session.title = title;
                        }
                        TermEvent::Wakeup => {
                            session.wakeup_times.push(now);
                        }
                        _ => {}
                    }
                }

                // Prune old wakeups outside the rolling window.
                session.wakeup_times.retain(|t| now.duration_since(*t) < WAKEUP_WINDOW);

                // Detect idle/active for sessions with a local terminal.
                // Freeze while user is typing to avoid flicker from echo.
                if !session.exited {
                    let user_typing = entry
                        .last_write_at
                        .map_or(false, |t| now.duration_since(t) < WAKEUP_WINDOW);
                    if !user_typing {
                        let burst = session.wakeup_times.len() >= WAKEUP_BURST_THRESHOLD;
                        let quiet = session.wakeup_times.is_empty();
                        if quiet && entry.status == TaskStatus::Running {
                            entry.status = TaskStatus::Blocked;
                        } else if burst && entry.status != TaskStatus::Running {
                            entry.status = TaskStatus::Running;
                        }
                    }
                }
            }
        }
    }

    /// Process all pending backend events (non-blocking).
    pub fn drain_backend_events(&mut self) {
        while let Ok(event) = self.backend.event_rx.try_recv() {
            match event {
                BackendEvent::TasksUpdated(tasks) => {
                    self.reconcile_tasks(tasks);
                }
                BackendEvent::Connected => {
                    self.connected = true;
                    self.set_status_msg("Connected to API");
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
                    if let Some(entry) = self
                        .entries
                        .iter_mut()
                        .find(|e| e.task_id.is_none() && e.name == name)
                    {
                        entry.task_id = Some(task_id);
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

    /// Reconcile API tasks with local session entries.
    fn reconcile_tasks(&mut self, tasks: Vec<Task>) {
        let selected_task_id = self
            .entries
            .get(self.active)
            .and_then(|e| e.task_id.clone());

        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

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
                && task.wip_branch.as_ref().map_or(false, |b| b.starts_with("cm/"));

            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.task_id.as_deref() == Some(&task.id))
            {
                entry.name = display_name;
                // Don't override status for sessions with a local terminal —
                // idle detection is handled locally via PTY output tracking.
                if entry.session.is_none() {
                    // If no local session and no cloud worker, show as waiting.
                    if task.worker_vm.is_none() {
                        entry.status = TaskStatus::Blocked;
                    } else {
                        entry.status = TaskStatus::from_api(&task.status);
                    }
                }
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
                        let repo_name = task.repo_url
                            .trim_end_matches('/')
                            .trim_end_matches(".git")
                            .rsplit('/')
                            .next()
                            .unwrap_or("repo");
                        let path = dirs::home_dir()
                            .unwrap_or_default()
                            .join(format!(".cm/worktrees/{}-{}", repo_name, slug));
                        if path.exists() { Some(path) } else { None }
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

                self.entries.push(SessionEntry {
                    task_id: Some(task.id.clone()),
                    name: display_name,
                    status: TaskStatus::from_api(&task.status),
                    session: None,
                    worker_vm: task.worker_vm.clone(),
                    worker_zone: task.worker_zone.clone(),
                    repo_url: Some(task.repo_url.clone()),
                    prompt: task.prompt.clone(),
                    wip_branch: task.wip_branch.clone(),
                    session_id: task.session_id.clone(),
                    blocked_at: task.blocked_at.clone(),
                    worktree_path,
                    main_repo_path,
                    last_write_at: None,
                });
            }
        }

        self.entries.retain(|e| {
            // Hide all done tasks from the TUI.
            if e.status == TaskStatus::Done {
                return false;
            }
            match &e.task_id {
                Some(id) => seen_ids.contains(id) || e.session.is_some(),
                None => true,
            }
        });

        self.entries.sort_by(|a, b| {
            fn status_rank(s: &TaskStatus) -> u8 {
                match s {
                    TaskStatus::Blocked => 0,
                    TaskStatus::Running => 1,
                    TaskStatus::Backlog => 2,
                    TaskStatus::Done => 3,
                }
            }
            status_rank(&a.status).cmp(&status_rank(&b.status))
        });

        if let Some(ref id) = selected_task_id {
            if let Some(pos) = self
                .entries
                .iter()
                .position(|e| e.task_id.as_deref() == Some(id))
            {
                self.active = pos;
            }
        }
        if !self.entries.is_empty() {
            self.active = self.active.min(self.entries.len() - 1);
        } else {
            self.active = 0;
        }
    }

    fn set_status_msg(&mut self, msg: &str) {
        self.status_msg = Some((msg.to_string(), Instant::now()));
    }

    /// Handle a crossterm event. Returns true if consumed.
    pub fn handle_event(&mut self, event: &CrosstermEvent) -> bool {
        // If in input mode, handle input events.
        if self.is_input_mode() {
            return self.handle_input_event(event);
        }

        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) {
                match key.code {
                    KeyCode::Char('q') => {
                        self.should_quit = true;
                        return true;
                    }
                    KeyCode::Char('j') | KeyCode::Char('k') => {
                        let order = self.visual_order();
                        if !order.is_empty() {
                            let cur_vis = order
                                .iter()
                                .position(|&i| i == self.active)
                                .unwrap_or(0);
                            let next_vis = if key.code == KeyCode::Char('j') {
                                (cur_vis + 1) % order.len()
                            } else {
                                (cur_vis + order.len() - 1) % order.len()
                            };
                            self.active = order[next_vis];
                        }
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
                        if let Some(entry) = self.entries.get(self.active) {
                            if let Some(ref session) = entry.session {
                                use alacritty_terminal::grid::Scroll;
                                session.term.lock().scroll_display(Scroll::Delta(10));
                            }
                        }
                        return true;
                    }
                    KeyCode::PageDown => {
                        if let Some(entry) = self.entries.get(self.active) {
                            if let Some(ref session) = entry.session {
                                use alacritty_terminal::grid::Scroll;
                                session.term.lock().scroll_display(Scroll::Delta(-10));
                            }
                        }
                        return true;
                    }
                    _ => {}
                }
            }
        }

        // Forward to active terminal.
        if let Some(entry) = self.entries.get_mut(self.active) {
            if let Some(ref session) = entry.session {
                if !session.exited {
                    let term_mode = *session.term.lock().mode();
                    if let Some(bytes) = input::event_to_bytes(event, &term_mode) {
                        session.write(bytes);
                        entry.last_write_at = Some(Instant::now());
                    }
                    return true;
                }
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
                            self.create_local_session(&repo, &label, branch.as_deref());
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
            }
        }
        true
    }

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

    /// Create a local Claude session in a worktree.
    fn create_local_session(&mut self, repo_url: &str, label: &str, start_branch: Option<&str>) {
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

        // Launch Claude Code, resuming the latest session if one exists.
        let (cols, rows) = self.last_term_size;
        let mut args = vec!["--dangerously-skip-permissions".to_string()];
        if let Some(sid) = Self::find_latest_session(&worktree_path) {
            args.push("--resume".to_string());
            args.push(sid);
        }

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
                let entry = SessionEntry {
                    task_id: None, // Will be set when TaskCreated event arrives.
                    name: label.to_string(),
                    status: TaskStatus::Running,
                    session: Some(s),
                    worker_vm: None,
                    worker_zone: None,
                    repo_url: Some(repo_url.to_string()),
                    prompt: None,
                    wip_branch: Some(branch.clone()),
                    session_id: None,
                    blocked_at: None,
                    worktree_path: Some(worktree_path),
                    main_repo_path: Some(main_repo),
                    last_write_at: None,
                };
                self.entries.push(entry);
                self.active = self.entries.len() - 1;

                // Create task in DB (async, background).
                self.backend.create_task(
                    label.to_string(),
                    repo_url.to_string(),
                    branch,
                );
                self.set_status_msg("Local session started");
            }
            Err(e) => {
                self.set_status_msg(&format!("Spawn: {}", e));
            }
        }
    }

    /// Attach to the active session (SSH for cloud, bash for local).
    fn attach_active(&mut self) {
        let (cols, rows) = self.last_term_size;
        if let Some(entry) = self.entries.get_mut(self.active) {
            if entry.session.is_some() {
                return;
            }

            if let Some(ref vm) = entry.worker_vm.clone() {
                // Cloud: SSH into the worker VM.
                let zone = entry
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
                entry.session =
                    Session::new("gcloud", &args, cols, rows, None, Default::default()).ok();
            } else if let Some(ref wt) = entry.worktree_path.clone() {
                // Local worktree: launch Claude Code, resuming latest session if exists.
                let mut args = vec!["--dangerously-skip-permissions".to_string()];
                if let Some(sid) = Self::find_latest_session(wt) {
                    args.push("--resume".to_string());
                    args.push(sid);
                }
                entry.session =
                    Session::new("claude", &args, cols, rows, Some(wt.clone()), Default::default())
                        .ok();
                entry.status = TaskStatus::Running;
            } else {
                // Fallback: bash shell.
                entry.session =
                    Session::new("/bin/bash", &[], cols, rows, None, Default::default()).ok();
            }
        }
    }

    /// Mark the active task as done via the API.
    fn mark_active_done(&mut self) {
        if let Some(entry) = self.entries.get_mut(self.active) {
            if let Some(ref id) = entry.task_id {
                let mut fields = HashMap::new();
                fields.insert(
                    "status".to_string(),
                    serde_json::Value::String("done".to_string()),
                );
                self.backend.update_task(id.clone(), fields);
                entry.status = TaskStatus::Done;
                self.set_status_msg("Marked done");
            }
        }
    }

    /// Delete the active task: close session, remove worktree + branch, delete from API.
    fn delete_active(&mut self) {
        if self.active >= self.entries.len() {
            return;
        }
        let entry = &self.entries[self.active];
        let task_id = entry.task_id.clone();
        let worktree_path = entry.worktree_path.clone();
        let main_repo = entry.main_repo_path.clone();
        let wip_branch = entry.wip_branch.clone();

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
        self.entries.remove(self.active);
        if !self.entries.is_empty() {
            self.active = self.active.min(self.entries.len() - 1);
        } else {
            self.active = 0;
        }
        self.set_status_msg("Deleted");
    }

    /// Push the active local session to the cloud.
    fn push_active(&mut self) {
        if let Some(entry) = self.entries.get(self.active) {
            // Only push sessions with a local worktree (not cloud-only tasks).
            if entry.worker_vm.is_some() {
                self.set_status_msg("Can only push local sessions");
                return;
            }
            let worktree_path = match &entry.worktree_path {
                Some(p) => p.clone(),
                None => {
                    self.set_status_msg("No worktree to push");
                    return;
                }
            };
            let repo_url = match &entry.repo_url {
                Some(u) => u.clone(),
                None => {
                    self.set_status_msg("No repo URL");
                    return;
                }
            };
            let name = entry.prompt.clone().unwrap_or_else(|| entry.name.clone());
            let task_id = entry.task_id.clone();
            self.backend.push(worktree_path, repo_url, name);

            // Mark the local entry as done and drop the terminal session.
            if let Some(entry) = self.entries.get_mut(self.active) {
                entry.session = None;
                entry.status = TaskStatus::Done;
                if let Some(ref id) = task_id {
                    let mut fields = HashMap::new();
                    fields.insert(
                        "status".to_string(),
                        serde_json::Value::String("done".to_string()),
                    );
                    self.backend.update_task(id.clone(), fields);
                }
            }
            self.set_status_msg("Pushing to cloud...");
        }
    }

    /// Pull the active cloud task to local.
    fn pull_active(&mut self) {
        if let Some(entry) = self.entries.get(self.active) {
            let task_id = match &entry.task_id {
                Some(id) => id.clone(),
                None => {
                    self.set_status_msg("Can only pull cloud tasks");
                    return;
                }
            };
            let repo_url = match &entry.repo_url {
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
                self.entries.push(SessionEntry {
                    task_id,
                    name: prompt.chars().take(60).collect(),
                    status: TaskStatus::Running,
                    session: Some(s),
                    worker_vm: None,
                    worker_zone: None,
                    repo_url: Some(repo_url),
                    prompt: Some(prompt),
                    wip_branch: None,
                    session_id: Some(session_id),
                    blocked_at: None,
                    worktree_path: Some(worktree_path),
                    main_repo_path: Some(main_repo),
                    last_write_at: None,
                });
                self.active = self.entries.len() - 1;
                self.set_status_msg("Resumed locally");
            }
            Err(e) => {
                self.set_status_msg(&format!("Resume failed: {}", e));
            }
        }
    }

    /// Handle terminal resize.
    pub fn resize_terminals(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
        for entry in &mut self.entries {
            if let Some(ref session) = entry.session {
                session.resize(cols, rows);
            }
        }
    }

    // ── Drawing ──────────────────────────────────────────────────────

    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();

        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);

        let content_area = rows[0];
        let bar_area = rows[1];

        let cols =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(30)]).split(content_area);

        self.draw_terminal(frame, cols[0]);
        self.draw_session_list(frame, cols[1]);
        self.draw_status_bar(frame, bar_area);

        // Draw input overlay if active.
        if let InputMode::NewSession {
            label_text,
            branch_text,
            repo_url,
            active_field,
        } = &self.input_mode
        {
            self.draw_input_dialog(frame, area, label_text, branch_text, repo_url, *active_field);
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

        let cursor = "█";
        let name_cursor = if active_field == 0 { cursor } else { "" };
        let branch_cursor = if active_field == 1 { cursor } else { "" };

        let branch_hint = if branch_text.is_empty() && active_field != 1 {
            "main"
        } else {
            ""
        };

        let lines = vec![
            Line::from(vec![
                Span::styled("  Repo: ", Style::default().fg(Color::DarkGray)),
                Span::styled(repo_name, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Name: ", Style::default().fg(Color::DarkGray)),
                Span::styled(label_text, Style::default().fg(Color::White)),
                Span::styled(name_cursor, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("Branch: ", Style::default().fg(Color::DarkGray)),
                Span::styled(branch_text, Style::default().fg(Color::White)),
                Span::styled(branch_cursor, Style::default().fg(Color::White)),
                Span::styled(branch_hint, Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Tab switch field · Enter start · Esc cancel",
                Style::default().fg(Color::DarkGray),
            )),
        ];

        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_terminal(&self, frame: &mut Frame, area: Rect) {
        let has_session = self
            .entries
            .get(self.active)
            .and_then(|e| e.session.as_ref())
            .is_some();

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

        if let Some(entry) = self.entries.get(self.active) {
            if let Some(ref session) = entry.session {
                let widget = TerminalWidget::new(&session.term, true);
                frame.render_widget(widget, inner);
            } else {
                let mut lines = vec![];
                if let Some(ref prompt) = entry.prompt {
                    lines.push(Line::from(Span::styled(
                        prompt.as_str(),
                        Style::default().fg(Color::White),
                    )));
                    lines.push(Line::from(""));
                }
                if let Some(ref repo) = entry.repo_url {
                    lines.push(Line::from(Span::styled(
                        format!("Repo: {}", repo),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                if let Some(ref vm) = entry.worker_vm {
                    lines.push(Line::from(Span::styled(
                        format!("VM: {}", vm),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                if !lines.is_empty() {
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(Span::styled(
                    if entry.worker_vm.is_some() {
                        "Press Alt+A to SSH into this session"
                    } else {
                        "Press Alt+A to attach"
                    },
                    Style::default().fg(Color::DarkGray),
                )));

                frame.render_widget(Paragraph::new(lines), inner);
            }
        } else {
            let msg = if self.connected {
                Paragraph::new("No tasks — press Alt+n to start a local session")
                    .style(Style::default().fg(Color::DarkGray))
            } else {
                Paragraph::new("Connecting to API...")
                    .style(Style::default().fg(Color::DarkGray))
            };
            frame.render_widget(msg, inner);
        }
    }

    fn draw_session_list(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                " Sessions ",
                Style::default().fg(Color::White),
            ));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 2 || inner.width < 4 {
            return;
        }

        let spinner = self.spinner_frame();
        let list_height = inner.height.saturating_sub(7);
        let dim = Style::default().fg(Color::DarkGray);

        // Build items grouped by section: active first, then waiting.
        let make_item = |i: usize, entry: &SessionEntry| -> ListItem {
            let is_active = i == self.active;
            let (indicator, indicator_style) = match entry.status {
                TaskStatus::Running => (spinner, Style::default().fg(Color::Green)),
                TaskStatus::Blocked => ("●", Style::default().fg(Color::White)),
                TaskStatus::Backlog => ("○", Style::default().fg(Color::DarkGray)),
                TaskStatus::Done => ("✓", Style::default().fg(Color::DarkGray)),
            };
            let max_name = (inner.width as usize).saturating_sub(4);
            let name = if entry.name.len() > max_name {
                format!("{}...", &entry.name[..max_name.saturating_sub(3)])
            } else {
                entry.name.clone()
            };
            let line = Line::from(vec![
                Span::styled(format!(" {} ", indicator), indicator_style),
                Span::raw(name),
            ]);
            let base_style = if is_active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            ListItem::new(line).style(base_style)
        };

        let active_entries: Vec<(usize, &SessionEntry)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.status, TaskStatus::Running))
            .collect();
        let waiting_entries: Vec<(usize, &SessionEntry)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !matches!(e.status, TaskStatus::Running))
            .collect();

        let mut items: Vec<ListItem> = Vec::new();
        let max = list_height as usize;

        for &(i, entry) in &active_entries {
            if items.len() >= max { break; }
            items.push(make_item(i, entry));
        }
        if !active_entries.is_empty() && !waiting_entries.is_empty() && items.len() < max {
            let sep_line = Line::from(Span::styled(
                format!(" {}", "─".repeat(inner.width.saturating_sub(2) as usize)),
                dim,
            ));
            items.push(ListItem::new(sep_line));
        }
        for &(i, entry) in &waiting_entries {
            if items.len() >= max { break; }
            items.push(make_item(i, entry));
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
        let help_rows = 5u16;
        let help_y = inner.y + inner.height.saturating_sub(help_rows + 1);
        let help_area = Rect {
            x: inner.x,
            y: help_y,
            width: inner.width,
            height: help_rows + 1,
        };

        let dim = Style::default().fg(Color::DarkGray);
        let sep = Line::from(Span::styled("─".repeat(inner.width as usize), dim));
        let col = inner.width / 2;

        let help_lines: Vec<(&str, &str)> = vec![
            ("A-j/k  nav", "A-d  done"),
            ("A-a    attach", "A-x  delete"),
            ("A-n    new", "A-p  push"),
            ("A-q    quit", "A-l  pull"),
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
            .entries
            .iter()
            .filter(|e| matches!(e.status, TaskStatus::Running))
            .count();
        let blocked = self
            .entries
            .iter()
            .filter(|e| matches!(e.status, TaskStatus::Blocked))
            .count();
        let backlog = self
            .entries
            .iter()
            .filter(|e| matches!(e.status, TaskStatus::Backlog))
            .count();

        let conn_indicator = if self.connected { "●" } else { "○" };
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
            Span::styled("claude-manager ", Style::default().fg(Color::DarkGray)),
            Span::styled(" ".repeat(pad_left as usize), Style::default()),
            Span::styled(center, Style::default().fg(Color::Yellow)),
            Span::styled(" ".repeat(pad_right as usize), Style::default()),
            Span::styled(right, Style::default().fg(Color::DarkGray)),
        ]);

        frame.render_widget(Paragraph::new(line), area);
    }

    fn active_title(&self) -> String {
        if let Some(entry) = self.entries.get(self.active) {
            format!(" {} ", entry.name)
        } else {
            " Terminal ".to_string()
        }
    }
}
