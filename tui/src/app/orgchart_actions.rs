//! Owner's actions in the F6 org chart (doc/org-model.md §13.4 #2, §11).
//!
//! On a member's box: `A` assigns a backlog task (the shared place dialog,
//! holder preset, app/place.rs) or an open board item (`item.set owner=`
//! the member's main session),
//! `p` hires a new member reporting to them, `m` moves them to another
//! manager or team, `X` retires them, `e` edits their title and mandate,
//! `o` opens their main session. On a team's box the member keys act on its
//! lead (O8: work for a team goes to its lead; a task is placed with the
//! team as holder and the API resolves the lead), `e` edits the charter and
//! standing rules, `E` the lead's profile, and `p` hires into the team. On
//! Owner's box `p` hires a report of Owner's.
//!
//! Every write is an existing org RPC (`org.hire`, `org.move`, `org.retire`,
//! `org.member_update`, `org.team_update`, `item.set`, `item.place`). The
//! viewer is the operator caller, so it acts as Owner, and the daemon and
//! planning API keep every rule: hiring and team edits are Owner's, profile
//! edits Owner's or the manager chain's. The keys are live only with the org
//! model (F6 from teams); the `?` legend lists them and the help line leads
//! with the focused box's.
use super::org_model::{owner_rpc, OrgSnapshot, RpcFail};
use super::orgchart::{ChartModel, NodeKind};
use super::place::Holder;
use super::*;
use crossterm::event::{KeyEvent, KeyEventKind};
use serde_json::{json, Value};
use std::sync::mpsc;

/// §8.2's limits; the API checks them too, with a named error.
const TITLE_MAX: usize = 60;
const MANDATE_MAX: usize = 400;
/// `HANDLE_RE` in dispatch/org_db.py: 2 to 40 of `[a-z0-9-]`.
const HANDLE_MAX: usize = 40;
const REASON_MAX: usize = 500;
/// §8.4: at most ten one-liners per team.
const RULES_MAX: usize = 10;
const RULE_MAX: usize = 200;
/// The API clips a charter here.
const CHARTER_MAX: usize = 20_000;
/// A new hire's box is focused once the snapshot has it, if it does by then.
const FOCUS_WAIT: Duration = Duration::from_secs(30);
/// Rows a long text field wraps to while focused.
const WRAP_ROWS: usize = 4;
/// Label column of a form row.
const LABEL_W: usize = 11;

/// Which write a reply answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Op {
    Hire,
    Move,
    Retire,
    Profile,
    Team,
    Item,
}

/// A reply, with the status line to show when it succeeds.
type Reply = (Op, String, Result<Value, RpcFail>);

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Input {
    /// Text with a cursor (a char index) and a length limit (chars).
    Text { text: String, cursor: usize, max: usize },
    /// `(value, label)` options, and the chosen one.
    Choice { options: Vec<(String, String)>, at: usize },
}

impl Input {
    fn text(s: &str, max: usize) -> Input {
        let text: String = s.chars().take(max).collect();
        Input::Text { cursor: text.chars().count(), text, max }
    }

