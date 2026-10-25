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
//! - Every other session goes under the member responsible for it
//!   ([`App::session_attachments`]), walking up until it reaches a member's
//!   main session or a team's task: its spawner (`managed_by_uid`), then its
//!   task's `parent_task_id` chain (a task a member's main session is bound
//!   to, or a team's coordinator task), then its workspace (the member's home
//!   workspace, else its main session's). Workspace ones are assistants, the
//!   rest workers. They fold under the box as a count with state glyphs; `a`
//!   expands them. Exited ones are hidden.
//!   Placement depends only on that path, never on how long a session lives.
//! - Sessions Owner started (no spawner) that reach no member sit in an
//!   Owner's sessions box under Owner, one box per task or loose workspace;
//!   only the rare rest (spawned, but no path) sit in Unassigned.
//! - A box with more than [`STACK_AT`] boxes under it that have none of their
//!   own stacks those in one column (as leaders do), so a wide fan-out never
//!   needs horizontal scrolling.
use super::org_model::{Member, OrgSnapshot, Team};
use super::orgchart::{plural, ChartModel, ChartNode, ChartSession, NodeKind, OrgCard};
use super::*;
use std::collections::{HashMap, HashSet};

/// The Owner-level box for sessions with no path to a member that some
/// session spawned (rare: the spawner leads nowhere either).
pub(super) const UNASSIGNED_LABEL: &str = "Unassigned";
pub(super) const UNASSIGNED_KEY: &str = "org:unassigned";
/// The Owner-level box for sessions Owner started directly (no spawner)
/// that reach no member through their task or workspace.
pub(super) const OWNER_SESSIONS_LABEL: &str = "Owner's sessions";
pub(super) const OWNER_SESSIONS_KEY: &str = "org:owner-sessions";

/// More leaf boxes than this under one box stack in a column; up to this
/// many sit side by side.
pub(super) const STACK_AT: usize = 3;

/// Whose box a non-member session goes under.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Attach {
    Member(String),
    /// A team's coordinator task (its lead's box, or a leadless team's).
    Team(String),
}

