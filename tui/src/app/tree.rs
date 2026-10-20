//! The nested work tree of the Sessions view's Task sub-view (org batch,
//! 2026-10-20; spec: Swarm-Coord's org-design PLAN.md, "msgfix").
//!
//! - A workspace block nests under the block that holds its root task's
//!   parent task, at any depth. Reporting lines follow `parent_task_id`.
//! - A division (an initiative with `kind = "division"`) groups the
//!   top-level blocks of its tree under one header.
//! - Task and division headers fold with Space/Enter; the fold state rides
//!   the manifest. A folded header shows its session rollup.
//! - The focused row's ancestor chain draws a coloured gutter bar, and its
//!   group a tint.
//! - Each task's lead session carries a leader mark (see [`lead_session_uid`]).
//!
//! Sections still group top-level blocks; a nested block follows its root
//! block's section. The Status sub-view and the continuous column are
//! unchanged.

use super::*;
use std::collections::{BTreeMap, HashMap, HashSet};

/// What a sidebar row belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Node {
    Division(String),
    Workspace(usize),
    Task(String),
}

/// Tree metadata for one Task sub-view row.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct TreeRow {
    /// Enclosing nodes of the blocks this row sits inside, outermost first:
    /// one gutter cell each.
    pub cells: Vec<Node>,
    /// Extra depth inside its own block: an inherit-mode subtask shares its
    /// parent's checkout and nests under it there.
    pub local: u8,
    /// The node the row belongs to: a header's own node, a session's task
    /// (or its workspace when unbound).
    pub node: Option<Node>,
}

/// `(sessions, running, idle)` under a folded header.
pub(super) type Rollup = (usize, usize, usize);

pub(super) struct TaskTree {
    pub items: Vec<VisualItem>,
    pub rows: HashMap<VisualItem, TreeRow>,
    /// Each node's parent in the tree.
    pub parents: HashMap<Node, Node>,
    /// The top-level block each visible workspace renders under.
    pub block_root: HashMap<usize, usize>,
    /// Rollups for folded headers.
    pub folded: HashMap<Node, Rollup>,
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

/// How visible workspaces relate through the task tree.
#[derive(Default)]
struct Forest {
    order: Vec<usize>,
    /// Session buckets per workspace, keyed by task (None = unbound).
    buckets: HashMap<usize, BTreeMap<Option<String>, Vec<usize>>>,
    /// Tasks bound to each workspace, in display order.
    bound: HashMap<usize, Vec<String>>,
    /// The workspace whose block holds each task's header.
    home: HashMap<String, usize>,
    /// A nested block: child workspace → (parent workspace, task it hangs under).
    attach: HashMap<usize, (usize, String)>,
    /// Blocks hanging under each task, in display order.
    children: HashMap<String, Vec<usize>>,
    /// Nearest ancestor of a task bound to the same workspace.
    local_parent: HashMap<String, String>,
}

impl Forest {
    fn root(&self, wi: usize) -> Option<&String> {
        self.bound
            .get(&wi)?
            .iter()
            .find(|t| !self.local_parent.contains_key(*t))
    }

    fn local_children<'a>(&'a self, wi: usize, t: &'a str) -> impl Iterator<Item = &'a String> + 'a {
        self.bound
            .get(&wi)
            .into_iter()
            .flatten()
            .filter(move |c| self.local_parent.get(*c).map(String::as_str) == Some(t))
    }

    fn nested_blocks(&self, wi: usize, t: &str) -> Vec<usize> {
        if self.home.get(t) != Some(&wi) {
            return Vec::new();
        }
        self.children.get(t).cloned().unwrap_or_default()
    }

    fn bucket(&self, wi: usize, t: Option<&str>) -> &[usize] {
        self.buckets
            .get(&wi)
            .and_then(|b| b.get(&t.map(str::to_owned)))
            .map_or(&[], Vec::as_slice)
    }

    /// A task header shows when anything sits under it.
    fn has_content(&self, wi: usize, t: &str) -> bool {
        !self.bucket(wi, Some(t)).is_empty()
            || !self.nested_blocks(wi, t).is_empty()
            || self.local_children(wi, t).any(|c| self.has_content(wi, c))
    }

