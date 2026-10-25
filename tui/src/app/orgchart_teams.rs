//! F6 drawn from teams and managers (doc/org-model.md §11), used whenever
//! `org.snapshot` answers with members (app/org_model.rs); otherwise F6
//! keeps the task-tree chart (app/orgchart.rs).
//!
//! - A team box carries its lead on top (the lead's main session and
//!   assistants), its current epics as chips, and its members under it.
//!   Sub-teams nest under their parent team. Standby teams are greyed;
//!   archived ones are not drawn (their members go to their managers).
//! - A member nests under its manager when the manager is a member of the
//!   same team, else under its primary team's box, else under its
//!   manager's box, else under Owner. Whenever the box drawn above a member
//!   is not its manager's, the card says who is (⋯, matrix cases).
//! - A member whose lead box lives in another team shows a pointer line in
//!   the other teams it leads; its sessions are drawn once.
//! - Guests get an empty ⊹ box in the team they visit; vacant members an
//!   empty ∅ box naming their stand-in.
//! - Assistants are the sessions in a member's home workspace (else its main
//!   session's workspace) other than any member's main, until S4 stamps
//!   `org.member_id` on them. They fold under the member as a count with
//!   state glyphs; `a` expands them. Exited ones are hidden.
//! - Sessions no member claims sit in an Unassigned box under Owner, one box
//!   per task or loose workspace.
use super::org_model::{Member, OrgSnapshot, Team};
use super::orgchart::{ChartModel, ChartNode, ChartSession, NodeKind, OrgCard};
use super::*;
use std::collections::{HashMap, HashSet};

/// The Owner-level box for sessions no member claims.
pub(super) const UNASSIGNED_LABEL: &str = "Unassigned";

pub(super) fn team_rank(t: &Team) -> u8 {
    match t.kind.as_str() {
        "division" => 0,
        "team" => 1,
        _ => 2,
    }
}

/// `handle · title`, or the handle alone.
fn member_line(m: &Member) -> String {
    match m.title.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!("{} \u{00b7} {t}", m.handle),
        None => m.handle.clone(),
    }
}

