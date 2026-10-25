//! The Sessions sidebar's Team sub-view (doc/org-model.md §11, S2): teams
//! parents-first, each team's members lead first, and under each member its
//! main session and its assistants: every session that reaches it through
//! the member stamp, its spawner chain, its task chain or its workspace
//! (`App::session_attachments`, the same rule as F6). Sessions Owner started
//! that reach no member follow under "Owner's sessions", the rare rest
//! under "Unassigned". `A-v` cycles Status → Task → Team; the
//! Team sub-view is skipped when the daemon has no org model, and draws the
//! Task tree until a snapshot with members arrives (app/org_model.rs).
//!
//! Team, member and Unassigned rows are tree headers, like task headers:
//! Space/Enter fold them (kept in the manifest as `folded_org`), and keys
//! typed there reach no terminal. A folded team hides its members and
//! sub-teams; a folded member hides its sessions.
use super::org_model::{Member, OrgSnapshot, Team};
use super::*;
use std::collections::HashSet;

pub(super) use super::orgchart_teams::{OWNER_SESSIONS_KEY, UNASSIGNED_KEY};
use super::orgchart_teams::Attach;
use std::collections::HashMap;

impl App {
    /// The Team sub-view is selected and the org model is live.
    pub(super) fn team_view_shown(&self) -> bool {
        self.sidebar_view == SidebarView::Team && self.view_mode == ViewMode::Sessions
    }

    /// The sidebar draws the Task tree: the Task sub-view, or the Team
    /// sub-view while the org model isn't live.
    pub(super) fn tree_view(&self) -> bool {
        match self.sidebar_view {
            SidebarView::Task => true,
            SidebarView::Team => self.org_live().is_none(),
            SidebarView::Status => false,
        }
    }

    /// The Team rows with folds applied.
    pub(super) fn visual_items_team(&self) -> Vec<VisualItem> {
        if self.org_live().is_none() {
            return self.visual_items_task();
        }
        let mut out = Vec::new();
        // Inside a folded team: skip until a header at its depth or above.
        let mut team_fold: Option<u8> = None;
        let mut member_fold = false;
        for item in self.visual_items_team_unfolded() {
            if let VisualItem::OrgHeader { key, depth } = &item {
                if team_fold.is_some_and(|d| *depth > d) {
                    continue;
                }
                team_fold = None;
                member_fold = false;
                if self.folded_org.contains(key) {
                    if key.starts_with("team:") {
                        team_fold = Some(*depth);
                    } else {
                        member_fold = true;
                    }
                }
                out.push(item);
                continue;
            }
            if matches!(item, VisualItem::Separator) {
                team_fold = None;
                member_fold = false;
                out.push(item);
                continue;
            }
            if team_fold.is_none() && !member_fold {
                out.push(item);
            }
        }
        out
    }

    /// Sessions a header's fold hides (the count beside a folded header).
    pub(super) fn org_header_hidden(&self, key: &str) -> usize {
        let items = self.visual_items_team_unfolded();
        let Some(at) = items.iter().position(|i| matches!(i, VisualItem::OrgHeader { key: k, .. } if k == key)) else { return 0 };
        let VisualItem::OrgHeader { depth, .. } = &items[at] else { return 0 };
        let team = key.starts_with("team:");
        items[at + 1..]
            .iter()
            .take_while(|i| match i {
                VisualItem::OrgHeader { depth: d, .. } => team && d > depth,
                VisualItem::Separator => false,
                _ => true,
            })
            .filter(|i| matches!(i, VisualItem::Session(..)))
            .count()
    }

    /// A header the cursor stands on that no longer draws falls back to the
    /// first session (or workspace) of the current view.
    pub(super) fn clamp_org_cursor(&mut self) {
        let Cursor::Org(key) = &self.cursor else { return };
        let items = self.visual_items();
        if items.iter().any(|i| matches!(i, VisualItem::OrgHeader { key: k, .. } if k == key)) {
            return;
        }
        self.cursor = items
            .iter()
            .find_map(|i| match i {
                VisualItem::Session(wi, si) => Some(Cursor::Session(*wi, *si)),
                VisualItem::WorkspaceHeader(wi) => Some(Cursor::Workspace(*wi)),
                _ => None,
            })
            .unwrap_or(Cursor::Workspace(0));
    }