    /// Every session row under a task header, nested blocks included.
    fn task_sessions(&self, wi: usize, t: &str, out: &mut Vec<(usize, usize)>) {
        out.extend(self.bucket(wi, Some(t)).iter().map(|si| (wi, *si)));
        for c in self.local_children(wi, t) {
            self.task_sessions(wi, c, out);
        }
        for wj in self.nested_blocks(wi, t) {
            self.block_sessions(wj, out);
        }
    }

    fn block_sessions(&self, wi: usize, out: &mut Vec<(usize, usize)>) {
        out.extend(self.bucket(wi, None).iter().map(|si| (wi, *si)));
        let roots: Vec<String> = self
            .bound
            .get(&wi)
            .into_iter()
            .flatten()
            .filter(|t| !self.local_parent.contains_key(*t))
            .cloned()
            .collect();
        for t in roots {
            self.task_sessions(wi, &t, out);
        }
    }
}

/// One entry of a top-level list: a block, or a division's blocks.
enum Entry {
    Block(usize),
    Division(String, Vec<usize>),
}

impl App {
    fn forest(&self, members: &HashSet<(usize, usize)>) -> Forest {
        let mut f = Forest {
            order: self.task_view_visible_workspaces(members),
            ..Forest::default()
        };
        let parents = self.task_parent_map();
        for &wi in &f.order {
            let ws = &self.workspaces[wi];
            let mut buckets: BTreeMap<Option<String>, Vec<usize>> = BTreeMap::new();
            for (si, ts) in ws.sessions.iter().enumerate() {
                if !members.contains(&(wi, si)) {
                    buckets.entry(ts.task_id.clone()).or_default().push(si);
                }
            }
            // Planning binding order first, then tasks known only from
            // their sessions (stale API state), so none silently vanish.
            let mut bound: Vec<String> = Vec::new();
            for task in &self.tasks {
                if task.workspace_id.as_deref() == Some(ws.id.as_str()) {
                    if let Some(tid) = task.task_id.as_ref().filter(|t| !bound.contains(t)) {
                        bound.push(tid.clone());
                    }
                }
            }
            let mut tail: Vec<String> = buckets
                .keys()
                .flatten()
                .filter(|t| !bound.contains(t))
                .cloned()
                .collect();
            tail.sort();
            bound.extend(tail);
            for t in &bound {
                f.home.entry(t.clone()).or_insert(wi);
            }
            for t in &bound {
                let mut seen: HashSet<&str> = HashSet::new();
                let mut p = parents.get(t.as_str()).copied();
                while let Some(pt) = p.filter(|pt| seen.insert(*pt)) {
                    if bound.iter().any(|b| b == pt) {
                        f.local_parent.insert(t.clone(), pt.to_owned());
                        break;
                    }
                    p = parents.get(pt).copied();
                }
            }
            f.buckets.insert(wi, buckets);
            f.bound.insert(wi, bound);
        }
        // Hang each block under the nearest ancestor of its root task that
        // another visible block holds.
        for &wi in &f.order {
            let Some(root) = f.root(wi).cloned() else { continue };
            let mut seen: HashSet<&str> = HashSet::new();
            let mut p = parents.get(root.as_str()).copied();
            while let Some(pt) = p.filter(|pt| seen.insert(*pt)) {
                if let Some(&h) = f.home.get(pt).filter(|h| **h != wi) {
                    f.attach.insert(wi, (h, pt.to_owned()));
                    break;
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
                while let Some((p, _)) = f.attach.get(&cur) {
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
        for &wi in &f.order {
            if let Some((_, t)) = f.attach.get(&wi) {
                f.children.entry(t.clone()).or_default().push(wi);
            }
        }
        f
    }

    /// The Task sub-view's rows and their tree metadata.
    pub(super) fn task_tree(&self) -> TaskTree {
        // While filtering, folds are ignored so a match is never hidden.
        self.task_tree_with(self.sidebar_filter.is_none())
    }

    /// Unfold every task and division header above a session (its own task
    /// included), so the row shows. For jumps from the org chart (F6) and
    /// the board (A-B). Returns true when a fold was cleared.
    pub(crate) fn reveal_session(&mut self, uid: &str) -> bool {
        let Some(node) = self.workspaces.iter().enumerate().find_map(|(wi, ws)| {
            let ts = ws.sessions.iter().find(|ts| ts.uid == uid)?;
            Some(ts.task_id.clone().map_or(Node::Workspace(wi), Node::Task))
        }) else {
            return false;
        };
        let mut changed = false;
        for n in self.task_tree_with(false).chain(&node) {
            changed |= match n {
                Node::Task(t) => self.folded_tasks.remove(&t),
                Node::Division(d) => self.folded_divisions.remove(&d),
                Node::Workspace(_) => false,
            };
        }
        if changed {
            self.save_session_manifest();
            self.needs_redraw = true;
        }
        changed
    }

    fn task_tree_with(&self, folds: bool) -> TaskTree {
        let members = self.continuous_members();
        let f = self.forest(&members);
        let mut out = TaskTree {
            items: Vec::new(),
            rows: HashMap::new(),
            parents: HashMap::new(),
            block_root: HashMap::new(),
            folded: HashMap::new(),
        };
        for &wi in &f.order {
            let mut top = wi;
            while let Some((p, _)) = f.attach.get(&top) {
                top = *p;
            }
            out.block_root.insert(wi, top);
        }
        let tops: Vec<usize> = f.order.iter().copied().filter(|wi| !f.attach.contains_key(wi)).collect();

        let mut loose: Vec<usize> = Vec::new();
        let mut by_section: HashMap<String, Vec<usize>> = HashMap::new();
        for &wi in &tops {
            match self.section_of_workspace(wi) {
                Some(sid) => by_section.entry(sid).or_default().push(wi),
                None => loose.push(wi),
            }
        }
        let mut emit = Emit { app: self, f: &f, out: &mut out, folds };
        for sec in &self.sections {
            emit.out.items.push(VisualItem::SectionHeader(sec.id.clone()));
            if !sec.folded {
                let entries = emit.entries(by_section.remove(&sec.id).unwrap_or_default());
                for (i, entry) in entries.into_iter().enumerate() {
                    if i > 0 {
                        emit.out.items.push(VisualItem::Separator);
                    }
                    emit.entry(entry);
                }
            }
            emit.out.items.push(VisualItem::SectionEnd);
        }
        for entry in emit.entries(loose) {
            if !emit.out.items.is_empty() && !matches!(emit.out.items.last(), Some(VisualItem::SectionEnd)) {
                emit.out.items.push(VisualItem::Separator);
            }
            emit.entry(entry);
        }
        out
    }

    /// The division a task belongs to, if its initiative is one.
    fn task_division(&self, task_id: &str) -> Option<String> {
        self.planning.task_division(task_id).map(|d| d.id)
    }

    /// `(sessions, running, idle)` for a set of session rows: engine state
    /// when reported, else the PTY status. Exited sessions are not counted.
    pub(super) fn rollup<'a>(&self, sessions: impl Iterator<Item = &'a TerminalSession>) -> Rollup {
        use super::agent_state::Activity;
        let (mut n, mut running, mut idle) = (0, 0, 0);
        for ts in sessions {
            n += 1;
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
        (n, running, idle)
    }

    /// The row the cursor is on, as a tree node.
    pub(super) fn cursor_node(&self) -> Option<Node> {
        match &self.cursor {
            Cursor::Division(id) => Some(Node::Division(id.clone())),
            Cursor::Task { task_id, .. } => Some(Node::Task(task_id.clone())),
            Cursor::Workspace(wi) => Some(Node::Workspace(*wi)),
            Cursor::Session(wi, si) => {
                let ts = self.workspaces.get(*wi)?.sessions.get(*si)?;
                Some(ts.task_id.clone().map_or(Node::Workspace(*wi), Node::Task))
            }
            Cursor::Backtest(_) | Cursor::Section(_) => None,
        }
    }

    pub(super) fn tree_folded(&self, node: &Node) -> bool {
        match node {
            Node::Task(t) => self.folded_tasks.contains(t),
            Node::Division(d) => self.folded_divisions.contains(d),
            Node::Workspace(_) => false,
        }
    }

    /// Space/Enter on a task or division header: fold or unfold it.
    pub(crate) fn toggle_tree_fold(&mut self) -> bool {
        let set = match &self.cursor {
            Cursor::Task { task_id, .. } => Some((&mut self.folded_tasks, task_id.clone())),
            Cursor::Division(id) => Some((&mut self.folded_divisions, id.clone())),
            _ => None,
        };
        let Some((set, id)) = set else { return false };
        if !set.remove(&id) {
            set.insert(id);
        }
        self.save_session_manifest();
        self.needs_redraw = true;
        true
    }

    /// The A-e Lead field: the task's live agent sessions as `(uid, label)`
    /// in spawn order (a pinned session that no longer qualifies is listed
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
        choices.sort_by(|a, b| spawn_order(&a.0).cmp(&spawn_order(&b.0)).then_with(|| a.0.cmp(&b.0)));
        if let Some(p) = pinned.as_ref().filter(|p| !choices.iter().any(|(u, _)| u == *p)) {
            choices.push((p.clone(), format!("{p} (not a live agent here)")));
        }
        (choices, pinned)
    }

    /// Lead session of every task with sessions in the sidebar, by the
    /// shared rule ([`lead_session_uid`]).
    pub(super) fn task_leads(&self) -> HashMap<String, String> {
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
        by_task
            .into_iter()
            .filter_map(|(t, candidates)| {
                let pinned = self
                    .find_task_entry(t)
                    .and_then(|e| e.metadata.as_ref())
                    .and_then(pinned_lead);
                Some((t.to_owned(), lead_session_uid(pinned, &candidates)?))
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
}

impl Emit<'_> {
    fn push(&mut self, item: VisualItem, row: TreeRow) {
        self.out.rows.insert(item.clone(), row);
        self.out.items.push(item);
    }

    /// Group a top-level list by division, a division at its first block,
    /// its coordinator's block first.
    fn entries(&self, tops: Vec<usize>) -> Vec<Entry> {
        let mut entries: Vec<Entry> = Vec::new();
        for wi in tops {
            let division = self.f.root(wi).and_then(|t| self.app.task_division(t));
            let Some(d) = division else {
                entries.push(Entry::Block(wi));
                continue;
            };
            match entries.iter_mut().find(|e| matches!(e, Entry::Division(id, _) if *id == d)) {
                Some(Entry::Division(_, blocks)) => blocks.push(wi),
                _ => entries.push(Entry::Division(d, vec![wi])),
            }
        }
        for entry in &mut entries {
            if let Entry::Division(_, blocks) = entry {
                let coordinator = |wi: &usize| {
                    !self.f.root(*wi).is_some_and(|t| self.app.planning.task_division(t).is_some_and(|d| d.coordinator))
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
                self.push(VisualItem::DivisionHeader(d.clone()), TreeRow { node: Some(node.clone()), ..TreeRow::default() });
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
        let rollup = self
            .app
            .rollup(sessions.iter().map(|(wi, si)| &self.app.workspaces[*wi].sessions[*si]));
        self.out.folded.insert(node, rollup);
    }

    fn block(&mut self, wi: usize, cells: Vec<Node>) {
        let node = Node::Workspace(wi);
        if let Some(parent) = cells.last() {
            self.out.parents.insert(node.clone(), parent.clone());
        }
        self.push(VisualItem::WorkspaceHeader(wi), TreeRow { cells: cells.clone(), local: 0, node: Some(node.clone()) });
        let unbound = self.f.bucket(wi, None).to_vec();
        self.sessions(wi, &unbound, &node, &cells, 0);
        let roots: Vec<String> = self.f.bound[&wi]
            .iter()
            .filter(|t| !self.f.local_parent.contains_key(*t))
            .cloned()
            .collect();
        for t in roots {
            self.task(wi, &t, &cells, 0);
        }
    }

    fn task(&mut self, wi: usize, t: &str, cells: &[Node], local: u8) {
        if !self.f.has_content(wi, t) {
            return;
        }
        let node = Node::Task(t.to_owned());
        let parent = match self.f.local_parent.get(t) {
            Some(p) => Node::Task(p.clone()),
            None => Node::Workspace(wi),
        };
        self.out.parents.insert(node.clone(), parent);
        self.push(
            VisualItem::TaskHeader { ws_idx: wi, task_id: t.to_owned() },
            TreeRow { cells: cells.to_vec(), local, node: Some(node.clone()) },
        );
        if self.folds && self.app.tree_folded(&node) {
            let mut sessions = Vec::new();
            self.f.task_sessions(wi, t, &mut sessions);
            self.fold(node, &sessions);
            return;
        }
        let own = self.f.bucket(wi, Some(t)).to_vec();
        self.sessions(wi, &own, &node, cells, local);
        let local_children: Vec<String> = self.f.local_children(wi, t).cloned().collect();
        for c in local_children {
            self.task(wi, &c, cells, local.saturating_add(1));
        }
        let mut inner = cells.to_vec();
        inner.push(node);
        for wj in self.f.nested_blocks(wi, t) {
            self.block(wj, inner.clone());
        }
    }

    /// A bucket's session rows: standalone (running first), then workflow
    /// groups in role order, exactly as the flat view drew them.
    fn sessions(&mut self, wi: usize, indices: &[usize], node: &Node, cells: &[Node], local: u8) {
        let ws = &self.app.workspaces[wi];
        let row = || TreeRow { cells: cells.to_vec(), local, node: Some(node.clone()) };
        let mut standalone: Vec<usize> = Vec::new();
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for si in indices {
            match &ws.sessions[*si].workflow_run_id {
                Some(run_id) => groups.entry(run_id.clone()).or_default().push(*si),
                None => standalone.push(*si),
            }
        }
        let (running, other): (Vec<_>, Vec<_>) =
            standalone.into_iter().partition(|si| ws.sessions[*si].status == SessionStatus::Running);
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
            self.push(VisualItem::WorkflowHeader { ws_idx: wi, run_id: run_id.clone() }, row());
            let role_order: Vec<String> = run
                .and_then(|r| self.app.workflows.get(&r.workflow_name))
                .map(|wf| wf.role_order.clone())
                .unwrap_or_default();
            let mut ordered = session_indices;
            ordered.sort_by_key(|si| {
                let role = ws.sessions[*si].workflow_role.as_deref().unwrap_or("");
                role_order.iter().position(|r| r == role).unwrap_or(usize::MAX)
            });
            for si in ordered {
                self.push(VisualItem::Session(wi, si), row());
            }
        }
    }
}

// The lead rule is shared with the daemon's `org` tool: one function.
pub(crate) use cm_daemon::org::{is_agent_engine, lead_session_uid, pinned_lead, spawn_order, LeadCandidate};

#[cfg(test)]
mod tree_tests {
    use super::super::sections::layout_tests::{bare_ws, sidebar_buffer, task, test_app};
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    /// A session of `engine` on task `tid`, live, with a well-formed uid.
    fn session(app: &mut App, wi: usize, tid: &str, uid: &str, engine: &str) {
        let pty = crate::session::Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None).unwrap();
        let mut ts = super::super::model::make_simple_session_with_uid(uid.to_string(), uid, engine, pty, None);
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

    /// `<depth>name`: workspace headers as `ws:<id>`, task headers as
    /// `task:<id>`, sessions by uid, with their gutter depth (cells) and
    /// local depth.
    fn outline(app: &App) -> Vec<String> {
        let tree = app.task_tree();
        tree.items
            .iter()
            .filter_map(|vi| {
                let label = match vi {
                    VisualItem::WorkspaceHeader(wi) => format!("ws:{}", app.workspaces[*wi].id),
                    VisualItem::TaskHeader { task_id, .. } => format!("task:{task_id}"),
                    VisualItem::Session(wi, si) => app.workspaces[*wi].sessions[*si].uid.clone(),
                    VisualItem::DivisionHeader(id) => format!("division:{id}"),
                    _ => return None,
                };
                let row = &tree.rows[vi];
                Some(format!("{}{}{label}", ">".repeat(row.cells.len()), "+".repeat(row.local as usize)))
            })
            .collect()
    }

    #[test]
    fn blocks_nest_under_their_parent_task_at_any_depth() {
        let app = org_app();
        assert_eq!(
            outline(&app),
            [
                "ws:r", "task:tr", "ts-10-0",
                // Inherit-mode subtask: same checkout, one level in.
                "+task:tr2", "+ts-11-0",
                // c under tr, g under tc: depth follows the task tree.
                ">ws:c", ">task:tc", ">ts-12-0",
                ">>ws:g", ">>task:tg", ">>ts-13-0",
                // tx has no block: o hangs under tx's nearest visible ancestor.
                ">ws:o", ">task:to", ">ts-14-0",
                "ws:l", "task:tl", "ts-15-0",
            ],
        );
        let tree = app.task_tree();
        assert_eq!(tree.block_root[&2], 0, "g renders under r's block");
        assert_eq!(
            tree.chain(&Node::Task("tg".into())),
            [
                Node::Task("tg".into()),
                Node::Workspace(2),
                Node::Task("tc".into()),
                Node::Workspace(1),
                Node::Task("tr".into()),
                Node::Workspace(0),
            ],
        );
    }

    #[test]
    fn a_folded_task_hides_its_subtree_and_rolls_it_up_until_unfolded() {
        let mut app = org_app();
        app.cursor = Cursor::Task { ws_idx: 1, task_id: "tc".into() };
        let space = Event::Key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert!(app.handle_event(&space));
        assert!(app.folded_tasks.contains("tc"));
        let outline_folded = outline(&app);
        assert!(outline_folded.contains(&">task:tc".to_string()), "the header stays");
        for hidden in [">ts-12-0", ">>ws:g", ">>ts-13-0"] {
            assert!(!outline_folded.contains(&hidden.to_string()), "{hidden} folds away");
        }
        let tree = app.task_tree();
        assert_eq!(tree.folded[&Node::Task("tc".into())].0, 2, "tc's session and g's");
        // Filtering ignores folds, so a match is never hidden.
        app.sidebar_filter = Some("ts-13".into());
        assert!(app.visual_items().iter().any(|vi| matches!(vi, VisualItem::Session(2, _))));
        app.sidebar_filter = None;
        assert!(app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))));
        assert!(!app.folded_tasks.contains("tc"));
        assert_eq!(outline(&app), outline(&org_app()));
        // reveal_session unfolds every header above a session, its own task's too.
        app.folded_tasks.extend(["tr".to_string(), "tc".to_string(), "tg".to_string(), "tl".to_string()]);
        assert!(app.reveal_session("ts-13-0"));
        assert!(app.folded_tasks.iter().eq(["tl".to_string()].iter()), "{:?}", app.folded_tasks);
        assert!(outline(&app).contains(&">>ts-13-0".to_string()));
        assert!(!app.reveal_session("ts-13-0"), "nothing left to unfold");
    }

    /// Owner-approved 2026-10-20: a task header is a tree row. Even with a
    /// single session under it, no key reaches that session's terminal.
    #[test]
    fn a_task_header_never_stands_in_for_its_session() {
        let mut app = org_app();
        app.cursor = Cursor::Task { ws_idx: 4, task_id: "tl".into() };
        assert!(app.active_session().is_none());
        assert!(app.active_session_mut().is_none());
    }

    #[test]
    fn fold_state_rides_the_manifest() {
        let mut app = org_app();
        app.folded_tasks.insert("tc".into());
        app.folded_divisions.insert("div-1".into());
        let manifest: cm_daemon::manifest::Manifest = serde_json::from_value(serde_json::json!({
            "folded_tasks": ["tc"], "folded_divisions": ["div-1"],
        }))
        .unwrap();
        assert!(manifest.folded_tasks.contains("tc") && manifest.folded_divisions.contains("div-1"));
        let json = serde_json::to_value(&cm_daemon::manifest::Manifest::default()).unwrap();
        assert!(json.get("folded_tasks").is_none(), "empty sets stay out of the file");
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

    #[test]
    fn a_division_header_groups_its_top_level_blocks_and_folds() {
        let mut app = org_app();
        // tl joins the division as its own top-level block; tr is the root.
        app.planning.update_from_api(vec![division_task("tr", None, true), division_task("tl", None, false)]);
        assert_eq!(app.planning.division("div-1").unwrap().name, "Trader");
        let outline = outline(&app);
        assert_eq!(outline[0], "division:div-1");
        assert_eq!(outline[1], ">ws:r", "the coordinator's block comes first, inside the division");
        assert!(outline.contains(&">ws:l".to_string()));
        assert!(outline.contains(&">>>ws:g".to_string()), "nesting continues inside the division");
        app.cursor = Cursor::Division("div-1".into());
        assert!(app.toggle_tree_fold());
        let tree = app.task_tree();
        assert_eq!(tree.items.len(), 1, "a folded division is its header alone");
        assert_eq!(tree.folded[&Node::Division("div-1".into())].0, 6);
        // An ordinary initiative draws no header.
        let mut app = org_app();
        let mut plain = division_task("tr", None, true);
        plain.initiative.as_mut().unwrap()["kind"] = serde_json::json!("initiative");
        app.planning.update_from_api(vec![plain]);
        assert_eq!(super::super::tree::tree_tests::outline(&app)[0], "ws:r");
    }

    /// A-e on a task header: the Lead field cycles auto → each live agent
    /// session, and a save pins it (the ★ moves at once; the API call goes
    /// through the backend thread).
    #[test]
    fn the_lead_field_pins_another_session() {
        let mut app = org_app();
        session(&mut app, 1, "tc", "ts-20-0", "claude");
        session(&mut app, 1, "tc", "ts-05-0", "bash");
        app.tasks.iter_mut().find(|t| t.task_id.as_deref() == Some("tc")).unwrap().workspace_id = Some("c".into());
        assert_eq!(app.task_leads()["tc"], "ts-12-0");
        app.cursor = Cursor::Task { ws_idx: 1, task_id: "tc".into() };
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::ALT))));
        let InputMode::TaskSettings { lead, lead_choices, .. } = &app.input_mode else {
            panic!("task settings, got {:?}", std::mem::discriminant(&app.input_mode));
        };
        assert_eq!(*lead, None, "no pin yet: auto");
        let uids: Vec<&str> = lead_choices.iter().map(|(u, _)| u.as_str()).collect();
        assert_eq!(uids, ["ts-12-0", "ts-20-0"], "live agents only, in spawn order");
        for _ in 0..3 {
            app.handle_event(&key(KeyCode::Tab));
        }
        app.handle_event(&key(KeyCode::Left));
        let InputMode::TaskSettings { lead, .. } = &app.input_mode else { panic!("still open") };
        assert_eq!(lead.as_deref(), Some("ts-20-0"), "← wraps from auto to the last choice");
        app.handle_event(&key(KeyCode::Enter));
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert_eq!(app.task_leads()["tc"], "ts-20-0");
        let meta = app.find_task_entry("tc").and_then(|t| t.metadata.as_ref()).unwrap();
        assert_eq!(pinned_lead(meta), Some("ts-20-0"));
    }

    #[test]
    fn the_sidebar_draws_the_lead_mark_gutter_bar_and_group_tint() {
        let mut app = org_app();
        // A second, later agent on tc: the earlier one leads; a bash
        // session never does.
        session(&mut app, 1, "tc", "ts-20-0", "claude");
        session(&mut app, 1, "tc", "ts-05-0", "bash");
        let leads = app.task_leads();
        assert_eq!(leads["tc"], "ts-12-0");
        app.cursor = Cursor::Session(1, 0);
        let buffer = sidebar_buffer(&mut app, 30);
        let rows: Vec<String> = (0..30u16)
            .map(|y| (0..40u16).map(|x| buffer[(x, y)].symbol().to_string()).collect())
            .collect();
        let row_of = |needle: &str| rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| panic!("{needle}\n{}", rows.join("\n")));
        assert!(rows[row_of("ts-12-0")].contains("\u{2605} ts-12-0"), "{}", rows.join("\n"));
        assert!(!rows[row_of("ts-20-0")].contains('\u{2605}'));
        // Focused group (tc and everything under it): bar and tint.
        for needle in ["tc", "ts-12-0", "ts-13-0"] {
            let y = row_of(needle) as u16;
            assert_eq!(buffer[(1, y)].symbol(), "\u{258e}", "{needle}: gutter bar");
            assert_eq!(buffer[(30, y)].bg, theme::focus_tint(None), "{needle}: group tint");
        }
        // Ancestors get the bar but not the tint; an unrelated block neither.
        let y = row_of("ts-10-0") as u16;
        assert_ne!(buffer[(30, y)].bg, theme::FOCUS_TINT);
        let y = row_of("ts-15-0") as u16;
        assert_eq!(buffer[(1, y)].symbol(), " ");
        assert_ne!(buffer[(30, y)].bg, theme::FOCUS_TINT);
    }
}
