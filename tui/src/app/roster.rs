//! The roster screen (`R` in F6; doc/org-model.md §11, S2): the proposed
//! members from `org.roster` for Owner to approve, decline or edit in one
//! pass. Live agent sessions that are no member's main follow in folded
//! groups: each member's assistants (sessions that reach it through
//! `App::session_attachments`, as on F6), where promoting one to a member of
//! its own (`p`) is a deliberate per-row action, then the few other sessions
//! that reach no member, to adopt (`y`). `Y` approves proposals only.
//!
//! Proposals regenerate on every read; edits are kept by `proposal_id` until
//! an apply accepts them. `A` sends every decided row in one
//! `org.roster_apply`; each row is checked on its own and a refusal (stale,
//! handle taken, cycle, …) stays on its row. Adopting goes to the session's
//! own host (`org.adopt`), which knows its daemon id. An old daemon or API
//! (app/org_model.rs) shows a one-line notice instead.
use super::org_model::{owner_rpc, Proposal, RosterRejection, RosterReply, RpcFail, Support};
use super::*;
use serde_json::{json, Value};
use std::sync::mpsc;

/// A typed line's purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Handle,
    Team,
    Manager,
    Home,
}

impl Field {
    fn label(self) -> &'static str {
        match self {
            Field::Handle => "handle",
            Field::Team => "team (id, slug or name; empty = none)",
            Field::Manager => "manager (handle, id or owner)",
            Field::Home => "home workspace path (empty = none)",
        }
    }
}

/// Owner's edits to one row, over what the proposal (or session) says.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Edit {
    /// `Some(true)` approve, `Some(false)` decline; `None` undecided.
    pub approve: Option<bool>,
    pub handle: Option<String>,
    pub kind: Option<String>,
    /// `Some("")` clears the proposal's team.
    pub team: Option<String>,
    pub manager: Option<String>,
    pub home: Option<String>,
}

/// A selectable row: a proposal, a group header, or a session to adopt.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Row {
    Proposal(usize),
    /// A folding header: `assistants:<member or team:<id>>`, or `other`.
    Group(String),
    /// `of`: the member (or `team:<id>`) whose assistant it is; `None` for a
    /// session that reaches no member.
    Adopt { uid: String, label: String, host: String, continuous: bool, of: Option<String> },
}

pub(super) const OTHER_GROUP: &str = "other";