    /// A-v: Status → Task → Team → Status. Team is skipped once the daemon
    /// has said it has no org model; before the first snapshot arrives (the
    /// Team view is what polls it) it draws the Task tree.
    pub(super) fn next_sidebar_view(&self) -> SidebarView {
        match self.sidebar_view {
            SidebarView::Status => SidebarView::Task,
            SidebarView::Task if self.org.support != super::org_model::Support::Off => SidebarView::Team,
            SidebarView::Task | SidebarView::Team => SidebarView::Status,
        }
    }

    /// Every Team row, ignoring folds (A-g reaches into folds through it).
    pub(super) fn visual_items_team_unfolded(&self) -> Vec<VisualItem> {
        let Some(snap) = self.org_live() else { return self.visual_items_task() };
        let continuous = self.continuous_members();
        let shown = |wi: usize, si: usize| {
            let ws = &self.workspaces[wi];
            !ws.is_closed && !self.is_past_workspace(wi) && !continuous.contains(&(wi, si))
        };
        let mut under: HashMap<Attach, Vec<(usize, usize)>> = HashMap::new();
        {
            let attached = self.session_attachments(snap);
            for (wi, ws) in self.workspaces.iter().enumerate() {
                for (si, ts) in ws.sessions.iter().enumerate() {
                    if let Some((to, _)) = attached.get(&ts.uid) {
                        under.entry(to.clone()).or_default().push((wi, si));
                    }
                }
            }
            for v in under.values_mut() {
                v.sort_by_key(|&(wi, si)| super::orgchart_teams::spawn_order(&self.workspaces[wi].sessions[si].uid));
            }
        }
        let mut claimed: HashSet<(usize, usize)> = HashSet::new();
        let mut items = Vec::new();
        let mut placed: HashSet<&str> = HashSet::new();
        for (team, depth) in team_order(snap) {
            items.push(VisualItem::OrgHeader { key: format!("team:{}", team.id), depth });
            // A team's own sessions (its coordinator task's tree) go with its
            // lead; a leadless team's sit under the team header.
            let led = team.lead_member_id.as_deref().and_then(|l| snap.member(l)).is_some_and(|l| !l.is_owner() && !l.retired());
            if !led {
                for (wi, si) in under.get(&Attach::Team(team.id.clone())).cloned().unwrap_or_default() {
                    if shown(wi, si) && claimed.insert((wi, si)) {
                        items.push(VisualItem::Session(wi, si));
                    }
                }
            }
            for m in team_members(snap, team) {
                if !placed.insert(m.id.as_str()) {
                    continue;
                }
                items.push(VisualItem::OrgHeader { key: format!("member:{}", m.id), depth: depth + 1 });
                for (wi, si) in self.member_rows(snap, m, &under) {
                    if shown(wi, si) && claimed.insert((wi, si)) {
                        items.push(VisualItem::Session(wi, si));
                    }
                }
            }
        }
        // Members on no team (not Owner): before the unassigned sessions.
        let mut loose: Vec<&Member> = snap.members.iter().filter(|m| !m.is_owner() && !m.retired() && !placed.contains(m.id.as_str())).collect();
        loose.sort_by_key(|m| m.handle.to_lowercase());
        for m in loose {
            items.push(VisualItem::OrgHeader { key: format!("member:{}", m.id), depth: 0 });
            for (wi, si) in self.member_rows(snap, m, &under) {
                if shown(wi, si) && claimed.insert((wi, si)) {
                    items.push(VisualItem::Session(wi, si));
                }
            }
        }
        let rest: Vec<(usize, usize)> = self
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(wi, ws)| (0..ws.sessions.len()).map(move |si| (wi, si)))
            .filter(|&(wi, si)| shown(wi, si) && !claimed.contains(&(wi, si)))
            .collect();
        // Owner started it (no spawner), else Unassigned.
        let (owners, orphans): (Vec<_>, Vec<_>) = rest.into_iter().partition(|&(wi, si)| self.workspaces[wi].sessions[si].managed_by_uid.is_none());
        for (key, rows) in [(OWNER_SESSIONS_KEY, owners), (UNASSIGNED_KEY, orphans)] {
            if rows.is_empty() {
                continue;
            }
            if !items.is_empty() {
                items.push(VisualItem::Separator);
            }
            items.push(VisualItem::OrgHeader { key: key.to_owned(), depth: 0 });
            items.extend(rows.into_iter().map(|(wi, si)| VisualItem::Session(wi, si)));
        }
        items
    }

    /// A member's main session, then its assistants (spawn order), with the
    /// sessions of the teams it leads.
    fn member_rows(&self, snap: &OrgSnapshot, m: &Member, under: &HashMap<Attach, Vec<(usize, usize)>>) -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = m.main_uid().and_then(|uid| self.find_session_by_uid(uid)).into_iter().collect();
        out.extend(under.get(&Attach::Member(m.id.clone())).into_iter().flatten().copied());
        for t in snap.teams.iter().filter(|t| !t.archived() && t.lead_member_id.as_deref() == Some(m.id.as_str())) {
            out.extend(under.get(&Attach::Team(t.id.clone())).into_iter().flatten().copied());
        }
        out
    }

    /// The text of a team / member / unassigned header row.
    pub(super) fn org_header_line(&self, key: &str, depth: u8, width: usize) -> Line<'static> {
        let folded = self.folded_org.contains(key);
        let pad = format!("{}{} ", "  ".repeat(depth as usize), if folded { "\u{25b8}" } else { "\u{25be}" });
        let count = if folded { format!(" ({})", self.org_header_hidden(key)) } else { String::new() };
        let snap = self.org_live();
        if key == UNASSIGNED_KEY || key == OWNER_SESSIONS_KEY {
            let label = if key == UNASSIGNED_KEY { super::orgchart_teams::UNASSIGNED_LABEL } else { super::orgchart_teams::OWNER_SESSIONS_LABEL };
            return Line::styled(format!("{pad}{label}{count}"), Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD));
        }
        if let Some(t) = key.strip_prefix("team:").and_then(|id| snap?.team(id)) {
            let mut st = Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD);
            if t.standby() {
                st = Style::default().fg(theme::DIM);
            }
            let lead = t.lead_member_id.as_deref().map(|l| format!("  \u{2605} {}", snap.map_or_else(String::new, |s| s.handle(l)))).unwrap_or_default();
            let glyph = if t.kind == "division" { symbols::DIVISION } else { symbols::HEADER_BOX };
            let text = format!("{pad}{glyph} {}{}{lead}{count}", t.name, if t.standby() { " (standby)" } else { "" });
            return Line::styled(crate::planning::truncate_with_ellipsis(&text, width), st);
        }
        if let Some(m) = key.strip_prefix("member:").and_then(|id| snap?.member(id)) {
            let lead = snap.is_some_and(|s| s.teams.iter().any(|t| t.lead_member_id.as_deref() == Some(m.id.as_str())));
            let mark = if m.vacant() { format!("{} ", symbols::VACANT) } else if lead { "\u{2605} ".to_owned() } else { String::new() };
            let title = m.title.as_deref().filter(|t| !t.is_empty()).map(|t| format!(" \u{00b7} {t}")).unwrap_or_default();
            let text = format!("{pad}{mark}{}{title}{count}", m.handle);
            let st = if m.vacant() { Style::default().fg(theme::DIM) } else { Style::default().fg(theme::TEXT) };
            return Line::styled(crate::planning::truncate_with_ellipsis(&text, width), st);
        }
        Line::styled(format!("{pad}?"), Style::default().fg(theme::DIM))
    }

    /// Filter text for a header row (A-/).
    pub(super) fn org_header_text(&self, key: &str) -> String {
        let snap = self.org_live();
        if key == UNASSIGNED_KEY {
            return super::orgchart_teams::UNASSIGNED_LABEL.to_owned();
        }
        if key == OWNER_SESSIONS_KEY {
            return super::orgchart_teams::OWNER_SESSIONS_LABEL.to_owned();
        }
        if let Some(t) = key.strip_prefix("team:").and_then(|id| snap?.team(id)) {
            return t.name.clone();
        }
        key.strip_prefix("member:").and_then(|id| snap?.member(id)).map_or_else(String::new, |m| m.handle.clone())
    }
}

