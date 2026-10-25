//! S3 boards (doc/org-model.md §7.3, §7.6, §11): headings and epics as a
//! folding tree, a PoC chip and team on each epic, and `g` on a group to
//! edit it (promote to epic, set the PoC, rename, add a heading under it,
//! close the epic through its close plan).
//!
//! A board is S3 when `board.read` carries `groups`; an older API omits the
//! field and the overlay keeps today's free-text headings. Group writes on a
//! daemon or API without them answer unsupported (app/org_model.rs) and the
//! overlay says so instead of failing loudly.
use super::*;

/// A display row of the overlay.
#[derive(Clone, Debug, PartialEq)]
pub(in crate::app) enum DRow {
    /// A heading or epic (S3 boards), at its tree depth.
    Group { id: String, depth: usize },
    /// An open item by index into `items`. `loose`: drawn under today's
    /// free-text group headings (old boards, and S3 items with no group).
    Item { idx: usize, depth: usize, loose: bool },
    /// A recently closed item by index.
    Closed(usize),
}

/// One row of a close plan: an item, the team, a member or a workspace.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PlanRow {
    /// `items`, `team`, `members` or `workspaces`: the `choices` key.
    pub section: &'static str,
    /// The item number or member/workspace id (empty for the team row).
    pub key: String,
    pub label: String,
    pub options: Vec<String>,
    pub choice: usize,
    /// Who must authorize it when the closer can't (`manager_chain`, `owner`).
    pub needs: Option<String>,
}

/// `epic.close_plan`'s reply, under review.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct ClosePlan {
    pub plan_id: String,
    pub revision: i64,
    pub name: String,
    pub outcome: String,
    pub rows: Vec<PlanRow>,
    pub cursor: usize,
    /// `epic.close_apply`'s pending rows once applied: who must decide.
    pub pending: Vec<String>,
    pub applied: bool,
}

impl ClosePlan {
    pub(super) fn from_reply(v: &Value, name: &str, outcome: &str) -> Self {
        let mut rows = Vec::new();
        let opts = |x: &Value, key: &str| -> Vec<String> { x[key].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(str::to_owned)).collect() };
        let pick = |options: &[String], x: &Value| x["default"].as_str().and_then(|d| options.iter().position(|o| o == d)).unwrap_or(0);
        for it in v["items"].as_array().into_iter().flatten() {
            let options = opts(it, "choice_options");
            rows.push(PlanRow {
                section: "items",
                key: it["n"].to_string(),
                label: format!("#{} {}", it["n"], it["title"].as_str().unwrap_or("")),
                choice: pick(&options, it),
                options,
                needs: None,
            });
        }
        let team = &v["team"];
        if team.is_object() {
            let options = opts(team, "options");
            rows.push(PlanRow { section: "team", key: String::new(), label: format!("team {}", team["name"].as_str().unwrap_or("")), choice: pick(&options, team), options, needs: team["needs"].as_str().map(str::to_owned) });
        }
        for m in v["members"].as_array().into_iter().flatten() {
            let options = opts(m, "options");
            rows.push(PlanRow {
                section: "members",
                key: m["member_id"].as_str().unwrap_or("").to_owned(),
                label: format!("{} ({})", m["handle"].as_str().or(m["member_id"].as_str()).unwrap_or("?"), m["role"].as_str().unwrap_or("member")),
                choice: pick(&options, m),
                options,
                needs: m["needs"].as_str().filter(|n| *n != "closer").map(str::to_owned),
            });
        }
        for w in v["workspaces"].as_array().into_iter().flatten() {
            let options = opts(w, "options");
            rows.push(PlanRow {
                section: "workspaces",
                key: w["workspace_id"].as_str().unwrap_or("").to_owned(),
                label: format!("workspace {}", w["path"].as_str().or(w["workspace_id"].as_str()).unwrap_or("?")),
                choice: pick(&options, w),
                options,
                needs: None,
            });
        }
        ClosePlan {
            plan_id: v["plan_id"].as_str().unwrap_or("").to_owned(),
            revision: v["revision"].as_i64().unwrap_or(0),
            name: name.to_owned(),
            outcome: outcome.to_owned(),
            rows,
            ..ClosePlan::default()
        }
    }

    /// `choices` for `epic.close_apply`: `{items: {n: opt}, team: opt,
    /// members: {id: opt}, workspaces: {id: opt}}`.
    pub(super) fn choices(&self) -> Value {
        let mut out = json!({"items": {}, "members": {}, "workspaces": {}});
        for r in &self.rows {
            let Some(opt) = r.options.get(r.choice) else { continue };
            if r.section == "team" {
                out["team"] = json!(opt);
            } else {
                out[r.section][&r.key] = json!(opt);
            }
        }
        out
    }
}