type Reply = (&'static str, Result<Value, RpcFail>);

#[derive(Default)]
pub(super) struct Roster {
    pub(super) visible: bool,
    pub(super) support: Support,
    pub(super) reply: RosterReply,
    loaded: bool,
    pub(super) cursor: usize,
    /// Edits by proposal id, or `adopt:<uid>`.
    pub(super) edits: HashMap<String, Edit>,
    pub(super) rejected: HashMap<String, RosterRejection>,
    pub(super) prompt: Option<(Field, String)>,
    rx: Option<mpsc::Receiver<Reply>>,
    /// Marked adopt rows still to send after `A` (one `org.adopt` each, to
    /// the session's own host).
    adopt_queue: std::collections::VecDeque<Row>,
    pub(super) status: String,
    pub(super) error: String,
    seq: u64,
    /// Unfolded groups (all start folded).
    pub(super) open: HashSet<String>,
}

fn adopt_key(uid: &str) -> String {
    format!("adopt:{uid}")
}

/// A handle from a session label: lowercase, dashes for runs of anything else.
pub(super) fn handle_from_label(label: &str) -> String {
    let mut out = String::new();
    for c in label.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

impl App {
    pub(super) fn open_roster(&mut self) {
        self.roster.visible = true;
        self.roster.prompt = None;
        self.roster.status.clear();
        self.roster.loaded = false;
        self.org_refresh_soon();
        self.roster_load();
    }

    fn roster_request(&mut self, op: &'static str, method: &'static str, params: Value, targets: Vec<(String, PathBuf, String)>) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send((op, owner_rpc(&targets, method, &params)));
        });
        self.roster.rx = Some(rx);
    }

    fn roster_load(&mut self) {
        if self.roster.rx.is_some() {
            return;
        }
        let targets = self.owner_rpc_targets();
        if targets.is_empty() {
            self.roster.error = "No daemon is reachable".into();
            return;
        }
        self.roster_request("load", "org.roster", json!({}), targets);
    }

    fn roster_request_id(&mut self, what: &str) -> String {
        self.roster.seq += 1;
        format!("tui-roster-{what}-{}-{}", board::now_unix(), self.roster.seq)
    }

    /// Every proposal and every session row, ignoring folds: what `A`
    /// applies. Assistants come grouped by member (by handle), then the
    /// other sessions.
    pub(super) fn roster_all_rows(&self) -> Vec<Row> {
        let mut rows: Vec<Row> = (0..self.roster.reply.proposals.len()).map(Row::Proposal).collect();
        let snap = self.org.snapshot.as_ref();
        let attached = snap.map(|s| self.session_attachments(s)).unwrap_or_default();
        let proposed = |uid: &str| self.roster.reply.proposals.iter().any(|p| p.pids.iter().any(|pid| pid.rsplit(':').next() == Some(uid)));
        let mut sessions: Vec<(String, Row)> = Vec::new();
        for ws in self.workspaces.iter().filter(|w| !w.is_closed) {
            for ts in &ws.sessions {
                let agent = matches!(ts.session_type.as_str(), "claude" | "claude-code" | "codex");
                // A member's main (the snapshot's, or stamped `main`).
                let main = matches!(attached.get(&ts.uid), Some((_, super::orgchart_teams::Via::Main)));
                let member = main || snap.is_some_and(|s| s.member_by_main(&ts.uid).is_some());
                if !agent || ts.session.exited || member || proposed(&ts.uid) {
                    continue;
                }
                let of = attached.get(&ts.uid).map(|(to, _)| match to {
                    super::orgchart_teams::Attach::Member(m) => m.clone(),
                    super::orgchart_teams::Attach::Team(t) => {
                        let lead = snap.and_then(|s| s.team(t)).and_then(|t| t.lead_member_id.clone()).filter(|l| snap.and_then(|s| s.member(l)).is_some_and(|l| !l.is_owner() && !l.retired()));
                        lead.unwrap_or_else(|| format!("team:{t}"))
                    }
                });
                let order = match &of {
                    Some(m) => format!("0{}", self.roster_group_name(m).to_lowercase()),
                    None => "1".into(),
                };
                sessions.push((order, Row::Adopt { uid: ts.uid.clone(), label: ts.label.clone(), host: ts.host_id.to_string(), continuous: ts.continuous_task_id.is_some(), of }));
            }
        }
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        rows.extend(sessions.into_iter().map(|(_, r)| r));
        rows
    }

    /// Who a group is of: the member's handle, or the leadless team's name.
    fn roster_group_name(&self, of: &str) -> String {
        let snap = self.org.snapshot.as_ref();
        match of.strip_prefix("team:") {
            Some(t) => snap.and_then(|s| s.team(t)).map_or_else(|| t.to_owned(), |t| t.name.clone()),
            None => snap.and_then(|s| s.member(of)).map_or_else(|| of.to_owned(), |m| m.handle.clone()),
        }
    }

    fn group_key(of: Option<&str>) -> String {
        of.map_or_else(|| OTHER_GROUP.to_owned(), |m| format!("assistants:{m}"))
    }

    /// Rows in display order: proposals, then each member's assistants and
    /// the other sessions, each under a folding header.
    pub(super) fn roster_rows(&self) -> Vec<Row> {
        Self::roster_visible(&self.roster.open, self.roster_all_rows())
    }

    fn roster_visible(open: &HashSet<String>, all: Vec<Row>) -> Vec<Row> {
        let mut rows = Vec::new();
        let mut last: Option<String> = None;
        for row in all {
            if let Row::Adopt { of, .. } = &row {
                let key = Self::group_key(of.as_deref());
                if last.as_deref() != Some(key.as_str()) {
                    rows.push(Row::Group(key.clone()));
                    last = Some(key.clone());
                }
                if !open.contains(&key) {
                    continue;
                }
            }
            rows.push(row);
        }
        rows
    }

    /// How many session rows each group holds.
    fn roster_group_lens(all: &[Row]) -> HashMap<String, usize> {
        let mut out = HashMap::new();
        for r in all {
            if let Row::Adopt { of, .. } = r {
                *out.entry(Self::group_key(of.as_deref())).or_insert(0) += 1;
            }
        }
        out
    }

    fn roster_key(&self, row: &Row) -> String {
        match row {
            Row::Proposal(i) => self.roster.reply.proposals[*i].proposal_id.clone(),
            Row::Group(k) => format!("group:{k}"),
            Row::Adopt { uid, .. } => adopt_key(uid),
        }
    }

    /// `(handle, kind, team, manager, home)` as they will be sent.
    pub(super) fn roster_values(&self, row: &Row) -> (String, String, Option<String>, Option<String>, Option<String>) {
        let e = self.roster.edits.get(&self.roster_key(row)).cloned().unwrap_or_default();
        let (handle, kind, team, manager, home) = match row {
            Row::Proposal(i) => {
                let p: &Proposal = &self.roster.reply.proposals[*i];
                (p.handle.clone(), p.kind.clone(), p.team_id.clone(), p.manager.clone(), p.home_workspace.as_ref().and_then(|h| h.path.clone()))
            }
            // A promoted assistant reports to the member it assisted.
            Row::Adopt { label, continuous, of, .. } => {
                let manager = of.as_deref().filter(|m| !m.starts_with("team:")).map_or("owner", |m| m).to_owned();
                (handle_from_label(label), if *continuous { "continuous" } else { "agent" }.to_owned(), None, Some(manager), None)
            }
            Row::Group(_) => (String::new(), String::new(), None, None, None),
        };
        let pick = |edit: Option<String>, base: Option<String>| match edit {
            Some(v) if v.is_empty() => None,
            Some(v) => Some(v),
            None => base,
        };
        (e.handle.unwrap_or(handle), e.kind.unwrap_or(kind), pick(e.team, team), pick(e.manager, manager), pick(e.home, home))
    }

    fn roster_edit(&mut self) -> Option<&mut Edit> {
        let row = self.roster_rows().get(self.roster.cursor).cloned()?;
        let key = self.roster_key(&row);
        Some(self.roster.edits.entry(key).or_default())
    }

    fn roster_submit(&mut self, field: Field, text: String) {
        let text = text.trim().to_owned();
        let snap = self.org.snapshot.clone();
        let value = match field {
            Field::Handle if text.is_empty() => {
                self.roster.status = "A handle can't be empty".into();
                return;
            }
            Field::Team if !text.is_empty() => match snap.as_ref().map(|s| s.resolve_team(&text).map(|t| t.id.clone())) {
                Some(Some(id)) => id,
                Some(None) => {
                    self.roster.status = format!("No team {text}");
                    return;
                }
                None => text,
            },
            Field::Manager if text.is_empty() => "owner".into(),
            Field::Manager => match snap.as_ref().map(|s| s.resolve_member(&text).map(|m| if m.is_owner() { "owner".to_owned() } else { m.id.clone() })) {
                Some(Some(id)) => id,
                Some(None) => {
                    self.roster.status = format!("No member {text}");
                    return;
                }
                None => text,
            },
            _ => text,
        };
        let row = self.roster_rows().get(self.roster.cursor).cloned();
        if let Some(e) = self.roster_edit() {
            match field {
                Field::Handle => e.handle = Some(value),
                Field::Team => e.team = Some(value),
                Field::Manager => e.manager = Some(value),
                Field::Home => e.home = Some(value),
            }
            // Editing a proposal means taking it, unless already declined.
            if matches!(row, Some(Row::Proposal(_))) && e.approve.is_none() {
                e.approve = Some(true);
            }
        }
    }

    /// The `org.roster_apply` rows: every decided proposal.
    pub(super) fn roster_apply_rows(&self) -> Vec<Value> {
        self
            .roster_all_rows()
            .iter()
            .filter_map(|row| {
                let Row::Proposal(i) = row else { return None };
                let id = self.roster.reply.proposals[*i].proposal_id.clone();
                let approve = self.roster.edits.get(&id)?.approve?;
                let (handle, kind, team, manager, _) = self.roster_values(row);
                let mut out = json!({
                    "proposal_id": id, "approve": approve, "handle": handle, "kind": kind,
                    "team_id": team, "manager": manager,
                });
                if let Ok(Some(home)) = self.roster_home_param(row) {
                    out["home_workspace"] = home;
                }
                Some(out)
            })
            .collect()
    }

    /// The `home_workspace` to send for `row`: nothing unless Owner edited it
    /// (the server keeps the proposal's own), null when cleared, else the
    /// full `{daemon_id, workspace_id, path}` of a known workspace with that
    /// path. The API needs both ids, so a path alone is refused here.
    pub(super) fn roster_home_param(&self, row: &Row) -> Result<Option<Value>, String> {
        let Some(path) = self.roster.edits.get(&self.roster_key(row)).and_then(|e| e.home.clone()) else {
            return Ok(None);
        };
        if path.is_empty() {
            return Ok(Some(Value::Null));
        }
        let members = self.org.snapshot.as_ref().map(|s| s.members.as_slice()).unwrap_or_default();
        self.roster
            .reply
            .proposals
            .iter()
            .filter_map(|p| p.home_workspace.as_ref())
            .chain(members.iter().filter_map(|m| m.home_workspace.as_ref()))
            .find(|h| h.path.as_deref() == Some(path.as_str()))
            .and_then(|h| Some(json!({"daemon_id": h.daemon_id.clone()?, "workspace_id": h.workspace_id.clone()?, "path": path})))
            .map(Some)
            .ok_or_else(|| format!("No known workspace at {path}; leave home unset and change it after applying"))
    }

    fn roster_apply(&mut self) {
        if self.roster.rx.is_some() {
            self.roster.status = "Busy; try again".into();
            return;
        }
        let decided = self.roster_all_rows().into_iter().filter(|r| self.roster.edits.get(&self.roster_key(r)).and_then(|e| e.approve) == Some(true));
        if let Some(err) = decided.filter_map(|r| self.roster_home_param(&r).err()).next() {
            self.roster.status = err;
            return;
        }
        let rows = self.roster_apply_rows();
        let adopts: Vec<Row> = self
            .roster_all_rows()
            .into_iter()
            .filter(|r| matches!(r, Row::Adopt { .. }) && self.roster.edits.get(&self.roster_key(r)).and_then(|e| e.approve) == Some(true))
            .collect();
        if rows.is_empty() && adopts.is_empty() {
            self.roster.status = "Nothing decided yet (y approve or mark, p promote an assistant, x decline)".into();
            return;
        }
        self.roster.adopt_queue = adopts.into();
        if rows.is_empty() {
            self.roster_pump_adopts();
            return;
        }
        let targets = self.owner_rpc_targets();
        let request_id = self.roster_request_id("apply");
        self.roster.status = format!("Applying {} row{}…", rows.len(), if rows.len() == 1 { "" } else { "s" });
        self.roster_request("apply", "org.roster_apply", json!({"rows": rows, "request_id": request_id}), targets);
    }

    /// Send the next marked adopt once nothing is in flight.
    fn roster_pump_adopts(&mut self) {
        if self.roster.rx.is_some() {
            return;
        }
        if let Some(row) = self.roster.adopt_queue.pop_front() {
            self.roster_adopt(&row);
        }
    }

    fn roster_adopt(&mut self, row: &Row) {
        let Row::Adopt { host, .. } = row else { return };
        if self.roster.rx.is_some() {
            self.roster.status = "Busy; try again".into();
            return;
        }
        // The session's own host fills in its daemon id (and the pid).
        let targets: Vec<_> = self.owner_rpc_targets().into_iter().filter(|(h, _, _)| h == host).collect();
        if targets.is_empty() && !cfg!(test) {
            self.roster.status = format!("Host {host} is not reachable");
            return;
        }
        let request_id = self.roster_request_id("adopt");
        let params = self.roster_adopt_params(row, &request_id);
        self.roster.status = format!("Adopting {}…", params["handle"].as_str().unwrap_or(""));
        self.roster_request("adopt", "org.adopt", params, targets);
    }

    /// `org.adopt` for an adopt row, by `session_uid` (the owning daemon
    /// fills in the pid).
    pub(super) fn roster_adopt_params(&self, row: &Row, request_id: &str) -> Value {
        let Row::Adopt { uid, .. } = row else { return Value::Null };
        let (handle, kind, team, manager, _) = self.roster_values(row);
        let mut params = json!({"session_uid": uid, "handle": handle, "kind": kind, "manager": manager, "request_id": request_id});
        if let Some(t) = team {
            params["team_id"] = json!(t);
        }
        if let Ok(Some(home)) = self.roster_home_param(row) {
            params["home_workspace"] = home;
        }
        params
    }

    /// Apply a reply (also the test entry point).
    pub(super) fn roster_accept(&mut self, op: &str, r: Result<Value, RpcFail>) {
        self.needs_redraw = true;
        match (op, r) {
            (_, Err(f)) if f.unsupported => {
                self.roster.support = Support::Off;
                self.roster.error.clear();
                self.roster.adopt_queue.clear();
            }
            (_, Err(f)) => {
                // Unsent adopts keep their marks for another A.
                self.roster.adopt_queue.clear();
                self.roster.error = f.message;
            }
            ("load", Ok(v)) => {
                self.roster.support = Support::On;
                self.roster.error.clear();
                self.roster.loaded = true;
                match serde_json::from_value::<RosterReply>(v) {
                    Ok(reply) => self.roster.reply = reply,
                    Err(e) => self.roster.error = format!("org.roster: {e}"),
                }
                // Edits for proposals that are gone go with them.
                let ids: HashSet<String> = self.roster.reply.proposals.iter().map(|p| p.proposal_id.clone()).collect();
                self.roster.edits.retain(|k, _| ids.contains(k) || k.starts_with("adopt:"));
                self.roster.rejected.retain(|k, _| ids.contains(k));
                let n = self.roster_rows().len();
                self.roster.cursor = self.roster.cursor.min(n.saturating_sub(1));
            }
            ("apply", Ok(v)) => {
                self.roster.error.clear();
                let rejected: Vec<RosterRejection> = serde_json::from_value(v["rejected"].clone()).unwrap_or_default();
                let created = v["created"].as_array().map_or(0, Vec::len);
                // Accepted rows' edits are spent; refused ones stay to fix.
                let refused: HashSet<String> = rejected.iter().map(|r| r.proposal_id.clone()).collect();
                self.roster.edits.retain(|k, e| refused.contains(k) || e.approve.is_none() || k.starts_with("adopt:"));
                self.roster.rejected = rejected.into_iter().map(|r| (r.proposal_id.clone(), r)).collect();
                self.roster.status = format!("{created} created, {} refused", self.roster.rejected.len());
                self.roster.loaded = false;
                self.org_refresh_soon();
            }
            ("adopt", Ok(v)) => {
                self.roster.error.clear();
                let handle = v["member"]["handle"].as_str().unwrap_or("member").to_owned();
                let mark = v["home_mark"]["state"].as_str().unwrap_or("central");
                self.roster.status = format!("Adopted {handle} (home mark {mark})");
                if let Some(uid) = v["member"]["main"]["session_uid"].as_str() {
                    self.roster.edits.remove(&adopt_key(uid));
                }
                self.org_refresh_soon();
            }
            _ => {}
        }
    }

    pub fn roster_tick(&mut self) {
        if let Some((op, r)) = self.roster.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.roster.rx = None;
            self.roster_accept(op, r);
        }
        self.roster_pump_adopts();
        if self.roster.visible && !self.roster.loaded && self.roster.rx.is_none() && self.roster.support != Support::Off && self.roster.error.is_empty() {
            self.roster_load();
        }
    }

    /// While visible the roster takes every key; Esc returns to F6.
    pub(super) fn roster_event(&mut self, event: &CrosstermEvent) -> bool {
        if !self.roster.visible {
            return false;
        }
        let CrosstermEvent::Key(key) = event else { return true };
        self.needs_redraw = true;
        if let Some((field, mut text)) = self.roster.prompt.take() {
            match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.roster_submit(field, text),
                KeyCode::Backspace => {
                    text.pop();
                    self.roster.prompt = Some((field, text));
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                    text.push(c);
                    self.roster.prompt = Some((field, text));
                }
                _ => self.roster.prompt = Some((field, text)),
            }
            return true;
        }
        let rows = self.roster_rows();
        let row = rows.get(self.roster.cursor).cloned();
        let editable = row.clone().filter(|r| !matches!(r, Row::Group(_)));
        let prompt = |app: &mut App, field: Field| {
            if let Some(row) = &editable {
                let (handle, _, team, manager, home) = app.roster_values(row);
                let current = match field {
                    Field::Handle => handle,
                    Field::Team => team.unwrap_or_default(),
                    Field::Manager => manager.unwrap_or_default(),
                    Field::Home => home.unwrap_or_default(),
                };
                app.roster.prompt = Some((field, current));
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.roster.visible = false,
            KeyCode::Char('j') | KeyCode::Down => self.roster.cursor = (self.roster.cursor + 1).min(rows.len().saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => self.roster.cursor = self.roster.cursor.saturating_sub(1),
            KeyCode::Char('r') => {
                self.roster.loaded = false;
                self.roster.error.clear();
                self.roster.support = Support::Unknown;
                self.roster_load();
            }
            KeyCode::Char(' ') | KeyCode::Enter if matches!(row, Some(Row::Group(_))) => {
                let Some(Row::Group(k)) = &row else { unreachable!() };
                if !self.roster.open.remove(k) {
                    self.roster.open.insert(k.clone());
                }
            }
            // Promoting an assistant to a member of its own: the one reason
            // to adopt it, only ever by its own key.
            KeyCode::Char('p') => match &row {
                Some(Row::Adopt { of: Some(_), .. }) => {
                    if let Some(e) = self.roster_edit() {
                        e.approve = if e.approve == Some(true) { None } else { Some(true) };
                    }
                }
                _ => self.roster.status = "p promotes an assistant to a member".into(),
            },
            KeyCode::Char('x') if matches!(row, Some(Row::Adopt { of: Some(_), .. })) => {
                if let Some(e) = self.roster_edit() {
                    e.approve = None;
                }
            }
            KeyCode::Char('y' | ' ') if matches!(row, Some(Row::Adopt { of: Some(_), .. })) => {
                self.roster.status = "An assistant of its member: p promotes it to a member of its own".into();
            }
            KeyCode::Char(c @ ('y' | 'x' | ' ')) => match &row {
                // An adopt row is marked (y / Space) or cleared (x); A sends it.
                Some(Row::Adopt { .. }) => {
                    if let Some(e) = self.roster_edit() {
                        e.approve = match (c, e.approve) {
                            ('y', _) => Some(true),
                            ('x', _) => None,
                            (_, Some(true)) => None,
                            (_, _) => Some(true),
                        };
                    }
                }
                Some(Row::Proposal(_)) => {
                    if let Some(e) = self.roster_edit() {
                        e.approve = match (c, e.approve) {
                            ('y', _) => Some(true),
                            ('x', _) => Some(false),
                            (_, None) => Some(true),
                            (_, Some(true)) => Some(false),
                            (_, Some(false)) => None,
                        };
                    }
                }
                _ => {}
            },
            KeyCode::Char('Y') => {
                // Approve every undecided proposal that has no conflict.
                for row in &rows {
                    let Row::Proposal(i) = row else { continue };
                    let p = &self.roster.reply.proposals[*i];
                    if p.conflicts.is_empty() {
                        let e = self.roster.edits.entry(p.proposal_id.clone()).or_default();
                        e.approve.get_or_insert(true);
                    }
                }
            }
            KeyCode::Char('e') => prompt(self, Field::Handle),
            KeyCode::Char('t') => prompt(self, Field::Team),
            KeyCode::Char('m') => prompt(self, Field::Manager),
            KeyCode::Char('w') => prompt(self, Field::Home),
            KeyCode::Char('K') => {
                if let Some(row) = &editable {
                    let (_, kind, ..) = self.roster_values(row);
                    let next = if kind == "agent" { "continuous" } else { "agent" };
                    if let Some(e) = self.roster_edit() {
                        e.kind = Some(next.into());
                    }
                }
            }
            KeyCode::Char('A') => self.roster_apply(),
            _ => {}
        }
        true
    }

    pub(super) fn draw_roster(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let r = &self.roster;
        let decided = r.edits.values().filter(|e| e.approve.is_some()).count();
        let title = Line::from(vec![
            Span::styled(" Roster ", Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(
                    "· {} proposed · {decided} decided · {} declined before · last applied {} ",
                    r.reply.proposals.len(),
                    r.reply.declined.len(),
                    r.reply.last_applied_at.as_deref().unwrap_or("never")
                ),
                Style::default().fg(theme::MUTED),
            ),
        ]);
        let block = Block::default().borders(Borders::ALL).title(title).border_style(Style::default().fg(theme::TEXT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 {
            return;
        }
        let width = inner.width as usize;
        let dim = Style::default().fg(theme::DIM);
        let snap = self.org.snapshot.as_ref();
        let team_name = |id: &Option<String>| match id {
            Some(id) => snap.and_then(|s| s.team(id)).map_or_else(|| id.clone(), |t| t.name.clone()),
            None => "—".into(),
        };
        let manager_name = |id: &Option<String>| match id.as_deref() {
            None | Some("owner") => "Owner".to_owned(),
            Some(id) => snap.and_then(|s| s.member(id)).map_or_else(|| id.to_owned(), |m| m.handle.clone()),
        };
        let mut lines: Vec<Line> = Vec::new();
        if r.support == Support::Off {
            lines.push(Line::styled("The org model is not available from this daemon or planning API yet (r retries).", dim));
        } else if !r.loaded && r.error.is_empty() {
            lines.push(Line::styled("Loading the roster…", dim));
        }
        if !r.error.is_empty() {
            lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&r.error, width), Style::default().fg(theme::ERROR)));
        }
        let header = format!("     {:<18} {:<11} {:<18} {:<14} {:<22} {}", "handle", "kind", "team", "manager", "home workspace", "source");
        lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&header, width), Style::default().fg(theme::HEADER)));
        // One pass over the sessions per frame: the rows and the group sizes.
        let all = self.roster_all_rows();
        let lens = Self::roster_group_lens(&all);
        let rows = Self::roster_visible(&r.open, all);
        let mut cursor_line = 0;
        for (k, row) in rows.iter().enumerate() {
            if let Row::Group(g) = row {
                let open = r.open.contains(g);
                let n = lens.get(g).copied().unwrap_or(0);
                let text = match g.strip_prefix("assistants:") {
                    Some(of) => format!("{} assistants of {} ({n})", if open { "\u{25be}" } else { "\u{25b8}" }, self.roster_group_name(of)),
                    None => format!("{} {}", if open { "\u{25be}" } else { "\u{25b8}" }, if n == 1 { "1 other session".to_owned() } else { format!("{n} other sessions") }),
                };
                let mut style = Style::default().fg(theme::HEADER);
                if k == r.cursor {
                    style = style.bg(theme::SELECT_BG);
                    cursor_line = lines.len() + 1;
                }
                lines.push(Line::from(""));
                lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&text, width), style));
                if open {
                    let note = if g == OTHER_GROUP {
                        "      These reach no member. Adopting one (y, then A) makes it a member of its own, under the manager shown."
                    } else {
                        "      Assistants stay with their member. p promotes one to a member of its own (then A); nothing else adopts them."
                    };
                    lines.push(Line::styled(crate::planning::truncate_with_ellipsis(note, width), dim));
                }
                continue;
            }
            let key = self.roster_key(row);
            let (handle, kind, team, manager, home) = self.roster_values(row);
            let mark = match (row, r.edits.get(&key).and_then(|e| e.approve)) {
                (Row::Adopt { of: Some(_), .. }, Some(true)) => "[\u{2191}]",
                (Row::Adopt { .. }, Some(true)) => "[+]",
                (Row::Adopt { .. }, _) => "[ ]",
                (_, Some(true)) => "[\u{2713}]",
                (_, Some(false)) => "[\u{2717}]",
                (_, None) => "[ ]",
            };
            let source = match row {
                Row::Proposal(i) => r.reply.proposals[*i].source.clone(),
                Row::Adopt { label, host, .. } => format!("{label} @ {host}"),
                Row::Group(_) => String::new(),
            };
            let text = format!(
                "{mark}  {:<18} {:<11} {:<18} {:<14} {:<22} {source}",
                crate::planning::truncate_with_ellipsis(&handle, 18),
                kind,
                crate::planning::truncate_with_ellipsis(&team_name(&team), 18),
                crate::planning::truncate_with_ellipsis(&manager_name(&manager), 14),
                crate::planning::truncate_with_ellipsis(home.as_deref().unwrap_or("—"), 22),
            );
            let mut style = Style::default().fg(theme::TEXT);
            if k == r.cursor {
                style = style.bg(theme::SELECT_BG);
                cursor_line = lines.len();
            }
            lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&text, width), style));
            if let Row::Proposal(i) = row {
                for c in &r.reply.proposals[*i].conflicts {
                    lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&format!("      ! {c}"), width), Style::default().fg(theme::ATTN)));
                }
            }
            if let Some(rej) = r.rejected.get(&key) {
                lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&format!("      refused ({}): {}", rej.code, rej.message), width), Style::default().fg(theme::ERROR)));
            }
        }
        if rows.is_empty() && r.loaded {
            lines.push(Line::styled("Nothing proposed, and every live agent session is a member.", dim));
        }
        let body_h = inner.height.saturating_sub(1) as usize;
        let skip = cursor_line.saturating_sub(body_h.saturating_sub(2));
        let body = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
        frame.render_widget(Paragraph::new(lines.into_iter().skip(skip).collect::<Vec<_>>()), body);
        let status_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let line = match &r.prompt {
            Some((field, text)) => Line::from(vec![
                Span::styled(format!("{}: ", field.label()), Style::default().fg(theme::ATTN)),
                Span::styled(text.clone(), Style::default().fg(theme::TEXT)),
            ]),
            None => {
                let help = "j/k move  y approve  x decline  Space cycle/fold  Y approve all clean proposals  p promote an assistant  e handle  t team  m manager  w home  K kind  A apply  r reload  Esc back";
                let text = if r.status.is_empty() { help.to_owned() } else { format!("{}  \u{00b7}  {help}", r.status) };
                Line::styled(crate::planning::truncate_with_ellipsis(&text, width), dim)
            }
        };
        frame.render_widget(Paragraph::new(line), status_area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::org_model::fixtures::{member, owner, snapshot, team};
    use super::super::org_model::HomeWorkspace;
    use crate::app::lifecycle::revive_session_tests::dummy_ts;
    use cm_daemon::host_id::HostId;
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

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.handle_event(&CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
    }

    fn enter(app: &mut App) {
        app.handle_event(&CrosstermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }

    fn roster_app() -> App {
        let mut app = test_app();
        app.org.accept(snapshot(1, vec![owner(), member("m1", "ep", Some("mem-owner"), Some("t1"), Some("ts-10-0"))], vec![team("t1", "Swarm", None, Some("m1"), &[("m1", "primary")])]));
        let new_ws = |id: &str| Workspace {
            org: None,
            color: None, pinned: false, id: id.into(), name: id.into(), is_closed: false, is_cloud: false, repo_url: None,
            worktree_path: None, main_repo_path: None, worker_vm: None, worker_zone: None, host_id: HostId::new("sessions"), sessions: vec![], tombstones: vec![],
        };
        let mut w = new_ws("w");
        // Items Lane sits in its own workspace and reaches no member: an
        // "other" session. (In ep's workspace it would be ep's assistant.)
        let mut lone = new_ws("w-lone");
        for (uid, label, kind) in [("ts-10-0", "ep", "claude"), ("ts-20-0", "Items Lane", "codex"), ("ts-30-0", "proposed", "claude"), ("ts-40-0", "shell", "bash")] {
            let mut ts = dummy_ts(uid, false, HostId::new("sessions"));
            ts.label = label.into();
            ts.session_type = kind.into();
            if uid == "ts-20-0" { lone.sessions.push(ts) } else { w.sessions.push(ts) }
        }
        app.workspaces = vec![w, lone];
        app.roster.open.insert(OTHER_GROUP.into());
        app.open_roster();
        app.roster_accept("load", Ok(json!({
            "proposals": [
                {"proposal_id": "lane:agent:d1:ts-30-0", "handle": "proposed", "kind": "agent", "title": null, "team_id": "t1", "manager": "m1", "home_workspace": null, "pids": ["agent:d1:ts-30-0"], "source": "lane", "conflicts": []},
                {"proposal_id": "lead:init-9", "handle": "ep", "kind": "agent", "team_id": null, "manager": "owner", "pids": [], "source": "lead", "conflicts": ["handle taken \u{2192} ep-2"]},
            ],
            "declined": ["lane:agent:d1:ts-77-0"],
            "last_applied_at": null,
        })));
        app
    }

    #[test]
    fn rows_are_proposals_then_sessions_with_no_member() {
        let app = roster_app();
        let rows = app.roster_rows();
        assert_eq!(rows.len(), 4);
        // ep is a member, ts-30-0 is proposed, bash is no agent: only Items Lane to adopt.
        assert_eq!(rows[2], Row::Group(OTHER_GROUP.into()));
        assert!(matches!(&rows[3], Row::Adopt { uid, host, of: None, .. } if uid == "ts-20-0" && host == "sessions"));
        assert_eq!(app.roster.reply.declined, ["lane:agent:d1:ts-77-0"]);
    }

    #[test]
    fn decisions_and_edits_become_apply_rows() {
        let mut app = roster_app();
        assert!(app.roster_apply_rows().is_empty(), "nothing decided");
        press(&mut app, "y"); // approve row 0
        press(&mut app, "j");
        press(&mut app, "e");
        app.roster.prompt.as_mut().unwrap().1.clear();
        press(&mut app, "ep-lead");
        enter(&mut app); // editing approves it
        press(&mut app, "m");
        app.roster.prompt.as_mut().unwrap().1.clear();
        press(&mut app, "@ep");
        enter(&mut app); // resolved to the member id
        let rows = app.roster_apply_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["proposal_id"], "lane:agent:d1:ts-30-0");
        assert_eq!(rows[0]["approve"], true);
        assert_eq!(rows[0]["team_id"], "t1");
        assert_eq!(rows[1]["handle"], "ep-lead");
        assert_eq!(rows[1]["manager"], "m1");
        press(&mut app, "x"); // decline row 1 instead
        assert_eq!(app.roster_apply_rows()[1]["approve"], false);
        // An unknown team is refused before it is sent.
        press(&mut app, "t");
        app.roster.prompt.as_mut().unwrap().1.clear();
        press(&mut app, "nope");
        enter(&mut app);
        assert_eq!(app.roster.status, "No team nope");
    }

    #[test]
    fn home_is_sent_only_when_edited_and_as_the_full_workspace() {
        let mut app = roster_app();
        app.roster.reply.proposals[0].home_workspace = Some(HomeWorkspace { daemon_id: Some("d1".into()), workspace_id: Some("w1".into()), path: Some("/wt/a".into()) });
        press(&mut app, "y");
        // Unedited: the key is absent, so the server keeps the proposal's own
        // home (a path-only object was refused for every row, 2026-10-25).
        assert!(app.roster_apply_rows()[0].get("home_workspace").is_none());
        press(&mut app, "w");
        app.roster.prompt.as_mut().unwrap().1.clear();
        press(&mut app, "/wt/a");
        enter(&mut app);
        assert_eq!(app.roster_apply_rows()[0]["home_workspace"], json!({"daemon_id": "d1", "workspace_id": "w1", "path": "/wt/a"}));
        // A path no known workspace has is refused before anything is sent.
        press(&mut app, "w");
        app.roster.prompt.as_mut().unwrap().1.clear();
        press(&mut app, "/nowhere");
        enter(&mut app);
        press(&mut app, "A");
        assert!(app.roster.rx.is_none());
        assert!(app.roster.status.starts_with("No known workspace at /nowhere"), "{}", app.roster.status);
        // Clearing it sends null.
        press(&mut app, "w");
        app.roster.prompt.as_mut().unwrap().1.clear();
        enter(&mut app);
        assert_eq!(app.roster_apply_rows()[0]["home_workspace"], Value::Null);
    }

    #[test]
    fn apply_keeps_refused_rows_and_their_reason() {
        let mut app = roster_app();
        press(&mut app, "Y"); // approves only the conflict-free row
        assert_eq!(app.roster_apply_rows().len(), 1);
        press(&mut app, "jy");
        app.roster_accept("apply", Ok(json!({"created": [{"id": "m9"}], "rejected": [{"proposal_id": "lead:init-9", "code": "handle_taken", "message": "ep is taken"}]})));
        assert_eq!(app.roster.status, "1 created, 1 refused");
        assert!(!app.roster.edits.contains_key("lane:agent:d1:ts-30-0"));
        assert!(app.roster.edits.contains_key("lead:init-9"));
        assert_eq!(app.roster.rejected["lead:init-9"].code, "handle_taken");
    }

    #[test]
    fn adopt_rows_are_marked_and_sent_with_a_not_on_y() {
        let mut app = roster_app();
        app.roster.visible = true;
        app.roster.cursor = 3; // the adopt row
        press(&mut app, "y");
        assert!(app.roster.rx.is_none(), "y only marks");
        assert_eq!(app.roster.edits["adopt:ts-20-0"].approve, Some(true));
        press(&mut app, " ");
        assert_eq!(app.roster.edits["adopt:ts-20-0"].approve, None, "Space clears a mark");
        press(&mut app, " ");
        let rows: Vec<Row> = app.roster_rows().into_iter().filter(|r| matches!(r, Row::Adopt { .. }) && app.roster.edits.get(&app.roster_key(r)).and_then(|e| e.approve) == Some(true)).collect();
        assert_eq!(rows.len(), 1);
        // An adopt error keeps the remaining marks for another A.
        app.roster.adopt_queue = rows.clone().into();
        app.roster_accept("adopt", Err(RpcFail { unsupported: false, message: "handle_taken".into() }));
        assert!(app.roster.adopt_queue.is_empty());
        assert_eq!(app.roster.edits["adopt:ts-20-0"].approve, Some(true));
    }

    #[test]
    fn adopt_goes_by_session_uid_with_label_handle() {
        let app = roster_app();
        let row = app.roster_rows()[3].clone();
        let p = app.roster_adopt_params(&row, "rid");
        assert_eq!(p["session_uid"], "ts-20-0");
        assert_eq!(p["handle"], "items-lane");
        assert_eq!(p["kind"], "agent");
        assert_eq!(p["manager"], "owner");
        assert!(p.get("pid").is_none() && p.get("team_id").is_none());
    }

    #[test]
    fn an_old_daemon_shows_a_notice() {
        let mut app = roster_app();
        app.roster_accept("load", Err(RpcFail { unsupported: true, message: "unknown method".into() }));
        assert_eq!(app.roster.support, Support::Off);
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("not available"), "{text}");
        press(&mut app, "q");
        assert!(!app.roster.visible);
    }

    #[test]
    fn handles_from_labels() {
        assert_eq!(handle_from_label("Swarm Coord"), "swarm-coord");
        assert_eq!(handle_from_label("agent: orgchart #15"), "agent-orgchart-15");
        assert_eq!(handle_from_label("--x--"), "x");
    }
}
