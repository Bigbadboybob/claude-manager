//! Alt+B: the work-item board overlay (doc/items-board.md).
//!
//! Boards come from the planning API (`GET /boards?open_only=true`); a board's
//! contents and every edit go through a daemon's `board.read` / `item.*` as
//! the Operator (Owner), local daemon first and then the other configured
//! hosts. It polls `board.read` with `since_version` every few seconds, and
//! only while visible (no periodic full polls; see CLAUDE.md).
use super::*;
use serde_json::{json, Value};
use std::sync::mpsc;

const POLL: Duration = Duration::from_secs(5);

/// What a typed line will be used for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PromptKind {
    Add,
    Note,
    Reassign,
    Drop,
    ResolveReassign,
    ResolveBlock,
    ResolveDrop,
}

/// A one-key menu.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Menu {
    Status,
    Resolve,
}

type Reply = (String, Result<Value, String>);

#[derive(Default)]
pub(super) struct Board {
    pub(super) visible: bool,
    boards: Vec<Value>,
    board_idx: usize,
    data: Value,
    version: Option<i64>,
    cursor: usize,
    detail: bool,
    prompt: Option<(PromptKind, String)>,
    menu: Option<Menu>,
    rx: Option<mpsc::Receiver<Reply>>,
    last_poll: Option<Instant>,
    listed: bool,
    status: String,
    error: String,
}

impl Board {
    fn slug(&self) -> Option<String> {
        self.boards.get(self.board_idx).and_then(|b| b["slug"].as_str()).map(str::to_owned)
    }

    fn items(&self) -> &[Value] {
        self.data["items"].as_array().map(Vec::as_slice).unwrap_or(&[])
    }

    fn selected_n(&self) -> Option<i64> {
        self.items().get(self.cursor).and_then(|i| i["n"].as_i64())
    }

    /// Apply a `board.read` reply: `{unchanged}` keeps the current data.
    fn accept(&mut self, v: Value) {
        if v["unchanged"] == true {
            return;
        }
        self.version = v["board"]["version"].as_i64();
        self.data = v;
        let n = self.items().len();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }
    }
}

fn age(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 86_400 {
        format!("{}d", s / 86_400)
    } else if s >= 3600 {
        format!("{}h", s / 3600)
    } else {
        format!("{}m", s / 60)
    }
}

