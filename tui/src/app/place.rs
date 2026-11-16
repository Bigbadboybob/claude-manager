//! Planning view placement (doc/org-model.md §7.1, §11, §13.4; S3).
//!
//! Planning is the backlog only (O7): it adds tasks and places them on
//! boards, and never launches a session (those start from the Sessions view,
//! `A-n` / `A-s`).
//!
//! - `A-f` on a task opens the place dialog: a board, an epic (or none) and
//!   who holds it: nobody, a member, a team, or a new hire (`org.hire` first,
//!   then the place). A team (O8) hands the item to its lead's main session,
//!   and the lead hands it out; CM never picks a member. On a placed task
//!   `u` unplaces it.
//! - Placed tasks show a board chip (`planning.rs`, from `Task::placement`).
//! - A read-only "On boards" strip under the board lists open epics and
//!   board-native items (`boards.on_boards`); `A-b` hides or shows it.
//!
//! `boards.on_boards` doubles as the feature check: it is polled only while
//! the planning view is shown. An old daemon or API turns the strip off and
//! `A-f` shows [`PLACE_UNSUPPORTED`] instead of the dialog (app/org_model.rs).
use super::org_model::{owner_rpc, RpcFail, Support, OFF_RETRY};
use super::*;
use serde_json::{json, Value};
use std::sync::mpsc;

const ON_BOARDS_EVERY: Duration = Duration::from_secs(30);
const ERROR_RETRY: Duration = Duration::from_secs(15);
const STRIP_MAX: u16 = 10;

/// What `A-f` says when the daemon or planning API cannot place. Planning
/// has no launch fallback (O7).
pub(super) const PLACE_UNSUPPORTED: &str = "Placing on boards needs a newer daemon and planning API. Planning no longer launches sessions: start one from the Sessions view (A-n / A-s).";

/// Who will hold the placed item.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Holder {
    Nobody,
    /// A member, by member id.
    Member(String),
    /// A team, by team id (O8): the API gives the item to the team lead's
    /// main session, and the lead hands it out.
    Team(String),
    /// Hire a new member for it: `(handle, title)`.
    Hire(String, String),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PlaceDialog {
    pub task_id: String,
    pub title: String,
    /// Open boards (`GET /boards`), loaded when the dialog opens.
    pub boards: Vec<Value>,
    pub board: usize,
    /// 0 = no epic, else index + 1 into the board's open epics.
    pub epic: usize,
    pub holder: Holder,
    /// 0 board, 1 epic, 2 holder.
    pub field: usize,
    /// Typing a hire's handle, then its title.
    pub typing: Option<(u8, String)>,
    pub error: String,
    /// The board the task is on now: placing elsewhere sends `replace`.
    pub placed_board: Option<String>,
    /// The task's current board link (`{board, n, ...}`), when placed: `u`
    /// unplaces it.
    pub placed: Option<Value>,
}

