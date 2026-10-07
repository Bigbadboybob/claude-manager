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
/// Holder states change without moving the board version, so the poll also
/// re-reads the whole board this often (doc/items-board.md §7).
const FULL_READ: Duration = Duration::from_secs(30);
/// First retry delay after a failed board list; doubles up to `LIST_BACKOFF_MAX`.
const LIST_BACKOFF: Duration = Duration::from_secs(5);
const LIST_BACKOFF_MAX: Duration = Duration::from_secs(120);

/// What a typed line will be used for.
#[derive(Clone, Copy, Debug, PartialEq)]
enum PromptKind {
    Add,
    Note,
    Reassign,
    Drop,
    /// ETA for `s` → `w` (waiting).
    WaitingEta,
    ResolveReassign,
    ResolveBlock,
    /// Check-back time after a `block` resolution's free-text reason.
    ResolveBlockCheck,
    ResolveDrop,
}

/// A one-key menu.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Menu {
    Status,
    Resolve,
}

/// `(op, board slug the request was for, result)`.
type Reply = (String, Option<String>, Result<Value, String>);

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
    /// Writes waiting for the in-flight request: `(method, params)`, sent in
    /// order (each already carries its board).
    queued: std::collections::VecDeque<(&'static str, Value)>,
    last_poll: Option<Instant>,
    last_full: Option<Instant>,
    listed: bool,
    list_retry_at: Option<Instant>,
    list_backoff: Duration,
    /// Free-text reason typed for a `block` resolution, kept while the
    /// check-back prompt is open.
    pending_block: Option<String>,
    /// Free-capacity names expanded under the summary (`f`).
    show_free: bool,
    /// Board to select when the list arrives: the focused session's
    /// `(initiative_id, root_task_id)`.
    preferred: (Option<String>, Option<String>),
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

    fn closed(&self) -> &[Value] {
        self.data["recently_closed"].as_array().map(Vec::as_slice).unwrap_or(&[])
    }

    /// Selectable rows: open items, then the recently-closed strip (so `o`
    /// can reopen a closed item).
    fn rows(&self) -> impl Iterator<Item = &Value> {
        self.items().iter().chain(self.closed().iter().take(CLOSED_SHOWN))
    }

    fn row_count(&self) -> usize {
        self.items().len() + self.closed().len().min(CLOSED_SHOWN)
    }

    fn selected(&self) -> Option<&Value> {
        self.rows().nth(self.cursor)
    }

    fn selected_n(&self) -> Option<i64> {
        self.selected().and_then(|i| i["n"].as_i64())
    }

    /// Apply a `board.read` reply: `{unchanged}` keeps the current data.
    fn accept(&mut self, v: Value) {
        if v["unchanged"] == true {
            return;
        }
        self.version = v["board"]["version"].as_i64();
        self.data = v;
        let n = self.row_count();
        if self.cursor >= n {
            self.cursor = n.saturating_sub(1);
        }
    }
}

/// Recently-closed items shown (and selectable) under the board.
const CLOSED_SHOWN: usize = 10;

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

impl App {
    fn board_request(&mut self, op: &str, method: &'static str, params: Value) {
        if self.board.rx.is_some() {
            return;
        }
        let targets = self.owner_rpc_targets();
        let (tx, rx) = mpsc::channel();
        self.board.rx = Some(rx);
        let op = op.to_owned();
        let tag = params["board"].as_str().map(str::to_owned);
        std::thread::spawn(move || {
            let mut errors = Vec::new();
            for (host, socket, token) in targets {
                match crate::client_session::rpc_messaging_board(&socket, &token, method, params.clone()) {
                    Ok(v) => {
                        let _ = tx.send((op, tag, Ok(v)));
                        return;
                    }
                    Err(e) => errors.push(format!("{host}: {e}")),
                }
            }
            let msg = if errors.is_empty() { "No daemon is reachable".to_owned() } else { errors.join("; ") };
            let _ = tx.send((op, tag, Err(msg)));
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
            let _ = tx.send(("list".into(), None, result));
        });
    }

    fn board_read(&mut self, incremental: bool) {
        let Some(slug) = self.board.slug() else {
            return;
        };
        let mut params = json!({"board": slug, "view": "full"});
        let full_due = self.board.last_full.is_none_or(|t| t.elapsed() >= FULL_READ);
        if incremental && !full_due {
            if let Some(v) = self.board.version {
                params["since_version"] = json!(v);
            }
        } else {
            self.board.last_full = Some(Instant::now());
        }
        self.board.last_poll = Some(Instant::now());
        self.board_request("read", "board.read", params);
    }

    /// Queue a write for the current board; sent in order as soon as no
    /// request is in flight (never dropped behind a poll).
    fn board_write(&mut self, method: &'static str, mut params: Value) {
        if let Some(slug) = self.board.slug() {
            params["board"] = json!(slug);
            self.board.queued.push_back((method, params));
            self.board.status = "Saving…".into();
            self.board_pump();
        }
    }

    fn board_pump(&mut self) {
        if self.board.rx.is_some() {
            return;
        }
        if let Some((method, params)) = self.board.queued.pop_front() {
            self.board_request("write", method, params);
        }
    }