impl Board {
    pub(super) fn groups(&self) -> Option<&[Value]> {
        self.data["groups"].as_array().map(Vec::as_slice)
    }

    pub(super) fn group(&self, id: &str) -> Option<&Value> {
        self.groups()?.iter().find(|g| g["id"].as_str() == Some(id))
    }

    /// Display rows: flagged items first (the daemon's order), then today's
    /// free-text groups, or on an S3 board the group tree with each group's
    /// items under it, then items with no group; then the closed strip.
    pub(super) fn display_rows(&self) -> Vec<DRow> {
        let items = self.items();
        let closed = (0..self.closed().len().min(CLOSED_SHOWN)).map(DRow::Closed);
        let Some(groups) = self.groups() else {
            return (0..items.len()).map(|idx| DRow::Item { idx, depth: 0, loose: true }).chain(closed).collect();
        };
        let flagged = |i: &Value| i["flags"].as_array().is_some_and(|f| !f.is_empty());
        let mut out: Vec<DRow> = items.iter().enumerate().filter(|(_, i)| flagged(i)).map(|(idx, _)| DRow::Item { idx, depth: 0, loose: false }).collect();
        let ids: HashSet<&str> = groups.iter().filter_map(|g| g["id"].as_str()).collect();
        let has_open = |id: &str| items.iter().any(|i| i["group_id"].as_str() == Some(id));
        // A closed group stays only while it still has open items.
        let shown = |g: &Value| g["closed_at"].is_null() || g["id"].as_str().is_some_and(has_open);
        let mut kids: HashMap<Option<&str>, Vec<&Value>> = HashMap::new();
        for g in groups.iter().filter(|g| shown(g)) {
            let parent = g["parent_group_id"].as_str().filter(|p| ids.contains(p) && Some(*p) != g["id"].as_str());
            kids.entry(parent).or_default().push(g);
        }
        for v in kids.values_mut() {
            v.sort_by(|a, b| {
                let ord = |g: &Value| g["ord"].as_f64().unwrap_or(f64::MAX);
                ord(a).total_cmp(&ord(b)).then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
            });
        }
        let mut seen: HashSet<&str> = HashSet::new();
        let mut stack: Vec<(&Value, usize)> = kids.get(&None).into_iter().flatten().rev().map(|g| (*g, 0)).collect();
        let all: Vec<&Value> = groups.iter().filter(|g| shown(g)).collect();
        loop {
            while let Some((g, depth)) = stack.pop() {
                let Some(id) = g["id"].as_str() else { continue };
                if !seen.insert(id) {
                    continue;
                }
                out.push(DRow::Group { id: id.to_owned(), depth });
                if self.folds.contains(id) {
                    // Its subtree is reached, just hidden.
                    let mut hidden: Vec<&str> = vec![id];
                    while let Some(h) = hidden.pop() {
                        for c in kids.get(&Some(h)).into_iter().flatten().filter_map(|c| c["id"].as_str()) {
                            if seen.insert(c) {
                                hidden.push(c);
                            }
                        }
                    }
                    continue;
                }
                for (idx, _) in items.iter().enumerate().filter(|(_, i)| !flagged(i) && i["group_id"].as_str() == Some(id)) {
                    out.push(DRow::Item { idx, depth: depth + 1, loose: false });
                }
                for c in kids.get(&Some(id)).into_iter().flatten().rev() {
                    stack.push((c, depth + 1));
                }
            }
            // A parent_group_id cycle leaves groups unreached: roots then.
            match all.iter().find(|g| g["id"].as_str().is_some_and(|id| !seen.contains(id))) {
                Some(g) => stack.push((g, 0)),
                None => break,
            }
        }
        // Items with no (known, shown) group, under today's headings.
        let placed: HashSet<usize> = out.iter().filter_map(|r| if let DRow::Item { idx, .. } = r { Some(*idx) } else { None }).collect();
        // An item whose group is in the tree but not walked sits under a fold.
        let in_tree: HashSet<&str> = kids.values().flatten().filter_map(|g| g["id"].as_str()).collect();
        let folded_away = |i: &Value| i["group_id"].as_str().is_some_and(|g| in_tree.contains(g));
        for (idx, i) in items.iter().enumerate() {
            if !placed.contains(&idx) && !folded_away(i) {
                out.push(DRow::Item { idx, depth: 0, loose: true });
            }
        }
        out.extend(closed);
        out
    }