/// One item line: `#14 active  title  @rl[idle 24m]  ⚑holder_idle · note`.
fn item_line(item: &Value) -> String {
    let holders: Vec<String> = item["holders"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|h| {
            let name = h["name"].as_str().or_else(|| h["pid"].as_str()).unwrap_or("?");
            let state = h["state"]["state"].as_str().unwrap_or("unknown");
            match h["state"]["for_s"].as_i64() {
                Some(s) => format!("@{name}[{state} {}]", age(s)),
                None => format!("@{name}[{state}]"),
            }
        })
        .collect();
    let flags: Vec<String> = item["flags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f["kind"].as_str().map(|k| format!("\u{2691}{k}")))
        .collect();
    let mut line = format!(
        "#{} {:<8} {}",
        item["n"],
        item["status"].as_str().unwrap_or("?"),
        item["title"].as_str().unwrap_or("")
    );
    for part in [holders.join(" "), flags.join(" ")] {
        if !part.is_empty() {
            line.push_str("  ");
            line.push_str(&part);
        }
    }
    if let Some(b) = item["blocked_by"].as_array().filter(|b| !b.is_empty()) {
        line.push_str(&format!("  \u{2190}{}", b.iter().map(|n| format!("#{n}")).collect::<Vec<_>>().join(",")));
    }
    if let Some(note) = item["note"].as_str().filter(|n| !n.is_empty()) {
        line.push_str(&format!(" \u{00b7} {note}"));
    }
    line
}

impl App {
    fn board_request(&mut self, op: &str, method: &'static str, params: Value) {
        if self.board.rx.is_some() {
            return;
        }
        let targets = self.owner_rpc_targets();
        let (tx, rx) = mpsc::channel();
        self.board.rx = Some(rx);
        let op = op.to_owned();
        std::thread::spawn(move || {
            let mut errors = Vec::new();
            for (host, socket, token) in targets {
                match crate::client_session::rpc_messaging_board(&socket, &token, method, params.clone()) {
                    Ok(v) => {
                        let _ = tx.send((op, Ok(v)));
                        return;
                    }
                    Err(e) => errors.push(format!("{host}: {e}")),
                }
            }
            let msg = if errors.is_empty() { "No daemon is reachable".to_owned() } else { errors.join("; ") };
            let _ = tx.send((op, Err(msg)));
        });
    }

    fn board_list(&mut self) {
        if self.board.rx.is_some() {
            return;
        }
        let (url, token) = (self.config.api_url.clone(), self.config.api_token.clone());
        let (tx, rx) = mpsc::channel();
        self.board.rx = Some(rx);
        std::thread::spawn(move || {
            let result = crate::api::list_open_boards(&url, &token)
                .map(Value::from)
                .map_err(|e| e.to_string());
            let _ = tx.send(("list".into(), result));
        });
    }

    fn board_read(&mut self, incremental: bool) {
        let Some(slug) = self.board.slug() else {
            return;
        };
        let mut params = json!({"board": slug, "view": "full"});
        if incremental {
            if let Some(v) = self.board.version {
                params["since_version"] = json!(v);
            }
        }
        self.board.last_poll = Some(Instant::now());
        self.board_request("read", "board.read", params);
    }

    fn board_write(&mut self, method: &'static str, mut params: Value) {
        if let Some(slug) = self.board.slug() {
            params["board"] = json!(slug);
            self.board_request("write", method, params);
        }
    }

    /// Replies and the visible-only poll. Called every main-loop tick.
    pub fn board_tick(&mut self) {
        if let Some((op, result)) = self.board.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.board.rx = None;
            self.needs_redraw = true;
            match (op.as_str(), result) {
                (_, Err(e)) => self.board.error = e,
                ("list", Ok(v)) => {
                    self.board.error.clear();
                    self.board.listed = true;
                    let keep = self.board.slug();
                    self.board.boards = v.as_array().cloned().unwrap_or_default();
                    self.board.board_idx = keep
                        .and_then(|s| self.board.boards.iter().position(|b| b["slug"] == s.as_str()))
                        .unwrap_or(0);
                    self.board.version = None;
                    self.board_read(false);
                }
                ("read", Ok(v)) => {
                    self.board.error.clear();
                    self.board.accept(v);
                }
                (_, Ok(_)) => {
                    self.board.error.clear();
                    self.board.status = "Saved".into();
                    self.board_read(true);
                }
            }
        }
        if !self.board.visible || self.board.rx.is_some() {
            return;
        }
        if !self.board.listed {
            self.board_list();
        } else if self.board.last_poll.is_none_or(|t| t.elapsed() >= POLL) {
            self.board_read(true);
        }
    }

    fn board_submit_prompt(&mut self, kind: PromptKind, text: String) {
        let text = text.trim().to_owned();
        let n = self.board.selected_n();
        match kind {
            PromptKind::Add if !text.is_empty() => {
                self.board_write("item.create", json!({"title": text, "status": "open", "holder": "none"}))
            }
            PromptKind::Note => {
                if let Some(n) = n {
                    self.board_write("item.set", json!({"n": n, "note": text}));
                }
            }
            PromptKind::Reassign if !text.is_empty() => {
                if let Some(n) = n {
                    self.board_write("item.set", json!({"n": n, "holder": text}));
                }
            }
            PromptKind::Drop => {
                if let Some(n) = n {
                    self.board_write("item.set", json!({"n": n, "status": "dropped", "reason": text}));
                }
            }
            PromptKind::ResolveReassign if !text.is_empty() => {
                if let Some(n) = n {
                    self.board_write("item.resolve", json!({"n": n, "action": "reassign", "holder": text}));
                }
            }
            PromptKind::ResolveBlock if !text.is_empty() => {
                if let Some(n) = n {
                    self.board_write("item.resolve", json!({"n": n, "action": "block", "blocked_on": text}));
                }
            }
            PromptKind::ResolveDrop => {
                if let Some(n) = n {
                    self.board_write("item.resolve", json!({"n": n, "action": "drop", "reason": text}));
                }
            }
            _ => {}
        }
    }

    /// Alt+B toggles; while visible every key is consumed.
    pub(super) fn board_event(&mut self, event: &CrosstermEvent) -> bool {
        let CrosstermEvent::Key(key) = event else {
            return self.board.visible;
        };
        let alt_b = key.modifiers.contains(KeyModifiers::ALT)
            && (key.code == KeyCode::Char('B')
                || key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::SHIFT));
        if !self.board.visible {
            if !alt_b {
                return false;
            }
            self.board.visible = true;
            self.board.last_poll = None;
            self.board.listed = false;
            self.board_tick();
            return true;
        }
        self.needs_redraw = true;
        if let Some((kind, mut text)) = self.board.prompt.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.board_submit_prompt(kind, text),
                KeyCode::Backspace => {
                    text.pop();
                    self.board.prompt = Some((kind, text));
                }
                KeyCode::Char(c) => {
                    text.push(c);
                    self.board.prompt = Some((kind, text));
                }
                _ => self.board.prompt = Some((kind, text)),
            }
            return true;
        }
        if let Some(menu) = self.board.menu.take() {
            let n = self.board.selected_n();
            match (menu, key.code, n) {
                (Menu::Status, KeyCode::Char(c), Some(n)) => {
                    let status = match c {
                        'o' => Some("open"),
                        'a' => Some("active"),
                        'w' => Some("waiting"),
                        'b' => Some("blocked"),
                        'd' => Some("done"),
                        _ => None,
                    };
                    if let Some(status) = status {
                        self.board_write("item.set", json!({"n": n, "status": status}));
                    }
                }
                (Menu::Resolve, KeyCode::Char('1'), Some(n)) => {
                    self.board_write("item.resolve", json!({"n": n, "action": "nudge"}))
                }
                (Menu::Resolve, KeyCode::Char('2'), Some(_)) => {
                    self.board.prompt = Some((PromptKind::ResolveReassign, String::new()))
                }
                (Menu::Resolve, KeyCode::Char('3'), Some(_)) => {
                    self.board.prompt = Some((PromptKind::ResolveBlock, String::new()))
                }
                (Menu::Resolve, KeyCode::Char('4'), Some(_)) => {
                    self.board.prompt = Some((PromptKind::ResolveDrop, String::new()))
                }
                _ => {}
            }
            return true;
        }
        let count = self.board.items().len();
        let has_item = self.board.selected_n().is_some();
        match key.code {
            _ if alt_b => self.board.visible = false,
            KeyCode::Esc => self.board.visible = false,
            KeyCode::Char('j') | KeyCode::Down => {
                self.board.cursor = (self.board.cursor + 1).min(count.saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => self.board.cursor = self.board.cursor.saturating_sub(1),
            KeyCode::Tab | KeyCode::Char(']') if !self.board.boards.is_empty() => {
                self.board.board_idx = (self.board.board_idx + 1) % self.board.boards.len();
                self.board_switched();
            }
            KeyCode::BackTab | KeyCode::Char('[') if !self.board.boards.is_empty() => {
                let n = self.board.boards.len();
                self.board.board_idx = (self.board.board_idx + n - 1) % n;
                self.board_switched();
            }
            KeyCode::Enter => self.board.detail = !self.board.detail,
            KeyCode::Char('a') => self.board.prompt = Some((PromptKind::Add, String::new())),
            KeyCode::Char('s') if has_item => self.board.menu = Some(Menu::Status),
            KeyCode::Char('n') if has_item => {
                let note = self.board.items()[self.board.cursor]["note"].as_str().unwrap_or("").to_owned();
                self.board.prompt = Some((PromptKind::Note, note));
            }
            KeyCode::Char('r') if has_item => self.board.prompt = Some((PromptKind::Reassign, String::new())),
            KeyCode::Char('x') if has_item => self.board.menu = Some(Menu::Resolve),
            KeyCode::Char('d') if has_item => self.board.prompt = Some((PromptKind::Drop, String::new())),
            KeyCode::Char('o') if has_item => {
                let n = self.board.selected_n().unwrap();
                self.board_write("item.set", json!({"n": n, "status": "active"}));
            }
            KeyCode::Char('g') => {
                self.board.listed = false;
                self.board.last_poll = None;
            }
            _ => {}
        }
        true
    }

    fn board_switched(&mut self) {
        self.board.data = Value::Null;
        self.board.version = None;
        self.board.cursor = 0;
        self.board.detail = false;
        self.board.last_poll = None;
    }

    pub(super) fn draw_board(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let b = &self.board;
        let title = match b.boards.get(b.board_idx) {
            Some(h) => format!(
                " Board: {} ({}/{}) ",
                h["name"].as_str().or_else(|| h["slug"].as_str()).unwrap_or("?"),
                b.board_idx + 1,
                b.boards.len()
            ),
            None => " Board ".into(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(theme::TEXT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines: Vec<Line> = Vec::new();
        let dim = Style::default().fg(theme::DIM);
        let bold = Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD);
        if b.boards.is_empty() && b.listed {
            lines.push(Line::styled("No boards with open items.", dim));
        }
        let header = &b.data["board"];
        if header.is_object() {
            let health = &header["health"];
            let flags = health["unresolved"].as_i64().unwrap_or(0);
            let mut head = format!("orchestrator {}", header["orchestrator"].as_str().unwrap_or("none"));
            if flags > 0 {
                head.push_str(&format!(" \u{00b7} \u{2691}{flags}"));
                if let Some(s) = health["oldest_s"].as_i64() {
                    head.push_str(&format!(" oldest {}", age(s)));
                }
            } else {
                head.push_str(" \u{00b7} no flags");
            }
            let free: Vec<&str> = b.data["free_capacity"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if !free.is_empty() {
                head.push_str(&format!(" \u{00b7} free: {}", free.join(", ")));
            }
            lines.push(Line::styled(head, dim));
            lines.push(Line::from(""));
        }
        let flags = b.data["flags"].as_array().cloned().unwrap_or_default();
        if !flags.is_empty() {
            lines.push(Line::styled("Flags", bold));
            for f in &flags {
                let detail = match &f["detail"] {
                    Value::Null => String::new(),
                    Value::String(s) => format!(" {s}"),
                    other => format!(" {other}"),
                };
                lines.push(Line::styled(
                    format!("  \u{2691} #{} {} {}{detail}", f["n"], f["kind"].as_str().unwrap_or("?"), age(f["age_s"].as_i64().unwrap_or(0))),
                    Style::default().fg(theme::ATTN),
                ));
            }
            lines.push(Line::from(""));
        }
        let mut group: Option<String> = None;
        for (i, item) in b.items().iter().enumerate() {
            let flagged = item["flags"].as_array().is_some_and(|f| !f.is_empty());
            let g = item["group"].as_str().unwrap_or("").to_owned();
            if !flagged && group.as_deref() != Some(g.as_str()) {
                lines.push(Line::styled(if g.is_empty() { "(no group)".to_owned() } else { g.clone() }, bold));
                group = Some(g);
            }
            let mut style = Style::default().fg(if flagged { theme::ATTN } else { theme::TEXT });
            if i == b.cursor {
                style = style.add_modifier(Modifier::REVERSED);
            }
            lines.push(Line::styled(format!("  {}", item_line(item)), style));
            if b.detail && i == b.cursor {
                for e in item["history"].as_array().into_iter().flatten() {
                    lines.push(Line::styled(
                        format!(
                            "      {} {} {}{}",
                            e["at"].as_str().unwrap_or("").get(11..16).unwrap_or(""),
                            e["actor"].as_str().unwrap_or("?"),
                            e["type"].as_str().unwrap_or("?"),
                            e["reason"].as_str().map(|r| format!(" ({r})")).unwrap_or_default()
                        ),
                        dim,
                    ));
                }
            }
        }
        let closed = b.data["recently_closed"].as_array().cloned().unwrap_or_default();
        if !closed.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::styled("Recently closed (24 h)", bold));
            for item in closed.iter().take(10) {
                lines.push(Line::styled(format!("  {}", item_line(item)), dim));
            }
        }
        let footer_h = 2u16;
        let body = Rect { height: inner.height.saturating_sub(footer_h), ..inner };
        // Keep the cursor row in view.
        let cursor_line = lines
            .iter()
            .position(|l| l.style.add_modifier.contains(Modifier::REVERSED))
            .unwrap_or(0) as u16;
        let scroll = cursor_line.saturating_sub(body.height.saturating_sub(3));
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), body);
        let footer = Rect { y: inner.y + body.height, height: footer_h.min(inner.height), ..inner };
        let first = if let Some((kind, text)) = &b.prompt {
            let label = match kind {
                PromptKind::Add => "New item title",
                PromptKind::Note => "Note",
                PromptKind::Reassign | PromptKind::ResolveReassign => "Holder (name)",
                PromptKind::Drop | PromptKind::ResolveDrop => "Reason",
                PromptKind::ResolveBlock => "Blocked on",
            };
            Line::styled(format!("{label}: {text}\u{2588}"), bold)
        } else if let Some(menu) = b.menu {
            Line::styled(
                match menu {
                    Menu::Status => "Status: o open · a active · w waiting · b blocked · d done",
                    Menu::Resolve => "Resolve flag: 1 nudge · 2 reassign · 3 block · 4 drop",
                },
                bold,
            )
        } else if !b.error.is_empty() {
            Line::styled(b.error.clone(), Style::default().fg(theme::ERROR))
        } else {
            Line::styled(b.status.clone(), dim)
        };
        let help = Line::styled(
            "j/k move · Enter history · a add · s status · n note · r reassign · x resolve · d drop · o reopen · Tab board · g refresh · Esc close",
            dim,
        );
        frame.render_widget(Paragraph::new(vec![first, help]), footer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_line_shows_holder_state_flags_blockers_and_note() {
        let item = json!({"n":14,"status":"active","title":"fuse SEJD",
            "holders":[{"pid":"p","name":"rl-scale-out","state":{"state":"idle","for_s":1440}}],
            "flags":[{"kind":"holder_idle"}],"blocked_by":[3],"note":"waiting on JP"});
        assert_eq!(item_line(&item),
            "#14 active   fuse SEJD  @rl-scale-out[idle 24m]  \u{2691}holder_idle  \u{2190}#3 \u{00b7} waiting on JP");
        assert_eq!(item_line(&json!({"n":2,"status":"open","title":"t","holders":[]})), "#2 open     t");
    }

    fn test_app() -> App {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::test_support::home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        app
    }

    fn loaded(app: &mut App) {
        app.board.visible = true;
        app.board.listed = true;
        app.board.last_poll = Some(Instant::now());
        app.board.boards = vec![json!({"slug":"sfd","name":"Swarm Focused Design"}), json!({"slug":"other","name":"Other"})];
        app.board.accept(json!({
            "board":{"slug":"sfd","version":3,"orchestrator":"Swarm-Coord [working]","health":{"unresolved":1,"oldest_s":1500}},
            "flags":[{"n":3,"kind":"holder_idle","age_s":1500,"detail":null}],
            "items":[
                {"n":3,"status":"active","title":"fuse SEJD","group":"perf","flags":[{"kind":"holder_idle"}],
                 "holders":[{"pid":"p","name":"rl","state":{"state":"idle","for_s":1500}}],
                 "history":[{"at":"2026-10-07T01:02:03Z","actor":"rl","type":"status","reason":null}]},
                {"n":1,"status":"waiting","title":"bench run","group":"perf","holders":[]},
                {"n":2,"status":"open","title":"docs","group":"docs","holders":[]}
            ],
            "recently_closed":[{"n":9,"status":"done","title":"old thing","holders":[]}],
            "free_capacity":["lane-b"]
        }));
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn board_renders_flags_first_then_groups_then_closed_strip() {
        let mut app = test_app();
        loaded(&mut app);
        let text = screen(&mut app);
        let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle:?} in\n{text}"));
        assert!(text.contains("Board: Swarm Focused Design (1/2)"));
        assert!(text.contains("orchestrator Swarm-Coord [working] \u{00b7} \u{2691}1 oldest 25m \u{00b7} free: lane-b"));
        assert!(pos("Flags") < pos("\u{2691} #3 holder_idle 25m"));
        assert!(pos("\u{2691} #3 holder_idle 25m") < pos("#3 active"));
        assert!(pos("\u{2502}perf") < pos("#1 waiting"), "group heading before its items");
        assert!(pos("#1 waiting") < pos("\u{2502}docs") && pos("\u{2502}docs") < pos("#2 open"));
        assert!(pos("Recently closed (24 h)") < pos("#9 done"));
        // Enter shows the selected item's history.
        app.board.detail = true;
        assert!(screen(&mut app).contains("01:02 rl status"));
    }

    #[test]
    fn board_keys_prompt_menu_move_switch_and_close() {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        let mut app = test_app();
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        // Alt+B opens over any view and consumes keys.
        assert!(app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::ALT))));
        assert!(app.board.visible && app.is_input_mode());
        loaded(&mut app);
        app.board.rx = None;
        app.handle_event(&key(KeyCode::Char('j')));
        assert_eq!(app.board.selected_n(), Some(1));
        app.handle_event(&key(KeyCode::Char('n')));
        assert_eq!(app.board.prompt, Some((PromptKind::Note, String::new())));
        for c in "hi".chars() { app.handle_event(&key(KeyCode::Char(c))); }
        app.handle_event(&key(KeyCode::Backspace));
        assert_eq!(app.board.prompt, Some((PromptKind::Note, "h".into())));
        assert!(screen(&mut app).contains("Note: h"));
        app.handle_event(&key(KeyCode::Esc));
        assert!(app.board.prompt.is_none() && app.board.visible, "Esc cancels the prompt only");
        app.handle_event(&key(KeyCode::Char('x')));
        assert_eq!(app.board.menu, Some(Menu::Resolve));
        assert!(screen(&mut app).contains("Resolve flag: 1 nudge"));
        app.handle_event(&key(KeyCode::Char('3')));
        assert_eq!(app.board.prompt, Some((PromptKind::ResolveBlock, String::new())));
        app.handle_event(&key(KeyCode::Esc));
        app.handle_event(&key(KeyCode::Tab));
        assert_eq!(app.board.slug().as_deref(), Some("other"));
        assert!(app.board.data.is_null() && app.board.cursor == 0, "switching boards clears the old view");
        app.handle_event(&key(KeyCode::Esc));
        assert!(!app.board.visible);
    }

    #[test]
    fn unchanged_reads_keep_data_and_cursor_is_clamped() {
        let mut b = Board::default();
        b.cursor = 5;
        b.accept(json!({"board":{"version":7},"items":[{"n":1},{"n":2}]}));
        assert_eq!((b.version, b.cursor), (Some(7), 1));
        b.accept(json!({"unchanged":true,"version":7}));
        assert_eq!(b.items().len(), 2);
        assert_eq!(b.selected_n(), Some(2));
    }
}