/// Active and standby teams, parents first (divisions, then teams, then by
/// name), with their depth. A team whose parent is missing or archived is a
/// root; cycles end where they repeat.
fn team_order(snap: &OrgSnapshot) -> Vec<(&Team, u8)> {
    let live: Vec<&Team> = snap.teams.iter().filter(|t| !t.archived()).collect();
    let ids: HashSet<&str> = live.iter().map(|t| t.id.as_str()).collect();
    let mut sorted = live.clone();
    sorted.sort_by_key(|t| (super::orgchart_teams::team_rank(t), t.name.to_lowercase()));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn walk<'a>(t: &'a Team, depth: u8, all: &[&'a Team], seen: &mut HashSet<&'a str>, out: &mut Vec<(&'a Team, u8)>) {
        if !seen.insert(t.id.as_str()) {
            return;
        }
        out.push((t, depth));
        for c in all.iter().filter(|c| c.parent_team_id.as_deref() == Some(t.id.as_str())) {
            walk(c, depth.saturating_add(1), all, seen, out);
        }
    }
    for t in sorted.iter().filter(|t| t.parent_team_id.as_deref().is_none_or(|p| !ids.contains(p))) {
        walk(t, 0, &sorted, &mut seen, &mut out);
    }
    for t in &sorted {
        walk(t, 0, &sorted, &mut seen, &mut out);
    }
    out
}

/// A team's members: the lead first, then by handle. A member shows under
/// its primary team only (or the first team listing it when it has none).
fn team_members<'a>(snap: &'a OrgSnapshot, team: &Team) -> Vec<&'a Member> {
    let mut out: Vec<&Member> = snap
        .members
        .iter()
        .filter(|m| !m.is_owner() && !m.retired())
        .filter(|m| match m.primary_team_id.as_deref() {
            Some(p) => p == team.id,
            None => team.members.iter().any(|tm| tm.member_id == m.id),
        })
        .collect();
    let lead = team.lead_member_id.as_deref();
    out.sort_by_key(|m| (Some(m.id.as_str()) != lead, m.handle.to_lowercase()));
    out
}

