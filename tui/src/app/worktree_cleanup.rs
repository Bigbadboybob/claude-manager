//! Explicit task completion + cloud-side cleanup; all RPCs run off the UI thread.
use super::*;
use cm_daemon::host_id::HostId;
use serde_json::{json, Value};
use std::sync::{mpsc, Arc};

pub(super) struct Menu {
    pub workspace_id: String,
    pub task_id: Option<String>,
    pub mark_done: bool,
    title: String,
    reap: bool,
    submitted: bool,
    closed: bool,
    scroll: usize,
    updates: mpsc::Receiver<(HostId, Result<Value, String>)>,
    commands: HashMap<HostId, mpsc::Sender<()>>,
    jobs: HashMap<HostId, Value>,
    errors: HashMap<HostId, String>,
}

fn rpc(pool: &crate::host_pool::HostPool, host: &HostId, params: Value) -> Result<Value, String> {
    let handle = pool.for_host(host).map_err(|e| e.to_string())?;
    let socket = handle.socket_path().ok_or("Host transport unavailable")?;
    crate::client_session::rpc_worktree_cleanup(&socket, &pool.operator_token_for(host), params)
        .map_err(|e| e.to_string())
}

impl Menu {
    pub fn new(
        pool: Arc<crate::host_pool::HostPool>,
        hosts: Vec<HostId>,
        owner: HostId,
        workspace_id: String,
        task_id: Option<String>,
        path: Option<PathBuf>,
        title: String,
        mark_done: bool,
    ) -> Self {
        let (tx, updates) = mpsc::channel();
        let mut commands = HashMap::new();
        for host in hosts {
            let (command_tx, command_rx) = mpsc::channel();
            commands.insert(host.clone(), command_tx);
            if cfg!(test) {
                continue;
            }
            let tx = tx.clone();
            let pool = Arc::clone(&pool);
            let task = task_id.clone();
            let root = if host == owner { path.clone() } else { None };
            std::thread::spawn(move || {
                let id = uuid::Uuid::new_v4().to_string();
                let mut params =
                    json!({"action":"preview", "id":id, "task_id":task, "worktree_path":root});
                loop {
                    let response = rpc(&pool, &host, params.clone());
                    // Preview and apply are idempotent. Keep retrying the same
                    // request until acknowledged, including a lost response.
                    if response.is_ok() {
                        params = json!({"action":"status", "id":id});
                    }
                    if tx.send((host.clone(), response)).is_err() {
                        break;
                    }
                    match command_rx.recv_timeout(Duration::from_secs(1)) {
                        Ok(()) => params = json!({"action":"apply", "id":id}),
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            });
        }
        Self {
            workspace_id,
            task_id,
            mark_done,
            title,
            reap: false,
            submitted: false,
            closed: false,
            scroll: 0,
            updates,
            commands,
            jobs: HashMap::new(),
            errors: HashMap::new(),
        }
    }

    pub fn poll(&mut self) -> bool {
        let updates: Vec<_> = self.updates.try_iter().collect();
        let changed = !updates.is_empty();
        for (host, result) in updates {
            match result {
                Ok(job) => {
                    self.errors.remove(&host);
                    self.jobs.insert(host, job);
                }
                Err(error) => {
                    self.errors.insert(host, error);
                }
            }
        }
        changed
    }

    fn ready(&self) -> bool {
        self.errors.is_empty()
            && !self.commands.is_empty()
            && self.commands.len() == self.jobs.len()
            && self.jobs.values().all(|job| job["phase"] == "preview")
    }

    pub fn close_ready(&self) -> bool {
        self.submitted
            && !self.closed
            && self.errors.is_empty()
            && !self.commands.is_empty()
            && self.commands.len() == self.jobs.len()
            && self.jobs.values().all(|job| {
                matches!(
                    job["phase"].as_str(),
                    Some("queued" | "running" | "complete")
                )
            })
    }

    pub fn key(&mut self, key: crossterm::event::KeyEvent) -> InputOutcome {
        match key.code {
            KeyCode::Esc => return InputOutcome::Cancel,
            KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if !self.submitted =>
            {
                self.reap = !self.reap
            }
            KeyCode::Char('j') | KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Enter if !self.submitted => {
                if self.reap && !self.ready() {
                    return InputOutcome::Status("Wait for all cleanup previews; unavailable hosts cannot be silently skipped.".into());
                }
                return InputOutcome::Submit(SubmitAction::CompleteWithCleanup);
            }
            _ => {}
        }
        InputOutcome::Consumed
    }

    pub fn start(&mut self) -> bool {
        if !self.reap {
            return false;
        }
        self.submitted = true;
        for (host, tx) in &self.commands {
            if tx.send(()).is_err() {
                self.errors.insert(
                    host.clone(),
                    "Cleanup connection closed; request not sent.".into(),
                );
            }
        }
        true
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4).min(120);
        let height = area.height.saturating_sub(4).min(32);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(if self.submitted {
                " Worktree cleanup "
            } else {
                " Close task · worktrees "
            })
            .borders(Borders::ALL);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let mut lines = vec![Line::from(self.title.clone()), Line::from("")];
        if !self.submitted {
            lines.push(Line::styled(
                format!(
                    "{} Keep worktrees     {} Reap this task + descendants",
                    if self.reap { "○" } else { "●" },
                    if self.reap { "●" } else { "○" }
                ),
                Style::default().fg(theme::TEXT),
            ));
            lines.push(Line::from(
                "Tab/←/→ choose · Enter close · j/k scroll · Esc cancel",
            ));
        } else {
            lines.push(Line::from(if self.closed {
                "Cleanup continues on the host if you disconnect. Esc closes this report."
            } else {
                "Waiting for hosts to accept cleanup before closing. Esc leaves the task open."
            }));
        }
        lines.push(Line::from(
            "Branches, saved changes and useful artifacts are preserved. Active/shared work stays.",
        ));
        lines.push(Line::from(""));
        let mut detail = Vec::new();
        let mut hosts: Vec<_> = self.commands.keys().collect();
        hosts.sort_by_key(|h| h.as_str());
        for host in hosts {
            if let Some(error) = self.errors.get(host) {
                detail.push(format!("{}: {}", host.as_str(), error));
                continue;
            }
            let Some(job) = self.jobs.get(host) else {
                detail.push(format!("{}: finding worktrees…", host.as_str()));
                continue;
            };
            detail.push(format!(
                "{}: {}",
                host.as_str(),
                job["message"].as_str().unwrap_or("Working…")
            ));
            let results = job.get("results").and_then(Value::as_array);
            if let Some(rows) = results {
                for row in rows {
                    detail.push(format!(
                        "  {} — {}",
                        row["path"].as_str().unwrap_or("?"),
                        row["message"].as_str().unwrap_or("")
                    ));
                }
            } else if let Some(rows) = job["candidates"].as_array() {
                for row in rows {
                    detail.push(format!(
                        "  {} — {}",
                        row["path"].as_str().unwrap_or("?"),
                        row["reason"].as_str().unwrap_or("recheck after close")
                    ));
                }
            }
            if let Some(warnings) = job["warnings"].as_array() {
                for warning in warnings {
                    detail.push(format!(
                        "  Kept / untracked: {}",
                        warning.as_str().unwrap_or("")
                    ));
                }
            }
            if let Some(id) = job["id"].as_str() {
                detail.push(format!("  Receipt: ~/.cm/worktree-cleanup/{id}.json"));
            }
        }
        let available = usize::from(inner.height).saturating_sub(lines.len());
        let start = self.scroll.min(detail.len().saturating_sub(available));
        lines.extend(
            detail
                .into_iter()
                .skip(start)
                .take(available)
                .map(Line::from),
        );
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

impl App {
    pub(super) fn open_worktree_completion(&mut self, mark_done: bool) {
        let Some(wi) = self.active_workspace_index() else {
            return;
        };
        let workspace = &self.workspaces[wi];
        let task_id = self.cursor_task_id().or_else(|| {
            let ids: Vec<_> = self
                .tasks
                .iter()
                .filter(|t| t.workspace_id.as_deref() == Some(&workspace.id))
                .filter_map(|t| t.task_id.clone())
                .collect();
            (ids.len() == 1).then(|| ids[0].clone())
        });
        if mark_done
            && task_id.is_none()
            && self
                .tasks
                .iter()
                .filter(|t| t.workspace_id.as_deref() == Some(&workspace.id) && t.task_id.is_some())
                .count()
                > 1
        {
            self.set_status_msg("Multiple tasks bound — pick one (A-d on its header)");
            return;
        }
        // A workspace shared by several tasks cannot acquire one task's scope
        // merely because that task happened to be the focused session.
        let task_id = if mark_done { task_id } else { None };
        let owner = workspace.host_id.clone();
        let hosts = if task_id.is_some() {
            self.hosts
                .hosts
                .iter()
                .filter(|h| h.id == owner || h.id != HostId::local())
                .map(|h| h.id.clone())
                .collect()
        } else {
            vec![owner.clone()]
        };
        self.input_mode = InputMode::WorktreeCleanup(Menu::new(
            Arc::clone(&self.host_pool),
            hosts,
            owner,
            workspace.id.clone(),
            task_id,
            workspace.worktree_path.clone(),
            workspace.name.clone(),
            mark_done,
        ));
    }

    pub(super) fn complete_with_cleanup(&mut self, mut menu: Menu) {
        let Some(wi) = self
            .workspaces
            .iter()
            .position(|w| w.id == menu.workspace_id)
        else {
            self.set_status_msg("Task workspace changed; reopen the completion dialog.");
            return;
        };
        self.cursor = match &menu.task_id {
            Some(id) => Cursor::Task {
                ws_idx: wi,
                task_id: id.clone(),
            },
            None => Cursor::Workspace(wi),
        };
        if !menu.submitted && menu.start() {
            // Do not close locally until every host acknowledges its durable
            // job. If the viewer exits before that, the task stays open.
            self.input_mode = InputMode::WorktreeCleanup(menu);
            return;
        }
        if menu.mark_done {
            self.mark_active_done();
        } else {
            self.close_active_workspace();
        }
        if menu.submitted {
            menu.closed = true;
            self.input_mode = InputMode::WorktreeCleanup(menu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        Menu,
        mpsc::Sender<(HostId, Result<Value, String>)>,
        Vec<mpsc::Receiver<()>>,
    ) {
        let (tx, updates) = mpsc::channel();
        let mut commands = HashMap::new();
        let mut receivers = vec![];
        for host in ["sessions", "manager"] {
            let (send, recv) = mpsc::channel();
            commands.insert(HostId::new(host), send);
            receivers.push(recv);
        }
        (
            Menu {
                workspace_id: "captured-workspace".into(),
                task_id: Some("captured-task".into()),
                mark_done: true,
                title: "Test task".into(),
                reap: false,
                submitted: false,
                closed: false,
                scroll: 0,
                updates,
                commands,
                jobs: HashMap::new(),
                errors: HashMap::new(),
            },
            tx,
            receivers,
        )
    }

    fn key(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn worktree_cleanup_keep_default_needs_no_network_and_does_not_queue() {
        let (mut menu, _, receivers) = fixture();
        assert!(matches!(
            menu.key(key(KeyCode::Enter)),
            InputOutcome::Submit(SubmitAction::CompleteWithCleanup)
        ));
        assert!(!menu.start());
        assert!(receivers.iter().all(|rx| rx.try_recv().is_err()));
        assert!(!menu.close_ready());
    }

    #[test]
    fn worktree_cleanup_requires_every_preview_and_durable_host_ack_before_close() {
        let (mut menu, tx, receivers) = fixture();
        menu.key(key(KeyCode::Tab));
        tx.send((HostId::new("sessions"), Ok(json!({"phase":"preview"}))))
            .unwrap();
        menu.poll();
        assert!(matches!(
            menu.key(key(KeyCode::Enter)),
            InputOutcome::Status(_)
        ));
        tx.send((HostId::new("manager"), Err("offline".into())))
            .unwrap();
        menu.poll();
        assert!(!menu.ready());
        tx.send((HostId::new("manager"), Ok(json!({"phase":"preview"}))))
            .unwrap();
        menu.poll();
        assert!(menu.ready());
        assert!(menu.start());
        assert!(receivers.iter().all(|rx| rx.try_recv().is_ok()));
        assert!(!menu.close_ready());
        tx.send((HostId::new("sessions"), Ok(json!({"phase":"queued"}))))
            .unwrap();
        menu.poll();
        assert!(!menu.close_ready());
        tx.send((HostId::new("manager"), Ok(json!({"phase":"running"}))))
            .unwrap();
        menu.poll();
        assert!(menu.close_ready());
        assert_eq!(menu.workspace_id, "captured-workspace");
        assert_eq!(menu.task_id.as_deref(), Some("captured-task"));
        menu.closed = true;
        assert!(!menu.close_ready(), "one close only");
    }

    #[test]
    fn worktree_cleanup_modal_scrolls_long_reports_and_escape_does_not_queue() {
        let (mut menu, tx, receivers) = fixture();
        let rows: Vec<_> = (0..60)
            .map(|i| json!({"path":format!("/worktrees/path-{i}"), "reason":"live_session"}))
            .collect();
        tx.send((
            HostId::new("sessions"),
            Ok(json!({"phase":"preview", "candidates":rows})),
        ))
        .unwrap();
        menu.poll();
        for _ in 0..45 {
            menu.key(key(KeyCode::Char('j')));
        }
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| menu.draw(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            text.contains("path-59"),
            "last checkout can be reached: {text}"
        );
        assert!(matches!(menu.key(key(KeyCode::Esc)), InputOutcome::Cancel));
        assert!(receivers.iter().all(|rx| rx.try_recv().is_err()));
        for _ in 0..100 {
            menu.key(key(KeyCode::Char('k')));
        }
        assert_eq!(menu.scroll, 0);
    }
}