    /// Replies and the visible-only poll. Called every main-loop tick.
    pub fn board_tick(&mut self) {
        if let Some((op, tag, result)) = self.board.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.board.rx = None;
            self.needs_redraw = true;
            // A read for a board the user has since switched away from is
            // stale: drop it (a write's outcome still reports).
            let stale = op == "read" && tag.is_some() && tag != self.board.slug();
            match (op.as_str(), result) {
                _ if stale => {}
                ("list", Err(e)) => {
                    self.board.error = e;
                    self.board.list_backoff = (self.board.list_backoff * 2).clamp(LIST_BACKOFF, LIST_BACKOFF_MAX);
                    self.board.list_retry_at = Some(Instant::now() + self.board.list_backoff);
                }
                (_, Err(e)) => self.board.error = e,
                ("list", Ok(v)) => {
                    self.board.error.clear();
                    self.board.listed = true;
                    self.board.list_backoff = Duration::ZERO;
                    self.board.list_retry_at = None;
                    let keep = self.board.slug();
                    self.board.boards = v.as_array().cloned().unwrap_or_default();
                    // The focused session's board first (its initiative's,
                    // else its root task's), then the board already shown.
                    let (initiative, root) = std::mem::take(&mut self.board.preferred);
                    let preferred = self.board.boards.iter().position(|b| {
                        initiative.as_deref().is_some_and(|i| b["initiative_id"] == i)
                    }).or_else(|| self.board.boards.iter().position(|b| {
                        root.as_deref().is_some_and(|r| b["root_task_id"] == r)
                    }));
                    let previous = self.board.board_idx;
                    self.board.board_idx = preferred
                        .or_else(|| keep.and_then(|s| self.board.boards.iter().position(|b| b["slug"] == s.as_str())))
                        .unwrap_or(0);
                    if self.board.board_idx != previous {
                        self.board.data = Value::Null;
                        self.board.cursor = 0;
                    }
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
                    // Re-read once the queue drains.
                    self.board.last_poll = None;
                }
            }
        }
        self.board_pump();
        if !self.board.visible || self.board.rx.is_some() {
            return;
        }
        if !self.board.listed {
            if self.board.list_retry_at.is_none_or(|t| Instant::now() >= t) {
                self.board_list();
            }
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
            PromptKind::WaitingEta if !text.is_empty() => {
                if let Some(n) = n {
                    self.board_write("item.set", json!({"n": n, "status": "waiting", "eta": text}));
                }
            }
            PromptKind::ResolveBlock if !text.is_empty() => {
                self.board.pending_block = Some(text);
                self.board.prompt = Some((PromptKind::ResolveBlockCheck, String::new()));
            }
            PromptKind::ResolveBlockCheck => {
                if let (Some(n), Some(blocked_on)) = (n, self.board.pending_block.take()) {
                    let mut params = json!({"n": n, "action": "block", "blocked_on": blocked_on});
                    if !text.is_empty() {
                        params["check_back"] = json!(text);
                    }
                    self.board_write("item.resolve", params);
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
            self.board.preferred = self.focused_board_hint();
            self.board.last_poll = None;
            self.board.listed = false;
            self.board_tick();
            return true;
        }
        self.needs_redraw = true;
        if let Some((kind, mut text)) = self.board.prompt.take() {
            match key.code {
                KeyCode::Esc => self.board.pending_block = None,
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
                        'w' => None,
                        'b' => Some("blocked"),
                        // Owner marks an item as waiting on their own decision.
                        'O' => Some("blocked_on_owner"),
                        'd' => Some("done"),
                        _ => None,
                    };
                    if c == 'w' {
                        // Waiting needs an ETA (e.g. 40m, 2h, or a time).
                        self.board.prompt = Some((PromptKind::WaitingEta, String::new()));
                    } else if let Some(status) = status {
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
        let count = self.board.row_count();
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
                let note = self.board.selected().and_then(|i| i["note"].as_str()).unwrap_or("").to_owned();
                self.board.prompt = Some((PromptKind::Note, note));
            }
            KeyCode::Char('r') if has_item => self.board.prompt = Some((PromptKind::Reassign, String::new())),
            KeyCode::Char('x') if has_item => self.board.menu = Some(Menu::Resolve),
            KeyCode::Char('d') if has_item => self.board.prompt = Some((PromptKind::Drop, String::new())),
            KeyCode::Char('o') if has_item => {
                let n = self.board.selected_n().unwrap();
                self.board_write("item.set", json!({"n": n, "status": "active"}));
            }
            KeyCode::Char('f') => self.board.show_free = !self.board.show_free,
            KeyCode::Char('g') => {
                self.board.listed = false;
                self.board.last_poll = None;
                self.board.last_full = None;
                self.board.list_retry_at = None;
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
        self.board.last_full = None;
    }

    /// Board for the focused session's task: its initiative's board, else
    /// its root task's. Matched locally against the board list (resolving
    /// through the API would create a board on first use).
    fn focused_board_hint(&self) -> (Option<String>, Option<String>) {
        let Some(task_id) = self.active_session().and_then(|(_, ts)| ts.task_id.clone()) else {
            return (None, None);
        };
        let initiative = self.planning.task_initiative(&task_id);
        let mut root = task_id;
        for _ in 0..64 {
            match self.tasks.iter().find(|t| t.task_id.as_deref() == Some(root.as_str())).and_then(|t| t.parent_task_id.clone()) {
                Some(parent) => root = parent,
                None => break,
            }
        }
        (initiative, Some(root))
    }

    pub(super) fn draw_board(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let b = &self.board;
        let tint = self.global_settings.tint_strength();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(board_title(b)))
            .border_style(Style::default().fg(theme::TEXT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let width = inner.width as usize;
        let dim = Style::default().fg(theme::DIM);
        let mut lines: Vec<Line> = Vec::new();
        if b.boards.is_empty() {
            lines.push(Line::styled(
                if b.listed { "No boards with open items." } else { "Loading boards…" },
                dim,
            ));
        }
        if b.data["board"].is_object() {
            lines.push(self.board_summary_line(width));
            if let Some(line) = blocked_summary(b.items(), width) {
                lines.push(line);
            }
            if b.show_free {
                let free: Vec<&str> = b.data["free_capacity"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                lines.push(Line::styled(
                    truncate(&format!("  free: {}", free.join(", ")), width),
                    Style::default().fg(theme::MUTED),
                ));
            }
            lines.push(Line::from(""));
        }
        // Flags
        let flags = b.data["flags"].as_array().cloned().unwrap_or_default();
        if !flags.is_empty() {
            lines.push(Line::styled(
                format!("\u{2691} Flags ({})", flags.len()),
                Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD),
            ));
            for f in &flags {
                let detail = match &f["detail"] {
                    Value::Null => String::new(),
                    Value::String(s) => format!("  {s}"),
                    other => format!("  {other}"),
                };
                lines.push(Line::styled(
                    truncate(&format!(
                        "  #{:<3} {:<22} {:>4}{detail}",
                        f["n"],
                        f["kind"].as_str().unwrap_or("?"),
                        age(f["age_s"].as_i64().unwrap_or(0))
                    ), width),
                    Style::default().fg(theme::ERROR),
                ));
            }
            lines.push(Line::from(""));
        }
        // Items: flagged first (already ordered by the daemon), then groups.
        let cols = Columns::for_width(width);
        let now = now_unix();
        let items = b.items();
        let index = item_index(b);
        let mut group: Option<String> = None;
        let mut cursor_line = 0usize;
        for (i, item) in items.iter().enumerate() {
            let flagged = item["flags"].as_array().is_some_and(|f| !f.is_empty());
            let g = item["group"].as_str().unwrap_or("").to_owned();
            if !flagged && group.as_deref() != Some(g.as_str()) {
                lines.push(group_header(&g, items, width, tint));
                group = Some(g);
            }
            if i == b.cursor {
                cursor_line = lines.len();
            }
            lines.push(self.item_row(item, &index, &cols, now, i == b.cursor, false));
        }
        let closed = b.closed();
        if !closed.is_empty() {
            lines.push(Line::from(""));
            lines.push(Line::styled(
                format!("Recently closed (24 h) \u{00b7} {} \u{00b7} o reopens", closed.len()),
                dim.add_modifier(Modifier::BOLD),
            ));
            for (j, item) in closed.iter().take(CLOSED_SHOWN).enumerate() {
                let selected = items.len() + j == b.cursor;
                if selected {
                    cursor_line = lines.len();
                }
                lines.push(self.item_row(item, &index, &cols, now, selected, true));
            }
        }
        // Layout: list, detail pane for the selected row, footer.
        let footer_h = 2u16;
        // The compact pane grows by the selected row's Blocked by / Blocks
        // lines, up to half the overlay.
        let edge_lines = b.selected().map_or(0, |item| detail_edge_lines(item, &index)) as u16;
        let detail_h: u16 = if b.selected().is_none() {
            0
        } else if b.detail {
            (inner.height / 2).max(6 + edge_lines)
        } else {
            (6 + edge_lines).min((inner.height / 2).max(6))
        }
        .min(inner.height.saturating_sub(footer_h + 3));
        let list_h = inner.height.saturating_sub(footer_h + detail_h);
        let list = Rect { height: list_h, ..inner };
        let scroll = (cursor_line as u16).saturating_sub(list_h.saturating_sub(3));
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), list);
        if detail_h > 0 {
            let area = Rect { y: inner.y + list_h, height: detail_h, ..inner };
            self.draw_board_detail(frame, area, &index, now);
        }
        let footer = Rect { y: inner.y + list_h + detail_h, height: footer_h.min(inner.height), ..inner };
        frame.render_widget(Paragraph::new(vec![self.board_status_line(), board_help(width)]), footer);
    }

    /// `● EP · ⚑ 2 oldest 25m · 7 open: 3 active · 2 waiting · 1 blocked · 1 open · free 3 (f)`
    fn board_summary_line(&self, width: usize) -> Line<'static> {
        let b = &self.board;
        let header = &b.data["board"];
        let mut spans: Vec<Span<'static>> = Vec::new();
        let (orch_name, orch_state) = parse_orchestrator(header["orchestrator"].as_str().unwrap_or("none"));
        if orch_name == "none" {
            spans.push(Span::styled("no orchestrator", Style::default().fg(theme::DIM)));
        } else {
            let (glyph, style) = holder_glyph(&orch_state, None, self.spinner_frame());
            spans.push(Span::styled(format!("{glyph} "), style));
            spans.push(Span::styled(orch_name, Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!(" ({orch_state})"), Style::default().fg(theme::DIM)));
        }
        let sep = || Span::styled(" \u{00b7} ", Style::default().fg(theme::DIM));
        let health = &header["health"];
        let unresolved = health["unresolved"].as_i64().unwrap_or(0);
        // Decisions waiting on Owner come first in the banner.
        let owner = health["blocked_on_owner"].as_i64().unwrap_or(0);
        if owner > 0 {
            spans.push(sep());
            let (glyph, style) = self.owner_indicator();
            spans.push(Span::styled(format!("{glyph} {owner} blocked on Owner"), style));
        }
        spans.push(sep());
        if unresolved > 0 {
            let oldest = health["oldest_s"].as_i64().map(|s| format!(" oldest {}", age(s))).unwrap_or_default();
            spans.push(Span::styled(
                format!("\u{2691} {unresolved} flag{}{oldest}", if unresolved == 1 { "" } else { "s" }),
                Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled("no flags", Style::default().fg(theme::OK)));
        }
        let free = b.data["free_capacity"].as_array().map_or(0, Vec::len);
        if free > 0 {
            spans.push(sep());
            spans.push(Span::styled(
                format!("free {free} ({})", if b.show_free { "f hide" } else { "f show" }),
                Style::default().fg(theme::MUTED),
            ));
        }
        spans.push(sep());
        spans.push(Span::styled(status_counts(b.items()), Style::default().fg(theme::MUTED)));
        clip_spans(spans, width)
    }

    /// One aligned row: marker, #n, status badge, ⚑, title…, holder chips,
    /// then the tail: what blocks it, what it blocks, the note.
    fn item_row(
        &self,
        item: &Value,
        index: &ItemIndex,
        cols: &Columns,
        now: i64,
        selected: bool,
        closed: bool,
    ) -> Line<'static> {
        let flagged = item["flags"].as_array().is_some_and(|f| !f.is_empty());
        let (badge, badge_style) = status_badge(item, now);
        let dim = closed;
        let fade = |s: Style| if dim { s.fg(theme::DIM) } else { s };
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(if selected { "\u{25b6} " } else { "  " }, Style::default().fg(theme::TEXT)),
            Span::styled(format!("{:>4} ", format!("#{}", item["n"])), fade(Style::default().fg(theme::MUTED))),
            Span::styled(pad(&badge, cols.badge), fade(badge_style)),
            if flagged {
                Span::styled("\u{2691} ", Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD))
            } else if item["status"] == "blocked_on_owner" && !closed {
                // Just below a flag: the alternating Owner diamond.
                let (glyph, style) = self.owner_indicator();
                Span::styled(format!("{glyph} "), style)
            } else {
                Span::raw("  ")
            },
            Span::styled(
                pad(&truncate(item["title"].as_str().unwrap_or(""), cols.title), cols.title + 1),
                fade(Style::default().fg(theme::TEXT)),
            ),
        ];
        // Holder chips: the sidebar's state glyphs, then the name.
        let mut used = 0usize;
        let holders = item["holders"].as_array().cloned().unwrap_or_default();
        if holders.is_empty() {
            let text = if closed { "" } else { "unassigned" };
            spans.push(Span::styled(pad(text, cols.chips), fade(Style::default().fg(theme::DIM))));
        } else {
            let mut chip_spans = Vec::new();
            for h in &holders {
                let name = h["name"].as_str().or_else(|| h["pid"].as_str()).unwrap_or("?");
                let state = h["state"]["state"].as_str().unwrap_or("unknown");
                let (glyph, style) = holder_glyph(state, h["state"]["for_s"].as_i64(), self.spinner_frame());
                let chip = format!("{glyph} {name} ");
                let w = chip.chars().count();
                if used + w > cols.chips {
                    if used < cols.chips {
                        chip_spans.push(Span::styled("+", Style::default().fg(theme::DIM)));
                        used += 1;
                    }
                    break;
                }
                used += w;
                chip_spans.push(Span::styled(format!("{glyph} "), fade(style)));
                chip_spans.push(Span::styled(format!("{name} "), fade(Style::default().fg(theme::MUTED))));
            }
            spans.extend(chip_spans);
            spans.push(Span::raw(" ".repeat(cols.chips.saturating_sub(used))));
        }
        if cols.note > 0 {
            let mut tail: Vec<Span<'static>> = Vec::new();
            if !closed {
                let reason = blocker_spans(item, index, now, cols.note.saturating_sub(1));
                if !reason.is_empty() {
                    tail.push(Span::raw(" "));
                    tail.extend(reason);
                }
                let blocks = blocks_of(item, index);
                if !blocks.is_empty() {
                    let list: Vec<String> = blocks.iter().map(|n| format!("#{n}")).collect();
                    tail.push(Span::styled(format!(" \u{2192} blocks {}", list.join(" ")), Style::default().fg(theme::DIM)));
                }
            }
            if let Some(note) = item["note"].as_str().filter(|n| !n.is_empty()) {
                tail.push(Span::styled(format!(" {note}"), Style::default().fg(theme::DIM)));
            }
            spans.extend(clip(tail, cols.note));
        }
        let mut line = Line::from(spans);
        if selected {
            line = line.style(Style::default().bg(theme::SELECT_BG));
        }
        line
    }

    fn draw_board_detail(&self, frame: &mut Frame, area: Rect, index: &ItemIndex, now: i64) {
        let Some(item) = self.board.selected() else { return };
        let width = area.width as usize;
        let rule = Line::styled("\u{2500}".repeat(width), Style::default().fg(theme::DIM));
        let (badge, badge_style) = status_badge(item, now);
        let mut lines = vec![
            rule,
            Line::from(vec![
                Span::styled(format!("#{} ", item["n"]), Style::default().fg(theme::MUTED)),
                Span::styled(item["title"].as_str().unwrap_or("").to_owned(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(badge.trim_end().to_owned(), badge_style),
            ]),
        ];
        let mut meta: Vec<String> = Vec::new();
        let holders: Vec<String> = item["holders"].as_array().into_iter().flatten().map(|h| {
            let name = h["name"].as_str().or_else(|| h["pid"].as_str()).unwrap_or("?");
            let state = h["state"]["state"].as_str().unwrap_or("unknown");
            match h["state"]["for_s"].as_i64() {
                Some(s) => format!("{name} ({state} {})", age(s)),
                None => format!("{name} ({state})"),
            }
        }).collect();
        meta.push(if holders.is_empty() { "holders: none".into() } else { format!("holders: {}", holders.join(", ")) });
        if let Some(g) = item["group"].as_str().filter(|g| !g.is_empty()) {
            meta.push(format!("group: {g}"));
        }
        if let Some(eta) = item["eta_at"].as_str() {
            meta.push(format!("eta {}", eta.get(11..16).unwrap_or(eta)));
        }
        lines.push(Line::styled(meta.join(" \u{00b7} "), Style::default().fg(theme::MUTED)));
        lines.extend(self.detail_edges(item, index, now));
        if let Some(note) = item["note"].as_str().filter(|n| !n.is_empty()) {
            lines.push(Line::styled(format!("note: {note}"), Style::default().fg(theme::TEXT)));
        }
        if let Some(links) = item["links"].as_array().filter(|l| !l.is_empty()) {
            lines.push(Line::styled(
                format!("links: {}", links.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("  ")),
                Style::default().fg(theme::HEADER),
            ));
        }
        for e in item["history"].as_array().into_iter().flatten() {
            lines.push(Line::styled(
                format!(
                    "  {} {} {}{}",
                    e["at"].as_str().unwrap_or("").get(11..16).unwrap_or(""),
                    e["actor"].as_str().unwrap_or("?"),
                    e["type"].as_str().unwrap_or("?"),
                    e["reason"].as_str().map(|r| format!(" ({r})")).unwrap_or_default()
                ),
                Style::default().fg(theme::DIM),
            ));
        }
        frame.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), area);
    }

    /// The detail pane's "Blocked by" and "Blocks" sections: each related
    /// item with its badge, holders and ETA, plus the blocked_on text and
    /// check-back time.
    fn detail_edges(&self, item: &Value, index: &ItemIndex, now: i64) -> Vec<Line<'static>> {
        let heading = |text: &str| Line::styled(text.to_owned(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD));
        let mut lines = Vec::new();
        let blocked_by = numbers(&item["blocked_by"]);
        let on = item["blocked_on"].as_str().filter(|s| !s.is_empty());
        if !blocked_by.is_empty() || on.is_some() {
            lines.push(heading("Blocked by"));
            for n in &blocked_by {
                lines.push(self.related_item_line(*n, index, now));
            }
            if let Some(on) = on {
                let owner = item["status"] == "blocked_on_owner";
                let mut spans = vec![
                    Span::raw("  "),
                    if owner {
                        Span::styled("\u{25c6} Owner decision: ", super::owner_blocked::owner_style())
                    } else {
                        Span::styled("\u{27f5} ", Style::default().fg(theme::ATTN))
                    },
                    Span::styled(format!("\"{on}\""), Style::default().fg(theme::TEXT)),
                ];
                if let Some((text, style)) = check_back(item, now) {
                    spans.push(Span::styled(" \u{00b7} ", Style::default().fg(theme::DIM)));
                    spans.push(Span::styled(text, style));
                    if let Some(at) = item["check_back_at"].as_str() {
                        spans.push(Span::styled(format!(" ({})", at.get(11..16).unwrap_or(at)), Style::default().fg(theme::DIM)));
                    }
                }
                lines.push(Line::from(spans));
            }
        }
        let blocks = blocks_of(item, index);
        if !blocks.is_empty() {
            lines.push(heading("Blocks"));
            for n in blocks {
                lines.push(self.related_item_line(n, index, now));
            }
        }
        lines
    }

    /// `  #3 BLOCKED bench run · ◐ bench · eta 21:40` for an item on this board.
    fn related_item_line(&self, n: i64, index: &ItemIndex, now: i64) -> Line<'static> {
        let dim = Style::default().fg(theme::DIM);
        let mut spans = vec![Span::styled(format!("  #{n} "), Style::default().fg(theme::MUTED))];
        let Some(other) = index.get(&n) else {
            spans.push(Span::styled("(not on the board: archived or another board)", dim));
            return Line::from(spans);
        };
        let (badge, style) = status_badge(other, now);
        spans.push(Span::styled(format!("{badge} "), style));
        spans.push(Span::styled(other["title"].as_str().unwrap_or("").to_owned(), Style::default().fg(theme::TEXT)));
        for h in other["holders"].as_array().into_iter().flatten() {
            let name = h["name"].as_str().or_else(|| h["pid"].as_str()).unwrap_or("?").to_owned();
            let state = h["state"]["state"].as_str().unwrap_or("unknown");
            let (glyph, gstyle) = holder_glyph(state, h["state"]["for_s"].as_i64(), self.spinner_frame());
            spans.push(Span::styled(" \u{00b7} ", dim));
            spans.push(Span::styled(format!("{glyph} "), gstyle));
            spans.push(Span::styled(name, Style::default().fg(theme::MUTED)));
        }
        if let Some(eta) = other["eta_at"].as_str() {
            spans.push(Span::styled(format!(" \u{00b7} eta {}", eta.get(11..16).unwrap_or(eta)), dim));
        }
        Line::from(spans)
    }

    fn board_status_line(&self) -> Line<'static> {
        let b = &self.board;
        let bold = Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD);
        if let Some((kind, text)) = &b.prompt {
            let label = match kind {
                PromptKind::Add => "New item title",
                PromptKind::Note => "Note",
                PromptKind::Reassign | PromptKind::ResolveReassign => "Holder (name)",
                PromptKind::Drop | PromptKind::ResolveDrop => "Reason",
                PromptKind::ResolveBlock => "Blocked on",
                PromptKind::ResolveBlockCheck => "Check back (e.g. 2h; empty for none)",
                PromptKind::WaitingEta => "ETA (e.g. 40m, 2h)",
            };
            return Line::styled(format!("{label}: {text}\u{2588}"), bold);
        }
        if let Some(menu) = b.menu {
            return Line::styled(
                match menu {
                    Menu::Status => "Status: o open \u{00b7} a active \u{00b7} w waiting \u{00b7} b blocked \u{00b7} O on Owner \u{00b7} d done",
                    Menu::Resolve => "Resolve flag: 1 nudge \u{00b7} 2 reassign \u{00b7} 3 block \u{00b7} 4 drop",
                },
                bold,
            );
        }
        if !b.error.is_empty() {
            return Line::styled(b.error.clone(), Style::default().fg(theme::ERROR));
        }
        Line::styled(b.status.clone(), Style::default().fg(theme::DIM))
    }
}