#[cfg(test)]
mod tests {
    use super::super::org_model::fixtures::{member, owner, snapshot, team};
    use super::*;
    use crate::app::lifecycle::revive_session_tests::dummy_ts;
    use cm_daemon::host_id::HostId;
    use crossterm::event::KeyEvent;

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

    fn push(app: &mut App, wi: usize, uid: &str, label: &str) {
        let mut ts = dummy_ts(uid, false, HostId::local());
        ts.label = label.into();
        ts.session_type = "claude".into();
        app.workspaces[wi].sessions.push(ts);
    }

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

    fn app_with_team() -> App {
        let mut app = test_app();
        app.workspaces.push(ws("ws-lane"));
        for (n, label) in [(1u32, "ep-main"), (2, "helper"), (3, "stray")] {
            push(&mut app, 0, &format!("ts-{n:x}-0"), label);
        }
        let mut ep = member("m1", "ep", Some("mem-owner"), Some("t1"), Some("ts-1-0"));
        ep["home_workspace"] = serde_json::json!({"workspace_id": "other"});
        let lead_team = team("t1", "Swarm", None, Some("m1"), &[("m1", "lead"), ("m2", "member")]);
        // beta's main session sits in ep's workspace: it stays beta's.
        app.org.accept(snapshot(3, vec![owner(), ep, member("m2", "beta", Some("m1"), Some("t1"), Some("ts-3-0"))], vec![lead_team]));
        app
    }

    #[test]
    fn a_v_skips_team_on_daemons_without_an_org_model() {
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        assert_eq!(app.next_sidebar_view(), SidebarView::Team, "unknown: try it (the Team view probes)");
        app.org.fail(crate::app::org_model::RpcFail { unsupported: true, message: "unknown method".into() });
        assert_eq!(app.next_sidebar_view(), SidebarView::Status);
        let mut app = app_with_team();
        app.sidebar_view = SidebarView::Task;
        assert_eq!(app.next_sidebar_view(), SidebarView::Team);
        app.sidebar_view = SidebarView::Team;
        assert_eq!(app.next_sidebar_view(), SidebarView::Status);
    }

