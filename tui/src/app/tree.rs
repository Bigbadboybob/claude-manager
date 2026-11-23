//! The nested work tree of the Sessions view's Task sub-view, titled
//! "Workspaces" (org batch, 2026-10-20; spec: Swarm-Coord's org-design
//! PLAN.md, "msgfix").
//!
//! - A workspace header has its sessions directly underneath it. Tasks
//!   live on the board, so the blue task subheaders are gone (Owner,
//!   2026-11-05); the workspace row stands for its task
//!   ([`App::workspace_tasks`]): A-D, A-e, A-i, A-w and A-s act on it there.
//! - A workspace block nests under the block that holds its root task's
//!   parent task, at any depth. Reporting lines follow `parent_task_id`.
//! - A division (an initiative with `kind = "division"`) groups the
//!   top-level blocks of its tree under one header.
//! - Every block draws at the top-level indent, whatever its depth
//!   (Owner, 2026-10-21): the group strip and the level marks carry the
//!   hierarchy.
//! - Workspace headers are tree rows here, as the task headers were: cursor
//!   stops whose keys reach no terminal. Space/Enter on one whose task has
//!   live reports in blocks nested under it folds those blocks (its own
//!   sessions stay) and shows the hidden subtree's rollup; a division
//!   header folds its whole tree. The fold state rides the manifest.
//! - The focused group draws a narrow coloured strip at the sidebar's left
//!   edge (no tint).
//! - The lead session (see [`task_lead`]) of a task with subtasks
//!   carries its level's leader mark ([`leader_glyph`]); leaves carry none.
//!
//! Sections still group top-level blocks. A nested block follows its root
//! block's section unless an explicit assignment pulls it into a different
//! section (or explicitly loose), in which case it becomes a new top-level
//! block. Legacy continuous sessions remain ordinary tree rows.

use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Workspace folds from the manifest's `folded_tasks` field, which holds
/// workspace ids since the task level went (2026-11-05). A key from the
/// task-header era (a JSON `[workspace, task]` pair) folds its workspace,
/// since a workspace header now folds what its task headers did; a bare
/// pre-pair task id, or a workspace the manifest no longer has, is dropped.
pub(super) fn migrate_workspace_folds<'a>(
    folds: &std::collections::BTreeSet<String>,
    workspaces: impl Iterator<Item = &'a String>,
) -> HashSet<String> {
    let known: HashSet<&String> = workspaces.collect();
    folds
        .iter()
        .filter_map(|key| {
            let ws = match serde_json::from_str::<[String; 2]>(key) {
                Ok([ws, _]) => ws,
                Err(_) => key.clone(),
            };
            known.contains(&ws).then_some(ws)
        })
        .collect()
}

/// What a sidebar row belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Node {
    Division(DivisionKey),
    Workspace(usize),
    Member(String),
    Group(String),
}

fn member_fold_key(member: &str) -> String { format!("sidebar:member:{member}") }

pub(super) fn checkout_key_workspace(key: &str) -> Option<String> {
    let encoded = key.strip_prefix("sidebar:checkout:").or_else(|| key.strip_prefix("sidebar:continuation:"))?;
    serde_json::from_str::<(String, String)>(encoded).ok().map(|(_, workspace)| workspace)
}

/// Tree metadata for one Task sub-view row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct TreeRow {
    /// Enclosing nodes of the blocks this row sits inside, outermost first.
    /// Structure only: the sidebar no longer indents by depth.
    pub cells: Vec<Node>,
    /// The node the row belongs to: a header's own node, a session's
    /// workspace.
    pub node: Option<Node>,
}

/// `(sessions, running, idle, alerting)` under a folded header.
pub(super) type Rollup = (usize, usize, usize, usize);

#[derive(Clone)]
pub(super) struct TaskTree {
    pub items: Vec<VisualItem>,
    pub rows: HashMap<VisualItem, TreeRow>,
    /// Each node's parent in the tree.
    pub parents: HashMap<Node, Node>,
    /// The top-level block each visible workspace renders under.
    pub block_root: HashMap<usize, usize>,
    /// Rollups for folded headers.
    pub folded: HashMap<Node, Rollup>,
    /// Workspaces whose task has live reports in nested blocks under
    /// them: their header folds.
    pub foldable: HashSet<usize>,
    /// The tasks each visible workspace row stands for (see
    /// [`App::workspace_tasks`]), for header rendering.
    pub roots: HashMap<usize, Vec<String>>,
    /// Divisions by initiative id, for header rendering.
    pub divisions: HashMap<String, crate::planning::Division>,
    /// Reporting lines for this frame: levels and live reports (the tasks
    /// whose lead carries a leader mark).
    pub org: OrgIndex,
    /// The section each visible workspace draws in (its effective block
    /// root's); loose workspaces are absent.
    pub section_of: HashMap<usize, String>,
    /// Each section's visible workspaces, in workspace order.
    pub section_members: HashMap<String, Vec<usize>>,
    pub section_sessions: HashMap<String, Vec<(usize, usize)>>,
    /// Main session row → member block, and the blocks with hidden content.
    pub member_headers: HashMap<(usize, usize), String>,
    pub member_foldable: HashSet<String>,
    /// Labels for synthetic member, team, and checkout rows.
    pub org_labels: HashMap<String, String>,
    /// Org rows can share a workspace across sections, so section is per row.
    pub row_sections: HashMap<VisualItem, String>,
    pub checkout_labels: HashMap<VisualItem, String>,
    pub team_labels: HashMap<VisualItem, String>,
    pub boxed_rows: HashSet<VisualItem>,
    pub boxed_last: HashSet<VisualItem>,
}

/// Reporting lines and liveness, built once per frame and shared by the
/// sidebar (leader marks) and the F6 org chart.
#[derive(Clone, Default)]
pub(crate) struct OrgIndex {
    parents: HashMap<String, String>,
    /// Tasks with at least one live report: a child task that is not done
    /// or archived and has a live agent session in its own subtree (through
    /// open tasks only). An empty workspace, a bash-only or sessionless
    /// subtask, or an all-done subtree kept on disk is not a report.
    reports: HashSet<String>,
}

impl OrgIndex {
    /// A task's level below Owner: 1 for a root task, 2 for its subtask,
    /// and so on, along `parent_task_id` (a cycle ends the walk).
    pub(crate) fn level(&self, task_id: &str) -> usize {
        let mut seen = HashSet::from([task_id]);
        let mut level = 1;
        let mut cur = task_id;
        while let Some(p) = self.parents.get(cur).map(String::as_str).filter(|p| seen.insert(*p)) {
            level += 1;
            cur = p;
        }
        level
    }

    /// Whether a task has live reports (see [`OrgIndex::reports`]).
    pub(crate) fn has_reports(&self, task_id: &str) -> bool {
        self.reports.contains(task_id)
    }
}

impl TaskTree {
    /// The node and its ancestors, innermost first.
    pub(super) fn chain(&self, node: &Node) -> Vec<Node> {
        let mut out = vec![node.clone()];
        let mut seen = HashSet::from([node.clone()]);
        let mut cur = node;
        while let Some(p) = self.parents.get(cur) {
            if !seen.insert(p.clone()) {
                break;
            }
            out.push(p.clone());
            cur = p;
        }
        out
    }

    /// Whether `node` lies inside `scope` (or is it).
    pub(super) fn within(&self, node: &Node, scope: &Node) -> bool {
        self.chain(node).contains(scope)
    }
}

/// The bound tasks with no bound ancestor, in order. A parent cycle among
/// them would leave no root and hide the workspace's task: it breaks at its
/// lowest task id, which becomes a root.
fn root_tasks(bound: &[String], parents: &HashMap<&str, &str>) -> Vec<String> {
    let mut local_parent: HashMap<&str, &str> = HashMap::new();
    for t in bound {
        let mut seen: HashSet<&str> = HashSet::new();
        let mut p = parents.get(t.as_str()).copied();
        while let Some(pt) = p.filter(|pt| seen.insert(*pt)) {
            if let Some(b) = bound.iter().find(|b| b.as_str() == pt) {
                local_parent.insert(t.as_str(), b.as_str());
                break;
            }
            p = parents.get(pt).copied();
        }
    }
    for t in bound {
        let mut seen: Vec<&str> = vec![t.as_str()];
        let mut cur = t.as_str();
        while let Some(p) = local_parent.get(cur).copied() {
            if let Some(at) = seen.iter().position(|s| *s == p) {
                if let Some(lowest) = seen[at..].iter().min().copied() {
                    local_parent.remove(lowest);
                }
                break;
            }
            seen.push(p);
            cur = p;
        }
    }
    bound
        .iter()
        .filter(|t| !local_parent.contains_key(t.as_str()))
        .cloned()
        .collect()
}

/// Hash a collection's elements in any order, without sorting or
/// allocating: the count, the sum and a mixed xor of the elements' own
/// hashes identify the multiset.
fn hash_unordered<T: std::hash::Hash>(items: impl IntoIterator<Item = T>, h: &mut impl std::hash::Hasher) {
    use std::hash::{Hash, Hasher};
    let (mut n, mut sum, mut mix) = (0u64, 0u64, 0u64);
    for item in items {
        let mut e = std::collections::hash_map::DefaultHasher::new();
        item.hash(&mut e);
        let v = e.finish();
        n += 1;
        sum = sum.wrapping_add(v);
        mix ^= v.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
    }
    (n, sum, mix).hash(h);
}

/// How visible workspaces relate through the task tree.
#[derive(Default)]
struct Forest {
    order: Vec<usize>,
    /// Session rows per workspace (continuous members excluded).
    sessions: HashMap<usize, Vec<usize>>,
    /// The root tasks of each workspace, in display order.
    roots: HashMap<usize, Vec<String>>,
    /// Every workspace that has the task bound.
    home: HashMap<String, Vec<usize>>,
    /// A nested block: child workspace → the workspace it hangs under.
    attach: HashMap<usize, usize>,
    /// Blocks hanging under each workspace, in display order.
    children: HashMap<usize, Vec<usize>>,
}

/// The snapshot's host-qualified placement, before sections or folding.
/// Sessions from older or unreachable hosts remain in the legacy forest.
#[derive(Default)]
pub(super) struct OrgPlacement {
    pub(super) members: HashMap<String, Vec<(usize, usize)>>,
    fallback: HashSet<(usize, usize)>,
}

#[derive(Debug)]
struct OrgMemberBlock {
    id: String,
    main: Option<(usize, usize)>,
    sessions: Vec<(usize, usize)>,
    parent: Option<String>,
    section: Option<String>,
    team: Option<String>,
    boxed: Option<String>,
}

#[derive(Debug)]
struct OrgContinuation {
    member: String,
    workspace: usize,
    section: Option<String>,
    sessions: Vec<(usize, usize)>,
}

#[derive(Default)]
struct OrgBlocks {
    members: Vec<OrgMemberBlock>,
    continuations: Vec<OrgContinuation>,
}

impl OrgPlacement {
    /// A continuous lead owns every placed member below it. If continuous
    /// teams nest, the outer lead claims the subtree first so a session can
    /// never be emitted in two boxes.
    pub(super) fn box_owners(&self, snap: &super::org_model::OrgSnapshot) -> HashMap<String, String> {
        let depth = |id: &str| {
            let mut seen = HashSet::new();
            let mut current = Some(id);
            while let Some(id) = current.filter(|id| seen.insert(*id)) {
                current = snap.member(id).and_then(|m| m.manager_member_id.as_deref());
            }
            seen.len()
        };
        let mut teams: Vec<_> = snap.teams.iter().filter(|team| !team.archived()).filter_map(|team| {
            Some((team, team.continuous.as_ref()?.lead_member_id.as_deref()?))
        }).collect();
        teams.sort_by(|(a, al), (b, bl)| depth(al).cmp(&depth(bl)).then(a.id.cmp(&b.id)));
        let mut owners = HashMap::new();
        for (team, lead) in teams {
            for member in self.members.keys().filter(|id| snap.in_member_subtree(id, lead)) {
                owners.entry(member.clone()).or_insert_with(|| team.id.clone());
            }
        }
        owners
    }
}

impl App {
    /// Build member blocks independently of task ancestry. A checkout
    /// override can detach a stream into a continuation, but never splits a
    /// main's home checkout or a continuous box.
    fn org_blocks(&self, snap: &super::org_model::OrgSnapshot, placement: &OrgPlacement) -> OrgBlocks {
        let mut active: HashSet<String> = placement.members.keys().cloned().collect();
        let mut expanded = placement.members.clone();
        for id in active.clone() {
            let mut seen = HashSet::from([id.clone()]);
            let mut parent = snap.member(&id).and_then(|m| m.manager_member_id.clone());
            while let Some(pid) = parent.filter(|p| seen.insert(p.clone())) {
                let Some(manager) = snap.member(&pid) else { break; };
                if manager.is_owner() || manager.retired() { break; }
                // A main on an old/unreachable brain is already rendered by
                // the legacy forest; do not invent a duplicate block for it.
                if manager.main.as_ref().and_then(|m| m.daemon_id.as_deref()).is_some_and(|daemon|
                    !snap.host_daemons.values().any(|known| known == daemon)) { break; }
                active.insert(pid.clone());
                expanded.entry(pid.clone()).or_default();
                parent = manager.manager_member_id.clone();
            }
        }
        let boxes = OrgPlacement { members: expanded, fallback: HashSet::new() }.box_owners(snap);
        let mut result = OrgBlocks::default();
        let mut section_by_member = HashMap::new();
        for member in snap.members.iter().filter(|m| active.contains(&m.id) && !m.retired()) {
            section_by_member.insert(member.id.clone(), super::sections::member_section(
                snap, &self.member_sections, &self.sections, &member.id));
        }
        for member in snap.members.iter().filter(|m| active.contains(&m.id) && !m.retired()) {
            let boxed = boxes.get(&member.id).cloned();
            // A continuous team has one box in its lead's section, even if
            // the lead's host is unavailable and several reports are roots.
            let section = if let Some(team) = boxed.as_deref() {
                snap.team(team).and_then(|team| team.continuous.as_ref())
                    .and_then(|continuous| continuous.lead_member_id.as_deref())
                    .and_then(|lead| super::sections::member_section(snap, &self.member_sections, &self.sections, lead))
            } else {
                section_by_member.get(&member.id).cloned().flatten()
            };
            let mut rows = placement.members.get(&member.id).cloned().unwrap_or_default();
            let main = rows.iter().copied().find(|(wi, si)| {
                let ts = &self.workspaces[*wi].sessions[*si];
                member.main.as_ref().is_some_and(|m| m.session_uid.as_deref() == Some(&ts.uid)
                    && m.daemon_id.as_deref() == snap.host_daemons.get(ts.host_id.as_str()).map(String::as_str))
            });
            let main_ws = main.map(|(wi, _)| wi);
            let mut continuations: BTreeMap<(usize, Option<String>), Vec<(usize, usize)>> = BTreeMap::new();
            rows.retain(|&(wi, si)| {
                // A main's checkout stays in the member's section; conflicting
                // overrides go on the review list. Only other checkouts get continuations.
                if boxed.is_some() || Some(wi) == main_ws { return true; }
                if self.fallback_only_workspace_sections.contains(&self.workspaces[wi].id) { return true; }
                let Some(choice) = self.workspace_sections.get(&self.workspaces[wi].id) else { return true; };
                let override_section = if choice.is_empty() { None } else if self.section_by_id(choice).is_some() {
                    Some(choice.clone())
                } else { return true; };
                if override_section == section { return true; }
                continuations.entry((wi, override_section)).or_default().push((wi, si));
                false
            });
            rows.sort_by_key(|&(wi, si)| ((wi, si) != main.unwrap_or((usize::MAX, usize::MAX)), Some(wi) != main_ws, wi, si));
            result.continuations.extend(continuations.into_iter().map(|((workspace, section), sessions)| OrgContinuation {
                member: member.id.clone(), workspace, section, sessions,
            }));
            let parent = member.manager_member_id.as_ref().filter(|id| {
                active.contains(*id) && snap.member(id).is_some_and(|m| !m.is_owner())
                    && (boxed.is_some() && boxes.get(*id) == boxed.as_ref()
                        || boxed.is_none() && !boxes.contains_key(*id) && section_by_member.get(*id) == section_by_member.get(&member.id))
            }).cloned();
            result.members.push(OrgMemberBlock {
                id: member.id.clone(), main, sessions: rows, parent, section,
                team: member.primary_team_id.clone(), boxed,
            });
        }
        // A malformed manager cycle must not hide the entire cycle.
        let parents: HashMap<_, _> = result.members.iter().filter_map(|m| Some((m.id.clone(), m.parent.clone()?))).collect();
        for block in &mut result.members {
            let mut seen = HashSet::from([block.id.as_str()]);
            let mut parent = block.parent.as_deref();
            while let Some(id) = parent {
                if !seen.insert(id) { block.parent = None; break; }
                parent = parents.get(id).map(String::as_str);
            }
        }
        result
    }

    pub(super) fn org_placement(&self, snap: &super::org_model::OrgSnapshot) -> OrgPlacement {
        let mut result = OrgPlacement::default();
        let by_host_uid: HashMap<_, _> = self.workspaces.iter()
            .flat_map(|w| &w.sessions).map(|session| ((session.host_id.as_str(), session.uid.as_str()), session)).collect();
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed { continue; }
            let mut placed = Vec::new();
            let mut members = HashSet::new();
            let mut non_agents = Vec::new();
            let mut unattributed_agent = false;
            for (si, ts) in ws.sessions.iter().enumerate() {
                let Some(daemon) = snap.host_daemons.get(ts.host_id.as_str()) else {
                    unattributed_agent |= is_agent_engine(&ts.session_type);
                    result.fallback.insert((wi, si));
                    continue;
                };
                if !is_agent_engine(&ts.session_type) {
                    non_agents.push(si);
                } else if let Some(member) = snap.member_by_session(daemon, &ts.uid).or_else(|| {
                    let mut seen = HashSet::from([ts.uid.as_str()]);
                    let mut parent = ts.managed_by_uid.as_deref();
                    while let Some(uid) = parent.filter(|uid| seen.insert(*uid)) {
                        if let Some(member) = snap.member_by_session(daemon, uid) { return Some(member); }
                        parent = by_host_uid.get(&(ts.host_id.as_str(), uid)).and_then(|session| session.managed_by_uid.as_deref());
                    }
                    snap.owner()
                }) {
                    members.insert(member.id.clone());
                    placed.push((si, member.id.clone()));
                } else {
                    unattributed_agent = true;
                    result.fallback.insert((wi, si));
                }
            }
            // A shell follows its workspace's member. If the workspace does
            // not identify one unique member, it belongs under Owner.
            let shell_member = if members.len() == 1 && !unattributed_agent { members.iter().next().cloned() }
                else { snap.owner().map(|owner| owner.id.clone()) };
            for si in non_agents {
                if let Some(member) = &shell_member { placed.push((si, member.clone())); }
                else { result.fallback.insert((wi, si)); }
            }
            for (si, member) in placed {
                result.members.entry(member).or_default().push((wi, si));
            }
        }
        result
    }

    /// Marks belong only to a member's actual main, and use live org
    /// relations rather than task ancestry. Shells never confer a mark.
    pub(super) fn org_member_marks(&self, snap: &super::org_model::OrgSnapshot, placement: &OrgPlacement)
        -> HashMap<(usize, usize), &'static str>
    {
        let mut marks = HashMap::new();
        for member in &snap.members {
            let Some(rows) = placement.members.get(&member.id) else { continue; };
            let main = rows.iter().copied().find(|(wi, si)| {
                let ts = &self.workspaces[*wi].sessions[*si];
                member.main.as_ref().is_some_and(|m| m.session_uid.as_deref() == Some(&ts.uid)
                    && m.daemon_id.as_deref() == snap.host_daemons.get(ts.host_id.as_str()).map(String::as_str))
            });
            let Some(main) = main else { continue; };
            if member.is_owner() { marks.insert(main, super::symbols::OWNER); continue; }
            let live = |wi: usize, si: usize| {
                let ts = &self.workspaces[wi].sessions[si];
                !ts.session.exited && is_agent_engine(&ts.session_type)
            };
            let assistant = rows.iter().any(|&(wi, si)| (wi, si) != main && live(wi, si));
            let report = placement.members.iter().any(|(other, rows)| other != &member.id
                && snap.in_member_subtree(other, &member.id) && rows.iter().any(|&(wi, si)| live(wi, si)));
            if !assistant && !report { continue; }
            let mut depth = 1;
            let mut seen = HashSet::from([member.id.as_str()]);
            let mut parent = member.manager_member_id.as_deref();
            while let Some(id) = parent.filter(|id| seen.insert(*id)) {
                let Some(manager) = snap.member(id) else { break; };
                if manager.is_owner() { break; }
                depth += 1;
                parent = manager.manager_member_id.as_deref();
            }
            marks.insert(main, leader_glyph(depth));
        }
        marks
    }
}