/// Column widths for one item row at `width` cells.
struct Columns {
    badge: usize,
    title: usize,
    chips: usize,
    note: usize,
}

impl Columns {
    /// Fixed: marker 2, #n 5, badge, flag 2. The rest splits title / chips /
    /// tail, the tail a bit wider than the title since it carries the blocker;
    /// it disappears on very narrow terminals.
    fn for_width(width: usize) -> Self {
        let badge = 13;
        let rest = width.saturating_sub(2 + 5 + badge + 2);
        let chips = (rest / 5).clamp(12, 30).min(rest.saturating_sub(16));
        let title = (rest.saturating_sub(chips) * 2 / 5).clamp(16, 60).min(rest.saturating_sub(chips + 1));
        let note = rest.saturating_sub(title + 1 + chips);
        Self { badge, title, chips, note: if note >= 10 { note } else { 0 } }
    }
}

/// Open and recently closed items by number, for blocker lookups.
type ItemIndex<'a> = HashMap<i64, &'a Value>;

fn item_index(b: &Board) -> ItemIndex<'_> {
    b.items().iter().chain(b.closed()).filter_map(|i| Some((i["n"].as_i64()?, i))).collect()
}

fn numbers(v: &Value) -> Vec<i64> {
    v.as_array().into_iter().flatten().filter_map(Value::as_i64).collect()
}