    #[test]
    fn team_rows_put_the_lead_first_with_its_sessions_and_unclaimed_last() {
        let mut app = app_with_team();
        app.sidebar_view = SidebarView::Team;
        let items = app.visual_items();
        let keys: Vec<String> = items
            .iter()
            .map(|i| match i {
                VisualItem::OrgHeader { key, depth } => format!("{depth}:{key}"),
                VisualItem::Session(_, si) => format!("s{si}"),
                VisualItem::Separator => "-".into(),
                other => format!("{other:?}"),
            })
            .collect();
        // ep's main session is s0; its home workspace isn't open, so its main
        // session's workspace supplies the rest (s1, not beta's s2).
        assert_eq!(keys, ["0:team:t1", "1:member:m1", "s0", "s1", "1:member:m2", "s2"]);
        let line = app.org_header_line("team:t1", 0, 60).to_string();
        assert!(line.contains("Swarm") && line.contains("\u{2605} ep"), "{line}");
        assert!(app.org_header_line("member:m1", 1, 60).to_string().contains("\u{2605} ep"));
    }

    #[test]
    fn owner_sessions_unassigned_and_old_daemons() {
        let mut app = app_with_team();
        app.workspaces.push(ws("solo"));
        push(&mut app, 1, "ts-9-0", "solo");
        app.sidebar_view = SidebarView::Team;
        let items = app.visual_items();
        // Owner started it and it reaches no member: Owner's sessions.
        let at = items.iter().position(|i| matches!(i, VisualItem::OrgHeader { key, .. } if key == OWNER_SESSIONS_KEY)).unwrap();
        assert_eq!(items[at + 1], VisualItem::Session(1, 0));
        assert!(!items.iter().any(|i| matches!(i, VisualItem::OrgHeader { key, .. } if key == UNASSIGNED_KEY)));
        // Spawned by a session that is gone: the rare Unassigned.
        push(&mut app, 1, "ts-a-0", "orphan");
        app.workspaces[1].sessions[1].managed_by_uid = Some("ts-gone-0".into());
        let items = app.visual_items();
        let at = items.iter().position(|i| matches!(i, VisualItem::OrgHeader { key, .. } if key == UNASSIGNED_KEY)).unwrap();
        assert_eq!(items[at + 1], VisualItem::Session(1, 1));
        assert!(app.org_header_line(OWNER_SESSIONS_KEY, 0, 60).to_string().contains("Owner's sessions"));
        // Headers are cursor stops, and keys there don't reach a terminal.
        app.cursor = Cursor::Org("team:t1".into());
        app.handle_event(&CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)));
        assert_eq!(app.cursor, Cursor::Org("member:m1".into()));
        // No live org: the Team view draws the Task tree.
        let mut app = test_app();
        app.sidebar_view = SidebarView::Team;
        assert!(!app.visual_items().iter().any(|i| matches!(i, VisualItem::OrgHeader { .. })));
    }

    #[test]
    fn space_folds_team_and_member_headers_and_a_g_reaches_into_folds() {
        let mut app = app_with_team();
        app.sidebar_view = SidebarView::Team;
        let space = CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.cursor = Cursor::Org("member:m1".into());
        app.handle_event(&space);
        assert!(app.folded_org.contains("member:m1"));
        let rows = |app: &App| app.visual_items().iter().filter(|i| matches!(i, VisualItem::Session(..))).count();
        assert_eq!(rows(&app), 1, "ep's sessions fold; beta's stays");
        let line = app.org_header_line("member:m1", 1, 60).to_string();
        assert!(line.contains("\u{25b8} \u{2605} ep") && line.ends_with(" (2)"), "{line}");
        app.cursor = Cursor::Org("team:t1".into());
        app.handle_event(&space);
        assert_eq!(app.visual_items(), vec![VisualItem::OrgHeader { key: "team:t1".into(), depth: 0 }]);
        assert_eq!(app.org_header_hidden("team:t1"), 3);
        // A-g still sees the folded sessions.
        assert_eq!(app.visual_items_team_unfolded().iter().filter(|i| matches!(i, VisualItem::Session(..))).count(), 3);
        // Leaving the Team view moves the cursor off a header that no longer draws.
        app.sidebar_view = SidebarView::Status;
        app.clamp_cursor();
        assert!(matches!(app.cursor, Cursor::Session(..)));
    }
}
