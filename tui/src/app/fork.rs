//! Fork into new task (A-F on a Claude / Codex session row).
//!
//! The focused session's conversation continues in a NEW task: the owning
//! host's daemon (`session.fork`, the same RPC behind the MCP
//! `fork_session` tool) mints a subtask of the source's task with its own
//! `cm-sub/...` worktree — cut at the source's HEAD or at trunk — and
//! spawns the engine's native fork there (`claude --resume <id>
//! --fork-session --session-id <new>` / `codex fork <id>`). The source
//! session is untouched and no history copy is kept by CM (contrast the
//! A-b agent-memory snapshots, which copy the transcript into
//! `~/.cm/agent-memories/` until deleted by hand).
//!
//! The RPC provisions a checkout before it replies, so it runs off the
//! input thread like remote A-n (`remote_create.rs`).
use super::*;
use serde_json::{json, Value};
use crossterm::event::KeyEvent;
use std::sync::mpsc::{self, Receiver, TryRecvError};

/// Task row + worktree + setup script + spawn on a large repository.
const FORK_TIMEOUT: Duration = Duration::from_secs(150);

/// The A-F form. Identifies the source by stable ids (a backend reorder
/// while the form is open must not retarget it).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForkForm {
    pub workspace_id: String,
    pub session_uid: String,
    pub source_label: String,
    /// TUI-internal engine name of the source (`claude` / `codex`); the
    /// fork always uses the same engine.
    pub session_type: String,
    pub task_name: String,
    /// false = cut the new branch at the source's HEAD (default), true =
    /// at the project's trunk.
    pub use_trunk: bool,
    pub prompt: String,
    /// 0 = task name, 1 = base, 2 = first prompt.
    pub active_field: u8,
    pub error: Option<String>,
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
            session_type: session_type.to_string(),
            task_name: format!("{} fork", source_label.trim()),
            use_trunk: false,
            prompt: String::new(),
            active_field: 0,
            error: None,
        }
    }

    pub(crate) fn key(&mut self, key: KeyEvent) -> InputOutcome {
        let plain = !key.modifiers.intersects(KeyModifiers::ALT | KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return InputOutcome::Cancel,
            KeyCode::Tab | KeyCode::Down => self.active_field = (self.active_field + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => self.active_field = (self.active_field + 2) % 3,
            KeyCode::Enter => {
                if self.task_name.trim().is_empty() {
                    self.error = Some("Task name is required".into());
                    self.active_field = 0;
                    return InputOutcome::Consumed;
                }
                return InputOutcome::Submit(SubmitAction::ForkSession);
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') if self.active_field == 1 => {
                self.use_trunk = !self.use_trunk;
            }
            KeyCode::Backspace => {
                if let Some(buf) = self.text_field() {
                    buf.pop();
                }
            }
            KeyCode::Char(c) if plain => {
                if let Some(buf) = self.text_field() {
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
            0 => Some(&mut self.task_name),
            2 => Some(&mut self.prompt),
            _ => None,
        }
    }

    /// `session.fork` params for this form. `uid` is pre-minted so the
    /// viewer can attach to exactly the session the host spawned.
    pub(crate) fn rpc_params(&self, uid: &str, cols: u16, rows: u16) -> Value {
        let task_name = self.task_name.trim();
        let mut params = json!({
            "source_uid": self.session_uid,
            "task_name": task_name,
            "label": task_name,
            "base": if self.use_trunk { "trunk" } else { "source" },
            "uid": uid,
            "cols": cols,
            "rows": rows,
        });
        if !self.prompt.trim().is_empty() {
            params["prompt"] = json!(self.prompt);
        }
        params
    }

    pub(crate) fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = 72u16.min(area.width.saturating_sub(4));
        let height = if self.error.is_some() { 14u16 } else { 12u16 };
        let x = area.x + (area.width.saturating_sub(width)) / 2;
        let y = area.y + (area.height.saturating_sub(height)) / 2;
        let dialog = Rect::new(x, y, width, height.min(area.height));
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Fork into new task ",
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let dim = Style::default().fg(theme::DIM);
        let white = Style::default().fg(theme::TEXT);
        let cursor = |field: u8| if self.active_field == field { "\u{2588}" } else { "" };
        let mark = |field: u8| if self.active_field == field { "> " } else { "  " };
        let engine = if self.session_type == "codex" { "Codex" } else { "Claude" };
        let (source_style, trunk_style) = if self.use_trunk { (dim, white) } else { (white, dim) };
        let mut lines = vec![
            Line::from(vec![
                Span::styled("  From:   ", dim),
                Span::styled(sanitize_for_display(&self.source_label), white),
                Span::styled(format!("  ({engine}, native fork)"), dim),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("{}Task:   ", mark(0)), dim),
                Span::styled(sanitize_for_display(&self.task_name), white),
                Span::styled(cursor(0), white),
            ]),
            Line::from(vec![
                Span::styled(format!("{}Base:   ", mark(1)), dim),
                Span::styled(
                    format!("{} source HEAD", if self.use_trunk { "\u{25cb}" } else { "\u{25cf}" }),
                    source_style,
                ),
                Span::styled("   ", dim),
                Span::styled(
                    format!("{} trunk", if self.use_trunk { "\u{25cf}" } else { "\u{25cb}" }),
                    trunk_style,
                ),
            ]),
            Line::from(vec![
                Span::styled(format!("{}Prompt: ", mark(2)), dim),
                Span::styled(sanitize_for_display(&self.prompt), white),
                Span::styled(cursor(2), white),
                Span::styled(if self.prompt.is_empty() && self.active_field != 2 { "(optional)" } else { "" }, dim),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  New subtask + worktree; the source session is untouched.",
                dim,
            )),
        ];
        if let Some(err) = &self.error {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                sanitize_for_display(err),
                Style::default().fg(theme::ERROR),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tab field \u{00b7} \u{2190}/\u{2192} base \u{00b7} Enter fork \u{00b7} Esc cancel",
            dim,
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// What the host reported back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ForkReply {
    pub session_uid: String,
    pub task_id: String,
    pub workspace_id: String,
    pub worktree_path: PathBuf,
    pub label: String,
    pub branch: Option<String>,
    /// The fork's OWN conversation id when known at spawn (claude pins
    /// it); codex's is bound by the host after the fork thread starts.
    pub transcript_id: Option<String>,
    pub source_transcript_id: Option<String>,
}

pub(crate) fn parse_fork_reply(v: &Value) -> anyhow::Result<ForkReply> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let need = |k: &str| s(k).ok_or_else(|| anyhow::anyhow!("session.fork reply missing {k}"));
    Ok(ForkReply {
        session_uid: need("session_uid")?,
        task_id: need("task_id")?,
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
    })
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
    )?;
    let reply = parse_fork_reply(&reply)?;
    let label = if reply.label.is_empty() { form.task_name.trim() } else { reply.label.as_str() };
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
        // nobody sees. The task and worktree stay for a later A-s.
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
        self.input_mode = InputMode::ForkSession(ForkForm::new(
            &ws.id,
            &ts.uid,
            &ts.label,
            &ts.session_type,
        ));
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
                    form.task_name.trim()
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
        let label = if reply.label.is_empty() {
            form.task_name.trim().to_string()
        } else {
            reply.label.clone()
        };
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
                    name: form.task_name.trim().to_string(),
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
                self.workspaces.len() - 1
            }
        };
        if let Some(sid) = section {
            self.workspace_sections.entry(reply.workspace_id.clone()).or_insert(sid);
        }
        self.workspaces[wi].is_closed = false;
        let si = self.workspaces[wi].sessions.len();
        self.workspaces[wi].sessions.push(ts);
        self.cursor = Cursor::Session(wi, si);
        self.save_session_manifest();
        self.set_status_msg(&format!(
            "Forked {} into \"{}\" (task {}, {})",
            form.source_label,
            form.task_name.trim(),
            transcripts::short_id(&reply.task_id),
            reply.branch.as_deref().unwrap_or("new worktree"),
        ));
    }
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

    fn form() -> ForkForm {
        ForkForm::new("ws-1", "ts-src-1", "planner", "claude")
    }

    #[test]
    fn form_prefills_name_and_defaults_to_source_base() {
        let f = form();
        assert_eq!(f.task_name, "planner fork");
        assert!(!f.use_trunk);
        assert_eq!(f.active_field, 0);
        assert!(f.prompt.is_empty());
    }

    #[test]
    fn typing_tab_toggle_and_submit() {
        let mut f = form();
        for _ in 0.."fork".len() + 1 {
            f.key(key(KeyCode::Backspace));
        }
        for c in "B".chars() {
            f.key(key(KeyCode::Char(c)));
        }
        assert_eq!(f.task_name, "plannerB");
        f.key(key(KeyCode::Tab));
        assert_eq!(f.active_field, 1);
        // Typing on the base row toggles nothing and edits nothing.
        f.key(key(KeyCode::Char('x')));
        assert_eq!(f.task_name, "plannerB");
        f.key(key(KeyCode::Right));
        assert!(f.use_trunk);
        f.key(key(KeyCode::Char(' ')));
        assert!(!f.use_trunk);
        f.key(key(KeyCode::Left));
        assert!(f.use_trunk);
        f.key(key(KeyCode::Tab));
        for c in "go on".chars() {
            f.key(key(KeyCode::Char(c)));
        }
        assert_eq!(f.prompt, "go on");
        f.key(key(KeyCode::BackTab));
        assert_eq!(f.active_field, 1);
        // Alt-chords never type into the form.
        f.active_field = 2;
        f.key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::ALT));
        assert_eq!(f.prompt, "go on");
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Submit(SubmitAction::ForkSession)));
        assert!(matches!(f.key(key(KeyCode::Esc)), InputOutcome::Cancel));
    }

    #[test]
    fn empty_name_is_refused_inline() {
        let mut f = form();
        f.task_name = "   ".into();
        f.active_field = 2;
        assert!(matches!(f.key(key(KeyCode::Enter)), InputOutcome::Consumed));
        assert_eq!(f.error.as_deref(), Some("Task name is required"));
        assert_eq!(f.active_field, 0, "focus returns to the missing field");
        f.key(key(KeyCode::Char('a')));
        assert!(f.error.is_none(), "the next edit clears the error");
    }

    #[test]
    fn rpc_params_carry_source_base_uid_and_optional_prompt() {
        let mut f = form();
        f.task_name = "  try B  ".into();
        let p = f.rpc_params("ts-new-1", 120, 40);
        assert_eq!(p["source_uid"], "ts-src-1");
        assert_eq!(p["task_name"], "try B");
        assert_eq!(p["label"], "try B");
        assert_eq!(p["base"], "source");
        assert_eq!(p["uid"], "ts-new-1");
        assert_eq!(p["cols"], 120);
        assert!(p.get("prompt").is_none(), "blank prompt is not sent");
        f.use_trunk = true;
        f.prompt = "continue".into();
        let p = f.rpc_params("ts-new-1", 120, 40);
        assert_eq!(p["base"], "trunk");
        assert_eq!(p["prompt"], "continue");
    }

    fn reply_json() -> Value {
        json!({
            "session_uid": "ts-new-1", "task_id": "task-f", "workspace_id": "ws-f",
            "worktree_path": "/wt/cm-sub-x", "label": "try B", "branch": "cm-sub/x",
            "transcript_id": "new-conv", "engine": "claude-code",
            "forked_from": {"session_uid": "ts-src-1", "transcript_id": "src-conv"},
        })
    }

    #[test]
    fn reply_parses_and_binds_the_forked_conversation() {
        let r = parse_fork_reply(&reply_json()).unwrap();
        assert_eq!(r.task_id, "task-f");
        assert_eq!(r.worktree_path, PathBuf::from("/wt/cm-sub-x"));
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
}