/// Open items this one blocks: the API's `blocks`, else computed here.
fn blocks_of(item: &Value, index: &ItemIndex) -> Vec<i64> {
    if item["blocks"].is_array() {
        return numbers(&item["blocks"]);
    }
    let Some(n) = item["n"].as_i64() else { return Vec::new() };
    let mut out: Vec<i64> = index
        .values()
        .filter(|o| !matches!(o["status"].as_str(), Some("done" | "dropped")))
        .filter(|o| numbers(&o["blocked_by"]).contains(&n))
        .filter_map(|o| o["n"].as_i64())
        .collect();
    out.sort_unstable();
    out
}

/// `check back 20m`, or `check back overdue 5m` in red.
fn check_back(item: &Value, now: i64) -> Option<(String, Style)> {
    let at = item["check_back_at"].as_str().and_then(parse_rfc3339)?;
    Some(if at >= now {
        (format!("check back {}", age(at - now)), Style::default().fg(theme::MUTED))
    } else {
        (format!("check back overdue {}", age(now - at)), Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD))
    })
}

/// What blocks a row, within `room` cells: `⟵ #3 bench run (WAIT 12m)`
/// (+N for more blockers), `⟵ "GPU quota" · check back 20m`, or the quoted
/// Owner question. The related title / text is what gets truncated.
fn blocker_spans(item: &Value, index: &ItemIndex, now: i64, room: usize) -> Vec<Span<'static>> {
    let arrow = || Span::styled("\u{27f5} ", Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD));
    let width = |spans: &[Span]| spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
    let blocked_by = numbers(&item["blocked_by"]);
    if let Some(&first) = blocked_by.first() {
        let mut head = vec![arrow(), Span::styled(format!("#{first}"), Style::default().fg(theme::MUTED))];
        let mut after = Vec::new();
        let mut title = String::new();
        if let Some(other) = index.get(&first) {
            let (badge, style) = status_badge(other, now);
            let badge = badge.strip_suffix(" left").unwrap_or(&badge).to_owned();
            title = other["title"].as_str().unwrap_or("").to_owned();
            after.push(Span::styled(format!(" ({badge})"), style));
        }
        if blocked_by.len() > 1 {
            after.push(Span::styled(format!(" +{}", blocked_by.len() - 1), Style::default().fg(theme::ATTN)));
        }
        let left = room.saturating_sub(width(&head) + width(&after) + 1);
        if !title.is_empty() && left >= 4 {
            head.push(Span::styled(format!(" {}", truncate(&title, left)), Style::default().fg(theme::TEXT)));
        }
        head.extend(after);
        return head;
    }
    let status = item["status"].as_str().unwrap_or("");
    let Some(on) = item["blocked_on"].as_str().filter(|s| !s.is_empty()) else { return Vec::new() };
    if !matches!(status, "blocked" | "blocked_on_owner") {
        return Vec::new();
    }
    let (lead, text_style) = if status == "blocked_on_owner" {
        (None, super::owner_blocked::owner_style())
    } else {
        (Some(arrow()), Style::default().fg(theme::TEXT))
    };
    let quoted = format!("\"{on}\"");
    let lead_w = lead.as_ref().map_or(0, |s| s.content.chars().count());
    let mut after = Vec::new();
    if let Some((text, style)) = check_back(item, now) {
        // Tight: `back 20m` / `overdue 5m` rather than cutting the reason.
        let full = 3 + text.chars().count();
        let text = if lead_w + quoted.chars().count() + full <= room {
            text
        } else {
            text.strip_prefix("check back ")
                .map(|t| if t.starts_with("overdue") { t.to_owned() } else { format!("back {t}") })
                .unwrap_or(text)
        };
        after.push(Span::styled(" \u{00b7} ", Style::default().fg(theme::DIM)));
        after.push(Span::styled(text, style));
    }
    // The reason keeps at least half the room; the check-back is clipped first.
    let left = room.saturating_sub(lead_w + width(&after)).max(room.saturating_sub(lead_w) / 2);
    let mut out: Vec<Span<'static>> = lead.into_iter().collect();
    out.push(Span::styled(truncate(&quoted, left.max(4)), text_style));
    out.extend(after);
    clip(out, room)
}