impl Forest {
    fn root(&self, wi: usize) -> Option<&String> {
        self.roots.get(&wi)?.first()
    }

    fn nested_blocks(&self, wi: usize) -> Vec<usize> {
        self.children.get(&wi).cloned().unwrap_or_default()
    }

    /// Every session row of a block, nested blocks included.
    fn block_sessions(&self, wi: usize, out: &mut Vec<(usize, usize)>) {
        out.extend(self.sessions.get(&wi).into_iter().flatten().map(|si| (wi, *si)));
        for wj in self.nested_blocks(wi) {
            self.block_sessions(wj, out);
        }
    }
}

impl App {
    fn org_emit_section(&self, out: &mut TaskTree, plan: &OrgBlocks, section: Option<&str>, folds: bool,
        legacy_boxes: &mut HashMap<String, Vec<(usize, usize)>>)
    {
        let mut visited = HashSet::new();
        let roots: Vec<_> = plan.members.iter().filter(|m| m.parent.is_none() && m.boxed.is_none()
            && m.section.as_deref() == section).collect();
        let mut groups: Vec<(Option<String>, Vec<&str>)> = Vec::new();
        for root in roots {
            if let Some((_, ids)) = groups.iter_mut().find(|(team, _)| team == &root.team && team.is_some()) {
                ids.push(&root.id);
            } else {
                groups.push((root.team.clone(), vec![&root.id]));
            }
        }
        for (team, ids) in groups {
            let mut cells = Vec::new();
            if let Some(team) = team {
                let key = format!("sidebar:team:{}", serde_json::to_string(&(team.as_str(), section)).unwrap_or_default());
                let node = Node::Group(key.clone());
                let label = self.sidebar_org_live().and_then(|s| s.team(&team)).map_or_else(|| team.clone(), |t| t.name.clone());
                out.org_labels.insert(key.clone(), label);
                let row = VisualItem::OrgHeader { key: key.clone(), depth: 0 };
                out.rows.insert(row.clone(), TreeRow { cells: Vec::new(), node: Some(node.clone()) });
                out.row_sections.insert(row.clone(), section.unwrap_or("").into());
                out.items.push(row);
                if folds && self.tree_folded(&node) { continue; }
                cells.push(node);
            }
            for id in ids { self.org_emit_block(out, plan, id, section, false, folds, cells.clone(), &mut visited); }
        }
        let mut box_ids = Vec::new();
        for block in plan.members.iter().filter(|m| m.parent.is_none() && m.boxed.is_some() && m.section.as_deref() == section) {
            let id = block.boxed.as_ref().unwrap();
            if !box_ids.contains(id) { box_ids.push(id.clone()); }
        }
        for team in box_ids {
            let header = VisualItem::ContinuousTeamHeader(team.clone());
            out.row_sections.insert(header.clone(), section.unwrap_or("").into());
            out.items.push(header);
            let start = out.items.len();
            for block in plan.members.iter().filter(|m| m.parent.is_none() && m.boxed.as_deref() == Some(team.as_str()) && m.section.as_deref() == section) {
                self.org_emit_block(out, plan, &block.id, section, true, folds, Vec::new(), &mut visited);
            }
            for (wi, si) in legacy_boxes.remove(&team).unwrap_or_default() {
                let row = VisualItem::ContinuousTeamSession(wi, si);
                out.boxed_rows.insert(row.clone());
                out.row_sections.insert(row.clone(), section.unwrap_or("").into());
                out.items.push(row);
            }
            if let Some(last) = out.items.get(start..).and_then(|rows| rows.last()).cloned() {
                out.boxed_last.insert(last);
            }
        }
        for continuation in plan.continuations.iter().filter(|c| c.section.as_deref() == section) {
            let key = format!("sidebar:continuation:{}", serde_json::to_string(&(continuation.member.as_str(), self.workspaces[continuation.workspace].id.as_str())).unwrap_or_default());
            let name = self.sidebar_org_live().map_or_else(|| continuation.member.clone(), |s| s.handle(&continuation.member));
            out.org_labels.insert(key.clone(), format!("↳ {name} · {}", self.workspaces[continuation.workspace].name));
            let header = VisualItem::OrgHeader { key, depth: 0 };
            out.row_sections.insert(header.clone(), section.unwrap_or("").into());
            out.items.push(header);
            for &(wi, si) in &continuation.sessions {
                let row = VisualItem::Session(wi, si);
                out.rows.insert(row.clone(), TreeRow { cells: Vec::new(), node: Some(Node::Member(continuation.member.clone())) });
                out.row_sections.insert(row.clone(), section.unwrap_or("").into());
                out.items.push(row);
            }
        }
    }

    fn org_emit_block(&self, out: &mut TaskTree, plan: &OrgBlocks, id: &str,
        section: Option<&str>, boxed: bool, folds: bool, cells: Vec<Node>, visited: &mut HashSet<String>)
    {
        if !visited.insert(id.to_owned()) { return; }
        let Some(block) = plan.members.iter().find(|m| m.id == id) else { return; };
        let node = Node::Member(id.into());
        if let Some(parent) = cells.last() { out.parents.insert(node.clone(), parent.clone()); }
        let children: Vec<&str> = plan.members.iter().filter(|m| m.parent.as_deref() == Some(id)).map(|m| m.id.as_str()).collect();
        if block.sessions.iter().any(|row| Some(*row) != block.main) || !children.is_empty() {
            out.member_foldable.insert(id.into());
        }
        let workspaces: HashSet<_> = block.sessions.iter().map(|(wi, _)| *wi).collect();
        let mut own_cells = cells.clone();
        own_cells.push(node.clone());
        let header = if let Some((wi, si)) = block.main {
            out.member_headers.insert((wi, si), id.into());
            if boxed { VisualItem::ContinuousTeamSession(wi, si) } else { VisualItem::Session(wi, si) }
        } else {
            let key = member_fold_key(id);
            let label = self.sidebar_org_live().map_or_else(|| id.to_owned(), |s| {
                if s.member(id).is_some_and(|m| m.is_owner()) { format!("{} Owner", super::symbols::OWNER) } else { s.handle(id) }
            });
            out.org_labels.insert(key.clone(), label);
            VisualItem::OrgHeader { key, depth: 0 }
        };
        out.rows.insert(header.clone(), TreeRow { cells: cells.clone(), node: Some(node.clone()) });
        if let Some(team) = block.team.as_ref().filter(|team| {
            block.parent.as_ref().and_then(|parent| plan.members.iter().find(|m| &m.id == parent))
                .is_some_and(|parent| parent.team.as_ref() != Some(*team))
        }) {
            let label = self.sidebar_org_live().and_then(|snap| snap.team(team)).map_or_else(|| team.clone(), |t| t.name.clone());
            out.team_labels.insert(header.clone(), label);
        }
        out.row_sections.insert(header.clone(), section.unwrap_or("").into());
        if boxed { out.boxed_rows.insert(header.clone()); }
        if let Some((wi, _)) = block.main {
            if workspaces.len() > 1 { out.checkout_labels.insert(header.clone(), self.workspaces[wi].name.clone()); }
            if let Some(sid) = section {
                let entries = out.section_members.entry(sid.into()).or_default();
                if !entries.contains(&wi) { entries.push(wi); }
            }
        }
        out.items.push(header);
        if folds && self.tree_folded(&node) && out.member_foldable.contains(id) {
            let mut hidden = Vec::new();
            for m in &plan.members {
                let mut parent = m.parent.as_deref();
                let mut seen = HashSet::new();
                let mut nested = false;
                while let Some(current) = parent.filter(|p| seen.insert(*p)) {
                    if current == id { nested = true; break; }
                    parent = plan.members.iter().find(|entry| entry.id == current).and_then(|entry| entry.parent.as_deref());
                }
                if m.id == id || nested {
                    hidden.extend(m.sessions.iter().copied().filter(|row| m.id != id || Some(*row) != block.main));
                }
            }
            out.folded.insert(node, self.rollup(hidden.iter().map(|(wi, si)| &self.workspaces[*wi].sessions[*si])));
            return;
        }
        let mut last_checkout = block.main.map(|(wi, _)| wi);
        for &(wi, si) in block.sessions.iter().filter(|row| Some(**row) != block.main) {
            if workspaces.len() > 1 && last_checkout != Some(wi) {
                let key = format!("sidebar:checkout:{}", serde_json::to_string(&(id, self.workspaces[wi].id.as_str())).unwrap_or_default());
                out.org_labels.insert(key.clone(), format!("· {}", self.workspaces[wi].name));
                let checkout = VisualItem::OrgHeader { key, depth: 0 };
                out.rows.insert(checkout.clone(), TreeRow { cells: own_cells.clone(), node: Some(node.clone()) });
                out.row_sections.insert(checkout.clone(), section.unwrap_or("").into());
                if boxed { out.boxed_rows.insert(checkout.clone()); }
                out.items.push(checkout);
            }
            last_checkout = Some(wi);
            let row = if boxed { VisualItem::ContinuousTeamSession(wi, si) } else { VisualItem::Session(wi, si) };
            out.rows.insert(row.clone(), TreeRow { cells: own_cells.clone(), node: Some(node.clone()) });
            out.row_sections.insert(row.clone(), section.unwrap_or("").into());
            if let Some(sid) = section {
                let entries = out.section_members.entry(sid.into()).or_default();
                if !entries.contains(&wi) { entries.push(wi); }
            }
            if boxed { out.boxed_rows.insert(row.clone()); }
            out.items.push(row);
        }
        for child in children {
            self.org_emit_block(out, plan, child, section, boxed, folds, own_cells.clone(), visited);
        }
    }
}

/// One entry of a top-level list: a block, or a division's blocks.
enum Entry {
    Block(usize),
    Division(DivisionKey, Vec<usize>),
}

