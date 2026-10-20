//! F6: the org chart (Owner request 2026-10-20; spec: Swarm-Coord's
//! org-design PLAN.md, "orgchart").
//!
//! Every session on every host, laid out as an org chart. Owner sits at the
//! top; under it come each division, a Standalone org and a Continuous org;
//! under those, root tasks and their subtasks along `parent_task_id`. A task
//! box holds that task's sessions with its lead (the shared lead rule) on
//! top. The Continuous org reuses the continuous column's grouping
//! (`continuous_rows_unfiltered`). Reporting lines only: nothing here
//! implies approval rights.
//!
//! The overlay takes every key while open: h/l siblings, j first child,
//! k parent, gg/G, Tab/S-Tab a session within the box, / n N search,
//! zc/zo/za fold, +/- zoom, H/J/K/L or arrows pan, c centre, b bash,
//! i info pane, Enter (or A-t) → Sessions view focused there, F6/Esc/q close.
//! The info pane's board items come from `board.read`, polled only while the
//! chart is open.

use super::*;
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc;

/// Open boards are re-listed this often while the chart is open.
const BOARD_LIST_EVERY: Duration = Duration::from_secs(60);
/// Each board is re-read this often (one request in flight at a time).
const BOARD_READ_EVERY: Duration = Duration::from_secs(30);
const BOARD_RETRY: Duration = Duration::from_secs(15);
/// Repaint cadence while open, so spinners and ages move.
const REPAINT: Duration = Duration::from_millis(250);
/// More leaf reports than this stack in a column instead of a row.
const STACK_MIN: usize = 3;
/// Session lines in a full card before "+N more".
const FULL_MAX_SESSIONS: usize = 6;
const INFO_WIDTH: u16 = 46;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Zoom {
    /// Bordered cards: task title, every session with glyph, engine, host.
    #[default]
    Full,
    /// One line per box: lead glyph and the box's name.
    Compact,
    /// One glyph per box: the whole org at a glance.
    Dense,
}

impl Zoom {
    fn box_w(self) -> i32 {
        match self {
            Zoom::Full => 30,
            Zoom::Compact => 18,
            Zoom::Dense => 2,
        }
    }
    fn hgap(self) -> i32 {
        match self {
            Zoom::Full => 2,
            Zoom::Compact => 1,
            Zoom::Dense => 0,
        }
    }
    /// Rows between a box and its reports: stem rows, then the bus row.
    fn vgap(self) -> i32 {
        match self {
            Zoom::Dense => 1,
            _ => 2,
        }
    }
    fn zoom_in(self) -> Self {
        match self {
            Zoom::Dense => Zoom::Compact,
            _ => Zoom::Full,
        }
    }
    fn zoom_out(self) -> Self {
        match self {
            Zoom::Full => Zoom::Compact,
            _ => Zoom::Dense,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Zoom::Full => "full",
            Zoom::Compact => "compact",
            Zoom::Dense => "dense",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NodeKind {
    Owner,
    /// A top-level org: a division, Standalone, or Continuous.
    Org,
    Task,
    /// Sessions bound to no task, grouped by workspace (Standalone).
    Workspace,
    Orchestrator,
    /// A continuous orchestrator's report: its subtask, or a same-task worker.
    Worker,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ChartSession {
    pub uid: String,
    pub wi: usize,
    pub si: usize,
    pub lead: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ChartNode {
    /// Stable across rebuilds: focus and folds are keyed by it.
    pub key: String,
    pub kind: NodeKind,
    pub title: String,
    pub task_id: Option<String>,
    pub sessions: Vec<ChartSession>,
    pub children: Vec<usize>,
    pub parent: Option<usize>,
    /// Sort rank among siblings (coordinator tasks first, then tasks, then
    /// loose workspaces); ties sort by title.
    rank: u8,
}

/// The org as a tree; `nodes[0]` is Owner.
#[derive(Debug, Default)]
pub(super) struct ChartModel {
    pub nodes: Vec<ChartNode>,
}

impl ChartModel {
    fn push(&mut self, parent: Option<usize>, key: String, kind: NodeKind, title: String, task_id: Option<String>, rank: u8) -> usize {
        let i = self.nodes.len();
        self.nodes.push(ChartNode { key, kind, title, task_id, sessions: Vec::new(), children: Vec::new(), parent, rank });
        if let Some(p) = parent {
            self.nodes[p].children.push(i);
        }
        i
    }

    pub(super) fn find(&self, key: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.key == key)
    }

    /// The node holding session `uid`, and its index in the box.
    pub(super) fn find_session(&self, uid: &str) -> Option<(usize, usize)> {
        self.nodes.iter().enumerate().find_map(|(i, n)| Some((i, n.sessions.iter().position(|s| s.uid == uid)?)))
    }

    /// Sessions in the subtree, the node's own included.
    pub(super) fn subtree_sessions(&self, i: usize) -> usize {
        self.nodes[i].sessions.len() + self.nodes[i].children.iter().map(|&c| self.subtree_sessions(c)).sum::<usize>()
    }

    /// Drop nodes with no session in their subtree (Owner always stays).
    fn prune(self) -> Self {
        let n = self.nodes.len();
        let mut keep = vec![false; n];
        // Children always follow their parent, so a reverse pass sees every
        // child before its parent.
        for i in (0..n).rev() {
            keep[i] = i == 0 || !self.nodes[i].sessions.is_empty() || self.nodes[i].children.iter().any(|&c| keep[c]);
        }
        let mut remap = vec![usize::MAX; n];
        let mut out = ChartModel::default();
        for (i, node) in self.nodes.into_iter().enumerate() {
            if !keep[i] {
                continue;
            }
            remap[i] = out.nodes.len();
            out.nodes.push(ChartNode { children: Vec::new(), parent: node.parent.map(|p| remap[p]), ..node });
        }
        for i in 0..out.nodes.len() {
            if let Some(p) = out.nodes[i].parent {
                out.nodes[p].children.push(i);
            }
        }
        out
    }

    fn sort_children(&mut self) {
        for i in 0..self.nodes.len() {
            let mut kids = std::mem::take(&mut self.nodes[i].children);
            kids.sort_by(|&a, &b| {
                let (a, b) = (&self.nodes[a], &self.nodes[b]);
                a.rank.cmp(&b.rank).then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase())).then_with(|| a.key.cmp(&b.key))
            });
            self.nodes[i].children = kids;
        }
    }

    /// Reports in display order: with more than `STACK_MIN` leaf reports
    /// the leaves come first (they stack in a column at the left), then
    /// the reports that have their own reports. Folded nodes show none.
    pub(super) fn visual_children(&self, i: usize, folds: &HashSet<String>) -> Vec<usize> {
        if folds.contains(&self.nodes[i].key) {
            return Vec::new();
        }
        let kids = &self.nodes[i].children;
        let leaf = |c: &usize| self.nodes[*c].children.is_empty() || folds.contains(&self.nodes[*c].key);
        let leaves = kids.iter().filter(|c| leaf(c)).count();
        if leaves <= STACK_MIN {
            return kids.clone();
        }
        kids.iter().copied().filter(|c| leaf(c)).chain(kids.iter().copied().filter(|c| !leaf(c))).collect()
    }

    /// Visible nodes in depth-first display order.
    pub(super) fn preorder(&self, folds: &HashSet<String>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![0];
        while let Some(i) = stack.pop() {
            out.push(i);
            stack.extend(self.visual_children(i, folds).into_iter().rev());
        }
        out
    }

    /// `i`'s ancestors, nearest first.
    fn ancestors(&self, i: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = self.nodes[i].parent;
        while let Some(p) = cur {
            out.push(p);
            cur = self.nodes[p].parent;
        }
        out
    }
}

/// A box's place in chart coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Placed {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Default)]
pub(super) struct ChartLayout {
    pub boxes: HashMap<usize, Placed>,
    /// Connector cells.
    pub lines: Vec<(i32, i32, char)>,
    pub width: i32,
    pub height: i32,
}

enum Unit {
    Sub(usize),
    /// Leaf reports stacked under a spine.
    Column(Vec<usize>),
}

struct Layouter<'a> {
    model: &'a ChartModel,
    zoom: Zoom,
    folds: &'a HashSet<String>,
    sizes: HashMap<usize, (i32, i32)>,
    out: ChartLayout,
}

impl Layouter<'_> {
    fn box_h(&self, i: usize) -> i32 {
        if self.zoom != Zoom::Full {
            return 1;
        }
        let n = self.model.nodes[i].sessions.len();
        let lines = if n == 0 { 1 } else if n > FULL_MAX_SESSIONS { FULL_MAX_SESSIONS } else { n };
        2 + lines as i32
    }

    fn units(&self, i: usize) -> Vec<Unit> {
        let kids = self.model.visual_children(i, self.folds);
        let leaf = |c: &usize| self.model.visual_children(*c, self.folds).is_empty();
        let leaves: Vec<usize> = kids.iter().copied().filter(|c| leaf(c)).collect();
        if leaves.len() <= STACK_MIN {
            return kids.into_iter().map(Unit::Sub).collect();
        }
        std::iter::once(Unit::Column(leaves)).chain(kids.into_iter().filter(|c| !leaf(c)).map(Unit::Sub)).collect()
    }

    fn unit_size(&mut self, u: &Unit) -> (i32, i32) {
        match u {
            Unit::Sub(c) => self.measure(*c),
            Unit::Column(ls) => (2 + self.zoom.box_w(), ls.iter().map(|&l| self.box_h(l)).sum()),
        }
    }

    fn band(&mut self, i: usize) -> (Vec<Unit>, i32, i32) {
        let units = self.units(i);
        let (mut w, mut h) = (0, 0);
        for (k, u) in units.iter().enumerate() {
            let (uw, uh) = self.unit_size(u);
            w += uw + if k > 0 { self.zoom.hgap() } else { 0 };
            h = h.max(uh);
        }
        (units, w, h)
    }

    fn measure(&mut self, i: usize) -> (i32, i32) {
        if let Some(s) = self.sizes.get(&i) {
            return *s;
        }
        let (bw, bh) = (self.zoom.box_w(), self.box_h(i));
        let (units, band_w, band_h) = self.band(i);
        let size = if units.is_empty() { (bw, bh) } else { (bw.max(band_w), bh + self.zoom.vgap() + band_h) };
        self.sizes.insert(i, size);
        size
    }

    fn line(&mut self, x: i32, y: i32, c: char) {
        self.out.lines.push((x, y, c));
    }