type Reply = (&'static str, Result<Value, RpcFail>);

#[derive(Default)]
pub(super) struct Place {
    pub(super) support: Support,
    pub(super) on_boards: Value,
    /// Replies in flight: the on-boards poll, a board list, a place or
    /// unplace, a hire. Each is kept until it answers.
    rx: Vec<mpsc::Receiver<Reply>>,
    on_boards_inflight: bool,
    /// When the last full (non-`unchanged`) on-boards read was sent.
    on_boards_full: Option<Instant>,
    last_poll: Option<Instant>,
    retry_at: Option<Instant>,
    pub(super) dialog: Option<PlaceDialog>,
    /// The "On boards" strip is hidden (`A-b`).
    pub(super) hide_strip: bool,
    /// An `item.place` waiting for an `org.hire` reply: its params.
    after_hire: Option<Value>,
    seq: u64,
}

impl App {
    pub(super) fn planning_place_open(&self) -> bool {
        self.place.dialog.is_some()
    }

    fn place_request(&mut self, op: &'static str, method: &'static str, params: Value) {
        let targets = self.owner_rpc_targets();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send((op, owner_rpc(&targets, method, &params)));
        });
        self.place.rx.push(rx);
    }

    fn place_request_id(&mut self, what: &str) -> String {
        self.place.seq += 1;
        format!("tui-place-{what}-{}-{}", board::now_unix(), self.place.seq)
    }

    /// Open epics of a board, from the on-boards read.
    pub(super) fn place_epics(&self, board: &str) -> Vec<Value> {
        self.place.on_boards["epics"].as_array().into_iter().flatten().filter(|e| e["board"] == board).cloned().collect()
    }

    /// Members a task can go to (active, not Owner), by handle.
    fn place_members(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self.org.snapshot.as_ref().map_or_else(Vec::new, |s| {
            s.members.iter().filter(|m| !m.is_owner() && m.status != "retired").map(|m| (m.id.clone(), m.handle.clone())).collect()
        });
        out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        out
    }

    /// Teams a task can go to (not archived), by name: `(id, name)`.
    fn place_teams(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self.org.snapshot.as_ref().map_or_else(Vec::new, |s| {
            s.teams.iter().filter(|t| !t.archived()).map(|t| (t.id.clone(), t.name.clone())).collect()
        });
        out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        out
    }

    /// Replies, and the planning-only on-boards poll.
    pub fn place_tick(&mut self) {
        let mut ready = Vec::new();
        self.place.rx.retain(|rx| match rx.try_recv() {
            Ok(reply) => {
                ready.push(reply);
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
        for (op, r) in ready {
            if op == "on_boards" {
                self.place.on_boards_inflight = false;
            }
            self.place_accept(op, r);
        }
        // Probe support once in Planning; after that, read only while the
        // strip shows and the viewer is focused.
        let wanted = match self.place.support {
            Support::Off => self.view_mode == ViewMode::Planning,
            Support::On => self.place_strip_shown() && self.focused,
            _ => self.view_mode == ViewMode::Planning && self.place.last_poll.is_none(),
        };
        if !wanted || self.place.on_boards_inflight || cfg!(test) {
            return;
        }
        if self.place.retry_at.is_some_and(|t| Instant::now() < t) || self.place.last_poll.is_some_and(|t| t.elapsed() < ON_BOARDS_EVERY) {
            return;
        }
        self.place.last_poll = Some(Instant::now());
        self.place.retry_at = None;
        self.place.on_boards_inflight = true;
        let params = self.on_boards_params();
        self.place_request("on_boards", "boards.on_boards", params);
    }

    /// `since_version` once a versioned read is held (an older API ignores
    /// it and answers in full); a full read at most every five minutes.
    fn on_boards_params(&mut self) -> Value {
        let version = self.place.on_boards["version"].as_i64();
        match version {
            Some(v) if self.place.on_boards_full.is_some_and(|t| t.elapsed() < super::org_model::FULL_READ) => json!({"since_version": v}),
            _ => {
                self.place.on_boards_full = Some(Instant::now());
                json!({})
            }
        }
    }

    /// The read-only On boards strip is on screen (or would be, once it has
    /// rows): the planning view with placement supported and `A-b` not off.
    pub(super) fn place_strip_shown(&self) -> bool {
        self.view_mode == ViewMode::Planning && self.place.support == Support::On && !self.place.hide_strip
    }

    /// Apply a reply (also the test entry point).
    pub(super) fn place_accept(&mut self, op: &str, r: Result<Value, RpcFail>) {
        self.needs_redraw = true;
        match (op, r) {
            ("on_boards", Ok(v)) => {
                self.place.support = Support::On;
                if v["unchanged"].as_bool() != Some(true) {
                    self.place.on_boards = v;
                }
                self.planning.placement_on = true;
            }
            ("on_boards", Err(f)) if f.unsupported => {
                self.place.support = Support::Off;
                self.place.on_boards = Value::Null;
                self.planning.placement_on = false;
                self.place.retry_at = Some(Instant::now() + OFF_RETRY);
                // A dialog opened before the first answer says so in place.
                if let Some(d) = self.place.dialog.as_mut() {
                    d.error = PLACE_UNSUPPORTED.into();
                }
            }
            ("on_boards", Err(_)) => {
                // Unknown support stays unknown; probe again after the retry.
                self.place.last_poll = None;
                self.place.retry_at = Some(Instant::now() + ERROR_RETRY);
            }
            ("boards", Ok(v)) => {
                if let Some(d) = self.place.dialog.as_mut() {
                    d.boards = v.as_array().cloned().unwrap_or_default();
                    if d.boards.is_empty() {
                        d.error = "No open boards".into();
                    }
                }
            }
            ("hire", Ok(v)) => {
                // The hire has no session yet, so the place names it by handle.
                let handle = v["member"]["handle"].as_str().or(v["handle"].as_str()).map(str::to_owned);
                if let Some(mut params) = self.place.after_hire.take() {
                    if let Some(h) = handle.or_else(|| params["hire_handle"].as_str().map(str::to_owned)) {
                        params["holder"] = json!(h);
                        params["role"] = json!("owner");
                    }
                    if let Some(o) = params.as_object_mut() {
                        o.remove("hire_handle");
                    }
                    self.place_request("place", "item.place", params);
                }
                self.org_refresh_soon();
            }
            ("hire", Err(f)) if f.unsupported => {
                self.place.after_hire = None;
                self.place_error("Hiring isn't available on this daemon or API yet; pick a member or nobody".into());
            }
            ("place", Ok(v)) => {
                self.place.dialog = None;
                let to = match v["team"]["name"].as_str() {
                    Some(team) => format!(", team {team}: its lead {} hands it out", v["team"]["lead_handle"].as_str().unwrap_or("?")),
                    None => String::new(),
                };
                self.set_status_msg(&format!("Placed on {} #{} ({}){to}", v["board"].as_str().unwrap_or("?"), v["n"], v["item_status"].as_str().unwrap_or("open")));
                self.place.last_poll = None;
            }
            ("unplace", Ok(_)) => {
                self.place.dialog = None;
                self.set_status_msg("Unplaced: back in the backlog");
                self.place.last_poll = None;
            }
            ("place" | "unplace", Err(f)) if f.unsupported => self.place_error(PLACE_UNSUPPORTED.into()),
            (_, Err(f)) if f.unsupported => self.place_error(format!("{op} is not supported by this daemon or planning API yet")),
            (_, Err(f)) => self.place_error(f.message),
            _ => {}
        }
    }

    fn place_error(&mut self, e: String) {
        match self.place.dialog.as_mut() {
            Some(d) => d.error = e,
            None => self.set_status_msg(&e),
        }
    }

    /// Planning-view keys this module owns: `A-f` (always: it places, or
    /// says why it can't; planning never launches), `A-b`, and every key
    /// while the dialog is open.
    pub(super) fn place_event(&mut self, event: &CrosstermEvent) -> bool {
        if self.view_mode != ViewMode::Planning {
            return false;
        }
        let CrosstermEvent::Key(key) = event else { return self.planning_place_open() };
        if key.kind == crossterm::event::KeyEventKind::Release {
            return self.planning_place_open();
        }
        if self.place.dialog.is_some() {
            self.place_dialog_key(key);
            return true;
        }
        let alt = key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::SHIFT);
        if !alt || !self.planning.normal_mode() {
            return false;
        }
        match key.code {
            KeyCode::Char('f') => {
                self.open_place();
                true
            }
            KeyCode::Char('b') if self.place.support == Support::On => {
                self.place.hide_strip = !self.place.hide_strip;
                true
            }
            _ => false,
        }
    }

    /// `A-f`: the place dialog for the selected task, or a notice.
    fn open_place(&mut self) {
        if self.place.support == Support::Off {
            // Ask again on the next tick, in case the daemon or API was
            // upgraded since the last answer.
            self.place.retry_at = None;
            self.place.last_poll = None;
            self.set_status_msg(PLACE_UNSUPPORTED);
            return;
        }
        let Some((task_id, title)) = self.planning.selected_task_ref() else {
            self.set_status_msg("A-f places a task on a board: select a task first");
            return;
        };
        let placed = self.planning.placements.get(&task_id).cloned();
        self.open_place_dialog(task_id, title, placed);
    }

    /// The place dialog for `task_id`; `placed` is its current board link
    /// (`Task::placement`), when it has one. Also F6's entry point (#96).
    pub(super) fn open_place_dialog(&mut self, task_id: String, title: String, placed: Option<Value>) {
        let placed_board = placed.as_ref().and_then(|p| p["board"].as_str()).map(str::to_owned);
        self.place.dialog = Some(PlaceDialog { task_id, title, boards: Vec::new(), board: 0, epic: 0, holder: Holder::Nobody, field: 0, typing: None, error: String::new(), placed_board, placed });
        self.org_refresh_soon();
        let (url, token) = (self.config.api_url.clone(), self.config.api_token.clone());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::api::list_open_boards(&url, &token).map(Value::from).map_err(|e| RpcFail { unsupported: false, message: e.to_string() });
            let _ = tx.send(("boards", r));
        });
        self.place.rx.push(rx);
        // The epic list reads the on-boards data: fresh, even with the strip off.
        if !self.place.on_boards_inflight && !cfg!(test) {
            self.place.on_boards_inflight = true;
            self.place.last_poll = Some(Instant::now());
            let params = self.on_boards_params();
            self.place_request("on_boards", "boards.on_boards", params);
        }
    }

    fn place_unplace(&mut self, task_id: &str) {
        let rid = self.place_request_id("unplace");
        self.place_request("unplace", "item.unplace", json!({"task_id": task_id, "request_id": rid}));
    }

    /// The holders the dialog cycles through: nobody, each member, each
    /// team, hire.
    fn place_holders(&self) -> Vec<Holder> {
        let mut out = vec![Holder::Nobody];
        out.extend(self.place_members().into_iter().map(|(id, _)| Holder::Member(id)));
        out.extend(self.place_teams().into_iter().map(|(id, _)| Holder::Team(id)));
        out.push(Holder::Hire(String::new(), String::new()));
        out
    }

    pub(super) fn place_dialog_key(&mut self, key: &crossterm::event::KeyEvent) {
        let holders = self.place_holders();
        let Some(mut d) = self.place.dialog.take() else { return };
        if let Some((stage, mut text)) = d.typing.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter if stage == 0 && !text.trim().is_empty() => {
                    d.holder = Holder::Hire(text.trim().to_owned(), String::new());
                    d.typing = Some((1, String::new()));
                }
                KeyCode::Enter if stage == 1 => {
                    if let Holder::Hire(h, _) = &d.holder {
                        d.holder = Holder::Hire(h.clone(), text.trim().to_owned());
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                    d.typing = Some((stage, text));
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    text.push(c);
                    d.typing = Some((stage, text));
                }
                _ => d.typing = Some((stage, text)),
            }
            self.place.dialog = Some(d);
            return;
        }
        let slug = d.boards.get(d.board).and_then(|b| b["slug"].as_str()).unwrap_or("").to_owned();
        let epics = self.place_epics(&slug).len();
        let step = |d: &mut PlaceDialog, dir: i32| match d.field {
            0 if !d.boards.is_empty() => {
                d.board = (d.board as i32 + dir).rem_euclid(d.boards.len() as i32) as usize;
                d.epic = 0;
            }
            1 => d.epic = (d.epic as i32 + dir).rem_euclid(epics as i32 + 1) as usize,
            2 => {
                let at = holders.iter().position(|h| *h == d.holder || matches!((h, &d.holder), (Holder::Hire(..), Holder::Hire(..)))).unwrap_or(0);
                d.holder = holders[(at as i32 + dir).rem_euclid(holders.len() as i32) as usize].clone();
            }
            _ => {}
        };
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Char('u') if key.modifiers.is_empty() && d.placed.is_some() => {
                d.error.clear();
                let task_id = d.task_id.clone();
                self.place_unplace(&task_id);
            }
            KeyCode::Tab | KeyCode::Down => d.field = (d.field + 1) % 3,
            KeyCode::BackTab | KeyCode::Up => d.field = (d.field + 2) % 3,
            KeyCode::Right | KeyCode::Char('l') => step(&mut d, 1),
            KeyCode::Left | KeyCode::Char('h') => step(&mut d, -1),
            KeyCode::Enter if d.field == 2 && matches!(&d.holder, Holder::Hire(h, _) if h.is_empty()) => d.typing = Some((0, String::new())),
            KeyCode::Enter => {
                if self.place.support == Support::Off {
                    d.error = PLACE_UNSUPPORTED.into();
                } else if slug.is_empty() {
                    d.error = "Pick a board first".into();
                } else {
                    d.error.clear();
                    let params = self.place_params(&d);
                    match &d.holder {
                        Holder::Hire(handle, title) => {
                            let rid = self.place_request_id("hire");
                            let mut params = params;
                            params["hire_handle"] = json!(handle);
                            self.place.after_hire = Some(params);
                            let mut hire = json!({"handle": handle, "kind": "agent", "request_id": rid});
                            if !title.is_empty() {
                                hire["title"] = json!(title);
                            }
                            self.place_request("hire", "org.hire", hire);
                        }
                        _ => self.place_request("place", "item.place", params),
                    }
                }
            }
            _ => {}
        }
        self.place.dialog = Some(d);
    }

    /// `item.place` for the dialog as it stands (a hire's holder is filled
    /// in from `org.hire`'s reply).
    pub(super) fn place_params(&mut self, d: &PlaceDialog) -> Value {
        let slug = d.boards.get(d.board).and_then(|b| b["slug"].as_str()).unwrap_or("").to_owned();
        let rid = self.place_request_id("place");
        let mut params = json!({"task_id": d.task_id, "board": slug, "request_id": rid});
        if let Some(e) = d.epic.checked_sub(1).and_then(|i| self.place_epics(&slug).get(i).cloned()) {
            params["group_id"] = e["group_id"].clone();
        }
        match &d.holder {
            Holder::Member(id) => {
                // Holders resolve by session uid or name, not member id.
                let m = self.org.snapshot.as_ref().and_then(|s| s.member(id));
                let r = m.and_then(|m| m.main_uid().map(str::to_owned).or_else(|| Some(m.handle.clone()))).unwrap_or_else(|| id.clone());
                params["holder"] = json!(r);
                params["role"] = json!("owner");
            }
            // The API resolves the lead's main session (O8); the TUI never
            // picks a member for a team.
            Holder::Team(id) => params["team"] = json!(id),
            Holder::Nobody | Holder::Hire(..) => {}
        }
        if d.placed_board.as_deref().is_some_and(|b| b != slug) {
            params["replace"] = json!(true);
        }
        params
    }

    pub(super) fn draw_place(&self, frame: &mut Frame, area: Rect) {
        let snap = self.org.snapshot.as_ref();
        let Some(d) = &self.place.dialog else { return };
        let w = 72.min(area.width);
        let h = 9u16.min(area.height);
        let r = Rect::new(area.x + area.width.saturating_sub(w) / 2, area.y + area.height.saturating_sub(h) / 2, w, h);
        frame.render_widget(Clear, r);
        let block = Block::default().borders(Borders::ALL).title(" Place on a board ").border_style(Style::default().fg(theme::ATTN));
        let inner = block.inner(r);
        frame.render_widget(block, r);
        let slug = d.boards.get(d.board).and_then(|b| b["slug"].as_str()).unwrap_or("");
        let board = if d.boards.is_empty() { "loading boards\u{2026}".to_owned() } else { format!("\u{25c2} {slug} \u{25b8}") };
        let epic = match d.epic.checked_sub(1).and_then(|i| self.place_epics(slug).get(i).cloned()) {
            Some(e) => format!("\u{25c2} {} {} \u{25b8}", symbols::EPIC, e["name"].as_str().unwrap_or("?")),
            None => "\u{25c2} none \u{25b8}".to_owned(),
        };
        let holder = match &d.holder {
            Holder::Nobody => "\u{25c2} nobody (open) \u{25b8}".to_owned(),
            Holder::Member(id) => format!("\u{25c2} {} \u{25b8}", snap.map_or_else(|| id.clone(), |s| s.handle(id))),
            Holder::Team(id) => {
                let team = snap.and_then(|s| s.team(id));
                let lead = match team.and_then(|t| t.lead_member_id.as_deref()) {
                    Some(l) => format!("\u{2192} lead {} hands it out", snap.map_or_else(|| l.to_owned(), |s| s.handle(l))),
                    None => "(no lead: placing is refused)".to_owned(),
                };
                format!("\u{25c2} team {} {lead} \u{25b8}", team.map_or(id.as_str(), |t| t.name.as_str()))
            }
            Holder::Hire(h, _) if h.is_empty() => "\u{25c2} hire a new member\u{2026} (Enter) \u{25b8}".to_owned(),
            Holder::Hire(h, t) => format!("\u{25c2} hire {h}{} \u{25b8}", if t.is_empty() { String::new() } else { format!(" \u{00b7} {t}") }),
        };
        let row = |i: usize, label: &str, value: String| {
            let st = if d.field == i { Style::default().fg(theme::TEXT).bg(theme::SELECT_BG) } else { Style::default().fg(theme::TEXT) };
            Line::from(vec![Span::styled(format!(" {label:<8}"), Style::default().fg(theme::DIM)), Span::styled(value, st)])
        };
        let on = d.placed.as_ref().map(|p| format!("  (on {} #{})", p["board"].as_str().unwrap_or("?"), p["n"])).unwrap_or_default();
        let mut lines = vec![
            Line::styled(format!(" {}{on}", crate::planning::truncate_with_ellipsis(&d.title, (w as usize).saturating_sub(4 + on.chars().count()))), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
            row(0, "board", board),
            row(1, "epic", epic),
            row(2, "holder", holder),
        ];
        match &d.typing {
            Some((0, t)) => lines.push(Line::styled(format!(" new member's handle: {t}\u{2588}"), Style::default().fg(theme::ATTN))),
            Some((_, t)) => lines.push(Line::styled(format!(" title (optional): {t}\u{2588}"), Style::default().fg(theme::ATTN))),
            None if !d.error.is_empty() => lines.push(Line::styled(format!(" {}", d.error), Style::default().fg(theme::ERROR))),
            None => lines.push(Line::from("")),
        }
        let unplace = if d.placed.is_some() { " \u{00b7} u unplace" } else { "" };
        lines.push(Line::styled(format!(" Tab field \u{00b7} \u{2190}/\u{2192} change \u{00b7} Enter place{unplace} \u{00b7} Esc cancel"), Style::default().fg(theme::DIM)));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// Rows the "On boards" strip takes under the planning board.
    pub(super) fn on_boards_height(&self, area: Rect) -> u16 {
        if self.place.support != Support::On || self.place.hide_strip {
            return 0;
        }
        // Keep the list viewport fixed as selection and asynchronous results change.
        STRIP_MAX.min(area.height / 3)
    }

    fn on_boards_lines(&self, width: usize) -> Vec<Line<'static>> {
        let v = &self.place.on_boards;
        let snap = self.org.snapshot.as_ref();
        let mut lines = vec![Line::styled("On boards (read-only) \u{00b7} A-b hides", Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD))];
        for e in v["epics"].as_array().into_iter().flatten() {
            let poc = e["poc_member_id"].as_str().map(|p| format!("  PoC {}", snap.map_or_else(|| p.to_owned(), |s| s.handle(p)))).unwrap_or_default();
            let text = format!(
                "{} {}  {}  {}{poc}  \u{00b7} {} open",
                symbols::EPIC,
                e["board"].as_str().unwrap_or("?"),
                e["name"].as_str().unwrap_or("?"),
                e["epic_status"].as_str().unwrap_or("active"),
                e["open_items"].as_i64().unwrap_or(0)
            );
            lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&text, width), Style::default().fg(theme::TEXT)));
        }
        for i in v["items"].as_array().into_iter().flatten() {
            let (mark, st) = symbols::board_status(i["status"].as_str().unwrap_or(""));
            let holders: Vec<&str> = i["holders"].as_array().into_iter().flatten().filter_map(|h| h["name"].as_str().or(h.as_str())).collect();
            let text = format!(" {} #{} {}  {}", i["board"].as_str().unwrap_or("?"), i["n"], i["title"].as_str().unwrap_or(""), holders.join(", "));
            lines.push(Line::from(vec![Span::styled(format!("  {mark}"), st), Span::styled(crate::planning::truncate_with_ellipsis(&text, width.saturating_sub(3)), Style::default().fg(theme::MUTED))]));
        }
        lines
    }

    pub(super) fn draw_on_boards(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let block = Block::default().borders(Borders::TOP).border_style(Style::default().fg(theme::DIM));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(self.on_boards_lines(inner.width as usize)), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::super::org_model::fixtures::{member, owner, snapshot, team};
    use super::*;
    use crossterm::event::KeyEvent;

    fn test_app() -> App {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::test_support::home_lock();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let app = App::new(crate::config::Config { api_url: String::new(), api_token: String::new(), gcp_project: String::new(), gcp_zone: String::new(), repos: HashMap::new() });
        match prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        app
    }

    fn key(app: &mut App, c: KeyCode, m: KeyModifiers) -> bool {
        app.place_event(&CrosstermEvent::Key(KeyEvent::new(c, m)))
    }

    fn on_boards() -> Value {
        json!({
            "epics": [{"board": "swarm", "group_id": "g-epic", "name": "Org model", "epic_status": "active", "poc_member_id": "m1", "open_items": 3}],
            "items": [{"board": "swarm", "n": 21, "title": "board-native chore", "status": "open", "holders": []}]
        })
    }

    fn placing_app() -> App {
        let mut app = test_app();
        app.view_mode = ViewMode::Planning;
        app.org.accept(snapshot(1, vec![owner(), member("m1", "ep", Some("mem-owner"), None, None), member("m2", "beta", Some("m1"), None, None)], vec![]));
        app.place_accept("on_boards", Ok(on_boards()));
        app
    }

    /// A planning view holding one backlog task, selected.
    fn with_task(mut app: App) -> App {
        let task: crate::api::Task = serde_json::from_value(json!({
            "id": "t1", "slug": "org-tui", "name": "Org TUI", "created_at": "2026-11-05T00:00:00Z",
            "repo_url": "", "repo_branch": "main", "status": "backlog", "project": "p",
            "description": "the work contract"
        }))
        .unwrap();
        app.planning.update_from_api(vec![task]);
        assert_eq!(app.planning.selected_task_ref().map(|t| t.0), Some("t1".into()), "the task is selected");
        app
    }

    fn status(app: &App) -> String {
        app.status_msg.as_ref().map(|m| m.0.clone()).unwrap_or_default()
    }

    /// O7: an old daemon or API gets a notice on A-f, never a launch.
    #[test]
    fn old_apis_get_a_notice_never_a_launch() {
        let mut app = with_task(test_app());
        app.view_mode = ViewMode::Planning;
        app.place_accept("on_boards", Err(RpcFail { unsupported: true, message: "unknown method".into() }));
        assert_eq!(app.place.support, Support::Off);
        assert!(key(&mut app, KeyCode::Char('f'), KeyModifiers::ALT), "A-f is placement's, even unsupported");
        assert!(app.place.dialog.is_none());
        assert_eq!(status(&app), PLACE_UNSUPPORTED);
        assert!(app.planning.normal_mode(), "no launch dialog opened");
        assert!(app.place.retry_at.is_none(), "A-f asks again on the next tick");
        assert_eq!(app.on_boards_height(Rect::new(0, 0, 100, 40)), 0);
        assert!(!key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT), "no strip to toggle");

        // Support not known yet: the dialog opens, and an unsupported answer
        // says so inside it; Enter then sends nothing.
        let mut app = with_task(test_app());
        app.view_mode = ViewMode::Planning;
        assert!(key(&mut app, KeyCode::Char('f'), KeyModifiers::ALT));
        assert!(app.place.dialog.is_some());
        app.place_accept("on_boards", Err(RpcFail { unsupported: true, message: "unknown method".into() }));
        assert_eq!(app.place.dialog.as_ref().unwrap().error, PLACE_UNSUPPORTED);
        app.place.dialog.as_mut().unwrap().boards = vec![json!({"slug": "swarm"})];
        let pending = app.place.rx.len();
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.place.rx.len(), pending, "nothing is sent");
        assert_eq!(app.place.dialog.as_ref().unwrap().error, PLACE_UNSUPPORTED);
        // A place that the daemon refuses as unknown reads the same.
        app.place_accept("place", Err(RpcFail { unsupported: true, message: "unknown method".into() }));
        assert_eq!(app.place.dialog.as_ref().unwrap().error, PLACE_UNSUPPORTED);
    }

    /// O7: A-f opens the place dialog itself (no Launch choice); `u` in it
    /// unplaces a placed task.
    #[test]
    fn a_f_opens_the_place_dialog_directly_and_u_unplaces() {
        let mut app = with_task(placing_app());
        assert!(key(&mut app, KeyCode::Char('f'), KeyModifiers::ALT));
        let d = app.place.dialog.clone().expect("the place dialog opens directly");
        assert_eq!((d.task_id.as_str(), d.placed.is_none()), ("t1", true));
        let pending = app.place.rx.len();
        key(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(app.place.rx.len(), pending, "u does nothing on an unplaced task");
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.place.dialog.is_none());

        app.planning.placements.insert("t1".into(), json!({"board": "swarm", "n": 15, "status": "active"}));
        assert!(key(&mut app, KeyCode::Char('f'), KeyModifiers::ALT));
        let d = app.place.dialog.clone().unwrap();
        assert_eq!(d.placed_board.as_deref(), Some("swarm"));
        key(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(app.place.rx.len(), pending + 2, "the board list and the unplace");
        app.place_accept("unplace", Ok(json!({"task_status": "backlog"})));
        assert!(app.place.dialog.is_none());
        assert_eq!(status(&app), "Unplaced: back in the backlog");
        // A-f with nothing selected says what it needs.
        let mut app = placing_app();
        assert!(key(&mut app, KeyCode::Char('f'), KeyModifiers::ALT));
        assert!(app.place.dialog.is_none());
        assert!(status(&app).contains("select a task first"));
    }

    /// O8: the holder field offers teams; a team sends `team` and no holder
    /// (the API gives it to the lead's main session).
    #[test]
    fn a_team_holder_sends_the_team_and_lets_the_lead_hand_it_out() {
        let mut app = placing_app();
        app.org.accept(snapshot(
            2,
            vec![owner(), member("m1", "ep", Some("mem-owner"), Some("team-a"), Some("ep-uid")), member("m2", "beta", Some("m1"), Some("team-a"), None)],
            vec![team("team-a", "Alpha", None, Some("m1"), &[("m1", "primary"), ("m2", "primary")]), team("team-b", "Beta squad", None, None, &[])],
        ));
        assert_eq!(
            app.place_holders(),
            vec![Holder::Nobody, Holder::Member("m2".into()), Holder::Member("m1".into()), Holder::Team("team-a".into()), Holder::Team("team-b".into()), Holder::Hire(String::new(), String::new())]
        );
        app.place.dialog = Some(PlaceDialog { task_id: "t1".into(), title: "x".into(), boards: vec![json!({"slug": "swarm"})], board: 0, epic: 0, holder: Holder::Member("m1".into()), field: 2, typing: None, error: String::new(), placed_board: None, placed: None });
        key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        let d = app.place.dialog.clone().unwrap();
        assert_eq!(d.holder, Holder::Team("team-a".into()));
        let p = app.place_params(&d);
        assert_eq!(p["team"], "team-a");
        assert!(p.get("holder").is_none() && p.get("role").is_none(), "CM never picks a member: {p}");
        let text: Vec<String> = dialog_text(&app);
        assert!(text.iter().any(|l| l.contains("team Alpha \u{2192} lead ep hands it out")), "{text:?}");
        key(&mut app, KeyCode::Right, KeyModifiers::NONE);
        assert!(dialog_text(&app).iter().any(|l| l.contains("team Beta squad (no lead")), "a leadless team says so");
        app.place_accept("place", Ok(json!({"board": "swarm", "n": 16, "item_status": "active", "team": {"name": "Alpha", "lead_handle": "ep"}})));
        assert_eq!(status(&app), "Placed on swarm #16 (active), team Alpha: its lead ep hands it out");
    }

    fn dialog_text(app: &App) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(100, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| app.draw_place(f, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect()
    }

    #[test]
    fn on_boards_strip_does_not_resize_with_selection_results() {
        let mut app = placing_app();
        let area = Rect::new(0, 0, 100, 40);
        let height = app.on_boards_height(area);
        assert!(height > 0);
        app.place.on_boards = json!({"epics": [], "items": []});
        assert_eq!(app.on_boards_height(area), height);
        app.place.on_boards = on_boards();
        assert_eq!(app.on_boards_height(area), height);
        app.place.hide_strip = true;
        assert_eq!(app.on_boards_height(area), 0);
    }

    #[test]
    fn on_boards_polls_send_since_version_and_keep_data_on_unchanged() {
        let mut app = test_app();
        assert_eq!(app.on_boards_params(), json!({}), "first read is full");
        app.place_accept("on_boards", Ok(json!({"version": 4, "epics": [{"board": "swarm", "name": "S3"}], "items": []})));
        assert_eq!(app.on_boards_params(), json!({"since_version": 4}));
        app.place_accept("on_boards", Ok(json!({"unchanged": true, "version": 4})));
        assert_eq!(app.place.on_boards["epics"][0]["name"], "S3");
        // An API without versions always reads in full.
        app.place_accept("on_boards", Ok(json!({"epics": [], "items": []})));
        assert_eq!(app.on_boards_params(), json!({}));
    }

    #[test]
    fn the_dialog_builds_item_place_with_epic_and_member() {
        let mut app = placing_app();
        app.place.dialog = Some(PlaceDialog { task_id: "t1".into(), title: "Org TUI".into(), boards: vec![json!({"slug": "swarm"})], board: 0, epic: 0, holder: Holder::Nobody, field: 1, typing: None, error: String::new(), placed_board: None, placed: None });
        key(&mut app, KeyCode::Right, KeyModifiers::NONE); // epic: Org model
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::Right, KeyModifiers::NONE); // holder: beta (sorted)
        key(&mut app, KeyCode::Right, KeyModifiers::NONE); // holder: ep
        let d = app.place.dialog.clone().unwrap();
        assert_eq!(d.holder, Holder::Member("m1".into()));
        let p = app.place_params(&d);
        assert_eq!((p["task_id"].as_str(), p["board"].as_str(), p["group_id"].as_str(), p["holder"].as_str(), p["role"].as_str()), (Some("t1"), Some("swarm"), Some("g-epic"), Some("ep"), Some("owner")));
        assert!(p.get("replace").is_none());
        // Nobody: no holder field at all.
        let mut d2 = d.clone();
        d2.holder = Holder::Nobody;
        d2.epic = 0;
        let p = app.place_params(&d2);
        assert!(p.get("holder").is_none() && p.get("group_id").is_none());
        // A task on another board moves with `replace`; the same board doesn't.
        d2.placed_board = Some("other".into());
        assert_eq!(app.place_params(&d2)["replace"], true);
        d2.placed_board = Some("swarm".into());
        assert!(app.place_params(&d2).get("replace").is_none());
    }

    #[test]
    fn hiring_types_a_handle_then_places_with_the_new_member() {
        let mut app = placing_app();
        app.place.dialog = Some(PlaceDialog { task_id: "t1".into(), title: "x".into(), boards: vec![json!({"slug": "swarm"})], board: 0, epic: 0, holder: Holder::Hire(String::new(), String::new()), field: 2, typing: None, error: String::new(), placed_board: None, placed: None });
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        for c in "newbie".chars() {
            key(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE); // no title
        assert_eq!(app.place.dialog.as_ref().unwrap().holder, Holder::Hire("newbie".into(), String::new()));
        // An API without org.hire says so in the dialog.
        app.place.after_hire = Some(json!({"task_id": "t1", "board": "swarm"}));
        app.place_accept("hire", Err(RpcFail { unsupported: true, message: "x".into() }));
        assert!(app.place.dialog.as_ref().unwrap().error.contains("Hiring isn't available"));
        assert!(app.place.after_hire.is_none());
    }

    #[test]
    fn placed_reply_closes_the_dialog_and_the_strip_lists_epics_and_items() {
        let mut app = placing_app();
        app.place.dialog = Some(PlaceDialog { task_id: "t1".into(), title: "x".into(), boards: vec![], board: 0, epic: 0, holder: Holder::Nobody, field: 0, typing: None, error: String::new(), placed_board: None, placed: None });
        app.place_accept("place", Ok(json!({"board": "swarm", "n": 15, "item_status": "open", "task_status": "backlog"})));
        assert!(app.place.dialog.is_none());
        let text: Vec<String> = app.on_boards_lines(100).iter().map(|l| l.to_string()).collect();
        assert!(text[1].contains("swarm  Org model  active  PoC ep"), "{text:?}");
        assert!(text[2].contains("swarm #21 board-native chore"), "{text:?}");
        assert!(app.on_boards_height(Rect::new(0, 0, 100, 40)) > 0);
        assert!(key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT));
        assert_eq!(app.on_boards_height(Rect::new(0, 0, 100, 40)), 0);
    }
}