impl App {
    /// Tasks bound to workspace `wi`, in display order: its planning
    /// bindings, then tasks known only from `sessions` (stale API state,
    /// sorted), so none silently vanish.
    fn bound_tasks<'a>(&self, wi: usize, sessions: impl Iterator<Item = &'a TerminalSession>) -> Vec<String> {
        let ws = &self.workspaces[wi];
        let mut bound: Vec<String> = Vec::new();
        for task in &self.tasks {
            if task.workspace_id.as_deref() == Some(ws.id.as_str()) {
                if let Some(tid) = task.task_id.as_ref().filter(|t| !bound.contains(t)) {
                    bound.push(tid.clone());
                }
            }
        }
        let mut tail: Vec<String> = sessions
            .filter_map(|ts| ts.task_id.clone())
            .filter(|t| !bound.contains(t))
            .collect();
        tail.sort();
        tail.dedup();
        bound.extend(tail);
        bound
    }

    /// The tasks a workspace row stands for: the root tasks bound to it
    /// (planning bindings first), where a subtask sharing its parent's
    /// checkout belongs to that parent. Usually one; several when unrelated
    /// tasks share the checkout. A-e and A-i show them all; A-d, A-w, A-s
    /// and A-f act on the workspace's task only when there is exactly one
    /// ([`App::workspace_task`]).
    pub(crate) fn workspace_tasks(&self, wi: usize) -> Vec<String> {
        let Some(ws) = self.workspaces.get(wi) else {
            return Vec::new();
        };
        let bound = self.bound_tasks(wi, ws.sessions.iter());
        root_tasks(&bound, &self.task_parent_map())
    }

    /// The workspace's task, when exactly one root task is bound to it.
    pub(crate) fn workspace_task(&self, wi: usize) -> Option<String> {
        let mut tasks = self.workspace_tasks(wi);
        (tasks.len() == 1).then(|| tasks.remove(0))
    }

    /// In the Task sub-view the workspace header is a tree row, as the task
    /// header it replaced was: no keys reach a terminal from it, even when
    /// the workspace has a single session (that session is one row down),
    /// and Space/Enter fold its nested blocks.
    pub(crate) fn workspace_header_is_tree_row(&self) -> bool {
        self.tree_view() && self.cursor_column == SidebarColumn::Main
    }

    fn forest(&self, members: &HashSet<(usize, usize)>) -> Forest {
        // A workspace whose every session was lifted into a continuous team
        // box has nothing left to show here; drawing its header anyway left
        // an empty duplicate of each boxed worker in its section (Owner,
        // 2026-11-23, the LEP-*/LPP5 rows). Sessionless workspaces still show.
        let order = self
            .task_view_visible_workspaces(members)
            .into_iter()
            .filter(|&wi| {
                let n = self.workspaces[wi].sessions.len();
                n == 0 || (0..n).any(|si| !members.contains(&(wi, si)))
            })
            .collect();
        let mut f = Forest {
            order,
            ..Forest::default()
        };
        let parents = self.task_parent_map();
        for &wi in &f.order {
            let ws = &self.workspaces[wi];
            let own: Vec<usize> = (0..ws.sessions.len())
                .filter(|si| !members.contains(&(wi, *si)))
                .collect();
            let bound = self.bound_tasks(wi, own.iter().map(|si| &ws.sessions[*si]));
            for t in &bound {
                f.home.entry(t.clone()).or_default().push(wi);
            }
            f.roots.insert(wi, root_tasks(&bound, &parents));
            f.sessions.insert(wi, own);
        }
        // Hang each block under the nearest ancestor of its root task that
        // another visible block holds.
        for &wi in &f.order {
            let Some(root) = f.root(wi).cloned() else {
                continue;
            };
            let mut seen: HashSet<&str> = HashSet::new();
            let mut p = parents.get(root.as_str()).copied();
            while let Some(pt) = p.filter(|pt| seen.insert(*pt)) {
                if let Some(candidates) = f.home.get(pt) {
                    // A manager's workspace is authoritative when it holds
                    // this ancestor. Otherwise choose a parent on the child's
                    // host; never attach a block to an unrelated remote host.
                    let ws = &self.workspaces[wi];
                    let candidates: Vec<usize> = candidates.iter().copied()
                        .filter(|h| *h != wi && self.workspaces[*h].host_id == ws.host_id)
                        .collect();
                    let manager_home = ws.sessions.iter().filter_map(|s| s.managed_by_uid.as_deref())
                        .find_map(|uid| candidates.iter().copied().find(|h|
                            self.workspaces[*h].sessions.iter().any(|s| s.uid == uid)));
                    if let Some(h) = manager_home.or_else(|| candidates.first().copied()) {
                        f.attach.insert(wi, h);
                        break;
                    }
                }
                p = parents.get(pt).copied();
            }
        }
        // A cycle (a task tree that loops through shared tasks) keeps its
        // blocks top-level rather than hiding them.
        let looping: Vec<usize> = f
            .order
            .iter()
            .copied()
            .filter(|&wi| {
                let mut seen = HashSet::from([wi]);
                let mut cur = wi;
                while let Some(p) = f.attach.get(&cur) {
                    if !seen.insert(*p) {
                        return true;
                    }
                    cur = *p;
                }
                false
            })
            .collect();
        for wi in looping {
            f.attach.remove(&wi);
        }
        // An explicit section assignment is a placement override. A nested
        // block stays nested when its assignment agrees with the section of
        // its current root; a different section, or explicit loose, detaches
        // it. Iterate because detaching a parent can change the root section
        // against which one of its children must be compared.
        loop {
            let mut changed = false;
            for &wi in &f.order {
                let Some(choice) = self.workspace_sections.get(&self.workspaces[wi].id) else {
                    continue; // auto: inherit the task-tree placement
                };
                if choice == super::sections::SECTION_NONE {
                    if f.attach.remove(&wi).is_some() {
                        changed = true;
                    }
                    continue;
                }
                // A deleted section is treated as auto by section resolution;
                // do the same for placement so stale manifest ids cannot
                // unexpectedly detach a block.
                if self.section_by_id(choice).is_none() || !f.attach.contains_key(&wi) {
                    continue;
                }
                let mut root = wi;
                let mut seen = HashSet::from([wi]);
                while let Some(parent) = f.attach.get(&root).copied() {
                    if !seen.insert(parent) {
                        break;
                    }
                    root = parent;
                }
                let root_section = self.section_of_workspace(root);
                if root_section.as_deref() != Some(choice.as_str()) {
                    f.attach.remove(&wi);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for &wi in &f.order {
            if let Some(parent) = f.attach.get(&wi) {
                f.children.entry(*parent).or_default().push(wi);
            }
        }
        f
    }

    /// The Task sub-view's rows and their tree metadata, owned (tests).
    #[cfg(test)]
    pub(super) fn task_tree(&self) -> TaskTree {
        // While filtering, folds are ignored so a match is never hidden.
        self.task_tree_shared_with(self.sidebar_filter.is_none()).as_ref().clone()
    }

    /// Shared tree model for rendering. The draw path only needs immutable
    /// access, so avoid cloning all row metadata on every frame.
    pub(super) fn task_tree_shared(&self) -> std::rc::Rc<TaskTree> {
        self.task_tree_shared_with(self.sidebar_filter.is_none())
    }
    /// Hash the inputs read by the forest/emitter. This keeps the cache
    /// correct across manifest refreshes and fold changes without requiring
    /// every mutation path to remember an explicit invalidation call. It
    /// runs on every key and frame, so it neither sorts nor allocates, and
    /// a closed workspace without sessions (the laptop keeps thousands for
    /// A-O) costs one check: it draws nothing and the tree reads nothing
    /// from it.
    fn task_tree_cache_key(&self, folds: bool) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        folds.hash(&mut h);
        for t in &self.tasks {
            t.task_id.hash(&mut h);
            t.parent_task_id.hash(&mut h);
            t.workspace_id.hash(&mut h);
            t.is_continuous.hash(&mut h);
            t.metadata
                .as_ref()
                .and_then(|m| m.get("filer"))
                .and_then(|f| f.get("task_id"))
                .and_then(serde_json::Value::as_str)
                .hash(&mut h);
            match t.api_status {
                TaskStatus::Running => 0u8,
                TaskStatus::Blocked => 1,
                TaskStatus::Backlog => 2,
                TaskStatus::Done => 3,
            }
            .hash(&mut h);
        }
        self.sidebar_org_live().is_some().hash(&mut h);
        if let Some(snap) = self.sidebar_org_live() {
            hash_unordered(&snap.host_daemons, &mut h);
            for member in &snap.members {
                member.id.hash(&mut h);
                member.status.hash(&mut h);
                member.manager_member_id.hash(&mut h);
                if let Some(main) = &member.main {
                    main.daemon_id.hash(&mut h);
                    main.session_uid.hash(&mut h);
                }
                for session in &member.assistants {
                    session.daemon_id.hash(&mut h);
                    session.session_uid.hash(&mut h);
                    session.acting_as.hash(&mut h);
                }
            }
            for team in &snap.teams {
                team.id.hash(&mut h);
                team.status.hash(&mut h);
                if let Some(marker) = &team.continuous {
                    marker.lead_member_id.hash(&mut h);
                    marker.host_id.hash(&mut h);
                    marker.lead_session_uid.hash(&mut h);
                    marker.next_run.map(f64::to_bits).hash(&mut h);
                    marker.cadence_secs.map(f64::to_bits).hash(&mut h);
                    marker.schedule_kind.hash(&mut h);
                    marker.missed_count.hash(&mut h);
                    marker.health.hash(&mut h);
                    serde_json::to_string(&marker.queues).unwrap_or_default().hash(&mut h);
                } else {
                    0u8.hash(&mut h);
                }
            }
        }
        hash_unordered(&self.sidebar_task_parents, &mut h);
        hash_unordered(&self.sidebar_done_task_ids, &mut h);
        // Open workspaces hash with their index: closing or removing one
        // shifts the indices after it.
        self.workspaces.len().hash(&mut h);
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed && ws.sessions.is_empty() {
                continue;
            }
            wi.hash(&mut h);
            ws.id.hash(&mut h);
            ws.is_closed.hash(&mut h);
            ws.is_cloud.hash(&mut h);
            ws.host_id.hash(&mut h);
            ws.worktree_path.hash(&mut h);
            // Folds are read for drawn workspaces only.
            self.folded_workspaces.contains(&ws.id).hash(&mut h);
            for ts in &ws.sessions {
                ts.uid.hash(&mut h);
                ts.host_id.hash(&mut h);
                ts.task_id.hash(&mut h);
                ts.managed_by_uid.hash(&mut h);
                ts.continuous_task_id.hash(&mut h);
                ts.session_type.hash(&mut h);
                ts.workflow_run_id.hash(&mut h);
                ts.workflow_role.hash(&mut h);
                ts.session.exited.hash(&mut h);
                ts.hidden.hash(&mut h);
                match ts.status {
                    SessionStatus::Running => 0u8,
                    SessionStatus::Idle => 1,
                }
                .hash(&mut h);
            }
        }
        let agent_states = self.agent_states.iter().map(|((host, uid), state)| {
            let activity = match state.activity {
                super::agent_state::Activity::Working => 0u8,
                super::agent_state::Activity::Background => 1,
                super::agent_state::Activity::Waiting => 2,
                super::agent_state::Activity::Errored => 3,
                super::agent_state::Activity::Idle => 4,
                super::agent_state::Activity::Starting => 5,
                super::agent_state::Activity::Exited => 6,
                super::agent_state::Activity::Unknown => 7,
            };
            (host, uid, activity, state.stalled)
        });
        hash_unordered(agent_states, &mut h);
        hash_unordered(self.owner_alert_rows.keys(), &mut h);
        hash_unordered(self.alerts.keys(), &mut h);
        hash_unordered(self.workflow_runs.iter().map(|run| (&run.run_id, &run.workflow_name)), &mut h);
        hash_unordered(self.workflows.iter().map(|(name, wf)| (name, &wf.role_order)), &mut h);
        for section in &self.sections {
            section.id.hash(&mut h);
            section.folded.hash(&mut h);
        }
        hash_unordered(&self.workspace_sections, &mut h);
        hash_unordered(&self.fallback_only_workspace_sections, &mut h);
        hash_unordered(&self.member_sections, &mut h);
        hash_unordered(&self.folded_org, &mut h);
        hash_unordered(&self.folded_divisions, &mut h);
        // What `divisions_by_task` and the closed set (`org_index`) read,
        // without building them.
        hash_unordered(
            self.planning
                .loaded_tasks()
                .filter(|t| t.initiative_kind.as_deref() == Some("division"))
                .map(|t| (&t.id, &t.initiative_id, &t.initiative_name, &t.initiative_color, t.is_initiative_coordinator)),
            &mut h,
        );
        hash_unordered(
            self.planning
                .loaded_tasks()
                .filter(|t| matches!(t.status, crate::planning::PlanStatus::Done | crate::planning::PlanStatus::Archived))
                .map(|t| &t.id),
            &mut h,
        );
        h.finish()
    }


    /// Make a session's row show: clear an A-/ filter that excludes it,
    /// then unfold every workspace and division header above its workspace
    /// (whose own fold never hides it) and its root block's section. For
    /// jumps from the org chart (F6) and the board (A-B). Returns true when
    /// anything changed.
    pub(crate) fn reveal_session(&mut self, uid: &str) -> bool {
        let Some((wi, si)) = self
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(wi, ws)| Some((wi, ws.sessions.iter().position(|ts| ts.uid == uid)?)))
        else {
            return false;
        };
        let tree = self.task_tree_unfolded();
        let boxed = VisualItem::ContinuousTeamSession(wi, si);
        let row = if tree.rows.contains_key(&boxed) { boxed } else { VisualItem::Session(wi, si) };
        let node = tree.rows.get(&row).and_then(|r| r.node.clone()).unwrap_or(Node::Workspace(wi));
        self.reveal_row(row, wi, &node)
    }

    /// Reveal the cursor's row when a fold, a folded section or the filter
    /// hides it. Called by the jumps (A-p, A-;, A-g, alerts, org chart) and
    /// by the sidebar draw when the cursor moved since the last draw (spawn,
    /// fork, plan launch, restore, events), so none of those leaves the
    /// cursor on no row; an unmoved cursor never undoes Owner's filter or
    /// folds. Returns true when anything changed.
    pub(crate) fn reveal_cursor(&mut self) -> bool {
        let Some((row, wi, node)) = self.cursor_target() else {
            return false;
        };
        self.reveal_row(row, wi, &node)
    }

    /// The main-column Task sub-view row the cursor stands for:
    /// `(row, workspace, node)`. Only the folds above the node can hide it:
    /// a workspace's own fold hides its nested blocks, never its header or
    /// sessions.
    fn cursor_target(&self) -> Option<(VisualItem, usize, Node)> {
        if !self.tree_view() || self.cursor_column != SidebarColumn::Main {
            return None;
        }
        let target = match &self.cursor {
            Cursor::Session(wi, si) => {
                self.workspaces.get(*wi).and_then(|w| w.sessions.get(*si))?;
                let tree = self.task_tree_unfolded();
                let normal = VisualItem::Session(*wi, *si);
                let boxed = VisualItem::ContinuousTeamSession(*wi, *si);
                let row = if tree.rows.contains_key(&boxed) { boxed } else { normal };
                let node = tree.rows.get(&row).and_then(|r| r.node.clone()).unwrap_or(Node::Workspace(*wi));
                (row, *wi, node)
            }
            Cursor::Workspace(wi) => (VisualItem::WorkspaceHeader(*wi), *wi, Node::Workspace(*wi)),
            _ => return None,
        };
        self.workspaces.get(target.1)?;
        Some(target)
    }

    /// Whether the cursor's row is missing from `visible` (a frame's rows).
    pub(super) fn cursor_row_hidden(&self, visible: &[VisualItem]) -> bool {
        self.cursor_target()
            .is_some_and(|(row, ..)| !visible.contains(&row))
    }

    fn reveal_row(&mut self, row: VisualItem, wi: usize, node: &Node) -> bool {
        let shown = |app: &App| app.visual_items().contains(&row);
        if shown(self) {
            return false;
        }
        let mut changed = false;
        // Filtering ignores folds, so a filter that hides the row is the
        // reason: drop it.
        if self.sidebar_filter.is_some() {
            self.sidebar_filter = None;
            changed = true;
            if shown(self) {
                self.needs_redraw = true;
                return true;
            }
        }
        changed |= self.reveal_node(node);
        let tree = self.task_tree_unfolded();
        let root = tree
            .block_root
            .get(&wi)
            .copied()
            .unwrap_or(wi);
        let section = tree.row_sections.get(&row).cloned().or_else(|| self.section_of_workspace(root));
        if let Some(sid) = section {
            if let Some(sec) = self.sections.iter_mut().find(|s| s.id == sid && s.folded) {
                sec.folded = false;
                changed = true;
                self.save_session_manifest();
            }
        }
        if changed {
            self.needs_redraw = true;
        }
        changed
    }

    /// Clear the folds above `node`.
    fn reveal_node(&mut self, node: &Node) -> bool {
        let mut changed = false;
        let chain = self.task_tree_unfolded().chain(node);
        let include_self = matches!(node, Node::Member(_));
        for n in chain.into_iter().skip(usize::from(!include_self)) {
            changed |= match n {
                Node::Workspace(wi) => match self.workspaces.get(wi) {
                    Some(ws) => self.folded_workspaces.remove(&ws.id),
                    None => false,
                },
                Node::Division(d) => self.folded_divisions.remove(&d.fold_key()),
                Node::Member(id) => self.folded_org.remove(&member_fold_key(&id)),
                Node::Group(key) => self.folded_org.remove(&key),
            };
        }
        if changed {
            self.save_session_manifest();
            self.needs_redraw = true;
        }
        changed
    }

    /// The tree with every fold open: what exists, not what shows.
    pub(super) fn task_tree_unfolded(&self) -> std::rc::Rc<TaskTree> {
        self.task_tree_shared_with(false)
    }

    fn task_tree_shared_with(&self, folds: bool) -> std::rc::Rc<TaskTree> {
        let cache_key = self.task_tree_cache_key(folds);
        if let Some((key, cached)) = self.task_tree_cache.borrow().as_ref() {
            if *key == cache_key {
                return cached.clone();
            }
        }
        // Legacy scheduler sessions are ordinary work-tree rows after phase
        // 5. Only live org-model continuous teams are lifted into their boxed
        // block below.
        let mut members: HashSet<(usize, usize)> = HashSet::new();
        let mut boxes: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        let mut org_plan = None;
        // Use the org manager subtree for hosts that identify their daemon.
        // Older hosts keep the legacy matching until their brain is upgraded.
        if let Some(snap) = self.sidebar_org_live() {
            let placement = self.org_placement(snap);
            if !snap.host_daemons.is_empty() {
                org_plan = Some(self.org_blocks(snap, &placement));
                members.extend(placement.members.values().flatten().copied());
            }
            for team in snap.teams.iter().filter(|t| t.continuous.is_some() && !t.archived()) {
                let lead_uid = team.continuous.as_ref().and_then(|m| m.lead_member_id.as_deref()).and_then(|id| snap.member(id)).and_then(|m| m.main_uid());
                for &(wi, si) in &placement.fallback {
                    let ts = &self.workspaces[wi].sessions[si];
                    if snap.host_daemons.contains_key(ts.host_id.as_str()) { continue; }
                    let belongs = lead_uid.is_some_and(|uid| ts.uid == uid || ts.managed_by_uid.as_deref() == Some(uid))
                        || (lead_uid.is_none() && ts.host_id.as_str() == team.continuous.as_ref().and_then(|m| m.host_id.as_deref()).unwrap_or(""));
                    if belongs && members.insert((wi, si)) { boxes.entry(team.id.clone()).or_default().push((wi, si)); }
                }
            }
            for rows in boxes.values_mut() { rows.sort_unstable(); }
        }
        let f = self.forest(&members);
        let by_task = self.planning.divisions_by_task();
        let mut out = TaskTree {
            items: Vec::new(),
            rows: HashMap::new(),
            parents: HashMap::new(),
            block_root: HashMap::new(),
            folded: HashMap::new(),
            foldable: HashSet::new(),
            roots: f.roots.clone(),
            divisions: by_task
                .values()
                .map(|d| (d.id.clone(), d.clone()))
                .collect(),
            org: self.org_index(),
            section_of: HashMap::new(),
            section_members: HashMap::new(),
            section_sessions: self.sections.iter().map(|s| (s.id.clone(), Vec::new())).collect(),
            member_headers: HashMap::new(),
            member_foldable: HashSet::new(),
            org_labels: HashMap::new(),
            row_sections: HashMap::new(),
            checkout_labels: HashMap::new(),
            team_labels: HashMap::new(),
            boxed_rows: HashSet::new(),
            boxed_last: HashSet::new(),
        };
        for &wi in &f.order {
            let mut top = wi;
            while let Some(p) = f.attach.get(&top) {
                top = *p;
            }
            out.block_root.insert(wi, top);
        }
        let tops: Vec<usize> = f
            .order
            .iter()
            .copied()
            .filter(|wi| !f.attach.contains_key(wi))
            .collect();

        let mut loose: Vec<usize> = Vec::new();
        let mut by_section: HashMap<String, Vec<usize>> = HashMap::new();
        let lookup = self.section_lookup();
        for &wi in &tops {
            match self.section_of_workspace_in(&lookup, wi) {
                Some(sid) => {
                    out.section_of.insert(wi, sid.clone());
                    by_section.entry(sid).or_default().push(wi);
                }
                None => loose.push(wi),
            }
        }
        // A nested block draws in its effective root block's section. The
        // forest has already detached explicit overrides, so a detached
        // workspace is its own root here.
        for &wi in &f.order {
            let Some(sid) = out.section_of.get(&out.block_root[&wi]).cloned() else {
                continue;
            };
            out.section_members.entry(sid.clone()).or_default().push(wi);
            out.section_sessions.entry(sid.clone()).or_default().extend(
                f.sessions.get(&wi).into_iter().flatten().map(|si| (wi, *si)));
            out.section_of.insert(wi, sid);
        }
        if let Some(plan) = &org_plan {
            for block in &plan.members {
                let mut root = block;
                let mut seen = HashSet::new();
                while let Some(parent) = root.parent.as_deref().filter(|p| seen.insert(*p)) {
                    let Some(next) = plan.members.iter().find(|m| m.id == parent) else { break; };
                    root = next;
                }
                if let Some(sid) = &root.section {
                    let entries = out.section_members.entry(sid.clone()).or_default();
                    for (wi, _) in &block.sessions { if !entries.contains(wi) { entries.push(*wi); } }
                    out.section_sessions.entry(sid.clone()).or_default().extend(block.sessions.iter().copied());
                }
            }
            for continuation in &plan.continuations {
                if let Some(sid) = &continuation.section {
                    let entries = out.section_members.entry(sid.clone()).or_default();
                    if !entries.contains(&continuation.workspace) { entries.push(continuation.workspace); }
                    out.section_sessions.entry(sid.clone()).or_default().extend(continuation.sessions.iter().copied());
                }
            }
        }
        let mut emit = Emit {
            app: self,
            f: &f,
            out: &mut out,
            folds,
            divisions: &by_task,
        };
        for sec in &self.sections {
            emit.out
                .items
                .push(VisualItem::SectionHeader(sec.id.clone()));
            // The unfolded tree (`folds == false`) opens sections too.
            if !emit.folds || !sec.folded {
                let entries = emit.entries(by_section.remove(&sec.id).unwrap_or_default(), Some(&sec.id));
                for (i, entry) in entries.into_iter().enumerate() {
                    if i > 0 {
                        emit.out.items.push(VisualItem::Separator);
                    }
                    emit.entry(entry);
                }
                if let Some(plan) = &org_plan {
                    self.org_emit_section(emit.out, plan, Some(&sec.id), folds, &mut boxes);
                }
            }
            emit.out.items.push(VisualItem::SectionEnd);
        }
        for entry in emit.entries(loose, None) {
            if !emit.out.items.is_empty()
                && !matches!(emit.out.items.last(), Some(VisualItem::SectionEnd))
            {
                emit.out.items.push(VisualItem::Separator);
            }
            emit.entry(entry);
        }
        if let Some(plan) = &org_plan {
            self.org_emit_section(emit.out, plan, None, folds, &mut boxes);
        }
        // Continuous org teams stay in the main Task sidebar as a boxed block.
        if let Some(snap) = self.sidebar_org_live() {
            for team in snap.teams.iter().filter(|t| t.continuous.is_some() && !t.archived()) {
                if org_plan.as_ref().is_some_and(|plan| plan.members.iter().any(|m| m.boxed.as_deref() == Some(team.id.as_str()))) { continue; }
                let rows = boxes.remove(&team.id).unwrap_or_default();
                if rows.is_empty() { continue; }
                if !out.items.is_empty() && !matches!(out.items.last(), Some(VisualItem::SectionEnd)) { out.items.push(VisualItem::Separator); }
                out.items.push(VisualItem::ContinuousTeamHeader(team.id.clone()));
                out.items.extend(rows.into_iter().map(|(wi, si)| VisualItem::ContinuousTeamSession(wi, si)));
            }
        }
        let cached = std::rc::Rc::new(out);
        *self.task_tree_cache.borrow_mut() = Some((cache_key, cached.clone()));
        cached
    }

    /// `(sessions, running, idle)` for a set of session rows: engine state
    /// when reported, else the PTY status. Exited sessions are not counted.
    pub(super) fn rollup<'a>(&self, sessions: impl Iterator<Item = &'a TerminalSession>) -> Rollup {
        use super::agent_state::Activity;
        let (mut n, mut running, mut idle, mut alerting) = (0, 0, 0, 0);
        for ts in sessions {
            n += 1;
            if self.session_has_alert(&ts.uid, &ts.host_id) {
                alerting += 1;
            }
            if ts.session.exited {
                continue;
            }
            match self.agent_activity(ts) {
                Some(Activity::Working | Activity::Starting | Activity::Background) => running += 1,
                Some(Activity::Idle) => idle += 1,
                Some(_) => {}
                None => match ts.status {
                    SessionStatus::Running => running += 1,
                    SessionStatus::Idle => idle += 1,
                },
            }
        }
        (n, running, idle, alerting)
    }

    /// The row the cursor is on, as a tree node.
    pub(super) fn cursor_node(&self) -> Option<Node> {
        match &self.cursor {
            Cursor::Division(id) => Some(Node::Division(id.clone())),
            Cursor::Workspace(wi) => Some(Node::Workspace(*wi)),
            Cursor::Session(wi, si) => {
                self.workspaces.get(*wi)?.sessions.get(*si)?;
                let tree = self.task_tree_unfolded();
                tree.rows.get(&VisualItem::Session(*wi, *si))
                    .or_else(|| tree.rows.get(&VisualItem::ContinuousTeamSession(*wi, *si)))
                    .and_then(|r| r.node.clone()).or(Some(Node::Workspace(*wi)))
            }
            Cursor::Org(key) if key.starts_with("sidebar:member:") => Some(Node::Member(key["sidebar:member:".len()..].into())),
            Cursor::Org(key) if key.starts_with("sidebar:team:") => Some(Node::Group(key.clone())),
            Cursor::Backtest(_) | Cursor::Section(_) | Cursor::Org(_) => None,
        }
    }

    pub(super) fn tree_folded(&self, node: &Node) -> bool {
        match node {
            Node::Workspace(wi) => self
                .workspaces
                .get(*wi)
                .is_some_and(|ws| self.folded_workspaces.contains(&ws.id)),
            Node::Division(d) => self.folded_divisions.contains(&d.fold_key()),
            Node::Member(id) => self.folded_org.contains(&member_fold_key(id)),
            Node::Group(key) => self.folded_org.contains(key),
        }
    }

    /// Space/Enter on a workspace (Task sub-view), division or Team header:
    /// fold or unfold it. A workspace without nested blocks has nothing to
    /// fold.
    pub(crate) fn toggle_tree_fold(&mut self) -> bool {
        let (set, id) = match self.cursor.clone() {
            Cursor::Workspace(wi) if self.workspace_header_is_tree_row() => {
                let Some(ws_id) = self.workspaces.get(wi).map(|w| w.id.clone()) else {
                    return false;
                };
                if !self.folded_workspaces.contains(&ws_id)
                    && !self.task_tree_unfolded().foldable.contains(&wi)
                {
                    self.set_status_msg("No subtasks to fold");
                    return true;
                }
                (&mut self.folded_workspaces, ws_id)
            }
            Cursor::Division(key) => (&mut self.folded_divisions, key.fold_key()),
            Cursor::Session(wi, si) => {
                let tree = self.task_tree_unfolded();
                let Some(id) = tree.member_headers.get(&(wi, si)).filter(|id| tree.member_foldable.contains(*id)).cloned() else { return false; };
                (&mut self.folded_org, member_fold_key(&id))
            }
            Cursor::Org(key) if checkout_key_workspace(&key).is_some() => return false,
            Cursor::Org(key) if key.starts_with("sidebar:member:") => {
                let id = &key["sidebar:member:".len()..];
                if !self.task_tree_unfolded().member_foldable.contains(id) { return false; }
                (&mut self.folded_org, key)
            }
            Cursor::Org(key) => (&mut self.folded_org, key),
            _ => return false,
        };
        if !set.remove(&id) {
            set.insert(id);
        }
        self.save_session_manifest();
        self.needs_redraw = true;
        true
    }


    /// The A-e Lead field: Owner, then the task's live agent sessions as
    /// `(uid, label)` in spawn order (a pinned session that no longer qualifies is listed
    /// too, marked, so the form shows it), and the current pin.
    pub(super) fn lead_choices(&self, task_id: &str) -> (Vec<(String, String)>, Option<String>) {
        let pinned = self
            .find_task_entry(task_id)
            .and_then(|e| e.metadata.as_ref())
            .and_then(pinned_lead)
            .map(str::to_owned);
        let mut choices: Vec<(String, String)> = self
            .workspaces
            .iter()
            .filter(|w| !w.is_closed)
            .flat_map(|w| &w.sessions)
            .filter(|ts| ts.task_id.as_deref() == Some(task_id))
            .filter(|ts| !ts.session.exited && is_agent_engine(&ts.session_type))
            .map(|ts| (ts.uid.clone(), ts.label.clone()))
            .collect();
        choices.sort_by(|a, b| {
            spawn_order(&a.0)
                .cmp(&spawn_order(&b.0))
                .then_with(|| a.0.cmp(&b.0))
        });
        // Owner can lead any task (shown first after auto).
        choices.insert(0, (OWNER_LEAD.to_owned(), "Owner".to_owned()));
        let pinned = pinned.map(|p| if is_owner_pin(&p) { OWNER_LEAD.to_owned() } else { p });
        if let Some(p) = pinned
            .as_ref()
            .filter(|p| !choices.iter().any(|(u, _)| u == *p))
        {
            choices.push((p.clone(), format!("{p} (not a live agent here)")));
        }
        (choices, pinned)
    }

    /// For a nested workspace (drawn inside another task's block), the
    /// section it inherits from its root block, as the A-e Section field
    /// shows it. None for a top-level block.
    pub(super) fn nested_section_label(&self, workspace_id: &str) -> Option<String> {
        let wi = self.workspaces.iter().position(|w| w.id == workspace_id)?;
        let root = *self.task_tree_unfolded().block_root.get(&wi)?;
        if root == wi {
            return None;
        }
        let section = self.section_of_workspace(root).map_or_else(
            || "none".to_string(),
            |id| {
                self.section_by_id(&id)
                    .map_or(id.clone(), |s| s.name.clone())
            },
        );
        Some(format!(
            "inherits {section} from {} (nested)",
            self.workspaces[root].name
        ))
    }

    /// Write a task's lead pin locally (the ★ moves at once); returns the
    /// previous pin.
    pub(super) fn set_local_lead_pin(
        &mut self,
        task_id: &str,
        pin: Option<String>,
    ) -> Option<String> {
        let task = self
            .tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some(task_id))?;
        let meta = task.metadata.get_or_insert_with(|| serde_json::json!({}));
        let previous = pinned_lead(meta).map(str::to_owned);
        if let Some(m) = meta.as_object_mut() {
            m.insert(
                "lead_session_uid".into(),
                pin.map_or(serde_json::Value::Null, Into::into),
            );
        }
        previous
    }

    /// The API refused a lead pin: restore the previous one, unless a newer
    /// pin has been set locally since.
    pub(super) fn on_task_lead_failed(
        &mut self,
        task_id: &str,
        attempted: Option<String>,
        previous: Option<String>,
        error: &str,
    ) {
        let current = self
            .find_task_entry(task_id)
            .and_then(|t| t.metadata.as_ref())
            .and_then(pinned_lead)
            .map(str::to_owned);
        if current == attempted {
            self.set_local_lead_pin(task_id, previous);
            self.needs_redraw = true;
        }
        self.set_status_msg(&format!("Lead not saved: {error}"));
    }

    /// Reporting lines and live reports, from the task tree and the live
    /// sessions; build it once per frame (see [`OrgIndex`]).
    pub(crate) fn org_index(&self) -> OrgIndex {
        let parents: HashMap<String, String> = self
            .task_parent_map()
            .into_iter()
            .map(|(t, p)| (t.to_owned(), p.to_owned()))
            .collect();
        // Borrowed: the planning snapshot's done tasks number thousands.
        let mut closed: HashSet<&str> = self
            .planning
            .loaded_tasks()
            .filter(|t| matches!(t.status, crate::planning::PlanStatus::Done | crate::planning::PlanStatus::Archived))
            .map(|t| t.id.as_str())
            .collect();
        closed.extend(self.sidebar_done_task_ids.iter().map(String::as_str));
        closed.extend(
            self.tasks
                .iter()
                .filter(|t| t.api_status == TaskStatus::Done)
                .filter_map(|t| t.task_id.as_deref()),
        );
        // Walk up from every open task with a live agent session: each open
        // task on the way is a live report of its parent. A closed task
        // breaks the chain.
        let mut active: HashSet<String> = HashSet::new();
        let mut reports: HashSet<String> = HashSet::new();
        let staffed = self
            .workspaces
            .iter()
            .filter(|w| !w.is_closed)
            .flat_map(|w| &w.sessions)
            .filter(|ts| !ts.session.exited && is_agent_engine(&ts.session_type))
            .filter_map(|ts| ts.task_id.as_deref());
        for t in staffed {
            let mut cur = t;
            while !closed.contains(cur) && active.insert(cur.to_owned()) {
                let Some(p) = parents.get(cur) else { break };
                reports.insert(p.clone());
                cur = p;
            }
        }
        OrgIndex { parents, reports }
    }

    /// Leader marks by session uid: the lead (shared rule, [`task_leads`])
    /// of each task with live reports gets its level's glyph. The sidebar
    /// and the F6 org chart both draw these.
    ///
    /// [`task_leads`]: App::task_leads
    pub(crate) fn leader_marks(&self, org: &OrgIndex) -> HashMap<String, &'static str> {
        self.task_leads()
            .into_iter()
            .filter(|(t, _)| org.has_reports(t))
            .map(|(t, uid)| (uid, leader_glyph(org.level(&t))))
            .collect()
    }

    /// Every task's lead by the shared rule ([`task_lead`]): tasks with
    /// sessions, division roots (Owner stands in when nobody is live) and
    /// tasks pinned to Owner. A task's pin comes from its work-list row's
    /// metadata (which A-e updates at once) when that is loaded, else from
    /// the planning row.
    pub(crate) fn all_leads(&self) -> HashMap<String, Lead> {
        let mut by_task: HashMap<&str, Vec<LeadCandidate>> = HashMap::new();
        for ws in self.workspaces.iter().filter(|w| !w.is_closed) {
            for ts in &ws.sessions {
                if let Some(t) = ts.task_id.as_deref() {
                    by_task.entry(t).or_default().push(LeadCandidate {
                        uid: &ts.uid,
                        engine: &ts.session_type,
                        live: !ts.session.exited,
                    });
                }
            }
        }
        let mut pins = self.planning.lead_pins();
        // A work-list row's metadata (which A-e updates at once) wins; a row
        // without metadata (not loaded) leaves the planning row's pin.
        for t in &self.tasks {
            if let (Some(id), Some(meta)) = (t.task_id.as_ref(), t.metadata.as_ref()) {
                match pinned_lead(meta) {
                    Some(pin) => pins.insert(id.clone(), pin.to_owned()),
                    None => pins.remove(id),
                };
            }
        }
        let roots: HashSet<String> = self
            .planning
            .divisions_by_task()
            .into_iter()
            .filter(|(_, d)| d.coordinator)
            .map(|(t, _)| t)
            .collect();
        let mut ids: HashSet<&str> = by_task.keys().copied().collect();
        ids.extend(roots.iter().map(String::as_str));
        ids.extend(pins.iter().filter(|(_, p)| is_owner_pin(p)).map(|(t, _)| t.as_str()));
        ids.into_iter()
            .map(|t| {
                let candidates = by_task.get(t).map_or(&[][..], Vec::as_slice);
                (t.to_owned(), task_lead(pins.get(t).map(String::as_str), candidates, roots.contains(t)))
            })
            .filter(|(_, lead)| *lead != Lead::None)
            .collect()
    }

    /// Lead session of every task whose lead is a session.
    pub(super) fn task_leads(&self) -> HashMap<String, String> {
        self.all_leads()
            .into_iter()
            .filter_map(|(t, lead)| Some((t, lead.session()?.to_owned())))
            .collect()
    }

    /// Tasks Owner leads, with whether Owner only stands in.
    pub(crate) fn owner_leads(&self) -> HashMap<String, bool> {
        self.all_leads()
            .into_iter()
            .filter_map(|(t, lead)| match lead {
                Lead::Owner { stand_in } => Some((t, stand_in)),
                _ => None,
            })
            .collect()
    }
}