    fn assign(&mut self, i: usize, x: i32, y: i32) {
        let (w, _) = self.measure(i);
        let (bw, bh) = (self.zoom.box_w(), self.box_h(i));
        let bx = x + (w - bw) / 2;
        self.out.boxes.insert(i, Placed { x: bx, y, w: bw, h: bh });
        let (units, band_w, _) = self.band(i);
        if units.is_empty() {
            return;
        }
        let px = bx + bw / 2;
        let child_y = y + bh + self.zoom.vgap();
        let bus_y = child_y - 1;
        let arm = if self.zoom == Zoom::Full { 1 } else { 0 };
        let mut cur = x + (w - band_w) / 2;
        let mut attach = Vec::new();
        for u in &units {
            let (uw, _) = self.unit_size(u);
            match u {
                Unit::Sub(c) => {
                    let (cw, _) = self.measure(*c);
                    attach.push(cur + (cw - bw) / 2 + bw / 2);
                    self.assign(*c, cur, child_y);
                }
                Unit::Column(ls) => {
                    attach.push(cur);
                    let mut yy = child_y;
                    let last_arm = child_y + ls.iter().take(ls.len() - 1).map(|&l| self.box_h(l)).sum::<i32>() + arm;
                    for row in child_y..last_arm {
                        self.line(cur, row, '\u{2502}');
                    }
                    for (k, &l) in ls.iter().enumerate() {
                        let lh = self.box_h(l);
                        let a = yy + arm;
                        self.line(cur, a, if k + 1 == ls.len() { '\u{2514}' } else { '\u{251c}' });
                        self.line(cur + 1, a, '\u{2500}');
                        self.out.boxes.insert(l, Placed { x: cur + 2, y: yy, w: bw, h: lh });
                        yy += lh;
                    }
                }
            }
            cur += uw + self.zoom.hgap();
        }
        for row in y + bh..bus_y {
            self.line(px, row, '\u{2502}');
        }
        let lo = attach.iter().copied().chain([px]).min().unwrap_or(px);
        let hi = attach.iter().copied().chain([px]).max().unwrap_or(px);
        for xx in lo..=hi {
            let c = junction(xx == px, attach.contains(&xx), xx > lo, xx < hi);
            self.line(xx, bus_y, c);
        }
    }
}

/// The box-drawing character joining the given directions.
fn junction(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (true, true, true, true) => '\u{253c}',
        (true, true, true, false) => '\u{2524}',
        (true, true, false, true) => '\u{251c}',
        (true, false, true, true) => '\u{2534}',
        (true, false, true, false) => '\u{2518}',
        (true, false, false, true) => '\u{2514}',
        (false, true, true, true) => '\u{252c}',
        (false, true, true, false) => '\u{2510}',
        (false, true, false, true) => '\u{250c}',
        (true, _, false, false) | (false, true, false, false) => '\u{2502}',
        _ => '\u{2500}',
    }
}

pub(super) fn layout(model: &ChartModel, zoom: Zoom, folds: &HashSet<String>) -> ChartLayout {
    let mut l = Layouter { model, zoom, folds, sizes: HashMap::new(), out: ChartLayout::default() };
    if model.nodes.is_empty() {
        return l.out;
    }
    let (w, h) = l.measure(0);
    l.assign(0, 0, 0);
    l.out.width = w;
    l.out.height = h;
    l.out
}

/// Spawn time from a `ts-<nanos hex>-<counter hex>` uid, in unix seconds.
fn spawned_at(uid: &str) -> Option<i64> {
    let nanos = uid.strip_prefix("ts-")?.split('-').next()?;
    Some((u128::from_str_radix(nanos, 16).ok()? / 1_000_000_000) as i64)
}