    /// The selected group row's id.
    pub(super) fn selected_group(&self) -> Option<String> {
        match self.display_rows().get(self.cursor)? {
            DRow::Group { id, .. } => Some(id.clone()),
            _ => None,
        }
    }

    /// Open items in a group's subtree (folded or not).
    fn subtree_items(&self, id: &str) -> Vec<Value> {
        let groups = self.groups().unwrap_or(&[]);
        let mut ids: HashSet<String> = HashSet::from([id.to_owned()]);
        loop {
            let before = ids.len();
            for g in groups {
                if let (Some(gid), Some(p)) = (g["id"].as_str(), g["parent_group_id"].as_str()) {
                    if ids.contains(p) {
                        ids.insert(gid.to_owned());
                    }
                }
            }
            if ids.len() == before {
                break;
            }
        }
        self.items().iter().filter(|i| i["group_id"].as_str().is_some_and(|g| ids.contains(g))).cloned().collect()
    }
}

impl App {
    /// `(PoC handle, team name)` for an epic: the team's lead from the org
    /// snapshot when there is one, else the API's derived `poc_member_id`.
    pub(super) fn epic_poc(&self, g: &Value) -> (Option<String>, Option<String>) {
        let snap = self.org.snapshot.as_ref();
        let team = g["team_id"].as_str().and_then(|t| snap.and_then(|s| s.team(t)));
        let poc = team.and_then(|t| t.lead_member_id.clone()).or_else(|| g["poc_member_id"].as_str().map(str::to_owned));
        let handle = poc.map(|id| snap.map_or_else(|| id.chars().take(8).collect(), |s| s.handle(&id)));
        (handle, team.map(|t| t.name.clone()))
    }