/// How a session was attached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Via {
    /// The member's home workspace (or its main session's): an assistant.
    Workspace,
    /// Spawned (`managed_by_uid`) by this session, or by one under it.
    Spawned(String),
    /// Through this task of the session's `parent_task_id` chain.
    Task(String),
    /// The daemon's member stamp (`org.member_id`, S4).
    Stamp,
}

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
        // Every main session is its member's; every other session goes to
        // the member (or team) its spawner, task chain or workspace leads to.
        let mains: HashSet<String> = people.iter().filter_map(|x| x.main_uid().map(str::to_owned)).collect();
        let attached = self.session_attachments(snap);
        let mut under: HashMap<Attach, Vec<(usize, usize)>> = HashMap::new();
        for (wi, ws) in self.workspaces.iter().enumerate().filter(|(_, w)| !w.is_closed) {
            for (si, ts) in ws.sessions.iter().enumerate() {
                if let Some((to, _)) = attached.get(&ts.uid) {
                    if !ts.session.exited && keep(ts) {
                        under.entry(to.clone()).or_default().push((wi, si));
                    }
                }
            }
        }

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
                    let card = m.nodes[i].card.as_mut().unwrap();
                    card.member_id = Some(l.id.clone());
                    // A vacant lead is drawn like a vacant member: empty, with its stand-in.
                    if l.vacant() {
                        top.push((self.vacant_line(snap, l), dim));
                    } else {
                        m.nodes[i].sessions = self.member_main(l, &keep);
                    }
                    let mut extra = under.remove(&Attach::Member(l.id.clone())).unwrap_or_default();
                    extra.extend(under.remove(&Attach::Team(t.id.clone())).unwrap_or_default());
                    self.fold_attached(&mut m.nodes[i], extra, &attached);
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
            // A team's own workers when its box isn't its lead's.
            if let Some(extra) = under.remove(&Attach::Team(t.id.clone())) {
                self.fold_attached(&mut m.nodes[i], extra, &attached);
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
                m.nodes[i].sessions = self.member_main(x, &keep);
            }
            let extra = under.remove(&Attach::Member(x.id.clone())).unwrap_or_default();
            self.fold_attached(&mut m.nodes[i], extra, &attached);
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

        // Attached to a member with no box of its own (a lead drawn under
        // another team): nobody; they would be lost, so Unassigned takes them.
        let lost: HashSet<String> = under.values().flatten().filter_map(|&(wi, si)| self.workspaces.get(wi)?.sessions.get(si).map(|t| t.uid.clone())).collect();

        // Everyone no member claims, by task, else workspace: what Owner
        // started (no spawner) under Owner's sessions, the rare rest (a
        // spawner that leads nowhere) under Unassigned.
        let mut groups: HashMap<&str, usize> = HashMap::new();
        let tasks: HashMap<&str, &TaskEntry> = self.tasks.iter().filter_map(|t| Some((t.task_id.as_deref()?, t))).collect();
        let mut boxes: HashMap<String, usize> = HashMap::new();
        for (wi, ws) in self.workspaces.iter().enumerate().filter(|(_, w)| !w.is_closed) {
            for (si, ts) in ws.sessions.iter().enumerate() {
                let claimed = mains.contains(&ts.uid) || (attached.contains_key(&ts.uid) && !lost.contains(&ts.uid));
                if claimed || !keep(ts) {
                    continue;
                }
                let (group, label, prefix, rank) = if ts.managed_by_uid.is_none() && !lost.contains(&ts.uid) {
                    (OWNER_SESSIONS_KEY, OWNER_SESSIONS_LABEL, "", 8)
                } else {
                    (UNASSIGNED_KEY, UNASSIGNED_LABEL, "u:", 9)
                };
                let u = *groups.entry(group).or_insert_with(|| m.push(Some(owner), group.into(), NodeKind::Org, label.into(), None, rank));
                let (key, kind, title, task, rank) = match ts.task_id.as_deref() {
                    Some(t) => (format!("{prefix}task:{t}"), NodeKind::Task, tasks.get(t).map_or_else(|| format!("task {}", t.chars().take(8).collect::<String>()), |r| r.name.clone()), Some(t.to_owned()), 1),
                    None => (format!("{prefix}ws:{}", ws.id), NodeKind::Workspace, ws.name.clone(), None, 2),
                };
                let i = *boxes.entry(key.clone()).or_insert_with(|| m.push(Some(u), key, kind, title, task, rank));
                m.nodes[i].sessions.push(ChartSession { uid: ts.uid.clone(), wi, si, lead: false, mark: None });
            }
        }

        for node in &mut m.nodes {
            if matches!(node.kind, NodeKind::Task | NodeKind::Workspace) {
                node.sessions.sort_by_key(|s| spawn_order(&s.uid));
            }
        }
        let mut m = m.prune();
        // Teams and members with reports stack their leaf reports, as do
        // other boxes (Owner, Unassigned) with a wide fan-out of leaves.
        for i in 0..m.nodes.len() {
            let leaves = m.nodes[i].children.iter().filter(|&&c| m.nodes[c].children.is_empty()).count();
            let org_box = matches!(m.nodes[i].kind, NodeKind::Team | NodeKind::Member) && !m.nodes[i].children.is_empty();
            m.nodes[i].leader = org_box || leaves > STACK_AT;
        }
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

    /// A member's main session, as its box's lead line.
    fn member_main(&self, x: &Member, keep: &dyn Fn(&TerminalSession) -> bool) -> Vec<ChartSession> {
        let Some((wi, si)) = x.main_uid().and_then(|uid| self.find_session_by_uid(uid)) else { return Vec::new() };
        let ts = &self.workspaces[wi].sessions[si];
        if !keep(ts) {
            return Vec::new();
        }
        vec![ChartSession { uid: ts.uid.clone(), wi, si, lead: !ts.session.exited, mark: None }]
    }

    /// Put attached sessions under a box: one line each once expanded
    /// (`a`), else folded into the count line. Records how each worker got
    /// there for the info pane, and names the task-tree ones on the card.
    fn fold_attached(&self, node: &mut ChartNode, mut extra: Vec<(usize, usize)>, attached: &HashMap<String, (Attach, Via)>) {
        if extra.is_empty() {
            return;
        }
        extra.sort_by_key(|&(wi, si)| spawn_order(&self.workspaces[wi].sessions[si].uid));
        extra.dedup();
        // Shells (shown with `b`) sit in their member's box as session
        // lines of their own, never in the assistants count.
        let (shells, extra): (Vec<_>, Vec<_>) = extra.into_iter().partition(|&(wi, si)| self.workspaces[wi].sessions[si].session_type == "bash");
        for (wi, si) in shells {
            let uid = self.workspaces[wi].sessions[si].uid.clone();
            node.sessions.push(ChartSession { uid, wi, si, lead: false, mark: None });
        }
        if extra.is_empty() {
            return;
        }
        let Some(card) = node.card.as_mut() else { return };
        for &(wi, si) in &extra {
            let uid = &self.workspaces[wi].sessions[si].uid;
            match attached.get(uid).map(|(_, v)| v) {
                Some(Via::Spawned(by)) => {
                    let who = self.find_session_by_uid(by).map_or_else(|| by.clone(), |(w, s)| self.workspaces[w].sessions[s].label.clone());
                    card.via.insert(uid.clone(), format!("spawned by {who}"));
                }
                Some(Via::Stamp) => {
                    card.via.insert(uid.clone(), "member stamp".into());
                }
                Some(Via::Task(t)) => {
                    let name = self.tasks.iter().find(|r| r.task_id.as_deref() == Some(t.as_str())).map_or_else(|| t.chars().take(8).collect(), |r| r.name.clone());
                    card.via.insert(uid.clone(), format!("task {name}"));
                    card.via_task += 1;
                }
                _ => {}
            }
        }
        let open = card.expand_key().is_some_and(|k| self.orgchart.expanded.contains(&k));
        if open {
            for (wi, si) in extra {
                let uid = self.workspaces[wi].sessions[si].uid.clone();
                node.sessions.push(ChartSession { uid, wi, si, lead: false, mark: None });
            }
            if card.via_task > 0 {
                card.bottom.push((format!("{} via the task tree", plural(card.via_task, "assistant")), Style::default().fg(theme::DIM)));
            }
        } else {
            card.folded.extend(extra);
        }
    }

    /// Who each non-member session belongs to, and how: walk up from the
    /// session until a member's main session or a team's task is reached —
    /// its spawner (`managed_by_uid`, recursively), then its task's
    /// `parent_task_id` chain (a task some member's main session is bound
    /// to, or a team's coordinator task), then its workspace (a member's
    /// home workspace, else the one its main session is in). Main sessions
    /// and sessions with no path are absent. Also the roster's filter.
    pub(super) fn session_attachments(&self, snap: &OrgSnapshot) -> HashMap<String, (Attach, Via)> {
        let people: Vec<&Member> = snap.members.iter().filter(|x| !x.is_owner() && !x.retired()).collect();
        let mut by_uid: HashMap<&str, (usize, &TerminalSession)> = HashMap::new();
        for (wi, ws) in self.workspaces.iter().enumerate().filter(|(_, w)| !w.is_closed) {
            for ts in &ws.sessions {
                by_uid.insert(ts.uid.as_str(), (wi, ts));
            }
        }
        let main_of: HashMap<&str, &str> = people.iter().filter_map(|x| Some((x.main_uid()?, x.id.as_str()))).collect();
        // Tasks the mains are bound to, and the teams' own tasks.
        let mut task_to: HashMap<&str, Attach> = HashMap::new();
        for t in snap.teams.iter().filter(|t| !t.archived()) {
            if let Some(task) = t.coordinator_task_id.as_deref() {
                task_to.insert(task, Attach::Team(t.id.clone()));
            }
        }
        for (uid, member) in &main_of {
            if let Some(task) = by_uid.get(uid).and_then(|(_, ts)| ts.task_id.as_deref()) {
                task_to.insert(task, Attach::Member((*member).to_owned()));
            }
        }
        let parent: HashMap<&str, &str> = self.tasks.iter().filter_map(|t| Some((t.task_id.as_deref()?, t.parent_task_id.as_deref()?))).collect();
        // Workspaces: a member's home workspace, then its main's.
        let mut ws_to: HashMap<usize, &str> = HashMap::new();
        for x in &people {
            let home = x.home_workspace.as_ref().and_then(|h| {
                self.workspaces.iter().position(|w| {
                    !w.is_closed
                        && (h.workspace_id.as_deref() == Some(w.id.as_str())
                            || h.path.as_deref().is_some_and(|p| w.worktree_path.as_deref() == Some(Path::new(p))))
                })
            });
            if let Some(wi) = home {
                ws_to.insert(wi, x.id.as_str());
            }
        }
        for x in &people {
            if let Some(&(wi, _)) = x.main_uid().and_then(|u| by_uid.get(u)) {
                ws_to.entry(wi).or_insert(x.id.as_str());
            }
        }
        let by_task = |start: &str| -> Option<(Attach, Via)> {
            let mut cur = Some(start);
            let mut seen = HashSet::new();
            while let Some(t) = cur.filter(|t| seen.insert(*t) && seen.len() <= 64) {
                if let Some(to) = task_to.get(t) {
                    return Some((to.clone(), Via::Task(t.to_owned())));
                }
                cur = parent.get(t).copied();
            }
            None
        };
        let members: HashSet<&str> = people.iter().map(|x| x.id.as_str()).collect();
        let own = |wi: usize, ts: &TerminalSession| -> Option<(Attach, Via)> {
            // The daemon's member stamp (S4, `org.member_id`) when present.
            if let Some(m) = ts.org.as_ref().and_then(|o| o.get("member_id")).and_then(|v| v.as_str()).filter(|m| members.contains(m)) {
                return Some((Attach::Member(m.to_owned()), Via::Stamp));
            }
            None.or_else(|| ts.task_id.as_deref().and_then(by_task)).or_else(|| ws_to.get(&wi).map(|m| (Attach::Member((*m).to_owned()), Via::Workspace)))
        };
        let mut out: HashMap<String, (Attach, Via)> = HashMap::new();
        let mut uids: Vec<&str> = by_uid.keys().copied().collect();
        uids.sort_unstable();
        for uid in uids {
            if main_of.contains_key(uid) || out.contains_key(uid) {
                continue;
            }
            // The spawner chain up from the session: [uid, its spawner, …],
            // ending at a member's main (`top`), a session already placed,
            // or the first session whose spawner is unknown.
            let mut chain = vec![uid];
            let mut top: Option<(Attach, Via)> = None;
            loop {
                let cur = *chain.last().unwrap();
                let Some(sp) = by_uid[cur].1.managed_by_uid.as_deref().filter(|sp| !chain.contains(sp)) else { break };
                if let Some(member) = main_of.get(sp) {
                    top = Some((Attach::Member((*member).to_owned()), Via::Spawned(sp.to_owned())));
                    break;
                }
                if let Some((to, _)) = out.get(sp) {
                    top = Some((to.clone(), Via::Spawned(sp.to_owned())));
                    break;
                }
                if !by_uid.contains_key(sp) || chain.len() >= 32 {
                    break;
                }
                chain.push(sp);
            }
            // The highest session in the chain that resolves on its own (its
            // stamp, task or workspace) places itself and everyone below it;
            // a member's main above them all wins.
            let mut from = chain.len();
            if top.is_none() {
                for (k, &c) in chain.iter().enumerate().rev() {
                    let (wi, ts) = by_uid[c];
                    if let Some(found) = own(wi, ts) {
                        top = Some(found);
                        from = k;
                        break;
                    }
                }
            }
            let Some((to, via)) = top else { continue };
            if from < chain.len() {
                out.insert(chain[from].to_owned(), (to.clone(), via));
            } else {
                out.insert(chain[chain.len() - 1].to_owned(), (to.clone(), via));
                from = chain.len() - 1;
            }
            for k in (0..from).rev() {
                out.insert(chain[k].to_owned(), (to.clone(), Via::Spawned(chain[k + 1].to_owned())));
            }
        }
        out
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
pub(super) fn spawn_order(uid: &str) -> (u8, u128, u64) {
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
    use super::super::orgchart::{layout, Zoom};
    use super::super::roster::{Row, OTHER_GROUP};
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
        assert_eq!(keys, ["owner", "team:t3", "team:t1", "member:m4", "member:m2", "team:t2", "member:m5", "guest:t2:m2", "org:owner-sessions", "task:loose"]);
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
        // A session Owner started that reaches no member: Owner's sessions, by task.
        assert_eq!(node(&m, "task:loose").sessions[0].uid, "ts-90-0");
        assert_eq!(node(&m, OWNER_SESSIONS_KEY).title, OWNER_SESSIONS_LABEL);
        assert!(m.find(UNASSIGNED_KEY).is_none());
    }

    #[test]
    fn a_vacant_lead_draws_like_a_vacant_member() {
        let mut app = team_app();
        let mut snap = app.org.snapshot.clone().unwrap();
        snap.members.iter_mut().find(|x| x.id == "m3").unwrap().status = "vacant".into();
        app.org.snapshot = Some(snap);
        let m = app.org_model(false);
        let items = node(&m, "team:t2");
        assert!(items.sessions.is_empty(), "{:?}", items.sessions);
        assert!(items.card.as_ref().unwrap().top.iter().any(|(t, _)| t.starts_with(&format!("{} vacant", symbols::VACANT))));
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
        for want in ["Swarm", "ep \u{00b7} ep title", "+1 assistant", "\u{22ef} reports to items", "\u{2205} vacant", "\u{22b9} orgchart", "guest \u{00b7} from Swarm", "standby", "Owner's sessions", "a assistants"] {
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

    fn task(id: &str, parent: Option<&str>) -> TaskEntry {
        TaskEntry {
            task_id: Some(id.into()),
            name: format!("task {id}"),
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
            parent_task_id: parent.map(str::to_owned),
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        }
    }

    fn spawned_by(app: &mut App, wi: usize, by: &str) {
        app.workspaces[wi].sessions.last_mut().unwrap().managed_by_uid = Some(by.into());
    }

    /// EP leads Swarm (coordinator task `coord`); beta is a member. Each
    /// session sits in a workspace of its own (an ephemeral worktree), so
    /// only the spawner and task chains can place it.
    fn attach_app() -> App {
        let mut app = test_app();
        app.workspaces = ["w-ep", "w-asst", "w-sub", "w-task", "w-coord", "w-own", "w-orph", "w-beta", "w-stamp"].map(ws).into();
        session(&mut app, 0, 0x10, "ep-main", Some("ep-task"));
        session(&mut app, 1, 0x20, "ep-assistant", None);
        spawned_by(&mut app, 1, "ts-10-0");
        session(&mut app, 2, 0x21, "sub-worker", None);
        spawned_by(&mut app, 2, "ts-20-0");
        session(&mut app, 3, 0x30, "via-task", Some("deep"));
        session(&mut app, 4, 0x31, "via-coord", Some("under-coord"));
        session(&mut app, 5, 0x40, "owner-shell", Some("owner-thing"));
        session(&mut app, 6, 0x50, "orphan", None);
        spawned_by(&mut app, 6, "ts-gone-0");
        session(&mut app, 7, 0x70, "beta-main", None);
        session(&mut app, 8, 0x60, "stamped", None);
        app.workspaces[8].sessions[0].org = serde_json::json!({"member_id": "m2", "acting_as": "assistant"}).as_object().cloned();
        app.tasks = vec![task("coord", None), task("ep-task", Some("coord")), task("mid", Some("ep-task")), task("deep", Some("mid")), task("under-coord", Some("coord")), task("owner-thing", None)];
        let mut swarm = team("t1", "Swarm", None, Some("m1"), &[("m1", "primary"), ("m2", "primary")]);
        swarm["coordinator_task_id"] = json!("coord");
        app.org.accept(snapshot(4, vec![owner(), member("m1", "ep", Some("mem-owner"), Some("t1"), Some("ts-10-0")), member("m2", "beta", Some("m1"), Some("t1"), Some("ts-70-0"))], vec![swarm]));
        app
    }

    fn uids(app: &App, folded: &[(usize, usize)]) -> Vec<String> {
        folded.iter().map(|&(wi, si)| app.workspaces[wi].sessions[si].uid.clone()).collect()
    }

    #[test]
    fn sessions_go_under_their_member_by_spawner_task_chain_and_stamp() {
        let app = attach_app();
        let attached = app.session_attachments(app.org_live().unwrap());
        // EP → its assistant → the assistant's sub-worker, each in its own
        // ephemeral worktree: all EP's.
        assert_eq!(attached["ts-20-0"], (Attach::Member("m1".into()), Via::Spawned("ts-10-0".into())));
        assert_eq!(attached["ts-21-0"], (Attach::Member("m1".into()), Via::Spawned("ts-20-0".into())));
        // Through the task chain: deep → mid → ep-task, EP's main's task.
        assert_eq!(attached["ts-30-0"], (Attach::Member("m1".into()), Via::Task("ep-task".into())));
        // Up to the team's coordinator task: the team.
        assert_eq!(attached["ts-31-0"], (Attach::Team("t1".into()), Via::Task("coord".into())));
        // The daemon's member stamp wins.
        assert_eq!(attached["ts-60-0"], (Attach::Member("m2".into()), Via::Stamp));
        // Mains, Owner's own and orphans have no attachment.
        for uid in ["ts-10-0", "ts-70-0", "ts-40-0", "ts-50-0"] {
            assert!(!attached.contains_key(uid), "{uid}");
        }

        let m = app.org_model(false);
        let swarm = node(&m, "team:t1");
        let card = swarm.card.as_ref().unwrap();
        assert_eq!(swarm.sessions.iter().map(|s| s.uid.as_str()).collect::<Vec<_>>(), ["ts-10-0"]);
        assert_eq!(uids(&app, &card.folded), ["ts-20-0", "ts-21-0", "ts-30-0", "ts-31-0"]);
        assert_eq!(card.via["ts-21-0"], "spawned by ep-assistant");
        assert_eq!(card.via["ts-30-0"], "task task ep-task");
        assert_eq!(uids(&app, &node(&m, "member:m2").card.as_ref().unwrap().folded), ["ts-60-0"]);
        // Owner's own session, and the one orphan.
        let own = node(&m, OWNER_SESSIONS_KEY);
        assert_eq!(m.nodes[own.children[0]].sessions[0].uid, "ts-40-0");
        let orphans = node(&m, UNASSIGNED_KEY);
        assert_eq!(m.nodes[orphans.children[0]].sessions[0].uid, "ts-50-0");
    }

    #[test]
    fn ep_assistant_and_sub_worker_render_under_ep() {
        let mut app = attach_app();
        app.cursor = Cursor::Session(0, 0);
        app.handle_event(&key(KeyCode::F(6)));
        let s = screen(&mut app, 200, 50);
        assert!(s.contains("+4 assistants"), "{s}");
        app.handle_event(&key(KeyCode::Char('a')));
        let m = app.org_model(false);
        let swarm = node(&m, "team:t1");
        let shown: Vec<&str> = swarm.sessions.iter().map(|s| s.uid.as_str()).collect();
        assert_eq!(shown, ["ts-10-0", "ts-20-0", "ts-21-0", "ts-30-0", "ts-31-0"]);
        assert!(swarm.card.as_ref().unwrap().bottom.iter().any(|(t, _)| t == "2 assistants via the task tree"));
        let s = screen(&mut app, 200, 50);
        for want in ["ep-assistant", "sub-worker", "via-task"] {
            assert!(s.contains(want), "missing {want}:\n{s}");
        }
        // The info pane says how one got there.
        let i = m.find("team:t1").unwrap();
        app.orgchart.session = Some("ts-21-0".into());
        let info = app.orgchart_info_lines(&m, i, 60).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(info.contains("attached assistant, spawned by ep-assistant"), "{info}");
    }

    #[test]
    fn a_shown_shell_sits_in_its_members_box_outside_the_count() {
        let mut app = attach_app();
        let mut sh = dummy_ts("ts-80-0", false, HostId::local());
        sh.label = "ep shell".into();
        sh.session_type = "bash".into();
        app.workspaces[0].sessions.push(sh);
        let m = app.org_model(true);
        let swarm = node(&m, "team:t1");
        assert_eq!(swarm.sessions.iter().map(|s| s.uid.as_str()).collect::<Vec<_>>(), ["ts-10-0", "ts-80-0"]);
        assert_eq!(swarm.card.as_ref().unwrap().folded.len(), 4, "the shell isn't an assistant");
        // Hidden by default (no `b`).
        assert!(app.org_model(false).find_session("ts-80-0").is_none());
    }

    #[test]
    fn a_wide_fan_out_stacks_vertically() {
        let mut app = attach_app();
        // Eight more sessions Owner started, each on its own task.
        for n in 0..8u32 {
            let id = format!("own-{n}");
            app.tasks.push(task(&id, None));
            app.workspaces.push(ws(&format!("w-own-{n}")));
            let wi = app.workspaces.len() - 1;
            session(&mut app, wi, 0x100 + n, &format!("own {n}"), Some(&id));
        }
        let m = app.org_model(false);
        let own = m.find(OWNER_SESSIONS_KEY).unwrap();
        assert!(m.nodes[own].leader, "more than {STACK_AT} leaves stack");
        let lay = layout(&m, Zoom::FULL, &HashSet::new());
        let xs: HashSet<i32> = m.nodes[own].children.iter().map(|c| lay.boxes[c].x).collect();
        assert_eq!(xs.len(), 1, "one column");
        // A small fan-out stays side by side.
        let orphans = m.find(UNASSIGNED_KEY).unwrap();
        assert!(!m.nodes[orphans].leader);
    }

    /// Today's shape: 3 teams with leads, 45 members, ~100 live sessions,
    /// each lane with a couple of spawned helpers in their own worktrees.
    #[test]
    fn a_full_roster_puts_every_worker_under_its_leader_and_stays_narrow() {
        let mut app = test_app();
        let mut members = vec![owner()];
        let mut teams = Vec::new();
        let mut n = 0x1000u32;
        for t in 0..3 {
            let tid = format!("t{t}");
            let coord = format!("coord-{t}");
            app.tasks.push(task(&coord, None));
            let mut roles = Vec::new();
            for k in 0..15 {
                let mid = format!("m{t}-{k}");
                let mtask = format!("lane-{t}-{k}");
                app.tasks.push(task(&mtask, Some(&coord)));
                app.workspaces.push(ws(&format!("w-{mid}")));
                let wi = app.workspaces.len() - 1;
                let main = n;
                session(&mut app, wi, main, &format!("{mid} main"), Some(&mtask));
                n += 1;
                for h in 0..1 + (k % 2) {
                    app.workspaces.push(ws(&format!("w-{mid}-h{h}")));
                    let hw = app.workspaces.len() - 1;
                    session(&mut app, hw, n, &format!("{mid} helper {h}"), None);
                    spawned_by(&mut app, hw, &format!("ts-{main:x}-0"));
                    n += 1;
                }
                let manager = if k == 0 { "mem-owner".to_owned() } else { format!("m{t}-0") };
                members.push(member(&mid, &mid, Some(&manager), Some(&tid), Some(&format!("ts-{main:x}-0"))));
                roles.push((mid, "primary"));
            }
            let roles_ref: Vec<(&str, &str)> = roles.iter().map(|(a, b)| (a.as_str(), *b)).collect();
            let mut tm = team(&tid, &format!("Team {t}"), None, Some(&format!("m{t}-0")), &roles_ref);
            tm["coordinator_task_id"] = json!(coord);
            teams.push(tm);
        }
        app.org.accept(snapshot(9, members, teams));
        let live = app.workspaces.iter().map(|w| w.sessions.len()).sum::<usize>();
        assert!(live >= 100, "{live}");
        let m = app.org_model(false);
        assert!(m.find(UNASSIGNED_KEY).is_none() && m.find(OWNER_SESSIONS_KEY).is_none(), "every session has a place");
        // Every helper is folded under its lane member.
        let folded: usize = m.nodes.iter().filter_map(|n| n.card.as_ref()).map(|c| c.folded.len()).sum();
        assert_eq!(folded, live - 45);
        let lay = layout(&m, Zoom::FULL, &HashSet::new());
        // Three team columns side by side (and Owner), nothing wider.
        let box_w = lay.boxes.values().map(|p| p.w).max().unwrap();
        assert!(lay.width <= 4 * (box_w + 4), "width {} for boxes {box_w} wide", lay.width);
        if std::env::var_os("CM_ORGCHART_DUMP").is_some() {
            println!("AFTER width={} height={}", lay.width, lay.height);
            app.handle_event(&key(KeyCode::F(6)));
            println!("{}", screen(&mut app, 160, 60));
        }
    }

    #[test]
    fn roster_lists_assistants_by_member_and_only_p_promotes_them() {
        let mut app = attach_app();
        for ws in &mut app.workspaces {
            for ts in &mut ws.sessions {
                ts.host_id = HostId::new("sessions");
            }
        }
        app.open_roster();
        app.roster_accept("load", Ok(json!({"proposals": [], "declined": [], "last_applied_at": null})));
        let rows = app.roster_rows();
        // Folded groups: EP's assistants, beta's, then the two that reach no member.
        assert_eq!(rows, [Row::Group("assistants:m2".into()), Row::Group("assistants:m1".into()), Row::Group(OTHER_GROUP.into())]);
        let len = |k: &str| app.roster_all_rows().iter().filter(|r| matches!(r, Row::Adopt { of, .. } if of.as_deref() == Some(k))).count();
        assert_eq!(len("m1"), 4);
        assert_eq!(len("m2"), 1);
        assert_eq!(app.roster_all_rows().iter().filter(|r| matches!(r, Row::Adopt { of: None, .. })).count(), 2);
        // Open EP's group: y doesn't mark an assistant, p does, and Y
        // (approve all clean proposals) leaves it alone.
        let at = rows.iter().position(|r| *r == Row::Group("assistants:m1".into())).unwrap();
        app.roster.cursor = at;
        app.handle_event(&key(KeyCode::Char(' ')));
        app.roster.cursor = at + 1;
        app.handle_event(&key(KeyCode::Char('y')));
        app.handle_event(&key(KeyCode::Char('Y')));
        assert!(app.roster.edits.values().all(|e| e.approve.is_none()));
        app.handle_event(&key(KeyCode::Char('p')));
        let row = app.roster_rows()[at + 1].clone();
        assert_eq!(app.roster.edits[&format!("adopt:{}", match &row { Row::Adopt { uid, .. } => uid.clone(), _ => unreachable!() })].approve, Some(true));
        // A promoted assistant reports to its member.
        assert_eq!(app.roster_adopt_params(&row, "rid")["manager"], "m1");
    }
}