/// Spawn order for sorting a box's sessions (unparseable uids last).
fn spawn_key(uid: &str) -> (u8, u128, u64, String) {
    let parsed = uid.strip_prefix("ts-").and_then(|r| {
        let (n, c) = r.split_once('-')?;
        Some((u128::from_str_radix(n, 16).ok()?, u64::from_str_radix(c, 16).ok()?))
    });
    match parsed {
        Some((n, c)) => (0, n, c, uid.to_owned()),
        None => (1, 0, 0, uid.to_owned()),
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

fn short_engine(t: &str) -> &str {
    match t {
        "claude" | "claude-code" => "claude",
        other => other,
    }
}

/// One board item a session holds.
#[derive(Clone, Debug)]
pub(super) struct HeldItem {
    pub board: String,
    pub item: Value,
}

type BoardReply = (Option<String>, Result<Value, String>);

/// `board.read` for every open board, round-robin, only while visible.
#[derive(Default)]
pub(super) struct ChartBoards {
    slugs: Vec<String>,
    /// Open items per board slug.
    pub(super) items: BTreeMap<String, Vec<Value>>,
    /// Last read attempt per board, success or failure, so a failing board
    /// waits its turn instead of starving the rest.
    read_at: HashMap<String, Instant>,
    /// The last read error per board.
    pub(super) read_errors: BTreeMap<String, String>,
    listed_at: Option<Instant>,
    retry_at: Option<Instant>,
    rx: Option<mpsc::Receiver<BoardReply>>,
    error: String,
}

impl ChartBoards {
    /// The open items session `uid` holds, by board then number.
    pub(super) fn held_by(&self, uid: &str) -> Vec<HeldItem> {
        let mut out = Vec::new();
        for (board, items) in &self.items {
            for item in items {
                let holds = item["holders"].as_array().into_iter().flatten().any(|h| h["session_uid"] == uid);
                if holds {
                    out.push(HeldItem { board: board.clone(), item: item.clone() });
                }
            }
        }
        out
    }
}

#[derive(Default)]
pub(super) struct OrgChart {
    pub(super) visible: bool,
    pub(super) zoom: Zoom,
    /// Focused node key, and the session within its box.
    pub(super) focus: Option<String>,
    pub(super) session: usize,
    pub(super) folds: HashSet<String>,
    pub(super) show_bash: bool,
    /// The bash session the chart was opened on: shown while bash is
    /// hidden, until the chart closes or `b` is pressed.
    pub(super) bash_uid: Option<String>,
    pub(super) hide_info: bool,
    /// Viewport origin in chart coordinates. Cells so draw can keep the
    /// focused box in view.
    ox: Cell<i32>,
    oy: Cell<i32>,
    /// Scroll the focused box into view on the next draw (cleared by a pan).
    follow: Cell<bool>,
    /// Chart viewport size at the last draw, for `c` (centre).
    viewport: Cell<(u16, u16)>,
    /// `g` or `z` waiting for its second key.
    pending: Option<char>,
    /// `/` query being typed, and where the cursor was when it began.
    search: Option<String>,
    search_origin: Option<(Option<String>, usize)>,
    last_query: Option<String>,
    status: String,
    last_paint: Option<Instant>,
    pub(super) boards: ChartBoards,
}

impl App {
    fn chart_task_title(&self, task_id: &str) -> String {
        self.tasks
            .iter()
            .find(|t| t.task_id.as_deref() == Some(task_id))
            .map(|t| t.name.clone())
            .unwrap_or_else(|| format!("task {}", task_id.chars().take(8).collect::<String>()))
    }

    /// Build the chart from the live session state.
    pub(super) fn org_model(&self, show_bash: bool) -> ChartModel {
        let mut m = ChartModel::default();
        let owner = m.push(None, "owner".into(), NodeKind::Owner, "Owner".into(), None, 0);
        // The one bash session the chart was opened on shows even with bash hidden.
        let opened_on = self.orgchart.bash_uid.as_deref();
        let keep = |ts: &TerminalSession| show_bash || ts.session_type != "bash" || opened_on == Some(ts.uid.as_str());
        let leads = self.task_leads();
        let parents = self.task_parent_map();
        let mut orgs: HashMap<String, usize> = HashMap::new();
        let mut org = |m: &mut ChartModel, key: &str, title: &str, rank: u8| -> usize {
            *orgs.entry(key.to_owned()).or_insert_with(|| m.push(Some(owner), key.to_owned(), NodeKind::Org, title.to_owned(), None, rank))
        };

        // Continuous org: the continuous column's grouping, verbatim. Tasks
        // it places map to their boxes, so a deeper subtask the column
        // doesn't group still lands under its parent here.
        let mut orch_of_task: HashMap<String, usize> = HashMap::new();
        let (mut orch, mut worker): (Option<usize>, Option<usize>) = (None, None);
        for r in self.continuous_rows_unfiltered() {
            let Some(si) = r.sess_idx else { continue };
            let ws = &self.workspaces[r.ws_idx];
            let ts = &ws.sessions[si];
            let cs = ChartSession { uid: ts.uid.clone(), wi: r.ws_idx, si, lead: false };
            let target = match (r.depth, orch) {
                (0, _) | (_, None) => {
                    let cont = org(&mut m, "continuous", "Continuous", 2);
                    let task = self.continuous_planning_task(ws, ts).map(str::to_owned);
                    let key = format!("orch:{}:{}", ws.host_id, ts.continuous_task_id.as_deref().unwrap_or(&ts.uid));
                    let title = task.as_deref().map_or_else(|| ts.label.clone(), |t| self.chart_task_title(t));
                    let i = m.push(Some(cont), key, NodeKind::Orchestrator, title, task.clone(), 0);
                    if let Some(t) = task {
                        orch_of_task.insert(t, i);
                    }
                    orch = Some(i);
                    worker = None;
                    i
                }
                // A retired predecessor orchestrator rides in the anchor's box.
                (1, Some(o)) if ts.continuous_task_id.is_some() => {
                    worker = None;
                    o
                }
                (1, Some(o)) => {
                    let otask = m.nodes[o].task_id.clone();
                    let task = ts.task_id.clone().filter(|t| Some(t) != otask.as_ref());
                    let title = task.as_deref().map_or_else(|| ts.label.clone(), |t| self.chart_task_title(t));
                    let i = m.push(Some(o), format!("worker:{}:{}", ws.host_id, task.as_deref().unwrap_or(&ts.uid)), NodeKind::Worker, title, task.clone(), 1);
                    if let Some(t) = task {
                        orch_of_task.insert(t, i);
                    }
                    worker = Some(i);
                    i
                }
                (_, Some(o)) => worker.unwrap_or(o),
            };
            if keep(ts) {
                m.nodes[target].sessions.push(cs);
            }
        }

        // Everyone else, by task.
        let members = self.continuous_members();
        let mut by_task: BTreeMap<String, Vec<ChartSession>> = BTreeMap::new();
        let mut loose: Vec<(usize, Vec<ChartSession>)> = Vec::new();
        for (wi, ws) in self.workspaces.iter().enumerate().filter(|(_, w)| !w.is_closed) {
            let mut unbound = Vec::new();
            for (si, ts) in ws.sessions.iter().enumerate() {
                if members.contains(&(wi, si)) || !keep(ts) {
                    continue;
                }
                let cs = ChartSession { uid: ts.uid.clone(), wi, si, lead: false };
                match &ts.task_id {
                    Some(t) => by_task.entry(t.clone()).or_default().push(cs),
                    None => unbound.push(cs),
                }
            }
            if !unbound.is_empty() {
                loose.push((wi, unbound));
            }
        }
        let mut task_node: HashMap<String, usize> = HashMap::new();
        for (task, sessions) in by_task {
            // The chain up to a node that already exists, an orchestrator's
            // task, or a root; a cycle ends the walk where it repeats.
            let mut chain = vec![task.clone()];
            let mut seen = HashSet::from([task.clone()]);
            let mut anchor = None;
            loop {
                let cur = chain.last().unwrap();
                if let Some(&i) = task_node.get(cur).or_else(|| orch_of_task.get(cur)) {
                    chain.pop();
                    anchor = Some(i);
                    break;
                }
                match parents.get(cur.as_str()) {
                    Some(p) if seen.insert((*p).to_owned()) => chain.push((*p).to_owned()),
                    _ => break,
                }
            }
            let mut parent = match anchor {
                Some(i) => i,
                None => {
                    let root = chain.last().cloned().unwrap_or_default();
                    match self.planning.task_division(&root) {
                        Some(d) => org(&mut m, &format!("div:{}", d.id), &d.name, 0),
                        None => org(&mut m, "standalone", "Standalone", 1),
                    }
                }
            };
            for t in chain.iter().rev() {
                let rank = if self.planning.task_division(t).is_some_and(|d| d.coordinator) { 0 } else { 1 };
                parent = m.push(Some(parent), format!("task:{t}"), NodeKind::Task, self.chart_task_title(t), Some(t.clone()), rank);
                task_node.insert(t.clone(), parent);
            }
            let i = task_node.get(&task).copied().unwrap_or(parent);
            m.nodes[i].sessions.extend(sessions);
        }
        for (wi, sessions) in loose {
            let s = org(&mut m, "standalone", "Standalone", 1);
            let ws = &self.workspaces[wi];
            let i = m.push(Some(s), format!("ws:{}", ws.id), NodeKind::Workspace, ws.name.clone(), None, 2);
            m.nodes[i].sessions = sessions;
        }

        // Leads, then sessions lead-first in spawn order.
        for node in &mut m.nodes {
            let lead = match node.kind {
                NodeKind::Orchestrator => node.sessions.first().map(|s| s.uid.clone()),
                _ => node.task_id.as_ref().and_then(|t| leads.get(t)).cloned(),
            };
            for s in &mut node.sessions {
                s.lead = lead.as_deref() == Some(s.uid.as_str()) && !self.workspaces[s.wi].sessions[s.si].session.exited;
            }
            if !matches!(node.kind, NodeKind::Orchestrator | NodeKind::Worker) {
                node.sessions.sort_by(|a, b| b.lead.cmp(&a.lead).then_with(|| spawn_key(&a.uid).cmp(&spawn_key(&b.uid))));
            } else {
                node.sessions.sort_by_key(|s| !s.lead);
            }
        }
        let mut m = m.prune();
        m.sort_children();
        m
    }

    fn chart_ts(&self, s: &ChartSession) -> Option<&TerminalSession> {
        self.workspaces.get(s.wi)?.sessions.get(s.si).filter(|ts| ts.uid == s.uid)
    }

    /// The sidebar's glyph and colour for a session (draw.rs's session row),
    /// with exited sessions as `×`.
    pub(super) fn chart_glyph(&self, ts: &TerminalSession, now: Instant) -> (&'static str, Style) {
        if ts.session.exited {
            return ("\u{00d7}", Style::default().fg(theme::DIM));
        }
        if self.session_has_alert(&ts.uid) {
            return self.alert_indicator();
        }
        if self.revive_failed.contains_key(&ts.uid) {
            return ("\u{2717}", Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD));
        }
        if self.owner_blocked_session(&ts.uid) {
            return self.owner_indicator();
        }
        if self.reconnecting_sessions.contains(&ts.uid) {
            return ("\u{27f3}", Style::default().fg(theme::ATTN));
        }
        if let Some(engine) = self.agent_indicator(ts) {
            return engine;
        }
        if self.display_running(ts) {
            return (self.spinner_frame(), Style::default().fg(theme::OK));
        }
        let color = match self.display_idle_bucket(ts, now) {
            Some(IdleAgeBucket::Afterglow) => theme::AFTERGLOW,
            Some(IdleAgeBucket::Stale) => theme::DIM,
            _ => theme::TEXT,
        };
        ("\u{25cf}", Style::default().fg(color))
    }

    /// `(status word, seconds in it)` for the info pane.
    fn chart_status(&self, ts: &TerminalSession) -> (String, Option<i64>) {
        use super::agent_state::Activity;
        if ts.session.exited {
            return ("exited".into(), None);
        }
        let engine = self.agent_view(ts);
        let since = engine
            .map(|v| (board::now_unix() as f64 - v.since).max(0.0) as i64)
            .or_else(|| ts.idle_since.map(|t| t.elapsed().as_secs() as i64));
        let word = match self.agent_activity(ts) {
            Some(Activity::Working) => "working",
            Some(Activity::Background) => "background work",
            Some(Activity::Waiting) => "waiting on a human",
            Some(Activity::Errored) => "errored",
            Some(Activity::Idle) => "idle",
            Some(Activity::Starting) => "starting",
            Some(Activity::Exited) => "exited",
            Some(Activity::Unknown) => "unknown",
            None if ts.status == SessionStatus::Running => "running",
            None => "idle",
        };
        let since = if word == "running" { None } else { since };
        (word.into(), since)
    }

    fn chart_host_marker(&self, ts: &TerminalSession) -> (String, Style) {
        if ts.host_id == cm_daemon::host_id::HostId::local() {
            ("\u{2302}".into(), Style::default().fg(theme::DIM))
        } else {
            let c = ts.host_id.as_str().chars().next().unwrap_or('?');
            (c.to_string(), Style::default().fg(theme::REMOTE))
        }
    }

    // ---- open / close / focus ---------------------------------------------

    fn open_orgchart(&mut self) {
        let c = &mut self.orgchart;
        c.visible = true;
        c.pending = None;
        c.search = None;
        c.status.clear();
        c.follow.set(true);
        c.boards.listed_at = None;
        let uid = self.cursor_session_uid();
        if let Some(uid) = uid {
            let bash = self.find_session_by_uid(&uid).map(|(wi, si)| self.workspaces[wi].sessions[si].session_type == "bash");
            if bash == Some(true) {
                self.orgchart.bash_uid = Some(uid.clone());
            }
            let model = self.org_model(self.orgchart.show_bash);
            if let Some((i, s)) = model.find_session(&uid) {
                self.orgchart_reveal(&model, i);
                self.orgchart.focus = Some(model.nodes[i].key.clone());
                self.orgchart.session = s;
            }
        }
        self.orgchart_tick();
    }

    fn close_orgchart(&mut self) {
        self.orgchart.visible = false;
        self.orgchart.bash_uid = None;
        self.orgchart.pending = None;
        self.orgchart.search = None;
    }

    /// Unfold `i`'s ancestors so it is visible.
    fn orgchart_reveal(&mut self, model: &ChartModel, i: usize) {
        for a in model.ancestors(i) {
            self.orgchart.folds.remove(&model.nodes[a].key);
        }
    }

    /// The focused node index, falling back to Owner when the key is gone
    /// or hidden under a fold.
    pub(super) fn orgchart_focus(&self, model: &ChartModel) -> usize {
        let c = &self.orgchart;
        let Some(i) = c.focus.as_deref().and_then(|k| model.find(k)) else { return 0 };
        // Under a fold: the nearest visible ancestor.
        let hidden_by = model.ancestors(i).into_iter().rev().find(|&a| c.folds.contains(&model.nodes[a].key));
        hidden_by.unwrap_or(i)
    }

    fn orgchart_set_focus(&mut self, model: &ChartModel, i: usize) {
        self.orgchart.focus = Some(model.nodes[i].key.clone());
        self.orgchart.session = 0;
        self.orgchart.follow.set(true);
    }

    /// The focused session, if the focused box has any.
    pub(super) fn orgchart_focused_session(&self, model: &ChartModel) -> Option<ChartSession> {
        let node = &model.nodes[self.orgchart_focus(model)];
        node.sessions.get(self.orgchart.session.min(node.sessions.len().saturating_sub(1))).cloned()
    }

    /// Enter: back to the Sessions view with the focused session selected.
    fn orgchart_jump(&mut self, model: &ChartModel) {
        let Some(s) = self.orgchart_focused_session(model) else {
            self.orgchart.status = "No session in this box".into();
            return;
        };
        self.close_orgchart();
        // Nothing may sit over the Sessions view: other overlays close and
        // an open form or prompt is cancelled.
        self.board.visible = false;
        self.messages.visible = false;
        self.input_mode = InputMode::Normal;
        self.view_mode = ViewMode::Sessions;
        let continuous = self.find_session_by_uid(&s.uid).is_some_and(|k| self.continuous_members().contains(&k));
        if continuous && !self.continuous_column_on {
            self.continuous_column_on = true;
            self.set_status_msg("Continuous column shown (A-c hides it)");
        }
        // The palette jump unfolds the sidebar above the session (reveal_cursor).
        self.apply_palette_jump(PaletteTarget::Session { uid: s.uid });
        self.needs_redraw = true;
    }

    fn orgchart_matches(&self, model: &ChartModel, query: &str) -> Vec<(usize, usize)> {
        let q = query.to_lowercase();
        let mut out = Vec::new();
        // Full depth-first order, folds ignored, so matches under a fold count.
        for i in model.preorder(&HashSet::new()) {
            let node = &model.nodes[i];
            let mut hit = false;
            for (k, s) in node.sessions.iter().enumerate() {
                if self.chart_ts(s).is_some_and(|ts| ts.label.to_lowercase().contains(&q)) {
                    out.push((i, k));
                    hit = true;
                }
            }
            if !hit && node.title.to_lowercase().contains(&q) {
                out.push((i, 0));
            }
        }
        out
    }

    /// Jump to the next (`dir > 0`) or previous match after the cursor.
    fn orgchart_search_step(&mut self, model: &ChartModel, query: &str, dir: i32, inclusive: bool) -> bool {
        let matches = self.orgchart_matches(model, query);
        if matches.is_empty() {
            self.orgchart.status = format!("No match for {query}");
            return false;
        }
        let order = model.preorder(&HashSet::new());
        let pos = |&(i, k): &(usize, usize)| (order.iter().position(|&o| o == i).unwrap_or(0), k);
        let here = (order.iter().position(|&o| o == self.orgchart_focus(model)).unwrap_or(0), self.orgchart.session);
        let pick = if dir > 0 {
            matches.iter().find(|m| if inclusive { pos(m) >= here } else { pos(m) > here }).or(matches.first())
        } else {
            matches.iter().rev().find(|m| pos(m) < here).or(matches.last())
        };
        let &(i, k) = pick.unwrap();
        let n = matches.iter().position(|m| *m == (i, k)).unwrap_or(0) + 1;
        self.orgchart_reveal(model, i);
        self.orgchart_set_focus(model, i);
        self.orgchart.session = k;
        self.orgchart.status = format!("match {n}/{}", matches.len());
        true
    }

    // ---- events -----------------------------------------------------------

    /// F6 opens the chart; while open it takes every key and mouse event.
    pub(super) fn orgchart_event(&mut self, event: &CrosstermEvent) -> bool {
        let CrosstermEvent::Key(key) = event else {
            return self.orgchart.visible;
        };
        if !self.orgchart.visible {
            if key.code == KeyCode::F(6) && key.modifiers.is_empty() {
                self.open_orgchart();
                return true;
            }
            return false;
        }
        self.needs_redraw = true;
        let model = self.org_model(self.orgchart.show_bash);
        if let Some(mut query) = self.orgchart.search.take() {
            match key.code {
                KeyCode::Esc => {
                    if let Some((focus, session)) = self.orgchart.search_origin.take() {
                        self.orgchart.focus = focus;
                        self.orgchart.session = session;
                        self.orgchart.follow.set(true);
                    }
                    self.orgchart.status.clear();
                    return true;
                }
                KeyCode::Enter => {
                    self.orgchart.search_origin = None;
                    self.orgchart.last_query = (!query.is_empty()).then_some(query);
                    return true;
                }
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Char(c) if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => query.push(c),
                _ => {}
            }
            if let Some((focus, session)) = self.orgchart.search_origin.clone() {
                self.orgchart.focus = focus;
                self.orgchart.session = session;
            }
            if !query.is_empty() {
                self.orgchart_search_step(&model, &query, 1, true);
            }
            self.orgchart.search = Some(query);
            return true;
        }
        let pending = self.orgchart.pending.take();
        let focus = self.orgchart_focus(&model);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let (step_x, step_y) = match self.orgchart.zoom {
            Zoom::Full => (16, 6),
            Zoom::Compact => (12, 4),
            Zoom::Dense => (8, 3),
        };
        match (pending, key.code) {
            (Some('g'), KeyCode::Char('g')) => self.orgchart_set_focus(&model, 0),
            (Some('z'), KeyCode::Char(c @ ('c' | 'o' | 'a'))) => {
                let k = model.nodes[focus].key.clone();
                let folded = self.orgchart.folds.contains(&k);
                if model.nodes[focus].children.is_empty() {
                    self.orgchart.status = "Nothing to fold".into();
                } else if c == 'c' || (c == 'a' && !folded) {
                    self.orgchart.folds.insert(k);
                } else {
                    self.orgchart.folds.remove(&k);
                }
                self.orgchart.follow.set(true);
            }
            (_, KeyCode::F(6) | KeyCode::Esc | KeyCode::Char('q')) => self.close_orgchart(),
            (_, KeyCode::Enter) => self.orgchart_jump(&model),
            (_, KeyCode::Char('t')) if alt => self.orgchart_jump(&model),
            (_, KeyCode::Char(c @ ('g' | 'z'))) => self.orgchart.pending = Some(c),
            (_, KeyCode::Char('G')) => {
                if let Some(&last) = model.preorder(&self.orgchart.folds).last() {
                    self.orgchart_set_focus(&model, last);
                }
            }
            (_, KeyCode::Char('h')) | (_, KeyCode::Char('l')) => {
                let next = key.code == KeyCode::Char('l');
                if let Some(p) = model.nodes[focus].parent {
                    let sibs = model.visual_children(p, &self.orgchart.folds);
                    let at = sibs.iter().position(|&s| s == focus).unwrap_or(0);
                    let to = if next { sibs.get(at + 1) } else { at.checked_sub(1).and_then(|a| sibs.get(a)) };
                    if let Some(&to) = to {
                        self.orgchart_set_focus(&model, to);
                    }
                }
            }
            (_, KeyCode::Char('j')) => {
                if let Some(&c) = model.visual_children(focus, &self.orgchart.folds).first() {
                    self.orgchart_set_focus(&model, c);
                }
            }
            (_, KeyCode::Char('k')) => {
                if let Some(p) = model.nodes[focus].parent {
                    self.orgchart_set_focus(&model, p);
                }
            }
            (_, KeyCode::Tab) | (_, KeyCode::BackTab) => {
                let n = model.nodes[focus].sessions.len();
                if n > 0 {
                    let cur = self.orgchart.session.min(n - 1);
                    self.orgchart.session = if key.code == KeyCode::Tab { (cur + 1) % n } else { (cur + n - 1) % n };
                }
            }
            (_, KeyCode::Char('/')) => {
                self.orgchart.search_origin = Some((self.orgchart.focus.clone(), self.orgchart.session));
                self.orgchart.search = Some(String::new());
            }
            (_, KeyCode::Char(c @ ('n' | 'N'))) => match self.orgchart.last_query.clone() {
                Some(q) => {
                    self.orgchart_search_step(&model, &q, if c == 'n' { 1 } else { -1 }, false);
                }
                None => self.orgchart.status = "No search yet (/)".into(),
            },
            (_, KeyCode::Char('+' | '=')) => {
                self.orgchart.zoom = self.orgchart.zoom.zoom_in();
                self.orgchart.follow.set(true);
            }
            (_, KeyCode::Char('-' | '_')) => {
                self.orgchart.zoom = self.orgchart.zoom.zoom_out();
                self.orgchart.follow.set(true);
            }
            (_, KeyCode::Char('b')) => {
                self.orgchart.bash_uid = None;
                self.orgchart.show_bash = !self.orgchart.show_bash;
                self.orgchart.status = if self.orgchart.show_bash { "Showing bash sessions" } else { "Bash sessions hidden" }.into();
                self.orgchart.follow.set(true);
            }
            (_, KeyCode::Char('i')) => self.orgchart.hide_info = !self.orgchart.hide_info,
            (_, KeyCode::Char('c')) => self.orgchart_centre(&model),
            (_, KeyCode::Char('H')) | (_, KeyCode::Left) => self.orgchart_pan(-step_x, 0),
            (_, KeyCode::Char('L')) | (_, KeyCode::Right) => self.orgchart_pan(step_x, 0),
            (_, KeyCode::Char('K')) | (_, KeyCode::Up) => self.orgchart_pan(0, -step_y),
            (_, KeyCode::Char('J')) | (_, KeyCode::Down) => self.orgchart_pan(0, step_y),
            _ => {}
        }
        true
    }

    fn orgchart_pan(&mut self, dx: i32, dy: i32) {
        let c = &self.orgchart;
        c.follow.set(false);
        c.ox.set(c.ox.get() + dx);
        c.oy.set(c.oy.get() + dy);
    }

    /// Centre the viewport on the focused box (`c`).
    fn orgchart_centre(&mut self, model: &ChartModel) {
        let lay = layout(model, self.orgchart.zoom, &self.orgchart.folds);
        if let Some(p) = lay.boxes.get(&self.orgchart_focus(model)) {
            let (vw, vh) = self.orgchart.viewport.get();
            self.orgchart.ox.set(p.x + p.w / 2 - vw as i32 / 2);
            self.orgchart.oy.set(p.y + p.h / 2 - vh as i32 / 2);
            self.orgchart.follow.set(false);
        }
    }

    /// Replies, the open-only board poll, and the repaint cadence.
    pub fn orgchart_tick(&mut self) {
        if let Some((slug, result)) = self.orgchart.boards.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            let b = &mut self.orgchart.boards;
            b.rx = None;
            self.needs_redraw = true;
            match (slug, result) {
                (None, Err(e)) => {
                    b.error = e;
                    b.retry_at = Some(Instant::now() + BOARD_RETRY);
                }
                (Some(slug), Err(e)) => {
                    b.read_at.insert(slug.clone(), Instant::now());
                    b.read_errors.insert(slug, e);
                }
                (None, Ok(v)) => {
                    b.error.clear();
                    b.listed_at = Some(Instant::now());
                    b.slugs = v.as_array().into_iter().flatten().filter_map(|x| x["slug"].as_str().map(str::to_owned)).collect();
                    let slugs = b.slugs.clone();
                    b.items.retain(|s, _| slugs.contains(s));
                    b.read_at.retain(|s, _| slugs.contains(s));
                    b.read_errors.retain(|s, _| slugs.contains(s));
                }
                (Some(slug), Ok(v)) => {
                    b.read_errors.remove(&slug);
                    b.read_at.insert(slug.clone(), Instant::now());
                    if v["unchanged"] != true {
                        b.items.insert(slug, v["items"].as_array().cloned().unwrap_or_default());
                    }
                }
            }
        }
        if !self.orgchart.visible {
            return;
        }
        if self.orgchart.last_paint.is_none_or(|t| t.elapsed() >= REPAINT) {
            self.orgchart.last_paint = Some(Instant::now());
            self.needs_redraw = true;
        }
        let b = &self.orgchart.boards;
        if b.rx.is_some() || b.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        if b.listed_at.is_none_or(|t| t.elapsed() >= BOARD_LIST_EVERY) {
            let (url, token) = (self.config.api_url.clone(), self.config.api_token.clone());
            let (tx, rx) = mpsc::channel();
            self.orgchart.boards.rx = Some(rx);
            std::thread::spawn(move || {
                let r = crate::api::list_open_boards(&url, &token).map(Value::from).map_err(|e| e.to_string());
                let _ = tx.send((None, r));
            });
            return;
        }
        let due = b.slugs.iter().find(|s| b.read_at.get(*s).is_none_or(|t| t.elapsed() >= BOARD_READ_EVERY)).cloned();
        let Some(slug) = due else { return };
        let targets = self.owner_rpc_targets();
        let (tx, rx) = mpsc::channel();
        self.orgchart.boards.rx = Some(rx);
        std::thread::spawn(move || {
            let params = json!({"board": slug, "view": "full"});
            let mut errors = Vec::new();
            for (host, socket, token) in targets {
                match crate::client_session::rpc_messaging_board(&socket, &token, "board.read", params.clone()) {
                    Ok(v) => {
                        let _ = tx.send((Some(slug), Ok(v)));
                        return;
                    }
                    Err(e) => errors.push(format!("{host}: {e}")),
                }
            }
            let msg = if errors.is_empty() { "No daemon is reachable".to_owned() } else { errors.join("; ") };
            let _ = tx.send((Some(slug), Err(msg)));
        });
    }

    // ---- draw -------------------------------------------------------------

    pub(super) fn draw_orgchart(&self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let model = self.org_model(self.orgchart.show_bash);
        let focus = self.orgchart_focus(&model);
        let sessions = model.subtree_sessions(0);
        let title = Line::from(vec![
            Span::styled(" Org chart ", Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("· {sessions} sessions · zoom {} {}", self.orgchart.zoom.name(), if self.orgchart.show_bash { "· bash shown " } else { "" }),
                Style::default().fg(theme::MUTED),
            ),
        ]);
        let block = Block::default().borders(Borders::ALL).title(title).border_style(Style::default().fg(theme::TEXT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 || inner.width < 10 {
            return;
        }
        let status_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
        let body = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
        let (chart_area, info_area) = if self.orgchart.hide_info {
            (body, None)
        } else if body.width >= 100 {
            let w = INFO_WIDTH.min(body.width / 2);
            (Rect::new(body.x, body.y, body.width - w, body.height), Some(Rect::new(body.x + body.width - w, body.y, w, body.height)))
        } else {
            let h = 12.min(body.height / 2);
            (Rect::new(body.x, body.y, body.width, body.height - h), Some(Rect::new(body.x, body.y + body.height - h, body.width, h)))
        };
        self.orgchart.viewport.set((chart_area.width, chart_area.height));
        let lay = layout(&model, self.orgchart.zoom, &self.orgchart.folds);
        self.orgchart_follow(&lay, focus, chart_area);
        self.draw_chart_body(frame.buffer_mut(), chart_area, &model, &lay, focus);
        if let Some(info) = info_area {
            self.draw_chart_info(frame, info, &model, focus);
        }
        frame.render_widget(Paragraph::new(self.orgchart_status_line(status_area.width as usize)), status_area);
    }

    /// Scroll so the focused box is in view (only after a focus move).
    fn orgchart_follow(&self, lay: &ChartLayout, focus: usize, area: Rect) {
        let c = &self.orgchart;
        let (vw, vh) = (area.width as i32, area.height as i32);
        // Never scroll past the chart's edges more than needed.
        c.ox.set(c.ox.get().clamp(-vw / 2, (lay.width - vw / 2).max(0)));
        c.oy.set(c.oy.get().clamp(-vh / 2, (lay.height - vh / 2).max(0)));
        if !c.follow.get() {
            return;
        }
        let Some(p) = lay.boxes.get(&focus) else { return };
        let (mut ox, mut oy) = (c.ox.get(), c.oy.get());
        let margin_x = 2.min((vw - p.w).max(0) / 2);
        if p.x - margin_x < ox || p.w > vw {
            ox = p.x - margin_x;
        } else if p.x + p.w + margin_x > ox + vw {
            ox = p.x + p.w + margin_x - vw;
        }
        let margin_y = 1.min((vh - p.h).max(0) / 2);
        if p.y - margin_y < oy || p.h > vh {
            oy = p.y - margin_y;
        } else if p.y + p.h + margin_y > oy + vh {
            oy = p.y + p.h + margin_y - vh;
        }
        c.ox.set(ox);
        c.oy.set(oy);
    }

    fn draw_chart_body(&self, buf: &mut ratatui::buffer::Buffer, area: Rect, model: &ChartModel, lay: &ChartLayout, focus: usize) {
        let (ox, oy) = (self.orgchart.ox.get(), self.orgchart.oy.get());
        let put = |buf: &mut ratatui::buffer::Buffer, x: i32, y: i32, s: &str, style: Style| {
            let (vy, mut vx) = (y - oy, x - ox);
            if vy < 0 || vy >= area.height as i32 {
                return;
            }
            for ch in s.chars() {
                if vx >= area.width as i32 {
                    break;
                }
                if vx >= 0 {
                    let cell = &mut buf[(area.x + vx as u16, area.y + vy as u16)];
                    cell.set_char(ch);
                    cell.set_style(style);
                }
                vx += 1;
            }
        };
        let line_style = Style::default().fg(theme::DIM);
        for &(x, y, ch) in &lay.lines {
            put(buf, x, y, &ch.to_string(), line_style);
        }
        let now = Instant::now();
        for (&i, p) in &lay.boxes {
            if p.x + p.w < ox || p.x > ox + area.width as i32 || p.y + p.h < oy || p.y > oy + area.height as i32 {
                continue;
            }
            let node = &model.nodes[i];
            let focused = i == focus;
            let selected = focused.then(|| self.orgchart.session.min(node.sessions.len().saturating_sub(1)));
            let folded = self.orgchart.folds.contains(&node.key) && !node.children.is_empty();
            let hidden = if folded { model.subtree_sessions(i) - node.sessions.len() } else { 0 };
            let header = matches!(node.kind, NodeKind::Owner | NodeKind::Org);
            let head_glyph = match node.sessions.first().and_then(|s| self.chart_ts(s)) {
                Some(ts) => self.chart_glyph(ts, now),
                None if header => ("\u{25c6}", Style::default().fg(theme::HEADER)),
                None => ("\u{25cb}", Style::default().fg(theme::DIM)),
            };
            let focus_bg = |s: Style| if focused { s.bg(theme::SELECT_BG) } else { s };
            match self.orgchart.zoom {
                Zoom::Dense => {
                    put(buf, p.x, p.y, head_glyph.0, focus_bg(head_glyph.1));
                    if folded {
                        put(buf, p.x + 1, p.y, "\u{25b8}", focus_bg(Style::default().fg(theme::MUTED)));
                    } else if focused {
                        put(buf, p.x + 1, p.y, " ", focus_bg(Style::default()));
                    }
                }
                Zoom::Compact => {
                    let lead = node.sessions.first().is_some_and(|s| s.lead);
                    let extra = node.sessions.len().saturating_sub(1) + hidden;
                    let tail = match (folded, extra) {
                        (true, _) => format!(" \u{25b8}{extra}"),
                        (false, 0) => String::new(),
                        (false, n) => format!(" +{n}"),
                    };
                    let room = (p.w as usize).saturating_sub(3 + tail.chars().count());
                    let name = crate::planning::truncate_with_ellipsis(&node.title, room);
                    let text_style = if header { Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme::TEXT) };
                    put(buf, p.x, p.y, &" ".repeat(p.w as usize), focus_bg(Style::default()));
                    put(buf, p.x, p.y, if lead { "\u{2605}" } else { " " }, focus_bg(Style::default().fg(theme::ATTN)));
                    put(buf, p.x + 1, p.y, head_glyph.0, focus_bg(head_glyph.1));
                    put(buf, p.x + 3, p.y, &name, focus_bg(text_style));
                    put(buf, p.x + p.w - tail.chars().count() as i32, p.y, &tail, focus_bg(Style::default().fg(theme::MUTED)));
                }
                Zoom::Full => {
                    let border = if focused {
                        Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)
                    } else if header {
                        Style::default().fg(theme::HEADER)
                    } else {
                        Style::default().fg(theme::MUTED)
                    };
                    let w = p.w as usize;
                    let fold_tag = if folded { format!(" \u{25b8} {hidden} below") } else { String::new() };
                    let title = crate::planning::truncate_with_ellipsis(&node.title, w.saturating_sub(5 + fold_tag.chars().count()));
                    put(buf, p.x, p.y, &format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(w - 2)), border);
                    let title_style = if header { Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD) };
                    put(buf, p.x + 2, p.y, &format!(" {title} "), title_style);
                    put(buf, p.x + 3 + title.chars().count() as i32, p.y, &fold_tag, Style::default().fg(theme::MUTED));
                    for row in 1..p.h - 1 {
                        put(buf, p.x, p.y + row, "\u{2502}", border);
                        put(buf, p.x + 1, p.y + row, &" ".repeat(w - 2), Style::default());
                        put(buf, p.x + p.w - 1, p.y + row, "\u{2502}", border);
                    }
                    put(buf, p.x, p.y + p.h - 1, &format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(w - 2)), border);
                    if node.sessions.is_empty() {
                        let text = match node.kind {
                            NodeKind::Owner => format!("{} \u{00b7} {}", plural(node.children.len(), "org"), plural(model.subtree_sessions(i), "session")),
                            NodeKind::Org => format!("{} \u{00b7} {}", plural(node.children.len(), "report"), plural(model.subtree_sessions(i), "session")),
                            _ => "no session here".into(),
                        };
                        put(buf, p.x + 2, p.y + 1, &crate::planning::truncate_with_ellipsis(&text, w - 4), focus_bg(Style::default().fg(theme::DIM)));
                        if focused {
                            put(buf, p.x + 1, p.y + 1, " ", focus_bg(Style::default()));
                        }
                        continue;
                    }
                    let n = node.sessions.len();
                    let shown = if n > FULL_MAX_SESSIONS { FULL_MAX_SESSIONS - 1 } else { n };
                    // Keep the selected session on screen inside a long box.
                    let first = match selected {
                        Some(s) if s >= shown => s + 1 - shown,
                        _ => 0,
                    };
                    for (row, k) in (first..(first + shown).min(n)).enumerate() {
                        let s = &node.sessions[k];
                        let Some(ts) = self.chart_ts(s) else { continue };
                        let y = p.y + 1 + row as i32;
                        let sel = selected == Some(k);
                        let bg = |st: Style| if sel { st.bg(theme::SELECT_BG) } else { st };
                        let (g, gs) = self.chart_glyph(ts, now);
                        let (host, hs) = self.chart_host_marker(ts);
                        let engine = short_engine(&ts.session_type);
                        let label_room = w.saturating_sub(4 + 1 + engine.len() + 2 + 1);
                        let label = crate::planning::truncate_with_ellipsis(&ts.label, label_room);
                        let label_style = if ts.session.exited { Style::default().fg(theme::DIM) } else { Style::default().fg(theme::TEXT) };
                        put(buf, p.x + 1, y, &" ".repeat(w - 2), bg(Style::default()));
                        put(buf, p.x + 1, y, if s.lead { "\u{2605}" } else { " " }, bg(Style::default().fg(theme::ATTN)));
                        put(buf, p.x + 2, y, g, bg(gs));
                        put(buf, p.x + 4, y, &label, bg(label_style));
                        put(buf, p.x + p.w as i32 - 3 - engine.len() as i32, y, engine, bg(Style::default().fg(theme::MUTED)));
                        put(buf, p.x + p.w as i32 - 2, y, &host, bg(hs));
                    }
                    if n > shown {
                        let more = n - shown;
                        put(buf, p.x + 4, p.y + p.h - 2, &format!("+{more} more (Tab)"), Style::default().fg(theme::DIM));
                    }
                }
            }
        }
    }

    fn draw_chart_info(&self, frame: &mut Frame, area: Rect, model: &ChartModel, focus: usize) {
        let block = Block::default().borders(Borders::ALL).title(" Info ").border_style(Style::default().fg(theme::DIM));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = self.orgchart_info_lines(model, focus, inner.width as usize);
        frame.render_widget(Paragraph::new(lines), inner);
    }

    /// The info pane's lines for the focused box (and session).
    pub(super) fn orgchart_info_lines(&self, model: &ChartModel, focus: usize, width: usize) -> Vec<Line<'static>> {
        let node = &model.nodes[focus];
        let label = |s: &str| Span::styled(format!("{s:<9}"), Style::default().fg(theme::DIM));
        let text = |s: String| Span::styled(crate::planning::truncate_with_ellipsis(&s, width.saturating_sub(9)), Style::default().fg(theme::TEXT));
        let mut lines: Vec<Line<'static>> = Vec::new();
        let reports = node.children.len();
        let boss = node.parent.map(|p| &model.nodes[p]);
        let boss_text = |b: &ChartNode| {
            let lead = b.sessions.iter().find(|s| s.lead).and_then(|s| self.chart_ts(s)).map(|ts| ts.label.clone());
            match lead {
                Some(l) => format!("{} (lead {l})", b.title),
                None => b.title.clone(),
            }
        };
        let picked = self.orgchart.session.min(node.sessions.len().saturating_sub(1));
        let Some(s) = node.sessions.get(picked).cloned() else {
            lines.push(Line::styled(node.title.clone(), Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD)));
            let kind = match node.kind {
                NodeKind::Owner => "owner",
                NodeKind::Org if node.key.starts_with("div:") => "division",
                NodeKind::Org => "org",
                _ => "task (no session)",
            };
            lines.push(Line::from(vec![label("kind"), text(kind.into())]));
            if let Some(b) = boss {
                lines.push(Line::from(vec![label("reports"), text(format!("to {}", boss_text(b)))]));
            }
            lines.push(Line::from(vec![label("direct"), text(plural(reports, "report"))]));
            lines.push(Line::from(vec![label("sessions"), text(format!("{} in this subtree", model.subtree_sessions(focus)))]));
            return lines;
        };
        let Some(ts) = self.chart_ts(&s) else { return lines };
        let ws = &self.workspaces[s.wi];
        let now = Instant::now();
        let (g, gs) = self.chart_glyph(ts, now);
        let mut head = vec![Span::styled(format!("{g} "), gs), Span::styled(ts.label.clone(), Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD))];
        if s.lead {
            head.push(Span::styled("  \u{2605} lead", Style::default().fg(theme::ATTN)));
        }
        lines.push(Line::from(head));
        let (word, since) = self.chart_status(ts);
        let status = match since {
            Some(secs) => format!("{word} {}", board::age(secs)),
            None => word,
        };
        lines.push(Line::from(vec![label("engine"), text(short_engine(&ts.session_type).to_owned())]));
        lines.push(Line::from(vec![label("status"), text(status)]));
        let age = spawned_at(&ts.uid).map(|t| board::age(board::now_unix() - t)).unwrap_or_else(|| board::age(ts.created_at.elapsed().as_secs() as i64));
        lines.push(Line::from(vec![label("age"), text(format!("{age} since spawn"))]));
        let last = if !ts.session.exited && self.display_running(ts) {
            "now".to_owned()
        } else if let Some(secs) = since {
            format!("{} ago", board::age(secs))
        } else if let Some(w) = ts.last_write_at {
            format!("input {} ago", board::age(w.elapsed().as_secs() as i64))
        } else {
            "unknown".into()
        };
        lines.push(Line::from(vec![label("activity"), text(last)]));
        let task = node.task_id.as_ref().or(ts.task_id.as_ref());
        lines.push(Line::from(vec![label("task"), text(task.map_or_else(|| "none (workspace session)".into(), |_| node.title.clone()))]));
        if let Some(b) = boss {
            lines.push(Line::from(vec![label("boss"), text(boss_text(b))]));
        }
        lines.push(Line::from(vec![label("reports"), text(format!("{reports} direct"))]));
        let host = if ts.host_id == cm_daemon::host_id::HostId::local() { "local \u{2302}".to_owned() } else { ts.host_id.to_string() };
        lines.push(Line::from(vec![label("host"), text(host)]));
        let wt = ws.worktree_path.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "—".into());
        lines.push(Line::from(vec![label("worktree"), text(wt)]));
        lines.push(Line::from(""));
        let held = self.orgchart.boards.held_by(&ts.uid);
        let b = &self.orgchart.boards;
        let heading = if held.is_empty() && b.listed_at.is_none() && b.error.is_empty() {
            "Board items: loading…".to_owned()
        } else {
            format!("Board items ({})", held.len())
        };
        lines.push(Line::styled(heading, Style::default().fg(theme::HEADER)));
        if !b.error.is_empty() {
            lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&format!("boards: {}", b.error), width), Style::default().fg(theme::ERROR)));
        }
        for (slug, e) in &b.read_errors {
            lines.push(Line::styled(crate::planning::truncate_with_ellipsis(&format!("{slug}: unreadable ({e})"), width), Style::default().fg(theme::ERROR)));
        }
        let unix = board::now_unix();
        for h in held {
            let item = &h.item;
            let (badge, badge_style) = board::status_badge(item, unix);
            let flags: Vec<String> = item["flags"].as_array().into_iter().flatten().filter_map(|f| f["kind"].as_str().map(str::to_owned)).collect();
            let mut spans = vec![
                Span::styled(format!("{} #{} ", h.board, item["n"]), Style::default().fg(theme::MUTED)),
                Span::styled(format!("{badge} "), badge_style),
            ];
            if item["status"] != "waiting" {
                if let Some(eta) = item["eta_at"].as_str().and_then(board::parse_rfc3339) {
                    let eta = if eta >= unix { format!("ETA {} ", board::age(eta - unix)) } else { format!("ETA +{} over ", board::age(unix - eta)) };
                    spans.push(Span::styled(eta, Style::default().fg(theme::HEADER)));
                }
            }
            if !flags.is_empty() {
                spans.push(Span::styled(format!("\u{2691} {} ", flags.join(",")), Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD)));
            }
            lines.push(Line::from(spans));
            lines.push(Line::styled(
                format!("  {}", crate::planning::truncate_with_ellipsis(item["title"].as_str().unwrap_or(""), width.saturating_sub(2))),
                Style::default().fg(theme::TEXT),
            ));
        }
        lines
    }

    fn orgchart_status_line(&self, width: usize) -> Line<'static> {
        let c = &self.orgchart;
        if let Some(q) = &c.search {
            return Line::from(vec![
                Span::styled("/", Style::default().fg(theme::ATTN)),
                Span::styled(q.clone(), Style::default().fg(theme::TEXT)),
                Span::styled(format!("  {}", c.status), Style::default().fg(theme::MUTED)),
            ]);
        }
        let help = "h/l sibling  j child  k boss  gg/G  Tab session  / n N  zc/zo fold  +/- zoom  HJKL pan  c centre  b bash  i info  Enter go  F6 close";
        let mut text = if c.status.is_empty() { help.to_owned() } else { format!("{}  \u{00b7}  {help}", c.status) };
        if let Some(p) = c.pending {
            text = format!("{p}…  {text}");
        }
        Line::styled(crate::planning::truncate_with_ellipsis(&text, width), Style::default().fg(theme::DIM))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::lifecycle::revive_session_tests::dummy_ts;
    use cm_daemon::host_id::HostId;
    use crossterm::event::KeyEvent;

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

    fn ws(id: &str, host: HostId) -> Workspace {
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
            host_id: host,
            sessions: vec![],
            tombstones: vec![],
        }
    }

    fn task(id: &str, name: &str, parent: Option<&str>) -> TaskEntry {
        TaskEntry {
            task_id: Some(id.into()),
            name: name.into(),
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

    /// Add a session `label` (uid `ts-<n>-0`) for `task` to workspace `wi`.
    fn session(app: &mut App, wi: usize, n: u32, label: &str, engine: &str, task: Option<&str>) -> String {
        let host = app.workspaces[wi].host_id.clone();
        let uid = format!("ts-{n:x}-0");
        let mut ts = dummy_ts(&uid, false, host);
        ts.label = label.into();
        ts.session_type = engine.into();
        ts.task_id = task.map(str::to_owned);
        app.workspaces[wi].sessions.push(ts);
        uid
    }

    fn key(c: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE))
    }

    fn press(app: &mut App, keys: &str) {
        for c in keys.chars() {
            app.handle_event(&key(KeyCode::Char(c)));
        }
    }

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height).map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn focused_title(app: &App) -> String {
        let m = app.org_model(app.orgchart.show_bash);
        m.nodes[app.orgchart_focus(&m)].title.clone()
    }

    /// root → mid → leaf (each in its own workspace), a standalone loose
    /// workspace session, and a bash session.
    fn org_app() -> App {
        let mut app = test_app();
        app.tasks = vec![task("root", "Root lead", None), task("mid", "Middle", Some("root")), task("leaf", "Leaf work", Some("mid"))];
        app.workspaces = vec![ws("w-root", HostId::local()), ws("w-mid", HostId::new("sessions")), ws("w-leaf", HostId::new("manager")), ws("w-loose", HostId::local())];
        session(&mut app, 0, 0x20, "root-b", "codex", Some("root"));
        session(&mut app, 0, 0x10, "root-a", "claude", Some("root"));
        session(&mut app, 0, 0x30, "root-sh", "bash", Some("root"));
        session(&mut app, 1, 0x40, "mid-1", "codex", Some("mid"));
        session(&mut app, 2, 0x50, "leaf-1", "codex", Some("leaf"));
        session(&mut app, 3, 0x60, "scratch", "claude", None);
        app
    }

    #[test]
    fn model_follows_the_task_tree_with_leads_first_and_bash_hidden() {
        let app = org_app();
        let m = app.org_model(false);
        let titles: Vec<&str> = m.preorder(&HashSet::new()).into_iter().map(|i| m.nodes[i].title.as_str()).collect();
        assert_eq!(titles, ["Owner", "Standalone", "Root lead", "Middle", "Leaf work", "w-loose"]);
        let root = &m.nodes[m.find("task:root").unwrap()];
        let uids: Vec<&str> = root.sessions.iter().map(|s| s.uid.as_str()).collect();
        // The earliest-spawned agent leads and sits on top; bash is hidden.
        assert_eq!(uids, ["ts-10-0", "ts-20-0"]);
        assert!(root.sessions[0].lead && !root.sessions[1].lead);
        // Loose workspace sessions have no task and so no lead.
        assert!(!m.nodes[m.find("ws:w-loose").unwrap()].sessions[0].lead);
        let with_bash = app.org_model(true);
        assert_eq!(with_bash.nodes[with_bash.find("task:root").unwrap()].sessions.len(), 3);
    }

    #[test]
    fn pinned_lead_wins_and_exited_never_leads() {
        let mut app = org_app();
        app.tasks[0].metadata = Some(json!({"lead_session_uid": "ts-20-0"}));
        let m = app.org_model(false);
        let root = &m.nodes[m.find("task:root").unwrap()];
        assert_eq!(root.sessions[0].uid, "ts-20-0");
        assert!(root.sessions[0].lead);
        app.workspaces[0].sessions[1].session.exited = true; // ts-10-0
        app.tasks[0].metadata = None;
        let m = app.org_model(false);
        let root = &m.nodes[m.find("task:root").unwrap()];
        assert_eq!(root.sessions.iter().find(|s| s.lead).map(|s| s.uid.as_str()), Some("ts-20-0"));
    }

    fn division_task(id: &str, parent: Option<&str>, coordinator: bool) -> crate::api::Task {
        serde_json::from_value(json!({
            "id": id, "slug": id, "created_at": "2026-10-20T00:00:00Z", "repo_url": "", "repo_branch": "main",
            "status": "running", "project": "p", "parent_task_id": parent,
            "initiative_id": "div-1",
            "initiative": {"id": "div-1", "name": "Trader", "kind": "division",
                           "coordinator_task_id": if coordinator { id } else { "root" }},
        }))
        .unwrap()
    }

    #[test]
    fn a_division_is_its_own_org_with_the_coordinator_first() {
        let mut app = org_app();
        app.tasks.push(task("side", "Another root", None));
        session(&mut app, 0, 0x11, "side-1", "codex", Some("side"));
        app.planning.update_from_api(vec![
            division_task("root", None, true),
            division_task("mid", Some("root"), false),
            division_task("side", None, false),
        ]);
        let m = app.org_model(false);
        let div = m.find("div:div-1").expect("division org");
        assert_eq!(m.nodes[div].title, "Trader");
        let orgs: Vec<&str> = m.nodes[0].children.iter().map(|&c| m.nodes[c].key.as_str()).collect();
        assert_eq!(orgs, ["div:div-1", "standalone"], "divisions come first");
        let roots: Vec<&str> = m.nodes[div].children.iter().map(|&c| m.nodes[c].title.as_str()).collect();
        assert_eq!(roots, ["Root lead", "Another root"], "the coordinator's tree leads the division");
        let standalone = m.find("standalone").unwrap();
        let loose: Vec<&str> = m.nodes[standalone].children.iter().map(|&c| m.nodes[c].title.as_str()).collect();
        assert_eq!(loose, ["w-loose"]);
        // The info pane names the division.
        let lines: Vec<String> = app.orgchart_info_lines(&m, div, 50).iter().map(|l| l.to_string()).collect();
        assert!(lines.iter().any(|l| l.contains("division")), "{lines:?}");
    }

    #[test]
    fn missing_ancestors_get_boxes_and_cycles_terminate() {
        let mut app = test_app();
        // "child" reports to "gone" (not in the task list) which reports to "child": a loop.
        app.tasks = vec![task("child", "Child", Some("gone")), task("gone", "Gone parent", Some("child"))];
        app.workspaces = vec![ws("w", HostId::local())];
        session(&mut app, 0, 1, "c", "codex", Some("child"));
        let m = app.org_model(false);
        assert!(m.find("task:child").is_some());
        assert!(m.find("task:gone").is_some(), "the ancestor without sessions still gets a box");
        assert_eq!(m.subtree_sessions(0), 1);
    }

    #[test]
    fn continuous_tasks_are_their_own_org_reusing_the_column_grouping() {
        let mut app = org_app();
        app.tasks.push(task("orch-task", "Health triage", None));
        app.tasks.push(task("bug-1", "bug one", Some("orch-task")));
        app.tasks.push(task("bug-1-sub", "bug one deeper", Some("bug-1")));
        app.workspaces.push(ws("w-orch", HostId::new("manager")));
        app.workspaces.push(ws("w-bug", HostId::new("manager")));
        app.workspaces.push(ws("w-deep", HostId::new("manager")));
        let o = session(&mut app, 4, 0x70, "health-orchestrator", "codex", Some("orch-task"));
        app.workspaces[4].sessions[0].continuous_task_id = Some("health".into());
        session(&mut app, 5, 0x80, "bug-1-fix", "codex", Some("bug-1"));
        app.workspaces[5].sessions[0].managed_by_uid = Some(o.clone());
        // A grandchild the column doesn't group still lands in the
        // Continuous org, under its parent's box.
        session(&mut app, 6, 0x90, "deeper", "claude", Some("bug-1-sub"));
        let m = app.org_model(false);
        let cont = m.find("continuous").unwrap();
        assert_eq!(m.nodes[cont].parent, Some(0));
        let orch = m.find("orch:manager:health").unwrap();
        assert_eq!(m.nodes[orch].parent, Some(cont));
        assert!(m.nodes[orch].sessions[0].lead);
        let worker = m.find("worker:manager:bug-1").unwrap();
        assert_eq!(m.nodes[worker].parent, Some(orch));
        let deep = m.find("task:bug-1-sub").unwrap();
        assert_eq!(m.nodes[deep].parent, Some(worker));
        // Owner's orgs: Standalone before Continuous.
        let orgs: Vec<&str> = m.nodes[0].children.iter().map(|&c| m.nodes[c].key.as_str()).collect();
        assert_eq!(orgs, ["standalone", "continuous"]);
        // The continuous sessions are not also in the main org.
        assert_eq!(m.nodes.iter().filter(|n| n.sessions.iter().any(|s| s.uid == o)).count(), 1);
    }

    fn assert_no_overlap(lay: &ChartLayout) {
        let boxes: Vec<&Placed> = lay.boxes.values().collect();
        for (a, pa) in boxes.iter().enumerate() {
            for pb in &boxes[a + 1..] {
                let apart = pa.x + pa.w <= pb.x || pb.x + pb.w <= pa.x || pa.y + pa.h <= pb.y || pb.y + pb.h <= pa.y;
                assert!(apart, "{pa:?} overlaps {pb:?}");
            }
        }
        let cells: HashSet<(i32, i32)> = lay.lines.iter().map(|&(x, y, _)| (x, y)).collect();
        for p in &boxes {
            for &(x, y) in &cells {
                assert!(!(x >= p.x && x < p.x + p.w && y >= p.y && y < p.y + p.h), "connector ({x},{y}) inside {p:?}");
            }
        }
        for p in boxes {
            assert!(p.x >= 0 && p.y >= 0 && p.x + p.w <= lay.width && p.y + p.h <= lay.height, "{p:?} outside {}x{}", lay.width, lay.height);
        }
    }

    /// A chain `depth` tasks deep, one session each.
    fn deep_app(depth: usize) -> App {
        let mut app = test_app();
        app.workspaces = vec![ws("w", HostId::local())];
        for d in 0..depth {
            let parent = (d > 0).then(|| format!("t{}", d - 1));
            app.tasks.push(task(&format!("t{d}"), &format!("level {d}"), parent.as_deref()));
            session(&mut app, 0, 0x100 + d as u32, &format!("s{d}"), "codex", Some(&format!("t{d}")));
        }
        app
    }

    /// One lead with `n` direct reports.
    fn wide_app(n: usize) -> App {
        let mut app = test_app();
        app.workspaces = vec![ws("w", HostId::local())];
        app.tasks.push(task("boss", "The boss", None));
        session(&mut app, 0, 1, "boss", "claude", Some("boss"));
        for k in 0..n {
            app.tasks.push(task(&format!("r{k:02}"), &format!("report {k:02}"), Some("boss")));
            session(&mut app, 0, 0x100 + k as u32, &format!("rep-{k:02}"), "codex", Some(&format!("r{k:02}")));
        }
        app
    }

    #[test]
    fn layouts_never_overlap_at_any_zoom_for_deep_and_wide_trees() {
        for app in [deep_app(12), wide_app(60), org_app()] {
            let m = app.org_model(true);
            for zoom in [Zoom::Full, Zoom::Compact, Zoom::Dense] {
                let lay = layout(&m, zoom, &HashSet::new());
                assert_eq!(lay.boxes.len(), m.nodes.len(), "{zoom:?}: every node placed");
                assert_no_overlap(&lay);
                // Every report sits below its boss.
                for (i, n) in m.nodes.iter().enumerate() {
                    if let Some(p) = n.parent {
                        assert!(lay.boxes[&i].y > lay.boxes[&p].y, "{zoom:?}: {} below {}", n.title, m.nodes[p].title);
                    }
                }
            }
        }
    }

    #[test]
    fn wide_fan_out_stacks_leaf_reports_in_a_column() {
        let app = wide_app(60);
        let m = app.org_model(false);
        let boss = m.find("task:boss").unwrap();
        for zoom in [Zoom::Full, Zoom::Compact, Zoom::Dense] {
            let lay = layout(&m, zoom, &HashSet::new());
            let xs: HashSet<i32> = m.nodes[boss].children.iter().map(|c| lay.boxes[c].x).collect();
            assert_eq!(xs.len(), 1, "{zoom:?}: 60 leaf reports share one column");
            // The chart stays narrow: about two boxes wide.
            assert!(lay.width <= 2 * zoom.box_w() + 4, "{zoom:?}: width {}", lay.width);
        }
        // Dense packs a deep chain into a narrow, short chart.
        let deep = deep_app(12);
        let dm = deep.org_model(false);
        let full = layout(&dm, Zoom::Full, &HashSet::new());
        let dense = layout(&dm, Zoom::Dense, &HashSet::new());
        assert!(dense.height < full.height / 2 && dense.width < full.width);
    }

    #[test]
    fn three_or_fewer_leaf_reports_sit_in_a_row_with_a_bus() {
        let app = wide_app(3);
        let m = app.org_model(false);
        let lay = layout(&m, Zoom::Full, &HashSet::new());
        let boss = m.find("task:boss").unwrap();
        let ys: HashSet<i32> = m.nodes[boss].children.iter().map(|c| lay.boxes[c].y).collect();
        assert_eq!(ys.len(), 1, "one row");
        let bus_y = lay.boxes[&m.nodes[boss].children[0]].y - 1;
        let bus: String = {
            let mut cells: Vec<(i32, char)> = lay.lines.iter().filter(|l| l.1 == bus_y).map(|l| (l.0, l.2)).collect();
            cells.sort();
            cells.into_iter().map(|c| c.1).collect()
        };
        assert!(bus.starts_with('\u{250c}') && bus.ends_with('\u{2510}') && bus.contains('\u{253c}'), "{bus}");
    }

    #[test]
    fn folding_hides_a_subtree_and_counts_it() {
        let mut app = deep_app(4);
        app.orgchart.visible = true;
        app.orgchart.focus = Some("task:t1".into());
        press(&mut app, "zc");
        let m = app.org_model(false);
        assert_eq!(layout(&m, Zoom::Full, &app.orgchart.folds).boxes.len(), 4, "owner, standalone, t0, folded t1");
        let s = screen(&mut app, 140, 40);
        assert!(s.contains("level 1 \u{25b8} 2 below") || s.contains("\u{25b8} 2 below"), "{s}");
        press(&mut app, "zo");
        assert_eq!(layout(&m, Zoom::Full, &app.orgchart.folds).boxes.len(), 6);
    }

    #[test]
    fn vim_keys_walk_the_tree_and_search_jumps_into_folds() {
        let mut app = org_app();
        app.cursor = Cursor::Session(1, 0); // mid-1
        app.handle_event(&key(KeyCode::F(6)));
        assert!(app.orgchart.visible);
        assert_eq!(focused_title(&app), "Middle", "opens on the Sessions view's focus");
        press(&mut app, "k");
        assert_eq!(focused_title(&app), "Root lead");
        press(&mut app, "k");
        assert_eq!(focused_title(&app), "Standalone");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "Standalone", "no sibling to the right");
        press(&mut app, "j");
        assert_eq!(focused_title(&app), "Root lead");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "w-loose");
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "Root lead");
        app.handle_event(&key(KeyCode::Tab));
        assert_eq!(app.orgchart.session, 1);
        press(&mut app, "G");
        assert_eq!(focused_title(&app), "w-loose");
        press(&mut app, "gg");
        assert_eq!(focused_title(&app), "Owner");
        // Fold the root task, then search for a session under it.
        app.orgchart.focus = Some("task:root".into());
        press(&mut app, "zc");
        press(&mut app, "/leaf");
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(focused_title(&app), "Leaf work");
        assert!(!app.orgchart.folds.contains("task:root"), "search unfolds the match's ancestors");
        press(&mut app, "/root-");
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(focused_title(&app), "Root lead");
        let first = app.orgchart.session;
        press(&mut app, "n");
        assert_ne!(app.orgchart.session, first, "n steps to the next matching session");
        press(&mut app, "N");
        assert_eq!(app.orgchart.session, first);
        // Esc mid-search restores the focus.
        press(&mut app, "/scratch");
        assert_eq!(focused_title(&app), "w-loose");
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(focused_title(&app), "Root lead");
        assert!(app.orgchart.visible);
        app.handle_event(&key(KeyCode::Esc));
        assert!(!app.orgchart.visible);
    }

    #[test]
    fn enter_focuses_the_session_in_the_sessions_view() {
        let mut app = org_app();
        app.view_mode = ViewMode::Planning;
        app.cursor = Cursor::Session(3, 0);
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.focus = Some("task:leaf".into());
        app.handle_event(&key(KeyCode::Enter));
        assert!(!app.orgchart.visible);
        assert_eq!(app.view_mode, ViewMode::Sessions);
        assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-50-0"));
        // Opening again starts on that session.
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "Leaf work");
        // Owner has no session: Enter stays put and says so.
        press(&mut app, "gg");
        app.handle_event(&key(KeyCode::Enter));
        assert!(app.orgchart.visible);
    }

    #[test]
    fn opening_on_a_bash_session_shows_only_that_one_until_close() {
        let mut app = org_app();
        app.workspaces[3].sessions.push(dummy_ts("ts-61-0", false, HostId::local()));
        app.workspaces[3].sessions[1].session_type = "bash".into();
        app.cursor = Cursor::Session(0, 2); // root-sh
        app.handle_event(&key(KeyCode::F(6)));
        assert!(!app.orgchart.show_bash, "the bash toggle is untouched");
        let m = app.org_model(false);
        assert_eq!(app.orgchart_focused_session(&m).unwrap().uid, "ts-30-0");
        assert!(m.find_session("ts-61-0").is_none(), "other bash sessions stay hidden");
        app.handle_event(&key(KeyCode::Esc));
        assert!(app.orgchart.bash_uid.is_none());
        assert!(app.org_model(false).find_session("ts-30-0").is_none(), "closing restores the filter");
        // `b` still toggles every bash session.
        app.handle_event(&key(KeyCode::F(6)));
        press(&mut app, "b");
        assert!(app.orgchart.show_bash && app.orgchart.bash_uid.is_none());
        assert!(app.org_model(true).find_session("ts-61-0").is_some());
    }

    #[test]
    fn the_same_continuous_task_on_two_hosts_gets_two_boxes() {
        let mut app = test_app();
        app.workspaces = vec![ws("w-a", HostId::new("manager")), ws("w-b", HostId::new("sessions"))];
        for wi in 0..2 {
            session(&mut app, wi, 0x10 + wi as u32, "triage-orchestrator", "codex", Some("orch-task"));
            app.workspaces[wi].sessions[0].continuous_task_id = Some("triage".into());
        }
        let m = app.org_model(false);
        let a = m.find("orch:manager:triage").expect("manager's box");
        let b = m.find("orch:sessions:triage").expect("sessions' box");
        assert_ne!(a, b);
        assert_eq!(m.nodes.iter().filter(|n| n.key.starts_with("orch:")).count(), 2);
    }

    #[test]
    fn a_failing_board_waits_its_turn_and_reports_only_itself() {
        let mut app = org_app();
        app.orgchart.visible = true;
        app.orgchart.boards.listed_at = Some(Instant::now());
        app.orgchart.boards.slugs = vec!["bad".into(), "good".into()];
        app.orgchart.boards.items.insert("good".into(), vec![]);
        let (tx, rx) = mpsc::channel();
        app.orgchart.boards.rx = Some(rx);
        tx.send((Some("bad".into()), Err("boom".into()))).unwrap();
        app.orgchart_tick();
        let b = &app.orgchart.boards;
        assert!(b.read_at.contains_key("bad"), "a failed read counts as an attempt");
        assert_eq!(b.read_errors.get("bad").map(String::as_str), Some("boom"));
        assert!(b.error.is_empty() && b.retry_at.is_none(), "one board's failure doesn't pause the poll");
        // The next request goes to the other board.
        let rx = app.orgchart.boards.rx.take().expect("next read in flight");
        drop(rx);
        assert!(app.orgchart.boards.read_at.get("good").is_none());
        let m = app.org_model(false);
        let text: Vec<String> = app.orgchart_info_lines(&m, m.find("task:mid").unwrap(), 60).iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("bad: unreadable (boom)")), "{text:?}");
        assert!(!text.iter().any(|l| l.contains("good:")), "{text:?}");
        // A good read clears that board's error.
        let (tx, rx) = mpsc::channel();
        app.orgchart.boards.rx = Some(rx);
        tx.send((Some("bad".into()), Ok(json!({"items": []})))).unwrap();
        app.orgchart_tick();
        assert!(app.orgchart.boards.read_errors.is_empty());
    }

    #[test]
    fn the_jump_closes_other_overlays_and_cancels_input() {
        let mut app = org_app();
        app.board.visible = true;
        app.messages.visible = true;
        app.input_mode = InputMode::SidebarSearch { query: String::new() };
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.focus = Some("task:leaf".into());
        app.handle_event(&key(KeyCode::Enter));
        assert!(!app.orgchart.visible && !app.board.visible && !app.messages.visible);
        assert!(matches!(app.input_mode, InputMode::Normal));
        assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-50-0"));
    }

    #[test]
    fn every_zoom_renders_and_keeps_the_focus_in_view() {
        let mut app = wide_app(60);
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.focus = Some("task:r59".into());
        app.orgchart.follow.set(true);
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("Org chart"), "{s}");
        assert!(s.contains("rep-59"), "the focused report scrolled into view:\n{s}");
        press(&mut app, "-");
        assert_eq!(app.orgchart.zoom, Zoom::Compact);
        app.orgchart.focus = Some("task:r00".into());
        app.orgchart.follow.set(true);
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("report 00"), "{s}");
        press(&mut app, "-");
        assert_eq!(app.orgchart.zoom, Zoom::Dense);
        let s = screen(&mut app, 120, 30);
        assert!(s.contains("zoom dense"), "{s}");
        press(&mut app, "--");
        assert_eq!(app.orgchart.zoom, Zoom::Dense, "dense is the floor");
        press(&mut app, "+++");
        assert_eq!(app.orgchart.zoom, Zoom::Full);
        // Panning moves the viewport and stops following.
        let before = app.orgchart.ox.get();
        press(&mut app, "L");
        assert!(!app.orgchart.follow.get());
        assert_eq!(app.orgchart.ox.get(), before + 16);
        // Narrow terminals put the info pane underneath.
        let s = screen(&mut app, 80, 40);
        assert!(s.contains(" Info "), "{s}");
    }

    #[test]
    fn cards_show_engine_and_host_markers_for_every_host() {
        let mut app = org_app();
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.hide_info = true;
        let s = screen(&mut app, 200, 40);
        let row = |label: &str| s.lines().find(|l| l.contains(label)).unwrap_or_else(|| panic!("{label}:\n{s}")).to_owned();
        assert!(row("root-a").contains("\u{2605}"), "lead star: {}", row("root-a"));
        assert!(row("root-a").contains("claude \u{2302}"), "local host: {}", row("root-a"));
        assert!(row("mid-1").contains("codex s"), "sessions host: {}", row("mid-1"));
        assert!(row("leaf-1").contains("codex m"), "manager host: {}", row("leaf-1"));
    }

    #[test]
    fn info_pane_shows_session_task_boss_and_held_board_items() {
        let mut app = org_app();
        app.cursor = Cursor::Session(1, 0); // mid-1
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.boards.listed_at = Some(Instant::now());
        app.orgchart.boards.items.insert(
            "sejd".into(),
            vec![
                json!({"n":14,"status":"active","title":"fuse kernels","eta_at":"2999-01-01T00:00:00Z","flags":[{"kind":"holder_idle"}],
                       "holders":[{"name":"mid-1","session_uid":"ts-40-0"}]}),
                json!({"n":15,"status":"open","title":"not mine","holders":[{"name":"x","session_uid":"ts-99-0"}]}),
            ],
        );
        let m = app.org_model(false);
        let text: Vec<String> = app.orgchart_info_lines(&m, app.orgchart_focus(&m), 60).iter().map(|l| l.to_string()).collect();
        let joined = text.join("\n");
        assert!(joined.contains("mid-1") && joined.contains("\u{2605} lead"), "{joined}");
        assert!(joined.contains("engine   codex"), "{joined}");
        assert!(joined.contains("since spawn"), "{joined}");
        assert!(joined.contains("task     Middle"), "{joined}");
        assert!(joined.contains("boss     Root lead (lead root-a)"), "{joined}");
        assert!(joined.contains("reports  1 direct"), "{joined}");
        assert!(joined.contains("host     sessions"), "{joined}");
        assert!(joined.contains("worktree /wt/w-mid"), "{joined}");
        assert!(joined.contains("Board items (1)"), "{joined}");
        assert!(joined.contains("sejd #14 ACTIVE") && joined.contains("ETA") && joined.contains("holder_idle"), "{joined}");
        assert!(joined.contains("fuse kernels") && !joined.contains("not mine"), "{joined}");
    }

    #[test]
    fn closed_chart_takes_no_keys_and_does_not_poll() {
        let mut app = org_app();
        assert!(!app.orgchart_event(&key(KeyCode::Char('j'))));
        app.orgchart_tick();
        assert!(app.orgchart.boards.rx.is_none(), "no board request while closed");
    }

    #[test]
    fn spawn_time_comes_from_the_uid() {
        assert_eq!(spawned_at("ts-3b9aca00-0"), Some(1));
        assert_eq!(spawned_at("bogus"), None);
        assert!(spawn_key("ts-9-0") < spawn_key("ts-10-0"));
        assert!(spawn_key("ts-10-0") < spawn_key("ts-10-1"));
        assert!(spawn_key("ts-ff-0") < spawn_key("ts-zz"));
    }
}
