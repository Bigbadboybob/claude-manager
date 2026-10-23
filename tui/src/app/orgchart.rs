//! F6: the org chart (Owner request 2026-10-20; spec: Swarm-Coord's
//! org-design PLAN.md, "orgchart").
//!
//! Every session on every host, laid out as an org chart. Owner sits at the
//! top; under it each project (then Other), split into Teams (its divisions,
//! then its other root tasks) and Continuous; under those, root tasks and
//! their subtasks along `parent_task_id`. A task
//! box holds that task's sessions with its lead (the shared lead rule) on
//! top. The Continuous org reuses the continuous column's grouping
//! (`continuous_rows_unfiltered`). Reporting lines only: nothing here
//! implies approval rights.
//!
//! A leader (a task with live reports, the leader marks' predicate, or a
//! continuous orchestrator) stacks its reports that have none of their own
//! in one column under it; reports with reports fan out beside that column.
//! Reports of structural boxes (Owner, projects, splits, divisions) never
//! stack.
//!
//! The overlay takes every key while open: h/l along the row (a stack is
//! one slot; at a stack's end, the leader's row), j back to the
//! last-focused child (else the first) or down a stack, k up a stack, else
//! parent, gg/G, Tab/S-Tab a session
//! within the box, / n N search, Space/zc/zo/za fold, zM/zR fold/unfold the
//! projects and splits, +/- zoom (ten levels), H/J/K/L or arrows pan,
//! c centre, b bash, i info pane, ? symbol legend (app/symbols.rs). Enter
//! (or A-t) and every exit (F6/Esc/q) return to the Sessions view focused on
//! the chart's session. Apart from stacks every level is one row; the
//! viewport follows the focus.
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
const INFO_WIDTH: u16 = 46;
/// A project's normal work: its divisions, then its other root tasks.
const TEAMS_LABEL: &str = "Teams";
/// A project's continuous orchestrators and their workers.
const CONTINUOUS_LABEL: &str = "Continuous";
/// The project node for tasks and workspaces with no project.
const OTHER_PROJECT_LABEL: &str = "Other";

/// How much a box shows at a zoom level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Detail {
    /// Bordered cards: task title, every session with glyph, engine, host.
    Full,
    /// One line per box: lead glyph and the box's name.
    Compact,
    /// One glyph per box: the whole org at a glance.
    Dense,
}

/// (detail, box width, gap between siblings, session lines in a card),
/// most zoomed out first.
const ZOOM_LEVELS: [(Detail, i32, i32, usize); 10] = [
    (Detail::Dense, 2, 0, 0),
    (Detail::Dense, 2, 1, 0),
    (Detail::Compact, 10, 1, 0),
    (Detail::Compact, 14, 1, 0),
    (Detail::Compact, 18, 1, 0),
    (Detail::Compact, 24, 2, 0),
    (Detail::Full, 22, 1, 2),
    (Detail::Full, 26, 2, 4),
    (Detail::Full, 30, 2, 6),
    (Detail::Full, 36, 2, 8),
];

/// An index into `ZOOM_LEVELS`; `+`/`-` step one level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Zoom(usize);

impl Default for Zoom {
    fn default() -> Self {
        Zoom::FULL
    }
}

impl Zoom {
    /// The widest card at the default card size.
    pub(super) const FULL: Zoom = Zoom(8);
    pub(super) const DENSE: Zoom = Zoom(0);
    pub(super) const MAX: Zoom = Zoom(ZOOM_LEVELS.len() - 1);

    pub(super) fn detail(self) -> Detail {
        ZOOM_LEVELS[self.0].0
    }
    fn box_w(self) -> i32 {
        ZOOM_LEVELS[self.0].1
    }
    fn hgap(self) -> i32 {
        ZOOM_LEVELS[self.0].2
    }
    /// Session lines in a full card before "+N more".
    fn max_sessions(self) -> usize {
        ZOOM_LEVELS[self.0].3
    }
    /// Rows between a box and its reports: stem rows, then the bus row.
    fn vgap(self) -> i32 {
        match self.detail() {
            Detail::Dense => 1,
            _ => 2,
        }
    }
    fn zoom_in(self) -> Self {
        Zoom((self.0 + 1).min(Zoom::MAX.0))
    }
    fn zoom_out(self) -> Self {
        Zoom(self.0.saturating_sub(1))
    }
    fn name(self) -> String {
        let d = match self.detail() {
            Detail::Full => "full",
            Detail::Compact => "compact",
            Detail::Dense => "dense",
        };
        format!("{d} {}/{}", self.0 + 1, ZOOM_LEVELS.len())
    }
}