/// Builds a [`TaskTree`] in display order.
struct Emit<'a> {
    app: &'a App,
    f: &'a Forest,
    out: &'a mut TaskTree,
    folds: bool,
    /// Division of each task in a division, indexed once per build.
    divisions: &'a HashMap<String, crate::planning::Division>,
}

impl Emit<'_> {
    fn push(&mut self, item: VisualItem, row: TreeRow) {
        self.out.rows.insert(item.clone(), row);
        self.out.items.push(item);
    }

    /// Group a top-level list (one section's, or the loose list when
    /// `section` is None) by division, a division at its first block, its
    /// coordinator's block first. The header's key carries `section`, so a
    /// division split across sections draws distinct rows.
    fn entries(&self, tops: Vec<usize>, section: Option<&str>) -> Vec<Entry> {
        let mut entries: Vec<Entry> = Vec::new();
        for wi in tops {
            let division = self
                .f
                .root(wi)
                .and_then(|t| self.divisions.get(t))
                .map(|d| d.id.clone());
            let Some(d) = division else {
                entries.push(Entry::Block(wi));
                continue;
            };
            match entries
                .iter_mut()
                .find(|e| matches!(e, Entry::Division(key, _) if key.id == d))
            {
                Some(Entry::Division(_, blocks)) => blocks.push(wi),
                _ => entries.push(Entry::Division(
                    DivisionKey { id: d, section: section.map(str::to_owned) },
                    vec![wi],
                )),
            }
        }
        for entry in &mut entries {
            if let Entry::Division(_, blocks) = entry {
                let coordinator = |wi: &usize| {
                    !self
                        .f
                        .root(*wi)
                        .is_some_and(|t| self.divisions.get(t).is_some_and(|d| d.coordinator))
                };
                blocks.sort_by_key(coordinator);
            }
        }
        entries
    }

    fn entry(&mut self, entry: Entry) {
        match entry {
            Entry::Block(wi) => self.block(wi, Vec::new()),
            Entry::Division(d, blocks) => {
                let node = Node::Division(d.clone());
                self.push(
                    VisualItem::DivisionHeader(d.clone()),
                    TreeRow {
                        node: Some(node.clone()),
                        ..TreeRow::default()
                    },
                );
                if self.folds && self.app.tree_folded(&node) {
                    let mut sessions = Vec::new();
                    for wi in &blocks {
                        self.f.block_sessions(*wi, &mut sessions);
                    }
                    self.fold(node, &sessions);
                    return;
                }
                for wi in blocks {
                    self.out.parents.insert(Node::Workspace(wi), node.clone());
                    self.block(wi, vec![node.clone()]);
                }
            }
        }
    }

    fn fold(&mut self, node: Node, sessions: &[(usize, usize)]) {
        let rollup = self.app.rollup(
            sessions
                .iter()
                .map(|(wi, si)| &self.app.workspaces[*wi].sessions[*si]),
        );
        self.out.folded.insert(node, rollup);
    }

    /// A workspace header, its sessions, then the blocks nested under it.
    /// A fold hides the nested blocks only: the workspace's own sessions
    /// stay. Only a workspace whose task has live reports folds (the rule
    /// the leader marks and the F6 chart use).
    fn block(&mut self, wi: usize, cells: Vec<Node>) {
        let node = Node::Workspace(wi);
        if let Some(parent) = cells.last() {
            self.out.parents.insert(node.clone(), parent.clone());
        }
        self.push(
            VisualItem::WorkspaceHeader(wi),
            TreeRow {
                cells: cells.clone(),
                node: Some(node.clone()),
            },
        );
        let own = self.f.sessions.get(&wi).cloned().unwrap_or_default();
        self.sessions(wi, &own, &node, &cells);
        let nested = self.f.nested_blocks(wi);
        if nested.is_empty() {
            return;
        }
        let reports = self
            .f
            .roots
            .get(&wi)
            .is_some_and(|roots| roots.iter().any(|t| self.out.org.has_reports(t)));
        if reports {
            self.out.foldable.insert(wi);
        }
        if reports && self.folds && self.app.tree_folded(&node) {
            let mut sessions = Vec::new();
            for wj in &nested {
                self.f.block_sessions(*wj, &mut sessions);
            }
            self.fold(node, &sessions);
            return;
        }
        let mut inner = cells;
        inner.push(node);
        for wj in nested {
            self.block(wj, inner.clone());
        }
    }

    /// A workspace's session rows: standalone (running first), then
    /// workflow groups in role order, exactly as the flat view drew them.
    fn sessions(&mut self, wi: usize, indices: &[usize], node: &Node, cells: &[Node]) {
        let ws = &self.app.workspaces[wi];
        let row = || TreeRow {
            cells: cells.to_vec(),
            node: Some(node.clone()),
        };
        let mut standalone: Vec<usize> = Vec::new();
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for si in indices {
            match &ws.sessions[*si].workflow_run_id {
                Some(run_id) => groups.entry(run_id.clone()).or_default().push(*si),
                None => standalone.push(*si),
            }
        }
        let (running, other): (Vec<_>, Vec<_>) = standalone
            .into_iter()
            .partition(|si| ws.sessions[*si].status == SessionStatus::Running);
        for si in running.into_iter().chain(other) {
            self.push(VisualItem::Session(wi, si), row());
        }
        for (run_id, session_indices) in groups {
            let run = self.app.workflow_runs.iter().find(|r| r.run_id == run_id);
            if run.is_none() {
                for si in session_indices {
                    self.push(VisualItem::Session(wi, si), row());
                }
                continue;
            }
            self.push(
                VisualItem::WorkflowHeader {
                    ws_idx: wi,
                    run_id: run_id.clone(),
                },
                row(),
            );
            let role_order: Vec<String> = run
                .and_then(|r| self.app.workflows.get(&r.workflow_name))
                .map(|wf| wf.role_order.clone())
                .unwrap_or_default();
            let mut ordered = session_indices;
            ordered.sort_by_key(|si| {
                let role = ws.sessions[*si].workflow_role.as_deref().unwrap_or("");
                role_order
                    .iter()
                    .position(|r| r == role)
                    .unwrap_or(usize::MAX)
            });
            for si in ordered {
                self.push(VisualItem::Session(wi, si), row());
            }
        }
    }
}


/// Leader marks by level below Owner, shared by the sidebar and the F6 org
/// chart: ★ for a root task's lead, ☆ one level down, then ✦, then ✧ for
/// every deeper level. ◆ is taken (divisions, coordinators).
pub(crate) const LEADER_GLYPHS: [&str; 4] = ["\u{2605}", "\u{2606}", "\u{2726}", "\u{2727}"];

/// The leader mark for a lead at `level` (1 = a root task).
pub(crate) fn leader_glyph(level: usize) -> &'static str {
    LEADER_GLYPHS[level.clamp(1, LEADER_GLYPHS.len()) - 1]
}

// The lead rule is shared with the daemon's `org` tool: one function.
pub(crate) use cm_daemon::org::{
    is_agent_engine, is_owner_pin, pinned_lead, spawn_order, task_lead, Lead,
    LeadCandidate, OWNER_LEAD,
};

#[cfg(test)]
mod tree_tests {
    use super::super::sections::layout_tests::{bare_ws, sidebar_buffer, task, test_app};
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn org_placement_qualifies_hosts_and_keeps_old_hosts_in_legacy_tree() {
        use super::super::org_model::fixtures::{member, owner, snapshot, team};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        app.workspaces = ["one", "two", "old"].into_iter().map(bare_ws).collect();
        session(&mut app, 0, "a", "same", "codex");
        session(&mut app, 0, "shell", "shell", "bash");
        session(&mut app, 1, "b", "same", "codex");
        session(&mut app, 2, "old", "same", "codex");
        for ts in &mut app.workspaces[0].sessions { ts.host_id = HostId::new("h1"); }
        app.workspaces[1].sessions[0].host_id = HostId::new("h2");
        app.workspaces[2].sessions[0].host_id = HostId::new("old");
        let a = member("a", "A", Some("mem-owner"), None, Some("same"));
        let mut b = member("b", "B", Some("a"), None, Some("same"));
        b["main"]["daemon_id"] = serde_json::json!("d2");
        let mut outer = team("outer", "Outer", None, Some("a"), &[]);
        outer["continuous"] = serde_json::json!({"lead_member_id":"a"});
        let mut inner = team("inner", "Inner", None, Some("b"), &[]);
        inner["continuous"] = serde_json::json!({"lead_member_id":"b"});
        // Put the inner team first to check that ancestor order wins.
        let mut value = snapshot(1, vec![owner(), a, b], vec![inner, outer]);
        value["host_daemons"] = serde_json::json!({"h1":"d1", "h2":"d2"});
        let snap = serde_json::from_value(value).unwrap();
        let result = app.org_placement(&snap);
        assert_eq!(result.members["a"], vec![(0, 0), (0, 1)]);
        assert_eq!(result.members["b"], vec![(1, 0)]);
        assert_eq!(result.fallback, HashSet::from([(2, 0)]));
        assert_eq!(result.box_owners(&snap), HashMap::from([("a".into(), "outer".into()), ("b".into(), "outer".into())]));
    }

    #[test]
    fn unstamped_live_sessions_use_spawner_chain_then_owner_not_tasks() {
        use super::super::org_model::fixtures::{member, owner, snapshot};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        app.workspaces = ["root", "stream", "owner"].into_iter().map(bare_ws).collect();
        for (wi, uid) in [(0, "main"), (1, "child"), (1, "grandchild"), (2, "owner-started")] {
            session(&mut app, wi, "unrelated-task", uid, "codex");
            app.workspaces[wi].sessions.last_mut().unwrap().host_id = HostId::new("h1");
        }
        app.workspaces[1].sessions[0].managed_by_uid = Some("main".into());
        app.workspaces[1].sessions[1].managed_by_uid = Some("child".into());
        let mut value = snapshot(1, vec![owner(), member("m", "M", Some("mem-owner"), None, Some("main"))], vec![]);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        let snap = serde_json::from_value(value).unwrap();
        let placed = app.org_placement(&snap);
        assert_eq!(placed.members["m"], vec![(0, 0), (1, 0), (1, 1)]);
        assert_eq!(placed.members["mem-owner"], vec![(2, 0)]);
        assert!(placed.fallback.is_empty());
    }

    #[test]
    fn continuous_box_takes_the_whole_org_subtree_once() {
        use super::super::org_model::fixtures::{member, owner, snapshot, team};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["lead", "report", "worker", "other", "grandchild"].into_iter().map(bare_ws).collect();
        for (wi, uid) in [(0, "lead"), (1, "report"), (2, "worker"), (3, "other"), (4, "grandchild")] {
            session(&mut app, wi, uid, uid, "codex");
            app.workspaces[wi].sessions[0].host_id = HostId::new("h1");
        }
        app.sections.push(SidebarSection { id: "sec".into(), name: "Section".into(), ..Default::default() });
        app.workspace_sections.insert("worker".into(), "sec".into());
        let lead = member("lead", "Lead", Some("mem-owner"), None, Some("lead"));
        let mut report = member("report", "Report", Some("lead"), None, Some("report"));
        report["assistants"] = serde_json::json!([{"daemon_id":"d1", "session_uid":"worker", "acting_as":"worker"}]);
        let grandchild = member("grandchild", "Grandchild", Some("report"), None, Some("grandchild"));
        let other = member("other", "Other", Some("mem-owner"), None, Some("other"));
        let mut ct = team("ct", "Continuous", None, Some("lead"), &[]);
        ct["continuous"] = serde_json::json!({"lead_member_id":"lead", "health":"ok"});
        let mut value = snapshot(1, vec![owner(), lead, report, grandchild, other], vec![ct]);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        app.org.accept(value);
        let tree = app.task_tree();
        for wi in [0, 1, 2, 4] {
            assert!(tree.items.contains(&VisualItem::ContinuousTeamSession(wi, 0)));
            assert!(!tree.items.contains(&VisualItem::Session(wi, 0)));
            assert!(!tree.items.contains(&VisualItem::WorkspaceHeader(wi)));
        }
        assert!(tree.items.contains(&VisualItem::Session(3, 0)));
        app.org.fail(super::super::org_model::RpcFail { unsupported: false, message: "offline".into() });
        let offline = app.task_tree();
        assert!(!offline.items.iter().any(|item| matches!(item, VisualItem::ContinuousTeamHeader(_))));
        for wi in 0..5 { assert!(offline.items.contains(&VisualItem::Session(wi, 0))); }
    }