    fn choice(options: Vec<(String, String)>, value: &str) -> Input {
        let at = options.iter().position(|(v, _)| v == value).unwrap_or(0);
        Input::Choice { options, at }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Field {
    /// What the value is sent as (`handle`, `manager`, `rule`, …).
    pub key: &'static str,
    pub label: String,
    pub input: Input,
}

impl Field {
    pub(super) fn value(&self) -> String {
        match &self.input {
            Input::Text { text, .. } => text.clone(),
            Input::Choice { options, at } => options.get(*at).map(|o| o.0.clone()).unwrap_or_default(),
        }
    }

    fn shown(&self) -> String {
        match &self.input {
            Input::Text { text, .. } => text.clone(),
            Input::Choice { options, at } => options.get(*at).map(|o| o.1.clone()).unwrap_or_default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum FormKind {
    Hire,
    Move { member: String, revision: i64 },
    Retire { member: String },
    Profile { member: String, revision: i64 },
    Team { team: String, revision: i64, rules: Vec<Value> },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Form {
    pub kind: FormKind,
    pub title: String,
    /// The member's handle or the team's name, for the status line.
    pub subject: String,
    /// What the action does, under the title.
    pub note: Vec<String>,
    pub fields: Vec<Field>,
    /// Each field's value when the form opened: only changes are sent.
    initial: Vec<String>,
    pub at: usize,
    pub error: String,
    /// Sent; waiting for the reply.
    pub busy: bool,
}

impl Form {
    fn new(kind: FormKind, title: String, subject: String, note: Vec<String>, fields: Vec<Field>) -> Form {
        let initial = fields.iter().map(Field::value).collect();
        Form { kind, title, subject, note, fields, initial, at: 0, error: String::new(), busy: false }
    }

    fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    pub(super) fn value(&self, key: &str) -> String {
        self.field(key).map(Field::value).unwrap_or_default()
    }

    /// The field's value if it changed since the form opened.
    fn changed(&self, key: &str) -> Option<String> {
        let i = self.fields.iter().position(|f| f.key == key)?;
        let now = self.fields[i].value();
        (self.initial.get(i) != Some(&now)).then_some(now)
    }
}

/// One thing `A` can hand to a member.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Pick {
    /// A backlog task not on a board yet: placed through the place dialog.
    Task { id: String, title: String, project: String },
    /// An open board item: its owner becomes the member's main session.
    Item { board: String, n: i64, title: String, status: String, holders: String },
}

impl Pick {
    fn haystack(&self) -> String {
        match self {
            Pick::Task { title, project, .. } => format!("{title} {project} task").to_lowercase(),
            Pick::Item { board, n, title, holders, .. } => format!("{board} #{n} {title} {holders}").to_lowercase(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Picker {
    /// Who gets it: a member id and handle (on a team's box, its lead).
    pub member: String,
    pub handle: String,
    /// The team's `(id, name)` when the box was the team's (O8): a task is
    /// placed with the team as holder, an item goes to the lead's session.
    pub team: Option<(String, String)>,
    /// The member's main session pid: board items are held by session.
    pub pid: Option<String>,
    pub rows: Vec<Pick>,
    pub query: String,
    /// Index into the filtered rows.
    pub at: usize,
    pub error: String,
    pub busy: bool,
}

impl Picker {
    pub(super) fn visible(&self) -> Vec<usize> {
        let q = self.query.trim().to_lowercase();
        (0..self.rows.len()).filter(|&i| q.is_empty() || self.rows[i].haystack().contains(&q)).collect()
    }
}

#[derive(Default)]
pub(super) struct Actions {
    pub(super) form: Option<Form>,
    pub(super) picker: Option<Picker>,
    rx: Vec<mpsc::Receiver<Reply>>,
    seq: u64,
    /// The place dialog is open over the chart (`A` on a backlog task),
    /// with the status message's time when it opened: a newer one is the
    /// place's result.
    pub(super) placing: Option<Option<Instant>>,
    /// A new hire's member id, and when: its box is focused once it shows.
    pub(super) focus_hire: Option<(String, Instant)>,
    /// The last write sent, as `(method, params)`.
    pub(super) last_sent: Option<(&'static str, Value)>,
}

/// What the focused box stands for.
#[derive(Clone, Debug, PartialEq)]
enum Target {
    Owner,
    /// A member's box (also a guest's box or a report link).
    Member(String),
    /// A team's box, and its lead (never Owner or a retired member).
    Team { team: String, lead: Option<String> },
}

fn target(snap: &OrgSnapshot, model: &ChartModel, focus: usize) -> Option<Target> {
    let node = model.nodes.get(focus)?;
    match node.kind {
        NodeKind::Owner => Some(Target::Owner),
        NodeKind::Member => node.card.as_ref()?.member_id.clone().filter(|id| snap.member(id).is_some_and(|m| !m.retired() && !m.is_owner())).map(Target::Member),
        NodeKind::Team => {
            let team = snap.team(node.card.as_ref()?.team_id.as_deref()?)?;
            let lead = team.lead_member_id.as_deref().and_then(|l| snap.member(l)).filter(|l| !l.is_owner() && !l.retired()).map(|l| l.id.clone());
            Some(Target::Team { team: team.id.clone(), lead })
        }
        _ => None,
    }
}

/// Managers to choose from: Owner, then every current member by handle,
/// less `except`.
fn managers(snap: &OrgSnapshot, except: Option<&str>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = snap.members.iter().filter(|m| !m.is_owner() && !m.retired() && Some(m.id.as_str()) != except).map(|m| (m.id.clone(), m.handle.clone())).collect();
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    out.insert(0, ("owner".into(), "Owner".into()));
    out
}

/// Teams to choose from (not archived) by name, after `none` when `none`.
fn teams(snap: &OrgSnapshot, none: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = snap.teams.iter().filter(|t| !t.archived()).map(|t| (t.id.clone(), t.name.clone())).collect();
    out.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
    if none {
        out.insert(0, (String::new(), "none".into()));
    }
    out
}

/// A rule's text: `{text, source}`, or a bare string from an older writer.
fn rule_text(r: &Value) -> String {
    r["text"].as_str().or(r.as_str()).unwrap_or("").to_owned()
}

fn valid_handle(h: &str) -> bool {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    (2..=HANDLE_MAX).contains(&h.len()) && h.chars().all(|c| ok(c) || c == '-') && h.starts_with(ok) && h.ends_with(ok)
}

/// `?`, last line: the keys this module adds.
pub(super) fn chart_actions_legend_line() -> Line<'static> {
    let key = Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD);
    let muted = Style::default().fg(theme::MUTED);
    let mut spans = vec![Span::styled("Owner actions  ", Style::default().fg(theme::HEADER))];
    for (k, what) in [("A", "assign"), ("p", "hire"), ("m", "move"), ("X", "retire"), ("e", "edit profile"), ("o", "main session")] {
        spans.push(Span::styled(k, key));
        spans.push(Span::styled(format!(" {what}  "), muted));
    }
    spans.push(Span::styled("\u{00b7} on a team box ", muted));
    spans.push(Span::styled("A m X o E", key));
    spans.push(Span::styled(" act on its lead, ", muted));
    spans.push(Span::styled("e", key));
    spans.push(Span::styled(" edits charter and rules", muted));
    Line::from(spans)
}

impl App {
    /// The chart closed: drop whatever was open over it.
    pub(super) fn chart_actions_close(&mut self) {
        let a = &mut self.orgchart.actions;
        a.form = None;
        a.picker = None;
        a.focus_hire = None;
        if a.placing.take().is_some() {
            self.place.dialog = None;
        }
    }

    /// The focused box's actions, for the help line.
    pub(super) fn chart_action_hints(&self, model: &ChartModel, focus: usize) -> String {
        let Some(snap) = self.org_live() else { return String::new() };
        match target(snap, model, focus) {
            Some(Target::Owner) => "p hire".into(),
            Some(Target::Member(_)) => "A assign  p hire  m move  X retire  e edit  o main".into(),
            Some(Target::Team { lead: Some(_), .. }) => "A assign (lead)  p hire  e team  E lead  m/X/o lead".into(),
            Some(Target::Team { lead: None, .. }) => "p hire  e team".into(),
            None => String::new(),
        }
    }

    fn chart_request_id(&mut self, what: &str) -> String {
        self.orgchart.actions.seq += 1;
        format!("tui-f6-{what}-{}-{}", board::now_unix(), self.orgchart.actions.seq)
    }

    fn chart_request(&mut self, op: Op, method: &'static str, params: Value, done: String) {
        self.orgchart.actions.last_sent = Some((method, params.clone()));
        let targets = self.owner_rpc_targets();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send((op, done, owner_rpc(&targets, method, &params)));
        });
        self.orgchart.actions.rx.push(rx);
    }

    // ---- keys -------------------------------------------------------------

    /// `A p m X e E o` on the focused box (orgchart.rs routes them here
    /// while the org model is live).
    pub(super) fn chart_action(&mut self, model: &ChartModel, focus: usize, c: char) {
        let Some(snap) = self.org_live().cloned() else { return };
        let Some(t) = target(&snap, model, focus) else {
            self.orgchart.status = "Owner actions work on a member's, a team's or Owner's box".into();
            return;
        };
        let team_name = |id: &str| snap.team(id).map_or_else(|| id.to_owned(), |t| t.name.clone());
        match (c, &t) {
            ('p', _) => self.open_hire(&snap, &t),
            ('e', Target::Team { team, .. }) => self.open_team_form(&snap, team),
            ('e' | 'E', Target::Member(m)) => self.open_profile(&snap, m),
            (_, Target::Owner) => self.orgchart.status = "On Owner's box: p hires a report of Owner's".into(),
            (_, Target::Team { team, lead: None }) => {
                let owner_led = snap.team(team).and_then(|t| t.lead_member_id.as_deref()).and_then(|l| snap.member(l)).is_some_and(|l| l.is_owner());
                self.orgchart.status = if owner_led { format!("Owner leads {}", team_name(team)) } else { format!("{} has no lead yet", team_name(team)) };
            }
            (_, Target::Member(m)) | (_, Target::Team { lead: Some(m), .. }) => {
                let via = match &t {
                    Target::Team { team, .. } => Some((team.clone(), team_name(team))),
                    _ => None,
                };
                match c {
                    'A' => self.open_assign(&snap, m, via),
                    'm' => self.open_move(&snap, m),
                    'X' => self.open_retire(&snap, m, via.map(|v| v.1)),
                    'E' => self.open_profile(&snap, m),
                    'o' => self.chart_open_main(&snap, model, m),
                    _ => {}
                }
            }
        }
    }

    /// Keys while a form, the picker or the place dialog is open: true when
    /// one took the key.
    pub(super) fn chart_actions_key(&mut self, key: &KeyEvent) -> bool {
        if self.orgchart.actions.placing.is_some() && self.place.dialog.is_some() {
            if key.kind != KeyEventKind::Release {
                self.place_dialog_key(key);
            }
            return true;
        }
        if self.orgchart.actions.picker.is_some() {
            if key.kind != KeyEventKind::Release {
                self.picker_key(key);
            }
            return true;
        }
        if self.orgchart.actions.form.is_some() {
            if key.kind != KeyEventKind::Release {
                self.form_key(key);
            }
            return true;
        }
        false
    }

    /// Bracketed paste into the focused text field or the picker's filter.
    pub(super) fn chart_actions_paste(&mut self, pasted: &str) {
        let a = &mut self.orgchart.actions;
        if let Some(p) = a.picker.as_mut() {
            p.query.push_str(&pasted.replace(['\n', '\r'], " "));
            p.at = 0;
        } else if let Some(f) = a.form.as_mut() {
            let field = &mut f.fields[f.at];
            let key = field.key;
            for c in pasted.chars() {
                text_insert(&mut field.input, key, c);
            }
            grow_rules(f);
        }
        self.needs_redraw = true;
    }

    fn form_key(&mut self, key: &KeyEvent) {
        let Some(mut f) = self.orgchart.actions.form.take() else { return };
        let n = f.fields.len().max(1);
        match key.code {
            KeyCode::Esc => {
                self.orgchart.status = if f.busy { format!("{}: the reply will show here", f.title) } else { String::new() };
                return;
            }
            KeyCode::Tab | KeyCode::Down => f.at = (f.at + 1) % n,
            KeyCode::BackTab | KeyCode::Up => f.at = (f.at + n - 1) % n,
            KeyCode::Enter if f.busy => {}
            KeyCode::Enter => {
                match self.form_params(&f) {
                    Ok((op, method, params, done)) => {
                        f.error.clear();
                        f.busy = true;
                        self.chart_request(op, method, params, done);
                    }
                    Err(e) => f.error = e,
                }
            }
            _ => {
                if let Some(field) = f.fields.get_mut(f.at) {
                    let k = field.key;
                    edit(&mut field.input, k, key);
                }
                grow_rules(&mut f);
            }
        }
        self.orgchart.actions.form = Some(f);
    }

    fn picker_key(&mut self, key: &KeyEvent) {
        let Some(mut p) = self.orgchart.actions.picker.take() else { return };
        let n = p.visible().len();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return,
            KeyCode::Down | KeyCode::Tab => p.at = (p.at + 1).min(n.saturating_sub(1)),
            KeyCode::Char('j' | 'n') if ctrl => p.at = (p.at + 1).min(n.saturating_sub(1)),
            KeyCode::Up | KeyCode::BackTab => p.at = p.at.saturating_sub(1),
            KeyCode::Char('k' | 'p') if ctrl => p.at = p.at.saturating_sub(1),
            KeyCode::PageDown => p.at = (p.at + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => p.at = p.at.saturating_sub(10),
            KeyCode::Backspace => {
                p.query.pop();
                p.at = 0;
            }
            KeyCode::Char('u') if ctrl => {
                p.query.clear();
                p.at = 0;
            }
            KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                p.query.push(c);
                p.at = 0;
            }
            KeyCode::Enter if p.busy => {}
            KeyCode::Enter => {
                let pick = p.visible().get(p.at).map(|&i| p.rows[i].clone());
                match pick {
                    None => p.error = "Nothing to assign".into(),
                    Some(Pick::Task { id, title, .. }) => {
                        // The shared place dialog, with the member as holder.
                        let since = self.status_msg.as_ref().map(|(_, t)| *t);
                        self.open_place_dialog(id, title, None);
                        if let Some(d) = self.place.dialog.as_mut() {
                            d.holder = match &p.team {
                                Some((team, _)) => Holder::Team(team.clone()),
                                None => Holder::Member(p.member.clone()),
                            };
                        }
                        self.orgchart.actions.placing = Some(since);
                        return;
                    }
                    Some(Pick::Item { board, n, .. }) => match p.pid.clone() {
                        None => p.error = format!("{} has no session yet, so it can't hold a board item", p.handle),
                        Some(pid) => {
                            p.error.clear();
                            p.busy = true;
                            let params = json!({"board": board, "n": n, "owner": pid, "reason": format!("assigned to {} in F6", p.handle)});
                            self.chart_request(Op::Item, "item.set", params, format!("{board} #{n} \u{2192} {}", p.handle));
                        }
                    },
                }
            }
            _ => {}
        }
        self.orgchart.actions.picker = Some(p);
    }

    // ---- opening ----------------------------------------------------------

    fn open_hire(&mut self, snap: &OrgSnapshot, t: &Target) {
        let (manager, team) = match t {
            Target::Owner => ("owner".to_owned(), None),
            Target::Member(m) => (m.clone(), snap.member(m).and_then(|x| x.primary_team_id.clone())),
            Target::Team { team, lead } => (lead.clone().unwrap_or_else(|| "owner".into()), Some(team.clone())),
        };
        let title = match (t, team.as_deref().and_then(|id| snap.team(id))) {
            (Target::Team { .. }, Some(team)) => format!("Hire into {}", team.name),
            _ if manager == "owner" => "Hire a report of Owner's".to_owned(),
            _ => format!("Hire a report of {}'s", snap.handle(&manager)),
        };
        let fields = vec![
            Field { key: "handle", label: "handle".into(), input: Input::text("", HANDLE_MAX) },
            Field { key: "title", label: "title".into(), input: Input::text("", TITLE_MAX) },
            Field { key: "mandate", label: "mandate".into(), input: Input::text("", MANDATE_MAX) },
            Field { key: "manager", label: "manager".into(), input: Input::choice(managers(snap, None), &manager) },
            Field { key: "team", label: "team".into(), input: Input::choice(teams(snap, true), team.as_deref().unwrap_or("")) },
        ];
        let note = vec!["A new member with no session yet: an empty box under its manager until a session takes the seat.".to_owned()];
        self.orgchart.actions.form = Some(Form::new(FormKind::Hire, title, String::new(), note, fields));
    }

    fn open_move(&mut self, snap: &OrgSnapshot, member: &str) {
        let Some(x) = snap.member(member) else { return };
        let fields = vec![
            Field { key: "manager", label: "manager".into(), input: Input::choice(managers(snap, Some(member)), x.manager_member_id.as_deref().filter(|m| snap.member(m).is_some_and(|m| !m.is_owner())).unwrap_or("owner")) },
            Field { key: "team", label: "team".into(), input: Input::choice(teams(snap, true), x.primary_team_id.as_deref().unwrap_or("")) },
            Field { key: "reason", label: "reason".into(), input: Input::text("", REASON_MAX) },
        ];
        let note = vec!["A new manager or primary team; their sessions and held items stay with them.".to_owned()];
        let kind = FormKind::Move { member: x.id.clone(), revision: x.revision };
        self.orgchart.actions.form = Some(Form::new(kind, format!("Move {}", x.handle), x.handle.clone(), note, fields));
    }

    fn open_retire(&mut self, snap: &OrgSnapshot, member: &str, lead_of: Option<String>) {
        let Some(x) = snap.member(member) else { return };
        let up = x.manager_member_id.as_deref().map_or_else(|| "Owner".to_owned(), |m| snap.handle(m));
        let mut note = vec![format!("Their reports and open board items move up to {up}; their sessions keep running. Owner can reinstate them.")];
        if let Some(team) = lead_of {
            note.push(format!("{} leads {team}: the team has no lead until Owner names one.", x.handle));
        }
        let fields = vec![Field { key: "reason", label: "reason".into(), input: Input::text("", REASON_MAX) }];
        let kind = FormKind::Retire { member: x.id.clone() };
        self.orgchart.actions.form = Some(Form::new(kind, format!("Retire {}", x.handle), x.handle.clone(), note, fields));
    }

    fn open_profile(&mut self, snap: &OrgSnapshot, member: &str) {
        let Some(x) = snap.member(member) else { return };
        let fields = vec![
            Field { key: "title", label: "title".into(), input: Input::text(x.title.as_deref().unwrap_or(""), TITLE_MAX) },
            Field { key: "mandate", label: "mandate".into(), input: Input::text(x.mandate.as_deref().unwrap_or(""), MANDATE_MAX) },
        ];
        let note = vec!["Authored by Owner or the manager chain (§8.2); an empty field clears it.".to_owned()];
        let kind = FormKind::Profile { member: x.id.clone(), revision: x.revision };
        self.orgchart.actions.form = Some(Form::new(kind, format!("{}'s profile", x.handle), x.handle.clone(), note, fields));
    }

    fn open_team_form(&mut self, snap: &OrgSnapshot, team: &str) {
        let Some(t) = snap.team(team) else { return };
        let mut fields = vec![Field { key: "charter", label: "charter".into(), input: Input::text(t.charter.as_deref().unwrap_or(""), CHARTER_MAX) }];
        for (i, r) in t.standing_rules.iter().enumerate() {
            fields.push(Field { key: "rule", label: format!("rule {}", i + 1), input: Input::text(&rule_text(r), RULE_MAX) });
        }
        let note = vec![format!("Standing rules: at most {RULES_MAX} one-liners; empty a rule to drop it. Ctrl+J starts a new charter line.")];
        let kind = FormKind::Team { team: t.id.clone(), revision: t.revision, rules: t.standing_rules.clone() };
        let mut f = Form::new(kind, format!("{}: charter and rules", t.name), t.name.clone(), note, fields);
        grow_rules(&mut f);
        self.orgchart.actions.form = Some(f);
    }

    fn open_assign(&mut self, snap: &OrgSnapshot, member: &str, team: Option<(String, String)>) {
        let Some(x) = snap.member(member) else { return };
        let pid = x.main.as_ref().map(|m| m.pid.clone()).filter(|p| !p.is_empty());
        let mut rows: Vec<Pick> = self.planning.backlog_tasks().into_iter().map(|(id, title, project)| Pick::Task { id, title, project }).collect();
        for (board, items) in &self.orgchart.boards.items {
            for it in items {
                let status = it["status"].as_str().unwrap_or("").to_owned();
                if matches!(status.as_str(), "done" | "dropped") {
                    continue;
                }
                let holders: Vec<&Value> = it["holders"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
                let theirs = holders.iter().any(|h| h["member_id"] == x.id.as_str() || (pid.is_some() && h["pid"].as_str() == pid.as_deref()) || x.main_uid().is_some_and(|u| h["session_uid"] == u));
                let Some(n) = it["n"].as_i64() else { continue };
                if theirs {
                    continue;
                }
                let names: Vec<&str> = holders.iter().filter_map(|h| h["name"].as_str()).collect();
                rows.push(Pick::Item { board: board.clone(), n, title: it["title"].as_str().unwrap_or("").to_owned(), status, holders: names.join(", ") });
            }
        }
        let error = if rows.is_empty() { "Nothing to assign: no backlog task off a board, no open board item".to_owned() } else { String::new() };
        self.orgchart.actions.picker = Some(Picker { member: x.id.clone(), handle: x.handle.clone(), team, pid, rows, query: String::new(), at: 0, error, busy: false });
    }

    /// `o`: the Sessions view on the member's main session.
    fn chart_open_main(&mut self, snap: &OrgSnapshot, model: &ChartModel, member: &str) {
        let Some(x) = snap.member(member) else { return };
        let Some(uid) = x.main_uid() else {
            self.orgchart.status = format!("{} has no session yet", x.handle);
            return;
        };
        let at = model.nodes.iter().position(|n| n.sessions.iter().any(|s| s.uid == uid));
        let Some(i) = at else {
            let why = if self.find_session_by_uid(uid).is_some() { "isn't on the chart (b shows bash)" } else { "is on a host this viewer doesn't show" };
            self.orgchart.status = format!("{}'s main session {why}", x.handle);
            return;
        };
        self.orgchart_reveal(model, i);
        self.orgchart_set_focus(model, i);
        self.orgchart.session = Some(uid.to_owned());
        self.orgchart_jump(model, true);
    }

    // ---- submitting -------------------------------------------------------

    /// The write a form sends, or why it can't yet.
    pub(super) fn form_params(&mut self, f: &Form) -> Result<(Op, &'static str, Value, String), String> {
        let reason = |f: &Form, default: &str| {
            let r = f.value("reason");
            if r.trim().is_empty() { default.to_owned() } else { r.trim().to_owned() }
        };
        match &f.kind {
            FormKind::Hire => {
                let handle = f.value("handle").trim().to_owned();
                if !valid_handle(&handle) {
                    return Err("A handle is 2-40 of a-z, 0-9 and -, starting and ending with a letter or digit".into());
                }
                let manager = f.value("manager");
                let rid = self.chart_request_id("hire");
                let mut p = json!({"handle": handle, "kind": "agent", "manager": manager, "reason": "hired in F6", "request_id": rid});
                for k in ["title", "mandate", "team"] {
                    let v = f.value(k);
                    if !v.trim().is_empty() {
                        p[k] = json!(v.trim());
                    }
                }
                let under = f.field("manager").map(Field::shown).unwrap_or_default();
                Ok((Op::Hire, "org.hire", p, format!("Hired {handle} under {under}: no session yet")))
            }
            FormKind::Move { member, revision } => {
                let rid = self.chart_request_id("move");
                let mut p = json!({"member": member, "expected_revision": revision, "reason": reason(f, "moved in F6"), "request_id": rid});
                let mut what = Vec::new();
                if let Some(m) = f.changed("manager") {
                    p["manager"] = json!(m);
                    what.push(format!("under {}", f.field("manager").map(Field::shown).unwrap_or_default()));
                }
                if let Some(t) = f.changed("team") {
                    // An explicit null clears the primary team.
                    p["primary_team_id"] = if t.is_empty() { Value::Null } else { json!(t) };
                    what.push(if t.is_empty() { "out of its team".to_owned() } else { format!("to {}", f.field("team").map(Field::shown).unwrap_or_default()) });
                }
                if what.is_empty() {
                    return Err("Nothing changed: pick another manager or team".into());
                }
                Ok((Op::Move, "org.move", p, format!("Moved {} {}", f.subject, what.join(" and "))))
            }
            FormKind::Retire { member } => {
                let rid = self.chart_request_id("retire");
                let p = json!({"member": member, "reason": reason(f, "retired in F6"), "request_id": rid});
                Ok((Op::Retire, "org.retire", p, format!("Retired {}", f.subject)))
            }
            FormKind::Profile { member, revision } => {
                let rid = self.chart_request_id("profile");
                let mut p = json!({"member": member, "expected_revision": revision, "reason": "edited in F6", "request_id": rid});
                let mut any = false;
                for k in ["title", "mandate"] {
                    if let Some(v) = f.changed(k) {
                        p[k] = json!(v.trim());
                        any = true;
                    }
                }
                if !any {
                    return Err("Nothing changed".into());
                }
                Ok((Op::Profile, "org.member_update", p, format!("Saved {}'s profile", f.subject)))
            }
            FormKind::Team { team, revision, rules } => {
                let rid = self.chart_request_id("team");
                let mut p = json!({"team": team, "expected_revision": revision, "reason": "edited in F6", "request_id": rid});
                let mut any = false;
                if let Some(c) = f.changed("charter") {
                    p["charter"] = json!(c);
                    any = true;
                }
                // A rule kept as it was keeps its source; a new or edited one is Owner's.
                let texts: Vec<String> = f.fields.iter().filter(|x| x.key == "rule").map(Field::value).collect();
                let mut new_rules = Vec::new();
                for (i, text) in texts.iter().enumerate() {
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    match rules.get(i) {
                        Some(r) if rule_text(r) == text => new_rules.push(r.clone()),
                        _ => new_rules.push(json!({"text": text, "source": "owner"})),
                    }
                }
                if new_rules.len() > RULES_MAX {
                    return Err(format!("At most {RULES_MAX} standing rules"));
                }
                if new_rules != *rules {
                    p["standing_rules"] = json!(new_rules);
                    any = true;
                }
                if !any {
                    return Err("Nothing changed".into());
                }
                Ok((Op::Team, "org.team_update", p, format!("Saved {}'s charter and rules", f.subject)))
            }
        }
    }

    /// Apply a reply (also the test entry point).
    pub(super) fn chart_actions_accept(&mut self, op: Op, done: String, r: Result<Value, RpcFail>) {
        self.needs_redraw = true;
        match r {
            Ok(v) => {
                if op == Op::Item {
                    if self.orgchart.actions.picker.as_ref().is_some_and(|p| p.busy) {
                        self.orgchart.actions.picker = None;
                    }
                    if let Some(board) = v["board"].as_str() {
                        self.orgchart.boards.reread(board);
                    }
                } else {
                    if self.orgchart.actions.form.as_ref().is_some_and(|f| f.busy) {
                        self.orgchart.actions.form = None;
                    }
                    self.org_refresh_soon();
                }
                if op == Op::Hire {
                    if let Some(id) = v["member"]["id"].as_str() {
                        self.orgchart.actions.focus_hire = Some((id.to_owned(), Instant::now()));
                    }
                }
                self.orgchart.status = done;
            }
            Err(f) => {
                let msg = if f.unsupported { "This needs a newer daemon or planning API".to_owned() } else { f.message };
                let a = &mut self.orgchart.actions;
                if let Some(p) = a.picker.as_mut().filter(|p| op == Op::Item && p.busy) {
                    p.busy = false;
                    p.error = msg;
                } else if let Some(form) = a.form.as_mut().filter(|f| op != Op::Item && f.busy) {
                    form.busy = false;
                    form.error = msg;
                } else {
                    self.orgchart.status = msg;
                }
            }
        }
    }

    /// Replies, the place dialog closing, and a new hire's box appearing.
    pub(super) fn chart_actions_tick(&mut self) {
        let mut ready = Vec::new();
        self.orgchart.actions.rx.retain(|rx| match rx.try_recv() {
            Ok(reply) => {
                ready.push(reply);
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => false,
        });
        for (op, done, r) in ready {
            self.chart_actions_accept(op, done, r);
        }
        if let Some(since) = self.orgchart.actions.placing {
            if self.place.dialog.is_none() {
                // Placed (place.rs said so in the status message) or cancelled.
                self.orgchart.actions.placing = None;
                self.orgchart.status = self.status_msg.as_ref().filter(|(_, t)| since.is_none_or(|s| *t > s)).map(|(m, _)| m.clone()).unwrap_or_default();
                self.needs_redraw = true;
            }
        }
        if let Some((id, at)) = self.orgchart.actions.focus_hire.clone() {
            if at.elapsed() > FOCUS_WAIT || !self.orgchart.visible {
                self.orgchart.actions.focus_hire = None;
            } else if self.org_live().is_some_and(|s| s.member(&id).is_some()) {
                self.orgchart.actions.focus_hire = None;
                let model = self.org_model(self.orgchart.show_bash);
                if let Some(i) = model.find(&format!("member:{id}")) {
                    self.orgchart_reveal(&model, i);
                    self.orgchart_set_focus(&model, i);
                    self.needs_redraw = true;
                }
            }
        }
    }

    // ---- drawing ----------------------------------------------------------

    pub(super) fn draw_chart_actions(&self, frame: &mut Frame, area: Rect) {
        let a = &self.orgchart.actions;
        if a.placing.is_some() && self.place.dialog.is_some() {
            self.draw_place(frame, area);
        } else if let Some(p) = &a.picker {
            draw_picker(frame, area, p);
        } else if let Some(f) = &a.form {
            draw_form(frame, area, f);
        }
    }
}

/// A key on a field.
fn edit(input: &mut Input, key_name: &str, key: &KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match input {
        Input::Choice { options, at } => {
            let n = options.len().max(1);
            match key.code {
                KeyCode::Left => *at = (*at + n - 1) % n,
                KeyCode::Right | KeyCode::Char(' ') => *at = (*at + 1) % n,
                KeyCode::Char(c) if !ctrl => {
                    // Jump to the next option starting with the letter.
                    let c = c.to_lowercase().next().unwrap_or(c);
                    let hit = (1..=n).map(|k| (*at + k) % n).find(|&i| options[i].1.to_lowercase().starts_with(c));
                    if let Some(i) = hit {
                        *at = i;
                    }
                }
                _ => {}
            }
        }
        Input::Text { .. } => {
            let typed = match key.code {
                KeyCode::Char('j') if ctrl && key_name == "charter" => Some('\n'),
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => Some(c),
                _ => None,
            };
            if let Some(c) = typed {
                text_insert(input, key_name, c);
                return;
            }
            let Input::Text { text, cursor, .. } = input else { return };
            let len = text.chars().count();
            match key.code {
                KeyCode::Left => *cursor = cursor.saturating_sub(1),
                KeyCode::Right => *cursor = (*cursor + 1).min(len),
                KeyCode::Home => *cursor = 0,
                KeyCode::End => *cursor = len,
                KeyCode::Char('a') if ctrl => *cursor = 0,
                KeyCode::Char('e') if ctrl => *cursor = len,
                KeyCode::Char('u') if ctrl => {
                    text.clear();
                    *cursor = 0;
                }
                KeyCode::Backspace if *cursor > 0 => {
                    let b = byte_at(text, *cursor - 1);
                    text.remove(b);
                    *cursor -= 1;
                }
                KeyCode::Delete if *cursor < len => {
                    let b = byte_at(text, *cursor);
                    text.remove(b);
                }
                _ => {}
            }
        }
    }
}

fn byte_at(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map_or(s.len(), |(b, _)| b)
}

/// Type `c` at the cursor, within the field's limit. A handle takes
/// lowercase letters, digits and `-` only (a space types `-`).
fn text_insert(input: &mut Input, key_name: &str, c: char) {
    let Input::Text { text, cursor, max } = input else { return };
    let c = if key_name == "handle" {
        match c.to_ascii_lowercase() {
            ' ' | '_' => '-',
            c if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' => c,
            _ => return,
        }
    } else if c == '\n' && key_name != "charter" {
        ' '
    } else if c.is_control() && c != '\n' {
        return;
    } else {
        c
    };
    if text.chars().count() >= *max {
        return;
    }
    let b = byte_at(text, *cursor);
    text.insert(b, c);
    *cursor += 1;
}

/// A team form keeps one empty "add rule" field while it has room.
fn grow_rules(f: &mut Form) {
    if !matches!(f.kind, FormKind::Team { .. }) {
        return;
    }
    let rules = f.fields.iter().filter(|x| x.key == "rule").count();
    let last_empty = f.fields.last().is_some_and(|x| x.key == "rule" && x.value().is_empty());
    if rules < RULES_MAX && !last_empty {
        f.fields.push(Field { key: "rule", label: "add rule".into(), input: Input::text("", RULE_MAX) });
        f.initial.push(String::new());
    }
}

fn centred(area: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(area.width), h.min(area.height));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

/// A text value as one row of `w` cells: newlines as ⏎, the cursor shown
/// (reversed) when focused, scrolled so it stays in view.
fn text_spans(text: &str, cursor: Option<usize>, w: usize, base: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().map(|c| if c == '\n' { '\u{23ce}' } else { c }).collect();
    let Some(cur) = cursor else {
        if chars.is_empty() {
            return vec![Span::styled("\u{2014}", Style::default().fg(theme::DIM))];
        }
        let s: String = chars.iter().collect();
        return vec![Span::styled(crate::planning::truncate_with_ellipsis(&s, w), base)];
    };
    let w = w.max(2);
    let start = cur.saturating_sub(w - 1);
    let end = (start + w - 1).min(chars.len());
    let before: String = chars[start..cur.min(end)].iter().collect();
    let at = chars.get(cur).copied().unwrap_or(' ');
    let after: String = chars.get(cur + 1..end.max(cur + 1)).map(|s| s.iter().collect()).unwrap_or_default();
    vec![Span::styled(before, base), Span::styled(at.to_string(), base.add_modifier(Modifier::REVERSED)), Span::styled(after, base)]
}

fn draw_form(frame: &mut Frame, area: Rect, f: &Form) {
    let w = 88.min(area.width);
    let inner_w = w.saturating_sub(2) as usize;
    let value_w = inner_w.saturating_sub(LABEL_W + 1);
    let dim = Style::default().fg(theme::DIM);
    let text = Style::default().fg(theme::TEXT);
    let mut lines: Vec<Line> = f.note.iter().map(|n| Line::styled(format!(" {}", crate::planning::truncate_with_ellipsis(n, inner_w.saturating_sub(1))), Style::default().fg(theme::MUTED))).collect();
    lines.push(Line::from(""));
    for (i, field) in f.fields.iter().enumerate() {
        let focused = i == f.at;
        let label_style = if focused { Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD) } else { dim };
        let label = Span::styled(format!(" {:<w$}", crate::planning::truncate_with_ellipsis(&field.label, LABEL_W - 1), w = LABEL_W - 1), label_style);
        match &field.input {
            Input::Choice { .. } => {
                let st = if focused { text.bg(theme::SELECT_BG) } else { text };
                lines.push(Line::from(vec![label, Span::styled(format!("\u{25c2} {} \u{25b8}", field.shown()), st)]));
            }
            Input::Text { text: t, cursor, max } => {
                let count = format!(" {}/{max}", t.chars().count());
                if !focused {
                    lines.push(Line::from([vec![label], text_spans(t, None, value_w, text)].concat()));
                } else if *max <= 100 || t.chars().count() < value_w.saturating_sub(count.len()) {
                    let mut spans = vec![label];
                    spans.extend(text_spans(t, Some(*cursor), value_w.saturating_sub(count.len()), text));
                    spans.push(Span::styled(count, dim));
                    lines.push(Line::from(spans));
                } else {
                    // Long text: wrapped, the rows up to the cursor's.
                    let chars: Vec<char> = t.chars().map(|c| if c == '\n' { '\u{23ce}' } else { c }).chain(std::iter::once(' ')).collect();
                    let row_w = value_w.max(8);
                    let rows: Vec<&[char]> = chars.chunks(row_w).collect();
                    let cur_row = cursor / row_w;
                    let first = (cur_row + 1).saturating_sub(WRAP_ROWS);
                    for (r, chunk) in rows.iter().enumerate().skip(first).take(WRAP_ROWS) {
                        let lead = if r == first { label.clone() } else { Span::raw(" ".repeat(LABEL_W)) };
                        let mut spans = vec![lead];
                        for (k, c) in chunk.iter().enumerate() {
                            let st = if r * row_w + k == *cursor { text.add_modifier(Modifier::REVERSED) } else { text };
                            spans.push(Span::styled(c.to_string(), st));
                        }
                        if r == cur_row {
                            spans.push(Span::styled(count.clone(), dim));
                        }
                        lines.push(Line::from(spans));
                    }
                }
            }
        }
    }
    lines.push(if f.busy {
        Line::styled(" Saving\u{2026}", Style::default().fg(theme::ATTN))
    } else if !f.error.is_empty() {
        Line::styled(format!(" {}", crate::planning::truncate_with_ellipsis(&f.error, inner_w.saturating_sub(1))), Style::default().fg(theme::ERROR))
    } else {
        Line::from("")
    });
    let enter = match f.kind {
        FormKind::Retire { .. } => "Enter retires",
        FormKind::Hire => "Enter hires",
        FormKind::Move { .. } => "Enter moves",
        _ => "Enter saves",
    };
    lines.push(Line::styled(format!(" Tab/\u{2191}\u{2193} field \u{00b7} \u{2190}/\u{2192} choose or move \u{00b7} Ctrl+U clear \u{00b7} {enter} \u{00b7} Esc cancel"), dim));
    let r = centred(area, w, lines.len() as u16 + 2);
    frame.render_widget(Clear, r);
    let block = Block::default().borders(Borders::ALL).title(format!(" {} ", f.title)).border_style(Style::default().fg(theme::ATTN));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_picker(frame: &mut Frame, area: Rect, p: &Picker) {
    let w = 96.min(area.width);
    let h = 24.min(area.height.saturating_sub(2)).max(8);
    let r = centred(area, w, h);
    frame.render_widget(Clear, r);
    let who = match &p.team {
        Some((_, team)) => format!(" Assign to {team} (lead {}) ", p.handle),
        None => format!(" Assign to {} ", p.handle),
    };
    let block = Block::default().borders(Borders::ALL).title(who).border_style(Style::default().fg(theme::ATTN));
    let inner = block.inner(r);
    frame.render_widget(block, r);
    let width = inner.width as usize;
    let dim = Style::default().fg(theme::DIM);
    let mut lines = vec![Line::from(vec![Span::styled(" filter ", dim), Span::styled(format!("{}\u{2588}", p.query), Style::default().fg(theme::TEXT))])];
    let shown = p.visible();
    let room = (inner.height as usize).saturating_sub(3);
    let first = p.at.saturating_sub(room.saturating_sub(1));
    if shown.is_empty() && p.error.is_empty() {
        lines.push(Line::styled(" Nothing matches", dim));
    }
    for (k, &i) in shown.iter().enumerate().skip(first).take(room) {
        let sel = k == p.at;
        let base = if sel { Style::default().fg(theme::TEXT).bg(theme::SELECT_BG).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme::TEXT) };
        let line = match &p.rows[i] {
            Pick::Task { title, project, .. } => {
                let text = format!(" task  {title}  \u{00b7} {project} backlog");
                Line::from(vec![Span::styled(crate::planning::truncate_with_ellipsis(&text, width), base)])
            }
            Pick::Item { board, n, title, status, holders } => {
                let (mark, st) = symbols::board_status(status);
                let held = if holders.is_empty() { String::new() } else { format!("  \u{00b7} {holders}") };
                let text = format!(" {board} #{n}  {title}{held}");
                Line::from(vec![Span::styled(format!(" {mark}"), if sel { st.bg(theme::SELECT_BG) } else { st }), Span::styled(crate::planning::truncate_with_ellipsis(&text, width.saturating_sub(2)), base)])
            }
        };
        lines.push(line);
    }
    let status_row = inner.height.saturating_sub(2) as usize;
    while lines.len() < status_row {
        lines.push(Line::from(""));
    }
    lines.push(if p.busy {
        Line::styled(" Assigning\u{2026}", Style::default().fg(theme::ATTN))
    } else if !p.error.is_empty() {
        Line::styled(format!(" {}", crate::planning::truncate_with_ellipsis(&p.error, width.saturating_sub(1))), Style::default().fg(theme::ERROR))
    } else {
        Line::from("")
    });
    lines.push(Line::styled(" type to filter \u{00b7} \u{2191}/\u{2193} choose \u{00b7} Enter assign (a task opens the place dialog) \u{00b7} Esc cancel", dim));
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::super::org_model::fixtures::{member, owner, snapshot, team};
    use super::*;
    use crate::app::lifecycle::revive_session_tests::dummy_ts;
    use cm_daemon::host_id::HostId;

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

    fn ws(id: &str) -> Workspace {
        Workspace {
            org: None,
            color: None,
            pinned: false,
            id: id.into(),
            name: id.into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: Some(format!("/wt/{id}").into()),
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: HostId::local(),
            sessions: vec![],
            tombstones: vec![],
        }
    }

    fn session(app: &mut App, wi: usize, n: u32, label: &str) {
        let mut ts = dummy_ts(&format!("ts-{n:x}-0"), false, HostId::local());
        ts.label = label.into();
        ts.session_type = "claude".into();
        app.workspaces[wi].sessions.push(ts);
    }

    fn ev(c: KeyCode, m: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(c, m))
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            let m = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
            app.handle_event(&ev(KeyCode::Char(c), m));
        }
    }

    fn hit(app: &mut App, c: KeyCode) {
        app.handle_event(&ev(c, KeyModifiers::NONE));
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    /// Swarm (lead ep, rules) ⊃ Items (lead items); orgchart reports to ep
    /// in Swarm; fresh is a hire with no session; Empty has no lead.
    fn chart_app() -> App {
        let mut app = test_app();
        app.workspaces = vec![ws("w-ep"), ws("w-oc"), ws("w-items")];
        session(&mut app, 0, 0x10, "ep-main");
        session(&mut app, 1, 0x20, "orgchart");
        session(&mut app, 2, 0x30, "items-lead");
        let mut swarm = team("t1", "Swarm", None, Some("m1"), &[("m1", "primary"), ("m2", "primary"), ("m6", "primary")]);
        swarm["charter"] = json!("Ship the org model");
        swarm["standing_rules"] = json!([{"text": "Post blockers in the channel", "source": "ruling-7"}]);
        swarm["revision"] = json!(4);
        app.org.accept(snapshot(
            3,
            vec![
                owner(),
                member("m1", "ep", Some("mem-owner"), Some("t1"), Some("ts-10-0")),
                member("m2", "orgchart", Some("m1"), Some("t1"), Some("ts-20-0")),
                member("m3", "items", Some("m1"), Some("t2"), Some("ts-30-0")),
                member("m6", "fresh", Some("m1"), Some("t1"), None),
            ],
            vec![swarm, team("t2", "Items", Some("t1"), Some("m3"), &[("m3", "primary")]), team("t9", "Empty", None, None, &[])],
        ));
        app.handle_event(&ev(KeyCode::F(6), KeyModifiers::NONE));
        assert!(app.orgchart.visible);
        app
    }

    fn focus(app: &mut App, key: &str) {
        let m = app.org_model(false);
        let i = m.find(key).unwrap_or_else(|| panic!("no {key}: {:?}", m.nodes.iter().map(|n| &n.key).collect::<Vec<_>>()));
        app.orgchart_set_focus(&m, i);
    }

    fn sent(app: &App) -> (&'static str, Value) {
        app.orgchart.actions.last_sent.clone().expect("a write was sent")
    }

    fn form(app: &App) -> &Form {
        app.orgchart.actions.form.as_ref().expect("a form is open")
    }

    fn api_task(id: &str, title: &str, status: &str, placement: Option<Value>) -> crate::api::Task {
        serde_json::from_value(json!({
            "id": id, "slug": id, "name": title, "created_at": "2026-11-05T00:00:00Z", "repo_url": "", "repo_branch": "main",
            "status": status, "project": "cm", "kind": "oneshot", "placement": placement,
        }))
        .unwrap()
    }

    #[test]
    fn the_keys_need_the_org_model() {
        let mut app = test_app();
        app.handle_event(&ev(KeyCode::F(6), KeyModifiers::NONE));
        press(&mut app, "pmXeA");
        assert!(app.orgchart.actions.form.is_none() && app.orgchart.actions.picker.is_none());
    }

    #[test]
    fn hire_under_a_member_defaults_to_them_and_their_team() {
        let mut app = chart_app();
        focus(&mut app, "member:m2");
        press(&mut app, "p");
        let f = form(&app);
        assert_eq!(f.title, "Hire a report of orgchart's");
        assert_eq!((f.value("manager"), f.value("team")), ("m2".into(), "t1".into()));
        // Handles are lowercase, a space types a dash, other marks are dropped.
        press(&mut app, "Rev Bot!");
        assert_eq!(form(&app).value("handle"), "rev-bot");
        hit(&mut app, KeyCode::Tab);
        press(&mut app, &"x".repeat(70));
        assert_eq!(form(&app).value("title").len(), TITLE_MAX, "the title stops at 60");
        hit(&mut app, KeyCode::Enter);
        let (method, p) = sent(&app);
        assert_eq!(method, "org.hire");
        assert_eq!((&p["handle"], &p["manager"], &p["team"], &p["kind"]), (&json!("rev-bot"), &json!("m2"), &json!("t1"), &json!("agent")));
        assert_eq!(p["title"].as_str().unwrap().len(), 60);
        assert!(p.get("mandate").is_none(), "empty fields are not sent: {p}");
        assert!(form(&app).busy);
        app.chart_actions_accept(Op::Hire, "Hired rev-bot under orgchart: no session yet".into(), Ok(json!({"member": {"id": "m9", "handle": "rev-bot"}})));
        assert!(app.orgchart.actions.form.is_none());
        assert_eq!(app.orgchart.status, "Hired rev-bot under orgchart: no session yet");
        assert_eq!(app.orgchart.actions.focus_hire.as_ref().map(|f| f.0.as_str()), Some("m9"));
    }

    #[test]
    fn hire_on_a_team_goes_under_its_lead_and_on_owner_under_owner() {
        let mut app = chart_app();
        focus(&mut app, "team:t2");
        press(&mut app, "p");
        assert_eq!(form(&app).title, "Hire into Items");
        assert_eq!((form(&app).value("manager"), form(&app).value("team")), ("m3".into(), "t2".into()));
        hit(&mut app, KeyCode::Esc);
        focus(&mut app, "team:t9");
        press(&mut app, "p");
        assert_eq!((form(&app).value("manager"), form(&app).value("team")), ("owner".into(), "t9".into()), "a leadless team: under Owner");
        hit(&mut app, KeyCode::Esc);
        focus(&mut app, "owner");
        press(&mut app, "p");
        assert_eq!(form(&app).title, "Hire a report of Owner's");
        assert_eq!((form(&app).value("manager"), form(&app).value("team")), ("owner".into(), String::new()));
        // An invalid handle is refused in the form, before any write.
        press(&mut app, "x");
        hit(&mut app, KeyCode::Enter);
        assert!(form(&app).error.starts_with("A handle is 2-40"), "{}", form(&app).error);
        assert!(app.orgchart.actions.last_sent.is_none());
    }

    #[test]
    fn move_sends_only_what_changed() {
        let mut app = chart_app();
        focus(&mut app, "member:m2");
        press(&mut app, "m");
        assert_eq!(form(&app).value("manager"), "m1");
        hit(&mut app, KeyCode::Enter);
        assert_eq!(form(&app).error, "Nothing changed: pick another manager or team");
        // Manager: type a letter to jump to it.
        press(&mut app, "i");
        assert_eq!(form(&app).value("manager"), "m3");
        hit(&mut app, KeyCode::Enter);
        let (method, p) = sent(&app);
        assert_eq!(method, "org.move");
        assert_eq!((&p["member"], &p["manager"], &p["expected_revision"], &p["reason"]), (&json!("m2"), &json!("m3"), &json!(1), &json!("moved in F6")));
        assert!(p.get("primary_team_id").is_none(), "{p}");
        // A refusal stays in the form.
        app.chart_actions_accept(Op::Move, String::new(), Err(RpcFail { unsupported: false, message: "local: cycle: m3 reports to m2".into() }));
        assert_eq!(form(&app).error, "local: cycle: m3 reports to m2");
        assert!(!form(&app).busy);
        // Teams: none, then by name (Empty, Items, Swarm); left of Swarm.
        hit(&mut app, KeyCode::Tab);
        hit(&mut app, KeyCode::Left);
        hit(&mut app, KeyCode::Enter);
        let (_, p) = sent(&app);
        assert_eq!((&p["manager"], &p["primary_team_id"]), (&json!("m3"), &json!("t2")), "{p}");
        // "none" sends an explicit null, which clears the primary team.
        hit(&mut app, KeyCode::Esc);
        press(&mut app, "m");
        hit(&mut app, KeyCode::Tab);
        press(&mut app, "n");
        hit(&mut app, KeyCode::Enter);
        let (_, p) = sent(&app);
        assert!(p.get("primary_team_id").is_some_and(Value::is_null) && p.get("manager").is_none(), "{p}");
    }

    #[test]
    fn retire_names_the_member_and_what_happens() {
        let mut app = chart_app();
        focus(&mut app, "member:m2");
        press(&mut app, "X");
        assert_eq!(form(&app).title, "Retire orgchart");
        assert!(form(&app).note[0].contains("move up to ep"), "{:?}", form(&app).note);
        hit(&mut app, KeyCode::Enter);
        let (method, p) = sent(&app);
        assert_eq!((method, &p["member"], &p["reason"]), ("org.retire", &json!("m2"), &json!("retired in F6")));
        // On a team box it is the lead, and the form says the team loses it.
        hit(&mut app, KeyCode::Esc);
        focus(&mut app, "team:t2");
        press(&mut app, "X");
        assert_eq!(form(&app).title, "Retire items");
        assert!(form(&app).note[1].contains("items leads Items"), "{:?}", form(&app).note);
        hit(&mut app, KeyCode::Esc);
        focus(&mut app, "team:t9");
        press(&mut app, "X");
        assert!(app.orgchart.actions.form.is_none());
        assert_eq!(app.orgchart.status, "Empty has no lead yet");
    }

    #[test]
    fn profile_edits_send_changed_fields_with_the_revision() {
        let mut app = chart_app();
        focus(&mut app, "member:m2");
        press(&mut app, "e");
        assert_eq!(form(&app).value("title"), "orgchart title");
        app.handle_event(&ev(KeyCode::Char('u'), KeyModifiers::CONTROL));
        press(&mut app, "Chart lead");
        hit(&mut app, KeyCode::Enter);
        let (method, p) = sent(&app);
        assert_eq!((method, &p["member"], &p["title"], &p["expected_revision"]), ("org.member_update", &json!("m2"), &json!("Chart lead"), &json!(1)));
        assert!(p.get("mandate").is_none(), "{p}");
        // Editing in the middle: the cursor moves.
        hit(&mut app, KeyCode::Esc);
        press(&mut app, "e");
        hit(&mut app, KeyCode::Tab);
        hit(&mut app, KeyCode::Home);
        press(&mut app, "The ");
        assert_eq!(form(&app).value("mandate"), "The orgchart mandate");
        // E on a team box edits its lead.
        hit(&mut app, KeyCode::Esc);
        focus(&mut app, "team:t1");
        press(&mut app, "E");
        assert_eq!(form(&app).title, "ep's profile");
    }

    #[test]
    fn team_edits_charter_and_rules_keeping_sources() {
        let mut app = chart_app();
        focus(&mut app, "team:t1");
        press(&mut app, "e");
        let labels: Vec<&str> = form(&app).fields.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["charter", "rule 1", "add rule"]);
        hit(&mut app, KeyCode::Tab);
        hit(&mut app, KeyCode::Tab);
        press(&mut app, "Review before merge");
        assert_eq!(form(&app).fields.len(), 4, "typing a rule offers another");
        hit(&mut app, KeyCode::Enter);
        let (method, p) = sent(&app);
        assert_eq!(method, "org.team_update");
        assert_eq!((&p["team"], &p["expected_revision"]), (&json!("t1"), &json!(4)));
        assert!(p.get("charter").is_none(), "unchanged charter: {p}");
        assert_eq!(p["standing_rules"], json!([{"text": "Post blockers in the channel", "source": "ruling-7"}, {"text": "Review before merge", "source": "owner"}]));
        // Emptying a rule drops it; Ctrl+J starts a charter line.
        hit(&mut app, KeyCode::Esc);
        press(&mut app, "e");
        hit(&mut app, KeyCode::End);
        app.handle_event(&ev(KeyCode::Char('j'), KeyModifiers::CONTROL));
        press(&mut app, "Then S6");
        hit(&mut app, KeyCode::Tab);
        app.handle_event(&ev(KeyCode::Char('u'), KeyModifiers::CONTROL));
        hit(&mut app, KeyCode::Enter);
        let (_, p) = sent(&app);
        assert_eq!(p["charter"], "Ship the org model\nThen S6");
        assert_eq!(p["standing_rules"], json!([]));
    }

    #[test]
    fn assign_lists_backlog_tasks_and_open_items_and_gives_an_item_to_the_main_session() {
        let mut app = chart_app();
        app.planning.update_from_api(vec![
            api_task("t-free", "Write the F6 doc", "backlog", None),
            api_task("t-placed", "Already placed", "backlog", Some(json!({"board": "swarm", "n": 5}))),
            api_task("t-run", "Running", "running", None),
        ]);
        app.orgchart.boards.items.insert(
            "swarm".into(),
            vec![
                json!({"n": 7, "title": "Fix the roster", "status": "open", "holders": []}),
                json!({"n": 8, "title": "Done thing", "status": "done", "holders": []}),
                json!({"n": 9, "title": "Theirs already", "status": "active", "holders": [{"pid": "agent:d1:ts-20-0", "name": "orgchart", "session_uid": "ts-20-0"}]}),
                json!({"n": 10, "title": "Someone else's", "status": "active", "holders": [{"pid": "agent:d1:ts-30-0", "name": "items"}]}),
            ],
        );
        focus(&mut app, "member:m2");
        press(&mut app, "A");
        let p = app.orgchart.actions.picker.as_ref().expect("picker");
        let titles: Vec<String> = p.rows.iter().map(|r| match r {
            Pick::Task { title, .. } | Pick::Item { title, .. } => title.clone(),
        }).collect();
        assert_eq!(titles, ["Write the F6 doc", "Fix the roster", "Someone else's"]);
        press(&mut app, "roster");
        assert_eq!(app.orgchart.actions.picker.as_ref().unwrap().visible().len(), 1);
        hit(&mut app, KeyCode::Enter);
        let (method, params) = sent(&app);
        assert_eq!(method, "item.set");
        assert_eq!((&params["board"], &params["n"], &params["owner"]), (&json!("swarm"), &json!(7), &json!("agent:d1:ts-20-0")));
        app.chart_actions_accept(Op::Item, "swarm #7 \u{2192} orgchart".into(), Ok(json!({})));
        assert!(app.orgchart.actions.picker.is_none());
        assert_eq!(app.orgchart.status, "swarm #7 \u{2192} orgchart");
        // A member with no session can't hold an item.
        focus(&mut app, "member:m6");
        press(&mut app, "A");
        press(&mut app, "roster");
        hit(&mut app, KeyCode::Enter);
        assert_eq!(app.orgchart.actions.picker.as_ref().unwrap().error, "fresh has no session yet, so it can't hold a board item");
        // On a team box the picker hands work to the lead (O8).
        hit(&mut app, KeyCode::Esc);
        focus(&mut app, "team:t2");
        press(&mut app, "A");
        let p = app.orgchart.actions.picker.as_ref().unwrap();
        assert_eq!((p.member.as_str(), p.team.clone()), ("m3", Some(("t2".to_owned(), "Items".to_owned()))));
    }

    #[test]
    fn assigning_a_task_opens_the_shared_place_dialog_with_the_member_as_holder() {
        let mut app = chart_app();
        app.planning.update_from_api(vec![api_task("t-free", "Write the F6 doc", "backlog", None)]);
        focus(&mut app, "member:m2");
        press(&mut app, "A");
        hit(&mut app, KeyCode::Enter);
        assert!(app.orgchart.actions.picker.is_none());
        let d = app.place.dialog.as_ref().expect("place dialog");
        assert_eq!((d.task_id.as_str(), &d.holder), ("t-free", &Holder::Member("m2".into())));
        assert!(app.orgchart.actions.placing.is_some());
        let s = screen(&mut app, 200, 40);
        assert!(s.contains("Place on a board") && s.contains("orgchart"), "{s}");
        // Its keys go to the dialog; Esc closes it and the chart is back.
        hit(&mut app, KeyCode::Esc);
        assert!(app.place.dialog.is_none());
        app.chart_actions_tick();
        assert!(app.orgchart.actions.placing.is_none() && app.orgchart.visible);
        // A place that lands reports through the status message.
        press(&mut app, "A");
        hit(&mut app, KeyCode::Enter);
        app.place_accept("place", Ok(json!({"board": "swarm", "n": 12, "item_status": "active"})));
        app.chart_actions_tick();
        assert_eq!(app.orgchart.status, "Placed on swarm #12 (active)");
        // From a team's box the team holds it (O8): the API resolves its lead.
        focus(&mut app, "team:t2");
        press(&mut app, "A");
        hit(&mut app, KeyCode::Enter);
        assert_eq!(app.place.dialog.as_ref().map(|d| &d.holder), Some(&Holder::Team("t2".into())));
        let d = app.place.dialog.clone().unwrap();
        assert_eq!(app.place_params(&d)["team"], "t2");
    }

    #[test]
    fn o_opens_the_main_session_and_says_why_when_it_cannot() {
        let mut app = chart_app();
        focus(&mut app, "member:m6");
        press(&mut app, "o");
        assert_eq!(app.orgchart.status, "fresh has no session yet");
        assert!(app.orgchart.visible);
        focus(&mut app, "member:m2");
        press(&mut app, "o");
        assert!(!app.orgchart.visible);
        assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-20-0"));
    }

    #[test]
    fn help_and_legend_name_the_actions() {
        let mut app = chart_app();
        focus(&mut app, "member:m2");
        let s = screen(&mut app, 220, 40);
        assert!(s.contains("A assign  p hire  m move  X retire  e edit  o main"), "{s}");
        focus(&mut app, "team:t9");
        let s = screen(&mut app, 220, 40);
        assert!(s.contains("p hire  e team  \u{2502}"), "{s}");
        press(&mut app, "?");
        let s = screen(&mut app, 220, 40);
        let legend = s.lines().find(|l| l.contains("Owner actions")).unwrap_or_else(|| panic!("{s}"));
        for part in ["A assign", "p hire", "m move", "X retire", "e edit profile", "o main session", "act on its lead"] {
            assert!(legend.contains(part), "{part}: {legend}");
        }
        // A form draws over the chart with its own keys.
        press(&mut app, "?");
        focus(&mut app, "member:m2");
        press(&mut app, "e");
        let s = screen(&mut app, 200, 40);
        assert!(s.contains("orgchart's profile") && s.contains("Enter saves"), "{s}");
    }

    #[test]
    fn a_new_hire_is_focused_once_it_shows() {
        let mut app = chart_app();
        app.orgchart.actions.focus_hire = Some(("m7".into(), Instant::now()));
        app.chart_actions_tick();
        assert!(app.orgchart.actions.focus_hire.is_some(), "not in the snapshot yet");
        let mut members: Vec<Value> = vec![owner(), member("m1", "ep", Some("mem-owner"), Some("t1"), Some("ts-10-0")), member("m2", "orgchart", Some("m1"), Some("t1"), Some("ts-20-0"))];
        members.push(member("m7", "rev", Some("m2"), Some("t1"), None));
        app.org.accept(snapshot(4, members, vec![team("t1", "Swarm", None, Some("m1"), &[("m1", "primary"), ("m2", "primary"), ("m7", "primary")])]));
        app.chart_actions_tick();
        assert!(app.orgchart.actions.focus_hire.is_none());
        assert_eq!(app.orgchart.focus.as_deref(), Some("member:m7"));
    }
}