/// `3 blocked: 2 on items · 1 on Owner · 1 external` under the summary.
fn blocked_summary(items: &[Value], width: usize) -> Option<Line<'static>> {
    let (mut on_items, mut owner, mut external) = (0, 0, 0);
    for item in items {
        match item["status"].as_str() {
            Some("blocked_on_owner") => owner += 1,
            Some("blocked") if !numbers(&item["blocked_by"]).is_empty() => on_items += 1,
            Some("blocked") => external += 1,
            _ => {}
        }
    }
    let total = on_items + owner + external;
    if total == 0 {
        return None;
    }
    let parts: Vec<String> = [(on_items, "on items"), (owner, "on Owner"), (external, "external")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, what)| format!("{n} {what}"))
        .collect();
    Some(Line::from(clip(
        vec![
            Span::styled(format!("\u{27f5} {total} blocked"), Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)),
            Span::styled(format!(": {}", parts.join(" \u{00b7} ")), Style::default().fg(theme::MUTED)),
        ],
        width,
    )))
}

/// Lines the detail pane needs for its Blocked by / Blocks sections.
fn detail_edge_lines(item: &Value, index: &ItemIndex) -> usize {
    let by = numbers(&item["blocked_by"]).len() + usize::from(item["blocked_on"].as_str().is_some_and(|s| !s.is_empty()));
    let blocks = blocks_of(item, index).len();
    by + usize::from(by > 0) + blocks + usize::from(blocks > 0)
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// RFC 3339 → Unix seconds (`2026-10-07T19:40:00Z`, fractional seconds and
/// numeric offsets accepted). The TUI carries no date crate.
fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        rest = frac.trim_start_matches(|c: char| c.is_ascii_digit());
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o.get(1..3)?.parse::<i64>().ok()? * 3600 + o.get(4..6)?.parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + sec - offset)
}

/// Colored status badge: ACTIVE green, WAIT cyan with the ETA countdown
/// (red once overdue), BLOCKED yellow, OPEN grey, DONE/DROPPED dim.
fn status_badge(item: &Value, now: i64) -> (String, Style) {
    let bold = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
    match item["status"].as_str().unwrap_or("?") {
        "active" => ("ACTIVE".into(), bold(theme::OK)),
        "waiting" => match item["eta_at"].as_str().and_then(parse_rfc3339) {
            Some(eta) if eta >= now => (format!("WAIT {} left", age(eta - now)), bold(theme::HEADER)),
            Some(eta) => (format!("WAIT +{} over", age(now - eta)), bold(theme::ERROR)),
            None => ("WAITING".into(), bold(theme::HEADER)),
        },
        "blocked" => ("BLOCKED".into(), bold(theme::ATTN)),
        "blocked_on_owner" => ("OWNER".into(), super::owner_blocked::owner_style()),
        "open" => ("OPEN".into(), Style::default().fg(theme::MUTED)),
        "done" => ("DONE".into(), Style::default().fg(theme::DIM)),
        "dropped" => ("DROPPED".into(), Style::default().fg(theme::DIM)),
        // Unknown statuses are shown verbatim, never as closed.
        other => (other.to_uppercase(), Style::default().fg(theme::TEXT)),
    }
}

/// The sidebar's state glyphs for a holder (see app/agent_state.rs).
fn holder_glyph(state: &str, for_s: Option<i64>, spinner: &'static str) -> (&'static str, Style) {
    match state {
        "working" => (spinner, Style::default().fg(theme::OK)),
        "starting" => (spinner, Style::default().fg(theme::DIM)),
        "working-background" => ("\u{25d0}", Style::default().fg(theme::OK).add_modifier(Modifier::DIM)),
        "waiting-on-human" => ("?", Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)),
        "errored" => ("\u{2717}", Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD)),
        "idle" => {
            let color = match for_s {
                Some(s) if s < 120 => theme::AFTERGLOW,
                Some(s) if s >= 1800 => theme::DIM,
                _ => theme::TEXT,
            };
            ("\u{25cf}", Style::default().fg(color))
        }
        "exited" => ("\u{00d7}", Style::default().fg(theme::DIM)),
        _ => ("\u{00b7}", Style::default().fg(theme::DIM)),
    }
}

/// `"EP [working] (set on the board)"` → ("EP", "working").
fn parse_orchestrator(s: &str) -> (String, String) {
    match (s.find(" ["), s.find(']')) {
        (Some(a), Some(b)) if b > a => (s[..a].to_owned(), s[a + 2..b].to_owned()),
        _ => (s.to_owned(), "unknown".to_owned()),
    }
}