impl App {
    /// The chart from the org snapshot: Owner → teams → members, with the
    /// live sessions placed by member.
    pub(super) fn team_chart_model(&self, snap: &OrgSnapshot, show_bash: bool) -> ChartModel {
        let mut m = ChartModel::default();
        let owner = m.push(None, "owner".into(), NodeKind::Owner, "Owner".into(), None, 0);
        let opened_on = self.orgchart.bash_uid.as_deref();
        let keep = |ts: &TerminalSession| show_bash || ts.session_type != "bash" || opened_on == Some(ts.uid.as_str());
        let muted = Style::default().fg(theme::MUTED);
        let dim = Style::default().fg(theme::DIM);
        let owner_id = snap.owner().map(|o| o.id.clone());

        let people: Vec<&Member> = snap.members.iter().filter(|x| !x.is_owner() && !x.retired()).collect();
        // Every main session is its member's, never someone's assistant.
        let mut claimed: HashSet<String> = people.iter().filter_map(|x| x.main_uid().map(str::to_owned)).collect();

        // Teams, parents first.
        let teams: Vec<&Team> = snap.teams.iter().filter(|t| !t.archived()).collect();
        let shown: HashSet<&str> = teams.iter().map(|t| t.id.as_str()).collect();
        let mut kids: HashMap<Option<&str>, Vec<&Team>> = HashMap::new();
        for t in &teams {
            let parent = t.parent_team_id.as_deref().filter(|p| shown.contains(p) && *p != t.id);
            kids.entry(parent).or_default().push(t);
        }
        for v in kids.values_mut() {
            v.sort_by(|a, b| team_rank(a).cmp(&team_rank(b)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
        }
        let mut team_node: HashMap<&str, usize> = HashMap::new();
        let mut order: Vec<&Team> = Vec::new();
        let mut stack: Vec<(Option<usize>, &Team)> = kids.get(&None).into_iter().flatten().rev().map(|t| (None, *t)).collect();
        while let Some((parent, t)) = stack.pop() {
            if team_node.contains_key(t.id.as_str()) {
                continue;
            }
            let card = OrgCard { team_id: Some(t.id.clone()), greyed: t.standby(), keep: true, ..OrgCard::default() };
            let i = m.push(Some(parent.unwrap_or(owner)), format!("team:{}", t.id), NodeKind::Team, t.name.clone(), None, team_rank(t));
            m.nodes[i].card = Some(card);
            team_node.insert(&t.id, i);
            order.push(t);
            for c in kids.get(&Some(t.id.as_str())).into_iter().flatten().rev() {
                stack.push((Some(i), c));
            }
        }
        // A cycle in parent_team_id leaves teams unreached: under Owner.
        for t in &teams {
            if !team_node.contains_key(t.id.as_str()) {
                let i = m.push(Some(owner), format!("team:{}", t.id), NodeKind::Team, t.name.clone(), None, team_rank(t));
                m.nodes[i].card = Some(OrgCard { team_id: Some(t.id.clone()), greyed: t.standby(), keep: true, ..OrgCard::default() });
                team_node.insert(&t.id, i);
                order.push(t);
            }
        }

        // Where each lead's own box is: the team it leads that is its primary
        // team, else the first it leads.
        let mut lead_home: HashMap<&str, usize> = HashMap::new();
        for t in &order {
            let Some(lead) = t.lead_member_id.as_deref() else { continue };
            if !people.iter().any(|x| x.id == lead) {
                continue;
            }
            let primary = snap.member(lead).and_then(|x| x.primary_team_id.as_deref()) == Some(t.id.as_str());
            if primary || !lead_home.contains_key(lead) {
                lead_home.insert(lead, team_node[t.id.as_str()]);
            }
        }

        // Member boxes, managers first.
        let mut member_node: HashMap<String, usize> = HashMap::new();
        let mut placing: HashSet<String> = HashSet::new();
        for x in &people {
            self.place_member(&mut m, snap, x, owner, &team_node, &lead_home, &mut member_node, &mut placing);
        }

        // Who a box stands for, for the matrix check: a member box its
        // member, a team box its lead, Owner's box Owner.
        let node_member = |m: &ChartModel, i: usize| -> Option<String> {
            let n = &m.nodes[i];
            match n.kind {
                NodeKind::Owner => owner_id.clone(),
                NodeKind::Member => n.card.as_ref()?.member_id.clone(),
                NodeKind::Team => snap.team(n.card.as_ref()?.team_id.as_deref()?)?.lead_member_id.clone(),
                _ => None,
            }
        };

        // Team boxes: the lead line (or a pointer, or the stand-in), the lead's
        // sessions, epic chips.
        for t in &order {
            let i = team_node[t.id.as_str()];
            let mut top = Vec::new();
            match t.lead_member_id.as_deref().and_then(|l| snap.member(l)).filter(|l| !l.retired()) {
                Some(l) if l.is_owner() => top.push((format!("{} Owner", symbols::OWNER), symbols::owner_style())),
                Some(l) if lead_home.get(l.id.as_str()) == Some(&i) => {
                    top.push((member_line(l), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
                    if l.vacant() {
                        top.push((self.vacant_line(snap, l), dim));
                    }
                    let (sessions, folded) = self.member_sessions(l, &mut claimed, &keep);
                    m.nodes[i].sessions = sessions;
                    let card = m.nodes[i].card.as_mut().unwrap();
                    card.member_id = Some(l.id.clone());
                    card.folded = folded;
                }
                Some(l) => {
                    let home = lead_home.get(l.id.as_str()).map(|&h| m.nodes[h].title.clone());
                    let text = match home {
                        Some(h) => format!("lead {} (in {h})", l.handle),
                        None => format!("lead {}", l.handle),
                    };
                    top.push((text, muted));
                }
                None => {
                    // A leadless team shows its parent's lead as stand-in.
                    let mut cur = t.parent_team_id.as_deref().and_then(|p| snap.team(p));
                    let mut seen = HashSet::new();
                    let mut stand_in = None;
                    while let Some(p) = cur.filter(|p| seen.insert(p.id.as_str())) {
                        if let Some(l) = p.lead_member_id.as_deref() {
                            stand_in = Some(snap.handle(l));
                            break;
                        }
                        cur = p.parent_team_id.as_deref().and_then(|q| snap.team(q));
                    }
                    top.push((format!("no lead \u{00b7} stand-in {}", stand_in.unwrap_or_else(|| "Owner".into())), dim));
                }
            }
            if t.standby() {
                top.push(("standby".into(), dim));
            }
            let epics = self.orgchart.boards.epics_of(&t.id);
            let card = m.nodes[i].card.as_mut().unwrap();
            card.top = top;
            if !epics.is_empty() {
                let chips: Vec<String> = epics.iter().map(|(_, g)| format!("{} {}", symbols::EPIC, g["name"].as_str().unwrap_or("epic"))).collect();
                card.bottom.push((chips.join("  "), Style::default().fg(theme::HEADER)));
            }
        }

        // Member boxes: title, vacancy, matrix, sessions.
        for x in &people {
            let Some(&i) = member_node.get(&x.id) else { continue };
            let mut top = Vec::new();
            if let Some(t) = x.title.as_deref().filter(|t| !t.is_empty()) {
                top.push((t.to_owned(), muted));
            }
            if x.vacant() {
                top.push((self.vacant_line(snap, x), dim));
            }
            if x.status == "suspended" {
                top.push(("suspended".into(), dim));
            }
            m.nodes[i].card.as_mut().unwrap().top = top;
            if !x.vacant() {
                let (sessions, folded) = self.member_sessions(x, &mut claimed, &keep);
                m.nodes[i].sessions = sessions;
                m.nodes[i].card.as_mut().unwrap().folded = folded;
            }
        }

        // Matrix: the box drawn above isn't the manager's.
        for i in 0..m.nodes.len() {
            let Some(card) = &m.nodes[i].card else { continue };
            let Some(member) = card.member_id.as_deref().and_then(|id| snap.member(id)) else { continue };
            let Some(manager) = member.manager_member_id.as_deref() else { continue };
            let above = m.nodes[i].parent.and_then(|p| node_member(&m, p));
            if above.as_deref() != Some(manager) {
                let who = snap.handle(manager);
                let card = m.nodes[i].card.as_mut().unwrap();
                card.matrix = Some(who.clone());
                card.top.push((format!("{} reports to {who}", symbols::MATRIX), muted));
            }
        }

        // Guests: an empty box in each team they visit.
        for t in &order {
            for g in t.members.iter().filter(|g| g.role == "guest") {
                let Some(x) = people.iter().find(|x| x.id == g.member_id) else { continue };
                if x.primary_team_id.as_deref() == Some(t.id.as_str()) {
                    continue;
                }
                let home = x.primary_team_id.as_deref().and_then(|p| snap.team(p)).map_or_else(|| "no team".to_owned(), |p| p.name.clone());
                let i = m.push(Some(team_node[t.id.as_str()]), format!("guest:{}:{}", t.id, x.id), NodeKind::Member, format!("{} {}", symbols::GUEST, x.handle), None, 5);
                m.nodes[i].card = Some(OrgCard {
                    member_id: Some(x.id.clone()),
                    team_id: Some(t.id.clone()),
                    top: vec![(format!("guest \u{00b7} from {home}"), muted)],
                    guest: true,
                    keep: true,
                    ..OrgCard::default()
                });
            }
        }

        // Everyone no member claims: Unassigned, by task, else workspace.
        let mut unassigned: Option<usize> = None;
        let tasks: HashMap<&str, &TaskEntry> = self.tasks.iter().filter_map(|t| Some((t.task_id.as_deref()?, t))).collect();
        let mut boxes: HashMap<String, usize> = HashMap::new();
        for (wi, ws) in self.workspaces.iter().enumerate().filter(|(_, w)| !w.is_closed) {
            for (si, ts) in ws.sessions.iter().enumerate() {
                if claimed.contains(&ts.uid) || !keep(ts) {
                    continue;
                }
                let u = *unassigned.get_or_insert_with(|| m.push(Some(owner), "org:unassigned".into(), NodeKind::Org, UNASSIGNED_LABEL.into(), None, 9));
                let (key, kind, title, task, rank) = match ts.task_id.as_deref() {
                    Some(t) => (format!("task:{t}"), NodeKind::Task, tasks.get(t).map_or_else(|| format!("task {}", t.chars().take(8).collect::<String>()), |r| r.name.clone()), Some(t.to_owned()), 1),
                    None => (format!("ws:{}", ws.id), NodeKind::Workspace, ws.name.clone(), None, 2),
                };
                let i = *boxes.entry(key.clone()).or_insert_with(|| m.push(Some(u), key, kind, title, task, rank));
                m.nodes[i].sessions.push(ChartSession { uid: ts.uid.clone(), wi, si, lead: false, mark: None });
            }
        }

        for node in &mut m.nodes {
            node.leader = matches!(node.kind, NodeKind::Team | NodeKind::Member) && !node.children.is_empty();
            if matches!(node.kind, NodeKind::Task | NodeKind::Workspace) {
                node.sessions.sort_by_key(|s| spawn_order(&s.uid));
            }
        }
        let mut m = m.prune();
        m.sort_children();
        m
    }

    /// Push `x`'s box (its manager's first) unless it lives in a team box.
    #[allow(clippy::too_many_arguments)]
    fn place_member(
        &self,
        m: &mut ChartModel,
        snap: &OrgSnapshot,
        x: &Member,
        owner: usize,
        team_node: &HashMap<&str, usize>,
        lead_home: &HashMap<&str, usize>,
        member_node: &mut HashMap<String, usize>,
        placing: &mut HashSet<String>,
    ) -> Option<usize> {
        if let Some(&i) = lead_home.get(x.id.as_str()) {
            return Some(i);
        }
        if let Some(&i) = member_node.get(&x.id) {
            return Some(i);
        }
        if x.is_owner() || x.retired() || !placing.insert(x.id.clone()) {
            // Owner, a retired member, or a manager cycle: Owner's box.
            return None;
        }
        let team = x.primary_team_id.as_deref().and_then(|t| team_node.get(t).copied());
        let manager = x.manager_member_id.as_deref().and_then(|id| snap.member(id)).filter(|mg| !mg.is_owner() && !mg.retired());
        // The manager's box, when the manager shares the primary team (or
        // there is no team to draw under).
        let same_team = |mg: &Member| team.is_none() || mg.primary_team_id == x.primary_team_id;
        let via_manager = manager.filter(|mg| same_team(mg)).and_then(|mg| self.place_member(m, snap, mg, owner, team_node, lead_home, member_node, placing));
        // A manager whose own box is a team box (a lead) puts the member
        // under its primary team's box, not the team the manager leads.
        let parent = match (via_manager, team) {
            (Some(p), Some(t)) if m.nodes[p].kind == NodeKind::Team => t,
            (Some(p), _) => p,
            (None, Some(t)) => t,
            (None, None) => owner,
        };
        let deputy = x.primary_team_id.as_deref().and_then(|t| snap.team(t)).and_then(|t| t.role_of(&x.id)) == Some("deputy");
        let mut title = if x.vacant() { format!("{} {}", symbols::VACANT, x.handle) } else { x.handle.clone() };
        if deputy {
            title.push_str(" (deputy)");
        }
        let i = m.push(Some(parent), format!("member:{}", x.id), NodeKind::Member, title, None, 3);
        m.nodes[i].card = Some(OrgCard {
            member_id: Some(x.id.clone()),
            team_id: x.primary_team_id.clone(),
            greyed: x.status == "suspended",
            keep: true,
            ..OrgCard::default()
        });
        member_node.insert(x.id.clone(), i);
        Some(i)
    }

    /// `∅ vacant · stand-in X`: the snapshot's stand-in, else the manager.
    fn vacant_line(&self, snap: &OrgSnapshot, x: &Member) -> String {
        let who = x.stand_in.as_deref().or(x.manager_member_id.as_deref()).map_or_else(|| "Owner".to_owned(), |id| snap.handle(id));
        format!("{} vacant \u{00b7} stand-in {who}", symbols::VACANT)
    }

    /// A member's main session (as lead) and its assistants: shown when
    /// expanded (`a`), else folded into the count line.
    fn member_sessions(&self, x: &Member, claimed: &mut HashSet<String>, keep: &dyn Fn(&TerminalSession) -> bool) -> (Vec<ChartSession>, Vec<(usize, usize)>) {
        let mut sessions = Vec::new();
        let main = x.main_uid().and_then(|uid| self.find_session_by_uid(uid));
        if let Some((wi, si)) = main {
            let ts = &self.workspaces[wi].sessions[si];
            if keep(ts) {
                sessions.push(ChartSession { uid: ts.uid.clone(), wi, si, lead: !ts.session.exited, mark: None });
            }
        }
        let home = x.home_workspace.as_ref().and_then(|h| {
            self.workspaces.iter().position(|w| {
                !w.is_closed
                    && (h.workspace_id.as_deref() == Some(w.id.as_str())
                        || h.path.as_deref().is_some_and(|p| w.worktree_path.as_deref() == Some(Path::new(p))))
            })
        });
        let mut folded = Vec::new();
        if let Some(wi) = home.or(main.map(|(wi, _)| wi)) {
            for (si, ts) in self.workspaces[wi].sessions.iter().enumerate() {
                if ts.session.exited || !keep(ts) || claimed.contains(&ts.uid) {
                    continue;
                }
                claimed.insert(ts.uid.clone());
                folded.push((wi, si));
            }
        }
        folded.sort_by_key(|&(wi, si)| spawn_order(&self.workspaces[wi].sessions[si].uid));
        if self.orgchart.expanded.contains(&x.id) {
            for (wi, si) in folded.drain(..) {
                let uid = self.workspaces[wi].sessions[si].uid.clone();
                sessions.push(ChartSession { uid, wi, si, lead: false, mark: None });
            }
        }
        (sessions, folded)
    }

    /// The folded-assistants line: `+3 assistants ●●◐`.
    pub(super) fn assistants_line(&self, folded: &[(usize, usize)], now: Instant) -> Vec<(String, Style)> {
        let mut out = vec![(format!("+{} assistant{} ", folded.len(), if folded.len() == 1 { "" } else { "s" }), Style::default().fg(theme::MUTED))];
        for &(wi, si) in folded {
            if let Some(ts) = self.workspaces.get(wi).and_then(|w| w.sessions.get(si)) {
                let (g, st) = self.chart_glyph(ts, now);
                out.push((g.to_owned(), st));
            }
        }
        out
    }

    /// The info pane's org lines for a team or member box.
    pub(super) fn org_card_info(&self, node: &ChartNode, card: &OrgCard, width: usize) -> Vec<Line<'static>> {
        let Some(snap) = self.org_live() else { return Vec::new() };
        let label = |s: &str| Span::styled(format!("{s:<9}"), Style::default().fg(theme::DIM));
        let text = |s: String| Span::styled(crate::planning::truncate_with_ellipsis(&s, width.saturating_sub(9)), Style::default().fg(theme::TEXT));
        let mut lines = Vec::new();
        if let Some(t) = card.team_id.as_deref().and_then(|t| snap.team(t)).filter(|_| node.kind == NodeKind::Team) {
            lines.push(Line::styled(t.name.clone(), Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD)));
            lines.push(Line::from(vec![label("kind"), text(format!("{} \u{00b7} {}", t.kind, if t.status.is_empty() { "active" } else { &t.status }))]));
            let lead = t.lead_member_id.as_deref().map_or_else(|| "none".to_owned(), |l| snap.handle(l));
            lines.push(Line::from(vec![label("lead"), text(lead)]));
            if let Some(p) = t.parent_team_id.as_deref() {
                lines.push(Line::from(vec![label("parent"), text(snap.team(p).map_or_else(|| p.to_owned(), |p| p.name.clone()))]));
            }
            let count = |role: &str| t.members.iter().filter(|x| x.role == role).count();
            lines.push(Line::from(vec![label("members"), text(format!("{} primary \u{00b7} {} guest \u{00b7} {} deputy", count("primary"), count("guest"), count("deputy")))]));
            if let Some(b) = &t.board {
                lines.push(Line::from(vec![label("board"), text(b.clone())]));
            }
            if let Some(c) = &t.channel {
                lines.push(Line::from(vec![label("channel"), text(c.clone())]));
            }
            for (board, g) in self.orgchart.boards.epics_of(&t.id) {
                lines.push(Line::from(vec![label("epic"), Span::styled(format!("{} ", symbols::EPIC), Style::default().fg(theme::HEADER)), text(format!("{} ({board})", g["name"].as_str().unwrap_or("")))]));
            }
            lines.push(Line::from(""));
        }
        let Some(x) = card.member_id.as_deref().and_then(|id| snap.member(id)) else { return lines };
        lines.push(Line::styled(member_line(x), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
        lines.push(Line::from(vec![label("member"), text(format!("{} \u{00b7} {}", x.kind, if x.status.is_empty() { "active" } else { &x.status }))]));
        let team = x.primary_team_id.as_deref().map_or_else(|| "none".to_owned(), |t| snap.team(t).map_or_else(|| t.to_owned(), |t| t.name.clone()));
        lines.push(Line::from(vec![label("team"), text(team)]));
        if let Some(mg) = x.manager_member_id.as_deref() {
            let matrix = if card.matrix.is_some() { "  (matrix)" } else { "" };
            lines.push(Line::from(vec![label("manager"), text(format!("{}{matrix}", snap.handle(mg)))]));
        }
        let guest_of: Vec<String> = snap.teams.iter().filter(|t| t.role_of(&x.id) == Some("guest")).map(|t| t.name.clone()).collect();
        if !guest_of.is_empty() {
            lines.push(Line::from(vec![label("guest in"), text(guest_of.join(", "))]));
        }
        if x.vacant() {
            lines.push(Line::from(vec![label("vacant"), text(self.vacant_line(snap, x))]));
        }
        if let Some(md) = x.mandate.as_deref().filter(|s| !s.is_empty()) {
            lines.push(Line::from(vec![label("mandate"), text(md.to_owned())]));
        }
        lines.push(Line::from(vec![label("focus"), text(x.focus_text().unwrap_or("—").to_owned())]));
        let uids: Vec<&str> = node.sessions.iter().map(|s| s.uid.as_str()).chain(card.folded.iter().filter_map(|&(wi, si)| self.workspaces.get(wi)?.sessions.get(si).map(|t| t.uid.as_str()))).collect();
        let held = self.orgchart.boards.held_by_member(&x.id, &uids);
        lines.push(Line::styled(format!("Current items ({})", held.len()), Style::default().fg(theme::HEADER)));
        let unix = board::now_unix();
        for h in held {
            let (mark, mark_style) = symbols::board_status(h.item["status"].as_str().unwrap_or(""));
            let (badge, badge_style) = board::status_badge(&h.item, unix);
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), mark_style),
                Span::styled(format!("{} #{} ", h.board, h.item["n"]), Style::default().fg(theme::MUTED)),
                Span::styled(format!("{badge} "), badge_style),
                Span::styled(crate::planning::truncate_with_ellipsis(h.item["title"].as_str().unwrap_or(""), width.saturating_sub(20)), Style::default().fg(theme::TEXT)),
            ]));
        }
        lines.push(Line::from(""));
        lines
    }
}

