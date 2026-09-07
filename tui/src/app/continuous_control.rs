//! Operator controls backed by task definitions, including exited sessions.
//! Every host has its own off-thread poller; a disconnected host never blocks
//! another host or turns a cached status into a positive stop confirmation.
use super::*;
use cm_daemon::host_id::HostId;
use serde_json::{json, Value};
use std::sync::{mpsc, Arc};

pub(super) struct Row {
    pub host: HostId,
    pub task: Value,
}

enum Update {
    Inventory(HostId, Result<Vec<Value>, String>),
    Action(HostId, String, Result<(), String>),
}

pub(super) struct Menu {
    rows: Vec<Row>,
    selected: usize,
    preferred: Option<(HostId, String)>,
    errors: HashMap<HostId, String>,
    rx: mpsc::Receiver<Update>,
    commands: HashMap<HostId, mpsc::Sender<(String, Value)>>,
    pending: bool,
    pending_host: Option<HostId>,
    awaiting_refresh: std::collections::HashSet<HostId>,
    message: String,
}

fn rpc(
    pool: &crate::host_pool::HostPool,
    host: &HostId,
    method: &str,
    params: Value,
) -> Result<Value, String> {
    let handle = pool.for_host(host).map_err(|e| e.to_string())?;
    let socket = handle
        .socket_path()
        .ok_or("Host transport is unavailable.")?;
    crate::client_session::rpc_continuous_control(
        &socket,
        &pool.operator_token_for(host),
        method,
        params,
    )
    .map_err(|e| e.to_string())
}