#[derive(Clone, Copy)]
enum Split {
    Teams,
    Continuous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum NodeKind {
    Owner,
    /// A planning project (the task rows' `project`), or Other.
    Project,
    /// A project's split (Teams or Continuous), or a division under Teams.
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
    /// The leader mark it carries: the sidebar's, from the same
    /// [`App::leader_marks`] (a lead of a task with live reports, by level).
    pub mark: Option<&'static str>,
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
    /// A leader: a task with live reports (`OrgIndex::has_reports`, the
    /// predicate behind the leader marks), or a continuous orchestrator.
    /// Its reports that have no reports of their own stack in a column.
    pub leader: bool,
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
        self.nodes.push(ChartNode { key, kind, title, task_id, sessions: Vec::new(), children: Vec::new(), parent, leader: false, rank });
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

    /// Reports in display order (left to right). Folded nodes show none.
    pub(super) fn visual_children(&self, i: usize, folds: &HashSet<String>) -> Vec<usize> {
        let mut out = self.stack(i, folds);
        out.extend(self.branches(i, folds));
        out
    }

    /// Whether `i` sits in its boss's stack: the boss is a leader and `i`
    /// has no reports in the chart. A stacked box is the bottom of its
    /// branch. Structural nodes are never leaders, so their reports never
    /// stack.
    pub(super) fn stacked(&self, i: usize) -> bool {
        self.nodes[i].children.is_empty() && self.nodes[i].parent.is_some_and(|p| self.nodes[p].leader)
    }

    /// `i`'s stacked reports, top to bottom (none when folded).
    pub(super) fn stack(&self, i: usize, folds: &HashSet<String>) -> Vec<usize> {
        if folds.contains(&self.nodes[i].key) {
            return Vec::new();
        }
        self.nodes[i].children.iter().copied().filter(|&c| self.stacked(c)).collect()
    }

    /// `i`'s reports laid out side by side (none when folded).
    pub(super) fn branches(&self, i: usize, folds: &HashSet<String>) -> Vec<usize> {
        if folds.contains(&self.nodes[i].key) {
            return Vec::new();
        }
        self.nodes[i].children.iter().copied().filter(|&c| !self.stacked(c)).collect()
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

struct Layouter<'a> {
    model: &'a ChartModel,
    zoom: Zoom,
    folds: &'a HashSet<String>,
    sizes: HashMap<usize, (i32, i32)>,
    out: ChartLayout,
}

impl Layouter<'_> {
    fn box_h(&self, i: usize) -> i32 {
        if self.zoom.detail() != Detail::Full {
            return 1;
        }
        let n = self.model.nodes[i].sessions.len();
        let lines = n.clamp(1, self.zoom.max_sessions());
        2 + lines as i32
    }

    /// A box's reports in layout order: its stack (one column) first, then
    /// its branches side by side. The chart grows as wide as the branches
    /// need.
    fn units(&self, i: usize) -> Vec<Unit> {
        let stack = self.model.stack(i, self.folds);
        let mut out = Vec::new();
        if !stack.is_empty() {
            out.push(Unit::Column(stack));
        }
        out.extend(self.model.branches(i, self.folds).into_iter().map(Unit::Sub));
        out
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
        let size = match units.as_slice() {
            [] => (bw, bh),
            // Only a stack: a list straight under the box, one spine row below it.
            [Unit::Column(_)] => (bw.max(1 + band_w), bh + 1 + band_h),
            _ => (bw.max(band_w), bh + self.zoom.vgap() + band_h),
        };
        self.sizes.insert(i, size);
        size
    }

    fn line(&mut self, x: i32, y: i32, c: char) {
        self.out.lines.push((x, y, c));
    }

    /// A stack under spine `sx`: the spine runs from row `top`, and the
    /// items start at row `y`, two cells right of it.
    fn column(&mut self, ls: &[usize], sx: i32, top: i32, y: i32) {
        // Full cards: the arm meets the card's first session row.
        let arm = if self.zoom.detail() == Detail::Full { 1 } else { 0 };
        let last_arm = y + ls.iter().take(ls.len() - 1).map(|&l| self.box_h(l)).sum::<i32>() + arm;
        for row in top..last_arm {
            self.line(sx, row, '\u{2502}');
        }
        let (bw, mut yy) = (self.zoom.box_w(), y);
        for (k, &l) in ls.iter().enumerate() {
            let lh = self.box_h(l);
            self.line(sx, yy + arm, if k + 1 == ls.len() { '\u{2514}' } else { '\u{251c}' });
            self.line(sx + 1, yy + arm, '\u{2500}');
            self.out.boxes.insert(l, Placed { x: sx + 2, y: yy, w: bw, h: lh });
            yy += lh;
        }
    }

    fn assign(&mut self, i: usize, x: i32, y: i32) {
        let (w, _) = self.measure(i);
        let (bw, bh) = (self.zoom.box_w(), self.box_h(i));
        let (units, band_w, _) = self.band(i);
        if let [Unit::Column(ls)] = units.as_slice() {
            // The leader sits at the left, its list hangs from a spine
            // under its second cell.
            self.out.boxes.insert(i, Placed { x, y, w: bw, h: bh });
            self.column(ls, x + 1, y + bh, y + bh + 1);
            return;
        }
        if units.is_empty() {
            self.out.boxes.insert(i, Placed { x: x + (w - bw) / 2, y, w: bw, h: bh });
            return;
        }
        let child_y = y + bh + self.zoom.vgap();
        let bus_y = child_y - 1;
        let mut cur = x + (w - band_w) / 2;
        let mut attach = Vec::new();
        for u in &units {
            let (uw, _) = self.unit_size(u);
            match u {
                Unit::Sub(c) => {
                    // Over the box as placed: a leader with only a stack
                    // sits at the left of its branch, not centred.
                    self.assign(*c, cur, child_y);
                    attach.push(self.out.boxes[c].x + bw / 2);
                }
                Unit::Column(ls) => {
                    attach.push(cur);
                    self.column(ls, cur, child_y, child_y);
                }
            }
            cur += uw + self.zoom.hgap();
        }
        // Centred over the band; over a lone report's attach point, so a
        // leader drawn at the left of its branch gets a straight line.
        let bx = match attach.as_slice() {
            [a] => (a - bw / 2).clamp(x, x + w - bw),
            _ => x + (w - bw) / 2,
        };
        self.out.boxes.insert(i, Placed { x: bx, y, w: bw, h: bh });
        let px = bx + bw / 2;
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

/// A box's reports as laid out: one branch, or a leader's stack.
enum Unit {
    Sub(usize),
    Column(Vec<usize>),
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

/// `?`, second line: the activity badge's marks (`symbols::activity_legend`).
fn orgchart_activity_legend_line() -> Line<'static> {
    let mut spans = Vec::new();
    for (sym, style, word) in symbols::activity_legend() {
        spans.push(Span::styled(sym, style));
        spans.push(Span::styled(format!(" {word}  "), Style::default().fg(theme::MUTED)));
    }
    Line::from(spans)
}

/// `?`: every symbol on the chart with its meaning, from `symbols::legend`.
fn orgchart_legend_line() -> Line<'static> {
    let mut spans = Vec::new();
    for (sym, style, word) in symbols::legend() {
        spans.push(Span::styled(sym, style));
        spans.push(Span::styled(format!(" {word}  "), Style::default().fg(theme::MUTED)));
    }
    let header = Style::default().fg(theme::HEADER);
    for (sym, style, word) in [(symbols::HEADER_BOX, header, "org  "), (symbols::DIVISION, header, "division  "), ("\u{25b8}", Style::default().fg(theme::MUTED), "folded")] {
        spans.push(Span::styled(sym, style));
        spans.push(Span::styled(format!(" {word}"), Style::default().fg(theme::MUTED)));
    }
    Line::from(spans)
}

/// Planning rows by task id, built once per chart rebuild.
type TaskRows<'a> = HashMap<&'a str, &'a TaskEntry>;

fn chart_task_title(tasks: &TaskRows, task_id: &str) -> String {
    tasks.get(task_id).map(|t| t.name.clone()).unwrap_or_else(|| format!("task {}", task_id.chars().take(8).collect::<String>()))
}

/// A task's project: the nearest one set on it or an ancestor.
fn chart_task_project(tasks: &TaskRows, task: &str, parents: &HashMap<&str, &str>) -> Option<String> {
    let mut cur = task;
    let mut seen = HashSet::new();
    while seen.insert(cur) {
        if let Some(p) = tasks.get(cur).and_then(|t| t.project.clone()) {
            return Some(p);
        }
        cur = parents.get(cur)?;
    }
    None
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
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
    /// Focused node key, and the selected session's uid within its box.
    /// By uid, not index: a box re-sorts (lead first) on every rebuild.
    pub(super) focus: Option<String>,
    pub(super) session: Option<String>,
    /// The sidebar cursor when the chart opened, restored by an exit that
    /// has no session to go to.
    opened_from: Option<(Cursor, SidebarColumn)>,
    pub(super) folds: HashSet<String>,
    /// Each node's last-focused report (node key → child key), so `j`
    /// goes back down the way `k` came up.
    pub(super) last_child: HashMap<String, String>,
    /// The session last focused in each box (node key → uid).
    pub(super) last_session: HashMap<String, String>,
    /// Where each slot of a row was left by `h`/`l`: a branch's key, or
    /// `<leader key>#stack` for a leader's stack → the box focused then
    /// (the branch's head or any box under it, or a stacked box).
    pub(super) branch_focus: HashMap<String, String>,
    pub(super) show_bash: bool,
    /// The bash session the chart was opened on: shown while bash is
    /// hidden, until the chart closes or `b` is pressed.
    pub(super) bash_uid: Option<String>,
    pub(super) hide_info: bool,
    /// `?`: the symbol legend above the help line.
    pub(super) legend: bool,
    /// Viewport origin in chart coordinates. Cells so draw can keep the
    /// focused box in view.
    ox: Cell<i32>,
    oy: Cell<i32>,
    /// Scroll the focused box into view on the next draw (cleared by a pan
    /// or `c`; any focus move sets it again).
    follow: Cell<bool>,
    /// The focused box's key and chart position at the last draw. While
    /// not following, a refresh that moves the box shifts the viewport with
    /// it, so it stays where the pan left it.
    anchor: std::cell::RefCell<Option<(String, i32, i32)>>,
    /// Chart viewport size at the last draw, for `c` (centre).
    viewport: Cell<(u16, u16)>,
    /// `g` or `z` waiting for its second key.
    pending: Option<char>,
    /// `/` query being typed, and where the cursor was when it began.
    search: Option<String>,
    search_origin: Option<(Option<String>, Option<String>)>,
    last_query: Option<String>,
    status: String,
    last_paint: Option<Instant>,
    pub(super) boards: ChartBoards,
}

impl App {

    /// Build the chart from the live session state.
    pub(super) fn org_model(&self, show_bash: bool) -> ChartModel {
        let mut m = ChartModel::default();
        let owner = m.push(None, "owner".into(), NodeKind::Owner, "Owner".into(), None, 0);
        // The one bash session the chart was opened on shows even with bash hidden.
        let opened_on = self.orgchart.bash_uid.as_deref();
        let keep = |ts: &TerminalSession| show_bash || ts.session_type != "bash" || opened_on == Some(ts.uid.as_str());
        let leads = self.task_leads();
        let parents = self.task_parent_map();
        // Owner → project → its split (Teams or Continuous) → divisions.
        // Each is made on first use, so empty ones never appear.
        let mut orgs: HashMap<String, usize> = HashMap::new();
        let mut org = |m: &mut ChartModel, project: Option<&str>, split: Split, division: Option<(&str, &str)>| -> usize {
            let pkey = format!("proj:{}", project.unwrap_or(""));
            let proj = *orgs.entry(pkey.clone()).or_insert_with(|| {
                let (title, rank) = match project {
                    Some(p) => (p.to_owned(), 0),
                    None => (OTHER_PROJECT_LABEL.to_owned(), 1),
                };
                m.push(Some(owner), pkey.clone(), NodeKind::Project, title, None, rank)
            });
            let (skey, stitle, srank) = match split {
                Split::Teams => (format!("teams:{pkey}"), TEAMS_LABEL, 0),
                Split::Continuous => (format!("continuous:{pkey}"), CONTINUOUS_LABEL, 1),
            };
            let split_node = *orgs.entry(skey.clone()).or_insert_with(|| m.push(Some(proj), skey.clone(), NodeKind::Org, stitle.to_owned(), None, srank));
            let Some((id, name)) = division else { return split_node };
            let dkey = format!("div:{}:{id}", project.unwrap_or(""));
            *orgs.entry(dkey.clone()).or_insert_with(|| m.push(Some(split_node), dkey, NodeKind::Org, name.to_owned(), None, 0))
        };
        let tasks: TaskRows = self.tasks.iter().filter_map(|t| Some((t.task_id.as_deref()?, t))).collect();
        let known_projects: HashSet<&str> = self.tasks.iter().filter_map(|t| t.project.as_deref()).collect();
        // A workspace with no task row to ask: its repository's name, when
        // that names a known project.
        let ws_project = |ws: &Workspace| -> Option<String> {
            let path = ws.main_repo_path.as_ref().or(ws.worktree_path.as_ref())?;
            let name = path.file_name()?.to_str()?;
            known_projects.contains(name).then(|| name.to_owned())
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
            let cs = ChartSession { uid: ts.uid.clone(), wi: r.ws_idx, si, lead: false, mark: None };
            let target = match (r.depth, orch) {
                (0, _) | (_, None) => {
                    let task = self.continuous_planning_task(ws, ts).map(str::to_owned);
                    let project = task.as_deref().and_then(|t| chart_task_project(&tasks, t, &parents)).or_else(|| ws_project(ws));
                    let cont = org(&mut m, project.as_deref(), Split::Continuous, None);
                    let key = format!("orch:{}:{}", ws.host_id, ts.continuous_task_id.as_deref().unwrap_or(&ts.uid));
                    let title = task.as_deref().map_or_else(|| ts.label.clone(), |t| chart_task_title(&tasks, t));
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
                    let title = task.as_deref().map_or_else(|| ts.label.clone(), |t| chart_task_title(&tasks, t));
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
                let cs = ChartSession { uid: ts.uid.clone(), wi, si, lead: false, mark: None };
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
                    let project = chart_task_project(&tasks, &task, &parents);
                    let division = self.planning.task_division(&root);
                    org(&mut m, project.as_deref(), Split::Teams, division.as_ref().map(|d| (d.id.as_str(), d.name.as_str())))
                }
            };
            for t in chain.iter().rev() {
                let rank = if self.planning.task_division(t).is_some_and(|d| d.coordinator) { 0 } else { 1 };
                parent = m.push(Some(parent), format!("task:{t}"), NodeKind::Task, chart_task_title(&tasks, t), Some(t.clone()), rank);
                task_node.insert(t.clone(), parent);
            }
            let i = task_node.get(&task).copied().unwrap_or(parent);
            m.nodes[i].sessions.extend(sessions);
        }
        for (wi, sessions) in loose {
            let ws = &self.workspaces[wi];
            let s = org(&mut m, ws_project(ws).as_deref(), Split::Teams, None);
            let i = m.push(Some(s), format!("ws:{}", ws.id), NodeKind::Workspace, ws.name.clone(), None, 2);
            m.nodes[i].sessions = sessions;
        }

        // Leads (the shared rule; an orchestrator without one falls back to
        // its first session), leader marks exactly as the sidebar draws
        // them, then sessions lead-first in spawn order.
        let org = self.org_index();
        let marks = self.leader_marks(&org);
        for node in &mut m.nodes {
            node.leader = match node.kind {
                NodeKind::Task | NodeKind::Worker => node.task_id.as_deref().is_some_and(|t| org.has_reports(t)),
                NodeKind::Orchestrator => true,
                _ => false,
            };
            let task_lead = node.task_id.as_ref().and_then(|t| leads.get(t)).cloned();
            let lead = match node.kind {
                NodeKind::Orchestrator => task_lead.or_else(|| node.sessions.first().map(|s| s.uid.clone())),
                _ => task_lead,
            };
            for s in &mut node.sessions {
                s.lead = lead.as_deref() == Some(s.uid.as_str()) && !self.workspaces[s.wi].sessions[s.si].session.exited;
                s.mark = marks.get(&s.uid).copied();
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

    // ---- open / close / focus ---------------------------------------------

    fn open_orgchart(&mut self) {
        let c = &mut self.orgchart;
        c.visible = true;
        c.pending = None;
        c.search = None;
        c.status.clear();
        c.follow.set(true);
        // The chart always opens fully unfolded.
        c.folds.clear();
        c.boards.listed_at = None;
        self.orgchart.opened_from = Some((self.cursor.clone(), self.cursor_column));
        let uid = self.cursor_session_uid();
        if let Some(uid) = &uid {
            let bash = self.find_session_by_uid(uid).map(|(wi, si)| self.workspaces[wi].sessions[si].session_type == "bash");
            if bash == Some(true) {
                self.orgchart.bash_uid = Some(uid.clone());
            }
        }
        // Start on the sidebar's focus: its session, else the box for its
        // task or workspace header, else Owner. Never the last visit's focus.
        let model = self.org_model(self.orgchart.show_bash);
        let start = match (&uid, &self.cursor) {
            (Some(uid), _) if model.find_session(uid).is_some() => model.find_session(uid).map(|(i, _)| (i, Some(uid.clone()))),
            (_, Cursor::Task { task_id, .. }) => model.find(&format!("task:{task_id}")).map(|i| (i, None)),
            (_, Cursor::Workspace(wi)) => self.workspaces.get(*wi).and_then(|ws| {
                // A loose workspace has its own box; otherwise the box of its first charted session.
                let own = model.find(&format!("ws:{}", ws.id));
                own.or_else(|| ws.sessions.iter().find_map(|ts| model.find_session(&ts.uid).map(|(i, _)| i)))
            }).map(|i| (i, None)),
            _ => None,
        };
        let (i, session) = start.unwrap_or((0, None));
        self.orgchart_reveal(&model, i);
        self.orgchart.focus = Some(model.nodes[i].key.clone());
        // Opened on a header: no session is selected until the focus moves,
        // so an exit from here puts the sidebar cursor back on that header.
        self.orgchart.session = session;
        self.orgchart_remember_path(&model, i);
        self.orgchart_tick();
    }

    fn close_orgchart(&mut self) {
        self.orgchart.visible = false;
        self.orgchart.opened_from = None;
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
        let here = self.orgchart_focus(model);
        if let Some(s) = self.orgchart_focused_session(model) {
            self.orgchart.last_session.insert(model.nodes[here].key.clone(), s.uid);
        }
        self.orgchart_remember_path(model, i);
        let node = &model.nodes[i];
        self.orgchart.focus = Some(node.key.clone());
        let back = self.orgchart.last_session.get(&node.key).filter(|uid| node.sessions.iter().any(|s| &s.uid == *uid));
        self.orgchart.session = back.cloned().or_else(|| node.sessions.first().map(|s| s.uid.clone()));
        self.orgchart.follow.set(true);
    }

    /// Record the way down from Owner to `i` for `j`.
    fn orgchart_remember_path(&mut self, model: &ChartModel, i: usize) {
        let mut c = i;
        while let Some(p) = model.nodes[c].parent {
            self.orgchart.last_child.insert(model.nodes[p].key.clone(), model.nodes[c].key.clone());
            c = p;
        }
    }

    /// The selected session's index in `node` (its first when the uid is
    /// gone or belongs to another box).
    fn orgchart_selected(&self, node: &ChartNode) -> usize {
        self.orgchart.session.as_ref().and_then(|uid| node.sessions.iter().position(|s| &s.uid == uid)).unwrap_or(0)
    }

    /// The focused session, if the focused box has any.
    pub(super) fn orgchart_focused_session(&self, model: &ChartModel) -> Option<ChartSession> {
        let node = &model.nodes[self.orgchart_focus(model)];
        node.sessions.get(self.orgchart_selected(node)).cloned()
    }

    /// Enter (`explicit`) or any other exit: back to the Sessions view with
    /// the focused session selected. Enter on a box with no session stays
    /// in the chart; other exits with no session selected put the sidebar
    /// cursor back where it was when the chart opened.
    fn orgchart_jump(&mut self, model: &ChartModel, explicit: bool) {
        // Without a selection (opened on a header, not moved since), only
        // Enter picks the box's first session.
        let target = self.orgchart_focused_session(model).filter(|_| explicit || self.orgchart.session.is_some());
        if target.is_none() && explicit {
            self.orgchart.status = "No session in this box".into();
            return;
        }
        let opened_from = self.orgchart.opened_from.take();
        self.close_orgchart();
        // Nothing may sit over the Sessions view: other overlays close and
        // an open form or prompt is cancelled.
        self.board.visible = false;
        self.messages.visible = false;
        self.input_mode = InputMode::Normal;
        self.view_mode = ViewMode::Sessions;
        let Some(s) = target else {
            if let Some((cursor, column)) = opened_from {
                self.cursor = cursor;
                self.cursor_column = column;
            }
            self.needs_redraw = true;
            return;
        };
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
        let focus = self.orgchart_focus(model);
        let here = (order.iter().position(|&o| o == focus).unwrap_or(0), self.orgchart_selected(&model.nodes[focus]));
        let pick = if dir > 0 {
            matches.iter().find(|m| if inclusive { pos(m) >= here } else { pos(m) > here }).or(matches.first())
        } else {
            matches.iter().rev().find(|m| pos(m) < here).or(matches.last())
        };
        let &(i, k) = pick.unwrap();
        let n = matches.iter().position(|m| *m == (i, k)).unwrap_or(0) + 1;
        self.orgchart_reveal(model, i);
        self.orgchart_set_focus(model, i);
        self.orgchart.session = model.nodes[i].sessions.get(k).map(|s| s.uid.clone());
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
        let (step_x, step_y) = match self.orgchart.zoom.detail() {
            Detail::Full => (16, 6),
            Detail::Compact => (12, 4),
            Detail::Dense => (8, 3),
        };
        match (pending, key.code) {
            (Some('g'), KeyCode::Char('g')) => self.orgchart_set_focus(&model, 0),
            (Some('z'), KeyCode::Char('M')) => {
                // Fold every project, split and division (zo then opens one level).
                for n in model.nodes.iter().filter(|n| matches!(n.kind, NodeKind::Project | NodeKind::Org) && !n.children.is_empty()) {
                    self.orgchart.folds.insert(n.key.clone());
                }
                self.orgchart.follow.set(true);
            }
            (Some('z'), KeyCode::Char('R')) => {
                self.orgchart.folds.clear();
                self.orgchart.follow.set(true);
            }
            (Some('z'), KeyCode::Char(c @ ('c' | 'o' | 'a'))) | (_, KeyCode::Char(c @ ' ')) => {
                let c = if c == ' ' { 'a' } else { c };
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
            (_, KeyCode::F(6) | KeyCode::Esc | KeyCode::Char('q')) => self.orgchart_jump(&model, false),
            (_, KeyCode::Enter) => self.orgchart_jump(&model, true),
            (_, KeyCode::Char('t')) if alt => self.orgchart_jump(&model, true),
            (_, KeyCode::Char(c @ ('g' | 'z'))) => self.orgchart.pending = Some(c),
            (_, KeyCode::Char('G')) => {
                if let Some(&last) = model.preorder(&self.orgchart.folds).last() {
                    self.orgchart_set_focus(&model, last);
                }
            }
            (_, KeyCode::Char('h')) | (_, KeyCode::Char('l')) => {
                // Sideways along a box's reports as laid out: its stack is
                // one slot (`None`), then each branch. At the end of a
                // stack's row, or of a leader's row, carry on along the row
                // above.
                let next = key.code == KeyCode::Char('l');
                let folds = &self.orgchart.folds;
                let slots = |p: usize| -> Vec<Option<usize>> {
                    let mut row = Vec::new();
                    if !model.stack(p, folds).is_empty() {
                        row.push(None);
                    }
                    row.extend(model.branches(p, folds).into_iter().map(Some));
                    row
                };
                let mut here = (model.nodes[focus].parent, (!model.stacked(focus)).then_some(focus));
                let mut moved = None;
                while let (Some(p), slot) = here {
                    let row = slots(p);
                    let at = row.iter().position(|&s| s == slot);
                    let to = at.and_then(|a| if next { row.get(a + 1) } else { a.checked_sub(1).and_then(|a| row.get(a)) });
                    if let Some(&to) = to {
                        moved = Some((p, slot, to));
                        break;
                    }
                    if slot.is_some() && !model.nodes[p].leader {
                        break;
                    }
                    here = (model.nodes[p].parent, Some(p));
                }
                if let Some((p, from, to)) = moved {
                    // Leave a note where this slot was, and come back into
                    // the other one where it was left.
                    let slot_key = |s: Option<usize>| match s {
                        Some(b) => model.nodes[b].key.clone(),
                        None => format!("{}#stack", model.nodes[p].key),
                    };
                    // The remembered box, if still shown in that slot: in
                    // the stack, or anywhere under the branch.
                    let stack = model.stack(p, folds);
                    let shown_in = |c: usize| match to {
                        None => stack.contains(&c),
                        Some(b) => {
                            let mut a = c;
                            while a != b {
                                match model.nodes[a].parent {
                                    Some(up) if !folds.contains(&model.nodes[up].key) => a = up,
                                    _ => return false,
                                }
                            }
                            true
                        }
                    };
                    let back = self.orgchart.branch_focus.get(&slot_key(to)).and_then(|k| model.find(k)).filter(|&c| shown_in(c));
                    let land = back.unwrap_or(to.unwrap_or_else(|| stack[0]));
                    self.orgchart.branch_focus.insert(slot_key(from), model.nodes[focus].key.clone());
                    self.orgchart_set_focus(&model, land);
                }
            }
            (_, KeyCode::Char('j')) if model.stacked(focus) => {
                // Down the stack; its last box is the bottom of the branch.
                let stack = model.stack(model.nodes[focus].parent.unwrap_or(0), &self.orgchart.folds);
                if let Some(&below) = stack.iter().skip_while(|&&c| c != focus).nth(1) {
                    self.orgchart_set_focus(&model, below);
                }
            }
            (_, KeyCode::Char('j')) => {
                // Back to the report last focused here, else the first
                // (the top of the stack, when there is one).
                let kids = model.visual_children(focus, &self.orgchart.folds);
                let back = self.orgchart.last_child.get(&model.nodes[focus].key).and_then(|k| kids.iter().copied().find(|&c| &model.nodes[c].key == k));
                if let Some(c) = back.or(kids.first().copied()) {
                    self.orgchart_set_focus(&model, c);
                }
            }
            (_, KeyCode::Char('k')) => {
                // Up the stack, then to the leader; elsewhere to the boss.
                let parent = model.nodes[focus].parent;
                let above = parent.filter(|_| model.stacked(focus)).and_then(|p| {
                    let stack = model.stack(p, &self.orgchart.folds);
                    let at = stack.iter().position(|&c| c == focus)?;
                    at.checked_sub(1).map(|a| stack[a])
                });
                if let Some(to) = above.or(parent) {
                    self.orgchart_set_focus(&model, to);
                }
            }
            (_, KeyCode::Tab) | (_, KeyCode::BackTab) => {
                let node = &model.nodes[focus];
                let n = node.sessions.len();
                if n > 0 {
                    let cur = self.orgchart_selected(node);
                    let next = if key.code == KeyCode::Tab { (cur + 1) % n } else { (cur + n - 1) % n };
                    self.orgchart.session = Some(node.sessions[next].uid.clone());
                    self.orgchart.follow.set(true);
                }
            }
            (_, KeyCode::Char('/')) => {
                self.orgchart.search_origin = Some((self.orgchart.focus.clone(), self.orgchart.session.clone()));
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
            (_, KeyCode::Char('i')) => {
                self.orgchart.hide_info = !self.orgchart.hide_info;
                self.orgchart.follow.set(true);
            }
            (_, KeyCode::Char('c')) => self.orgchart_centre(&model),
            (_, KeyCode::Char('?')) => {
                self.orgchart.legend = !self.orgchart.legend;
                self.orgchart.follow.set(true);
            }
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
        let legend_rows = if self.orgchart.legend && inner.height > 5 { 2 } else { 0 };
        let body = Rect::new(inner.x, inner.y, inner.width, inner.height - 1 - legend_rows);
        if legend_rows == 2 {
            let legend_area = Rect::new(inner.x, inner.y + inner.height - 3, inner.width, 2);
            frame.render_widget(Paragraph::new(vec![orgchart_legend_line(), orgchart_activity_legend_line()]), legend_area);
        }
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
        self.orgchart_follow(&model, &lay, focus, chart_area);
        self.draw_chart_body(frame.buffer_mut(), chart_area, &model, &lay, focus);
        if let Some(info) = info_area {
            self.draw_chart_info(frame, info, &model, focus);
        }
        frame.render_widget(Paragraph::new(self.orgchart_status_line(status_area.width as usize)), status_area);
    }

    /// Scroll so the focused box is in view. After a pan or `c` the
    /// viewport stays put, but moves with the box when a refresh shifts
    /// the layout, and follows again if the focus itself changes.
    fn orgchart_follow(&self, model: &ChartModel, lay: &ChartLayout, focus: usize, area: Rect) {
        let c = &self.orgchart;
        let (vw, vh) = (area.width as i32, area.height as i32);
        if let Some(p) = lay.boxes.get(&focus) {
            let key = &model.nodes[focus].key;
            let mut anchor = c.anchor.borrow_mut();
            match anchor.as_ref() {
                Some((k, x, y)) if k == key => {
                    if !c.follow.get() {
                        c.ox.set(c.ox.get() + p.x - x);
                        c.oy.set(c.oy.get() + p.y - y);
                    }
                }
                // A different box is focused (e.g. the old one vanished).
                Some(_) => c.follow.set(true),
                None => {}
            }
            *anchor = Some((key.clone(), p.x, p.y));
        }
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

    /// Whether Owner leads this box, and if so whether only as stand-in: a
    /// division box by its root task, any other box by its own task.
    pub(super) fn chart_owner_lead(&self, node: &ChartNode, owner_led: &HashMap<String, bool>) -> Option<bool> {
        // Division boxes are keyed `div:<project>:<division id>`.
        let task = match node.key.strip_prefix("div:").and_then(|rest| rest.rsplit(':').next()) {
            Some(div) => self.planning.division_root(div)?,
            None => node.task_id.clone()?,
        };
        owner_led.get(&task).copied()
    }

    fn draw_chart_body(&self, buf: &mut ratatui::buffer::Buffer, area: Rect, model: &ChartModel, lay: &ChartLayout, focus: usize) {
        let owner_led = self.owner_leads();
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
            let selected = focused.then(|| self.orgchart_selected(node));
            let owner = self.chart_owner_lead(node, &owner_led);
            let folded = self.orgchart.folds.contains(&node.key) && !node.children.is_empty();
            let hidden = if folded { model.subtree_sessions(i) - node.sessions.len() } else { 0 };
            let header = matches!(node.kind, NodeKind::Owner | NodeKind::Project | NodeKind::Org);
            let head_glyph = match node.sessions.first().and_then(|s| self.chart_ts(s)) {
                Some(ts) => self.chart_glyph(ts, now),
                None if node.key.starts_with("div:") => (symbols::DIVISION, Style::default().fg(theme::HEADER)),
                None if header => (symbols::HEADER_BOX, Style::default().fg(theme::HEADER)),
                // A task box whose sessions are all elsewhere in its subtree.
                None => ("\u{2218}", Style::default().fg(theme::DIM)),
            };
            let focus_bg = |s: Style| if focused { s.bg(theme::SELECT_BG) } else { s };
            match self.orgchart.zoom.detail() {
                Detail::Dense => {
                    put(buf, p.x, p.y, head_glyph.0, focus_bg(head_glyph.1));
                    if folded {
                        put(buf, p.x + 1, p.y, "\u{25b8}", focus_bg(Style::default().fg(theme::MUTED)));
                    } else if focused {
                        put(buf, p.x + 1, p.y, " ", focus_bg(Style::default()));
                    }
                }
                Detail::Compact => {
                    let mark = node.sessions.first().and_then(|s| s.mark);
                    let (mark, mark_style) = match (mark, owner) {
                        (Some(m), _) => (m, Style::default().fg(theme::ATTN)),
                        (None, Some(_)) => (symbols::OWNER, symbols::owner_style()),
                        (None, None) => (" ", Style::default()),
                    };
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
                    put(buf, p.x, p.y, mark, focus_bg(mark_style));
                    put(buf, p.x + 1, p.y, head_glyph.0, focus_bg(head_glyph.1));
                    put(buf, p.x + 3, p.y, &name, focus_bg(text_style));
                    put(buf, p.x + p.w - tail.chars().count() as i32, p.y, &tail, focus_bg(Style::default().fg(theme::MUTED)));
                }
                Detail::Full => {
                    let border = if focused {
                        Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD)
                    } else if header {
                        Style::default().fg(theme::HEADER)
                    } else {
                        Style::default().fg(theme::MUTED)
                    };
                    let w = p.w as usize;
                    let fold_tag = if folded { format!(" \u{25b8} {hidden} below") } else { String::new() };
                    // An Owner-led box opens its title with ♔.
                    let crown = if owner.is_some() { 2 } else { 0 };
                    let title = crate::planning::truncate_with_ellipsis(&node.title, w.saturating_sub(5 + crown + fold_tag.chars().count()));
                    put(buf, p.x, p.y, &format!("\u{256d}{}\u{256e}", "\u{2500}".repeat(w - 2)), border);
                    let title_style = if header { Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD) };
                    if owner.is_some() {
                        put(buf, p.x + 2, p.y, " ", title_style);
                        put(buf, p.x + 3, p.y, symbols::OWNER, symbols::owner_style());
                    }
                    put(buf, p.x + 2 + crown as i32, p.y, &format!(" {title} "), title_style);
                    put(buf, p.x + 3 + crown as i32 + title.chars().count() as i32, p.y, &fold_tag, Style::default().fg(theme::MUTED));
                    for row in 1..p.h - 1 {
                        put(buf, p.x, p.y + row, "\u{2502}", border);
                        put(buf, p.x + 1, p.y + row, &" ".repeat(w - 2), Style::default());
                        put(buf, p.x + p.w - 1, p.y + row, "\u{2502}", border);
                    }
                    put(buf, p.x, p.y + p.h - 1, &format!("\u{2570}{}\u{256f}", "\u{2500}".repeat(w - 2)), border);
                    if node.sessions.is_empty() {
                        let text = match node.kind {
                            NodeKind::Owner => format!("{} \u{00b7} {}", plural(node.children.len(), "project"), plural(model.subtree_sessions(i), "session")),
                            NodeKind::Project => format!("{} \u{00b7} {}", plural(model.subtree_sessions(i), "session"), node.children.iter().map(|&c| model.nodes[c].title.as_str()).collect::<Vec<_>>().join(" + ")),
                            NodeKind::Org => format!("{} \u{00b7} {}", plural(node.children.len(), "report"), plural(model.subtree_sessions(i), "session")),
                            _ => "no session here".into(),
                        };
                        // An Owner-led box says so at the top: "Owner" or
                        // "Owner (stand-in)", then its usual summary.
                        let text = match owner {
                            Some(stand_in) => {
                                let who = symbols::owner_label(stand_in);
                                put(buf, p.x + 2, p.y + 1, &crate::planning::truncate_with_ellipsis(who, w - 4), focus_bg(symbols::owner_style()));
                                let rest = if matches!(node.kind, NodeKind::Task) { String::new() } else { text };
                                (!rest.is_empty()).then(|| format!(" \u{00b7} {rest}")).map(|r| (who.chars().count(), r))
                            }
                            None => Some((0, text)),
                        };
                        if let Some((skip, text)) = text {
                            let room = (w - 4).saturating_sub(skip);
                            put(buf, p.x + 2 + skip as i32, p.y + 1, &crate::planning::truncate_with_ellipsis(&text, room), focus_bg(Style::default().fg(theme::DIM)));
                        }
                        if focused {
                            put(buf, p.x + 1, p.y + 1, " ", focus_bg(Style::default()));
                        }
                        continue;
                    }
                    let n = node.sessions.len();
                    let max = self.orgchart.zoom.max_sessions();
                    let shown = if n > max { max - 1 } else { n };
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
                        let (host, hs) = symbols::host(&ts.host_id);
                        let (engine, es) = symbols::engine(&ts.session_type);
                        let held = self.orgchart.boards.held_by(&ts.uid);
                        let mark = symbols::held_mark(held.iter().map(|h| (h.item["status"].as_str().unwrap_or(""), h.item["flags"].as_array().is_some_and(|f| !f.is_empty()))));
                        // Right edge: held-item mark, engine, host, each one
                        // cell with a blank cell between, so a terminal that
                        // draws a mark two cells wide never covers the next.
                        // The live activity badge (agent_state.rs) sits just
                        // left of those when the whole label fits beside it.
                        let room = w.saturating_sub(4 + 7);
                        let label_w = ts.label.chars().count();
                        let badge = self.row_badge(ts).filter(|b| super::agent_state::badge_fits(room, label_w, b));
                        let label = crate::planning::truncate_with_ellipsis(&ts.label, room);
                        let label_style = if ts.session.exited { Style::default().fg(theme::DIM) } else { Style::default().fg(theme::TEXT) };
                        put(buf, p.x + 1, y, &" ".repeat(w - 2), bg(Style::default()));
                        put(buf, p.x + 1, y, s.mark.unwrap_or(" "), bg(Style::default().fg(theme::ATTN)));
                        put(buf, p.x + 2, y, g, bg(gs));
                        put(buf, p.x + 4, y, &label, bg(label_style));
                        if let Some(b) = badge {
                            let mut x = p.x + 4 + room as i32 - super::agent_state::badge_width(&b) as i32;
                            for span in super::agent_state::badge_spans(&b) {
                                put(buf, x, y, &span.content, bg(span.style));
                                x += span.width() as i32;
                            }
                        }
                        if let Some((m, ms)) = mark {
                            put(buf, p.x + p.w - 6, y, m, bg(ms));
                        }
                        put(buf, p.x + p.w - 4, y, engine, bg(es));
                        put(buf, p.x + p.w - 2, y, &host, bg(hs));
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
        let picked = self.orgchart_selected(node);
        let Some(s) = node.sessions.get(picked).cloned() else {
            lines.push(Line::styled(node.title.clone(), Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD)));
            let kind = match node.kind {
                NodeKind::Owner => "owner",
                NodeKind::Project => "project",
                NodeKind::Org if node.key.starts_with("div:") => "division",
                NodeKind::Org if node.key.starts_with("continuous:") => "continuous work",
                NodeKind::Org => "teams (normal work)",
                _ => "task (no session)",
            };
            lines.push(Line::from(vec![label("kind"), text(kind.into())]));
            if let Some(stand_in) = self.chart_owner_lead(node, &self.owner_leads()) {
                lines.push(Line::from(vec![label("lead"), Span::styled(format!("{} {}", symbols::OWNER, symbols::owner_label(stand_in)), symbols::owner_style())]));
            }
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
            // The leader mark only where the box has reports.
            let lead = s.mark.map_or_else(|| "  lead".to_string(), |g| format!("  {g} lead"));
            head.push(Span::styled(lead, Style::default().fg(theme::ATTN)));
        }
        lines.push(Line::from(head));
        let (word, since) = self.chart_status(ts);
        let status = match since {
            Some(secs) => format!("{word} {}", board::age(secs)),
            None => word,
        };
        let (mark, mark_style) = symbols::engine(&ts.session_type);
        lines.push(Line::from(vec![label("engine"), Span::styled(format!("{mark} "), mark_style), text(symbols::engine_name(&ts.session_type).to_owned())]));
        if let Some(model) = ts.model.as_deref() {
            lines.push(Line::from(vec![
                label("model"),
                text(super::model_field::with_source(model, ts.model_source.as_deref())),
            ]));
        }
        if let Some(effort) = ts.effort.as_deref() {
            lines.push(Line::from(vec![
                label("effort"),
                text(super::model_field::with_source(effort, ts.effort_source.as_deref())),
            ]));
        }
        lines.push(Line::from(vec![label("status"), text(status)]));
        // The activity badge in full, F9 toggle or not.
        if let Some(b) = self.session_badge(ts) {
            let (mark, mark_style) = symbols::doing(b.kind);
            lines.push(Line::from(vec![
                label("doing"),
                Span::styled(format!("{mark} "), mark_style),
                Span::styled(
                    crate::planning::truncate_with_ellipsis(&b.full, width.saturating_sub(11)),
                    symbols::doing_text(b.kind),
                ),
            ]));
        }
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
        if let Some(stand_in) = self.chart_owner_lead(node, &self.owner_leads()) {
            lines.push(Line::from(vec![label("lead"), Span::styled(format!("{} {}", symbols::OWNER, symbols::owner_label(stand_in)), symbols::owner_style())]));
        }
        if let Some(b) = boss {
            lines.push(Line::from(vec![label("boss"), text(boss_text(b))]));
        }
        lines.push(Line::from(vec![label("reports"), text(format!("{reports} direct"))]));
        let (mark, mark_style) = symbols::host(&ts.host_id);
        let host = if ts.host_id == cm_daemon::host_id::HostId::local() { "local".to_owned() } else { ts.host_id.to_string() };
        lines.push(Line::from(vec![label("host"), Span::styled(format!("{mark} "), mark_style), text(host)]));
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
            let (mark, mark_style) = symbols::board_status(item["status"].as_str().unwrap_or(""));
            let mut spans = vec![
                Span::styled(format!("{mark} "), mark_style),
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
                spans.push(Span::styled(format!("{} {} ", symbols::FLAG, flags.join(",")), symbols::flag_style()));
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
        let help = "h/l sibling  j child  k boss  gg/G  Tab session  / n N  Space fold  zM/zR orgs  +/- zoom  HJKL pan  c centre  b bash  i info  ? legend  Enter go  F6 close";
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
        // No task has a project, so everything sits under Other → Teams.
        assert_eq!(titles, ["Owner", "Other", "Teams", "Root lead", "Middle", "Leaf work", "w-loose"]);
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
        let div = m.find("div::div-1").expect("division org");
        assert_eq!(m.nodes[div].title, "Trader");
        let teams = m.find("teams:proj:").unwrap();
        assert_eq!(m.nodes[div].parent, Some(teams), "a division sits under its project's Teams");
        let kids: Vec<&str> = m.nodes[teams].children.iter().map(|&c| m.nodes[c].key.as_str()).collect();
        assert_eq!(kids, ["div::div-1", "ws:w-loose"], "divisions come first, then the other work");
        let roots: Vec<&str> = m.nodes[div].children.iter().map(|&c| m.nodes[c].title.as_str()).collect();
        assert_eq!(roots, ["Root lead", "Another root"], "the coordinator's tree leads the division");
        // The info pane names the division.
        let lines: Vec<String> = app.orgchart_info_lines(&m, div, 50).iter().map(|l| l.to_string()).collect();
        assert!(lines.iter().any(|l| l.contains("division")), "{lines:?}");
    }

    /// Owner as leader (2026-10-22): an Owner-led division box says so at
    /// the top, "Owner (stand-in)" when Owner only stands in for a root with
    /// no live lead, "Owner" when pinned; the info pane names Owner as lead.
    #[test]
    fn an_owner_led_division_box_names_owner_at_the_top() {
        let mut app = org_app();
        app.planning.update_from_api(vec![
            division_task("root", None, true),
            division_task("mid", Some("root"), false),
        ]);
        // The root's sessions are gone: nobody live leads it.
        app.workspaces[0].sessions.clear();
        assert_eq!(app.owner_leads().get("root"), Some(&true));
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.hide_info = true;
        let s = screen(&mut app, 200, 40);
        assert!(s.contains("\u{2654} Trader "), "division box title: {s}");
        assert!(s.contains("\u{2654} Root lead "), "root box title: {s}");
        assert!(s.contains("Owner (stand-in) \u{00b7} 1 r"), "{s}");
        assert!(s.contains("mid-1"), "the reports still show: {s}");
        let m = app.org_model(false);
        let div = m.find("div::div-1").unwrap();
        let info = app.orgchart_info_lines(&m, div, 60).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(info.contains("lead     \u{2654} Owner (stand-in)"), "{info}");
        // Pinned explicitly: plain "Owner".
        app.tasks[0].metadata = Some(json!({"lead_session_uid": "owner"}));
        let s = screen(&mut app, 200, 40);
        assert!(s.contains("Owner \u{00b7} 1 report") && !s.contains("stand-in"), "{s}");
    }

    #[test]
    fn projects_are_the_top_level_and_split_into_teams_and_continuous() {
        let mut app = test_app();
        let in_project = |id: &str, name: &str, parent: Option<&str>, project: &str| TaskEntry { project: Some(project.into()), ..task(id, name, parent) };
        app.tasks = vec![
            in_project("cm-root", "CM work", None, "claude-manager"),
            // A subtask without its own project takes its parent's.
            task("cm-sub", "CM subtask", Some("cm-root")),
            in_project("pt-root", "Trader work", None, "predictionTrading"),
            in_project("pt-orch", "Bug triage", None, "predictionTrading"),
            task("nowhere", "No project", None),
        ];
        app.workspaces = vec![ws("w-cm", HostId::local()), ws("w-pt", HostId::local()), ws("w-orch", HostId::new("manager")), ws("w-x", HostId::local()), ws("w-loose", HostId::local())];
        app.workspaces[4].main_repo_path = Some("/home/u/code/claude-manager".into());
        session(&mut app, 0, 0x10, "cm-1", "claude", Some("cm-root"));
        session(&mut app, 0, 0x11, "cm-2", "codex", Some("cm-sub"));
        session(&mut app, 1, 0x20, "pt-1", "codex", Some("pt-root"));
        session(&mut app, 2, 0x30, "triage-orchestrator", "codex", Some("pt-orch"));
        app.workspaces[2].sessions[0].continuous_task_id = Some("bug-triage".into());
        session(&mut app, 3, 0x40, "x-1", "codex", Some("nowhere"));
        session(&mut app, 4, 0x50, "scratch", "claude", None);
        let m = app.org_model(false);
        let title = |i: usize| m.nodes[i].title.as_str();
        let kids = |key: &str| -> Vec<&str> { m.nodes[m.find(key).unwrap()].children.iter().map(|&c| title(c)).collect() };
        assert_eq!(kids("owner"), ["claude-manager", "predictionTrading", "Other"], "projects by name, Other last");
        assert_eq!(kids("proj:claude-manager"), ["Teams"], "no Continuous without continuous work");
        assert_eq!(kids("proj:predictionTrading"), ["Teams", "Continuous"]);
        assert_eq!(kids("teams:proj:claude-manager"), ["CM work", "w-loose"], "a loose workspace joins its repo's project");
        assert_eq!(kids("task:cm-root"), ["CM subtask"]);
        assert_eq!(kids("continuous:proj:predictionTrading"), ["Bug triage"]);
        assert_eq!(kids("teams:proj:"), ["No project"]);
        assert!(m.nodes.iter().all(|n| n.key != "continuous:proj:" && n.key != "standalone"), "no global Continuous or Standalone");
        // Other only appears when something has no project.
        app.tasks.retain(|t| t.task_id.as_deref() != Some("nowhere"));
        app.workspaces[3].sessions.clear();
        assert!(app.org_model(false).find("proj:").is_none());
        // Project and split boxes fold like any other.
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.focus = Some("proj:predictionTrading".into());
        press(&mut app, " ");
        let m = app.org_model(false);
        assert!(layout(&m, app.orgchart.zoom, &app.orgchart.folds).boxes.keys().all(|&i| !m.nodes[i].key.starts_with("continuous:")));
    }

    #[test]
    fn opening_off_a_session_focuses_its_header_box_or_owner_never_the_last_visit() {
        let mut app = org_app();
        app.cursor = Cursor::Session(2, 0); // leaf-1
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "Leaf work");
        app.handle_event(&key(KeyCode::Esc));
        // A task header opens on that task's box; Esc puts the cursor back.
        let on_task = Cursor::Task { ws_idx: 1, task_id: "mid".into() };
        app.cursor = on_task.clone();
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "Middle");
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(app.cursor, on_task, "exit without a selection restores the cursor exactly");
        // ...but Enter there goes to the box's first session.
        app.handle_event(&key(KeyCode::F(6)));
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-40-0"));
        // A loose workspace header opens on its workspace box.
        app.cursor = Cursor::Workspace(3);
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "w-loose");
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(app.cursor, Cursor::Workspace(3));
        // A workspace whose sessions are in a task box opens on that box.
        app.cursor = Cursor::Workspace(0);
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "Root lead");
        app.handle_event(&key(KeyCode::Esc));
        // Anything else opens on Owner, not on the previous visit's box.
        app.orgchart.focus = Some("task:leaf".into());
        app.cursor = Cursor::Section("nowhere".into());
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "Owner");
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(app.cursor, Cursor::Section("nowhere".into()));
        assert_eq!(app.view_mode, ViewMode::Sessions);
    }

    #[test]
    fn the_selection_survives_a_lead_exit_and_the_bash_toggle() {
        let mut app = org_app();
        app.cursor = Cursor::Session(0, 1); // root-a, the lead
        app.handle_event(&key(KeyCode::F(6)));
        app.handle_event(&key(KeyCode::Tab));
        let picked = || "ts-20-0";
        let selected = |app: &App| app.orgchart_focused_session(&app.org_model(app.orgchart.show_bash)).unwrap().uid;
        assert_eq!(selected(&app), picked());
        // root-a exits: root-b becomes the lead and sorts first.
        app.workspaces[0].sessions[1].session.exited = true;
        let m = app.org_model(false);
        assert_eq!(m.nodes[m.find("task:root").unwrap()].sessions[0].uid, "ts-20-0", "re-sorted");
        assert_eq!(selected(&app), picked(), "the selection follows the uid, not the index");
        screen(&mut app, 160, 40);
        // b adds root-sh to the box; the selection stays.
        press(&mut app, "b");
        assert_eq!(app.org_model(true).nodes[app.org_model(true).find("task:root").unwrap()].sessions.len(), 3);
        assert_eq!(selected(&app), picked());
        press(&mut app, "b");
        assert_eq!(selected(&app), picked());
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(app.cursor_session_uid().as_deref(), Some(picked()));
    }

    #[test]
    fn exit_after_zm_puts_the_cursor_back() {
        let mut app = org_app();
        app.cursor = Cursor::Session(2, 0); // leaf-1
        app.view_mode = ViewMode::Planning;
        app.handle_event(&key(KeyCode::F(6)));
        press(&mut app, "zM");
        // The focus shows on the folded project, which has no session.
        assert_eq!(focused_title(&app), "Other");
        screen(&mut app, 160, 40);
        app.handle_event(&key(KeyCode::Esc));
        assert!(!app.orgchart.visible);
        assert_eq!(app.view_mode, ViewMode::Sessions);
        assert_eq!(app.cursor, Cursor::Session(2, 0));
        // The next open is unfolded and back on leaf-1.
        app.handle_event(&key(KeyCode::F(6)));
        assert!(app.orgchart.folds.is_empty());
        assert_eq!(focused_title(&app), "Leaf work");
    }

    #[test]
    fn a_continuous_orchestrator_finds_its_project_by_repo_or_lands_in_other() {
        let mut app = test_app();
        app.tasks = vec![TaskEntry { project: Some("predictionTrading".into()), ..task("pt", "Trader work", None) }, task("o1", "Repo triage", None), task("o2", "Lost triage", None)];
        app.workspaces = vec![ws("w-pt", HostId::local()), ws("w-o1", HostId::new("manager")), ws("w-o2", HostId::new("manager"))];
        app.workspaces[1].main_repo_path = Some("/home/u/.cm/repos/predictionTrading".into());
        app.workspaces[2].main_repo_path = Some("/home/u/.cm/repos/somewhere-else".into());
        session(&mut app, 0, 0x10, "pt-1", "codex", Some("pt"));
        for (wi, n, label, tid, ct) in [(1, 0x20, "repo-orchestrator", "o1", "repo-triage"), (2, 0x30, "lost-orchestrator", "o2", "lost-triage")] {
            session(&mut app, wi, n, label, "codex", Some(tid));
            app.workspaces[wi].sessions[0].continuous_task_id = Some(ct.into());
        }
        let m = app.org_model(false);
        let repo = m.find("orch:manager:repo-triage").unwrap();
        assert_eq!(m.nodes[m.nodes[repo].parent.unwrap()].key, "continuous:proj:predictionTrading", "placed by its repo's name");
        let lost = m.find("orch:manager:lost-triage").unwrap();
        assert_eq!(m.nodes[m.nodes[lost].parent.unwrap()].key, "continuous:proj:", "no project and an unknown repo: Other");
        assert_eq!(m.nodes[m.find("proj:").unwrap()].title, "Other");
        assert!(m.find("teams:proj:").is_none(), "Other has only Continuous here");
    }

    #[test]
    fn after_a_pan_a_refresh_keeps_the_focused_box_where_it_was() {
        let mut app = wide_roots_app(10);
        app.cursor = Cursor::Session(0, 5); // rt-05
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.hide_info = true;
        screen(&mut app, 100, 30);
        press(&mut app, "H");
        screen(&mut app, 100, 30);
        let on_screen = |app: &App| {
            let m = app.org_model(false);
            let p = layout(&m, app.orgchart.zoom, &app.orgchart.folds).boxes[&app.orgchart_focus(&m)];
            (p.x - app.orgchart.ox.get(), p.y - app.orgchart.oy.get())
        };
        let before = on_screen(&app);
        // Two new root tasks sort before rt-05 and push its box right.
        for (k, id) in ["r00a", "r00b"].into_iter().enumerate() {
            app.tasks.push(task(id, &format!("root 00{}", ["a", "b"][k]), None));
            session(&mut app, 0, 0x200 + k as u32, &format!("new-{k}"), "codex", Some(id));
        }
        screen(&mut app, 100, 30);
        assert!(!app.orgchart.follow.get(), "still panned");
        assert_eq!(on_screen(&app), before, "the viewport moved with the box");
        // The next focus move follows again.
        press(&mut app, "l");
        assert!(app.orgchart.follow.get());
        screen(&mut app, 100, 30);
        assert!(focus_in_view(&app));
    }

    #[test]
    fn j_goes_back_down_to_the_box_and_session_k_came_up_from() {
        let mut app = wide_roots_app(3);
        app.cursor = Cursor::Session(0, 0); // rt-00
        app.handle_event(&key(KeyCode::F(6)));
        press(&mut app, "ll");
        assert_eq!(focused_title(&app), "root 02");
        press(&mut app, "kj");
        assert_eq!(focused_title(&app), "root 02", "j returns to the last-focused report");
        // Up two levels and back down two.
        press(&mut app, "hkkjj");
        assert_eq!(focused_title(&app), "root 01");
        // A box never visited still starts at its first report.
        press(&mut app, "gg");
        app.orgchart.last_child.clear();
        press(&mut app, "jjj"); // Owner → Other → Teams → its first root task
        assert_eq!(focused_title(&app), "root 00");

        // Inside a box: k from the second session, then j, lands on it again.
        let mut app = org_app();
        app.cursor = Cursor::Session(1, 0); // mid-1
        app.handle_event(&key(KeyCode::F(6)));
        press(&mut app, "k");
        assert_eq!(focused_title(&app), "Root lead");
        assert_eq!(app.orgchart.session.as_deref(), Some("ts-10-0"), "a box opens on its lead the first time");
        app.handle_event(&key(KeyCode::Tab));
        let picked = app.orgchart_focused_session(&app.org_model(false)).unwrap().uid;
        assert_eq!(picked, "ts-20-0");
        press(&mut app, "j");
        assert_eq!(focused_title(&app), "Middle", "j from Root lead returns to Middle");
        press(&mut app, "k");
        assert_eq!(app.orgchart_focused_session(&app.org_model(false)).unwrap().uid, picked, "k returns to the session left");
        press(&mut app, "kj");
        assert_eq!(app.orgchart_focused_session(&app.org_model(false)).unwrap().uid, picked, "and so does j");
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
        let cont = m.find("continuous:proj:").unwrap();
        let other = m.find("proj:").unwrap();
        assert_eq!(m.nodes[cont].parent, Some(other), "Continuous is a split of its project");
        let orch = m.find("orch:manager:health").unwrap();
        assert_eq!(m.nodes[orch].parent, Some(cont));
        assert!(m.nodes[orch].sessions[0].lead);
        let worker = m.find("worker:manager:bug-1").unwrap();
        assert_eq!(m.nodes[worker].parent, Some(orch));
        let deep = m.find("task:bug-1-sub").unwrap();
        assert_eq!(m.nodes[deep].parent, Some(worker));
        // The project's splits: Teams before Continuous.
        let splits: Vec<&str> = m.nodes[other].children.iter().map(|&c| m.nodes[c].title.as_str()).collect();
        assert_eq!(splits, ["Teams", "Continuous"]);
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

    /// `n` root tasks side by side under Teams, which never stacks.
    fn wide_roots_app(n: usize) -> App {
        let mut app = test_app();
        app.workspaces = vec![ws("w", HostId::local())];
        for k in 0..n {
            app.tasks.push(task(&format!("r{k:02}"), &format!("root {k:02}"), None));
            session(&mut app, 0, 0x100 + k as u32, &format!("rt-{k:02}"), "codex", Some(&format!("r{k:02}")));
        }
        app
    }

    #[test]
    fn layouts_never_overlap_at_any_zoom_for_deep_and_wide_trees() {
        for app in [deep_app(12), wide_app(60), wide_roots_app(60), mixed_app(), org_app()] {
            let m = app.org_model(true);
            for zoom in all_zooms() {
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

    fn all_zooms() -> Vec<Zoom> {
        (0..ZOOM_LEVELS.len()).map(Zoom).collect()
    }

    #[test]
    fn wide_fan_out_stays_one_row_at_every_zoom() {
        let app = wide_roots_app(60);
        let m = app.org_model(false);
        let teams = m.find("teams:proj:").unwrap();
        for zoom in all_zooms() {
            let lay = layout(&m, zoom, &HashSet::new());
            let kids = &m.nodes[teams].children;
            assert_eq!(kids.len(), 60);
            let ys: HashSet<i32> = kids.iter().map(|c| lay.boxes[c].y).collect();
            assert_eq!(ys.len(), 1, "{zoom:?}: 60 root tasks share one row");
            let xs: Vec<i32> = kids.iter().map(|c| lay.boxes[c].x).collect();
            assert!(xs.windows(2).all(|w| w[0] + zoom.box_w() + zoom.hgap() <= w[1]), "{zoom:?}: left to right, apart");
            // The chart grows as wide as the row needs.
            assert!(lay.width >= 60 * zoom.box_w() + 59 * zoom.hgap(), "{zoom:?}: width {}", lay.width);
        }
        // Dense packs a deep chain into a narrow, short chart.
        let deep = deep_app(12);
        let dm = deep.org_model(false);
        let full = layout(&dm, Zoom::FULL, &HashSet::new());
        let dense = layout(&dm, Zoom::DENSE, &HashSet::new());
        assert!(dense.height < full.height / 2 && dense.width < full.width);
    }

    #[test]
    fn zoom_steps_through_every_level_one_at_a_time() {
        let widths: Vec<i32> = all_zooms().into_iter().map(Zoom::box_w).collect();
        let full: Vec<i32> = all_zooms().into_iter().filter(|z| z.detail() == Detail::Full).map(Zoom::box_w).collect();
        let compact: Vec<i32> = all_zooms().into_iter().filter(|z| z.detail() == Detail::Compact).map(Zoom::box_w).collect();
        assert!(full.windows(2).all(|w| w[0] < w[1]) && compact.windows(2).all(|w| w[0] < w[1]), "{widths:?}");
        assert!(full.len() >= 3 && compact.len() >= 3, "intermediate levels: {widths:?}");
        let mut z = Zoom::DENSE;
        for k in 1..ZOOM_LEVELS.len() {
            z = z.zoom_in();
            assert_eq!(z, Zoom(k));
        }
        assert_eq!(z.zoom_in(), Zoom::MAX, "the widest card is the ceiling");
        assert_eq!(Zoom::DENSE.zoom_out(), Zoom::DENSE, "dense is the floor");
        assert_eq!(Zoom::default(), Zoom::FULL);
    }

    #[test]
    fn reports_sit_in_a_row_with_a_bus() {
        let app = wide_roots_app(3);
        let m = app.org_model(false);
        let lay = layout(&m, Zoom::FULL, &HashSet::new());
        let boss = m.find("teams:proj:").unwrap();
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

    /// Teams: "Neighbour" (leaves "nleaf 0/1"), "Solo" (no reports),
    /// "The boss", whose reports are three leaves "leaf 0..2" and
    /// "Sub lead" with two leaves of its own, "deep 0/1", and "Zed" (no
    /// reports) right of the boss. Sessions, in cursor order: boss,
    /// lf-0..2, sub, dp-0/1, nb, nl-0/1, solo, zed.
    fn mixed_app() -> App {
        let mut app = test_app();
        app.workspaces = vec![ws("w", HostId::local())];
        let add = |app: &mut App, n: u32, id: &str, title: &str, parent: Option<&str>, label: &str| {
            app.tasks.push(task(id, title, parent));
            session(app, 0, n, label, "codex", Some(id));
        };
        add(&mut app, 0x1, "boss", "The boss", None, "boss");
        for k in 0..3 {
            add(&mut app, 0x10 + k, &format!("l{k}"), &format!("leaf {k}"), Some("boss"), &format!("lf-{k}"));
        }
        add(&mut app, 0x20, "sub", "Sub lead", Some("boss"), "sub");
        for k in 0..2 {
            add(&mut app, 0x30 + k, &format!("d{k}"), &format!("deep {k}"), Some("sub"), &format!("dp-{k}"));
        }
        add(&mut app, 0x40, "nb", "Neighbour", None, "nb");
        for k in 0..2 {
            add(&mut app, 0x50 + k, &format!("n{k}"), &format!("nleaf {k}"), Some("nb"), &format!("nl-{k}"));
        }
        add(&mut app, 0x60, "solo", "Solo", None, "solo");
        add(&mut app, 0x70, "zed", "Zed", None, "zed");
        app
    }

    fn node(m: &ChartModel, key: &str) -> usize {
        m.find(key).unwrap_or_else(|| panic!("no {key}"))
    }

    #[test]
    fn leaves_stack_only_under_leaders() {
        let app = mixed_app();
        let m = app.org_model(false);
        for (key, leader) in [("task:boss", true), ("task:sub", true), ("task:nb", true), ("task:solo", false), ("task:zed", false), ("task:l0", false), ("teams:proj:", false), ("owner", false)] {
            assert_eq!(m.nodes[node(&m, key)].leader, leader, "{key}");
        }
        for (key, stacked) in [("task:l0", true), ("task:l2", true), ("task:d1", true), ("task:n0", true), ("task:sub", false), ("task:solo", false), ("task:boss", false), ("task:nb", false)] {
            assert_eq!(m.stacked(node(&m, key)), stacked, "{key}");
        }
        // Reports of Owner, a project, Teams/Continuous, a division or
        // Other never stack, whatever they are.
        for (i, n) in m.nodes.iter().enumerate() {
            if matches!(n.kind, NodeKind::Owner | NodeKind::Project | NodeKind::Org) {
                assert!(!n.leader, "{}", n.key);
                assert!(n.children.iter().all(|&c| !m.stacked(c)), "{}", n.key);
                assert!(m.stack(i, &HashSet::new()).is_empty());
            }
        }
        // A root without reports stays in the Teams row.
        let teams = node(&m, "teams:proj:");
        for zoom in all_zooms() {
            let lay = layout(&m, zoom, &HashSet::new());
            let ys: HashSet<i32> = m.nodes[teams].children.iter().map(|c| lay.boxes[c].y).collect();
            assert_eq!(ys.len(), 1, "{zoom:?}");
        }

        // A continuous orchestrator is a leader: a worker without reports
        // stacks, one with a subtask stays a branch.
        let mut app = org_app();
        app.tasks.push(task("orch-task", "Health triage", None));
        app.tasks.push(task("bug-1", "bug one", Some("orch-task")));
        app.tasks.push(task("bug-1-sub", "bug one deeper", Some("bug-1")));
        app.tasks.push(task("bug-2", "bug two", Some("orch-task")));
        for name in ["w-orch", "w-bug", "w-deep", "w-bug2"] {
            app.workspaces.push(ws(name, HostId::new("manager")));
        }
        let o = session(&mut app, 4, 0x70, "health-orchestrator", "codex", Some("orch-task"));
        app.workspaces[4].sessions[0].continuous_task_id = Some("health".into());
        session(&mut app, 5, 0x80, "bug-1-fix", "codex", Some("bug-1"));
        app.workspaces[5].sessions[0].managed_by_uid = Some(o.clone());
        session(&mut app, 6, 0x90, "deeper", "claude", Some("bug-1-sub"));
        session(&mut app, 7, 0xa0, "bug-2-fix", "codex", Some("bug-2"));
        app.workspaces[7].sessions[0].managed_by_uid = Some(o);
        let m = app.org_model(false);
        let orch = node(&m, "orch:manager:health");
        assert!(m.nodes[orch].leader);
        assert!(m.stacked(node(&m, "worker:manager:bug-2")), "a leaf worker stacks");
        assert!(!m.stacked(node(&m, "worker:manager:bug-1")), "a worker with a subtask is a branch");
        assert!(!m.stacked(orch), "Continuous is structural");
    }

    #[test]
    fn a_leader_with_both_shows_its_stack_as_a_column_beside_its_branches() {
        let app = mixed_app();
        let m = app.org_model(false);
        let [boss, sub, l0, l1, l2, d0, d1] = ["task:boss", "task:sub", "task:l0", "task:l1", "task:l2", "task:d0", "task:d1"].map(|k| node(&m, k));
        for zoom in all_zooms() {
            let lay = layout(&m, zoom, &HashSet::new());
            assert_no_overlap(&lay);
            let b = |i: usize| lay.boxes[&i];
            // The leaves: one column, top to bottom, on the row of the
            // sub-leader's box and left of it.
            assert!([l1, l2].iter().all(|&l| b(l).x == b(l0).x), "{zoom:?}");
            assert_eq!(b(l1).y, b(l0).y + b(l0).h, "{zoom:?}");
            assert_eq!(b(l2).y, b(l1).y + b(l1).h, "{zoom:?}");
            assert_eq!(b(l0).y, b(sub).y, "{zoom:?}");
            assert!(b(l0).x + b(l0).w <= b(sub).x, "{zoom:?}");
            assert!(b(l0).y > b(boss).y + b(boss).h, "{zoom:?}");
            // The sub-leader's own leaves hang from a spine under its second cell.
            assert_eq!(b(d0).x, b(sub).x + 3, "{zoom:?}");
            assert_eq!(b(d0).y, b(sub).y + b(sub).h + 1, "{zoom:?}");
            assert_eq!(b(d1).y, b(d0).y + b(d0).h, "{zoom:?}");
            // The column's spine meets the bus.
            let bus_y = b(sub).y - 1;
            let spine_x = b(l0).x - 2;
            assert!(lay.lines.iter().any(|&(x, y, _)| x == spine_x && y == bus_y), "{zoom:?}");
            assert!(lay.lines.iter().any(|&(x, y, c)| x == spine_x && y > bus_y && c == '\u{2514}'), "{zoom:?}: the last leaf's arm");
        }
    }

    /// A continuous orchestrator with two leaf workers, alone under its
    /// project's Continuous split.
    fn lone_orchestrator_app() -> App {
        let mut app = test_app();
        app.tasks = vec![task("orch-task", "Health triage", None), task("bug-1", "bug one", Some("orch-task")), task("bug-2", "bug two", Some("orch-task"))];
        app.workspaces = vec![ws("w-orch", HostId::new("manager")), ws("w-bug", HostId::new("manager"))];
        let o = session(&mut app, 0, 0x70, "health-orchestrator", "codex", Some("orch-task"));
        app.workspaces[0].sessions[0].continuous_task_id = Some("health".into());
        for (k, t) in ["bug-1", "bug-2"].into_iter().enumerate() {
            session(&mut app, 1, 0x80 + k as u32, &format!("{t}-fix"), "codex", Some(t));
            app.workspaces[1].sessions[k].managed_by_uid = Some(o.clone());
        }
        app
    }

    #[test]
    fn a_lone_report_hangs_straight_under_its_boss() {
        for app in [wide_app(3), lone_orchestrator_app(), mixed_app(), org_app()] {
            let m = app.org_model(false);
            if app.workspaces.iter().any(|w| w.id == "w-orch") {
                assert!(m.nodes[m.find("orch:manager:health").unwrap()].leader);
            }
            for zoom in all_zooms() {
                let lay = layout(&m, zoom, &HashSet::new());
                let bw = zoom.box_w();
                for (i, n) in m.nodes.iter().enumerate() {
                    let [c] = m.branches(i, &HashSet::new())[..] else { continue };
                    if !m.stack(i, &HashSet::new()).is_empty() {
                        continue;
                    }
                    let (p, b) = (lay.boxes[&i], lay.boxes[&c]);
                    assert_eq!(p.x + bw / 2, b.x + bw / 2, "{zoom:?}: {} over {}", n.title, m.nodes[c].title);
                    let jog: Vec<_> = lay.lines.iter().filter(|l| l.1 >= p.y + p.h && l.1 < b.y && l.0 >= p.x.min(b.x) && l.0 < (p.x + p.w).max(b.x + b.w) && l.0 != p.x + bw / 2).collect();
                    assert!(jog.is_empty(), "{zoom:?}: {} → {}: {jog:?}", n.title, m.nodes[c].title);
                }
            }
        }
        // As drawn: no jog between Teams and a lone leader, or Continuous
        // and a lone orchestrator.
        for mut app in [wide_app(3), lone_orchestrator_app()] {
            app.handle_event(&key(KeyCode::F(6)));
            app.orgchart.hide_info = true;
            for zoom in all_zooms() {
                app.orgchart.zoom = zoom;
                let s = screen(&mut app, 150, 60);
                for jog in ["\u{250c}\u{2518}", "\u{2514}\u{2510}", "\u{2510}\u{2514}", "\u{2518}\u{250c}", "\u{250c}\u{2500}\u{2518}", "\u{2514}\u{2500}\u{2510}"] {
                    assert!(!s.contains(jog), "{zoom:?}:\n{s}");
                }
            }
        }
    }

    #[test]
    fn a_flat_fan_out_under_a_leader_is_one_narrow_column() {
        let app = wide_app(60);
        let m = app.org_model(false);
        let boss = node(&m, "task:boss");
        let wide = wide_roots_app(60);
        let wm = wide.org_model(false);
        for zoom in all_zooms() {
            let lay = layout(&m, zoom, &HashSet::new());
            let kids = &m.nodes[boss].children;
            assert_eq!(kids.len(), 60);
            let xs: HashSet<i32> = kids.iter().map(|c| lay.boxes[c].x).collect();
            assert_eq!(xs.len(), 1, "{zoom:?}: one column");
            assert!(kids.windows(2).all(|w| lay.boxes[&w[0]].y + lay.boxes[&w[0]].h == lay.boxes[&w[1]].y), "{zoom:?}: top to bottom");
            assert!(lay.width <= zoom.box_w() + 3, "{zoom:?}: width {} for box {}", lay.width, zoom.box_w());
            let row = layout(&wm, zoom, &HashSet::new());
            assert!(lay.width * 10 < row.width, "{zoom:?}: {} vs a row of {}", lay.width, row.width);
            assert_no_overlap(&lay);
        }
    }

    #[test]
    fn j_and_k_walk_a_stack_and_h_and_l_leave_it_sideways() {
        let mut app = mixed_app();
        app.cursor = Cursor::Session(0, 1); // lf-0
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "leaf 0");
        press(&mut app, "j");
        assert_eq!(focused_title(&app), "leaf 1");
        press(&mut app, "jj");
        assert_eq!(focused_title(&app), "leaf 2", "the last leaf is the bottom");
        press(&mut app, "kk");
        assert_eq!(focused_title(&app), "leaf 0");
        press(&mut app, "k");
        assert_eq!(focused_title(&app), "The boss", "k on the first leaf goes to the leader");
        press(&mut app, "j");
        assert_eq!(focused_title(&app), "leaf 0", "j enters the stack at the item it was left from");
        // From the column, l goes to the leader's sub-leader branch and h
        // comes back to the same leaf.
        press(&mut app, "jjl");
        assert_eq!(focused_title(&app), "Sub lead");
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "leaf 2");
        // Down the sub-leader's stack. Its row and the boss's row end
        // there, so l climbs to the boss's right neighbour, and h comes
        // back to the box it left.
        press(&mut app, "ljj");
        assert_eq!(focused_title(&app), "deep 1");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "Zed");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "Zed", "the end of the Teams row");
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "deep 1");
        // The same from the sub-leader itself.
        press(&mut app, "kk");
        assert_eq!(focused_title(&app), "Sub lead");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "Zed");
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "Sub lead");
        // h from the sub-leader goes to the column left of it.
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "leaf 2");
        // h from the column: the leader's neighbour in the Teams row.
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "Solo");
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "Neighbour");
        // A leader with only a stack: l from any of its leaves goes to the
        // leader's neighbour.
        press(&mut app, "jj");
        assert_eq!(focused_title(&app), "nleaf 1");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "Solo");
        // Tab still cycles sessions within a stacked box.
        let mut app2 = mixed_app();
        app2.workspaces[0].sessions.push({
            let mut ts = dummy_ts("ts-99-0", false, HostId::local());
            ts.label = "lf-1-b".into();
            ts.session_type = "codex".into();
            ts.task_id = Some("l1".into());
            ts
        });
        app2.cursor = Cursor::Session(0, 2); // lf-1
        app2.handle_event(&key(KeyCode::F(6)));
        app2.handle_event(&key(KeyCode::Tab));
        assert_eq!(focused_title(&app2), "leaf 1");
        assert_eq!(app2.orgchart.session.as_deref(), Some("ts-99-0"));
    }

    #[test]
    fn coming_back_to_a_branch_lands_on_the_stacked_item_left() {
        let mut app = mixed_app();
        app.cursor = Cursor::Session(0, 3); // lf-2
        app.handle_event(&key(KeyCode::F(6)));
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "Solo");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "leaf 2", "back on the leaf, not the leader");
        // Through Neighbour's stack and back.
        press(&mut app, "hhjj");
        assert_eq!(focused_title(&app), "nleaf 1");
        press(&mut app, "ll");
        assert_eq!(focused_title(&app), "leaf 2");
        press(&mut app, "hh");
        assert_eq!(focused_title(&app), "nleaf 1");
        // Up to Teams and down again: j remembers the way too.
        press(&mut app, "kkk");
        assert_eq!(focused_title(&app), "Teams");
        press(&mut app, "jj");
        assert_eq!(focused_title(&app), "nleaf 0", "k walked up through nleaf 0");
        // A branch left from its head comes back to the head.
        press(&mut app, "kl");
        assert_eq!(focused_title(&app), "Solo");
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "Neighbour");
    }

    #[test]
    fn search_folds_exit_and_follow_work_for_stacked_items() {
        let mut app = wide_app(60);
        app.handle_event(&key(KeyCode::F(6)));
        press(&mut app, "/rep-47");
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(focused_title(&app), "report 47");
        for zoom in all_zooms() {
            app.orgchart.zoom = zoom;
            app.orgchart.follow.set(true);
            screen(&mut app, 120, 30);
            assert!(focus_in_view(&app), "{zoom:?}");
        }
        app.orgchart.zoom = Zoom::FULL;
        // Walking the stack keeps the focus on screen.
        for k in 48..60 {
            press(&mut app, "j");
            assert_eq!(focused_title(&app), format!("report {k:02}"));
            screen(&mut app, 120, 30);
            assert!(focus_in_view(&app), "report {k}");
        }
        // Folding the leader hides its stack; the search unfolds it again.
        press(&mut app, "gg");
        app.orgchart.focus = Some("task:boss".into());
        press(&mut app, "zc");
        let m = app.org_model(false);
        assert_eq!(layout(&m, Zoom::FULL, &app.orgchart.folds).boxes.len(), 4, "owner, other, teams, the folded boss");
        assert!(m.stack(node(&m, "task:boss"), &app.orgchart.folds).is_empty());
        press(&mut app, "/rep-03");
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(focused_title(&app), "report 03");
        assert!(!app.orgchart.folds.contains("task:boss"));
        // The exit lands on the stacked box's session.
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-103-0"));
    }

    #[test]
    fn folding_hides_a_subtree_and_counts_it() {
        let mut app = deep_app(4);
        app.orgchart.visible = true;
        app.orgchart.focus = Some("task:t1".into());
        press(&mut app, "zc");
        let m = app.org_model(false);
        assert_eq!(layout(&m, Zoom::FULL, &app.orgchart.folds).boxes.len(), 5, "owner, other, teams, t0, folded t1");
        let s = screen(&mut app, 140, 40);
        assert!(s.contains("level 1 \u{25b8} 2 below") || s.contains("\u{25b8} 2 below"), "{s}");
        press(&mut app, "zo");
        assert_eq!(layout(&m, Zoom::FULL, &app.orgchart.folds).boxes.len(), 7);
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
        assert_eq!(focused_title(&app), "Teams");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "Teams", "no sibling to the right");
        press(&mut app, "j");
        assert_eq!(focused_title(&app), "Root lead");
        press(&mut app, "l");
        assert_eq!(focused_title(&app), "w-loose");
        press(&mut app, "h");
        assert_eq!(focused_title(&app), "Root lead");
        app.handle_event(&key(KeyCode::Tab));
        assert_eq!(app.orgchart.session.as_deref(), Some("ts-20-0"));
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
        let first = app.orgchart.session.clone();
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

    /// Whether the focused box is fully inside the chart viewport at the
    /// last draw.
    fn focus_in_view(app: &App) -> bool {
        let m = app.org_model(app.orgchart.show_bash);
        let lay = layout(&m, app.orgchart.zoom, &app.orgchart.folds);
        let p = lay.boxes[&app.orgchart_focus(&m)];
        let (vw, vh) = app.orgchart.viewport.get();
        let (ox, oy) = (app.orgchart.ox.get(), app.orgchart.oy.get());
        p.x >= ox && p.x + p.w.min(vw as i32) <= ox + vw as i32 && p.y >= oy && p.y + p.h.min(vh as i32) <= oy + vh as i32
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
        // Step down one level at a time to dense, the focus always in view.
        for k in (0..Zoom::FULL.0).rev() {
            press(&mut app, "-");
            assert_eq!(app.orgchart.zoom, Zoom(k));
            let s = screen(&mut app, 120, 30);
            assert!(s.contains(&format!("zoom {}", Zoom(k).name())), "{s}");
            assert!(focus_in_view(&app), "level {k}");
        }
        press(&mut app, "--");
        assert_eq!(app.orgchart.zoom, Zoom::DENSE, "dense is the floor");
        for k in 1..=Zoom::MAX.0 {
            press(&mut app, "+");
            screen(&mut app, 120, 30);
            assert!(focus_in_view(&app), "level {k}");
        }
        assert_eq!(app.orgchart.zoom, Zoom::MAX);
        // Panning moves the viewport and stops following.
        let before = app.orgchart.ox.get();
        press(&mut app, "H");
        assert!(!app.orgchart.follow.get());
        assert_eq!(app.orgchart.ox.get(), before - 16);
        // Narrow terminals put the info pane underneath.
        let s = screen(&mut app, 80, 40);
        assert!(s.contains(" Info "), "{s}");
    }

    #[test]
    fn the_viewport_follows_focus_along_a_wide_row_search_and_zoom() {
        let mut app = wide_roots_app(60);
        app.cursor = Cursor::Session(0, 0); // rt-00
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(focused_title(&app), "root 00");
        screen(&mut app, 120, 30);
        assert!(focus_in_view(&app));
        let start = app.orgchart.ox.get();
        for k in 1..60 {
            press(&mut app, "l");
            assert_eq!(focused_title(&app), format!("root {k:02}"));
            let s = screen(&mut app, 120, 30);
            assert!(focus_in_view(&app), "after {k} l presses");
            assert!(s.contains(&format!("rt-{k:02}")), "{s}");
        }
        assert!(app.orgchart.ox.get() > start, "the viewport scrolled right");
        for k in (0..59).rev() {
            press(&mut app, "h");
            screen(&mut app, 120, 30);
            assert!(focus_in_view(&app), "back at {k}");
        }
        // A pan, then a search jump far off screen: the viewport follows again.
        press(&mut app, "KK");
        press(&mut app, "/rt-47");
        app.handle_event(&key(KeyCode::Enter));
        assert_eq!(focused_title(&app), "root 47");
        screen(&mut app, 120, 30);
        assert!(focus_in_view(&app), "after the search jump");
        // A pan, then a zoom change.
        press(&mut app, "HHHH");
        press(&mut app, "-");
        screen(&mut app, 120, 30);
        assert!(focus_in_view(&app), "after zooming out");
        press(&mut app, "LLLL++");
        screen(&mut app, 120, 30);
        assert!(focus_in_view(&app), "after zooming in");
    }

    #[test]
    fn every_exit_returns_to_the_sessions_view_on_the_charts_focus() {
        for exit in [KeyCode::Esc, KeyCode::F(6), KeyCode::Char('q')] {
            let mut app = org_app();
            app.view_mode = ViewMode::Planning;
            app.board.visible = true;
            app.cursor = Cursor::Session(3, 0); // scratch
            app.handle_event(&key(KeyCode::F(6)));
            press(&mut app, "/leaf-1");
            app.handle_event(&key(KeyCode::Enter));
            app.handle_event(&key(exit));
            assert!(!app.orgchart.visible, "{exit:?}");
            assert!(!app.board.visible, "{exit:?}");
            assert_eq!(app.view_mode, ViewMode::Sessions, "{exit:?}");
            assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-50-0"), "{exit:?}");
        }
        // Tab picks a session inside the box; the exit lands on that one.
        let mut app = org_app();
        app.cursor = Cursor::Session(0, 0); // root-b
        app.handle_event(&key(KeyCode::F(6)));
        assert_eq!(app.orgchart_focused_session(&app.org_model(false)).unwrap().uid, "ts-20-0");
        app.handle_event(&key(KeyCode::Tab));
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-10-0"));
        // A box with no session: the exit still closes, the cursor stays.
        app.handle_event(&key(KeyCode::F(6)));
        press(&mut app, "gg");
        app.handle_event(&key(KeyCode::Esc));
        assert!(!app.orgchart.visible);
        assert_eq!(app.cursor_session_uid().as_deref(), Some("ts-10-0"));
    }

    #[test]
    fn the_chart_opens_unfolded_and_folds_top_level_orgs() {
        let mut app = org_app();
        app.tasks.push(task("solo", "Solo", None));
        session(&mut app, 3, 0x70, "solo-1", "codex", Some("solo"));
        app.handle_event(&key(KeyCode::F(6)));
        assert!(app.orgchart.folds.is_empty(), "the default is fully unfolded");
        let m = app.org_model(false);
        let all = layout(&m, app.orgchart.zoom, &app.orgchart.folds).boxes.len();
        assert_eq!(all, m.nodes.len());
        // zM folds every project and split, zR unfolds everything.
        press(&mut app, "zM");
        assert!(app.orgchart.folds.contains("proj:") && app.orgchart.folds.contains("teams:proj:"));
        assert_eq!(layout(&m, app.orgchart.zoom, &app.orgchart.folds).boxes.len(), 2, "Owner and Other");
        let s = screen(&mut app, 140, 40);
        assert!(s.contains("\u{25b8}"), "{s}");
        press(&mut app, "zR");
        assert!(app.orgchart.folds.is_empty());
        // Space toggles the focused org.
        app.orgchart.focus = Some("teams:proj:".into());
        press(&mut app, " ");
        assert!(app.orgchart.folds.contains("teams:proj:"));
        press(&mut app, " ");
        assert!(!app.orgchart.folds.contains("teams:proj:"));
        // Folds never survive a close: the next open is unfolded again.
        press(&mut app, " ");
        app.handle_event(&key(KeyCode::Esc));
        app.handle_event(&key(KeyCode::F(6)));
        assert!(app.orgchart.folds.is_empty());
    }

    #[test]
    fn cards_show_engine_and_host_markers_for_every_host() {
        let mut app = org_app();
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.hide_info = true;
        let s = screen(&mut app, 200, 40);
        let row = |label: &str| s.lines().find(|l| l.contains(label)).unwrap_or_else(|| panic!("{label}:\n{s}")).to_owned();
        assert!(row("root-a").contains("\u{2605}"), "lead star: {}", row("root-a"));
        // Engines and hosts are one-cell symbols, not words.
        assert!(row("root-a").contains("\u{273b} \u{2302}"), "claude, local host: {}", row("root-a"));
        assert!(row("mid-1").contains("\u{2b21} s"), "codex, sessions host: {}", row("mid-1"));
        assert!(row("leaf-1").contains("\u{2b21} m"), "codex, manager host: {}", row("leaf-1"));
        assert!(!row("root-a").contains("claude") && !row("mid-1").contains("codex"), "{s}");
        // A held board item puts its mark before the engine: a flag wins.
        app.orgchart.boards.items.insert(
            "sejd".into(),
            vec![
                json!({"n":1,"status":"blocked","holders":[{"session_uid":"ts-40-0"}]}),
                json!({"n":2,"status":"active","holders":[{"session_uid":"ts-50-0"}],"flags":[{"kind":"holder_idle"}]}),
            ],
        );
        let s = screen(&mut app, 200, 40);
        let row = |label: &str| s.lines().find(|l| l.contains(label)).unwrap().to_owned();
        assert!(row("mid-1").contains("\u{2298} \u{2b21} s"), "blocked item: {}", row("mid-1"));
        assert!(row("leaf-1").contains("\u{2691} \u{2b21} m"), "flagged item: {}", row("leaf-1"));
    }

    #[test]
    fn question_mark_shows_the_symbol_legend() {
        let mut app = org_app();
        app.handle_event(&key(KeyCode::F(6)));
        let s = screen(&mut app, 220, 40);
        assert!(!s.contains("host initial") && s.contains("? legend"), "{s}");
        press(&mut app, "?");
        let s = screen(&mut app, 220, 40);
        let legend = s.lines().find(|l| l.contains("host initial")).unwrap_or_else(|| panic!("{s}"));
        for part in ["\u{273b} claude", "\u{2b21} codex", "\u{2302} local", "\u{27a4} active", "\u{229b} on Owner", "\u{2691} flagged", "\u{229e} org", "\u{25c6} division", "\u{25b8} folded"] {
            assert!(legend.contains(part), "{part}: {legend}");
        }
        // The activity badge's marks, on the line below.
        let activity = s.lines().find(|l| l.contains("rate limited")).unwrap_or_else(|| panic!("{s}"));
        for part in ["\u{2699} working", "\u{29d7} background jobs", "\u{261b} waits on you", "\u{26a0} rate limited", "\u{23f1} stalled"] {
            assert!(activity.contains(part), "{part}: {activity}");
        }
        press(&mut app, "?");
        assert!(!screen(&mut app, 220, 40).contains("host initial"));
    }

    /// Owner 2026-10-22: cards carry the live activity badge (a rate limit
    /// distinctly), the info pane spells it out, and F9's toggle hides it
    /// on cards only.
    #[test]
    fn cards_and_info_pane_show_the_activity_badge() {
        let mut app = org_app();
        let now = board::now_unix() as f64;
        app.apply_agent_state(&HostId::new("sessions"), "ts-40-0", &json!({"state":"errored","since":now - 120.0,
            "detail":{"error_kind":"usageLimitExceeded","resumes_at":now + 900.0}}));
        app.apply_agent_state(&HostId::new("manager"), "ts-50-0", &json!({"state":"working-background","since":now,
            "background":{"jobs":[{"id":"1","label":"cargo test"}]}}));
        app.cursor = Cursor::Session(1, 0); // mid-1
        app.handle_event(&key(KeyCode::F(6)));
        app.orgchart.hide_info = true;
        let s = screen(&mut app, 200, 40);
        let row = |s: &str, label: &str| s.lines().find(|l| l.contains(label)).unwrap_or_else(|| panic!("{label}:\n{s}")).to_owned();
        assert!(row(&s, "mid-1").contains("\u{26a0}15m") || row(&s, "mid-1").contains("\u{26a0}14m"), "{}", row(&s, "mid-1"));
        assert!(row(&s, "leaf-1").contains("\u{29d7}1") && !s.contains("cargo test"), "{}", row(&s, "leaf-1"));
        assert!(!row(&s, "root-a").contains('\u{2699}'), "no state, no badge");
        // Engine and host marks keep their cells.
        assert!(row(&s, "mid-1").contains("\u{2b21} s"), "{}", row(&s, "mid-1"));
        let m = app.org_model(false);
        let info = app.orgchart_info_lines(&m, app.orgchart_focus(&m), 80).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(info.contains("doing    \u{26a0} rate limited (usageLimitExceeded), resumes in 1"), "{info}");
        // F9 off: cards drop it, the info pane keeps it.
        for c in [KeyCode::F(9), KeyCode::Down, KeyCode::Char(' '), KeyCode::Enter] {
            app.global_settings.handle_event(&key(c));
        }
        let s = screen(&mut app, 200, 40);
        assert!(!row(&s, "mid-1").contains('\u{26a0}'), "{}", row(&s, "mid-1"));
        let info = app.orgchart_info_lines(&m, app.orgchart_focus(&m), 80).iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(info.contains("doing"), "{info}");
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
        // mid-1 leads Middle (level 2, with reports): ☆, the sidebar's mark.
        assert!(joined.contains("mid-1") && joined.contains("\u{2606} lead"), "{joined}");
        // The info pane keeps words beside the symbols.
        assert!(joined.contains("engine   \u{2b21} codex"), "{joined}");
        assert!(joined.contains("since spawn"), "{joined}");
        assert!(joined.contains("task     Middle"), "{joined}");
        assert!(joined.contains("boss     Root lead (lead root-a)"), "{joined}");
        assert!(joined.contains("reports  1 direct"), "{joined}");
        assert!(joined.contains("host     s sessions"), "{joined}");
        assert!(joined.contains("worktree /wt/w-mid"), "{joined}");
        assert!(joined.contains("Board items (1)"), "{joined}");
        assert!(joined.contains("\u{27a4} sejd #14 ACTIVE") && joined.contains("ETA") && joined.contains("holder_idle"), "{joined}");
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
