//! Fork a session into a task (A-F on a Claude / Codex session row).
//!
//! The focused session's conversation continues in a NEW task or an
//! EXISTING one. The owning host's daemon (`session.fork`, the same RPC
//! behind the MCP `fork_session` tool) does the work:
//! - new task: a subtask of the source's task (or another parent, or
//!   top-level in the same project) with its own `cm-sub/...` worktree cut
//!   at the source's HEAD or at trunk;
//! - existing task: joins that task's workspace exactly like
//!   `start_session(task_id=…)` (shared checkouts are fine and reported),
//!   minting one if the task never ran;
//!
//! then spawns the engine's native fork there (`claude --resume <id>
//! --fork-session --session-id <new>` / `codex fork <id>`). The source
//! session is untouched and CM keeps no history copy (contrast the A-b
//! agent-memory snapshots, which copy the transcript into
//! `~/.cm/agent-memories/` until deleted by hand).
//!
//! The fork starts with NO first prompt — it waits at its composer for
//! Owner's own first message. Where it came from (source session and
//! branch, and the source commits its checkout lacks) is shown on the
//! status line, never typed into the session.
//!
//! The RPC may provision a checkout before it replies, so it runs off the
//! input thread like remote A-n (`remote_create.rs`).
use super::*;
use crossterm::event::KeyEvent;
use serde_json::{json, Value};
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Task row + worktree + setup script + spawn on a large repository.
const FORK_TIMEOUT: Duration = Duration::from_secs(150);

/// Shown when the host's brain predates `session.fork`.
pub(crate) const FORK_UNSUPPORTED: &str =
    "host daemon too old for fork (no session.fork); it needs a brain deploy";

/// Form rows. `Parent` only applies to a new task.
const F_TARGET: u8 = 0;
const F_TASK: u8 = 1;
const F_PARENT: u8 = 2;
const F_BASE: u8 = 3;
const FIELDS: u8 = 4;

/// A task the user can pick (from the viewer's task list).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskChoice {
    pub task_id: String,
    pub name: String,
}

/// Where a NEW task hangs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ParentChoice {
    /// The source session's task (the host's default).
    SourceTask,
    /// No parent: a top-level task in the source task's project.
    TopLevel,
    Task(TaskChoice),
}

/// Which list the task picker is choosing for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum PickFor {
    Existing,
    Parent,
}

/// Type-to-filter task picker overlaying the form.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskPicker {
    pub purpose: PickFor,
    pub query: String,
    pub selected: usize,
}

/// The A-F form. Identifies the source by stable ids (a backend reorder
/// while the form is open must not retarget it).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForkForm {
    pub workspace_id: String,
    pub session_uid: String,
    pub source_label: String,
    /// The source's task name for the parent row ("" when taskless).
    pub source_task_name: String,
    /// TUI-internal engine name of the source (`claude` / `codex`); the
    /// fork always uses the same engine.
    pub session_type: String,
    /// false = a new task, true = an existing task.
    pub existing: bool,
    pub task_name: String,
    pub existing_task: Option<TaskChoice>,
    pub parent: ParentChoice,
    /// false = cut new branches at the source's HEAD (default), true = at
    /// the project's trunk.
    pub use_trunk: bool,
    pub active_field: u8,
    pub error: Option<String>,
    /// Tasks offered by the pickers, captured at open.
    pub tasks: Vec<TaskChoice>,
    pub picker: Option<TaskPicker>,
}

impl ForkForm {
    pub(crate) fn new(
        workspace_id: &str,
        session_uid: &str,
        source_label: &str,
        session_type: &str,
    ) -> Self {
        Self {
            workspace_id: workspace_id.to_string(),
            session_uid: session_uid.to_string(),
            source_label: source_label.to_string(),
            source_task_name: String::new(),
            session_type: session_type.to_string(),
            existing: false,
            task_name: format!("{} fork", source_label.trim()),
            existing_task: None,
            parent: ParentChoice::SourceTask,
            use_trunk: false,
            active_field: F_TASK,
            error: None,
            tasks: Vec::new(),
            picker: None,
        }
    }

    fn field_applies(&self, field: u8) -> bool {
        !(self.existing && field == F_PARENT)
    }

    fn step_field(&mut self, delta: i8) {
        for _ in 0..FIELDS {
            self.active_field = ((self.active_field as i8 + delta).rem_euclid(FIELDS as i8)) as u8;
            if self.field_applies(self.active_field) {
                break;
            }
        }
    }

    /// Picker candidates for `query`: prefix matches first, then substring
    /// matches (case-insensitive), each in the captured task order.
    pub(crate) fn filtered(&self, query: &str) -> Vec<usize> {
        let q = query.trim().to_lowercase();
        super::nav::rank_prefix_then_substring(self.tasks.len(), |i| {
            let name = self.tasks[i].name.to_lowercase();
            if q.is_empty() || name.starts_with(&q) {
                Some(true)
            } else if name.contains(&q) || self.tasks[i].task_id.starts_with(&q) {
                Some(false)
            } else {
                None
            }
        })
    }

    fn open_picker(&mut self, purpose: PickFor, query: String) {
        self.picker = Some(TaskPicker { purpose, query, selected: 0 });
    }

