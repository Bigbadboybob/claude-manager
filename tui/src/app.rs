use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

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

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL_MS: u128 = 80;

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
    /// Typing a prompt for a new local session.
    NewSession {
        prompt_text: String,
        repo_url: String,
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

    /// Process all pending terminal events (non-blocking).
    pub fn drain_terminal_events(&mut self) {
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
                        _ => {}
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
                BackendEvent::PullComplete {
                    task_id: _,
                    worktree_path,
                    main_repo,
                    session_id,
                    repo_url,
                    prompt,
                } => {
                    self.spawn_resumed_session(
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

            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|e| e.task_id.as_deref() == Some(&task.id))
            {
                entry.name = task.prompt.chars().take(60).collect();
                entry.status = TaskStatus::from_api(&task.status);
                entry.worker_vm = task.worker_vm.clone();
                entry.worker_zone = task.worker_zone.clone();
                entry.repo_url = Some(task.repo_url.clone());
                entry.prompt = Some(task.prompt.clone());
                entry.wip_branch = task.wip_branch.clone();
                entry.session_id = task.session_id.clone();
                entry.blocked_at = task.blocked_at.clone();
            } else {
                self.entries.push(SessionEntry {
                    task_id: Some(task.id.clone()),
                    name: task.prompt.chars().take(60).collect(),
                    status: TaskStatus::from_api(&task.status),
                    session: None,
                    worker_vm: task.worker_vm.clone(),
                    worker_zone: task.worker_zone.clone(),
                    repo_url: Some(task.repo_url.clone()),
                    prompt: Some(task.prompt.clone()),
                    wip_branch: task.wip_branch.clone(),
                    session_id: task.session_id.clone(),
                    blocked_at: task.blocked_at.clone(),
                    worktree_path: None,
                    main_repo_path: None,
                });
            }
        }

        self.entries.retain(|e| match &e.task_id {
            Some(id) => seen_ids.contains(id) || e.session.is_some(),
            None => true,
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
                    KeyCode::Char('j') => {
                        if !self.entries.is_empty() {
                            self.active = (self.active + 1) % self.entries.len();
                        }
                        return true;
                    }
                    KeyCode::Char('k') => {
                        if !self.entries.is_empty() {
                            self.active =
                                (self.active + self.entries.len() - 1) % self.entries.len();
                        }
                        return true;
                    }
                    KeyCode::Enter => {
                        self.attach_active();
                        return true;
                    }
                    KeyCode::Char('d') => {
                        self.mark_active_done();
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

        // Forward to active terminal.
        if let Some(entry) = self.entries.get(self.active) {
            if let Some(ref session) = entry.session {
                if !session.exited {
                    if let Some(bytes) = input::event_to_bytes(event) {
                        session.write(bytes);
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
                    prompt_text,
                    repo_url,
                } => match key.code {
                    KeyCode::Esc => {
                        self.input_mode = InputMode::Normal;
                        return true;
                    }
                    KeyCode::Enter => {
                        if !prompt_text.trim().is_empty() {
                            let prompt = prompt_text.clone();
                            let repo = repo_url.clone();
                            self.input_mode = InputMode::Normal;
                            self.create_local_session(&repo, &prompt);
                        }
                        return true;
                    }
                    KeyCode::Backspace => {
                        prompt_text.pop();
                        return true;
                    }
                    KeyCode::Char(c) => {
                        prompt_text.push(c);
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
            prompt_text: String::new(),
            repo_url,
        };
    }

    /// Create a local Claude session in a worktree.
    fn create_local_session(&mut self, repo_url: &str, prompt: &str) {
        let main_repo = match worktree::find_local_repo(repo_url) {
            Some(p) => p,
            None => {
                self.set_status_msg("Repo not found locally");
                return;
            }
        };

        let slug = worktree::slugify(prompt);
        if slug.is_empty() {
            self.set_status_msg("Invalid task name");
            return;
        }

        let worktree_path = match worktree::create_worktree(&main_repo, &slug) {
            Ok(p) => p,
            Err(e) => {
                self.set_status_msg(&format!("Worktree: {}", e));
                return;
            }
        };

        // Run setup_worktree.sh if it exists.
        worktree::setup_worktree(&main_repo, &worktree_path);

        // Spawn Claude Code in the worktree.
        let (cols, rows) = self.last_term_size;
        let args = vec![
            "--dangerously-skip-permissions".to_string(),
            "-p".to_string(),
            prompt.to_string(),
        ];

        let session = Session::new(
            "claude",
            &args,
            cols,
            rows,
            Some(worktree_path.clone()),
            Default::default(),
        );

        match session {
            Ok(s) => {
                let entry = SessionEntry {
                    task_id: None,
                    name: prompt.chars().take(60).collect(),
                    status: TaskStatus::Running,
                    session: Some(s),
                    worker_vm: None,
                    worker_zone: None,
                    repo_url: Some(repo_url.to_string()),
                    prompt: Some(prompt.to_string()),
                    wip_branch: None,
                    session_id: None,
                    blocked_at: None,
                    worktree_path: Some(worktree_path),
                    main_repo_path: Some(main_repo),
                };
                self.entries.push(entry);
                self.active = self.entries.len() - 1;
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
            } else {
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

    /// Push the active local session to the cloud.
    fn push_active(&mut self) {
        if let Some(entry) = self.entries.get(self.active) {
            // Only push local sessions (no task_id) that have a worktree.
            if entry.task_id.is_some() {
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
            self.backend.push(worktree_path, repo_url, name);
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
                    task_id: None,
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
            prompt_text,
            repo_url,
        } = &self.input_mode
        {
            self.draw_input_dialog(frame, area, prompt_text, repo_url);
        }
    }

    fn draw_input_dialog(
        &self,
        frame: &mut Frame,
        area: Rect,
        prompt_text: &str,
        repo_url: &str,
    ) {
        let width = 60u16.min(area.width.saturating_sub(4));
        let height = 7u16;
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

        // Extract repo name for display.
        let repo_name = repo_url
            .trim_end_matches('/')
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .unwrap_or(repo_url);

        let lines = vec![
            Line::from(vec![
                Span::styled("Repo: ", Style::default().fg(Color::DarkGray)),
                Span::styled(repo_name, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("Prompt: ", Style::default().fg(Color::DarkGray)),
                Span::styled(prompt_text, Style::default().fg(Color::White)),
                Span::styled("█", Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Enter to start · Esc to cancel",
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
                        "Press Alt+Enter to SSH into this session"
                    } else {
                        "Press Alt+Enter to attach"
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
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .enumerate()
            .take(list_height as usize)
            .map(|(i, entry)| {
                let is_active = i == self.active;

                let (indicator, indicator_style) = match entry.status {
                    TaskStatus::Running => (spinner, Style::default().fg(Color::Green)),
                    TaskStatus::Blocked => ("●", Style::default().fg(Color::Yellow)),
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
            })
            .collect();

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
            ("A-Ent  attach", "A-r  refresh"),
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