/// Spawn order for a `ts-<nanos hex>-<counter hex>` uid (unparseable last).
fn spawn_order(uid: &str) -> (u8, u128, u64) {
    let parsed = uid.strip_prefix("ts-").and_then(|r| {
        let (n, c) = r.split_once('-')?;
        Some((u128::from_str_radix(n, 16).ok()?, u64::from_str_radix(c, 16).ok()?))
    });
    parsed.map_or((1, 0, 0), |(n, c)| (0, n, c))
}

#[cfg(test)]
mod tests {
    use super::super::org_model::fixtures::{member, owner, snapshot, team};
    use super::super::org_model::{RpcFail, Support};
    use super::*;
    use crate::app::lifecycle::revive_session_tests::dummy_ts;
    use cm_daemon::host_id::HostId;
    use crossterm::event::KeyEvent;
    use serde_json::json;

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

    fn session(app: &mut App, wi: usize, n: u32, label: &str, task: Option<&str>) {
        let mut ts = dummy_ts(&format!("ts-{n:x}-0"), false, HostId::local());
        ts.label = label.into();
        ts.session_type = "claude".into();
        ts.task_id = task.map(str::to_owned);
        app.workspaces[wi].sessions.push(ts);
    }

    fn key(c: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE))
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    /// Swarm (lead ep, with an assistant) ⊃ Items (lead items); orgchart and
    /// a matrix member on Swarm; a vacant member and orgchart as a guest on
    /// Items; a leadless standby team; one session no member claims.
    fn team_app() -> App {
        let mut app = test_app();
        app.workspaces = vec![ws("w-ep"), ws("w-oc"), ws("w-items"), ws("w-loose")];
        session(&mut app, 0, 0x10, "ep-main", None);
        session(&mut app, 0, 0x11, "ep-helper", None);
        session(&mut app, 1, 0x20, "orgchart", None);
        session(&mut app, 2, 0x30, "items-lead", None);
        session(&mut app, 3, 0x90, "stray", Some("loose"));
        app.tasks = vec![TaskEntry {
            task_id: Some("loose".into()),
            name: "Loose work".into(),
            api_status: TaskStatus::Running,
            repo_url: None,
            prompt: None,
            wip_branch: None,
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            is_continuous: false,
            workspace_id: None,
            project: None,
            parent_task_id: None,
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        }];
        let mut matrix = member("m4", "matrix", Some("m3"), Some("t1"), None);
        matrix["status"] = json!("active");
        let mut vacant = member("m5", "hole", Some("m3"), Some("t2"), None);
        vacant["status"] = json!("vacant");
        let mut oc = member("m2", "orgchart", Some("m1"), Some("t1"), Some("ts-20-0"));
        oc["focus_pinned"] = json!("org model TUI");
        let mut old = team("t3", "Old", None, None, &[]);
        old["status"] = json!("standby");
        app.org.accept(snapshot(
            3,
            vec![owner(), member("m1", "ep", Some("mem-owner"), Some("t1"), Some("ts-10-0")), oc, member("m3", "items", Some("m1"), Some("t2"), Some("ts-30-0")), matrix, vacant],
            vec![
                team("t1", "Swarm", None, Some("m1"), &[("m1", "primary"), ("m2", "primary"), ("m4", "primary")]),
                team("t2", "Items", Some("t1"), Some("m3"), &[("m3", "primary"), ("m5", "primary"), ("m2", "guest")]),
                old,
            ],
        ));
        app
    }

    fn node<'a>(m: &'a ChartModel, key: &str) -> &'a ChartNode {
        &m.nodes[m.find(key).unwrap_or_else(|| panic!("no {key}"))]
    }

    #[test]
    fn teams_and_managers_shape_the_chart() {
        let app = team_app();
        let m = app.org_model(false);
        let keys: Vec<&str> = m.preorder(&HashSet::new()).into_iter().map(|i| m.nodes[i].key.as_str()).collect();
        // Teams sort by title (Old, Swarm); a team's leaf members stack first.
        assert_eq!(keys, ["owner", "team:t3", "team:t1", "member:m4", "member:m2", "team:t2", "member:m5", "guest:t2:m2", "org:unassigned", "task:loose"]);
        // The lead sits on top of its team box, with its assistant folded.
        let swarm = node(&m, "team:t1");
        assert_eq!(swarm.sessions.iter().map(|s| s.uid.as_str()).collect::<Vec<_>>(), ["ts-10-0"]);
        assert!(swarm.sessions[0].lead);
        let card = swarm.card.as_ref().unwrap();
        assert_eq!(card.top[0].0, "ep \u{00b7} ep title");
        assert_eq!(card.folded.len(), 1);
        // A manager in another team: a dotted marker.
        let matrix = node(&m, "member:m4").card.clone().unwrap();
        assert_eq!(matrix.matrix.as_deref(), Some("items"));
        assert!(matrix.top.iter().any(|(t, _)| t == "\u{22ef} reports to items"));
        // orgchart's manager is Swarm's lead: no marker.
        assert!(node(&m, "member:m2").card.as_ref().unwrap().matrix.is_none());
        assert_eq!(node(&m, "member:m2").sessions[0].uid, "ts-20-0");
        // Vacant: an empty box naming the stand-in (the manager here).
        let hole = node(&m, "member:m5");
        assert!(hole.sessions.is_empty() && hole.title.starts_with(symbols::VACANT));
        assert!(hole.card.as_ref().unwrap().top.iter().any(|(t, _)| t.contains("stand-in items")));
        // A guest box in the visited team.
        assert!(node(&m, "guest:t2:m2").card.as_ref().unwrap().guest);
        // Standby, leadless: greyed, with Owner standing in.
        let old = node(&m, "team:t3").card.clone().unwrap();
        assert!(old.greyed);
        assert!(old.top.iter().any(|(t, _)| t == "no lead \u{00b7} stand-in Owner"));
        // Nobody's session: Unassigned, by task.
        assert_eq!(node(&m, "task:loose").sessions[0].uid, "ts-90-0");
        assert_eq!(node(&m, "org:unassigned").title, UNASSIGNED_LABEL);
    }

    #[test]
    fn epic_chips_come_from_board_groups() {
        let mut app = team_app();
        app.orgchart.boards.groups.insert(
            "swarm".into(),
            vec![
                json!({"id": "g1", "name": "S3 epics", "kind": "epic", "epic_status": "active", "team_id": "t1", "closed_at": null}),
                json!({"id": "g2", "name": "Done one", "kind": "epic", "epic_status": "done", "team_id": "t1"}),
                json!({"id": "g3", "name": "Heading", "kind": "heading", "team_id": "t1"}),
            ],
        );
        let m = app.org_model(false);
        let bottom = &node(&m, "team:t1").card.as_ref().unwrap().bottom;
        assert_eq!(bottom.len(), 1);
        assert_eq!(bottom[0].0, format!("{} S3 epics", symbols::EPIC));
    }

    #[test]
    fn cards_render_lead_line_assistants_markers_and_greyed_teams() {
        let mut app = team_app();
        app.handle_event(&key(KeyCode::F(6)));
        let s = screen(&mut app, 220, 50);
        for want in ["Swarm", "ep \u{00b7} ep title", "+1 assistant", "\u{22ef} reports to items", "\u{2205} vacant", "\u{22b9} orgchart", "guest \u{00b7} from Swarm", "standby", "Unassigned", "a assistants"] {
            assert!(s.contains(want), "missing {want:?}:\n{s}");
        }
    }

    #[test]
    fn a_expands_and_folds_assistants() {
        let mut app = team_app();
        app.cursor = Cursor::Session(0, 0);
        app.handle_event(&key(KeyCode::F(6)));
        let m = app.org_model(false);
        assert_eq!(m.nodes[app.orgchart_focus(&m)].key, "team:t1");
        app.handle_event(&key(KeyCode::Char('a')));
        let m = app.org_model(false);
        let swarm = node(&m, "team:t1");
        assert_eq!(swarm.sessions.len(), 2);
        assert!(swarm.card.as_ref().unwrap().folded.is_empty());
        app.handle_event(&key(KeyCode::Char('a')));
        assert_eq!(node(&app.org_model(false), "team:t1").sessions.len(), 1);
    }

    #[test]
    fn opening_on_a_folded_assistant_expands_its_member() {
        let mut app = team_app();
        app.cursor = Cursor::Session(0, 1); // ep-helper
        app.handle_event(&key(KeyCode::F(6)));
        let m = app.org_model(false);
        assert_eq!(app.orgchart_focused_session(&m).unwrap().uid, "ts-11-0");
    }

    #[test]
    fn info_pane_shows_member_facts_and_items() {
        let mut app = team_app();
        app.cursor = Cursor::Session(1, 0); // orgchart
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.boards.items.insert("swarm".into(), vec![json!({"n": 15, "status": "active", "title": "Org model TUI", "holders": [{"session_uid": "ts-20-0"}]})]);
        let m = app.org_model(false);
        let text: Vec<String> = app.orgchart_info_lines(&m, app.orgchart_focus(&m), 70).iter().map(|l| l.to_string()).collect();
        let joined = text.join("\n");
        for want in ["orgchart \u{00b7} orgchart title", "team     Swarm", "manager  ep", "guest in Items", "mandate  orgchart mandate", "focus    org model TUI", "Current items (1)", "swarm #15", "engine"] {
            assert!(joined.contains(want), "missing {want:?}:\n{joined}");
        }
        // The session's own items aren't listed a second time.
        assert!(!joined.contains("Board items"), "{joined}");
    }

    #[test]
    fn old_daemons_keep_the_task_chart() {
        let mut app = team_app();
        app.org.fail(RpcFail { unsupported: true, message: "unknown method".into() });
        assert_eq!(app.org.support, Support::Off);
        let m = app.org_model(false);
        assert!(m.find("team:t1").is_none());
        assert!(m.find("task:loose").is_some(), "today's chart");
        let s = screen(&mut { app.handle_event(&key(KeyCode::F(6))); app }, 200, 40);
        assert!(!s.contains("R roster"), "{s}");
    }

    #[test]
    fn org_polls_only_while_a_screen_needs_it() {
        let mut app = team_app();
        assert!(!app.org_wanted());
        app.handle_event(&key(KeyCode::F(6)));
        assert!(app.org_wanted());
        app.handle_event(&key(KeyCode::Esc));
        assert!(!app.org_wanted());
    }

    #[test]
    fn manager_cycles_and_missing_teams_terminate_under_owner() {
        let mut app = test_app();
        app.org.accept(snapshot(1, vec![owner(), member("a", "a", Some("b"), None, None), member("b", "b", Some("a"), None, None), member("c", "c", Some("a"), Some("gone"), None)], vec![]));
        let m = app.org_model(false);
        for k in ["member:a", "member:b", "member:c"] {
            assert!(m.find(k).is_some(), "{k}");
        }
    }
}