    fn picker_key(&mut self, key: KeyEvent) -> InputOutcome {
        let Some(mut picker) = self.picker.take() else { return InputOutcome::Consumed };
        let plain = !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
        let matches = self.filtered(&picker.query);
        match key.code {
            KeyCode::Esc => return InputOutcome::Consumed,
            KeyCode::Up | KeyCode::BackTab => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                picker.selected = (picker.selected + 1).min(matches.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(&i) = matches.get(picker.selected) {
                    let choice = self.tasks[i].clone();
                    match picker.purpose {
                        PickFor::Existing => self.existing_task = Some(choice),
                        PickFor::Parent => self.parent = ParentChoice::Task(choice),
                    }
                    self.error = None;
                }
                return InputOutcome::Consumed;
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char(c) if plain => {
                picker.query.push(c);
                picker.selected = 0;
            }
            _ => {}
        }
        self.picker = Some(picker);
        InputOutcome::Consumed
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> InputOutcome {
        if self.picker.is_some() {
            return self.picker_key(key);
        }
        let plain = !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
        let toggle = matches!(key.code, KeyCode::Left | KeyCode::Right | KeyCode::Char(' '));
        match key.code {
            KeyCode::Esc => return InputOutcome::Cancel,
            KeyCode::Tab | KeyCode::Down => self.step_field(1),
            KeyCode::BackTab | KeyCode::Up => self.step_field(-1),
            KeyCode::Enter if self.active_field == F_TASK && self.existing => {
                self.open_picker(PickFor::Existing, String::new());
            }
            KeyCode::Enter if self.active_field == F_PARENT => {
                self.open_picker(PickFor::Parent, String::new());
            }
            KeyCode::Enter => {
                if !self.existing && self.task_name.trim().is_empty() {
                    self.error = Some("Task name is required".into());
                    self.active_field = F_TASK;
                    return InputOutcome::Consumed;
                }
                if self.existing && self.existing_task.is_none() {
                    self.error = Some("Pick the task to fork into (Enter on Task)".into());
                    self.active_field = F_TASK;
                    return InputOutcome::Consumed;
                }
                return InputOutcome::Submit(SubmitAction::ForkSession);
            }
            _ if toggle && self.active_field == F_TARGET => self.existing = !self.existing,
            _ if toggle && self.active_field == F_BASE => self.use_trunk = !self.use_trunk,
            KeyCode::Left | KeyCode::Right if self.active_field == F_PARENT => {
                // ←/→ cycle the two fixed choices; Enter picks any task.
                self.parent = match self.parent {
                    ParentChoice::SourceTask => ParentChoice::TopLevel,
                    _ => ParentChoice::SourceTask,
                };
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.text_field() {
                    buf.pop();
                }
            }
            KeyCode::Char(c) if plain => {
                if self.active_field == F_TASK && self.existing {
                    self.open_picker(PickFor::Existing, c.to_string());
                } else if self.active_field == F_PARENT {
                    self.open_picker(PickFor::Parent, c.to_string());
                } else if let Some(buf) = self.text_field() {
                    buf.push(c);
                }
            }
            _ => {}
        }
        self.error = None;
        InputOutcome::Consumed
    }

    fn text_field(&mut self) -> Option<&mut String> {
        match self.active_field {
            F_TASK if !self.existing => Some(&mut self.task_name),
            _ => None,
        }
    }

    /// The task the fork lands in, for status lines.
    pub(crate) fn target_name(&self) -> String {
        if self.existing {
            self.existing_task.as_ref().map(|t| t.name.clone()).unwrap_or_default()
        } else {
            self.task_name.trim().to_string()
        }
    }

    /// `session.fork` params for this form. `uid` is pre-minted so the
    /// viewer can attach to exactly the session the host spawned; it also
    /// serves as the request's idempotency key.
    pub(crate) fn rpc_params(&self, uid: &str, cols: u16, rows: u16) -> Value {
        let mut params = json!({
            "source_uid": self.session_uid,
            "base": if self.use_trunk { "trunk" } else { "source" },
            "uid": uid,
            "request_id": uid,
            "cols": cols,
            "rows": rows,
        });
        if self.existing {
            if let Some(t) = &self.existing_task {
                params["task_id"] = json!(t.task_id);
                params["label"] = json!(t.name);
            }
        } else {
            let name = self.task_name.trim();
            params["task_name"] = json!(name);
            params["label"] = json!(name);
            match &self.parent {
                ParentChoice::SourceTask => {}
                ParentChoice::TopLevel => params["top_level"] = json!(true),
                ParentChoice::Task(t) => params["parent_task_id"] = json!(t.task_id),
            }
        }
        params
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = 76u16.min(area.width.saturating_sub(4));
        let height = if self.error.is_some() { 16u16 } else { 14u16 };
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect::new(x, y, width, height.min(area.height));
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Fork into a task ",
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let cursor = |field: u8| if self.active_field == field { "\u{2588}" } else { "" };
        let mark = |field: u8| if self.active_field == field { "> " } else { "  " };
        let radio = |on: bool| if on { "\u{25cf}" } else { "\u{25cb}" };
        let pick = |on: bool| if on { white } else { dim };
        let engine = if self.session_type == "codex" { "Codex" } else { "Claude" };
        let mut lines = vec![
            Line::from(vec![
                Span::styled("  From:   ", dim),
                Span::styled(sanitize_for_display(&self.source_label), white),
                Span::styled(format!("  ({engine}, native fork)"), dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("{}Into:   ", mark(F_TARGET)), dim),
                Span::styled(format!("{} new task", radio(!self.existing)), pick(!self.existing)),
                Span::styled("   ", dim),
                Span::styled(format!("{} existing task", radio(self.existing)), pick(self.existing)),
            ]),
        ];
        if self.existing {
            let name = self
                .existing_task
                .as_ref()
                .map(|t| sanitize_for_display(&t.name))
                .unwrap_or_else(|| "(Enter or type to pick)".into());
            lines.push(Line::from(vec![
                Span::styled(format!("{}Task:   ", mark(F_TASK)), dim),
                Span::styled(name, white),
            ]));
            lines.push(Line::from(Span::styled(
                "          joins the task's workspace (shared checkouts are fine)",
                dim,
            )));
        } else {
            let parent = match &self.parent {
                ParentChoice::SourceTask if self.source_task_name.is_empty() => {
                    "source task (none: top-level)".to_string()
                }
                ParentChoice::SourceTask => format!("{} (source task)", self.source_task_name),
                ParentChoice::TopLevel => "top-level, same project".to_string(),
                ParentChoice::Task(t) => t.name.clone(),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{}Task:   ", mark(F_TASK)), dim),
                Span::styled(sanitize_for_display(&self.task_name), white),
                Span::styled(cursor(F_TASK), white),
            ]));
            lines.push(Line::from(vec![
                Span::styled(format!("{}Parent: ", mark(F_PARENT)), dim),
                Span::styled(sanitize_for_display(&parent), white),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled(format!("{}Base:   ", mark(F_BASE)), dim),
            Span::styled(format!("{} source HEAD", radio(!self.use_trunk)), pick(!self.use_trunk)),
            Span::styled("   ", dim),
            Span::styled(format!("{} trunk", radio(self.use_trunk)), pick(self.use_trunk)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Uncommitted changes in the source stay behind; the source is untouched.",
            dim,
        )));
        lines.push(Line::from(Span::styled(
            "  The fork starts with no prompt: it waits for your first message.",
            dim,
        )));
        if let Some(err) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                sanitize_for_display(err),
                Style::default().fg(theme::ERROR),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tab field \u{00b7} \u{2190}/\u{2192} choose \u{00b7} Enter fork/pick \u{00b7} Esc cancel",
            dim,
        )));
        frame.render_widget(Paragraph::new(lines), inner);
        if let Some(picker) = &self.picker {
            self.draw_picker(frame, dialog, picker);
        }
    }

    fn draw_picker(&self, frame: &mut Frame, over: Rect, picker: &TaskPicker) {
        let matches = self.filtered(&picker.query);
        let height = (matches.len() as u16 + 4).clamp(6, 16).min(over.height);
        let area = Rect::new(over.x + 2, over.y + 3, over.width.saturating_sub(4), height);
        frame.render_widget(Clear, area);
        let title = match picker.purpose {
            PickFor::Existing => " Fork into task ",
            PickFor::Parent => " Parent task ",
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let dim = Style::default().fg(theme::DIM);
        let mut lines = vec![Line::from(vec![
            Span::styled("/ ", dim),
            Span::styled(sanitize_for_display(&picker.query), Style::default().fg(theme::TEXT)),
            Span::styled("\u{2588}", Style::default().fg(theme::TEXT)),
        ])];
        let rows = inner.height.saturating_sub(1) as usize;
        let start = picker.selected.saturating_sub(rows.saturating_sub(1));
        if matches.is_empty() {
            lines.push(Line::from(Span::styled("no matching tasks", dim)));
        }
        for (pos, &i) in matches.iter().enumerate().skip(start).take(rows) {
            let style = if pos == picker.selected {
                Style::default().fg(theme::TEXT).add_modifier(Modifier::REVERSED)
            } else {
                Style::default().fg(theme::TEXT)
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{}  {}",
                    sanitize_for_display(&self.tasks[i].name),
                    transcripts::short_id(&self.tasks[i].task_id)
                ),
                style,
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// What the host reported back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForkReply {
    pub session_uid: String,
    pub task_id: String,
    pub task_name: Option<String>,
    pub workspace_id: String,
    pub worktree_path: PathBuf,
    pub label: String,
    pub branch: Option<String>,
    /// The fork's OWN conversation id when known at spawn (claude pins
    /// it); codex's is bound by the host after the fork thread starts.
    pub transcript_id: Option<String>,
    pub source_transcript_id: Option<String>,
    /// Files with uncommitted changes in the source (not carried over).
    pub uncommitted_files: u64,
    /// Other live sessions in the checkout the fork joined.
    pub shared_with: usize,
    /// The source's branch (None: detached HEAD, or an older host).
    pub source_branch: Option<String>,
    /// `git log --oneline` lines on the source that the fork's checkout
    /// lacks (capped by the host; `commits_truncated` marks more).
    pub commits_not_carried: Vec<String>,
    pub commits_truncated: bool,
}

pub(crate) fn parse_fork_reply(v: &Value) -> anyhow::Result<ForkReply> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let need = |k: &str| s(k).ok_or_else(|| anyhow::anyhow!("session.fork reply missing {k}"));
    Ok(ForkReply {
        session_uid: need("session_uid")?,
        task_id: need("task_id")?,
        task_name: s("task_name"),
        workspace_id: need("workspace_id")?,
        worktree_path: PathBuf::from(need("worktree_path")?),
        label: s("label").unwrap_or_default(),
        branch: s("branch"),
        transcript_id: s("transcript_id"),
        source_transcript_id: v
            .get("forked_from")
            .and_then(|f| f.get("transcript_id"))
            .and_then(Value::as_str)
            .map(str::to_string),
        uncommitted_files: v.get("uncommitted_files").and_then(Value::as_u64).unwrap_or(0),
        shared_with: v
            .get("workspace_shared_with")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        source_branch: v
            .get("forked_from")
            .and_then(|f| f.get("branch"))
            .and_then(Value::as_str)
            .map(str::to_string),
        commits_not_carried: v
            .get("commits_not_carried")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        commits_truncated: v
            .get("commits_not_carried_truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// An old brain answers `unknown_method`; say what to do about it.
pub(crate) fn describe_fork_error(e: anyhow::Error) -> anyhow::Error {
    let old_brain = e
        .downcast_ref::<crate::client_session::DaemonRpcError>()
        .is_some_and(|r| r.code == cm_daemon::control::protocol::ErrorCode::UnknownMethod);
    if old_brain { anyhow::anyhow!(FORK_UNSUPPORTED) } else { e }
}

pub(super) struct PreparedFork {
    session: Session,
    reply: ForkReply,
}

pub(super) struct ForkFlight {
    form: ForkForm,
    host: cm_daemon::host_id::HostId,
    result: Receiver<anyhow::Result<PreparedFork>>,
}

fn run_fork(
    pool: &crate::host_pool::HostPool,
    host: &cm_daemon::host_id::HostId,
    form: &ForkForm,
    uid: &str,
    cols: u16,
    rows: u16,
) -> anyhow::Result<PreparedFork> {
    let socket = pool
        .for_host(host)
        .ok()
        .and_then(|h| h.socket_path())
        .ok_or_else(|| anyhow::anyhow!("host `{}` not reachable (no live socket)", host.as_str()))?;
    let token = pool.operator_token_for(host);
    let reply = crate::client_session::rpc_session_fork(
        &socket,
        &token,
        form.rpc_params(uid, cols, rows),
        FORK_TIMEOUT,
    )
    .map_err(describe_fork_error)?;
    let reply = parse_fork_reply(&reply)?;
    let fallback = form.target_name();
    let label = if reply.label.is_empty() { fallback.as_str() } else { reply.label.as_str() };
    let session = try_attach_via_daemon_with_deps(
        pool,
        &reply.session_uid,
        &reply.workspace_id,
        &reply.worktree_path,
        &form.session_type,
        label,
        cols,
        rows,
        Some(&reply.task_id),
        None,
        None,
        host,
        None,
    )
    .map_err(|e| {
        // The fork is running headless; stop it rather than leave a session
        // nobody sees. The task and its worktree stay for a later A-s.
        let _ = crate::client_session::rpc_kill_session(&socket, &token, &reply.session_uid);
        e.context(format!(
            "attach to the fork failed; stopped it — task {} and its worktree remain",
            reply.task_id
        ))
    })?;
    Ok(PreparedFork { session, reply })
}

impl App {
    /// A-F: open the fork form for the focused Claude / Codex session.
    pub(super) fn open_fork_session(&mut self) {
        let Cursor::Session(wi, si) = self.cursor else {
            self.set_status_msg("Fork: focus a Claude or Codex session first");
            return;
        };
        let Some(ws) = self.workspaces.get(wi) else { return };
        let Some(ts) = ws.sessions.get(si) else { return };
        if !matches!(ts.session_type.as_str(), "claude" | "codex") {
            self.set_status_msg("Fork: only Claude / Codex sessions have a conversation to fork");
            return;
        }
        if ws.is_cloud {
            self.set_status_msg("Fork: cloud worker sessions can't be forked");
            return;
        }
        if ts.transcript_id.is_none() {
            self.set_status_msg("Fork: no conversation yet — let the session finish a turn first");
            return;
        }
        let mut form = ForkForm::new(&ws.id, &ts.uid, &ts.label, &ts.session_type);
        form.tasks = self
            .tasks
            .iter()
            .filter(|t| t.api_status != TaskStatus::Done)
            .filter_map(|t| {
                t.task_id.as_ref().map(|id| TaskChoice { task_id: id.clone(), name: t.name.clone() })
            })
            .collect();
        form.source_task_name = ts
            .task_id
            .as_ref()
            .and_then(|id| self.tasks.iter().find(|t| t.task_id.as_ref() == Some(id)))
            .map(|t| t.name.clone())
            .unwrap_or_default();
        self.input_mode = InputMode::ForkSession(form);
    }

    /// Enter on the fork form: run the fork on the source's host.
    pub(super) fn submit_fork(&mut self, form: ForkForm) {
        let Some((wi, si)) =
            resolve_session_by_ids(&self.workspaces, &form.workspace_id, &form.session_uid)
        else {
            self.set_status_msg("Fork cancelled — the source session is no longer in the sidebar");
            return;
        };
        let host = self.workspaces[wi].sessions[si].host_id.clone();
        if self.fork_flights.iter().any(|f| f.form.session_uid == form.session_uid) {
            self.set_status_msg("A fork of this session is already in progress");
            return;
        }
        let pool = std::sync::Arc::clone(&self.host_pool);
        let (cols, rows) = self.last_term_size;
        let uid = new_session_uid();
        let (tx, result) = mpsc::channel();
        let job_form = form.clone();
        let job_host = host.clone();
        let spawn = std::thread::Builder::new()
            .name("cm-session-fork".into())
            .spawn(move || {
                let _ = tx.send(run_fork(&pool, &job_host, &job_form, &uid, cols, rows));
            });
        match spawn {
            Ok(_) => {
                self.set_status_msg(&format!(
                    "Forking {} into \"{}\"…",
                    form.source_label,
                    form.target_name()
                ));
                self.fork_flights.push(ForkFlight { form, host, result });
            }
            Err(e) => self.set_status_msg(&format!("Could not start fork worker: {e}")),
        }
    }

    pub(crate) fn drain_fork_flights(&mut self) {
        let mut i = 0;
        while i < self.fork_flights.len() {
            let result = match self.fork_flights[i].result.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("fork worker disconnected")),
            };
            let flight = self.fork_flights.remove(i);
            match result {
                Ok(prepared) => self.finish_fork(flight.form, flight.host, prepared),
                Err(e) => self.set_status_msg(&format!(
                    "Fork of {} failed: {e:#}",
                    flight.form.source_label
                )),
            }
            self.needs_redraw = true;
        }
    }

    fn finish_fork(
        &mut self,
        form: ForkForm,
        host: cm_daemon::host_id::HostId,
        prepared: PreparedFork,
    ) {
        let reply = prepared.reply;
        let task_name = reply.task_name.clone().unwrap_or_else(|| form.target_name());
        let label = if reply.label.is_empty() { task_name.clone() } else { reply.label.clone() };
        let mut ts = make_simple_session_with_uid(
            reply.session_uid.clone(),
            &label,
            &form.session_type,
            prepared.session,
            None,
        );
        ts.task_id = Some(reply.task_id.clone());
        ts.host_id = host.clone();
        // Bound to the FORK's own conversation, never the source's: claude's
        // id is pinned at spawn; codex's arrives with the host's transcript
        // broadcast once the forked thread starts.
        ts.transcript_id = fork_transcript_binding(&form.session_type, &reply);
        let source_ws = self.workspaces.iter().find(|w| w.id == form.workspace_id);
        let (repo_url, main_repo_path, color) = source_ws
            .map(|w| (w.repo_url.clone(), w.main_repo_path.clone(), w.color.clone()))
            .unwrap_or_default();
        let section = self.workspace_sections.get(&form.workspace_id).cloned();
        // The host may already have announced the session (manifest.watch).
        for ws in &mut self.workspaces {
            ws.sessions.retain(|s| s.uid != ts.uid);
        }
        let wi = match self.workspaces.iter().position(|w| w.id == reply.workspace_id) {
            Some(wi) => wi,
            None => {
                self.workspaces.push(Workspace {
                    id: reply.workspace_id.clone(),
                    name: task_name.clone(),
                    is_closed: false,
                    is_cloud: false,
                    repo_url,
                    worktree_path: Some(reply.worktree_path.clone()),
                    main_repo_path,
                    worker_vm: None,
                    worker_zone: None,
                    host_id: host.clone(),
                    color,
                    pinned: false,
                    sessions: vec![],
                    tombstones: vec![],
                });
                if let Some(sid) = section {
                    self.workspace_sections.entry(reply.workspace_id.clone()).or_insert(sid);
                }
                self.workspaces.len() - 1
            }
        };
        self.workspaces[wi].is_closed = false;
        let si = self.workspaces[wi].sessions.len();
        self.workspaces[wi].sessions.push(ts);
        self.cursor = Cursor::Session(wi, si);
        self.save_session_manifest();
        self.set_status_msg(&fork_status_line(&form.source_label, &task_name, &reply));
    }
}

/// The result line after a fork lands.
pub(crate) fn fork_status_line(source_label: &str, task_name: &str, reply: &ForkReply) -> String {
    let mut line = format!(
        "Forked {}{} into \"{}\" (task {}, {})",
        source_label,
        reply.source_branch.as_deref().map(|b| format!(" ({b})")).unwrap_or_default(),
        task_name,
        transcripts::short_id(&reply.task_id),
        reply.branch.as_deref().unwrap_or("worktree"),
    );
    if !reply.commits_not_carried.is_empty() {
        let shas: Vec<&str> = reply
            .commits_not_carried
            .iter()
            .filter_map(|l| l.split_whitespace().next())
            .collect();
        line.push_str(&format!(
            " — {}{} source commit(s) not carried: {}{}",
            if reply.commits_truncated { "over " } else { "" },
            reply.commits_not_carried.len(),
            shas.join(" "),
            if reply.commits_truncated { " …" } else { "" },
        ));
    }
    if reply.shared_with > 0 {
        line.push_str(&format!(" — shares its checkout with {} live session(s)", reply.shared_with));
    }
    if reply.uncommitted_files > 0 {
        line.push_str(&format!(
            " — warning: {} uncommitted file(s) in the source were left behind",
            reply.uncommitted_files
        ));
    }
    line
}

/// The fork row's transcript binding: claude's pinned fork id (never the
/// source's id, even if a buggy host echoed it), nothing yet for codex.
pub(crate) fn fork_transcript_binding(session_type: &str, reply: &ForkReply) -> Option<String> {
    if session_type != "claude" {
        return None;
    }
    reply
        .transcript_id
        .clone()
        .filter(|id| Some(id) != reply.source_transcript_id.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_str(f: &mut ForkForm, s: &str) {
        for c in s.chars() {
            f.key(key(KeyCode::Char(c)));
        }
    }

    fn form() -> ForkForm {
        let mut f = ForkForm::new("ws-1", "ts-src-1", "planner", "claude");
        f.tasks = vec![
            TaskChoice { task_id: "t-alpha".into(), name: "Alpha rollout".into() },
            TaskChoice { task_id: "t-beta".into(), name: "Beta".into() },
            TaskChoice { task_id: "t-gamma".into(), name: "Fix alpha bug".into() },
        ];
        f
    }

    #[test]
    fn form_prefills_name_and_defaults_to_new_task_source_parent_source_base() {
        let f = form();
        assert_eq!(f.task_name, "planner fork");
        assert!(!f.existing);
        assert_eq!(f.parent, ParentChoice::SourceTask);
        assert!(!f.use_trunk);
        assert_eq!(f.active_field, F_TASK);
    }

    #[test]
    fn typing_tab_toggles_and_submit_for_a_new_task() {
        let mut f = form();
        for _ in 0.."fork".len() + 1 {
            f.key(key(KeyCode::Backspace));
        }
        type_str(&mut f, "B");
        assert_eq!(f.task_name, "plannerB");
        f.key(key(KeyCode::Tab));
        assert_eq!(f.active_field, F_PARENT);
        f.key(key(KeyCode::Right));
        assert_eq!(f.parent, ParentChoice::TopLevel);
        f.key(key(KeyCode::Left));
        assert_eq!(f.parent, ParentChoice::SourceTask);
        f.key(key(KeyCode::Tab));
        assert_eq!(f.active_field, F_BASE);
        f.key(key(KeyCode::Char('x')));
        assert_eq!(f.task_name, "plannerB", "typing on the base row edits nothing");
        f.key(key(KeyCode::Right));
        assert!(f.use_trunk);
        f.key(key(KeyCode::Char(' ')));
        assert!(!f.use_trunk);
        // No Prompt row: Tab from Base wraps to Into.
        f.key(key(KeyCode::Tab));
        assert_eq!(f.active_field, F_TARGET);
        f.active_field = F_TASK;
        f.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT));
        assert_eq!(f.task_name, "plannerB", "Alt-chords never type into the form");
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Submit(SubmitAction::ForkSession)));
        assert!(matches!(f.key(key(KeyCode::Esc)), InputOutcome::Cancel));
    }

    #[test]
    fn empty_name_is_refused_inline() {
        let mut f = form();
        f.task_name = "   ".into();
        f.active_field = F_BASE;
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Consumed));
        assert_eq!(f.error.as_deref(), Some("Task name is required"));
        assert_eq!(f.active_field, F_TASK, "focus returns to the missing field");
        f.key(key(KeyCode::Char('a')));
        assert!(f.error.is_none(), "the next edit clears the error");
    }

    #[test]
    fn picker_ranks_prefix_before_substring() {
        let f = form();
        let names = |q: &str| f.filtered(q).into_iter().map(|i| f.tasks[i].name.clone()).collect::<Vec<_>>();
        assert_eq!(names(""), vec!["Alpha rollout", "Beta", "Fix alpha bug"]);
        assert_eq!(names("alp"), vec!["Alpha rollout", "Fix alpha bug"]);
        assert_eq!(names("BUG"), vec!["Fix alpha bug"]);
        assert!(names("zzz").is_empty());
    }

    #[test]
    fn existing_target_picks_a_task_and_skips_the_parent_row() {
        let mut f = form();
        f.active_field = F_TARGET;
        f.key(key(KeyCode::Right));
        assert!(f.existing);
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Consumed), "no task yet");
        assert!(f.error.is_some());
        assert_eq!(f.active_field, F_TASK);
        // Typing on the Task row opens the picker pre-filtered.
        type_str(&mut f, "alp");
        assert_eq!(f.picker.as_ref().unwrap().query, "alp");
        f.key(key(KeyCode::Down));
        f.key(key(KeyCode::Enter));
        assert!(f.picker.is_none());
        assert_eq!(f.existing_task.as_ref().unwrap().task_id, "t-gamma");
        f.key(key(KeyCode::Tab));
        assert_eq!(f.active_field, F_BASE, "Parent does not apply to an existing task");
        f.key(key(KeyCode::BackTab));
        assert_eq!(f.active_field, F_TASK);
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Consumed), "Enter re-opens picker");
        f.key(key(KeyCode::Esc));
        assert!(f.picker.is_none(), "Esc closes only the picker");
        f.active_field = F_BASE;
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Submit(SubmitAction::ForkSession)));
    }

    #[test]
    fn parent_picker_sets_an_explicit_parent() {
        let mut f = form();
        f.active_field = F_PARENT;
        f.key(key(KeyCode::Enter));
        type_str(&mut f, "be");
        f.key(key(KeyCode::Enter));
        assert_eq!(
            f.parent,
            ParentChoice::Task(TaskChoice { task_id: "t-beta".into(), name: "Beta".into() })
        );
    }

    #[test]
    fn rpc_params_for_each_target_and_parent() {
        let mut f = form();
        f.task_name = "  try B  ".into();
        let p = f.rpc_params("ts-new-1", 120, 40);
        assert_eq!(p["source_uid"], "ts-src-1");
        assert_eq!(p["task_name"], "try B");
        assert_eq!(p["label"], "try B");
        assert_eq!(p["base"], "source");
        assert_eq!(p["uid"], "ts-new-1");
        assert_eq!(p["request_id"], "ts-new-1", "the pre-minted uid is the idempotency key");
        assert!(p.get("prompt").is_none() && p.get("parent_task_id").is_none() && p.get("top_level").is_none());
        f.parent = ParentChoice::TopLevel;
        assert_eq!(f.rpc_params("u", 1, 1)["top_level"], true);
        f.parent = ParentChoice::Task(TaskChoice { task_id: "t-beta".into(), name: "Beta".into() });
        assert_eq!(f.rpc_params("u", 1, 1)["parent_task_id"], "t-beta");
        f.use_trunk = true;
        let p = f.rpc_params("u", 1, 1);
        assert_eq!(p["base"], "trunk");
        assert!(p.get("prompt").is_none(), "the viewer never sends a first prompt");
        f.existing = true;
        f.existing_task = Some(TaskChoice { task_id: "t-alpha".into(), name: "Alpha rollout".into() });
        let p = f.rpc_params("u", 1, 1);
        assert_eq!(p["task_id"], "t-alpha");
        assert_eq!(p["label"], "Alpha rollout");
        assert!(p.get("task_name").is_none() && p.get("parent_task_id").is_none());
    }

    fn reply_json() -> Value {
        json!({
            "session_uid": "ts-new-1", "task_id": "task-f", "task_name": "try B",
            "workspace_id": "ws-f", "worktree_path": "/wt/cm-sub-x", "label": "try B",
            "branch": "cm-sub/x", "transcript_id": "new-conv", "engine": "claude-code",
            "forked_from": {"session_uid": "ts-src-1", "transcript_id": "src-conv", "branch": "cm/feature"},
            "commits_not_carried": ["abc1234 later work", "def5678 more"],
            "commits_not_carried_truncated": false,
            "uncommitted_files": 3,
            "workspace_shared_with": [{"session_uid": "ts-other", "label": "o"}],
        })
    }

    #[test]
    fn reply_parses_and_binds_the_forked_conversation() {
        let r = parse_fork_reply(&reply_json()).unwrap();
        assert_eq!(r.task_id, "task-f");
        assert_eq!(r.worktree_path, PathBuf::from("/wt/cm-sub-x"));
        assert_eq!(r.uncommitted_files, 3);
        assert_eq!(r.shared_with, 1);
        assert_eq!(fork_transcript_binding("claude", &r).as_deref(), Some("new-conv"));
        // Codex binds after the fork thread starts — never to the source.
        assert_eq!(fork_transcript_binding("codex", &r), None);
        let mut echoed = r.clone();
        echoed.transcript_id = Some("src-conv".into());
        assert_eq!(fork_transcript_binding("claude", &echoed), None);
        let mut missing = reply_json();
        missing.as_object_mut().unwrap().remove("task_id");
        assert!(parse_fork_reply(&missing).is_err());
    }

    #[test]
    fn status_line_warns_about_uncommitted_and_shared_checkouts() {
        let r = parse_fork_reply(&reply_json()).unwrap();
        let line = fork_status_line("planner", "try B", &r);
        assert!(line.starts_with("Forked planner (cm/feature) into \"try B\" (task task-f, cm-sub/x)"));
        assert!(line.contains("2 source commit(s) not carried: abc1234 def5678"), "{line}");
        assert!(line.contains("shares its checkout with 1 live session(s)"));
        assert!(line.contains("warning: 3 uncommitted file(s) in the source were left behind"));
        // An older host's reply (no branch / commit fields) still renders.
        let mut old = reply_json();
        old.as_object_mut().unwrap().remove("commits_not_carried");
        old["forked_from"].as_object_mut().unwrap().remove("branch");
        let line = fork_status_line("planner", "try B", &parse_fork_reply(&old).unwrap());
        assert!(line.starts_with("Forked planner into \"try B\""), "{line}");
        assert!(!line.contains("not carried"));
    }

    #[test]
    fn old_brain_unknown_method_names_the_deploy() {
        let e = anyhow::Error::new(crate::client_session::DaemonRpcError {
            method: "session.fork".into(),
            code: cm_daemon::control::protocol::ErrorCode::UnknownMethod,
            message: "unknown method".into(),
        });
        assert_eq!(describe_fork_error(e).to_string(), FORK_UNSUPPORTED);
        let other = describe_fork_error(anyhow::anyhow!("boom"));
        assert_eq!(other.to_string(), "boom");
    }

    fn test_app() -> App {
        App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        })
    }

    fn dummy_session() -> Session {
        Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None).expect("dummy session")
    }

    fn app_with_source(session_type: &str, transcript: Option<&str>) -> App {
        let mut app = test_app();
        let mut ts = make_simple_session_with_uid(
            "ts-src-1".into(),
            "planner",
            session_type,
            dummy_session(),
            None,
        );
        ts.transcript_id = transcript.map(str::to_string);
        ts.task_id = Some("task-src".into());
        app.workspaces.push(Workspace {
            color: Some("teal".into()),
            pinned: false,
            id: "ws-1".into(),
            name: "planner".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: Some("git@example.org:o/r.git".into()),
            worktree_path: Some(std::env::temp_dir()),
            main_repo_path: Some(PathBuf::from("/repo")),
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            sessions: vec![ts],
            tombstones: Vec::new(),
        });
        app.cursor = Cursor::Session(0, 0);
        app.sessions_restored = false;
        app
    }

    #[test]
    fn open_requires_an_agent_row_with_a_conversation() {
        let mut app = test_app();
        app.open_fork_session();
        assert!(matches!(app.input_mode, InputMode::Normal), "no session focused");
        let mut app = app_with_source("bash", None);
        app.open_fork_session();
        assert!(matches!(app.input_mode, InputMode::Normal), "bash has nothing to fork");
        let mut app = app_with_source("claude", None);
        app.open_fork_session();
        assert!(matches!(app.input_mode, InputMode::Normal), "no conversation yet");
    }

    #[test]
    fn alt_shift_f_opens_the_fork_form_in_both_key_encodings() {
        let mut app = app_with_source("codex", Some("src-conv"));
        let ev = CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::ALT));
        assert!(app.handle_event(&ev));
        match &app.input_mode {
            InputMode::ForkSession(f) => {
                assert_eq!(f.session_uid, "ts-src-1");
                assert_eq!(f.session_type, "codex");
                assert_eq!(f.task_name, "planner fork");
            }
            _ => panic!("A-F must open the fork form"),
        }
        let mut app = app_with_source("codex", Some("src-conv"));
        let ev = CrosstermEvent::Key(KeyEvent::new(
            KeyCode::Char('f'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ));
        app.handle_event(&ev);
        assert!(matches!(app.input_mode, InputMode::ForkSession(_)), "shift-as-modifier form");
    }

    #[test]
    fn finish_fork_adds_a_workspace_bound_to_the_new_task_and_conversation() {
        let mut app = app_with_source("claude", Some("src-conv"));
        app.workspace_sections.insert("ws-1".into(), "sec-a".into());
        let form = ForkForm::new("ws-1", "ts-src-1", "planner", "claude");
        let reply = parse_fork_reply(&reply_json()).unwrap();
        app.finish_fork(
            form,
            cm_daemon::host_id::HostId::local(),
            PreparedFork { session: dummy_session(), reply },
        );
        assert_eq!(app.workspaces.len(), 2);
        let ws = &app.workspaces[1];
        assert_eq!(ws.id, "ws-f");
        assert_eq!(ws.name, "try B");
        assert_eq!(ws.worktree_path, Some(PathBuf::from("/wt/cm-sub-x")));
        assert_eq!(ws.main_repo_path, Some(PathBuf::from("/repo")), "same repository as the source");
        assert_eq!(app.workspace_sections.get("ws-f").map(String::as_str), Some("sec-a"));
        let ts = &ws.sessions[0];
        assert_eq!(ts.uid, "ts-new-1");
        assert_eq!(ts.label, "try B");
        assert_eq!(ts.session_type, "claude");
        assert_eq!(ts.task_id.as_deref(), Some("task-f"));
        assert_eq!(ts.transcript_id.as_deref(), Some("new-conv"));
        assert!(matches!(app.cursor, Cursor::Session(1, 0)));
        let src = &app.workspaces[0].sessions[0];
        assert_eq!(src.transcript_id.as_deref(), Some("src-conv"), "source untouched");
        assert_eq!(src.task_id.as_deref(), Some("task-src"));
    }

    /// An existing task's workspace already in the sidebar gets the fork
    /// as one more session — no duplicate workspace row.
    #[test]
    fn finish_fork_into_an_existing_workspace_joins_it() {
        let mut app = app_with_source("claude", Some("src-conv"));
        let mut form = ForkForm::new("ws-1", "ts-src-1", "planner", "claude");
        form.existing = true;
        let mut reply = parse_fork_reply(&reply_json()).unwrap();
        reply.workspace_id = "ws-1".into();
        app.finish_fork(
            form,
            cm_daemon::host_id::HostId::local(),
            PreparedFork { session: dummy_session(), reply },
        );
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.workspaces[0].sessions.len(), 2);
        assert!(matches!(app.cursor, Cursor::Session(0, 1)));
    }
}