    #[test]
    fn member_main_row_opens_its_checkout_settings_without_an_extra_row() {
        use super::super::org_model::fixtures::{member, owner, snapshot, team};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        let _home = crate::test_support::home_lock();
        app.sessions_restored = true;
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["home", "other"].into_iter().map(bare_ws).collect();
        session(&mut app, 0, "lead", "lead", "codex");
        session(&mut app, 1, "other", "other", "codex");
        for ws in &mut app.workspaces { ws.sessions[0].host_id = HostId::new("h1"); }
        app.sections.push(SidebarSection { id: "a".into(), name: "A".into(), ..Default::default() });
        let mut ct = team("ct", "Continuous", None, Some("lead"), &[]);
        ct["continuous"] = serde_json::json!({"lead_member_id":"lead", "health":"ok"});
        let mut value = snapshot(1, vec![owner(), member("lead", "Lead", Some("mem-owner"), None, Some("lead")),
            member("other", "Other", Some("mem-owner"), None, Some("other"))], vec![ct]);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        app.org.accept(value);
        let tree = app.task_tree();
        assert_eq!(tree.items.iter().filter(|row| **row == VisualItem::ContinuousTeamSession(0, 0)).count(), 1);
        assert!(!tree.items.contains(&VisualItem::WorkspaceHeader(0)));

        app.cursor = Cursor::Session(0, 0);
        let key = |code, mods| Event::Key(KeyEvent::new(code, mods));
        assert!(app.handle_event(&key(KeyCode::Char('e'), KeyModifiers::ALT)));
        assert!(matches!(app.input_mode, InputMode::SessionSettings { .. }), "A-e still edits the session");
        app.handle_event(&key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.handle_event(&key(KeyCode::Char('e'), KeyModifiers::ALT | KeyModifiers::CONTROL)));
        assert!(matches!(&app.input_mode, InputMode::WorkspaceSettings { workspace_id, .. } if workspace_id == "home"));
        // Rename, pick a color, pin, and set the checkout override through
        // the same user-facing form reached from the member row.
        app.handle_event(&key(KeyCode::Char('!'), KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Right, KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        let selected = match &app.input_mode {
            InputMode::WorkspaceSettings { color: Some(color), .. } => color.clone(),
            _ => panic!("color was not selected"),
        };
        app.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Char(' '), KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Right, KeyModifiers::NONE));
        app.handle_event(&key(KeyCode::Right, KeyModifiers::NONE));
        app.workspaces.swap(0, 1); // the form uses the stable workspace id
        app.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        let home = app.workspaces.iter().find(|ws| ws.id == "home").unwrap();
        assert_eq!(home.name, "home!");
        assert_eq!(home.color.as_deref(), Some(selected.as_str()));
        assert!(home.pinned);
        assert_eq!(app.workspace_sections.get("home").map(String::as_str), Some("a"));
        let saved = App::load_manifest();
        assert_eq!(saved.workspace_sections.get("home").map(String::as_str), Some("a"));
        assert!(app.member_section_migration_issues.iter().any(|(ws, _, _)| ws == "home"));

        app.keybinding_helper_visible = true;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 50)).unwrap();
        terminal.draw(|frame| app.draw_session_list(frame, frame.area())).unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.height).map(|y|
            (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>().join("\n");
        assert!(text.contains("C-A-e workspace"), "{text}");
        app.cursor = Cursor::Session(1, 0);
        app.control_bound = true;
        app.status_msg = None;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 20)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let status: String = (0..buffer.area.width).map(|x| buffer[(x, buffer.area.height - 1)].symbol()).collect();
        assert!(status.contains("C-A-e workspace"), "{status}");
    }

    #[test]
    fn continuous_box_with_unreachable_lead_does_not_duplicate_reports_across_sections() {
        use super::super::org_model::fixtures::{member, owner, snapshot, team};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["first", "second"].into_iter().map(bare_ws).collect();
        for (wi, uid) in ["first", "second"].into_iter().enumerate() {
            session(&mut app, wi, uid, uid, "codex");
            app.workspaces[wi].sessions[0].host_id = HostId::new("h1");
        }
        app.sections = ["a", "b"].into_iter().map(|id| SidebarSection {
            id: id.into(), name: id.to_uppercase(), ..Default::default()
        }).collect();
        app.member_sections = HashMap::from([
            ("lead".into(), "a".into()), ("first".into(), "a".into()), ("second".into(), "b".into())
        ]);
        let mut lead = member("lead", "Lead", Some("mem-owner"), None, Some("lead"));
        lead["main"]["daemon_id"] = serde_json::json!("unreachable");
        let first = member("first", "First", Some("lead"), None, Some("first"));
        let second = member("second", "Second", Some("lead"), None, Some("second"));
        let mut ct = team("ct", "Continuous", None, Some("lead"), &[]);
        ct["continuous"] = serde_json::json!({"lead_member_id":"lead", "health":"ok"});
        let mut value = snapshot(1, vec![owner(), lead, first, second], vec![ct]);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        app.org.accept(value);

        let tree = app.task_tree();
        assert_eq!(tree.items.iter().filter(|item| **item == VisualItem::ContinuousTeamHeader("ct".into())).count(), 1);
        for wi in 0..2 {
            let row = VisualItem::ContinuousTeamSession(wi, 0);
            assert_eq!(tree.items.iter().filter(|item| **item == row).count(), 1);
            assert!(!tree.items.contains(&VisualItem::Session(wi, 0)));
            assert_eq!(tree.row_sections.get(&row).map(String::as_str), Some("a"));
        }
        let buffer = sidebar_buffer(&mut app, 24);
        let rendered = (0..buffer.area.height).map(|y|
            (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>().join("\n");
        assert_eq!(rendered.matches("first").count(), 1, "{rendered}");
        assert_eq!(rendered.matches("second").count(), 1, "{rendered}");
    }

    #[test]
    fn sidebar_marks_follow_live_org_reports_and_assistants() {
        use super::super::org_model::fixtures::{member, owner, snapshot};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["a", "b", "c", "worker", "leaf"].into_iter().map(bare_ws).collect();
        for (wi, uid) in ["a", "b", "c", "worker", "leaf"].into_iter().enumerate() {
            session(&mut app, wi, uid, uid, "codex");
            app.workspaces[wi].sessions[0].host_id = HostId::new("h1");
        }
        let a = member("a", "A", Some("mem-owner"), None, Some("a"));
        let b = member("b", "B", Some("a"), None, Some("b"));
        let mut c = member("c", "C", Some("b"), None, Some("c"));
        c["assistants"] = serde_json::json!([{"daemon_id":"d1", "session_uid":"worker", "acting_as":"assistant"}]);
        let leaf = member("leaf", "Leaf", Some("mem-owner"), None, Some("leaf"));
        let mut value = snapshot(1, vec![owner(), a, b, c, leaf], vec![]);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        app.org.accept(value);
        let snap = app.org_live().unwrap();
        let marks = app.org_member_marks(snap, &app.org_placement(snap));
        assert_eq!(marks.get(&(0, 0)), Some(&"★"));
        assert_eq!(marks.get(&(1, 0)), Some(&"☆"));
        assert_eq!(marks.get(&(2, 0)), Some(&"✦"));
        assert!(!marks.contains_key(&(3, 0)) && !marks.contains_key(&(4, 0)));
        let buffer = sidebar_buffer(&mut app, 30);
        let lines: Vec<String> = (0..buffer.area.height).map(|y|
            (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect()).collect();
        assert!(lines.iter().any(|line| line.contains("★ a")), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("☆ b")), "{lines:?}");
        assert!(lines.iter().any(|line| line.contains("✦ c")), "{lines:?}");
        app.workspaces[3].sessions[0].session.exited = true;
        let snap = app.org_live().unwrap();
        let marks = app.org_member_marks(snap, &app.org_placement(snap));
        assert!(!marks.contains_key(&(2, 0)), "an exited assistant does not confer a mark");
        assert_eq!(marks.get(&(1, 0)), Some(&"☆"), "the live report still confers a mark");
    }

    #[test]
    fn org_blocks_nest_reports_and_split_only_stream_overrides() {
        use super::super::org_model::fixtures::{member, owner, snapshot, team};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        let _home = crate::test_support::home_lock();
        app.workspaces = ["home", "stream", "report", "worker", "grand"].into_iter().map(bare_ws).collect();
        for (wi, uid) in ["lead", "stream", "report", "worker", "grand"].into_iter().enumerate() {
            session(&mut app, wi, uid, uid, "codex");
            app.workspaces[wi].sessions[0].host_id = HostId::new("h1");
        }
        app.sections = ["a", "b"].into_iter().map(|id| SidebarSection { id: id.into(), name: id.to_uppercase(), ..Default::default() }).collect();
        app.member_sections.insert("lead".into(), "a".into());
        app.workspace_sections.insert("home".into(), "b".into());
        app.workspace_sections.insert("stream".into(), "b".into());
        let mut lead = member("lead", "Lead", Some("mem-owner"), Some("ta"), Some("lead"));
        lead["assistants"] = serde_json::json!([{"daemon_id":"d1", "session_uid":"stream", "acting_as":"assistant"}]);
        let mut report = member("report", "Report", Some("lead"), Some("tb"), Some("report"));
        report["assistants"] = serde_json::json!([{"daemon_id":"d1", "session_uid":"worker", "acting_as":"worker"}]);
        let grand = member("grand", "Grand", Some("report"), Some("tb"), Some("grand"));
        let mut value = snapshot(1, vec![owner(), lead, report, grand], vec![
            team("ta", "Team A", None, Some("lead"), &[]),
            team("tb", "Team B", None, Some("report"), &[]),
        ]);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        let mut snap: super::super::org_model::OrgSnapshot = serde_json::from_value(value.clone()).unwrap();
        let plan = app.org_blocks(&snap, &app.org_placement(&snap));
        assert_eq!(plan.members.len(), 3);
        assert_eq!(plan.members[0].main, Some((0, 0)));
        assert_eq!(plan.members[0].sessions, vec![(0, 0)], "main workspace override does not split the block");
        assert_eq!(plan.members[1].parent.as_deref(), Some("lead"));
        assert_eq!(plan.members[1].section.as_deref(), Some("a"));
        assert_eq!(plan.members[1].sessions, vec![(2, 0), (3, 0)]);
        assert_eq!(plan.members[2].parent.as_deref(), Some("report"));
        assert_eq!(plan.continuations.len(), 1);
        let continuation = &plan.continuations[0];
        assert_eq!((&*continuation.member, continuation.workspace, continuation.section.as_deref(), &continuation.sessions),
            ("lead", 1, Some("b"), &vec![(1, 0)]));
        app.sidebar_view = SidebarView::Task;
        app.org.accept(value.clone());
        assert!(app.workspace_override_conflicts("home", Some("b")).iter().any(|(ws, choice, reason)|
            ws == "home" && choice == "b" && reason.contains("home checkout of member lead")));
        let tree = app.task_tree();
        assert_eq!(tree.parents.get(&Node::Member("report".into())), Some(&Node::Member("lead".into())));
        for wi in 0..5 {
            assert_eq!(tree.items.iter().filter(|row| **row == VisualItem::Session(wi, 0)).count(), 1);
            assert!(!tree.items.contains(&VisualItem::WorkspaceHeader(wi)));
        }
        assert!(tree.org_labels.values().any(|label| label.contains("↳ Lead · stream")));
        assert_eq!(tree.row_sections.get(&VisualItem::Session(1, 0)).map(String::as_str), Some("b"));
        let buffer = sidebar_buffer(&mut app, 24);
        let screenshot = (0..buffer.area.height).map(|y|
            (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n");
        assert!(screenshot.contains("↳ Lead · stream"), "{screenshot}");
        assert!(screenshot.contains("Team A") && screenshot.contains("Team B"), "cross-team report keeps its label: {screenshot}");
        println!("{screenshot}");
        app.cursor = Cursor::Session(0, 0);
        assert!(app.toggle_tree_fold());
        let folded = app.task_tree();
        assert!(folded.items.contains(&VisualItem::Session(0, 0)));
        assert!(!folded.items.contains(&VisualItem::Session(2, 0)));
        assert!(!folded.items.contains(&VisualItem::Session(4, 0)));
        assert!(folded.items.contains(&VisualItem::Session(1, 0)), "detached stream remains in its section");
        assert!(app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::ALT))));
        assert!(app.task_tree().items.contains(&VisualItem::Session(2, 0)), "Alt+Space unfolds the focused main");
        app.open_session_settings();
        assert!(matches!(&app.input_mode, InputMode::SessionSettings { member_section: Some(form), .. } if form.editable && form.choice.as_deref() == Some("a")));
        if let InputMode::SessionSettings { active_field, .. } = &mut app.input_mode { *active_field = 8; }
        app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE)));
        app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(app.member_sections.get("lead").map(String::as_str), Some("b"));
        app.cursor = Cursor::Session(1, 0);
        app.open_session_settings();
        assert!(matches!(&app.input_mode, InputMode::SessionSettings { member_section: Some(form), .. } if !form.editable));
        app.input_mode = InputMode::Normal;
        let continuation_key = tree.org_labels.iter().find(|(_, label)| label.contains("↳ Lead · stream")).unwrap().0.clone();
        app.cursor = Cursor::Org(continuation_key);
        app.open_session_settings();
        assert!(matches!(&app.input_mode, InputMode::WorkspaceSettings { workspace_id, .. } if workspace_id == "stream"));
        app.input_mode = InputMode::Normal;
        app.member_sections.insert("lead".into(), "a".into());

        let mut ct = team("ct", "Continuous", None, Some("lead"), &[]);
        ct["continuous"] = serde_json::json!({"lead_member_id":"lead"});
        value["teams"] = serde_json::json!([ct]);
        snap = serde_json::from_value(value).unwrap();
        let boxed = app.org_blocks(&snap, &app.org_placement(&snap));
        assert!(boxed.continuations.is_empty());
        assert_eq!(boxed.members[0].sessions, vec![(0, 0), (1, 0)]);
        assert!(boxed.members.iter().all(|m| m.boxed.as_deref() == Some("ct")));
        assert_eq!(boxed.members[1].parent.as_deref(), Some("lead"));
    }

    #[test]
    fn migration_is_persisted_and_unresolved_list_is_shown_once() {
        use super::super::org_model::fixtures::{member, owner, snapshot};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        let _home = crate::test_support::home_lock();
        app.sessions_restored = true;
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["home", "stream", "other", "mixed"].into_iter().map(bare_ws).collect();
        for (wi, uid) in [(0, "main"), (1, "stream"), (2, "other"), (3, "helper-a"), (3, "helper-b")] {
            session(&mut app, wi, "unrelated", uid, "codex");
            app.workspaces[wi].sessions.last_mut().unwrap().host_id = HostId::new("h1");
        }
        app.sections = ["a", "b"].into_iter().map(|id| SidebarSection { id: id.into(), name: id.into(), ..Default::default() }).collect();
        app.workspace_sections = HashMap::from([("home".into(), "a".into()), ("stream".into(), "b".into()), ("mixed".into(), "a".into())]);
        let mut main = member("m", "M", Some("mem-owner"), None, Some("main"));
        main["assistants"] = serde_json::json!([
            {"daemon_id":"d1", "session_uid":"stream", "acting_as":"assistant"},
            {"daemon_id":"d1", "session_uid":"helper-a", "acting_as":"assistant"}]);
        let mut other = member("n", "N", Some("mem-owner"), None, Some("other"));
        other["assistants"] = serde_json::json!([{"daemon_id":"d1", "session_uid":"helper-b", "acting_as":"assistant"}]);
        let mut value = snapshot(1, vec![owner(), main, other], vec![]);
        app.org.accept(value.clone());
        app.member_section_migration_tick();
        assert!(!app.member_section_migration_done, "wait for the host identity before migrating");
        assert_eq!(app.workspace_sections.len(), 3);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        app.org.accept(value);
        app.member_section_migration_tick();
        assert!(app.member_section_migration_done);
        assert_eq!(app.member_sections.get("m").map(String::as_str), Some("a"));
        assert_eq!(app.workspace_sections.get("stream").map(String::as_str), Some("b"));
        assert!(app.fallback_only_workspace_sections.contains("home") && app.fallback_only_workspace_sections.contains("mixed"));
        assert!(!app.fallback_only_workspace_sections.contains("stream"));
        assert_eq!(app.member_section_migration_issues.len(), 1);
        assert!(matches!(&app.input_mode, InputMode::TaskPeek { lines, .. } if lines.iter().any(|line| matches!(line, PeekLine::Text(s) if s.contains("mixed")))));
        let persisted = App::load_manifest();
        assert!(persisted.member_section_migration_done);
        assert_eq!(persisted.member_section_migration_issues.len(), 1);
        app.input_mode = InputMode::Normal;
        app.member_section_migration_tick();
        assert!(app.member_section_migration_issues.is_empty());
        assert!(App::load_manifest().member_section_migration_issues.is_empty());
        assert!(matches!(app.input_mode, InputMode::Normal));
        app.org.fail(super::super::org_model::RpcFail { unsupported: false, message: "offline".into() });
        assert_eq!(app.section_of_workspace(0).as_deref(), Some("a"), "legacy choice survives offline fallback");
    }

    #[test]
    fn new_session_in_section_copies_assignment_to_member_when_stamp_arrives() {
        use super::super::org_model::fixtures::{member, owner, snapshot};
        use cm_daemon::host_id::HostId;
        let mut app = test_app();
        let _home = crate::test_support::home_lock();
        app.sessions_restored = true;
        app.member_section_migration_done = true;
        app.workspaces = vec![bare_ws("new")];
        session(&mut app, 0, "task", "main", "codex");
        app.workspaces[0].sessions[0].host_id = HostId::new("h1");
        app.sections.push(SidebarSection { id: "a".into(), name: "A".into(), ..Default::default() });
        app.workspace_sections.insert("new".into(), "a".into());
        app.pending_member_sections.insert("new".into(), "a".into());
        let mut value = snapshot(1, vec![owner(), member("m", "M", Some("mem-owner"), None, None)], vec![]);
        value["host_daemons"] = serde_json::json!({"h1":"d1"});
        app.org.accept(value.clone());
        app.member_section_migration_tick();
        assert!(app.pending_member_sections.contains_key("new"));
        value["members"][1]["main"] = serde_json::json!({"daemon_id":"d1", "session_uid":"main"});
        app.org.accept(value.clone());
        app.member_section_migration_tick();
        assert_eq!(app.member_sections.get("m").map(String::as_str), Some("a"));
        assert!(!app.pending_member_sections.contains_key("new"));
        assert_eq!(app.workspace_sections.get("new").map(String::as_str), Some("a"));
        assert!(app.fallback_only_workspace_sections.contains("new"));
        let persisted = App::load_manifest();
        assert_eq!(persisted.workspace_sections.get("new").map(String::as_str), Some("a"));
        assert!(persisted.fallback_only_workspace_sections.contains("new"));
        value["members"][1]["main"]["session_uid"] = serde_json::json!("replacement");
        app.org.accept(value);
        assert_eq!(super::super::sections::member_section(app.org_live().unwrap(), &app.member_sections, &app.sections, "m").as_deref(), Some("a"));
        app.org.fail(super::super::org_model::RpcFail { unsupported: false, message: "offline".into() });
        assert_eq!(app.section_of_workspace(0).as_deref(), Some("a"));
    }

    #[test]
    fn boxed_only_workspace_does_not_leave_an_empty_section_header() {
        use super::super::org_model::fixtures::{member, owner, snapshot, team};
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["lead", "worker", "mixed"].into_iter().map(bare_ws).collect();
        session(&mut app, 0, "lead-task", "ts-lead", "codex");
        session(&mut app, 1, "worker-task", "ts-worker", "codex");
        app.workspaces[1].sessions[0].managed_by_uid = Some("ts-lead".into());
        session(&mut app, 2, "worker-task", "ts-other-worker", "codex");
        app.workspaces[2].sessions[0].managed_by_uid = Some("ts-lead".into());
        session(&mut app, 2, "other-task", "ts-unboxed", "codex");
        app.sections.push(SidebarSection { id: "s1".into(), name: "Work".into(), ..Default::default() });
        app.workspace_sections.insert("worker".into(), "s1".into());
        app.workspace_sections.insert("mixed".into(), "s1".into());
        let mut ct = team("ct", "Continuous", None, Some("lead"), &[("lead", "primary")]);
        ct["continuous"] = serde_json::json!({"lead_member_id":"lead", "health":"ok"});
        app.org.accept(snapshot(1, vec![owner(), member("lead", "Lead", Some("mem-owner"), Some("ct"), Some("ts-lead"))], vec![ct]));
        let tree = app.task_tree();
        assert!(!tree.items.contains(&VisualItem::WorkspaceHeader(1)));
        assert!(tree.items.contains(&VisualItem::WorkspaceHeader(2)), "mixed workspace stays visible");
        for (wi, si) in [(0, 0), (1, 0), (2, 0)] {
            assert!(tree.items.contains(&VisualItem::ContinuousTeamSession(wi, si)));
            assert!(!tree.items.contains(&VisualItem::Session(wi, si)));
        }
        assert!(tree.items.contains(&VisualItem::Session(2, 1)));
        let buffer = sidebar_buffer(&mut app, 24);
        let rendered = (0..buffer.area.height).map(|y| {
            (0..buffer.area.width).map(|x| buffer[(x, y)].symbol()).collect::<String>()
        }).collect::<Vec<_>>().join("\n");
        assert!(rendered.contains("⟳"), "{rendered}");
        println!("{rendered}");
    }

    /// A session of `engine` on task `tid`, live, with a well-formed uid.
    fn session(app: &mut App, wi: usize, tid: &str, uid: &str, engine: &str) {
        let pty =
            crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None)
                .unwrap();
        let mut ts = super::super::model::make_simple_session_with_uid(
            uid.to_string(),
            uid,
            engine,
            pty,
            None,
        );
        ts.task_id = Some(tid.into());
        ts.session.exited = false;
        app.workspaces[wi].sessions.push(ts);
    }

    /// r: root task tr, plus tr2 (an inherit-mode subtask of tr in r's
    /// checkout). c: tc under tr. g: tg under tc. o: to under tx, a task with
    /// no visible block, whose parent is tr. l: loose.
    fn org_app() -> App {
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["r", "c", "g", "o", "l"].into_iter().map(bare_ws).collect();
        app.tasks = vec![
            task("tr", "r", None),
            task("tr2", "r", Some("tr")),
            task("tc", "c", Some("tr")),
            task("tg", "g", Some("tc")),
            task("tx", "gone", Some("tr")),
            task("to", "o", Some("tx")),
            task("tl", "l", None),
        ];
        for (wi, t, uid) in [
            (0, "tr", "ts-10-0"),
            (0, "tr2", "ts-11-0"),
            (1, "tc", "ts-12-0"),
            (2, "tg", "ts-13-0"),
            (3, "to", "ts-14-0"),
            (4, "tl", "ts-15-0"),
        ] {
            session(&mut app, wi, t, uid, "codex");
        }
        app
    }

    /// `<depth>name`: workspace headers as `ws:<id>`, sessions by uid,
    /// with their gutter depth (cells).
    fn outline(app: &App) -> Vec<String> {
        let tree = app.task_tree();
        tree.items
            .iter()
            .filter_map(|vi| {
                let label = match vi {
                    VisualItem::WorkspaceHeader(wi) => format!("ws:{}", app.workspaces[*wi].id),
                    VisualItem::Session(wi, si) => app.workspaces[*wi].sessions[*si].uid.clone(),
                    VisualItem::DivisionHeader(key) => format!("division:{}", key.id),
                    _ => return None,
                };
                let row = &tree.rows[vi];
                Some(format!("{}{label}", ">".repeat(row.cells.len())))
            })
            .collect()
    }

    /// Owner, 2026-11-23: a workspace whose every session is lifted into a
    /// continuous team box must not stay behind as an empty header (the
    /// duplicated LEP-*/LPP5 rows). A workspace that keeps any session, and
    /// blocks nested under the hidden one, still render.
    #[test]
    fn fully_boxed_workspace_leaves_no_empty_header() {
        use super::super::org_model::fixtures::{member, owner, snapshot, team};
        let mut app = org_app();
        // Lead = ts-10-0 (in r); c's only session is its worker.
        app.workspaces[1].sessions[0].managed_by_uid = Some("ts-10-0".into());
        let mut t = team("t-lat", "Latency", None, Some("m-lead"), &[("m-lead", "primary")]);
        t["continuous"] = serde_json::json!({"lead_member_id": "m-lead", "health": "ok"});
        app.org.accept(snapshot(
            1,
            vec![owner(), member("m-lead", "lead", Some("mem-owner"), Some("t-lat"), Some("ts-10-0"))],
            vec![t],
        ));
        let lines = outline(&app);
        assert!(!lines.iter().any(|l| l.ends_with("ws:c")), "fully boxed c must not draw: {lines:?}");
        assert!(lines.iter().any(|l| l.ends_with("ws:r")), "r keeps ts-11-0: {lines:?}");
        assert!(lines.iter().any(|l| l.ends_with("ws:g")), "g still renders: {lines:?}");
        assert!(!lines.iter().any(|l| l.ends_with("ts-12-0") || l.ends_with("ts-10-0")), "boxed sessions only in the box: {lines:?}");
        let tree = app.task_tree();
        let boxed: Vec<_> = tree.items.iter().filter(|v| matches!(v, VisualItem::ContinuousTeamSession(..))).collect();
        assert_eq!(boxed.len(), 2, "lead + worker in the box");
    }

    /// Owner, 2026-11-05: no task subheaders. A workspace shows its
    /// sessions directly under it, an inherit-mode subtask's included, and
    /// blocks still nest by the task tree.
    #[test]
    fn blocks_nest_under_their_parent_task_at_any_depth() {
        let app = org_app();
        assert_eq!(
            outline(&app),
            [
                "ws:r",
                "ts-10-0",
                // Inherit-mode subtask tr2: same checkout, same level.
                "ts-11-0",
                // c under tr, g under tc: depth follows the task tree.
                ">ws:c",
                ">ts-12-0",
                ">>ws:g",
                ">>ts-13-0",
                // tx has no block: o hangs under tx's nearest visible ancestor.
                ">ws:o",
                ">ts-14-0",
                "ws:l",
                "ts-15-0",
            ],
        );
        let tree = app.task_tree();
        assert_eq!(tree.block_root[&2], 0, "g renders under r's block");
        assert_eq!(
            tree.chain(&Node::Workspace(2)),
            [Node::Workspace(2), Node::Workspace(1), Node::Workspace(0)],
        );
        assert_eq!(tree.roots[&0], ["tr"], "r stands for tr, not its subtask tr2");
        assert_eq!(app.workspace_task(0).as_deref(), Some("tr"));
        assert_eq!(app.workspace_task(2).as_deref(), Some("tg"));
    }

    /// Owner, 2026-10-21, now on the workspace header: a fold hides the
    /// blocks nested under it only. Its own sessions stay, and the rollup
    /// counts just the hidden subtree.
    #[test]
    fn a_folded_workspace_hides_its_nested_blocks_only_and_rolls_them_up_until_unfolded() {
        let mut app = org_app();
        app.cursor = Cursor::Workspace(1);
        let space = Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.handle_event(&space));
        assert!(app.folded_workspaces.contains("c"));
        let outline_folded = outline(&app);
        for kept in ["ws:r", ">ws:c", ">ts-12-0"] {
            assert!(
                outline_folded.contains(&kept.to_string()),
                "{kept}: the header, its parent and its own session stay"
            );
        }
        for hidden in [">>ws:g", ">>ts-13-0"] {
            assert!(
                !outline_folded.contains(&hidden.to_string()),
                "{hidden} folds away"
            );
        }
        let tree = app.task_tree();
        assert_eq!(tree.folded[&Node::Workspace(1)].0, 1, "only g's session is hidden");
        // Filtering ignores folds, so a match is never hidden.
        app.sidebar_filter = Some("ts-13".into());
        assert!(app
            .visual_items()
            .iter()
            .any(|vi| matches!(vi, VisualItem::Session(2, _))));
        app.sidebar_filter = None;
        assert!(app.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        ))));
        assert!(!app.folded_workspaces.contains("c"));
        assert_eq!(outline(&app), outline(&org_app()));
        // reveal_session unfolds every header above a session's workspace;
        // the workspace's own fold never hid it, so it stays.
        app.folded_workspaces
            .extend(["r", "c", "g", "l"].map(String::from));
        assert!(app.reveal_session("ts-13-0"));
        assert_eq!(app.folded_workspaces, HashSet::from(["g".to_string(), "l".to_string()]));
        assert!(outline(&app).contains(&">>ts-13-0".to_string()));
        assert!(!app.reveal_session("ts-13-0"), "nothing left to unfold");
        // A workspace without nested blocks has nothing to fold.
        app.folded_workspaces.clear();
        app.cursor = Cursor::Workspace(4);
        assert!(app.handle_event(&space));
        assert!(app.folded_workspaces.is_empty(), "a leaf does not fold");
        assert!(!app.task_tree().foldable.contains(&4));
        // Folding r hides every block below it; tr2's session lives in r's
        // own checkout, so it stays.
        app.cursor = Cursor::Workspace(0);
        assert!(app.handle_event(&space));
        assert_eq!(outline(&app), ["ws:r", "ts-10-0", "ts-11-0", "ws:l", "ts-15-0"]);
        assert_eq!(app.task_tree().folded[&Node::Workspace(0)].0, 3);
    }

    /// Owner, 2026-10-21 (longstanding): a task-feed refresh re-sorts the
    /// workspaces. The cursor stays on the same workspace header.
    #[test]
    fn a_workspace_header_keeps_the_cursor_through_a_task_refresh() {
        let mut app = org_app();
        let api = |id: &str, parent: Option<&str>| -> crate::api::Task {
            serde_json::from_value(serde_json::json!({
                "id": id, "created_at": "2026-10-21T00:00:00Z", "repo_url": "",
                "repo_branch": "main", "name": id, "status": "running",
                "parent_task_id": parent,
            }))
            .unwrap()
        };
        let feed = vec![
            api("tr", None),
            api("tr2", Some("tr")),
            api("tc", Some("tr")),
            api("tg", Some("tc")),
            api("to", Some("tx")),
            api("tl", None),
        ];
        // Move c to the end first, so the re-sort really moves it.
        let c = app.workspaces.remove(1);
        app.workspaces.push(c);
        app.cursor = Cursor::Workspace(4);
        for _ in 0..3 {
            app.reconcile_tasks(feed.clone());
            let Cursor::Workspace(wi) = &app.cursor else {
                panic!("cursor left the workspace header: {:?}", app.cursor);
            };
            assert_eq!(app.workspaces[*wi].id, "c");
        }
        // A workspace whose task only its sessions know (not in the feed
        // yet) holds the cursor too: its header is a tree row.
        app.tasks.retain(|t| t.task_id.as_deref() != Some("tl"));
        let wl = app.workspaces.iter().position(|w| w.id == "l").unwrap();
        app.cursor = Cursor::Workspace(wl);
        app.clamp_cursor();
        assert_eq!(app.cursor, Cursor::Workspace(wl));
        assert_eq!(app.workspace_task(wl).as_deref(), Some("tl"));
    }

    /// Owner, 2026-10-21: only the lead of a task with subtasks carries a
    /// leader mark, by level below Owner (★ ☆ ✦ ✧, then ✧ again), and the
    /// F6 org chart uses the same marks.
    #[test]
    fn only_leads_with_reports_carry_their_levels_mark_in_both_views() {
        assert_eq!(
            (1..=6).map(leader_glyph).collect::<Vec<_>>(),
            ["\u{2605}", "\u{2606}", "\u{2726}", "\u{2727}", "\u{2727}", "\u{2727}"]
        );
        assert_eq!(leader_glyph(0), "\u{2605}");
        assert!(!LEADER_GLYPHS.contains(&"\u{25c6}"), "◆ marks divisions");
        let mut app = org_app();
        // A deeper chain: tg → tz (level 4) → ty (level 5).
        app.workspaces.push(bare_ws("z"));
        app.workspaces.push(bare_ws("y"));
        app.tasks.push(task("tz", "z", Some("tg")));
        app.tasks.push(task("ty", "y", Some("tz")));
        session(&mut app, 5, "tz", "ts-16-0", "codex");
        session(&mut app, 6, "ty", "ts-17-0", "codex");
        let org = app.org_index();
        assert_eq!(
            ["tr", "tr2", "tc", "tg", "tx", "to", "tz", "ty", "tl"].map(|t| org.level(t)),
            [1, 2, 2, 3, 2, 3, 4, 5, 1]
        );
        let marks = app.leader_marks(&app.task_tree().org);
        let expected = HashMap::from([
            ("ts-10-0".to_string(), "\u{2605}"), // tr: tr2, c and o report to it
            ("ts-12-0".to_string(), "\u{2606}"), // tc: g
            ("ts-13-0".to_string(), "\u{2726}"), // tg: z
            ("ts-16-0".to_string(), "\u{2727}"), // tz: y
        ]);
        assert_eq!(marks, expected, "leaves (tr2, to, ty, tl) carry none");
        assert_eq!(chart_marks(&app), expected, "the org chart agrees");
    }

    /// Marks the F6 chart draws, by session uid (bash shown or not).
    fn chart_marks(app: &App) -> HashMap<String, &'static str> {
        let mut out = HashMap::new();
        for show_bash in [false, true] {
            for node in app.org_model(show_bash).nodes {
                for s in node.sessions {
                    if let Some(g) = s.mark {
                        assert_eq!(*out.entry(s.uid.clone()).or_insert(g), g, "{}", s.uid);
                    }
                }
            }
        }
        out
    }

    /// Marks the sidebar draws, by session uid, read from the render.
    fn sidebar_marks(app: &mut App) -> HashMap<String, &'static str> {
        let buffer = sidebar_buffer(app, 40);
        let mut out = HashMap::new();
        for y in 0..40u16 {
            let row: String = (0..40u16).map(|x| buffer[(x, y)].symbol().to_string()).collect();
            for g in LEADER_GLYPHS {
                if let Some(at) = row.find(&format!("{g} ts-")) {
                    let uid: String = row[at + g.len() + 1..].split_whitespace().next().unwrap().into();
                    out.insert(uid, g);
                }
            }
        }
        out
    }

    /// Review of f385d1b (1, 2): a report is a LIVE subtask with a live
    /// agent in its subtree. Sessionless, bash-only and done subtasks don't
    /// count, a lead that exits hands the mark on, and the sidebar (mark and
    /// fold glyph) and the F6 chart agree on every case.
    #[test]
    fn only_live_reports_count_and_both_views_agree() {
        // tc's only report is g (tg, one codex session).
        let base = || {
            let mut app = org_app();
            app.cursor = Cursor::Workspace(4);
            app
        };
        let fold_glyph = |app: &mut App, t: &str| -> bool {
            let buffer = sidebar_buffer(app, 40);
            (0..40u16).any(|y| {
                let row: String = (0..40u16).map(|x| buffer[(x, y)].symbol().to_string()).collect();
                row.contains(&format!("\u{25be} {t}")) || row.contains(&format!("\u{25b8} {t}"))
            })
        };
        let mut app = base();
        assert_eq!(sidebar_marks(&mut app).get("ts-12-0"), Some(&"\u{2606}"));
        assert!(fold_glyph(&mut app, "c"), "c's header folds: tc has a live report");

        // Sessionless: g's workspace stays, its session is gone.
        let mut app = base();
        app.workspaces[2].sessions.clear();
        // Bash-only.
        let mut bash = base();
        bash.workspaces[2].sessions.clear();
        session(&mut bash, 2, "tg", "ts-13-0", "bash");
        // Only an exited agent.
        let mut exited = base();
        exited.workspaces[2].sessions[0].session.exited = true;
        // Done, kept on disk: by the planning feed, or by the work list.
        let mut done = base();
        done.sidebar_done_task_ids.insert("tg".into());
        let mut done_entry = base();
        done_entry.tasks.iter_mut().find(|t| t.task_id.as_deref() == Some("tg")).unwrap().api_status =
            TaskStatus::Done;
        for (case, app) in [
            ("sessionless", &mut app),
            ("bash-only", &mut bash),
            ("exited", &mut exited),
            ("done", &mut done),
            ("done entry", &mut done_entry),
        ] {
            assert!(!app.org_index().has_reports("tc"), "{case}");
            let sidebar = sidebar_marks(app);
            assert!(!sidebar.contains_key("ts-12-0"), "{case}: no mark on tc's lead");
            assert!(!fold_glyph(app, "c"), "{case}: c does not fold");
            assert_eq!(chart_marks(app), sidebar, "{case}: the chart agrees");
            // tr still has live reports (o's to): its mark stays.
            assert_eq!(sidebar.get("ts-10-0"), Some(&"\u{2605}"), "{case}");
        }

        // tr's lead exits: the mark moves to its next live agent, in both
        // views; with no live agent left, neither view marks tr.
        let mut app = base();
        session(&mut app, 0, "tr", "ts-30-0", "claude");
        app.workspaces[0].sessions[0].session.exited = true; // ts-10-0
        let sidebar = sidebar_marks(&mut app);
        assert_eq!(sidebar.get("ts-30-0"), Some(&"\u{2605}"));
        assert!(!sidebar.contains_key("ts-10-0"));
        assert_eq!(chart_marks(&app), sidebar);
        let last = app.workspaces[0].sessions.len() - 1;
        app.workspaces[0].sessions[last].session.exited = true;
        let sidebar = sidebar_marks(&mut app);
        assert!(!sidebar.contains_key("ts-10-0") && !sidebar.contains_key("ts-30-0"));
        assert_eq!(chart_marks(&app), sidebar);
    }

    /// #77: duplicate task bindings on two hosts must not duplicate children,
    /// and each workspace header owns its own fold and reveal chain.
    #[test]
    fn shared_task_routes_children_by_manager_and_host_and_folds_independently() {
        let mut app = org_app();
        app.workspaces.push(bare_ws("c2"));
        app.workspaces[5].host_id = crate::hosts::HostId("remote".into());
        session(&mut app, 5, "tc", "ts-40-0", "codex");
        app.workspaces[5].sessions[0].host_id = crate::hosts::HostId("remote".into());
        app.workspaces.push(bare_ws("g2"));
        app.workspaces[6].host_id = crate::hosts::HostId("remote".into());
        app.tasks.push(task("tg2", "g2", Some("tc")));
        session(&mut app, 6, "tg2", "ts-41-0", "codex");
        app.workspaces[6].sessions[0].host_id = crate::hosts::HostId("remote".into());
        app.workspaces[6].sessions[0].managed_by_uid = Some("ts-40-0".into());
        let tree = app.task_tree();
        assert_eq!(tree.parents[&Node::Workspace(2)], Node::Workspace(1));
        assert_eq!(tree.parents[&Node::Workspace(6)], Node::Workspace(5));
        for wi in [2, 6] {
            assert_eq!(tree.items.iter().filter(|v| **v == VisualItem::Session(wi, 0)).count(), 1);
        }
        app.cursor = Cursor::Workspace(5);
        assert!(app.toggle_tree_fold());
        assert!(!shown(&app, "ts-41-0"));
        assert!(shown(&app, "ts-13-0"));
        assert!(app.reveal_session("ts-41-0"));
        assert!(shown(&app, "ts-41-0"));
        app.cursor = Cursor::Workspace(1);
        assert!(app.toggle_tree_fold());
        assert!(!shown(&app, "ts-13-0"));
        assert!(shown(&app, "ts-41-0"));
    }

    #[test]
    fn attachment_prefers_manager_then_falls_back_to_the_child_host() {
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = ["first", "manager", "child", "foreign"]
            .into_iter().map(bare_ws).collect();
        app.workspaces[3].host_id = crate::hosts::HostId("foreign".into());
        app.tasks = vec![task("parent", "first", None), task("child", "child", Some("parent"))];
        session(&mut app, 0, "parent", "ts-1-0", "codex");
        session(&mut app, 1, "parent", "ts-2-0", "codex");
        session(&mut app, 2, "child", "ts-3-0", "codex");
        session(&mut app, 3, "parent", "ts-4-0", "codex");
        app.workspaces[2].sessions[0].managed_by_uid = Some("ts-2-0".into());
        assert_eq!(app.task_tree().parents[&Node::Workspace(2)], Node::Workspace(1));
        // A stale manager must fall back to this host, not the foreign one.
        app.workspaces[2].sessions[0].managed_by_uid = Some("missing".into());
        assert_eq!(app.task_tree().parents[&Node::Workspace(2)], Node::Workspace(0));
        // If no parent is on the child's host, leave it top-level.
        app.workspaces[0].host_id = crate::hosts::HostId("foreign".into());
        app.workspaces[1].host_id = crate::hosts::HostId("foreign".into());
        assert!(!app.task_tree().parents.contains_key(&Node::Workspace(2)));
    }

    /// Task folds saved before the task subheaders went (`[workspace,
    /// task]` pairs) fold that workspace; workspace ids pass through; keys
    /// naming no known workspace are dropped.
    #[test]
    fn saved_task_folds_migrate_to_their_workspace() {
        let workspaces = ["c".to_string(), "c2".to_string()];
        let folds = std::collections::BTreeSet::from([
            serde_json::to_string(&["c", "tc"]).unwrap(),
            "c2".to_string(),
            "tc".to_string(),
            serde_json::to_string(&["gone", "tg"]).unwrap(),
        ]);
        let migrated = migrate_workspace_folds(&folds, workspaces.iter());
        assert_eq!(migrated, HashSet::from(["c".to_string(), "c2".to_string()]));
        let saved = migrated.iter().cloned().collect();
        assert_eq!(migrate_workspace_folds(&saved, workspaces.iter()), migrated, "stable");
    }

    /// Owner-approved 2026-10-20, now for the workspace header: in the
    /// Task sub-view it is a tree row, so even with a single session under
    /// it no key reaches that session's terminal. The flat Status view
    /// keeps its old single-session shortcut.
    #[test]
    fn a_workspace_header_never_stands_in_for_its_session() {
        let mut app = org_app();
        app.cursor = Cursor::Workspace(4);
        assert!(app.active_session().is_none());
        assert!(app.active_session_mut().is_none());
        app.sidebar_view = SidebarView::Status;
        assert!(app.active_session().is_some());
    }

    #[test]
    fn fold_state_rides_the_manifest() {
        let mut app = org_app();
        app.folded_workspaces.insert("c".into());
        app.folded_divisions.insert("div-1".into());
        let manifest: cm_daemon::manifest::Manifest = serde_json::from_value(serde_json::json!({
            "folded_tasks": ["tc"], "folded_divisions": ["div-1"],
        }))
        .unwrap();
        assert!(
            manifest.folded_tasks.contains("tc") && manifest.folded_divisions.contains("div-1")
        );
        let json = serde_json::to_value(&cm_daemon::manifest::Manifest::default()).unwrap();
        assert!(
            json.get("folded_tasks").is_none(),
            "empty sets stay out of the file"
        );
    }

    fn division_task(id: &str, parent: Option<&str>, coordinator: bool) -> crate::api::Task {
        serde_json::from_value(serde_json::json!({
            "id": id, "slug": id, "created_at": "2026-10-20T00:00:00Z", "repo_url": "", "repo_branch": "main",
            "status": "running", "project": "p", "parent_task_id": parent,
            "initiative_id": "div-1",
            "initiative": {"id": "div-1", "name": "Trader", "kind": "division",
                           "coordinator_task_id": if coordinator { id } else { "tr" }},
        }))
        .unwrap()
    }

    fn owner_division_app() -> App {
        let mut app = org_app();
        app.planning.update_from_api(vec![
            division_task("tr", None, true),
            division_task("tc", Some("tr"), false),
            division_task("tl", None, false),
        ]);
        app
    }

    /// Owner as leader (2026-10-22): the TUI's leads are the daemon's
    /// `task_lead` over the same inputs. Explicit Owner, the stand-in for a
    /// division root only, and a live session overriding the stand-in.
    #[test]
    fn owner_leads_match_the_shared_rule() {
        let mut app = owner_division_app();
        let rows: HashMap<&str, serde_json::Value> = [
            ("tr", serde_json::json!({"id": "tr", "initiative": {"kind": "division", "coordinator_task_id": "tr"}})),
            ("tc", serde_json::json!({"id": "tc", "initiative": {"kind": "division", "coordinator_task_id": "tr"}})),
            ("tl", serde_json::json!({"id": "tl", "initiative": {"kind": "division", "coordinator_task_id": "tr"}})),
            ("tg", serde_json::json!({"id": "tg"})),
        ]
        .into();
        let check = |app: &App, why: &str| {
            let leads = app.all_leads();
            for (t, row) in &rows {
                let owned: Vec<(String, String, bool)> = app
                    .workspaces
                    .iter()
                    .flat_map(|w| &w.sessions)
                    .filter(|s| s.task_id.as_deref() == Some(*t))
                    .map(|s| (s.uid.clone(), s.session_type.clone(), !s.session.exited))
                    .collect();
                let candidates: Vec<LeadCandidate> =
                    owned.iter().map(|(uid, engine, live)| LeadCandidate { uid, engine, live: *live }).collect();
                let pin = app.find_task_entry(t).and_then(|e| e.metadata.as_ref()).and_then(pinned_lead).map(str::to_owned);
                let want = task_lead(pin.as_deref(), &candidates, cm_daemon::org::is_division_root(row));
                assert_eq!(leads.get(*t).cloned().unwrap_or(Lead::None), want, "{why}: {t}");
            }
        };
        check(&app, "every task has a live session");
        assert!(app.owner_leads().is_empty());
        // The root loses its session: Owner stands in; a member doesn't.
        app.workspaces[0].sessions.retain(|s| s.uid != "ts-10-0");
        app.workspaces[4].sessions.clear(); // tl
        check(&app, "root and tl without a session");
        assert_eq!(app.owner_leads(), HashMap::from([("tr".to_string(), true)]));
        assert!(!app.all_leads().contains_key("tl"), "a non-root task with no lead has none");
        // A live session overrides the stand-in again.
        let pty = crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None).unwrap();
        let mut ts = super::super::model::make_simple_session_with_uid("ts-30-0".into(), "ts-30-0", "claude", pty, None);
        ts.task_id = Some("tr".into());
        ts.session.exited = false;
        app.workspaces[0].sessions.push(ts);
        assert_eq!(app.all_leads()["tr"], Lead::Session("ts-30-0".into()));
        // Explicit Owner on tc: Owner leads even with tc's live session, and
        // that session loses its ★ (sessions only lead when Owner doesn't).
        app.set_local_lead_pin("tc", Some("owner".into()));
        check(&app, "tc pinned to Owner");
        assert_eq!(app.all_leads()["tc"], Lead::Owner { stand_in: false });
        assert!(!app.task_leads().contains_key("tc"));
        assert!(!app.leader_marks(&app.task_tree().org).contains_key("ts-12-0"));
        assert_eq!(app.leader_marks(&app.task_tree().org).get("ts-30-0"), Some(&"\u{2605}"), "tr's lead keeps its mark");
    }

    /// Review of 3c8d674: a work-list row without metadata doesn't hide the
    /// planning row's Owner pin.
    #[test]
    fn a_planning_pin_shows_when_the_work_list_row_has_no_metadata() {
        let mut app = org_app();
        let mut pinned = division_task("tc", Some("tr"), false);
        pinned.metadata = Some(serde_json::json!({"lead_session_uid": "owner"}));
        app.planning.update_from_api(vec![division_task("tr", None, true), pinned]);
        assert!(app.find_task_entry("tc").unwrap().metadata.is_none());
        assert_eq!(app.all_leads()["tc"], Lead::Owner { stand_in: false });
        // Loaded work-list metadata wins, both ways.
        app.set_local_lead_pin("tc", None);
        assert_eq!(app.all_leads()["tc"], Lead::Session("ts-12-0".into()));
    }

    /// The A-e Lead field (on the workspace header, for its task) offers
    /// Owner first after auto, and saving it pins "owner".
    #[test]
    fn the_lead_field_offers_owner() {
        let mut app = owner_division_app();
        let (choices, pinned) = app.lead_choices("tc");
        assert_eq!(choices[0], ("owner".to_string(), "Owner".to_string()));
        assert_eq!(pinned, None);
        app.cursor = Cursor::Workspace(1);
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT))));
        for _ in 0..5 {
            app.handle_event(&key(KeyCode::Tab));
        }
        app.handle_event(&key(KeyCode::Right));
        let InputMode::WorkspaceSettings { tasks, task_at, active_field, .. } = &app.input_mode else { panic!("open") };
        assert_eq!(*active_field, 5);
        assert_eq!(tasks[*task_at].task_id, "tc");
        assert_eq!(tasks[*task_at].lead.as_deref(), Some("owner"));
        app.handle_event(&key(KeyCode::Enter));
        let meta = app.find_task_entry("tc").and_then(|t| t.metadata.as_ref()).unwrap();
        assert_eq!(pinned_lead(meta), Some("owner"));
        assert_eq!(app.lead_choices("tc").1.as_deref(), Some("owner"));
    }

    /// The sidebar: an Owner-led division and workspace header (for its
    /// task) carry the level's mark with ♔.
    #[test]
    fn owner_led_headers_carry_the_level_mark_with_the_owner_crown() {
        let mut app = owner_division_app();
        app.workspaces[0].sessions.retain(|s| s.uid != "ts-10-0"); // tr: stand-in
        app.set_local_lead_pin("tc", Some("owner".into()));
        let buffer = sidebar_buffer(&mut app, 30);
        let rows: Vec<String> = (0..30u16).map(|y| (0..40u16).map(|x| buffer[(x, y)].symbol().to_string()).collect()).collect();
        let joined = rows.join("\n");
        assert!(joined.contains("\u{25c6} \u{2605}\u{2654} Trader"), "division header: {joined}");
        assert!(rows.iter().any(|r| r.contains("\u{25be} \u{2605}\u{2654} \u{25c6} r")), "r, tr's workspace (level 1, coordinator ◆): {joined}");
        assert!(rows.iter().any(|r| r.contains("\u{25be} \u{2606}\u{2654} c")), "c, tc's workspace (level 2): {joined}");
        assert!(!rows.iter().any(|r| r.contains("\u{2654} l")), "tl is led by its session: {joined}");
    }

    #[test]
    fn a_division_header_groups_its_top_level_blocks_and_folds() {
        let mut app = org_app();
        // tl joins the division as its own top-level block; tr is the root.
        app.planning.update_from_api(vec![
            division_task("tr", None, true),
            division_task("tl", None, false),
        ]);
        assert_eq!(app.planning.division("div-1").unwrap().name, "Trader");
        let outline = outline(&app);
        assert_eq!(outline[0], "division:div-1");
        assert_eq!(
            outline[1], ">ws:r",
            "the coordinator's block comes first, inside the division"
        );
        assert!(outline.contains(&">ws:l".to_string()));
        assert!(
            outline.contains(&">>>ws:g".to_string()),
            "nesting continues inside the division"
        );
        app.cursor = Cursor::Division(loose_division("div-1"));
        assert!(app.toggle_tree_fold());
        let tree = app.task_tree();
        assert_eq!(tree.items.len(), 1, "a folded division is its header alone");
        assert_eq!(tree.folded[&Node::Division(loose_division("div-1"))].0, 6);
        // An ordinary initiative draws no header.
        let mut app = org_app();
        let mut plain = division_task("tr", None, true);
        plain.initiative.as_mut().unwrap()["kind"] = serde_json::json!("initiative");
        app.planning.update_from_api(vec![plain]);
        assert_eq!(super::super::tree::tree_tests::outline(&app)[0], "ws:r");
    }

    fn loose_division(id: &str) -> DivisionKey {
        DivisionKey { id: id.into(), section: None }
    }

    /// Laptop diag 2026-11-05: a division with top-level blocks both in a
    /// section and in the loose list drew two headers with one identity, so
    /// the cursor resolved to the first one and A-k (or A-j from above) on
    /// the lower block jumped to the top. Each header is its own row now:
    /// every row is drawn once, A-j/A-k walk the list in order, and each
    /// header folds on its own.
    #[test]
    fn a_division_split_across_sections_draws_distinct_rows_and_navigates_linearly() {
        let mut app = org_app();
        app.planning.update_from_api(vec![
            division_task("tr", None, true),
            division_task("tl", None, false),
        ]);
        app.sections.push(cm_daemon::manifest::SidebarSection {
            id: "s1".into(),
            name: "SEJD".into(),
            color: None,
            folded: false,
        });
        app.workspace_sections.insert("r".into(), "s1".into());
        let items = app.task_tree().items;
        let upper = DivisionKey { id: "div-1".into(), section: Some("s1".into()) };
        let lower = loose_division("div-1");
        let pos = |k: &DivisionKey| {
            items.iter().position(|vi| *vi == VisualItem::DivisionHeader(k.clone())).unwrap()
        };
        assert!(pos(&upper) < pos(&lower), "{items:?}");
        let mut drawn = HashSet::new();
        for vi in items.iter().filter(|vi| !matches!(vi, VisualItem::Separator | VisualItem::SectionEnd)) {
            assert!(drawn.insert(vi.clone()), "{vi:?} drawn twice: {items:?}");
        }

        // A-j from the top visits every stop once, in order, then wraps.
        app.cursor = Cursor::Section("s1".into());
        let mut visited = vec![app.sidebar_cursor_index(&items).unwrap()];
        loop {
            app.navigate(1);
            let at = app.sidebar_cursor_index(&items).unwrap();
            if at <= *visited.last().unwrap() {
                assert_eq!(at, 0, "wraps to the top only after the last row");
                break;
            }
            visited.push(at);
        }
        assert!(visited.contains(&pos(&upper)) && visited.contains(&pos(&lower)), "{visited:?}");
        // A-k from the lower header goes to the row just above it.
        app.cursor = Cursor::Division(lower.clone());
        app.navigate(-1);
        let above = visited[visited.iter().position(|p| *p == pos(&lower)).unwrap() - 1];
        assert_eq!(app.sidebar_cursor_index(&items), Some(above));
        assert!(above > pos(&upper));

        // Folding the lower header folds it alone and keeps the cursor there.
        app.cursor = Cursor::Division(lower.clone());
        assert!(app.toggle_tree_fold());
        let tree = app.task_tree();
        assert!(tree.folded.contains_key(&Node::Division(lower.clone())));
        assert!(!tree.folded.contains_key(&Node::Division(upper.clone())));
        assert!(tree.items.contains(&VisualItem::Session(0, 0)), "the upper division stays open");
        assert_eq!(
            app.sidebar_cursor_index(&tree.items),
            tree.items.iter().position(|vi| *vi == VisualItem::DivisionHeader(lower.clone()))
        );
        // A header that stops drawing falls back to the division's other one.
        app.workspace_sections.insert("l".into(), "s1".into());
        app.clamp_cursor();
        assert_eq!(app.cursor, Cursor::Division(upper));
    }

    /// A-e on a workspace header: the Lead field of its task cycles auto →
    /// Owner → each live agent session, and a save pins it (the ★ moves at
    /// once; the API call goes through the backend thread).
    #[test]
    fn the_lead_field_pins_another_session() {
        let mut app = org_app();
        session(&mut app, 1, "tc", "ts-20-0", "claude");
        session(&mut app, 1, "tc", "ts-05-0", "bash");
        app.tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some("tc"))
            .unwrap()
            .workspace_id = Some("c".into());
        assert_eq!(app.task_leads()["tc"], "ts-12-0");
        app.cursor = Cursor::Workspace(1);
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(app.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('e'),
            KeyModifiers::ALT
        ))));
        let InputMode::WorkspaceSettings { tasks, .. } = &app.input_mode else {
            panic!(
                "workspace settings, got {:?}",
                std::mem::discriminant(&app.input_mode)
            );
        };
        assert_eq!(tasks.len(), 1, "c's one task");
        assert_eq!(tasks[0].lead, None, "no pin yet: auto");
        let uids: Vec<&str> = tasks[0].lead_choices.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(
            uids,
            ["owner", "ts-12-0", "ts-20-0"],
            "Owner, then live agents only, in spawn order"
        );
        for _ in 0..5 {
            app.handle_event(&key(KeyCode::Tab));
        }
        app.handle_event(&key(KeyCode::Left));
        let InputMode::WorkspaceSettings { tasks, .. } = &app.input_mode else {
            panic!("still open")
        };
        assert_eq!(
            tasks[0].lead.as_deref(),
            Some("ts-20-0"),
            "← wraps from auto to the last choice"
        );
        app.handle_event(&key(KeyCode::Enter));
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert_eq!(app.task_leads()["tc"], "ts-20-0");
        let meta = app
            .find_task_entry("tc")
            .and_then(|t| t.metadata.as_ref())
            .unwrap();
        assert_eq!(pinned_lead(meta), Some("ts-20-0"));
    }

    fn shown(app: &App, uid: &str) -> bool {
        app.visual_items().iter().any(|vi| matches!(vi, VisualItem::Session(wi, si) if app.workspaces[*wi].sessions[*si].uid == uid))
    }

    /// Review (1): any path that lands the cursor inside a fold (a jump,
    /// spawn, fork or restore) unfolds its ancestors; the next A-j then
    /// moves from that row instead of the top.
    #[test]
    fn a_cursor_landing_inside_a_fold_unfolds_it() {
        let mut app = org_app();
        app.folded_workspaces.extend(["r", "c", "l"].map(String::from));
        assert!(!shown(&app, "ts-13-0"));
        // The A-p / A-; / org chart jump.
        assert!(app.jump_cursor_to_session_uid("ts-13-0"));
        assert!(shown(&app, "ts-13-0"));
        assert_eq!(
            app.folded_workspaces,
            HashSet::from(["l".to_string()]),
            "only the folds above it open"
        );
        // A spawn / restore that sets the cursor directly: the next draw
        // reveals it, and A-j moves on from there.
        app.folded_workspaces.insert("r".into());
        app.cursor = Cursor::Session(3, 0); // ts-14-0, under r
        sidebar_buffer(&mut app, 30);
        app.navigate(1);
        assert!(!app.folded_workspaces.contains("r"));
        assert_eq!(
            app.cursor,
            Cursor::Workspace(4),
            "A-j moved on from ts-14-0 to l's header, not from the top"
        );
        app.folded_workspaces.insert("r".into());
        app.cursor = Cursor::Session(2, 0);
        sidebar_buffer(&mut app, 30);
        assert!(shown(&app, "ts-13-0"), "the draw reveals it too");
        // A folded section around the root block opens too, and a filter
        // that excludes the row is dropped.
        app.sections.push(cm_daemon::manifest::SidebarSection {
            id: "s1".into(),
            name: "Project".into(),
            color: None,
            folded: true,
        });
        app.workspace_sections.insert("r".into(), "s1".into());
        app.sidebar_filter = Some("ts-15".into());
        assert!(!shown(&app, "ts-13-0"));
        assert!(app.reveal_session("ts-13-0"));
        assert!(!app.sections[0].folded && app.sidebar_filter.is_none());
        assert!(shown(&app, "ts-13-0"));
        // A filter that already matches stays.
        app.sidebar_filter = Some("ts-13".into());
        assert!(!app.reveal_session("ts-13-0"));
        assert_eq!(app.sidebar_filter.as_deref(), Some("ts-13"));
    }

    /// Review follow-up: an unmoved cursor never fights Owner. A filter
    /// typed while the cursor sits on a row it excludes stays, through
    /// draws and A-j; a header folded around the active session stays folded.
    #[test]
    fn an_unmoved_cursor_never_undoes_a_filter_or_a_fold() {
        let mut app = org_app();
        app.cursor = Cursor::Session(2, 0); // ts-13-0 under tc
        sidebar_buffer(&mut app, 30);
        app.sidebar_filter = Some("ts-15".into());
        sidebar_buffer(&mut app, 30);
        assert_eq!(
            app.sidebar_filter.as_deref(),
            Some("ts-15"),
            "the draw kept the filter"
        );
        app.navigate(1);
        assert_eq!(
            app.sidebar_filter.as_deref(),
            Some("ts-15"),
            "A-j kept it too"
        );
        app.sidebar_filter = None;
        // Owner folds tc and its section with the active session inside
        // (folding happens on the header, so the cursor leaves the session).
        app.sections.push(cm_daemon::manifest::SidebarSection {
            id: "s1".into(),
            name: "Project".into(),
            color: None,
            folded: false,
        });
        app.workspace_sections.insert("r".into(), "s1".into());
        app.cursor = Cursor::Workspace(1);
        assert!(app.toggle_tree_fold());
        sidebar_buffer(&mut app, 30);
        assert!(app.folded_workspaces.contains("c"), "the fold stays");
        app.cursor = Cursor::Section("s1".into());
        assert!(app.toggle_section_fold());
        sidebar_buffer(&mut app, 30);
        sidebar_buffer(&mut app, 30);
        assert!(app.sections[0].folded, "the section fold stays");
        // A later jump into it still reveals.
        assert!(app.jump_cursor_to_session_uid("ts-13-0"));
        assert!(!app.sections[0].folded && !app.folded_workspaces.contains("c"));
    }

    /// Review (2): A-g reaches an alert inside a fold, and a folded header
    /// counts its alerts.
    #[test]
    fn a_g_reaches_an_alert_inside_a_fold_and_the_rollup_counts_it() {
        let mut app = org_app();
        app.raise_alert("ts-13-0", "g", "decide");
        app.folded_workspaces.insert("c".into());
        let tree = app.task_tree();
        assert_eq!(tree.folded[&Node::Workspace(1)].3, 1, "one alert under c");
        let buffer = sidebar_buffer(&mut app, 30);
        let text: String = (0..30u16)
            .flat_map(|y| (0..40u16).map(move |x| (x, y)))
            .map(|p| buffer[p].symbol().to_string())
            .collect();
        assert!(text.contains("\u{2691}1"), "folded c shows ⚑1");
        app.cursor = Cursor::Workspace(4);
        app.jump_to_next_attention();
        assert_eq!(app.cursor, Cursor::Session(2, 0));
        assert!(!app.folded_workspaces.contains("c") && shown(&app, "ts-13-0"));
    }

    /// Review (4): a parent cycle inside one checkout keeps every session;
    /// the lowest task id becomes the root.
    #[test]
    fn a_parent_cycle_in_one_workspace_keeps_its_sessions() {
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = vec![bare_ws("w")];
        app.tasks = vec![task("t2", "w", Some("t1")), task("t1", "w", Some("t2"))];
        session(&mut app, 0, "t1", "ts-1-0", "codex");
        session(&mut app, 0, "t2", "ts-2-0", "codex");
        assert_eq!(outline(&app), ["ws:w", "ts-1-0", "ts-2-0"]);
        assert_eq!(app.workspace_task(0).as_deref(), Some("t1"), "the cycle breaks at t1");
    }

    /// Review (3): a header that stops drawing moves the cursor to a row.
    #[test]
    fn a_vanished_header_moves_the_cursor_to_a_row() {
        let mut app = org_app();
        app.cursor = Cursor::Division(loose_division("div-gone"));
        app.clamp_cursor();
        assert_eq!(app.cursor, Cursor::Workspace(0));
        // tl's only session leaves: the cursor moves to l's header.
        app.workspaces[4].sessions.clear();
        app.cursor = Cursor::Session(4, 0);
        app.clamp_cursor();
        assert_eq!(app.cursor, Cursor::Workspace(4));
    }

    /// Review (5): a refused lead pin rolls back, unless a newer one won.
    #[test]
    fn a_failed_lead_pin_rolls_back_unless_superseded() {
        let mut app = org_app();
        let previous = app.set_local_lead_pin("tc", Some("ts-12-0".into()));
        assert_eq!(previous, None);
        app.on_task_lead_failed("tc", Some("ts-12-0".into()), previous, "500");
        let pin = |app: &App| {
            app.find_task_entry("tc")
                .and_then(|t| t.metadata.as_ref())
                .and_then(pinned_lead)
                .map(str::to_owned)
        };
        assert_eq!(pin(&app), None, "rolled back");
        app.set_local_lead_pin("tc", Some("ts-12-0".into()));
        app.set_local_lead_pin("tc", Some("ts-99-0".into()));
        app.on_task_lead_failed("tc", Some("ts-12-0".into()), None, "500");
        assert_eq!(pin(&app).as_deref(), Some("ts-99-0"), "a newer pin is kept");
    }

    /// Review (6): a nested row with an assignment equal to its root block's
    /// section stays nested, and A-e exposes that assignment for editing.
    #[test]
    fn a_nested_row_takes_its_root_blocks_section() {
        let mut app = org_app();
        app.sections.push(cm_daemon::manifest::SidebarSection {
            id: "s1".into(),
            name: "Project".into(),
            color: None,
            folded: false,
        });
        app.workspace_sections.insert("r".into(), "s1".into());
        app.workspace_sections.insert("g".into(), "s1".into());
        app.cursor = Cursor::Session(2, 0);
        assert_eq!(app.cursor_section_id().as_deref(), Some("s1"));
        assert_eq!(
            app.nested_section_label("g").as_deref(),
            Some("inherits Project from r (nested)")
        );
        assert_eq!(
            app.nested_section_label("r"),
            None,
            "a top-level block chooses its own"
        );
        // The Section field (A-e on the workspace header) cycles even when
        // the workspace is nested.
        app.cursor = Cursor::Workspace(2);
        app.open_session_settings();
        let tab = Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        for _ in 0..3 {
            app.handle_event(&tab);
        }
        let InputMode::WorkspaceSettings {
            section,
            active_field,
            ..
        } = &app.input_mode
        else {
            panic!("ws settings")
        };
        assert_eq!(*active_field, 3);
        assert_eq!(
            section.as_deref(),
            Some("s1"),
            "the nested workspace's explicit choice is editable"
        );
        app.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::NONE,
        )));
        let InputMode::WorkspaceSettings { section, .. } = &app.input_mode else {
            panic!("ws settings")
        };
        assert_eq!(*section, None, "Right from the matching section chooses auto");
    }

    #[test]
    fn explicit_nested_section_overrides_detach_and_auto_renests() {
        let mut app = org_app();
        for (id, name) in [("s1", "Project"), ("s2", "Latency")] {
            app.sections.push(cm_daemon::manifest::SidebarSection {
                id: id.into(), name: name.into(), color: None, folded: false,
            });
        }
        app.workspace_sections.insert("r".into(), "s1".into());
        app.workspace_sections.insert("g".into(), "s2".into());
        let tree = app.task_tree();
        assert_eq!(tree.block_root[&1], 0, "c remains under r");
        assert_eq!(tree.block_root[&2], 2, "g is detached into s2");
        assert_eq!(tree.section_of[&2], "s2");
        assert_eq!(tree.section_members.get("s2"), Some(&vec![2]));
        app.cursor = Cursor::Session(2, 0);
        assert_eq!(app.cursor_section_id().as_deref(), Some("s2"));
        assert!(outline(&app).contains(&"ws:g".to_string()));

        // A child with no override follows the detached block.
        app.workspaces.push(bare_ws("z"));
        app.tasks.push(task("tz", "z", Some("tg")));
        session(&mut app, 5, "tz", "ts-16-0", "codex");
        let tree = app.task_tree();
        assert_eq!(tree.block_root[&5], 2, "detached block keeps its child");
        assert_eq!(tree.section_of[&5], "s2");

        // Explicit none also detaches, as a loose top-level block.
        app.workspace_sections.insert("g".into(), super::super::sections::SECTION_NONE.into());
        let tree = app.task_tree();
        assert_eq!(tree.block_root[&2], 2);
        assert!(!tree.section_of.contains_key(&2));

        // Auto removes the override and restores the task-tree nesting.
        app.workspace_sections.remove("g");
        let tree = app.task_tree();
        assert_eq!(tree.block_root[&2], 0, "auto re-nests under r");
        assert_eq!(tree.section_of[&2], "s1");

        // Cursor clamping remains valid while the workspace changes roots.
        app.cursor = Cursor::Workspace(2);
        app.clamp_cursor();
        assert_eq!(app.cursor, Cursor::Workspace(2));
    }

    #[test]
    fn nested_workspace_section_field_saves_detach_and_auto() {
        let mut app = org_app();
        for (id, name) in [("s1", "Project"), ("s2", "Latency")] {
            app.sections.push(cm_daemon::manifest::SidebarSection {
                id: id.into(), name: name.into(), color: None, folded: false,
            });
        }
        app.workspace_sections.insert("r".into(), "s1".into());
        app.workspace_sections.insert("g".into(), "s1".into());
        app.cursor = Cursor::Workspace(2);
        app.open_session_settings();
        let InputMode::WorkspaceSettings { section, .. } = &mut app.input_mode else {
            panic!("workspace settings")
        };
        *section = Some("s2".into());
        app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(app.workspace_sections.get("g").map(String::as_str), Some("s2"));
        assert_eq!(app.task_tree().block_root[&2], 2, "saved override detaches it");

        app.cursor = Cursor::Workspace(2);
        app.open_session_settings();
        let InputMode::WorkspaceSettings { section, .. } = &mut app.input_mode else {
            panic!("workspace settings")
        };
        *section = None;
        app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!app.workspace_sections.contains_key("g"));
        assert_eq!(app.task_tree().block_root[&2], 0, "saved auto re-nests it");
    }

    /// The workspace row stands for its task (Owner, 2026-11-05: the task
    /// subheaders are gone). A-i shows the task's brief; A-s, A-d and the
    /// workflow launch scope to it; A-x on the row still means the
    /// workspace. A checkout shared by two tasks names the other one in
    /// A-i, scopes nothing, and A-e offers both.
    #[test]
    fn the_workspace_row_stands_for_its_task() {
        let mut app = org_app();
        app.tasks
            .iter_mut()
            .find(|t| t.task_id.as_deref() == Some("tc"))
            .unwrap()
            .prompt = Some("Fix the flaky bench".into());
        app.cursor = Cursor::Workspace(1);
        let peek = format!("{:?}", app.build_peek_lines());
        assert!(peek.contains("Fix the flaky bench"), "{peek}");
        assert!(!peek.contains("Also here"), "{peek}");
        assert_eq!(app.cursor_scope_task_id().as_deref(), Some("tc"));
        assert_eq!(app.cursor_task_id(), None, "A-x on the row deletes the workspace, not the task");
        // A second, unrelated task in c's checkout.
        app.tasks.push(task("tq", "c", None));
        assert_eq!(app.workspace_tasks(1), ["tc", "tq"]);
        assert_eq!(app.workspace_task(1), None);
        assert_eq!(app.cursor_scope_task_id(), None, "ambiguous: a session row picks");
        let peek = format!("{:?}", app.build_peek_lines());
        assert!(peek.contains("Fix the flaky bench") && peek.contains("Also here"), "{peek}");
        app.open_session_settings();
        let InputMode::WorkspaceSettings { tasks, .. } = &app.input_mode else {
            panic!("workspace settings")
        };
        let ids: Vec<&str> = tasks.iter().map(|t| t.task_id.as_str()).collect();
        assert_eq!(ids, ["tc", "tq"]);
    }

    /// A-e renames the workspace's task from the merged form, and a
    /// workspace without its own colour shows its task's legacy colour.
    #[test]
    fn the_workspace_form_renames_its_task() {
        let mut app = org_app();
        app.cursor = Cursor::Workspace(4);
        app.open_session_settings();
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        for _ in 0..4 {
            app.handle_event(&key(KeyCode::Tab));
        }
        for c in "-v2".chars() {
            app.handle_event(&key(KeyCode::Char(c)));
        }
        app.handle_event(&key(KeyCode::Enter));
        assert!(matches!(app.input_mode, InputMode::Normal));
        let name = |app: &App| app.tasks.iter().find(|t| t.task_id.as_deref() == Some("tl")).unwrap().name.clone();
        assert_eq!(name(&app), "tl-v2");
        assert_eq!(app.workspaces[4].name, "l", "the workspace keeps its own name");
        // The legacy task colour prefills the workspace's Color field, and
        // a save moves it onto the workspace; another workspace showing the
        // same task keeps it as its own.
        app.workspaces.push(bare_ws("l2"));
        session(&mut app, 5, "tl", "ts-50-0", "codex");
        app.task_colors.insert("tl".into(), "cyan".into());
        app.open_session_settings();
        let InputMode::WorkspaceSettings { color, .. } = &app.input_mode else {
            panic!("workspace settings")
        };
        assert_eq!(color.as_deref(), Some("cyan"));
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(app.workspaces[4].color.as_deref(), Some("cyan"));
        assert_eq!(app.workspaces[5].color.as_deref(), Some("cyan"));
        assert!(!app.task_colors.contains_key("tl"));
    }

    /// Owner, 2026-10-21: no indent by depth (every block at the top-level
    /// indent; sessions keep theirs), and the focused group shows only as
    /// a narrow strip at the sidebar's left edge: no tint, no ancestor bar.
    /// Owner, 2026-11-05: no task subheaders; sessions sit one indent under
    /// their workspace header, which carries the fold glyph.
    #[test]
    fn the_sidebar_draws_flat_blocks_level_marks_and_a_group_edge_strip() {
        let mut app = org_app();
        // A second, later agent on tc: the earlier one leads; a bash
        // session never does.
        session(&mut app, 1, "tc", "ts-20-0", "claude");
        session(&mut app, 1, "tc", "ts-05-0", "bash");
        assert_eq!(app.task_leads()["tc"], "ts-12-0");
        app.cursor = Cursor::Session(1, 0);
        let buffer = sidebar_buffer(&mut app, 30);
        let rows: Vec<Vec<String>> = (0..30u16)
            .map(|y| (0..40u16).map(|x| buffer[(x, y)].symbol().to_string()).collect())
            .collect();
        let text = |y: usize| rows[y].concat();
        // Column of `needle` in its row (cells, not bytes).
        let at = |needle: &str| -> (usize, usize) {
            let chars: Vec<char> = needle.chars().collect();
            for (y, row) in rows.iter().enumerate() {
                for x in 0..row.len().saturating_sub(chars.len()) {
                    if (0..chars.len()).all(|k| row[x + k] == chars[k].to_string()) {
                        return (y, x);
                    }
                }
            }
            panic!("{needle}\n{}", (0..30).map(text).collect::<Vec<_>>().join("\n"));
        };
        // No task subheaders: no row names a task.
        for t in ["tr", "tc", "tg", "to", "tl"] {
            assert!(!rows.iter().any(|r| r.concat().contains(&format!(" {t} "))), "{t} has no row");
        }
        // Workspace headers at every depth share one column; a leaf has no
        // fold glyph. Sessions keep one indent, two cells in from it.
        let (_, r) = at("\u{25be} r");
        assert_eq!(at("\u{25be} c").1, r, "c (level 2) at the root's indent");
        assert_eq!(at("g \u{2302}").1, r, "g (level 3) too, a leaf: no fold glyph");
        assert_eq!(at("o \u{2302}").1, r, "o (level 3, under a missing tx) too");
        assert_eq!(at("ts-13-0").1, at("ts-15-0").1, "sessions keep their indent");
        // Level marks on the leads with reports only.
        assert_eq!(at("\u{2605} ts-10-0").1 + 2, at("ts-10-0").1);
        assert!(text(at("ts-12-0").0).contains("\u{2606} ts-12-0"), "tc is level 2");
        for leaf in ["ts-15-0", "ts-14-0", "ts-11-0", "ts-20-0"] {
            let row = text(at(leaf).0);
            assert!(!row.contains(['\u{2605}', '\u{2606}', '\u{2726}', '\u{2727}']), "{leaf}: {row}");
        }
        // The group (tc and everything under it) gets the edge strip and no
        // background; its ancestors and an unrelated block get neither.
        for needle in ["\u{25be} c", "ts-12-0", "ts-20-0", "g \u{2302}", "ts-13-0"] {
            let (y, _) = at(needle);
            assert_eq!(rows[y][1], "\u{258f}", "{needle}: edge strip");
            assert_eq!(buffer[(1, y as u16)].fg, theme::TREE_FOCUS);
        }
        for needle in ["\u{25be} r", "ts-10-0", "ts-15-0"] {
            assert_eq!(rows[at(needle).0][1], " ", "{needle}: no strip");
        }
        assert!(
            buffer.content().iter().all(|c| c.bg == ratatui::style::Color::Reset),
            "no group tint anywhere"
        );
    }

    #[test]
    fn task_tree_cache_rebuilds_for_workflow_host_and_filer_inputs() {
        let mut app = org_app();
        session(&mut app, 0, "tr", "ts-wf-0", "claude");
        session(&mut app, 0, "tr", "ts-wf-1", "claude");
        for (si, role) in [(1usize, "worker"), (2usize, "reviewer")] {
            app.workspaces[0].sessions[si].workflow_run_id = Some("wf".into());
            app.workspaces[0].sessions[si].workflow_role = Some(role.into());
        }
        app.workflow_runs.push(WorkflowRun::new(
            "wf".into(),
            "wf".into(),
            "ws-1".into(),
            BTreeMap::new(),
            "worker".into(),
            BTreeMap::new(),
            None,
            BTreeMap::new(),
            0,
        ));
        app.workflows.insert(
            "wf".into(),
            Workflow {
                name: "wf".into(),
                description: String::new(),
                roles: BTreeMap::new(),
                role_order: vec!["worker".into(), "reviewer".into()],
                transitions: Vec::new(),
            },
        );
        let session_order = |app: &App, tree: &TaskTree| {
            tree.items
                .iter()
                .filter_map(|item| match item {
                    VisualItem::Session(wi, si) => Some(app.workspaces[*wi].sessions[*si].uid.clone()),
                    _ => None,
                })
                .filter(|uid| uid.starts_with("ts-wf-"))
                .collect::<Vec<_>>()
        };
        let first = app.task_tree_shared();
        let initial_order = session_order(&app, &first);
        assert_eq!(initial_order.len(), 2);

        app.workflows.get_mut("wf").unwrap().role_order.reverse();
        let reordered = app.task_tree_shared();
        assert!(!std::rc::Rc::ptr_eq(&first, &reordered));
        assert_eq!(session_order(&app, &reordered).len(), initial_order.len());

        app.workspaces[0].sessions[2].workflow_role = Some("manager".into());
        let role_changed = app.task_tree_shared();
        assert!(!std::rc::Rc::ptr_eq(&reordered, &role_changed));
        assert_eq!(session_order(&app, &role_changed).len(), session_order(&app, &reordered).len());

        // Both participants leave the run: its header goes.
        app.workspaces[0].sessions[1].workflow_run_id = None;
        app.workspaces[0].sessions[2].workflow_run_id = None;
        let ungrouped = app.task_tree_shared();
        assert!(!std::rc::Rc::ptr_eq(&role_changed, &ungrouped));
        assert!(ungrouped.items.iter().filter(|item| matches!(item, VisualItem::WorkflowHeader { .. })).count() < role_changed.items.iter().filter(|item| matches!(item, VisualItem::WorkflowHeader { .. })).count());

        let alert_host = app.workspaces[0].sessions[2].host_id.clone();
        app.owner_alert_rows.insert((alert_host, "ts-wf-0".into()), "host-alert".into());
        assert!(app.session_has_alert("ts-wf-0", &app.workspaces[0].sessions[2].host_id));
        app.folded_workspaces.insert("r".into());
        let alert_before = app.task_tree_shared();
        app.workspaces[0].sessions[2].host_id = cm_daemon::host_id::HostId::new("other");
        let host_changed = app.task_tree_shared();
        assert!(!std::rc::Rc::ptr_eq(&alert_before, &host_changed), "session host rebinding invalidates folded alert/rollup cache");

        app.tasks.iter_mut().find(|t| t.task_id.as_deref() == Some("tl")).unwrap().metadata =
            Some(serde_json::json!({"filer": {"task_id": "tr2"}}));
        let filer_changed = app.task_tree_shared();
        assert!(!std::rc::Rc::ptr_eq(&host_changed, &filer_changed));

        app.sections.push(cm_daemon::manifest::SidebarSection { id: "sec".into(), name: "Project".into(), color: None, folded: false });
        app.workspace_sections.insert("r".into(), "sec".into());
        app.tasks.iter_mut().find(|t| t.task_id.as_deref() == Some("tl")).unwrap().metadata =
            Some(serde_json::json!({"filer": {"task_id": "tr"}}));
        let section_tree = app.task_tree_shared();
        assert_eq!(app.section_of_workspace(4).as_deref(), Some("sec"));
        assert!(!std::rc::Rc::ptr_eq(&filer_changed, &section_tree));
        app.tasks.iter_mut().find(|t| t.task_id.as_deref() == Some("tl")).unwrap().metadata =
            Some(serde_json::json!({"filer": {"task_id": "missing"}}));
        let loose_tree = app.task_tree_shared();
        assert_eq!(app.section_of_workspace(4), None);
        assert!(!std::rc::Rc::ptr_eq(&section_tree, &loose_tree));
    }

    /// The key skips a closed workspace without sessions (the laptop keeps
    /// thousands for A-O): a fold on one keeps the cached tree; reopening
    /// it or folding a drawn workspace rebuilds.
    #[test]
    fn task_tree_cache_skips_closed_workspaces_without_sessions() {
        let mut app = org_app();
        let mut gone = super::super::sections::layout_tests::bare_ws("gone");
        gone.is_closed = true;
        app.workspaces.push(gone);
        let gi = app.workspaces.len() - 1;
        let first = app.task_tree_shared();
        app.folded_workspaces.insert("gone".into());
        assert!(std::rc::Rc::ptr_eq(&first, &app.task_tree_shared()));

        app.workspaces[gi].is_closed = false;
        let reopened = app.task_tree_shared();
        assert!(!std::rc::Rc::ptr_eq(&first, &reopened));
        assert!(reopened.items.contains(&VisualItem::WorkspaceHeader(gi)));

        let id = app.workspaces[0].id.clone();
        app.folded_workspaces.insert(id);
        assert!(!std::rc::Rc::ptr_eq(&reopened, &app.task_tree_shared()));
    }
}
