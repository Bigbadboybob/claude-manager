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

mod epics;
pub(super) use epics::DRow;

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
    /// `g` on a group (board/epics.rs).
    GroupRename,
    GroupNew,
    GroupPoc,
}

/// A one-key menu.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Menu {
    Status,
    Resolve,
    /// Close an epic as `d` done or `c` cancelled (board/epics.rs).
    Close,
}

/// What a prompt or menu acts on, fixed when it opens: a poll that moves
/// rows while Owner types must not redirect the write to another item or
/// group (or send that group's revision).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Target {
    pub n: Option<i64>,
    pub group_id: Option<String>,
    pub revision: Value,
    pub group_name: String,
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
    list_rows: std::cell::Cell<usize>,
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
    /// Folded groups (S3 boards), by group id.
    folds: HashSet<String>,
    /// `g` on a group: the one-key group menu is open.
    group_menu: bool,
    /// The row the open prompt or menu acts on.
    pub(super) target: Option<Target>,
    /// An epic's close plan under review (`g` → `c`), board/epics.rs.
    plan: Option<epics::ClosePlan>,
    /// The epic whose close plan was asked for: `(name, outcome)`.
    pending_close: Option<(String, String)>,
    seq: u64,
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

    fn row_count(&self) -> usize {
        self.display_rows().len()
    }

    /// The selected item (open or recently closed); `None` on a group row.
    fn selected(&self) -> Option<&Value> {
        match self.display_rows().get(self.cursor)? {
            DRow::Item { idx, .. } => self.items().get(*idx),
            DRow::Closed(j) => self.closed().get(*j),
            DRow::Group { .. } => None,
        }
    }

    fn selected_n(&self) -> Option<i64> {
        self.selected().and_then(|i| i["n"].as_i64())
    }

    /// A strict board (`backlog_allowed: false`, doc/strict-boards.md)
    /// refuses waiting and open: `w` declares a job's ETA on the active item
    /// instead, and `o` is not offered.
    fn strict(&self) -> bool {
        self.data["board"]["backlog_allowed"] == false
    }

    /// Apply a `board.read` reply: `{unchanged}` keeps the current data.
    fn accept(&mut self, v: Value) {
        if v["unchanged"] == true {
            if let Some(limits) = v.get("account_limits") {
                self.data["account_limits"] = limits.clone();
            }
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

pub(super) fn age(secs: i64) -> String {
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
            // A group/epic/placement write to an older daemon or API says so.
            let r = super::org_model::owner_rpc(&targets, method, &params).map_err(|f| {
                if f.unsupported { format!("{method} is not supported by this daemon or planning API yet") } else { f.message }
            });
            let _ = tx.send((op, tag, r));
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
            // The close plan's own reply (and error) is told apart.
            let op = if method == "epic.close_plan" { "close_plan" } else { "write" };
            self.board_request(op, method, params);
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
                (_, Err(e)) => {
                    if op == "close_plan" {
                        self.board.pending_close = None;
                    }
                    // A group edited elsewhere since our read: re-read it so
                    // the next edit carries the new revision.
                    let reread = op != "read" && op != "list" && e.contains("revision_changed");
                    self.board.error = e;
                    if reread {
                        self.board.version = None;
                        self.board_read(false);
                    }
                }
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
                (_, Ok(v)) => {
                    self.board.error.clear();
                    self.board.status = "Saved".into();
                    self.board_write_reply(&v);
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

    /// The selected row as a prompt or menu target.
    fn board_target(&self) -> Target {
        let b = &self.board;
        let group_id = b.selected_group();
        let g = group_id.as_deref().and_then(|id| b.group(id));
        Target {
            n: b.selected_n(),
            revision: g.map_or(Value::Null, |g| g["revision"].clone()),
            group_name: g.and_then(|g| g["name"].as_str()).unwrap_or("").to_owned(),
            group_id,
        }
    }

    fn board_submit_prompt(&mut self, kind: PromptKind, text: String) {
        let text = text.trim().to_owned();
        let n = self.board.target.as_ref().and_then(|t| t.n);
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
                    let status = if self.board.strict() { "active" } else { "waiting" };
                    self.board_write("item.set", json!({"n": n, "status": status, "eta": text}));
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
            PromptKind::GroupRename | PromptKind::GroupNew | PromptKind::GroupPoc => self.board_group_submit(kind, text),
            _ => {}
        }
    }

    /// Alt+B toggles; while visible every key is consumed. A prompt or menu
    /// that opens takes its target row then (`Target`).
    pub(super) fn board_event(&mut self, event: &CrosstermEvent) -> bool {
        let open = |b: &Board| b.prompt.is_some() || b.menu.is_some() || b.group_menu;
        let before = open(&self.board);
        let consumed = self.board_event_inner(event);
        if !open(&self.board) {
            self.board.target = None;
        } else if !before {
            self.board.target = Some(self.board_target());
        }
        consumed
    }

    fn board_event_inner(&mut self, event: &CrosstermEvent) -> bool {
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
        // Prompts and one-key menus must never treat a paging chord as an
        // unmodified letter (A-d could otherwise mark an item done).
        if key.modifiers.contains(KeyModifiers::ALT) && matches!(key.code, KeyCode::Char('u' | 'd' | 'U' | 'D'))
            && (self.board.prompt.is_some() || self.board.menu.is_some() || self.board.group_menu)
        {
            return true;
        }
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
        if self.board.plan.is_some() {
            if alt_b {
                self.board.visible = false;
            } else {
                self.board_plan_event(key);
            }
            return true;
        }
        if std::mem::take(&mut self.board.group_menu) {
            if let KeyCode::Char(c) = key.code {
                self.board_group_key(c);
            }
            return true;
        }
        if let Some(menu) = self.board.menu.take() {
            let n = self.board.target.as_ref().and_then(|t| t.n);
            if menu == Menu::Close {
                if let KeyCode::Char(c @ ('d' | 'c')) = key.code {
                    self.board_close_epic(if c == 'd' { "done" } else { "cancelled" });
                }
                return true;
            }
            match (menu, key.code, n) {
                (Menu::Status, KeyCode::Char(c), Some(n)) => {
                    let strict = self.board.strict();
                    let status = match c {
                        'o' if !strict => Some("open"),
                        'a' => Some("active"),
                        'w' => None,
                        'b' => Some("blocked"),
                        // Owner marks an item as waiting on their own decision.
                        'O' => Some("blocked_on_owner"),
                        'd' => Some("done"),
                        _ => None,
                    };
                    if c == 'w' {
                        // Waiting needs an ETA (e.g. 40m, 2h, or a time); on a
                        // strict board the ETA is a job's on the active item.
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
                // A suggested transition (a DONE/STOP post): confirm or reject.
                (Menu::Resolve, KeyCode::Char('5'), Some(n)) => {
                    // Confirm exactly the target shown; the API refuses a
                    // suggestion that changed since.
                    let to = self.board.data["flags"].as_array().into_iter().flatten()
                        .find(|f| f["n"] == n && f["kind"] == "suggested_transition")
                        .and_then(|f| f["detail"]["to"].as_str().map(str::to_owned));
                    match to {
                        Some(to) => self.board_write("item.resolve", json!({"n": n, "action": "apply", "to": to})),
                        None => self.board.status = format!("#{n} has no suggested transition"),
                    }
                }
                (Menu::Resolve, KeyCode::Char('6'), Some(n)) => {
                    self.board_write("item.resolve", json!({"n": n, "action": "dismiss"}))
                }
                _ => {}
            }
            return true;
        }
        let count = self.board.row_count();
        let has_item = self.board.selected_n().is_some();
        let half_page = key.modifiers == KeyModifiers::ALT && matches!(key.code, KeyCode::Char('u' | 'd'));
        if half_page {
            let step = (self.board.list_rows.get() / 2).max(1);
            self.board.cursor = if key.code == KeyCode::Char('u') {
                self.board.cursor.saturating_sub(step)
            } else {
                self.board.cursor.saturating_add(step).min(count.saturating_sub(1))
            };
            return true;
        }
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
            // On a group row (S3 boards): fold it, or open the group menu.
            KeyCode::Char(' ') if self.board.selected_group().is_some() => {
                let id = self.board.selected_group().unwrap();
                if !self.board.folds.remove(&id) {
                    self.board.folds.insert(id);
                }
            }
            KeyCode::Char('g') if self.board.selected_group().is_some() => self.board.group_menu = true,
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
            // Unlaunched work beside who could take it (doc/strict-boards.md §4.2).
            if let Some(line) = cm_daemon::items::rpc::launch_line_now(&b.data["launch"]) {
                lines.push(Line::styled(
                    truncate(&format!("  \u{25b6} {line}"), width),
                    Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD),
                ));
            }
            if b.show_free {
                let free: Vec<&str> = b.data["free_capacity"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                lines.push(Line::styled(
                    truncate(&format!("  free: {}", free.join(", ")), width),
                    Style::default().fg(theme::MUTED),
                ));
                let untracked: Vec<&str> = b.data["untracked"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if !untracked.is_empty() {
                    lines.push(Line::styled(
                        truncate(&format!("  untracked (working, no item): {}", untracked.join(", ")), width),
                        Style::default().fg(theme::MUTED),
                    ));
                }
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
        // Items: flagged first (already ordered by the daemon), then groups:
        // free-text headings, or on an S3 board the heading/epic tree.
        let items = b.items();
        let gated = items.iter().chain(b.closed()).any(|i| item_gate(i).is_some());
        let cols = Columns::with_gate(width, gated);
        let now = now_unix();
        let index = item_index(b);
        let mut group: Option<String> = None;
        let mut cursor_line = 0usize;
        let closed = b.closed();
        let mut closed_heading = false;
        for (r, row) in b.display_rows().iter().enumerate() {
            let selected = r == b.cursor;
            if selected {
                cursor_line = lines.len();
            }
            match row {
                DRow::Group { id, depth } => {
                    lines.push(self.board_group_row(id, *depth, width, tint, selected));
                    group = None;
                }
                DRow::Item { idx, depth, loose } => {
                    let item = &items[*idx];
                    let flagged = item["flags"].as_array().is_some_and(|f| !f.is_empty());
                    let g = item["group"].as_str().unwrap_or("").to_owned();
                    if *loose && !flagged && group.as_deref() != Some(g.as_str()) {
                        if selected {
                            cursor_line += 1;
                        }
                        lines.push(group_header(&g, items, width, tint));
                        group = Some(g);
                    }
                    let mut line = self.item_row(item, &index, &cols, now, selected, false);
                    if *depth > 0 {
                        line.spans.insert(0, Span::raw("  ".repeat(*depth)));
                    }
                    lines.push(line);
                }
                DRow::Closed(j) => {
                    if !closed_heading {
                        closed_heading = true;
                        lines.push(Line::from(""));
                        lines.push(Line::styled(
                            format!("Recently closed (24 h) \u{00b7} {} \u{00b7} o reopens", closed.len()),
                            dim.add_modifier(Modifier::BOLD),
                        ));
                        if selected {
                            cursor_line += 2;
                        }
                    }
                    lines.push(self.item_row(&closed[*j], &index, &cols, now, selected, true));
                }
            }
        }
        // Layout: list, detail pane for the selected row, footer.
        let footer_h = 2u16;
        // Reserve enough for the largest compact detail across the board, even
        // on group rows. Only board data, terminal size or Enter can resize it.
        let mut reverse_counts: HashMap<i64, usize> = HashMap::new();
        for item in items.iter().chain(closed.iter()).filter(|i| !matches!(i["status"].as_str(), Some("done" | "dropped"))) {
            for n in numbers(&item["blocked_by"]) { *reverse_counts.entry(n).or_default() += 1; }
        }
        let edges = items.iter().chain(closed.iter()).map(|item| {
            let by = numbers(&item["blocked_by"]).len() + usize::from(item["blocked_on"].as_str().is_some_and(|s| !s.is_empty()));
            let blocks = item["blocks"].as_array().map(Vec::len)
                .unwrap_or_else(|| item["n"].as_i64().and_then(|n| reverse_counts.get(&n).copied()).unwrap_or(0));
            by + usize::from(by > 0) + blocks + usize::from(blocks > 0) + gate_detail_lines(item)
        }).max().unwrap_or(0).min((u16::MAX - 6) as usize) as u16;
        let detail_h: u16 = if b.detail {
            (inner.height / 2).max(6 + edges)
        } else {
            (6 + edges).min((inner.height / 2).max(6))
        };
        let detail_h = detail_h.min(inner.height.saturating_sub(footer_h + 3));
        let list_h = inner.height.saturating_sub(footer_h + detail_h);
        if b.plan.is_some() {
            let body = Rect { height: inner.height.saturating_sub(footer_h), ..inner };
            self.draw_close_plan(frame, body);
            let footer = Rect { y: inner.y + body.height, height: footer_h.min(inner.height), ..inner };
            frame.render_widget(Paragraph::new(vec![self.board_status_line(), board_help(width)]), footer);
            return;
        }
        b.list_rows.set(list_h as usize);
        let list = Rect { height: list_h, ..inner };
        let scroll = (cursor_line as u16).saturating_sub(list_h.saturating_sub(3));
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), list);
        if detail_h > 0 && b.selected().is_some() {
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
        if let Some(limits) = b.data["account_limits"].as_array() {
            for limit in limits {
                let host = limit["host"].as_str().unwrap_or("unknown host");
                let sessions = limit["session_count"].as_i64().unwrap_or(0);
                let reset = match limit["resets_at"].as_f64() {
                    Some(at) if at > now_unix() as f64 => format!("resets {}", age((at - now_unix() as f64) as i64)),
                    _ => "reset unknown".to_owned(),
                };
                let attribution = limit["attribution"].as_str().unwrap_or("unknown");
                let items = limit["items"].as_array().map(|ns| {
                    ns.iter().filter_map(Value::as_i64).map(|n| format!("#{n}")).collect::<Vec<_>>().join(",")
                }).unwrap_or_default();
                spans.push(sep());
                spans.push(Span::styled(
                    format!("⚠ Claude usage limit {host}: {sessions} session{} · {reset} · {attribution}{}",
                            if sessions == 1 { "" } else { "s" },
                            if items.is_empty() { String::new() } else { format!(" · items {items}") }),
                    Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD),
                ));
            }
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
        let stale = b.items().iter().map(|i| gates_of(i).filter(|(_, g)| g["verdict"] == "STALE").count()).sum::<usize>();
        if stale > 0 {
            spans.push(sep());
            spans.push(Span::styled(
                format!("\u{27f2} {stale} stale receipt{}", if stale == 1 { "" } else { "s" }),
                Style::default().fg(theme::ATTN),
            ));
        }
        let runner_void = b.data["flags"].as_array().into_iter().flatten()
            .filter(|f| f["kind"] == "runner_void")
            .filter_map(|f| f["detail"]["count"].as_i64())
            .max();
        if let Some(count) = runner_void {
            spans.push(sep());
            spans.push(Span::styled(format!("runner VOID \u{00d7}{count}"), Style::default().fg(GATE_VOID).add_modifier(Modifier::BOLD)));
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
            match item_gate(item).filter(|_| cols.gate > 0) {
                // Leading space: a full-width badge (`WAIT 32m left`) fills its column.
                Some((cell, style)) => Span::styled(pad(&format!(" {cell}"), cols.gate), fade(style)),
                None => Span::raw(" ".repeat(cols.gate)),
            },
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
                let (glyph, style) = if h["kind"] == "shell" { super::symbols::engine("bash") } else { holder_glyph(state, h["state"]["for_s"].as_i64(), self.spinner_frame()) };
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
        lines.extend(gate_detail(item));
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
            let (glyph, gstyle) = if h["kind"] == "shell" { super::symbols::engine("bash") } else { holder_glyph(state, h["state"]["for_s"].as_i64(), self.spinner_frame()) };
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
                PromptKind::WaitingEta if b.strict() => "Job ETA, item stays active (e.g. 40m, 2h)",
                PromptKind::WaitingEta => "ETA (e.g. 40m, 2h)",
                PromptKind::GroupRename => "Group name",
                PromptKind::GroupNew => "New heading under this group",
                PromptKind::GroupPoc => "PoC (handle, member id or owner)",
            };
            return Line::styled(format!("{label}: {text}\u{2588}"), bold);
        }
        if b.group_menu {
            return Line::styled("Group: e promote to epic \u{00b7} p set PoC \u{00b7} r rename \u{00b7} n new heading under it \u{00b7} c close epic", bold);
        }
        if let Some(menu) = b.menu {
            return Line::styled(
                match menu {
                    Menu::Status if b.strict() => "Status (strict board): a active \u{00b7} w job eta (stays active) \u{00b7} b blocked \u{00b7} O on Owner \u{00b7} d done",
                    Menu::Status => "Status: o open \u{00b7} a active \u{00b7} w waiting \u{00b7} b blocked \u{00b7} O on Owner \u{00b7} d done",
                    Menu::Resolve => "Resolve flag: 1 nudge \u{00b7} 2 reassign \u{00b7} 3 block \u{00b7} 4 drop \u{00b7} 5 apply suggestion \u{00b7} 6 dismiss",
                    Menu::Close => "Close the epic as: d done \u{00b7} c cancelled (the close plan opens next)",
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
    /// The gate-receipt cell (` ⟲STALE+1 `); 0 when no item has a receipt.
    gate: usize,
    title: usize,
    chips: usize,
    note: usize,
}

impl Columns {
    /// Fixed: marker 2, #n 5, badge, flag 2. The rest splits title / chips /
    /// tail, the tail a bit wider than the title since it carries the blocker;
    /// it disappears on very narrow terminals.
    fn for_width(width: usize) -> Self {
        Self::with_gate(width, false)
    }

    /// As `for_width`, with room for the gate cell when any row has a receipt.
    fn with_gate(width: usize, gated: bool) -> Self {
        let badge = 13;
        let gate = if gated { GATE_CELL + 2 } else { 0 };
        let rest = width.saturating_sub(2 + 5 + badge + gate + 2);
        let chips = (rest / 5).clamp(12, 30).min(rest.saturating_sub(16));
        let title = (rest.saturating_sub(chips) * 2 / 5).clamp(16, 60).min(rest.saturating_sub(chips + 1));
        let note = rest.saturating_sub(title + 1 + chips);
        Self { badge, gate, title, chips, note: if note >= 10 { note } else { 0 } }
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

/// Width of the row's gate cell: `⟲STALE+9`.
const GATE_CELL: usize = 8;
/// VOID (no result: infra trouble or a missing input) is neither red nor green.
const GATE_VOID: ratatui::style::Color = ratatui::style::Color::Magenta;

/// The item's latest receipt per gate (doc/gate-receipts.md §9), by gate name.
fn gates_of(item: &Value) -> impl Iterator<Item = (&String, &Value)> {
    item["gate"].as_object().into_iter().flatten()
}

/// Worst first: STALE > FAIL > VOID > INCOMPLETE > PASS; unknown verdicts last.
fn gate_rank(verdict: &str) -> u8 {
    match verdict {
        "STALE" => 5,
        "FAIL" => 4,
        "VOID" => 3,
        "INCOMPLETE" => 2,
        "PASS" => 1,
        _ => 0,
    }
}

/// `✓PASS` green, `✗FAIL` red, `⊘VOID` magenta, `◐INC` amber, `⟲STALE` dim.
fn gate_badge(verdict: &str) -> (String, Style) {
    let bold = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
    match verdict {
        "PASS" => ("\u{2713}PASS".into(), bold(theme::OK)),
        "FAIL" => ("\u{2717}FAIL".into(), bold(theme::ERROR)),
        "VOID" => ("\u{2298}VOID".into(), bold(GATE_VOID)),
        "INCOMPLETE" => ("\u{25d0}INC".into(), bold(theme::ATTN)),
        "STALE" => ("\u{27f2}STALE".into(), Style::default().fg(theme::DIM)),
        // An unknown verdict is shown verbatim, never as a pass.
        other => (truncate(other, GATE_CELL), Style::default().fg(theme::TEXT)),
    }
}

/// The row cell: the worst gate's badge, plus `+N` for the other gates.
fn item_gate(item: &Value) -> Option<(String, Style)> {
    let gates: Vec<&Value> = gates_of(item).map(|(_, g)| g).collect();
    let worst = gates.iter().max_by_key(|g| gate_rank(g["verdict"].as_str().unwrap_or("")))?;
    let (mut text, style) = gate_badge(worst["verdict"].as_str().unwrap_or("?"));
    if gates.len() > 1 {
        text.push_str(&format!("+{}", gates.len() - 1));
    }
    Some((text, style))
}

/// `412 passed · 3 skipped` / `Found 2 errors` / `rc 1` for one step.
fn step_summary(step: &Value) -> String {
    let p = &step["parsed"];
    if p["passed"].is_i64() {
        let mut parts = vec![format!("{} passed", p["passed"])];
        for key in ["failed", "errors", "skipped", "deselected"] {
            if let Some(n) = p[key].as_i64().filter(|n| *n > 0) {
                parts.push(format!("{n} {key}"));
            }
        }
        return parts.join(" \u{00b7} ");
    }
    if let Some(ok) = p["ok"].as_bool() {
        return if ok { "Success".into() } else { format!("Found {} errors", p["errors"]) };
    }
    if let Some(v) = p["violations"].as_i64() {
        return if v == 0 { "All checks passed".into() } else { format!("{v} violations") };
    }
    match step["state"].as_str() {
        Some("ran") => step["rc"].as_i64().map(|rc| format!("rc {rc}")).unwrap_or_default(),
        Some(state) => state.replace('_', " "),
        None => String::new(),
    }
}

/// The detail pane's Gate section: one block per gate.
fn gate_detail(item: &Value) -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme::DIM);
    let mut lines = Vec::new();
    for (name, g) in gates_of(item) {
        let verdict = g["verdict"].as_str().unwrap_or("?");
        let (badge, style) = gate_badge(verdict);
        lines.push(Line::from(vec![
            Span::styled(format!("Gate {name} "), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
            Span::styled(badge, style),
            Span::styled(format!("  {}", g["headline"].as_str().unwrap_or("")), Style::default().fg(theme::MUTED)),
        ]));
        let mut meta = vec![Span::styled(
            format!("  head {}", g["head"].as_str().map(|h| h.get(..7).unwrap_or(h)).unwrap_or("?")),
            Style::default().fg(theme::MUTED),
        )];
        if let Some(stale) = g["stale"].as_object() {
            let mut why = stale.get("reason").and_then(Value::as_str).unwrap_or("stale").to_owned();
            let (prev, new) = (stale.get("prev").and_then(Value::as_str), stale.get("new").and_then(Value::as_str));
            if prev.is_some() || new.is_some() {
                why.push_str(&format!(" {} \u{2192} {}", prev.unwrap_or("?"), new.unwrap_or("?")));
            }
            if let Some(was) = g["was"].as_str() {
                why.push_str(&format!(" (was {was})"));
            }
            meta.push(Span::styled(" \u{00b7} ", dim));
            meta.push(Span::styled(format!("STALE: {why}"), Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)));
        }
        if let Some(s) = g["unchecked_s"].as_i64() {
            meta.push(Span::styled(" \u{00b7} ", dim));
            meta.push(Span::styled(format!("base unchecked {}", age(s)), Style::default().fg(theme::ATTN)));
        }
        if let Some(att) = g["attestation"].as_str() {
            meta.push(Span::styled(format!(" \u{00b7} {att}"), dim));
        }
        lines.push(Line::from(meta));
        let reasons: Vec<&Value> = g["reasons"].as_array().into_iter().flatten().collect();
        for step in g["steps"].as_array().into_iter().flatten() {
            let sname = step["name"].as_str().unwrap_or("?");
            let blocked = reasons.iter().any(|r| r["step"] == sname && r["level"] != "void");
            let (glyph, gstyle) = match step["state"].as_str() {
                Some("void") => ("\u{2298}", Style::default().fg(GATE_VOID)),
                Some("ran") if blocked => ("\u{2717}", Style::default().fg(theme::ERROR)),
                Some("ran") => ("\u{2713}", Style::default().fg(theme::OK)),
                _ => ("\u{00b7}", dim),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {glyph} "), gstyle),
                Span::styled(format!("{sname} "), Style::default().fg(theme::TEXT)),
                Span::styled(step_summary(step), Style::default().fg(theme::MUTED)),
            ]));
        }
        for r in &reasons {
            let color = match r["level"].as_str() {
                Some("fail") => theme::ERROR,
                Some("void") => GATE_VOID,
                _ => theme::ATTN,
            };
            lines.push(Line::styled(format!("  ! {}", r["text"].as_str().unwrap_or("")), Style::default().fg(color)));
        }
        if let Some(id) = g["id"].as_str() {
            lines.push(Line::styled(format!("  receipt {id}"), dim));
        }
    }
    lines
}

/// Lines the detail pane needs for its Gate section.
fn gate_detail_lines(item: &Value) -> usize {
    gates_of(item)
        .map(|(_, g)| 3 + g["steps"].as_array().map_or(0, Vec::len) + g["reasons"].as_array().map_or(0, Vec::len))
        .sum()
}

pub(super) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// RFC 3339 → Unix seconds (`2026-10-07T19:40:00Z`, fractional seconds and
/// numeric offsets accepted). The TUI carries no date crate.
pub(super) fn parse_rfc3339(s: &str) -> Option<i64> {
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
pub(super) fn status_badge(item: &Value, now: i64) -> (String, Style) {
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
    let full = "j/k move \u{00b7} A-u/d half page \u{00b7} Enter more detail \u{00b7} a add \u{00b7} s status \u{00b7} n note \u{00b7} r reassign \u{00b7} x resolve \u{00b7} d drop \u{00b7} o reopen \u{00b7} f free \u{00b7} Tab board \u{00b7} g refresh (on a group: edit) \u{00b7} Space fold \u{00b7} Esc close";
    let short = "j/k A-u/d \u{00b7} Enter \u{00b7} a s n r x d o \u{00b7} f \u{00b7} Tab \u{00b7} g \u{00b7} Esc";
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
    fn account_limit_banner_shows_reset_and_item_drilldown_without_hiding_flags() {
        let mut app = test_app();
        loaded(&mut app);
        app.board.data["account_limits"] = json!([{
            "host": "cm-sessions",
            "session_count": 2,
            "resets_at": now_unix() + 1800,
            "items": [3, 4],
            "attribution": "current"
        }]);
        let text = screen_at(&mut app, 160, 34);
        assert!(text.contains("Claude usage limit cm-sessions: 2 sessions"), "{text}");
        assert!(text.contains("resets 30m") && text.contains("items #3,#4"), "{text}");
        assert!(text.contains("⚑ Flags (1)"), "raw holder flags remain visible: {text}");
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
    fn launch_line_and_untracked_show_beside_free_capacity() {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        let mut app = test_app();
        loaded(&mut app);
        let since = iso(-18 * 60);
        app.board.data["launch"] = json!({"unlaunched":[{"n":2,"title":"docs","since":since}],"free":["lane-b","lane-d"]});
        app.board.data["untracked"] = json!(["busy-lane"]);
        let screen = screen_at(&mut app, 120, 30);
        assert!(screen.contains("1 unlaunched (#2 18m) · free: lane-b, lane-d"), "{screen}");
        assert!(!screen.contains("busy-lane"), "untracked folds with free capacity");
        app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(screen_at(&mut app, 120, 30).contains("untracked (working, no item): busy-lane"));
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

    fn gate(verdict: &str) -> Value {
        json!({"id":"r-test-fake-board-3-1","verdict":verdict,"was":null,"headline":verdict,
               "head":"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678","stale":null,"attestation":"cm-gate",
               "reasons":[],"steps":[]})
    }

    #[test]
    fn gate_cell_for_each_verdict_and_the_worst_of_several() {
        let cell = |v: Value| item_gate(&v).map(|(t, s)| (t, s.fg));
        for (verdict, text, color) in [
            ("PASS", "\u{2713}PASS", theme::OK),
            ("FAIL", "\u{2717}FAIL", theme::ERROR),
            ("VOID", "\u{2298}VOID", GATE_VOID),
            ("INCOMPLETE", "\u{25d0}INC", theme::ATTN),
            ("STALE", "\u{27f2}STALE", theme::DIM),
        ] {
            assert_eq!(cell(json!({"gate":{"gate": gate(verdict)}})), Some((text.into(), Some(color))));
        }
        assert_eq!(cell(json!({"n":1})), None, "no receipt, no cell");
        let several = json!({"gate":{"gate":gate("PASS"),"bench":gate("VOID"),"integ":gate("FAIL")}});
        assert_eq!(cell(several).unwrap().0, "\u{2717}FAIL+2", "worst first, then +N");
        let stale = json!({"gate":{"gate":gate("FAIL"),"bench":gate("STALE")}});
        assert_eq!(cell(stale).unwrap().0, "\u{27f2}STALE+1", "STALE outranks FAIL");
        assert!(gate_badge("STALE").0.chars().count() + 2 <= GATE_CELL, "room for +N");
    }

    fn gated(app: &mut App) {
        loaded(app);
        let mut data = app.board.data.clone();
        let item = &mut data["items"][0];
        item["gate"] = json!({
            "gate": {"id":"r-test-fake-board-3-4","verdict":"STALE","was":"PASS",
                     "headline":"STALE (was PASS): head_moved a1b2c3d \u{2192} 9f8e7d6",
                     "head":"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678",
                     "stale":{"reason":"head_moved","prev":"a1b2c3d","new":"9f8e7d6"},
                     "unchecked_s":2400,"attestation":"cm-gate",
                     "reasons":[],
                     "steps":[{"name":"pytest","state":"ran","rc":0,"parsed":{"passed":412,"failed":0,"errors":0,"skipped":3}},
                              {"name":"mypy","state":"ran","rc":0,"parsed":{"ok":true,"errors":0,"files":9}},
                              {"name":"fetch","state":"void","rc":null,"parsed":null}]},
            "bench": {"id":"r-test-fake-board-3-2","verdict":"VOID","was":null,"headline":"VOID: fetch void (FETCH_FAIL)",
                      "head":"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678","stale":null,"attestation":"self",
                      "reasons":[{"level":"void","text":"fetch: void (FETCH_FAIL)","step":"fetch"},
                                 {"level":"fail","text":"ruff: 3 violations","step":"ruff"}],
                      "steps":[{"name":"ruff","state":"ran","rc":1,"parsed":{"violations":3}},
                               {"name":"fetch","state":"void","rc":null,"parsed":null}]}
        });
        data["items"][1]["gate"] = json!({"gate": gate("PASS")});
        data["flags"].as_array_mut().unwrap().push(json!({"n":3,"kind":"runner_void","age_s":60,
            "detail":{"runner_sha":"2222222","count":2,"receipts":[]}}));
        app.board.data = data;
        app.board.cursor = 0;
        app.board.detail = true;
    }

    #[test]
    fn gate_receipts_render_in_rows_detail_and_banner() {
        for width in [100u16, 160] {
            let mut app = test_app();
            gated(&mut app);
            let text = screen_at(&mut app, width, 60);
            if std::env::var_os("CM_BOARD_DUMP").is_some() {
                println!("--- {width} columns ---\n{text}");
            }
            // Row cells: the worst gate plus +N, and a plain PASS.
            assert!(row(&text, "3").contains("\u{27f2}STALE+1"), "{text}");
            assert!(row(&text, "1").contains("\u{2713}PASS"), "{text}");
            // Banner counts.
            assert!(text.contains("\u{27f2} 1 stale receipt"), "{text}");
            assert!(text.contains("runner VOID \u{00d7}2"), "{text}");
            // Detail: stale reason, unchecked base, attestation, steps, reasons, id.
            assert!(text.contains("Gate gate \u{27f2}STALE"), "{text}");
            assert!(text.contains("STALE: head_moved a1b2c3d \u{2192} 9f8e7d6 (was PASS)"), "{text}");
            assert!(text.contains("head a1b2c3d"), "{text}");
            assert!(text.contains("base unchecked 40m"), "{text}");
            assert!(text.contains("\u{2713} pytest 412 passed \u{00b7} 3 skipped"), "{text}");
            assert!(text.contains("\u{2713} mypy Success"), "{text}");
            assert!(text.contains("\u{2298} fetch void"), "{text}");
            assert!(text.contains("Gate bench \u{2298}VOID"), "{text}");
            assert!(text.contains("\u{2717} ruff 3 violations"), "{text}");
            assert!(text.contains("! fetch: void (FETCH_FAIL)"), "{text}");
            assert!(text.contains("receipt r-test-fake-board-3-4"), "{text}");
            // Badge columns still align with the gate column added.
            let badge_at = |n: &str| {
                let l: Vec<char> = row(&text, n).chars().collect();
                ["ACTIVE", "WAIT", "BLOCKED", "OPEN"].iter().filter_map(|b| {
                    let b: Vec<char> = b.chars().collect();
                    l.windows(b.len()).position(|w| w == b.as_slice())
                }).min()
            };
            let starts: Vec<_> = ["3", "1", "4", "2"].iter().map(|n| badge_at(n)).collect();
            assert!(starts.iter().all(|s| s.is_some() && *s == starts[0]), "{starts:?}\n{text}");
        }
        let narrow = Columns::with_gate(98, true);
        assert!(2 + 5 + narrow.badge + narrow.gate + 2 + narrow.title + 1 + narrow.chips + narrow.note <= 98);
        assert_eq!(Columns::for_width(98).gate, 0, "no receipts, no gate column");
    }

    #[test]
    fn board_half_page_and_detail_height_are_stable_across_rows() {
        use crossterm::event::{Event, KeyEvent, KeyModifiers};
        let mut app = test_app();
        blockers(&mut app);
        app.board.visible = true;
        app.board.data["groups"] = json!([{ "id": "g", "name": "Group", "kind": "heading" }]);
        let rows = app.board.display_rows();
        let group = rows.iter().position(|r| matches!(r, DRow::Group { .. })).expect("group row");
        let item = rows.iter().position(|r| matches!(r, DRow::Item { .. })).unwrap();
        app.board.cursor = group;
        screen_at(&mut app, 120, 30);
        let group_height = app.board.list_rows.get();
        app.board.cursor = item;
        screen_at(&mut app, 120, 30);
        assert_eq!(app.board.list_rows.get(), group_height);
        app.board.cursor = 0;
        let alt = |c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT));
        app.handle_event(&alt('d'));
        assert_eq!(app.board.cursor, (group_height / 2).max(1).min(rows.len() - 1));
        assert!(app.board.prompt.is_none(), "A-d must not trigger drop");
        app.board.menu = Some(Menu::Status);
        app.handle_event(&alt('d'));
        assert_eq!(app.board.menu, Some(Menu::Status), "A-d must not select done in a menu");
        app.board.menu = None;
        app.handle_event(&alt('u'));
        assert_eq!(app.board.cursor, 0);
        app.handle_event(&alt('u'));
        assert_eq!(app.board.cursor, 0);
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
        // On a strict board w declares a job's ETA on the active item, and o is gone.
        app.board.data["board"]["backlog_allowed"] = json!(false);
        app.handle_event(&key(KeyCode::Char('s')));
        assert!(screen_at(&mut app, 140, 30).contains("w job eta (stays active)"));
        app.handle_event(&key(KeyCode::Char('w')));
        assert!(screen_at(&mut app, 140, 30).contains("Job ETA, item stays active"));
        for c in "2h".chars() { app.handle_event(&key(KeyCode::Char(c))); }
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(app.board.queued.back().unwrap().1, json!({"n":3,"status":"active","eta":"2h","board":"test-fake-board"}));
        let queued = app.board.queued.len();
        app.handle_event(&key(KeyCode::Char('s')));
        app.handle_event(&key(KeyCode::Char('o')));
        assert_eq!(app.board.queued.len(), queued, "no open on a strict board");
        app.board.data["board"]["backlog_allowed"] = json!(true);
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
        b.accept(json!({"unchanged":true,"version":7,"account_limits":[{"host":"cm","session_count":1}]}));
        assert_eq!(b.data["account_limits"][0]["host"], "cm");
    }
}