/// `"3 active · 1 waiting · 1 open"` over open items, in a fixed order.
fn status_counts(items: &[Value]) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for item in items {
        let s = item["status"].as_str().unwrap_or("?").to_owned();
        match counts.iter_mut().find(|(k, _)| *k == s) {
            Some((_, n)) => *n += 1,
            None => counts.push((s, 1)),
        }
    }
    let order = ["blocked_on_owner", "active", "waiting", "blocked", "open"];
    counts.sort_by_key(|(k, _)| order.iter().position(|o| o == k).unwrap_or(order.len()));
    if counts.is_empty() {
        return "no open items".into();
    }
    let total: usize = counts.iter().map(|(_, n)| n).sum();
    format!(
        "{total} open: {}",
        counts
            .iter()
            .map(|(k, n)| format!("{n} {}", if k == "blocked_on_owner" { "blocked on Owner" } else { k }))
            .collect::<Vec<_>>()
            .join(" \u{00b7} ")
    )
}

/// A group heading with its counts, on the section tint.
fn group_header(group: &str, items: &[Value], width: usize, tint: f64) -> Line<'static> {
    let members: Vec<Value> = items
        .iter()
        .filter(|i| i["group"].as_str().unwrap_or("") == group && i["flags"].as_array().is_none_or(Vec::is_empty))
        .cloned()
        .collect();
    let name = if group.is_empty() { "Ungrouped".to_owned() } else { group.to_owned() };
    let counts = status_counts(&members);
    let counts = counts.split_once(": ").map_or(counts.clone(), |(_, c)| c.to_owned());
    let bg = theme::sidebar_section_bg(Some("blue"), tint);
    let text = format!(" {name}   ");
    let used = text.chars().count() + counts.chars().count();
    Line::from(vec![
        Span::styled(text, Style::default().fg(theme::HEADER).bg(bg).add_modifier(Modifier::BOLD)),
        Span::styled(counts, Style::default().fg(theme::MUTED).bg(bg)),
        Span::styled(" ".repeat(width.saturating_sub(used)), Style::default().bg(bg)),
    ])
}

/// ` Board · SEJD (sejd) · initiative board · 1/3 · Tab next `
fn board_title(b: &Board) -> Vec<Span<'static>> {
    let Some(h) = b.boards.get(b.board_idx) else {
        return vec![Span::raw(" Work board ")];
    };
    let slug = h["slug"].as_str().unwrap_or("?").to_owned();
    let name = h["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(&slug).to_owned();
    let kind = if !h["initiative_id"].is_null() {
        "initiative board"
    } else if !h["root_task_id"].is_null() {
        "task board"
    } else {
        "board"
    };
    let mut spans = vec![
        Span::raw(" Work board \u{00b7} "),
        Span::styled(name.clone(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
    ];
    if name != slug {
        spans.push(Span::styled(format!(" ({slug})"), Style::default().fg(theme::MUTED)));
    }
    spans.push(Span::styled(format!(" \u{00b7} {kind}"), Style::default().fg(theme::MUTED)));
    if b.boards.len() > 1 {
        spans.push(Span::styled(
            format!(" \u{00b7} board {}/{} \u{00b7} Tab next ", b.board_idx + 1, b.boards.len()),
            Style::default().fg(theme::HEADER),
        ));
    } else {
        spans.push(Span::raw(" "));
    }
    spans
}

fn board_help(width: usize) -> Line<'static> {
    let full = "j/k move \u{00b7} Enter more detail \u{00b7} a add \u{00b7} s status \u{00b7} n note \u{00b7} r reassign \u{00b7} x resolve \u{00b7} d drop \u{00b7} o reopen \u{00b7} f free \u{00b7} Tab board \u{00b7} g refresh \u{00b7} Esc close";
    let short = "j/k \u{00b7} Enter \u{00b7} a s n r x d o \u{00b7} f \u{00b7} Tab \u{00b7} g \u{00b7} Esc";
    Line::styled(if full.chars().count() <= width { full } else { short }, Style::default().fg(theme::DIM))
}

/// Cut to `max` cells with a one-cell `…`.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('\u{2026}');
    out
}

fn pad(s: &str, width: usize) -> String {
    let n = s.chars().count();
    if n >= width { s.to_owned() } else { format!("{s}{}", " ".repeat(width - n)) }
}

/// Keep spans within `width` cells (the summary line on narrow terminals).
fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Line<'static> {
    Line::from(clip(spans, width))
}

