//! The Sessions sidebar's Team sub-view (doc/org-model.md §11, S2): teams
//! parents-first, each team's members lead first, and under each member its
//! main session and the sessions of its home workspace. Sessions no member
//! claims follow under "Unassigned". `A-v` cycles Status → Task → Team; the
//! Team sub-view is skipped when the daemon has no org model, and draws the
//! Task tree until a snapshot with members arrives (app/org_model.rs).
//!
//! Team and member rows are headers only: the cursor moves over sessions.
use super::org_model::{Member, OrgSnapshot, Team};
use super::*;
use std::collections::HashSet;

pub(super) const UNASSIGNED_KEY: &str = "org:unassigned";

impl App {
    /// The Team sub-view is selected and the org model is live.
    pub(super) fn team_view_shown(&self) -> bool {
        self.sidebar_view == SidebarView::Team && self.view_mode == ViewMode::Sessions
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

    pub(super) fn visual_items_team(&self) -> Vec<VisualItem> {
        let Some(snap) = self.org_live() else { return self.visual_items_task() };
        let continuous = self.continuous_members();
        let shown = |wi: usize, si: usize| {
            let ws = &self.workspaces[wi];
            !ws.is_closed && !self.is_past_workspace(wi) && !continuous.contains(&(wi, si))
        };
        let mut claimed: HashSet<(usize, usize)> = HashSet::new();
        let mut items = Vec::new();
        let mut placed: HashSet<&str> = HashSet::new();
        for (team, depth) in team_order(snap) {
            items.push(VisualItem::OrgHeader { key: format!("team:{}", team.id), depth });
            for m in team_members(snap, team) {
                if !placed.insert(m.id.as_str()) {
                    continue;
                }
                items.push(VisualItem::OrgHeader { key: format!("member:{}", m.id), depth: depth + 1 });
                for (wi, si) in self.member_rows(m) {
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
            for (wi, si) in self.member_rows(m) {
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
        if !rest.is_empty() {
            if !items.is_empty() {
                items.push(VisualItem::Separator);
            }
            items.push(VisualItem::OrgHeader { key: UNASSIGNED_KEY.to_owned(), depth: 0 });
            items.extend(rest.into_iter().map(|(wi, si)| VisualItem::Session(wi, si)));
        }
        items
    }

    /// A member's main session, then the other sessions of its home
    /// workspace (else of its main session's workspace), by spawn order.
    fn member_rows(&self, m: &Member) -> Vec<(usize, usize)> {
        let main = m.main_uid().and_then(|uid| self.find_session_by_uid(uid));
        let home = m.home_workspace.as_ref().and_then(|h| {
            self.workspaces.iter().position(|w| {
                !w.is_closed
                    && (h.workspace_id.as_deref() == Some(w.id.as_str())
                        || h.path.as_deref().is_some_and(|p| w.worktree_path.as_deref() == Some(std::path::Path::new(p))))
            })
        });
        let mut out: Vec<(usize, usize)> = main.into_iter().collect();
        if let Some(wi) = home.or(main.map(|(wi, _)| wi)) {
            out.extend((0..self.workspaces[wi].sessions.len()).map(|si| (wi, si)).filter(|p| Some(*p) != main));
        }
        out
    }

    /// The text of a team / member / unassigned header row.
    pub(super) fn org_header_line(&self, key: &str, depth: u8, width: usize) -> Line<'static> {
        let pad = "  ".repeat(depth as usize + 1);
        let snap = self.org_live();
        if key == UNASSIGNED_KEY {
            return Line::styled(format!("{pad}{}", super::orgchart_teams::UNASSIGNED_LABEL), Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD));
        }
        if let Some(t) = key.strip_prefix("team:").and_then(|id| snap?.team(id)) {
            let mut st = Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD);
            if t.standby() {
                st = Style::default().fg(theme::DIM);
            }
            let lead = t.lead_member_id.as_deref().map(|l| format!("  \u{2605} {}", snap.map_or_else(String::new, |s| s.handle(l)))).unwrap_or_default();
            let glyph = if t.kind == "division" { symbols::DIVISION } else { symbols::HEADER_BOX };
            let text = format!("{pad}{glyph} {}{}{lead}", t.name, if t.standby() { " (standby)" } else { "" });
            return Line::styled(crate::planning::truncate_with_ellipsis(&text, width), st);
        }
        if let Some(m) = key.strip_prefix("member:").and_then(|id| snap?.member(id)) {
            let lead = snap.is_some_and(|s| s.teams.iter().any(|t| t.lead_member_id.as_deref() == Some(m.id.as_str())));
            let mark = if m.vacant() { format!("{} ", symbols::VACANT) } else if lead { "\u{2605} ".to_owned() } else { String::new() };
            let title = m.title.as_deref().filter(|t| !t.is_empty()).map(|t| format!(" \u{00b7} {t}")).unwrap_or_default();
            let text = format!("{pad}{mark}{}{title}", m.handle);
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
        app.org.accept(snapshot(3, vec![owner(), ep, member("m2", "beta", Some("m1"), Some("t1"), None)], vec![lead_team]));
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
        // session's workspace supplies the rest (s1, s2).
        assert_eq!(keys, ["0:team:t1", "1:member:m1", "s0", "s1", "s2", "1:member:m2"]);
        let line = app.org_header_line("team:t1", 0, 60).to_string();
        assert!(line.contains("Swarm") && line.contains("\u{2605} ep"), "{line}");
        assert!(app.org_header_line("member:m1", 1, 60).to_string().contains("\u{2605} ep"));
    }

    #[test]
    fn unassigned_sessions_and_old_daemons() {
        let mut app = app_with_team();
        app.workspaces.push(ws("solo"));
        push(&mut app, 1, "ts-9-0", "solo");
        app.sidebar_view = SidebarView::Team;
        let items = app.visual_items();
        let at = items.iter().position(|i| matches!(i, VisualItem::OrgHeader { key, .. } if key == UNASSIGNED_KEY)).unwrap();
        assert_eq!(items[at + 1], VisualItem::Session(1, 0));
        // Headers are not cursor stops.
        app.handle_event(&CrosstermEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)));
        assert!(matches!(app.cursor, Cursor::Session(..)));
        // No live org: the Team view draws the Task tree.
        let mut app = test_app();
        app.sidebar_view = SidebarView::Team;
        assert!(!app.visual_items().iter().any(|i| matches!(i, VisualItem::OrgHeader { .. })));
    }
}