impl Menu {
    pub fn new(
        pool: Arc<crate::host_pool::HostPool>,
        hosts: &crate::hosts::HostsConfig,
        preferred: Option<(HostId, String)>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut commands = HashMap::new();
        if !cfg!(test) {
            for host in &hosts.hosts {
                let host = host.id.clone();
                let (command_tx, command_rx) = mpsc::channel::<(String, Value)>();
                commands.insert(host.clone(), command_tx);
                let tx = tx.clone();
                let pool = Arc::clone(&pool);
                std::thread::spawn(move || loop {
                    let inventory =
                        rpc(&pool, &host, "continuous.list", json!({})).and_then(|value| {
                            value
                                .get("tasks")
                                .and_then(Value::as_array)
                                .cloned()
                                .ok_or("Invalid task inventory response.".into())
                        });
                    if tx.send(Update::Inventory(host.clone(), inventory)).is_err() {
                        return;
                    }
                    match command_rx.recv_timeout(Duration::from_secs(2)) {
                        Ok((method, params)) => {
                            let task_id = params["task_id"].as_str().unwrap_or("").to_string();
                            let result = rpc(&pool, &host, &method, params).map(|_| ());
                            if tx
                                .send(Update::Action(host.clone(), task_id, result))
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                });
            }
        }
        Self {
            rows: Vec::new(),
            selected: 0,
            preferred,
            errors: HashMap::new(),
            rx,
            commands,
            pending: false,
            pending_host: None,
            awaiting_refresh: Default::default(),
            message: "Loading continuous tasks…".into(),
        }
    }

    pub fn poll(&mut self) -> bool {
        let updates: Vec<_> = self.rx.try_iter().collect();
        let changed = !updates.is_empty();
        for update in updates {
            match update {
                Update::Inventory(host, result) => match result {
                    Ok(tasks) => {
                        let selected = self.rows.get(self.selected).map(|r| {
                            (
                                r.host.clone(),
                                r.task["task_id"].as_str().unwrap_or("").to_string(),
                            )
                        });
                        self.errors.remove(&host);
                        if self.pending_host.as_ref() != Some(&host) {
                            self.awaiting_refresh.remove(&host);
                        }
                        self.rows.retain(|row| row.host != host);
                        self.rows.extend(tasks.into_iter().map(|task| Row {
                            host: host.clone(),
                            task,
                        }));
                        self.rows.sort_by(|a, b| {
                            (a.host.as_str(), a.task["task_id"].as_str())
                                .cmp(&(b.host.as_str(), b.task["task_id"].as_str()))
                        });
                        let target = self.preferred.as_ref().or(selected.as_ref());
                        if let Some(index) = target.and_then(|(h, id)| {
                            self.rows.iter().position(|r| {
                                &r.host == h && r.task["task_id"].as_str() == Some(id)
                            })
                        }) {
                            self.selected = index;
                            self.preferred = None;
                        } else {
                            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
                        }
                        if self.message == "Loading continuous tasks…" {
                            self.message.clear();
                        }
                    }
                    Err(error) => {
                        self.errors.insert(host, error);
                    }
                },
                Update::Action(host, task, result) => {
                    self.pending = false;
                    self.pending_host = None;
                    self.awaiting_refresh.insert(host.clone());
                    self.message = match result {
                        Ok(()) => format!(
                            "{task} @{}: request accepted; awaiting refreshed status.",
                            host.as_str()
                        ),
                        Err(error) => format!("{task} @{}: {error}", host.as_str()),
                    };
                }
            }
        }
        changed
    }

    pub fn key(&mut self, key: crossterm::event::KeyEvent) -> InputOutcome {
        match key.code {
            KeyCode::Esc => return InputOutcome::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.preferred = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1));
                self.preferred = None;
            }
            KeyCode::Char('s') | KeyCode::Char('r') if !self.pending => {
                if let Some(row) = self.rows.get(self.selected) {
                    let resume = key.code == KeyCode::Char('r');
                    let method = if resume {
                        "continuous.pause"
                    } else {
                        "continuous.drain"
                    };
                    let mut params = json!({"task_id": row.task["task_id"]});
                    if resume {
                        params["paused"] = json!(false);
                        params["expected_admission_revision"] =
                            row.task["admission_revision"].clone();
                        if let Some(id) = row
                            .task
                            .pointer("/drain/request_id")
                            .and_then(Value::as_str)
                        {
                            params["expected_drain_request_id"] = json!(id);
                        }
                    }
                    if let Some(tx) = self.commands.get(&row.host) {
                        if tx.send((method.into(), params)).is_ok() {
                            self.pending = true;
                            self.pending_host = Some(row.host.clone());
                            self.message = "Sending request…".into();
                        } else {
                            self.message = "Task-control connection closed.".into();
                        }
                    }
                }
            }
            _ => {}
        }
        InputOutcome::Consumed
    }

    fn status(&self, row: &Row) -> &str {
        if self.errors.contains_key(&row.host) {
            return "Status unavailable";
        }
        if self.pending_host.as_ref() == Some(&row.host)
            || self.awaiting_refresh.contains(&row.host)
        {
            return "Refreshing status";
        }
        match row
            .task
            .pointer("/drain_status/state")
            .and_then(Value::as_str)
        {
            Some("drained") => "Stopped",
            Some("draining") => "Stopping after current work",
            Some("blocked") => "Stopping — needs attention",
            _ if row.task["recovery_hold"].is_object() => "Held — batch needs reconciliation",
            _ if row.task["paused"] == true => "Paused",
            _ if row.task["enabled"] == false => "Disabled",
            _ => "Scheduling enabled",
        }
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let width = area.width.saturating_sub(4).min(110);
        let height = area.height.saturating_sub(4).min(25);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .title(" Continuous tasks · Alt+Shift+C ")
            .borders(Borders::ALL);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let mut lines = vec![
            Line::from("↑/↓ select   s stop after current work   r resume   Esc close"),
            Line::from(""),
        ];
        let count = inner.height.saturating_sub(8) as usize;
        let start = self.selected.saturating_sub(count.saturating_sub(1));
        for (index, row) in self.rows.iter().enumerate().skip(start).take(count) {
            let label = row.task["label"]
                .as_str()
                .or(row.task["task_id"].as_str())
                .unwrap_or("Task");
            let text = format!(
                "{} {} @{} — {}",
                if index == self.selected { "›" } else { " " },
                label,
                row.host.as_str(),
                self.status(row)
            );
            lines.push(Line::styled(
                text,
                Style::default().fg(if index == self.selected {
                    theme::TEXT
                } else {
                    theme::MUTED
                }),
            ));
        }
        if self.rows.is_empty() && self.message.is_empty() {
            lines.push(Line::from("No continuous tasks on connected hosts."));
        }
        lines.push(Line::from(""));
        if let Some(row) = self.rows.get(self.selected) {
            if !self.errors.contains_key(&row.host) {
                if let Some(obligations) = row
                    .task
                    .pointer("/drain_status/outstanding")
                    .and_then(Value::as_array)
                {
                    for obligation in obligations.iter().take(2) {
                        lines.push(Line::from(
                            obligation["detail"]
                                .as_str()
                                .unwrap_or("Pending obligation")
                                .to_string(),
                        ));
                    }
                }
            }
        }
        for (host, error) in &self.errors {
            lines.push(Line::styled(
                format!("@{}: {error}", host.as_str()),
                Style::default().fg(theme::ERROR),
            ));
        }
        lines.push(Line::from(self.message.clone()));
        frame.render_widget(
            Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
            inner,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Menu, mpsc::Sender<Update>) {
        let (tx, rx) = mpsc::channel();
        (
            Menu {
                rows: vec![],
                selected: 0,
                preferred: None,
                errors: HashMap::new(),
                rx,
                commands: HashMap::new(),
                pending: false,
                pending_host: None,
                awaiting_refresh: Default::default(),
                message: String::new(),
            },
            tx,
        )
    }

    fn task(id: &str) -> Value {
        json!({"task_id": id, "paused": true, "current_session_uid": null,
            "admission_revision": 7, "drain": {"request_id": "request-7"},
            "drain_status": {"state": "drained", "outstanding": []}})
    }

    #[test]
    fn continuous_control_routes_exact_host_and_task_and_guards_cached_resume() {
        let (mut menu, tx) = fixture();
        let remote = HostId::new("cm-manager");
        let local = HostId::local();
        let (local_tx, local_rx) = mpsc::channel();
        let (remote_tx, remote_rx) = mpsc::channel();
        menu.commands.insert(local.clone(), local_tx);
        menu.commands.insert(remote.clone(), remote_tx);
        menu.preferred = Some((remote.clone(), "same-task-id".into()));
        tx.send(Update::Inventory(
            local.clone(),
            Ok(vec![task("same-task-id")]),
        ))
        .unwrap();
        menu.poll();
        tx.send(Update::Inventory(
            remote.clone(),
            Ok(vec![task("same-task-id")]),
        ))
        .unwrap();
        menu.poll();
        assert_eq!(menu.rows[menu.selected].host, remote);
        menu.key(crossterm::event::KeyEvent::new(
            KeyCode::Char('r'),
            KeyModifiers::NONE,
        ));
        let (method, params) = remote_rx.try_recv().unwrap();
        assert_eq!(method, "continuous.pause");
        assert_eq!(
            params,
            json!({"task_id": "same-task-id", "paused": false,
            "expected_drain_request_id": "request-7", "expected_admission_revision": 7})
        );
        assert!(local_rx.try_recv().is_err());
        assert_eq!(menu.status(&menu.rows[menu.selected]), "Refreshing status");
        // An already-queued inventory predates the mutation and cannot confirm it.
        tx.send(Update::Inventory(
            remote.clone(),
            Ok(vec![task("same-task-id")]),
        ))
        .unwrap();
        menu.poll();
        assert_eq!(menu.status(&menu.rows[menu.selected]), "Refreshing status");
        tx.send(Update::Action(
            remote.clone(),
            "same-task-id".into(),
            Ok(()),
        ))
        .unwrap();
        menu.poll();
        assert_eq!(menu.status(&menu.rows[menu.selected]), "Refreshing status");
        tx.send(Update::Inventory(remote, Err("SSH disconnected".into())))
            .unwrap();
        menu.poll();
        assert_eq!(menu.status(&menu.rows[menu.selected]), "Status unavailable");
        // A second host still remains independently usable.
        menu.selected = menu.rows.iter().position(|r| r.host == local).unwrap();
        menu.key(crossterm::event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        ));
        assert_eq!(
            local_rx.try_recv().unwrap(),
            (
                "continuous.drain".into(),
                json!({"task_id": "same-task-id"})
            )
        );
    }

    #[test]
    fn continuous_control_displays_exited_tasks_and_positive_status_only_after_refresh() {
        let (mut menu, tx) = fixture();
        let host = HostId::local();
        tx.send(Update::Inventory(
            host.clone(),
            Ok(vec![task("exited-task")]),
        ))
        .unwrap();
        menu.poll();
        assert_eq!(menu.status(&menu.rows[0]), "Stopped");
        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| menu.draw(frame, frame.area()))
            .unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(rendered.contains("exited-task"));
        assert!(rendered.contains("Stopped"));
        assert!(rendered.contains("stop after current work"));
        tx.send(Update::Inventory(host.clone(), Err("unreachable".into())))
            .unwrap();
        menu.poll();
        assert_eq!(menu.status(&menu.rows[0]), "Status unavailable");
        tx.send(Update::Inventory(host, Ok(vec![task("exited-task")])))
            .unwrap();
        menu.poll();
        assert_eq!(menu.status(&menu.rows[0]), "Stopped");
    }
}