    /// `▾ ⟐ S3 epics  active  PoC ep · Swarm   3 open: …`
    pub(super) fn board_group_row(&self, id: &str, depth: usize, width: usize, tint: f64, selected: bool) -> Line<'static> {
        let b = &self.board;
        let Some(g) = b.group(id) else { return Line::from("") };
        let epic = g["kind"] == "epic";
        let fold = if b.folds.contains(id) { "\u{25b8}" } else { "\u{25be}" };
        let bg = if selected { theme::SELECT_BG } else { theme::sidebar_section_bg(Some(if epic { "magenta" } else { "blue" }), tint) };
        let mut spans = vec![
            Span::styled(format!("{}{fold} ", "  ".repeat(depth)), Style::default().fg(theme::MUTED).bg(bg)),
        ];
        if epic {
            spans.push(Span::styled(format!("{} ", symbols::EPIC), Style::default().fg(theme::HEADER).bg(bg)));
        }
        spans.push(Span::styled(g["name"].as_str().unwrap_or("?").to_owned(), Style::default().fg(theme::HEADER).bg(bg).add_modifier(Modifier::BOLD)));
        if epic {
            let status = g["epic_status"].as_str().unwrap_or("active").to_owned();
            spans.push(Span::styled(format!("  {status}"), Style::default().fg(theme::MUTED).bg(bg)));
            let (poc, team) = self.epic_poc(g);
            let chip = match (poc, team) {
                (Some(p), Some(t)) => format!("  PoC {p} \u{00b7} {t}"),
                (Some(p), None) => format!("  PoC {p}"),
                (None, _) => "  no PoC".to_owned(),
            };
            spans.push(Span::styled(chip, Style::default().fg(theme::LEAD).bg(bg)));
        }
        let counts = status_counts(&b.subtree_items(id));
        spans.push(Span::styled(format!("   {counts}"), Style::default().fg(theme::MUTED).bg(bg)));
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        spans.push(Span::styled(" ".repeat(width.saturating_sub(used)), Style::default().bg(bg)));
        clip_spans(spans, width)
    }

    /// `g` on a group row: the group menu. Elsewhere `g` keeps refreshing.
    pub(super) fn board_group_key(&mut self, c: char) {
        self.board.group_menu = false;
        let Some(id) = self.board.selected_group() else { return };
        let Some(g) = self.board.group(&id).cloned() else { return };
        let epic = g["kind"] == "epic";
        let rev = g["revision"].clone();
        match c {
            'e' if !epic => {
                let rid = self.board_request_id("promote");
                self.board_write("group.set", json!({"group_id": id, "kind": "epic", "expected_revision": rev, "request_id": rid}));
            }
            'e' => self.board.status = "Already an epic".into(),
            'p' if epic => self.board.prompt = Some((PromptKind::GroupPoc, String::new())),
            'p' => self.board.status = "Promote it to an epic first (e)".into(),
            'r' => self.board.prompt = Some((PromptKind::GroupRename, g["name"].as_str().unwrap_or("").to_owned())),
            'n' => self.board.prompt = Some((PromptKind::GroupNew, String::new())),
            'c' if epic => self.board.prompt = Some((PromptKind::GroupClose, "done".into())),
            'c' => self.board.status = "Only an epic closes (e promotes it)".into(),
            _ => {}
        }
    }

    pub(super) fn board_request_id(&mut self, what: &str) -> String {
        self.board.seq += 1;
        format!("tui-board-{what}-{}-{}", now_unix(), self.board.seq)
    }

    /// The group prompts' Enter.
    pub(super) fn board_group_submit(&mut self, kind: PromptKind, text: String) {
        let Some(id) = self.board.selected_group() else { return };
        let rev = self.board.group(&id).map(|g| g["revision"].clone()).unwrap_or(Value::Null);
        match kind {
            PromptKind::GroupRename if !text.is_empty() => {
                let rid = self.board_request_id("rename");
                self.board_write("group.set", json!({"group_id": id, "name": text, "expected_revision": rev, "request_id": rid}));
            }
            PromptKind::GroupNew if !text.is_empty() => {
                let rid = self.board_request_id("new");
                self.board_write("group.create", json!({"name": text, "kind": "heading", "parent_group_id": id, "request_id": rid}));
            }
            PromptKind::GroupPoc if !text.is_empty() => {
                // A handle, id or `owner`; the API resolves it and checks authority.
                let member = self.org.snapshot.as_ref().and_then(|s| s.resolve_member(&text)).map_or(text.clone(), |m| if m.is_owner() { "owner".into() } else { m.id.clone() });
                let rid = self.board_request_id("poc");
                self.board_write("epic.set_poc", json!({"group_id": id, "member": member, "request_id": rid}));
            }
            PromptKind::GroupClose => {
                let outcome = if text.trim().starts_with('c') { "cancelled" } else { "done" };
                let name = self.board.group(&id).and_then(|g| g["name"].as_str()).unwrap_or("").to_owned();
                self.board.pending_close = Some((name, outcome.to_owned()));
                self.board_write("epic.close_plan", json!({"group_id": id, "outcome": outcome}));
            }
            _ => {}
        }
    }

    /// A write reply that carries more than "saved": PoC staffing offers,
    /// a close plan, a close outcome.
    pub(super) fn board_write_reply(&mut self, v: &Value) {
        if let Some((name, outcome)) = self.board.pending_close.take().filter(|_| v["plan_id"].is_string()) {
            self.board.plan = Some(ClosePlan::from_reply(v, &name, &outcome));
            self.board.status = "Close plan: review the choices, A applies".into();
        } else if let Some(staff) = v["staffing"].as_array() {
            let holders: usize = staff.len();
            self.board.status = format!("PoC set (team {}); {holders} holder{} to offer staffing", v["team_id"].as_str().unwrap_or("?"), if holders == 1 { "" } else { "s" });
        } else if v["applied"].is_boolean() || v["pending"].is_array() {
            let pending: Vec<String> = v["pending"].as_array().into_iter().flatten().map(|p| format!("{} → {}", p["row"].as_str().unwrap_or("?"), p["who_must_decide"].as_str().unwrap_or("?"))).collect();
            if let Some(plan) = self.board.plan.as_mut() {
                plan.applied = true;
                plan.pending = pending.clone();
            }
            self.board.status = if pending.is_empty() { "Epic closed".into() } else { format!("Epic closed; {} pending", pending.len()) };
        }
    }

    /// The close plan's keys; Esc leaves it (nothing applied).
    pub(super) fn board_plan_event(&mut self, key: &crossterm::event::KeyEvent) {
        let Some(plan) = self.board.plan.as_mut() else { return };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.board.plan = None;
                self.board.status = "Close plan left; nothing applied".into();
            }
            KeyCode::Char('j') | KeyCode::Down => plan.cursor = (plan.cursor + 1).min(plan.rows.len().saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => plan.cursor = plan.cursor.saturating_sub(1),
            KeyCode::Char(' ') | KeyCode::Char('l') | KeyCode::Right if !plan.applied => {
                if let Some(r) = plan.rows.get_mut(plan.cursor).filter(|r| !r.options.is_empty()) {
                    r.choice = (r.choice + 1) % r.options.len();
                }
            }
            KeyCode::Char('h') | KeyCode::Left if !plan.applied => {
                if let Some(r) = plan.rows.get_mut(plan.cursor).filter(|r| !r.options.is_empty()) {
                    r.choice = (r.choice + r.options.len() - 1) % r.options.len();
                }
            }
            KeyCode::Char('A') if !plan.applied => {
                let mut params = json!({"plan_id": plan.plan_id, "revision": plan.revision, "choices": plan.choices()});
                params["request_id"] = json!(self.board_request_id("close"));
                self.board_write("epic.close_apply", params);
            }
            _ => {}
        }
    }

    pub(super) fn draw_close_plan(&self, frame: &mut Frame, area: Rect) {
        let Some(plan) = &self.board.plan else { return };
        let width = area.width as usize;
        let mut lines = vec![
            Line::styled(format!("Close epic {} as {}", plan.name, plan.outcome), Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD)),
            Line::styled("Space/h/l change a choice \u{00b7} A apply \u{00b7} Esc leave (nothing applied)", Style::default().fg(theme::DIM)),
            Line::from(""),
        ];
        let mut section = "";
        for (k, r) in plan.rows.iter().enumerate() {
            if r.section != section {
                section = r.section;
                let title = match section {
                    "items" => "Open items",
                    "team" => "Team",
                    "members" => "Members",
                    _ => "Workspaces",
                };
                lines.push(Line::styled(title, Style::default().fg(theme::HEADER)));
            }
            let choice = r.options.get(r.choice).cloned().unwrap_or_else(|| "—".into());
            let needs = r.needs.as_deref().map(|n| format!("  (needs {n})")).unwrap_or_default();
            let text = format!("  {:<40} {choice}{needs}", truncate(&r.label, 40));
            let mut style = Style::default().fg(theme::TEXT);
            if k == plan.cursor {
                style = style.bg(theme::SELECT_BG);
            }
            lines.push(Line::styled(truncate(&text, width), style));
        }
        if plan.applied {
            lines.push(Line::from(""));
            lines.push(Line::styled(if plan.pending.is_empty() { "Applied.".to_owned() } else { "Applied; pending decisions:".to_owned() }, Style::default().fg(theme::OK)));
            for p in &plan.pending {
                lines.push(Line::styled(format!("  {p}"), Style::default().fg(theme::ATTN)));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::org_model::fixtures::{member, owner, snapshot, team};
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

    fn key(app: &mut App, c: KeyCode) {
        app.handle_event(&CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE)));
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            key(app, KeyCode::Char(c));
        }
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 34)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    /// An S3 board: epic "Org model" (team Swarm, led by ep) holding a
    /// heading "TUI"; a heading "Ops"; one flagged item; one item with no group.
    fn s3_app() -> App {
        let mut app = test_app();
        app.org.accept(snapshot(1, vec![owner(), member("m1", "ep", Some("mem-owner"), Some("t1"), None)], vec![team("t1", "Swarm", None, Some("m1"), &[("m1", "primary")])]));
        app.board.visible = true;
        app.board.listed = true;
        app.board.last_poll = Some(Instant::now());
        app.board.last_full = Some(Instant::now());
        app.board.boards = vec![json!({"slug": "swarm", "name": "Swarm", "initiative_id": "ini-1"})];
        app.board.accept(json!({
            "board": {"slug": "swarm", "version": 4, "orchestrator": "none", "health": {"unresolved": 1}},
            "groups": [
                {"id": "g-ops", "name": "Ops", "parent_group_id": null, "ord": 2, "kind": "heading", "revision": 1, "closed_at": null},
                {"id": "g-epic", "name": "Org model", "parent_group_id": null, "ord": 1, "kind": "epic", "epic_status": "active", "team_id": "t1", "poc_member_id": "m1", "revision": 3, "closed_at": null},
                {"id": "g-tui", "name": "TUI", "parent_group_id": "g-epic", "ord": 1, "kind": "heading", "revision": 1, "closed_at": null},
                {"id": "g-old", "name": "Old", "parent_group_id": null, "ord": 3, "kind": "epic", "epic_status": "done", "revision": 1, "closed_at": "2026-10-01T00:00:00Z"}
            ],
            "items": [
                {"n": 7, "status": "active", "title": "flagged one", "group_id": "g-ops", "flags": [{"kind": "holder_idle"}], "holders": []},
                {"n": 15, "status": "active", "title": "org model TUI", "group_id": "g-tui", "holders": []},
                {"n": 13, "status": "active", "title": "org core", "group_id": "g-epic", "holders": []},
                {"n": 20, "status": "open", "title": "rotate logs", "group_id": "g-ops", "holders": []},
                {"n": 30, "status": "open", "title": "loose", "group": "Misc", "holders": []}
            ],
            "recently_closed": []
        }));
        app
    }

    #[test]
    fn groups_draw_as_a_tree_with_items_under_them() {
        let app = s3_app();
        let rows = app.board.display_rows();
        let want = vec![
            DRow::Item { idx: 0, depth: 0, loose: false },
            DRow::Group { id: "g-epic".into(), depth: 0 },
            DRow::Item { idx: 2, depth: 1, loose: false },
            DRow::Group { id: "g-tui".into(), depth: 1 },
            DRow::Item { idx: 1, depth: 2, loose: false },
            DRow::Group { id: "g-ops".into(), depth: 0 },
            DRow::Item { idx: 3, depth: 1, loose: false },
            DRow::Item { idx: 4, depth: 0, loose: true },
        ];
        assert_eq!(rows, want, "closed epics with no open items are hidden");
    }

    #[test]
    fn epics_show_status_and_poc_chip() {
        let mut app = s3_app();
        let s = screen(&mut app);
        assert!(s.contains("\u{27d0} Org model  active  PoC ep \u{00b7} Swarm"), "{s}");
        assert!(s.contains("TUI") && s.contains("Ops") && s.contains("Misc"), "{s}");
        assert!(!s.contains("Old"), "{s}");
        // The PoC chip comes from the API's derived field without a snapshot.
        app.org = Default::default();
        let s = screen(&mut app);
        assert!(s.contains("PoC m1"), "{s}");
    }

    #[test]
    fn space_folds_a_group_and_its_subtree() {
        let mut app = s3_app();
        press(&mut app, "j"); // the epic row
        assert_eq!(app.board.selected_group().as_deref(), Some("g-epic"));
        assert!(app.board.selected().is_none());
        press(&mut app, " ");
        let rows = app.board.display_rows();
        assert!(!rows.contains(&DRow::Group { id: "g-tui".into(), depth: 1 }));
        assert!(!rows.iter().any(|r| matches!(r, DRow::Item { idx: 1 | 2, .. })), "{rows:?}");
        // Folded items don't fall back to the ungrouped list.
        assert!(rows.contains(&DRow::Item { idx: 4, depth: 0, loose: true }));
        press(&mut app, " ");
        assert_eq!(app.board.display_rows().len(), 8);
    }

    /// Hold the request slot so writes queue and can be inspected.
    fn hold(app: &mut App) {
        let (_tx, rx) = std::sync::mpsc::channel();
        app.board.rx = Some(rx);
    }

    #[test]
    fn g_on_a_group_edits_it_and_g_elsewhere_still_refreshes() {
        let mut app = s3_app();
        // On an item, g refreshes as before.
        press(&mut app, "g");
        assert!(!app.board.listed && !app.board.group_menu);
        let mut app = s3_app();
        hold(&mut app);
        press(&mut app, "jjjjj"); // Ops heading
        assert_eq!(app.board.selected_group().as_deref(), Some("g-ops"));
        press(&mut app, "ge");
        let (m, p) = app.board.queued.pop_front().unwrap();
        assert_eq!(m, "group.set");
        assert_eq!((p["board"].as_str(), p["group_id"].as_str(), p["kind"].as_str(), p["expected_revision"].as_i64()), (Some("swarm"), Some("g-ops"), Some("epic"), Some(1)));
        // A heading has no PoC yet.
        press(&mut app, "gp");
        assert!(app.board.prompt.is_none());
        assert!(app.board.status.contains("Promote"));
        // On the epic: PoC by handle → member id.
        app.board.cursor = 1;
        press(&mut app, "gp");
        press(&mut app, "ep");
        key(&mut app, KeyCode::Enter);
        let (m, p) = app.board.queued.pop_front().unwrap();
        assert_eq!((m, p["member"].as_str()), ("epic.set_poc", Some("m1")));
        press(&mut app, "gn");
        press(&mut app, "API");
        key(&mut app, KeyCode::Enter);
        let (m, p) = app.board.queued.pop_front().unwrap();
        assert_eq!((m, p["parent_group_id"].as_str(), p["name"].as_str()), ("group.create", Some("g-epic"), Some("API")));
    }

    #[test]
    fn closing_an_epic_reviews_and_applies_its_plan() {
        let mut app = s3_app();
        app.board.cursor = 1;
        hold(&mut app);
        press(&mut app, "gc");
        key(&mut app, KeyCode::Enter); // default "done"
        let (m, p) = app.board.queued.pop_front().unwrap();
        assert_eq!((m, p["outcome"].as_str()), ("epic.close_plan", Some("done")));
        app.board_write_reply(&json!({
            "plan_id": "plan-1", "revision": 2,
            "items": [{"n": 13, "title": "org core", "choice_options": ["move", "unplace", "drop"]}],
            "team": {"name": "Swarm", "options": ["keep", "standby", "archive"], "default": "keep"},
            "members": [{"member_id": "m2", "handle": "orgchart", "role": "primary", "options": ["keep", "move", "retire"], "default": "keep", "needs": "manager_chain"}],
            "workspaces": []
        }));
        assert!(app.board.plan.is_some());
        let s = screen(&mut app);
        assert!(s.contains("Close epic Org model as done") && s.contains("#13 org core") && s.contains("needs manager_chain"), "{s}");
        press(&mut app, "j "); // team → standby
        press(&mut app, "A");
        let (m, p) = app.board.queued.pop_front().unwrap();
        assert_eq!(m, "epic.close_apply");
        assert_eq!(p["plan_id"], "plan-1");
        assert_eq!(p["revision"], 2);
        assert_eq!(p["choices"], json!({"items": {"13": "move"}, "team": "standby", "members": {"m2": "keep"}, "workspaces": {}}));
        app.board_write_reply(&json!({"applied": true, "pending": [{"row": "m2", "who_must_decide": "ep"}]}));
        assert_eq!(app.board.status, "Epic closed; 1 pending");
        assert!(screen(&mut app).contains("m2 \u{2192} ep"));
        key(&mut app, KeyCode::Esc);
        assert!(app.board.plan.is_none() && app.board.visible);
    }

    #[test]
    fn a_group_cycle_still_shows_every_group() {
        let mut app = s3_app();
        let mut data = app.board.data.clone();
        data["groups"][2]["parent_group_id"] = json!("g-ops");
        data["groups"][0]["parent_group_id"] = json!("g-tui");
        app.board.accept(data);
        let groups: Vec<String> = app.board.display_rows().into_iter().filter_map(|r| if let DRow::Group { id, .. } = r { Some(id) } else { None }).collect();
        assert_eq!(groups.len(), 3, "{groups:?}");
    }

    #[test]
    fn an_old_board_keeps_free_text_groups() {
        let mut app = s3_app();
        let mut data = app.board.data.clone();
        data.as_object_mut().unwrap().remove("groups");
        app.board.accept(data);
        assert!(app.board.display_rows().iter().all(|r| matches!(r, DRow::Item { depth: 0, loose: true, .. })));
        press(&mut app, "jg");
        assert!(!app.board.group_menu, "g refreshes");
    }
}