fn clip(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let n = span.content.chars().count();
        if used + n <= width {
            used += n;
            out.push(span);
        } else {
            let room = width.saturating_sub(used);
            if room > 1 {
                out.push(Span::styled(truncate(&span.content, room), span.style));
            }
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn iso(offset_s: i64) -> String {
        // RFC 3339 UTC for now + offset (civil-from-days, Howard Hinnant).
        let t = now_unix() + offset_s;
        let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", secs / 3600, secs % 3600 / 60, secs % 60)
    }

    fn loaded(app: &mut App) {
        app.board.visible = true;
        app.board.listed = true;
        app.board.last_poll = Some(Instant::now());
        app.board.last_full = Some(Instant::now());
        app.board.boards = vec![
            json!({"slug":"test-fake-board","name":"SEJD","initiative_id":"ini-1","root_task_id":null}),
            json!({"slug":"other","name":"other","initiative_id":null,"root_task_id":"t-root"}),
        ];
        app.board.accept(json!({
            "board":{"slug":"test-fake-board","version":3,"orchestrator":"EP [working] (set on the board)","health":{"unresolved":1,"oldest_s":1500}},
            "flags":[{"n":3,"kind":"holder_idle","age_s":1500,"detail":null}],
            "items":[
                {"n":3,"status":"active","title":"fuse SEJD hot operators across the whole training pipeline","group":"RL training","flags":[{"kind":"holder_idle"}],
                 "holders":[{"pid":"p","name":"rl","state":{"state":"idle","for_s":1500}}],
                 "note":"profiling shows the fused kernel saves 18% but the backward pass still dominates; next try the custom allreduce",
                 "history":[{"at":"2026-10-07T01:02:03Z","actor":"rl","type":"status","reason":null}]},
                {"n":1,"status":"waiting","title":"bench run","group":"RL training","eta_at": iso(1920),
                 "holders":[{"pid":"q","name":"bench","state":{"state":"working-background","for_s":300}}]},
                {"n":4,"status":"blocked","title":"needs EP GO","group":"RL training","blocked_on":"EP GO",
                 "holders":[{"pid":"r","name":"lane-c","state":{"state":"waiting-on-human","for_s":60}}]},
                {"n":2,"status":"open","title":"docs","group":"Docs","holders":[]}
            ],
            "recently_closed":[{"n":9,"status":"done","title":"old thing","holders":[]}],
            "free_capacity":["lane-b","lane-d"]
        }));
    }

    fn screen(app: &mut App) -> String {
        screen_at(app, 140, 30)
    }

    fn screen_at(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn blocked_on_owner_gets_badge_diamond_banner_and_counts_first() {
        for width in [100u16, 160] {
            let mut app = test_app();
            loaded(&mut app);
            let mut data = app.board.data.clone();
            data["board"]["health"]["blocked_on_owner"] = json!(1);
            data["items"][2]["status"] = json!("blocked_on_owner");
            data["items"][2]["blocked_on"] = json!("approve 8xH100?");
            app.board.accept(data);
            let out = screen_at(&mut app, width, 30);
            assert!(out.contains("1 blocked on Owner"), "banner:\n{out}");
            let row = out.lines().find(|l| l.contains("needs EP GO")).expect("row");
            assert!(row.contains("OWNER"), "{row}");
            assert!(row.contains('\u{25c7}') || row.contains('\u{25c6}'), "diamond cell: {row}");
            assert!(out.contains("1 blocked on Owner \u{00b7} "), "counts list it first:\n{out}");
        }
        let (badge, style) = status_badge(&json!({"status": "blocked_on_owner"}), 0);
        assert_eq!(badge, "OWNER");
        assert_eq!(style.fg, Some(theme::OWNER_BLOCKED));
    }

    /// The `loaded` board plus a blocker graph: #4 waits on #1, #5 on an
    /// external approval, #6 on an Owner decision (check-back overdue), #7 on
    /// #1 and #3.
    fn blockers(app: &mut App) {
        loaded(app);
        let mut data = app.board.data.clone();
        let items = data["items"].as_array_mut().unwrap();
        items[2]["blocked_by"] = json!([1]);
        items[2]["blocked_on"] = Value::Null;
        items.push(json!({"n":5,"status":"blocked","title":"scale to 64 GPUs","group":"Docs","blocked_on":"GPU quota approval",
            "check_back_at": iso(1200),"holders":[]}));
        items.push(json!({"n":6,"status":"blocked_on_owner","title":"allocator","group":"Docs","blocked_on":"pick allocator variant",
            "check_back_at": iso(-300),"holders":[]}));
        items.push(json!({"n":7,"status":"blocked","title":"final eval","group":"Docs","blocked_by":[1,3],"holders":[]}));
        app.board.accept(data);
    }

    fn row<'a>(text: &'a str, n: &str) -> &'a str {
        text.lines().find(|l| l.contains(&format!("   #{n} "))).unwrap_or_else(|| panic!("no row #{n}:\n{text}"))
    }

    #[test]
    fn blockers_show_inline_with_reverse_edges_and_detail_sections() {
        for width in [100u16, 160] {
            let mut app = test_app();
            blockers(&mut app);
            app.board.cursor = 2; // #4
            let text = screen_at(&mut app, width, 40);
            if std::env::var_os("CM_BOARD_DUMP").is_some() {
                println!("--- {width} columns ---\n{text}");
            }
            // Inline: the blocking item, its status and ETA.
            let r4 = row(&text, "4");
            assert!(r4.contains("\u{27f5} #1 bench run (WAIT 3"), "{r4}");
            assert!(!r4.contains("left)"), "the countdown is compact: {r4}");
            // External blocker with its check-back; Owner question quoted.
            // Narrow terminals shorten the check-back wording, never the reason.
            let (back, overdue) = if width >= 160 { ("check back 2", "check back overdue 5m") } else { ("back 2", "overdue 5m") };
            let r5 = row(&text, "5");
            assert!(r5.contains(&format!("\u{27f5} \"GPU quota approval\" \u{00b7} {back}")), "{r5}");
            let r6 = row(&text, "6");
            assert!(r6.contains("OWNER") && r6.contains("\"pick allocator vari"), "{r6}");
            assert!(r6.contains(overdue), "{r6}");
            // Several blockers: the first, then +N.
            assert!(row(&text, "7").contains("\u{27f5} #1") && row(&text, "7").contains("+1"), "{text}");
            // Reverse edges, computed client-side when the read lacks them.
            assert!(row(&text, "1").contains("\u{2192} blocks #4 #7"), "{text}");
            assert!(row(&text, "3").contains("\u{2192} blocks #7"), "{text}");
            // Summary of what the blocked items wait on.
            assert!(text.contains("\u{27f5} 4 blocked: 2 on items \u{00b7} 1 on Owner \u{00b7} 1 external"), "{text}");
            // Detail pane for #4: Blocked by with badge, holders and ETA.
            let at = text.find("Blocked by").expect("detail section");
            let detail = &text[at..];
            assert!(detail.contains("#1 WAIT 3") && detail.contains("bench run \u{00b7} \u{25d0} bench \u{00b7} eta "), "{detail}");
        }
        // #6: the Owner decision and its check-back time; #1: what it blocks.
        let mut app = test_app();
        blockers(&mut app);
        app.board.cursor = 5;
        let text = screen_at(&mut app, 160, 40);
        let detail = &text[text.find("Blocked by").expect("section")..];
        assert!(detail.contains("\u{25c6} Owner decision: \"pick allocator variant\" \u{00b7} check back overdue 5m ("), "{detail}");
        app.board.cursor = 1;
        let text = screen_at(&mut app, 160, 40);
        let detail = &text[text.find("Blocks").expect("section")..];
        assert!(detail.contains("#4 BLOCKED needs EP GO \u{00b7} ? lane-c") && detail.contains("#7 BLOCKED final eval"), "{detail}");
        // The API's own reverse edges win over the client-side computation.
        let mut data = app.board.data.clone();
        data["items"][1]["blocks"] = json!([4]);
        app.board.accept(data);
        assert!(row(&screen_at(&mut app, 160, 40), "1").contains("\u{2192} blocks #4 "));
    }

    #[test]
    fn blocker_text_truncates_the_title_not_the_status() {
        let mut app = test_app();
        blockers(&mut app);
        let index = item_index(&app.board);
        let item = json!({"n":8,"status":"blocked","blocked_by":[3,1]});
        let text: String = blocker_spans(&item, &index, now_unix(), 30).iter().map(|s| s.content.to_string()).collect();
        assert!(text.starts_with("\u{27f5} #3 fuse") && text.ends_with("\u{2026} (ACTIVE) +1"), "{text}");
        assert!(text.chars().count() <= 30, "{text}");
        let gone = json!({"n":8,"status":"blocked","blocked_by":[99]});
        let text: String = blocker_spans(&gone, &index, now_unix(), 30).iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "\u{27f5} #99", "an archived blocker is still named");
    }

    #[test]
    fn board_renders_title_summary_badges_chips_columns_and_groups() {
        for width in [100u16, 160] {
            let mut app = test_app();
            loaded(&mut app);
            let text = screen_at(&mut app, width, 34);
            if std::env::var_os("CM_BOARD_DUMP").is_some() {
                println!("--- {width} columns ---\n{text}");
            }
            let pos = |needle: &str| text.find(needle).unwrap_or_else(|| panic!("missing {needle:?} at {width}:\n{text}"));
            // Title: name, slug, kind, which board of how many.
            assert!(text.contains("Work board \u{00b7} SEJD (test-fake-board) \u{00b7} initiative board \u{00b7} board 1/2 \u{00b7} Tab next"));
            // Summary: orchestrator with state, flags, counts, free collapsed to a count.
            assert!(text.contains("EP (working)"));
            assert!(text.contains("\u{2691} 1 flag oldest 25m"));
            assert!(text.contains("free 2 (f show)"));
            if width >= 160 {
                assert!(text.contains("4 open: 1 active \u{00b7} 1 waiting \u{00b7} 1 blocked \u{00b7} 1 open"));
            }
            assert!(!text.contains("lane-b"), "free capacity collapsed to a count");
            // Flags first, then group headers with counts.
            assert!(pos("\u{2691} Flags (1)") < pos("#3 "));
            assert!(text.contains(" RL training   1 waiting \u{00b7} 1 blocked"));
            assert!(pos(" RL training") < pos(" Docs"));
            // Badges with the ETA countdown, chips with the sidebar glyphs.
            assert!(text.contains("ACTIVE") && text.contains("BLOCKED") && text.contains("OPEN"));
            assert!(text.contains("WAIT 3") && text.contains("left"), "ETA countdown");
            assert!(text.contains("\u{25cf} rl") && text.contains("\u{25d0} bench") && text.contains("? lane-c"));
            assert!(text.contains("unassigned"));
            // One line per item; the full note lives in the detail pane.
            assert!(text.contains("\u{2026}"));
            assert!(text.contains("note: profiling shows the fused kernel"));
            // Columns align: the badge starts at the same cell on every item row.
            let starts: Vec<usize> = text
                .lines()
                // Item rows are indented ("▶   #3" / "    #1"); the detail
                // pane's "#3 title" line and the flags list are not.
                .filter(|l| ["   #3 ", "   #1 ", "   #4 ", "   #2 "].iter().any(|n| l.contains(n)))
                .filter_map(|l| {
                    let chars: Vec<char> = l.chars().collect();
                    ["ACTIVE", "WAIT", "BLOCKED", "OPEN"].iter().filter_map(|b| {
                        let b: Vec<char> = b.chars().collect();
                        chars.windows(b.len()).position(|w| w == b.as_slice())
                    }).min()
                })
                .collect();
            assert_eq!(starts.len(), 4, "{text}");
            assert!(starts.windows(2).all(|w| w[0] == w[1]), "badge column misaligned: {starts:?}");
        }
    }

    #[test]
    fn free_capacity_expands_and_the_focused_sessions_board_is_preferred() {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        let mut app = test_app();
        loaded(&mut app);
        app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(screen_at(&mut app, 120, 30).contains("free: lane-b, lane-d"));
        // The list reply selects the board whose root task matches the hint.
        app.board.preferred = (None, Some("t-root".into()));
        let tx = busy(&mut app);
        let boards = json!(app.board.boards.clone());
        tx.send(("list".into(), None, Ok(boards))).unwrap();
        app.board_tick();
        assert_eq!(app.board.slug().as_deref(), Some("other"));
        // ... and the initiative match wins over the root task.
        app.board.preferred = (Some("ini-1".into()), Some("t-root".into()));
        let tx = busy(&mut app);
        tx.send(("list".into(), None, Ok(json!(app.board.boards.clone())))).unwrap();
        app.board_tick();
        assert_eq!(app.board.slug().as_deref(), Some("test-fake-board"));
    }

    #[test]
    fn badges_glyphs_columns_and_time_parsing() {
        assert_eq!(parse_rfc3339("1970-01-02T00:00:00Z"), Some(86_400));
        assert_eq!(parse_rfc3339("2026-10-07T19:40:00.123+02:00"), parse_rfc3339("2026-10-07T17:40:00Z"));
        assert_eq!(parse_rfc3339("not a time"), None);
        let now = parse_rfc3339("2026-10-07T12:00:00Z").unwrap();
        let badge = |v: Value| status_badge(&v, now).0;
        assert_eq!(badge(json!({"status":"waiting","eta_at":"2026-10-07T12:40:00Z"})), "WAIT 40m left");
        assert_eq!(badge(json!({"status":"waiting","eta_at":"2026-10-07T11:00:00Z"})), "WAIT +1h over");
        assert_eq!(status_badge(&json!({"status":"waiting","eta_at":"2026-10-07T11:00:00Z"}), now).1.fg, Some(theme::ERROR));
        assert_eq!(status_badge(&json!({"status":"active"}), now).1.fg, Some(theme::OK));
        assert_eq!(status_badge(&json!({"status":"blocked"}), now).1.fg, Some(theme::ATTN));
        assert_eq!(badge(json!({"status":"someday"})), "SOMEDAY", "unknown statuses are shown verbatim");
        assert_eq!(holder_glyph("working-background", None, "x").0, "\u{25d0}");
        assert_eq!(holder_glyph("bogus", None, "x").0, "\u{00b7}", "an unknown state is never idle");
        assert_eq!(parse_orchestrator("EP [idle] (set on the board)"), ("EP".into(), "idle".into()));
        let narrow = Columns::for_width(98);
        let wide = Columns::for_width(158);
        assert!(wide.note > narrow.note && wide.title > narrow.title, "wide terminals give the extra room to title and note");
        assert_eq!(Columns::for_width(60).note, 0, "very narrow terminals drop the note column");
        assert!(2 + 5 + wide.badge + 2 + wide.title + 1 + wide.chips + wide.note <= 158);
        assert!(2 + 5 + narrow.badge + 2 + narrow.title + 1 + narrow.chips <= 98);
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

    /// Hold the request slot with a reply that never comes, so nothing in a
    /// test spawns a real RPC.
    fn busy(app: &mut App) -> mpsc::Sender<Reply> {
        let (tx, rx) = mpsc::channel();
        app.board.rx = Some(rx);
        tx
    }

    #[test]
    fn review_fixes_backoff_queue_stale_replies_prompts_and_closed_rows() {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let mut app = test_app();
        // 1. A failed list backs off instead of re-listing every tick.
        app.board.visible = true;
        let tx = busy(&mut app);
        tx.send(("list".into(), None, Err("api down".into()))).unwrap();
        app.board_tick();
        let retry = app.board.list_retry_at.expect("backoff scheduled");
        assert!(retry > Instant::now() + Duration::from_secs(4));
        assert!(app.board.rx.is_none() && !app.board.listed, "no immediate re-list");
        assert_eq!(app.board.error, "api down");
        // 2-3. Writes queue behind an in-flight request; a late read for the
        // board we left is discarded.
        loaded(&mut app);
        let tx = busy(&mut app);
        app.board_write("item.set", json!({"n": 3, "note": "a"}));
        app.board_write("item.set", json!({"n": 1, "note": "b"}));
        assert_eq!(app.board.queued.len(), 2, "nothing dropped while busy");
        assert_eq!(app.board.queued[0].1["board"], "test-fake-board");
        app.handle_event(&key(KeyCode::Tab));
        tx.send(("read".into(), Some("test-fake-board".into()), Ok(json!({"board":{"version":9},"items":[{"n":42}]})))).unwrap();
        app.board.rx.as_ref().unwrap();
        let reply = app.board.rx.as_ref().unwrap().try_recv().unwrap();
        let (tx2, rx2) = mpsc::channel();
        app.board.rx = Some(rx2);
        tx2.send(reply).unwrap();
        let _hold = busy_after_tick(&mut app);
        assert!(app.board.data.is_null(), "stale read for the previous board ignored");
        assert_eq!(app.board.queued.len(), 1, "the queue drains one write per free slot");
        // 5. s -> w asks for an ETA; x -> 3 asks for blocked_on, then check-back.
        app.board.board_idx = 0;
        loaded(&mut app);
        let _hold = busy(&mut app);
        app.board.queued.clear();
        app.handle_event(&key(KeyCode::Char('s')));
        app.handle_event(&key(KeyCode::Char('w')));
        assert_eq!(app.board.prompt, Some((PromptKind::WaitingEta, String::new())));
        for c in "40m".chars() { app.handle_event(&key(KeyCode::Char(c))); }
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(app.board.queued.back().unwrap().1, json!({"n":3,"status":"waiting","eta":"40m","board":"test-fake-board"}));
        app.handle_event(&key(KeyCode::Char('x')));
        app.handle_event(&key(KeyCode::Char('3')));
        for c in "EP GO".chars() { app.handle_event(&key(KeyCode::Char(c))); }
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(app.board.prompt, Some((PromptKind::ResolveBlockCheck, String::new())));
        for c in "2h".chars() { app.handle_event(&key(KeyCode::Char(c))); }
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(app.board.queued.back().unwrap().1,
            json!({"n":3,"action":"block","blocked_on":"EP GO","check_back":"2h","board":"test-fake-board"}));
        // 6. The closed strip is selectable, so `o` can reopen it.
        for _ in 0..5 { app.handle_event(&key(KeyCode::Char('j'))); }
        assert_eq!(app.board.selected_n(), Some(9));
        app.handle_event(&key(KeyCode::Char('o')));
        assert_eq!(app.board.queued.back().unwrap().1, json!({"n":9,"status":"active","board":"test-fake-board"}));
    }

    /// Run one tick, then hold the slot again so the drained write is not sent.
    fn busy_after_tick(app: &mut App) -> Option<mpsc::Sender<Reply>> {
        app.board_tick();
        // board_tick may have pumped a queued write into a real request;
        // replace it with a held slot (the popped write stays popped).
        Some(busy(app))
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
