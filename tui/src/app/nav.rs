//! Sidebar navigation: cursor/visual-items builders, idle-age + attention system, MRU quick-switch, fuzzy palette, detail peek.

use super::*;

/// An idle session stays in "afterglow" (bright warm dot — it probably just
/// finished and wants eyes) for this long after going idle.
const IDLE_AFTERGLOW_WINDOW: Duration = Duration::from_secs(2 * 60);

/// Past this idle age a session is "stale" — dim dot + dimmed label; it's
/// been waiting long enough that it's background noise, not a fresh signal.
const IDLE_STALE_THRESHOLD: Duration = Duration::from_secs(30 * 60);

/// Display bucket for an idle session's age. Ordering of the variants is
/// young → old.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdleAgeBucket {
    /// Went idle < [`IDLE_AFTERGLOW_WINDOW`] ago: warm bright highlight.
    Afterglow,
    /// Between the two thresholds: the normal white idle dot.
    Settled,
    /// Idle > [`IDLE_STALE_THRESHOLD`] — or the age is unknown (`None`,
    /// e.g. the spell started before a TUI restart): dimmed.
    Stale,
}

/// Pure bucket classifier over "how long has this session been idle".
/// `None` = unknown age → treated as old ([`IdleAgeBucket::Stale`]).
fn idle_age_bucket(idle_for: Option<Duration>) -> IdleAgeBucket {
    match idle_for {
        Some(d) if d < IDLE_AFTERGLOW_WINDOW => IdleAgeBucket::Afterglow,
        Some(d) if d < IDLE_STALE_THRESHOLD => IdleAgeBucket::Settled,
        _ => IdleAgeBucket::Stale,
    }
}

/// Bucket for a session's `idle_since` stamp as of `now`. Saturates to a
/// zero age if `idle_since` is somehow in the future.
pub(super) fn idle_age_bucket_at(idle_since: Option<Instant>, now: Instant) -> IdleAgeBucket {
    idle_age_bucket(idle_since.map(|t| now.saturating_duration_since(t)))
}

/// Pure candidate picker for the A-g "next needs attention" jump.
///
/// `rows` is every session row in sidebar visual order, projected down to
/// `(has_alert, is_idle, hidden)`. Candidates are, in priority order:
///   1. rows with a pending `notify_user` alert (alerts override hidden,
///      matching the sidebar indicator), in visual order;
///   2. idle, non-hidden rows without an alert, in visual order.
/// `current` is the cursor's position in `rows` (if it's on a session row).
/// Returns the row index to jump to: the candidate after `current` in the
/// priority-ordered ring (wrapping), or the top-priority candidate when the
/// cursor isn't on a candidate. `None` = nothing needs attention.
fn next_attention_index(
    rows: &[(bool, bool, bool)],
    current: Option<usize>,
) -> Option<usize> {
    let mut candidates: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, (has_alert, _, _))| *has_alert)
        .map(|(i, _)| i)
        .collect();
    candidates.extend(
        rows.iter()
            .enumerate()
            .filter(|(_, (has_alert, is_idle, hidden))| {
                !*has_alert && *is_idle && !*hidden
            })
            .map(|(i, _)| i),
    );
    if candidates.is_empty() {
        return None;
    }
    let cur_pos = current.and_then(|c| candidates.iter().position(|&i| i == c));
    Some(match cur_pos {
        Some(p) => candidates[(p + 1) % candidates.len()],
        None => candidates[0],
    })
}

/// Most-recently-used session history depth. Enough to alt-tab through a
/// realistic working set without the deque growing unbounded.
const SESSION_MRU_CAP: usize = 16;

/// Record `prev_uid` as the most recent focus in the MRU deque:
/// dedup (an old occurrence moves to the front), push front, cap.
fn mru_record(mru: &mut VecDeque<String>, prev_uid: &str, cap: usize) {
    mru.retain(|u| u != prev_uid);
    mru.push_front(prev_uid.to_string());
    mru.truncate(cap);
}

/// In-progress A-; walk: the MRU ring frozen at the first press plus the
/// current position in it. Slot 0 is the walk's starting focus (or a
/// sentinel that never resolves when the walk started on a non-session
/// row), so wrapping cycles back through the start like classic alt-tab.
/// Cleared by any OTHER key press (see `handle_event`) — the closest a
/// TUI can get to "the modifier was released".
#[derive(Clone, Debug)]
pub(super) struct MruWalk {
    list: Vec<String>,
    pos: usize,
}

/// Pure walk-step picker for the A-; quick-switch. Scans forward from
/// `pos + 1` (wrapping) for the first entry that `resolves` to a live,
/// jumpable session and isn't the currently-focused uid. Returns the new
/// walk position; `None` when nothing else in the ring is reachable.
fn mru_next_walk_target(
    list: &[String],
    pos: usize,
    current: Option<&str>,
    resolves: impl Fn(&str) -> bool,
) -> Option<usize> {
    if list.is_empty() {
        return None;
    }
    let n = list.len();
    let mut p = pos;
    for _ in 0..n {
        p = (p + 1) % n;
        let uid = list[p].as_str();
        if Some(uid) != current && resolves(uid) {
            return Some(p);
        }
    }
    None
}

/// Rows shown in the palette at once ("top ~15 matches").
pub(super) const PALETTE_MAX_RESULTS: usize = 15;

/// What a palette row jumps to. Stored as stable IDs (session uid /
/// workspace id), resolved against the live rows at submit time — a
/// backend reconcile can reorder `App::workspaces` while the modal is
/// open, so indices would misjump (same pattern as `SaveSnapshot`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PaletteTarget {
    Session { uid: String },
    Workspace { ws_id: String },
}

/// One searchable row in the A-p palette, snapshotted at modal open.
#[derive(Clone, Debug)]
pub(crate) struct PaletteCandidate {
    pub target: PaletteTarget,
    pub display: String,
}

/// Pure palette matcher: case-insensitive SUBSTRING filter over
/// `displays`, ranked prefix-matches first, then other substring matches,
/// each group in original (sidebar) order. Empty query = everything in
/// order. Returns indices into `displays`.
pub(super) fn palette_match_indices(query: &str, displays: &[&str]) -> Vec<usize> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return (0..displays.len()).collect();
    }
    let mut prefix: Vec<usize> = Vec::new();
    let mut substr: Vec<usize> = Vec::new();
    for (i, d) in displays.iter().enumerate() {
        let dl = d.to_lowercase();
        if dl.starts_with(&q) {
            prefix.push(i);
        } else if dl.contains(&q) {
            substr.push(i);
        }
    }
    prefix.extend(substr);
    prefix
}

/// One logical line of the A-i info overlay, in plain data so the
/// assembly functions below stay pure/unit-testable. The draw maps each
/// variant to its style (labels DIM, values TEXT, title TEXT+BOLD,
/// status colored by `TaskStatus`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PeekLine {
    /// Headline (task or workspace name): TEXT + BOLD.
    Title(String),
    /// `label: value` — label DIM, value TEXT.
    Field { label: String, value: String },
    /// Task status line — value colored by the status.
    Status { value: String, status: TaskStatus },
    /// Free text (prompt body, session list rows): TEXT.
    Text(String),
    Blank,
}

fn task_status_label(s: &TaskStatus) -> &'static str {
    match s {
        TaskStatus::Running => "running",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Backlog => "backlog",
        TaskStatus::Done => "done",
    }
}

/// Peek body for a bound task: name, status, metadata, and the full
/// prompt (the main payload — "what was this agent asked to do").
/// `parent_name` is the resolved parent task's name when the caller
/// could find it; the raw id is shown otherwise.
fn peek_task_lines(task: &TaskEntry, parent_name: Option<&str>) -> Vec<PeekLine> {
    let mut out = vec![
        PeekLine::Title(task.name.clone()),
        PeekLine::Status {
            value: task_status_label(&task.api_status).to_string(),
            status: task.api_status.clone(),
        },
    ];
    if let Some(p) = task.project.as_deref() {
        out.push(PeekLine::Field { label: "Project".into(), value: p.to_string() });
    }
    if let Some(pid) = task.parent_task_id.as_deref() {
        let shown = parent_name.unwrap_or(pid).to_string();
        out.push(PeekLine::Field { label: "Parent".into(), value: shown });
    }
    if let Some(b) = task.wip_branch.as_deref() {
        out.push(PeekLine::Field { label: "Branch".into(), value: b.to_string() });
    }
    out.push(PeekLine::Blank);
    out.push(PeekLine::Field { label: "Prompt".into(), value: String::new() });
    match task.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            for l in p.lines() {
                out.push(PeekLine::Text(l.to_string()));
            }
        }
        None => out.push(PeekLine::Text("(no prompt)".into())),
    }
    out
}

/// Peek fallback when no task is bound: the workspace's identity plus its
/// session roster. `sessions` rows are `(label, type, status)`.
fn peek_workspace_lines(
    name: &str,
    worktree_path: Option<&str>,
    main_repo_path: Option<&str>,
    repo_url: Option<&str>,
    host: &str,
    sessions: &[(String, String, String)],
) -> Vec<PeekLine> {
    let mut out = vec![PeekLine::Title(name.to_string())];
    if let Some(p) = worktree_path {
        out.push(PeekLine::Field { label: "Worktree".into(), value: p.to_string() });
    }
    if let Some(p) = main_repo_path {
        out.push(PeekLine::Field { label: "Main repo".into(), value: p.to_string() });
    }
    if let Some(u) = repo_url {
        out.push(PeekLine::Field { label: "Repo".into(), value: u.to_string() });
    }
    out.push(PeekLine::Field { label: "Host".into(), value: host.to_string() });
    out.push(PeekLine::Blank);
    out.push(PeekLine::Field { label: "Sessions".into(), value: String::new() });
    if sessions.is_empty() {
        out.push(PeekLine::Text("(none)".into()));
    } else {
        for (label, stype, status) in sessions {
            out.push(PeekLine::Text(format!("{} ({}) \u{2014} {}", label, stype, status)));
        }
    }
    out
}

/// Peek header block for a Session cursor: identity + provenance + the
/// last delivered prompt snippet when one is recorded.
fn peek_session_lines(
    label: &str,
    session_type: &str,
    uid: &str,
    seeded_from: Option<&str>,
    last_delivery: Option<&str>,
) -> Vec<PeekLine> {
    let mut out = vec![
        PeekLine::Field {
            label: "Session".into(),
            value: format!("{} ({})", label, session_type),
        },
        PeekLine::Field { label: "Uid".into(), value: uid.to_string() },
    ];
    if let Some(s) = seeded_from {
        out.push(PeekLine::Field { label: "Seeded from".into(), value: s.to_string() });
    }
    if let Some(d) = last_delivery {
        out.push(PeekLine::Field { label: "Last prompt".into(), value: d.to_string() });
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub enum Cursor {
    /// Cursor is on a workspace header (by workspace index).
    Workspace(usize),
    /// Cursor is on a task subheader within a workspace. Identified by
    /// workspace index plus task_id (tasks can move / be renumbered, so an
    /// index wouldn't be stable).
    Task { ws_idx: usize, task_id: String },
    /// Cursor is on a session within a workspace (workspace index, session index).
    Session(usize, usize),
    /// Cursor is inside the sidebar's `backtests` group (cloud backtest
    /// runs — no workspace, no sessions; see `app/backtests.rs`).
    /// Identified structurally (stem / task_id), not by index, so it
    /// survives rows entering/leaving the group between refreshes.
    Backtest(BacktestCursor),
}

/// Which row of the `backtests` group the cursor is on. Space/Enter (which
/// have no PTY to go to here) toggle the fold of the group / a fleet.
#[derive(Clone, Debug, PartialEq)]
pub enum BacktestCursor {
    /// The `backtests` group header.
    Group,
    /// A fleet row (runs sharing this label stem).
    Fleet(String),
    /// A single run row, by task id.
    Run(String),
}

/// Which sidebar column the cursor is navigating (two-column continuous panel,
/// S4 of DESIGN_CONTINUOUS_PANEL.md). `Main` is the always-present sessions
/// sidebar; `Continuous` is the dedicated column (only reachable when
/// `continuous_column_on`). `A-h`/`A-l` move between them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarColumn {
    Main,
    Continuous,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SidebarView {
    Status,
    Task,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ViewMode {
    Sessions,
    Planning,
}

#[derive(Clone, Debug)]
pub(super) enum VisualItem {
    WorkspaceHeader(usize),
    /// Subheader for a task inside a workspace. Sessions tagged with this
    /// task_id follow immediately after.
    TaskHeader { ws_idx: usize, task_id: String },
    Session(usize, usize),
    Separator,
    /// Header row for a workflow grouping, followed by its participant Sessions.
    WorkflowHeader { ws_idx: usize, run_id: String },
    /// 12e: header row for a host group. Emitted only when
    /// `HostsConfig.hosts.len() > 1`. Sessions tagged with this
    /// host's `host_id` follow until the next `HostHeader` or
    /// the end of the list.
    HostHeader(cm_daemon::host_id::HostId),
    /// Continuous Tasks: header row for the continuous-session
    /// group, sorted to the bottom of the sidebar. Emitted only
    /// when at least one session carries a `continuous_task_id`.
    /// Sessions tagged continuous follow until the end of the
    /// group. Non-selectable, like `HostHeader`. See
    /// DESIGN_CONTINUOUS_TASKS.md §12.
    ContinuousHeader,
    /// Cloud-backtests group header, appended at the bottom of BOTH
    /// sidebar sub-views when any backtest row is live. SELECTABLE
    /// (unlike `ContinuousHeader`) so Space/Enter can fold the group.
    BacktestHeader,
    /// A fleet row inside the backtests group: ≥2 runs sharing a label
    /// stem, rendered as one line (collapsed by default). Selectable.
    BacktestFleet(String),
    /// One backtest run. `depth` is 1 for fleet members (indented under
    /// their fleet row), 0 for singletons. Selectable (A-i peeks it).
    BacktestRun { task_id: String, depth: u8 },
}

/// One row in the dedicated continuous column (the two-column panel, S2 of
/// DESIGN_CONTINUOUS_PANEL.md). A continuous-task orchestrator (`depth == 0`)
/// followed by the subtasks it spawned (`depth == 1`, matched by
/// `managed_by_uid`). Carries `(ws_idx, sess_idx)` exactly like
/// `VisualItem::Session`, so the cursor + `active_session()` resolve a row
/// identically; `depth` only drives render indentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContinuousRow {
    pub ws_idx: usize,
    pub sess_idx: usize,
    pub depth: u8,
}

/// Uid of the session the cursor currently selects, if it's on a session row
/// that resolves to a live session. This is the clear-on-focus target for
/// `notify_user` alerts (`reap_and_clear_alerts`). Pure so it can be
/// unit-tested without standing up a full `App`.
pub(super) fn cursor_selected_session_uid<'a>(
    cursor: &Cursor,
    workspaces: &'a [Workspace],
) -> Option<&'a str> {
    match cursor {
        Cursor::Session(wi, si) => workspaces
            .get(*wi)
            .and_then(|ws| ws.sessions.get(*si))
            .map(|ts| ts.uid.as_str()),
        _ => None,
    }
}

impl App {
    /// True iff the session with this uid has a pending `notify_user` alert.
    pub(crate) fn session_has_alert(&self, uid: &str) -> bool {
        self.alerts.contains_key(uid)
    }

    /// Record an attention alert for `uid` and fire the desktop notification.
    /// Called by the `notify_user` control-socket handler. Idempotent on the
    /// uid — a second alert just overwrites the message and re-notifies.
    pub(crate) fn raise_alert(&mut self, uid: &str, label: &str, message: &str) {
        self.alerts.insert(uid.to_string(), message.to_string());
        notify_user_alert(label, message);
        self.needs_redraw = true;
    }

    /// Drive the blink: when any alert is pending, force a redraw on each phase
    /// flip so the indicator keeps pulsing even while the alerting session is
    /// idle. Cheap no-op when `alerts` is empty (the common case). Called once
    /// per main-loop iteration.
    pub fn tick_alerts(&mut self) {
        if self.alerts.is_empty() {
            return;
        }
        let frame = self.alert_frame();
        if frame != self.last_alert_frame {
            self.last_alert_frame = frame;
            self.needs_redraw = true;
        }
    }

    /// Repaint when an idle session's AGE BUCKET changes. Idle sessions
    /// emit no PTY events, so without this the afterglow→settled→stale
    /// indicator transitions (and the status-bar rollup color) would only
    /// render on the next unrelated redraw. Throttled to ~1 Hz and only
    /// flips `needs_redraw` when the fingerprint of `(uid, bucket)` pairs
    /// actually changes — bucket boundaries are minutes apart, so this
    /// never becomes a busy-repaint. Called once per main-loop iteration.
    pub fn tick_idle_ages(&mut self) {
        let now = Instant::now();
        if self
            .last_idle_bucket_check
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(1))
        {
            return;
        }
        self.last_idle_bucket_check = Some(now);
        // FNV-1a over each idle session's uid + bucket discriminant. Any
        // session entering/leaving Idle also perturbs the fingerprint, but
        // those paths already set `needs_redraw` themselves — this only
        // needs to catch pure age progression.
        const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
        let mut fp = FNV_OFFSET;
        let mut mix = |byte: u64| fp = (fp ^ byte).wrapping_mul(FNV_PRIME);
        for ws in self.workspaces.iter().filter(|w| !w.is_closed) {
            for ts in &ws.sessions {
                if ts.status != SessionStatus::Idle || ts.session.exited {
                    continue;
                }
                for b in ts.uid.as_bytes() {
                    mix(*b as u64);
                }
                mix(idle_age_bucket_at(ts.idle_since, now) as u64 + 1);
            }
        }
        drop(mix);
        if fp != self.idle_bucket_fingerprint {
            self.idle_bucket_fingerprint = fp;
            self.needs_redraw = true;
        }
    }

    /// Clear an alert once the user selects its session, and reap alerts whose
    /// session no longer exists (so a dead uid can't keep the blink — and thus
    /// the forced redraws — alive forever). Called once per main-loop iteration;
    /// gated on a non-empty `alerts` map so it's free in the common case.
    pub fn reap_and_clear_alerts(&mut self) {
        if self.alerts.is_empty() {
            return;
        }
        // Clear-on-focus: the chosen semantics are "selecting the session's
        // sidebar row counts as focusing it" (attach selects first, so it's
        // covered too). Resolve the uid first and drop the workspaces borrow
        // before mutating `alerts`.
        let selected = cursor_selected_session_uid(&self.cursor, &self.workspaces)
            .map(str::to_string);
        if let Some(uid) = selected {
            if self.alerts.remove(&uid).is_some() {
                self.needs_redraw = true;
            }
        }
        // Reap alerts for sessions that have since gone away.
        if !self.alerts.is_empty() {
            let live: HashSet<&str> = self
                .workspaces
                .iter()
                .flat_map(|ws| ws.sessions.iter().map(|ts| ts.uid.as_str()))
                .collect();
            let before = self.alerts.len();
            self.alerts.retain(|uid, _| live.contains(uid.as_str()));
            if self.alerts.len() != before {
                self.needs_redraw = true;
            }
        }
    }

    /// Stable re-sort that floats pinned workspaces to the top
    /// immediately after a pin toggle (the full status-ranked sort
    /// re-runs on the next `reconcile_tasks`, which applies the same
    /// pinned-first key). Stability preserves the existing status order
    /// within both the pinned and unpinned groups. The cursor is
    /// re-resolved by workspace id / session uid so it stays on the
    /// same row across the reorder.
    pub(super) fn resort_workspaces_for_pin(&mut self) {
        let (saved_ws_id, saved_uid, saved_task) = match &self.cursor {
            Cursor::Session(wi, si) => (
                self.workspaces.get(*wi).map(|w| w.id.clone()),
                self.workspaces
                    .get(*wi)
                    .and_then(|w| w.sessions.get(*si))
                    .map(|s| s.uid.clone()),
                None,
            ),
            Cursor::Workspace(wi) => {
                (self.workspaces.get(*wi).map(|w| w.id.clone()), None, None)
            }
            Cursor::Task { ws_idx, task_id } => (
                self.workspaces.get(*ws_idx).map(|w| w.id.clone()),
                None,
                Some(task_id.clone()),
            ),
            // Backtest rows live outside the workspace list — the reorder
            // can't move them, so the cursor needs no re-resolution.
            Cursor::Backtest(_) => (None, None, None),
        };
        self.workspaces.sort_by_key(|w| !w.pinned);
        if let Some(id) = saved_ws_id {
            if let Some(wi) = self.workspaces.iter().position(|w| w.id == id) {
                self.cursor = if let Some(uid) = saved_uid {
                    match self.workspaces[wi]
                        .sessions
                        .iter()
                        .position(|s| s.uid == uid)
                    {
                        Some(si) => Cursor::Session(wi, si),
                        None => Cursor::Workspace(wi),
                    }
                } else if let Some(task_id) = saved_task {
                    Cursor::Task { ws_idx: wi, task_id }
                } else {
                    Cursor::Workspace(wi)
                };
            }
        }
        self.clamp_cursor();
    }

    /// Clamp cursor so it points to a valid item.
    pub(super) fn clamp_cursor(&mut self) {
        // Backtest cursors are validated against the live rows/groups, not
        // the workspace list (their group renders even with zero
        // workspaces). Vanished target → step up: run → its fleet if it
        // still exists → the group header → first workspace.
        if let Cursor::Backtest(bc) = &self.cursor {
            if self.backtest_rows.is_empty() {
                self.cursor = Cursor::Workspace(0);
                self.clamp_cursor();
                return;
            }
            // Validate against the group's VISIBLE rows (fold state counts:
            // a run whose fleet just folded — or formed around it — is no
            // longer a row the cursor can sit on).
            let mut rows = Vec::new();
            self.append_backtest_items(&mut rows);
            let visible = rows.iter().any(|item| match (bc, item) {
                (BacktestCursor::Group, VisualItem::BacktestHeader) => true,
                (BacktestCursor::Fleet(stem), VisualItem::BacktestFleet(s)) => {
                    stem == s
                }
                (
                    BacktestCursor::Run(tid),
                    VisualItem::BacktestRun { task_id, .. },
                ) => tid == task_id,
                _ => false,
            });
            if !visible {
                self.cursor = Cursor::Backtest(BacktestCursor::Group);
            }
            return;
        }
        if self.workspaces.is_empty() {
            self.cursor = Cursor::Workspace(0);
            return;
        }
        let max = self.workspaces.len() - 1;
        match &self.cursor {
            Cursor::Workspace(wi) => {
                if *wi > max {
                    self.cursor = Cursor::Workspace(max);
                }
            }
            Cursor::Session(wi, si) => {
                let wi = *wi;
                let si = *si;
                if wi > max {
                    self.cursor = Cursor::Workspace(max);
                } else if self.workspaces[wi].sessions.is_empty() {
                    self.cursor = Cursor::Workspace(wi);
                } else if si >= self.workspaces[wi].sessions.len() {
                    self.cursor =
                        Cursor::Session(wi, self.workspaces[wi].sessions.len() - 1);
                }
            }
            Cursor::Task { ws_idx, task_id } => {
                let wi = *ws_idx;
                let tid = task_id.clone();
                if wi > max {
                    self.cursor = Cursor::Workspace(max);
                } else if !self
                    .tasks
                    .iter()
                    .any(|t| t.task_id.as_deref() == Some(tid.as_str()))
                {
                    // Task disappeared — fall back to workspace.
                    self.cursor = Cursor::Workspace(wi);
                }
            }
            // Fully handled by the early-return at the top of this fn.
            Cursor::Backtest(_) => {}
        }
    }

    /// Build visual items for the current sidebar view. The cloud-backtests
    /// group rides at the bottom of BOTH sub-views (its rows aren't sessions,
    /// so neither sub-view's own builder can place them).
    pub(super) fn visual_items(&self) -> Vec<VisualItem> {
        let mut items = match self.sidebar_view {
            SidebarView::Status => self.visual_items_status(),
            SidebarView::Task => self.visual_items_task(),
        };
        self.append_backtest_items(&mut items);
        items
    }

    /// Append the `backtests` group: a header (with rollup counts when
    /// folded), then — expanded — fleet rows (each folding its members)
    /// and singleton runs. Nothing is appended when no backtest row is
    /// live, so the sidebar is byte-identical to pre-feature.
    fn append_backtest_items(&self, items: &mut Vec<VisualItem>) {
        if self.backtest_rows.is_empty() {
            return;
        }
        if !items.is_empty() {
            items.push(VisualItem::Separator);
        }
        items.push(VisualItem::BacktestHeader);
        if self.backtests_folded {
            return;
        }
        for group in group_backtest_rows(&self.backtest_rows) {
            match group {
                BacktestGroupItem::Single(i) => items.push(VisualItem::BacktestRun {
                    task_id: self.backtest_rows[i].task_id.clone(),
                    depth: 0,
                }),
                BacktestGroupItem::Fleet { stem, members } => {
                    let unfolded = self.backtest_unfolded_fleets.contains(&stem);
                    items.push(VisualItem::BacktestFleet(stem));
                    if unfolded {
                        for i in members {
                            items.push(VisualItem::BacktestRun {
                                task_id: self.backtest_rows[i].task_id.clone(),
                                depth: 1,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Space/Enter while the cursor is inside the backtests group: fold or
    /// unfold what the cursor is on (the group header, or a fleet row). A
    /// run row folds its enclosing fleet — so Space anywhere in an expanded
    /// fleet collapses it without first navigating up. Returns whether
    /// anything toggled (the input path uses this to decide consumption).
    pub(super) fn toggle_backtest_fold(&mut self) -> bool {
        let cursor = match &self.cursor {
            Cursor::Backtest(c) => c.clone(),
            _ => return false,
        };
        match cursor {
            BacktestCursor::Group => {
                self.backtests_folded = !self.backtests_folded;
            }
            BacktestCursor::Fleet(stem) => {
                if !self.backtest_unfolded_fleets.remove(&stem) {
                    self.backtest_unfolded_fleets.insert(stem);
                }
            }
            BacktestCursor::Run(task_id) => {
                let Some(stem) = self
                    .backtest_rows
                    .iter()
                    .find(|r| r.task_id == task_id)
                    .and_then(|row| {
                        group_backtest_rows(&self.backtest_rows)
                            .into_iter()
                            .find_map(|g| match g {
                                BacktestGroupItem::Fleet { stem, members }
                                    if members.iter().any(|&i| {
                                        self.backtest_rows[i].task_id == row.task_id
                                    }) =>
                                {
                                    Some(stem)
                                }
                                _ => None,
                            })
                    })
                else {
                    return false; // singleton run — nothing to fold
                };
                self.backtest_unfolded_fleets.remove(&stem);
                self.cursor = Cursor::Backtest(BacktestCursor::Fleet(stem));
            }
        }
        self.needs_redraw = true;
        true
    }

    /// Status view: flat list of sessions grouped by status.
    /// Running sessions first, then idle, then workspaces with no sessions.
    /// Past workspaces (closed / all-tasks-done) are hidden — open the
    /// A-O picker to reach them.
    ///
    /// 12e: when `hosts.hosts.len() > 1`, the list is partitioned
    /// by host (in `HostsConfig` order); each host's section is
    /// preceded by a `HostHeader` row. Single-host (the
    /// synthesized local default) renders unchanged — no host
    /// header, identical to pre-12e.
    pub(super) fn visual_items_status(&self) -> Vec<VisualItem> {
        if self.hosts.hosts.len() > 1 {
            return self.visual_items_status_multihost();
        }
        let members = self.continuous_members();
        let mut running: Vec<VisualItem> = Vec::new();
        let mut idle: Vec<VisualItem> = Vec::new();
        let mut no_session: Vec<VisualItem> = Vec::new();

        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed || self.is_past_workspace(wi) {
                continue;
            }
            // Continuous-orchestrator sessions + their subtasks live ONLY in the
            // dedicated continuous column (or are hidden when it's off) — exclude
            // them from the main sidebar so a continuous task never shows twice.
            let visible: Vec<usize> = (0..ws.sessions.len())
                .filter(|si| !members.contains(&(wi, *si)))
                .collect();
            if ws.sessions.is_empty() {
                no_session.push(VisualItem::WorkspaceHeader(wi));
            } else if visible.is_empty() {
                // Continuous-only workspace → nothing in the main sidebar.
                continue;
            } else {
                for si in visible {
                    let item = VisualItem::Session(wi, si);
                    match ws.sessions[si].status {
                        SessionStatus::Running => running.push(item),
                        SessionStatus::Idle => idle.push(item),
                    }
                }
            }
        }

        let mut items = Vec::new();
        items.extend(running);
        if !items.is_empty() && (!idle.is_empty() || !no_session.is_empty()) {
            items.push(VisualItem::Separator);
        }
        items.extend(idle);
        if !items.is_empty() && !no_session.is_empty() {
            if !matches!(items.last(), Some(VisualItem::Separator)) {
                items.push(VisualItem::Separator);
            }
        }
        items.extend(no_session);
        items
    }

    /// 12e multi-host status view: emit a `HostHeader` per
    /// configured host, then the running + idle sessions
    /// belonging to that host. Workspaces with no sessions
    /// can't be host-tagged (workspace itself has no host) —
    /// they go in a single tail section after all host groups.
    pub(super) fn visual_items_status_multihost(&self) -> Vec<VisualItem> {
        // Per host: (running, idle). Continuous-orchestrator sessions + their
        // subtasks are EXCLUDED here — they render only in the dedicated
        // continuous column (or are hidden when it's off), never in the main
        // per-host groups.
        let members = self.continuous_members();
        let mut by_host: std::collections::HashMap<
            cm_daemon::host_id::HostId,
            (Vec<VisualItem>, Vec<VisualItem>),
        > = std::collections::HashMap::new();
        let mut no_session: Vec<VisualItem> = Vec::new();
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed || self.is_past_workspace(wi) {
                continue;
            }
            if ws.sessions.is_empty() {
                no_session.push(VisualItem::WorkspaceHeader(wi));
                continue;
            }
            for (si, ts) in ws.sessions.iter().enumerate() {
                if members.contains(&(wi, si)) {
                    continue;
                }
                let entry = by_host
                    .entry(ts.host_id.clone())
                    .or_insert_with(|| (Vec::new(), Vec::new()));
                let item = VisualItem::Session(wi, si);
                match ts.status {
                    SessionStatus::Running => entry.0.push(item),
                    SessionStatus::Idle => entry.1.push(item),
                }
            }
        }
        // Emit a host group (header + running + idle). A host group with no
        // non-continuous sessions is dropped (no bare header).
        let push_host_group = |items: &mut Vec<VisualItem>,
                               id: cm_daemon::host_id::HostId,
                               group: (Vec<VisualItem>, Vec<VisualItem>)| {
            if !items.is_empty() {
                items.push(VisualItem::Separator);
            }
            items.push(VisualItem::HostHeader(id));
            items.extend(group.0); // running
            items.extend(group.1); // idle
        };
        let mut items: Vec<VisualItem> = Vec::new();
        for host in &self.hosts.hosts {
            let group = by_host.remove(&host.id).unwrap_or_default();
            if group.0.is_empty() && group.1.is_empty() {
                continue;
            }
            push_host_group(&mut items, host.id.clone(), group);
        }
        // Sessions on hosts NO LONGER in hosts.toml (rare —
        // operator removed an entry; existing sessions remain
        // pinned). Surface them under a synthetic header so
        // they don't silently vanish.
        let mut orphan_ids: Vec<_> = by_host.keys().cloned().collect();
        orphan_ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        for id in orphan_ids {
            let group = by_host.remove(&id).unwrap();
            if group.0.is_empty() && group.1.is_empty() {
                continue;
            }
            push_host_group(&mut items, id, group);
        }
        if !items.is_empty() && !no_session.is_empty() {
            items.push(VisualItem::Separator);
        }
        items.extend(no_session);
        items
    }

    /// Task view: workspace headers with sessions indented underneath.
    /// Sessions grouped by workflow run appear contiguously under a workflow
    /// subheader. Standalone sessions render first; each workflow group
    /// follows. Past workspaces are hidden — reachable via the A-O picker.
    pub(super) fn visual_items_task(&self) -> Vec<VisualItem> {
        let members = self.continuous_members();
        let mut items = Vec::new();
        let mut first = true;
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed || self.is_past_workspace(wi) {
                continue;
            }
            // Continuous-orchestrator sessions + their subtasks render only in
            // the dedicated column (or are hidden when it's off). A workspace
            // whose ONLY sessions are continuous members — the orchestrator's
            // own workspace, a cm-sub subtask workspace — is skipped entirely so
            // no bare header is left in the main sidebar.
            if !ws.sessions.is_empty()
                && (0..ws.sessions.len()).all(|si| members.contains(&(wi, si)))
            {
                continue;
            }
            if !first {
                items.push(VisualItem::Separator);
            }
            first = false;
            items.push(VisualItem::WorkspaceHeader(wi));

            // Partition sessions by task_id bucket. Unbound sessions live in
            // the `None` bucket and render at workspace level (no subheader).
            // Continuous members are excluded (they live in the column).
            let mut by_task: std::collections::BTreeMap<Option<String>, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (si, ts) in ws.sessions.iter().enumerate() {
                if members.contains(&(wi, si)) {
                    continue;
                }
                by_task.entry(ts.task_id.clone()).or_default().push(si);
            }

            // Render buckets: unbound first, then task buckets in binding order.
            let task_order: Vec<Option<String>> = {
                let mut ordered: Vec<Option<String>> = Vec::new();
                if by_task.contains_key(&None) {
                    ordered.push(None);
                }
                // Tasks bound to this workspace, in insertion order of self.tasks.
                for task in &self.tasks {
                    if task.workspace_id.as_deref() != Some(ws.id.as_str()) {
                        continue;
                    }
                    let Some(tid) = task.task_id.as_deref() else { continue };
                    let key = Some(tid.to_string());
                    if by_task.contains_key(&key) && !ordered.contains(&key) {
                        ordered.push(key);
                    }
                }
                // Catch any orphaned task_ids tagged on sessions but not in
                // self.tasks (stale API state) — render them too so they don't
                // silently disappear.
                let mut tail: Vec<Option<String>> = by_task
                    .keys()
                    .filter(|k| k.is_some() && !ordered.contains(k))
                    .cloned()
                    .collect();
                tail.sort();
                ordered.extend(tail);
                ordered
            };

            for bucket_key in task_order {
                // Emit task subheader for task-scoped buckets.
                if let Some(tid) = bucket_key.as_deref() {
                    items.push(VisualItem::TaskHeader {
                        ws_idx: wi,
                        task_id: tid.to_string(),
                    });
                }
                let indices = by_task.remove(&bucket_key).unwrap_or_default();

                // Split each bucket into standalone + workflow groups.
                let mut standalone: Vec<usize> = Vec::new();
                let mut groups: std::collections::BTreeMap<String, Vec<usize>> =
                    std::collections::BTreeMap::new();
                for si in &indices {
                    match &ws.sessions[*si].workflow_run_id {
                        Some(run_id) => groups.entry(run_id.clone()).or_default().push(*si),
                        None => standalone.push(*si),
                    }
                }
                // Standalone: running first, then idle.
                let (run_, other): (Vec<_>, Vec<_>) = standalone
                    .into_iter()
                    .partition(|si| ws.sessions[*si].status == SessionStatus::Running);
                for si in run_ {
                    items.push(VisualItem::Session(wi, si));
                }
                for si in other {
                    items.push(VisualItem::Session(wi, si));
                }
                // Workflow groups.
                for (run_id, session_indices) in groups {
                    let is_active_run =
                        self.workflow_runs.iter().any(|r| r.run_id == run_id);
                    if !is_active_run {
                        for si in session_indices {
                            items.push(VisualItem::Session(wi, si));
                        }
                        continue;
                    }
                    items.push(VisualItem::WorkflowHeader {
                        ws_idx: wi,
                        run_id: run_id.clone(),
                    });
                    let role_order: Vec<String> = self
                        .workflow_runs
                        .iter()
                        .find(|r| r.run_id == run_id)
                        .and_then(|r| self.workflows.get(&r.workflow_name))
                        .map(|wf| wf.role_order.clone())
                        .unwrap_or_default();
                    let mut ordered = session_indices.clone();
                    ordered.sort_by_key(|si| {
                        let role = ws.sessions[*si].workflow_role.as_deref().unwrap_or("");
                        role_order.iter().position(|r| r == role).unwrap_or(usize::MAX)
                    });
                    for si in ordered {
                        items.push(VisualItem::Session(wi, si));
                    }
                }
            }

        }
        items
    }

    /// Map each session task_id to its `parent_task_id` (from `self.tasks`).
    /// The respawn-robust link a continuous subtask keeps to its orchestrator:
    /// the orchestrator session's uid changes when it respawns (fresh context /
    /// restart), but the TASK tree doesn't, so a subtask's `parent_task_id`
    /// still points at the orchestrator's task.
    fn task_parent_map(&self) -> std::collections::HashMap<&str, &str> {
        self.tasks
            .iter()
            .filter_map(|t| Some((t.task_id.as_deref()?, t.parent_task_id.as_deref()?)))
            .collect()
    }

    /// The set of `(ws_idx, sess_idx)` that belong to a continuous orchestrator's
    /// tree: an orchestrator itself (`continuous_task_id`), or a subtask of one
    /// — matched by `managed_by_uid == orchestrator.uid` (a direct child of the
    /// CURRENT orchestrator session) OR by task-tree parent
    /// (`parent_task_id == orchestrator.task_id`, which survives a respawn that
    /// minted a new session uid — the latter is why BUG-007/008, spawned by a
    /// prior orchestrator instance, still group under the live orchestrator)
    /// OR an agent-spawned session bound to the orchestrator's OWN task
    /// (`managed_by_uid.is_some() && task_id == orchestrator.task_id` — the
    /// momentum-detective shape: ephemeral workers spawned with no subtask,
    /// so they share the orchestrator's task and worktree). Without that
    /// third rule a worker spawned by a PRIOR orchestrator instance fell
    /// out of the column into the main sidebar the moment the scheduler
    /// respawned its parent (its `managed_by_uid` now names a dead uid,
    /// and there is no `parent_task_id` to fall back on).
    ///
    /// These render ONLY in the dedicated continuous column (when it's on) or
    /// nowhere (when it's off) — the main sidebar builders exclude them, so a
    /// continuous task never appears in both places. Closed workspaces skipped.
    pub(super) fn continuous_members(&self) -> std::collections::HashSet<(usize, usize)> {
        use std::collections::HashSet;
        let mut orch_uids: HashSet<&str> = HashSet::new();
        let mut orch_tasks: HashSet<&str> = HashSet::new();
        for ws in &self.workspaces {
            if ws.is_closed {
                continue;
            }
            for ts in &ws.sessions {
                if ts.continuous_task_id.is_some() {
                    orch_uids.insert(ts.uid.as_str());
                    if let Some(t) = ts.task_id.as_deref() {
                        orch_tasks.insert(t);
                    }
                }
            }
        }
        let parent_of = self.task_parent_map();
        let mut keys: HashSet<(usize, usize)> = HashSet::new();
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed {
                continue;
            }
            for (si, ts) in ws.sessions.iter().enumerate() {
                let by_uid = ts
                    .managed_by_uid
                    .as_deref()
                    .is_some_and(|u| orch_uids.contains(u));
                let by_task = ts
                    .task_id
                    .as_deref()
                    .and_then(|t| parent_of.get(t).copied())
                    .is_some_and(|p| orch_tasks.contains(p));
                let by_same_task = ts.managed_by_uid.is_some()
                    && ts.continuous_task_id.is_none()
                    && ts
                        .task_id
                        .as_deref()
                        .is_some_and(|t| orch_tasks.contains(t));
                if ts.continuous_task_id.is_some() || by_uid || by_task || by_same_task {
                    keys.insert((wi, si));
                }
            }
        }
        keys
    }

    /// Build the dedicated continuous column's rows (DESIGN_CONTINUOUS_PANEL.md):
    /// each continuous-task orchestrator (a session tagged `continuous_task_id`)
    /// at `depth 0`, followed by its subtasks nested at `depth 1`. A subtask
    /// nests under an orchestrator when its `managed_by_uid` is that
    /// orchestrator's uid OR its task's `parent_task_id` is the orchestrator's
    /// task_id (the respawn-robust link — see `continuous_members`) OR it is
    /// agent-spawned onto the orchestrator's own task (same-task workers —
    /// each its own depth-1 row, never merged into one task group).
    /// Orchestrators ordered by label (then uid); subtasks by label under each.
    /// Closed workspaces skipped. Pure read — the render + cursor consume this;
    /// it's only drawn/navigated when the column is on.
    pub(super) fn visual_items_continuous(&self) -> Vec<ContinuousRow> {
        // Orchestrators: (ws_idx, sess_idx, uid, task_id, label).
        let mut orchestrators: Vec<(usize, usize, &str, Option<&str>, &str)> = Vec::new();
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed {
                continue;
            }
            for (si, ts) in ws.sessions.iter().enumerate() {
                if ts.continuous_task_id.is_some() {
                    orchestrators.push((
                        wi,
                        si,
                        ts.uid.as_str(),
                        ts.task_id.as_deref(),
                        ts.label.as_str(),
                    ));
                }
            }
        }
        orchestrators.sort_by(|a, b| a.4.cmp(b.4).then(a.2.cmp(b.2)));
        let parent_of = self.task_parent_map();

        use std::collections::BTreeMap;
        let mut rows = Vec::new();
        for &(owi, osi, ouid, otask, _) in &orchestrators {
            rows.push(ContinuousRow {
                ws_idx: owi,
                sess_idx: osi,
                depth: 0,
            });
            // This orchestrator's member sessions (excluding orchestrators
            // themselves — a continuous task nested under another stays
            // top-level). Matched by managed_by_uid OR task-tree parent. A
            // member carries its own task_id so we can regroup below.
            // Re-scan (immutable; small N). (wi, si, task_id, is_bash, label)
            let mut members: Vec<(usize, usize, Option<&str>, bool, &str)> = Vec::new();
            for (wi, ws) in self.workspaces.iter().enumerate() {
                if ws.is_closed {
                    continue;
                }
                for (si, ts) in ws.sessions.iter().enumerate() {
                    if ts.continuous_task_id.is_some() {
                        continue;
                    }
                    let by_uid = ts.managed_by_uid.as_deref() == Some(ouid);
                    let by_task = otask.is_some()
                        && ts
                            .task_id
                            .as_deref()
                            .and_then(|t| parent_of.get(t).copied())
                            == otask;
                    let by_same_task = otask.is_some()
                        && ts.managed_by_uid.is_some()
                        && ts.task_id.as_deref() == otask;
                    if by_uid || by_task || by_same_task {
                        // Same-task workers carry the ORCHESTRATOR's task_id;
                        // grouping them by it would fold every worker under
                        // one anchor. Treat them as taskless (own group).
                        let group_task = if by_same_task && !by_task {
                            None
                        } else {
                            ts.task_id.as_deref()
                        };
                        members.push((
                            wi,
                            si,
                            group_task,
                            ts.session_type == "bash",
                            ts.label.as_str(),
                        ));
                    }
                }
            }
            // Group members by their subtask (task_id) so multiple sessions in
            // ONE subtask nest together: e.g. a bash the operator adds to a
            // subtask sits UNDER that subtask's agent, not as a flat sibling.
            // The agent (non-bash) is the depth-1 anchor of each group; extra
            // sessions nest at depth 2. A single-session subtask is unchanged
            // (one depth-1 row) — so the common case looks identical.
            // Members sharing a REAL task_id form one group (the extra session
            // nests under the subtask's agent). A member with NO task_id (a
            // managed_by_uid match with no planning task) is its OWN group — a
            // distinct depth-1 entry, never merged with other taskless members.
            let mut task_groups: BTreeMap<&str, Vec<(usize, usize, bool, &str)>> =
                BTreeMap::new();
            let mut session_groups: Vec<Vec<(usize, usize, bool, &str)>> = Vec::new();
            for (wi, si, tid, is_bash, label) in members {
                match tid {
                    Some(t) => task_groups
                        .entry(t)
                        .or_default()
                        .push((wi, si, is_bash, label)),
                    None => session_groups.push(vec![(wi, si, is_bash, label)]),
                }
            }
            session_groups.extend(task_groups.into_values());
            // (anchor_label, group_rows) — ordered by anchor label so the
            // one-session-per-subtask ordering matches the old label sort.
            let mut groups: Vec<(String, Vec<ContinuousRow>)> = Vec::new();
            for mut sessions in session_groups {
                // Anchor first: agents (non-bash) before bash, then by label.
                sessions.sort_by(|a, b| a.2.cmp(&b.2).then(a.3.cmp(b.3)));
                let anchor_label = sessions[0].3.to_string();
                let group_rows = sessions
                    .iter()
                    .enumerate()
                    .map(|(idx, &(wi, si, _, _))| ContinuousRow {
                        ws_idx: wi,
                        sess_idx: si,
                        depth: if idx == 0 { 1 } else { 2 },
                    })
                    .collect();
                groups.push((anchor_label, group_rows));
            }
            groups.sort_by(|a, b| a.0.cmp(&b.0));
            for (_label, group_rows) in groups {
                rows.extend(group_rows);
            }
        }
        rows
    }

    /// Move the cursor between sidebar columns (S4). `dir > 0` steps RIGHT
    /// (toward the continuous column), `dir < 0` steps LEFT (back to main).
    /// No-op when the continuous column isn't shown; entering an empty
    /// continuous column is also a no-op. EACH column remembers the row you
    /// left off at: the main cursor is stashed on the way in / restored on the
    /// way out, and the continuous cursor likewise — so re-entering a column
    /// lands where you were, not on its first row (the saved continuous spot is
    /// validated against the live rows and falls back to the first if gone).
    /// The stable session UID at the current cursor — used to stash a column
    /// position that survives a manifest reindex (indices shift on a refresh;
    /// UIDs don't). None if the cursor isn't on a live session.
    pub(super) fn cursor_session_uid(&self) -> Option<String> {
        match &self.cursor {
            Cursor::Session(wi, si) => self
                .workspaces
                .get(*wi)
                .and_then(|w| w.sessions.get(*si))
                .map(|s| s.uid.clone()),
            _ => None,
        }
    }

    pub(super) fn step_column(&mut self, dir: i32) {
        if !self.continuous_column_on {
            self.cursor_column = SidebarColumn::Main;
            return;
        }
        match (self.cursor_column, dir) {
            (SidebarColumn::Main, d) if d > 0 => {
                let rows = self.visual_items_continuous();
                if rows.is_empty() {
                    return;
                }
                // Restore the last continuous spot by UID (stable across a
                // manifest reindex) if that session is still a row; else the
                // first row.
                let target = self
                    .saved_continuous_uid
                    .as_deref()
                    .and_then(|uid| {
                        rows.iter().find(|r| {
                            self.workspaces[r.ws_idx].sessions[r.sess_idx].uid == uid
                        })
                    })
                    .map(|r| Cursor::Session(r.ws_idx, r.sess_idx))
                    .unwrap_or_else(|| {
                        let r = rows[0];
                        Cursor::Session(r.ws_idx, r.sess_idx)
                    });
                self.saved_main_cursor = Some(self.cursor.clone());
                self.cursor = target;
                self.cursor_column = SidebarColumn::Continuous;
                self.needs_redraw = true;
            }
            (SidebarColumn::Continuous, d) if d < 0 => {
                self.saved_continuous_uid = self.cursor_session_uid();
                self.cursor = self
                    .saved_main_cursor
                    .take()
                    .unwrap_or(Cursor::Workspace(0));
                self.cursor_column = SidebarColumn::Main;
                self.needs_redraw = true;
            }
            _ => {}
        }
    }

    /// Navigate the cursor up or down. +1 = down, -1 = up.
    /// Skips non-selectable items (Separators, headers with sessions).
    pub(super) fn navigate(&mut self, direction: i32) {
        // S4: in the continuous column, navigate over its own row list
        // (every row is a selectable session, so just wrap ±1).
        if self.cursor_column == SidebarColumn::Continuous && self.continuous_column_on {
            let rows = self.visual_items_continuous();
            if rows.is_empty() {
                // Column emptied out from under us — fall back to main.
                self.cursor = self
                    .saved_main_cursor
                    .take()
                    .unwrap_or(Cursor::Workspace(0));
                self.cursor_column = SidebarColumn::Main;
                return;
            }
            let cur = match &self.cursor {
                Cursor::Session(wi, si) => rows
                    .iter()
                    .position(|r| r.ws_idx == *wi && r.sess_idx == *si)
                    .unwrap_or(0),
                _ => 0,
            };
            let n = rows.len() as i32;
            let next = (cur as i32 + direction).rem_euclid(n) as usize;
            let r = rows[next];
            self.cursor = Cursor::Session(r.ws_idx, r.sess_idx);
            return;
        }

        let items = self.visual_items();
        if items.is_empty() {
            return;
        }

        // Workspace headers are selectable only when the workspace has no
        // sessions (otherwise the cursor lives on a child session).
        // Task headers are always selectable — they support A-d / A-x / A-e
        // etc. even when they have sessions underneath.
        let is_selectable = |item: &VisualItem| match item {
            VisualItem::Session(_, _) => true,
            VisualItem::WorkspaceHeader(wi) => self
                .workspaces
                .get(*wi)
                .map_or(false, |w| w.sessions.is_empty()),
            VisualItem::TaskHeader { .. } => true,
            VisualItem::Separator => false,
            VisualItem::WorkflowHeader { .. } => false,
            // 12e: host headers are presentation-only; skip
            // them in cursor navigation.
            VisualItem::HostHeader(_) => false,
            // Continuous header is presentation-only, like
            // `HostHeader`; skip it in cursor navigation.
            VisualItem::ContinuousHeader => false,
            // Backtest rows are all selectable — the header and fleet rows
            // for the Space/Enter fold toggle, run rows for the A-i peek.
            VisualItem::BacktestHeader => true,
            VisualItem::BacktestFleet(_) => true,
            VisualItem::BacktestRun { .. } => true,
        };

        if !items.iter().any(is_selectable) {
            return;
        }

        let cur_pos = items
            .iter()
            .position(|item| match (&self.cursor, item) {
                (Cursor::Workspace(wi), VisualItem::WorkspaceHeader(vwi)) => wi == vwi,
                (Cursor::Session(wi, si), VisualItem::Session(vwi, vsi)) => {
                    wi == vwi && si == vsi
                }
                (
                    Cursor::Task { ws_idx, task_id },
                    VisualItem::TaskHeader { ws_idx: vwi, task_id: vtid },
                ) => ws_idx == vwi && task_id == vtid,
                (
                    Cursor::Backtest(BacktestCursor::Group),
                    VisualItem::BacktestHeader,
                ) => true,
                (
                    Cursor::Backtest(BacktestCursor::Fleet(stem)),
                    VisualItem::BacktestFleet(vstem),
                ) => stem == vstem,
                (
                    Cursor::Backtest(BacktestCursor::Run(tid)),
                    VisualItem::BacktestRun { task_id: vtid, .. },
                ) => tid == vtid,
                _ => false,
            })
            .unwrap_or(0);

        let len = items.len() as i32;
        let mut next = cur_pos as i32;
        for _ in 0..items.len() {
            next = (next + direction).rem_euclid(len);
            if is_selectable(&items[next as usize]) {
                break;
            }
        }

        match &items[next as usize] {
            VisualItem::Session(wi, si) => self.cursor = Cursor::Session(*wi, *si),
            VisualItem::WorkspaceHeader(wi) => self.cursor = Cursor::Workspace(*wi),
            VisualItem::TaskHeader { ws_idx, task_id } => {
                self.cursor = Cursor::Task {
                    ws_idx: *ws_idx,
                    task_id: task_id.clone(),
                };
            }
            VisualItem::BacktestHeader => {
                self.cursor = Cursor::Backtest(BacktestCursor::Group);
            }
            VisualItem::BacktestFleet(stem) => {
                self.cursor = Cursor::Backtest(BacktestCursor::Fleet(stem.clone()));
            }
            VisualItem::BacktestRun { task_id, .. } => {
                self.cursor = Cursor::Backtest(BacktestCursor::Run(task_id.clone()));
            }
            _ => {}
        }
    }

    /// A-g: jump the cursor to the next session that needs a human, in
    /// priority order — pending `notify_user` alerts first, then idle
    /// sessions (any age) — cycling in sidebar visual order (main column
    /// rows first, then the continuous column's rows when it's shown).
    /// Hidden sessions are skipped unless they hold an alert (alerts
    /// override hidden, matching the sidebar indicator). Uses the same
    /// cursor assignment navigation uses, so the attached terminal
    /// follows; column bookkeeping mirrors `step_column` so a later
    /// A-h/A-l lands back where you were.
    pub(super) fn jump_to_next_attention(&mut self) {
        // (ws_idx, sess_idx, is_continuous_row) in sidebar visual order.
        let mut rows: Vec<(usize, usize, bool)> = Vec::new();
        for vi in self.visual_items() {
            if let VisualItem::Session(wi, si) = vi {
                rows.push((wi, si, false));
            }
        }
        if self.continuous_column_on {
            for r in self.visual_items_continuous() {
                rows.push((r.ws_idx, r.sess_idx, true));
            }
        }
        let flags: Vec<(bool, bool, bool)> = rows
            .iter()
            .map(|(wi, si, _)| {
                let ts = &self.workspaces[*wi].sessions[*si];
                (
                    self.session_has_alert(&ts.uid),
                    ts.status == SessionStatus::Idle && !ts.session.exited,
                    ts.hidden,
                )
            })
            .collect();
        let current = match &self.cursor {
            Cursor::Session(cwi, csi) => rows
                .iter()
                .position(|(wi, si, _)| wi == cwi && si == csi),
            _ => None,
        };
        let Some(next) = next_attention_index(&flags, current) else {
            self.set_status_msg("No sessions need attention");
            return;
        };
        let (wi, si, to_continuous) = rows[next];
        // Crossing between columns keeps step_column's saved-spot invariants.
        if to_continuous && self.cursor_column == SidebarColumn::Main {
            self.saved_main_cursor = Some(self.cursor.clone());
        }
        if !to_continuous && self.cursor_column == SidebarColumn::Continuous {
            self.saved_continuous_uid = self.cursor_session_uid();
        }
        self.cursor_column = if to_continuous {
            SidebarColumn::Continuous
        } else {
            SidebarColumn::Main
        };
        self.cursor = Cursor::Session(wi, si);
        self.needs_redraw = true;
    }

    /// uid → `(ws_idx, sess_idx)` among non-closed workspaces. `None`
    /// when the session is gone for good (workspace closed or session
    /// removed) — MRU jumps DROP such uids from the deque.
    fn find_session_by_uid(&self, uid: &str) -> Option<(usize, usize)> {
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed {
                continue;
            }
            if let Some(si) = ws.sessions.iter().position(|s| s.uid == uid) {
                return Some((wi, si));
            }
        }
        None
    }

    /// Resolve a uid to a jumpable cursor target: `(ws_idx, sess_idx,
    /// lives_in_continuous_column)`. Continuous-member sessions are only
    /// jumpable while the continuous column is shown — when it's off
    /// they're hidden entirely, so a cursor there would sit on an
    /// invisible row. Such uids are SKIPPED (kept in the MRU deque; they
    /// come back when the column is toggled on), not dropped.
    fn resolve_session_target(&self, uid: &str) -> Option<(usize, usize, bool)> {
        let (wi, si) = self.find_session_by_uid(uid)?;
        let continuous = self.continuous_members().contains(&(wi, si));
        if continuous && !self.continuous_column_on {
            return None;
        }
        Some((wi, si, continuous))
    }

    /// Move the cursor onto session `uid`, crossing sidebar columns with
    /// the same saved-spot bookkeeping as `jump_to_next_attention` /
    /// `step_column` (so a later A-h/A-l lands back where you were). The
    /// terminal pane follows because it renders the cursor's session.
    /// Returns false when the uid doesn't resolve to a visible row.
    fn jump_cursor_to_session_uid(&mut self, uid: &str) -> bool {
        let Some((wi, si, to_continuous)) = self.resolve_session_target(uid) else {
            return false;
        };
        if to_continuous && self.cursor_column == SidebarColumn::Main {
            self.saved_main_cursor = Some(self.cursor.clone());
        }
        if !to_continuous && self.cursor_column == SidebarColumn::Continuous {
            self.saved_continuous_uid = self.cursor_session_uid();
        }
        self.cursor_column = if to_continuous {
            SidebarColumn::Continuous
        } else {
            SidebarColumn::Main
        };
        self.cursor = Cursor::Session(wi, si);
        self.needs_redraw = true;
        true
    }

    /// MRU recording funnel. Every USER-driven cursor move (A-j/k, A-g,
    /// A-h/l, the A-p palette jump, A-; itself) captures the focused
    /// session uid BEFORE the move and calls this after: if focus landed
    /// somewhere else, the previous session is pushed onto the MRU deque.
    /// Reconcile-driven cursor shuffles never come through here — they
    /// aren't user intent and must not pollute the history.
    pub(super) fn note_session_focus_change(&mut self, prev_uid: Option<String>) {
        if self.cursor_session_uid() == prev_uid {
            return;
        }
        if let Some(p) = prev_uid {
            mru_record(&mut self.session_mru, &p, SESSION_MRU_CAP);
        }
    }

    /// A-;: jump to the most recent OTHER session. Repeated presses walk
    /// deeper into the MRU ring (classic alt-tab): the ring is frozen at
    /// the first press of a walk and any other key resets the walk, so
    /// press-press-press goes current → B → C → … and wraps back through
    /// the start. Only the walk's STARTING focus is recorded into the
    /// deque — sessions passed through mid-walk are transient and don't
    /// reorder the history (matching classic alt-tab semantics), which is
    /// also what makes a single press ping-pong A↔B.
    pub(super) fn mru_jump(&mut self) {
        // Prune uids that are gone for good. Continuous-hidden sessions
        // are NOT dead (resolve_session_target skips them instead).
        let dead: Vec<String> = self
            .session_mru
            .iter()
            .filter(|u| self.find_session_by_uid(u).is_none())
            .cloned()
            .collect();
        if !dead.is_empty() {
            self.session_mru.retain(|u| !dead.contains(u));
        }

        let current = self.cursor_session_uid();
        let fresh_walk = self.mru_walk.is_none();
        if fresh_walk {
            // Freeze the ring: slot 0 = the walk's start (an empty
            // sentinel when the cursor isn't on a session — it never
            // resolves, so wrap-around skips it), then the MRU deque.
            let mut list: Vec<String> = vec![current.clone().unwrap_or_default()];
            for uid in &self.session_mru {
                if !list.iter().any(|u| u == uid) {
                    list.push(uid.clone());
                }
            }
            self.mru_walk = Some(MruWalk { list, pos: 0 });
        }
        let picked = {
            let walk = self.mru_walk.as_ref().expect("walk ensured above");
            mru_next_walk_target(&walk.list, walk.pos, current.as_deref(), |u| {
                self.resolve_session_target(u).is_some()
            })
            .map(|p| (p, walk.list[p].clone()))
        };
        let Some((pos, uid)) = picked else {
            self.mru_walk = None;
            self.set_status_msg("No recent session to switch to");
            return;
        };
        if let Some(w) = self.mru_walk.as_mut() {
            w.pos = pos;
        }
        if self.jump_cursor_to_session_uid(&uid) && fresh_walk {
            if let Some(prev) = current {
                mru_record(&mut self.session_mru, &prev, SESSION_MRU_CAP);
            }
        }
    }

    /// Open the A-p palette over a snapshot of the current rows.
    pub(super) fn open_session_palette(&mut self) {
        self.input_mode = InputMode::SessionPalette {
            candidates: self.build_palette_candidates(),
            query: String::new(),
            selected: 0,
        };
    }

    /// Candidate rows for the palette: every non-closed workspace (header
    /// row) and its sessions, in `self.workspaces` order (the stable
    /// backbone of sidebar order). Session rows show
    /// "workspace / label", plus "[role]" for workflow participants and
    /// the bound task's name; continuous members get a "⟳cont" tag and
    /// are listed only while the continuous column is on (off = they're
    /// hidden entirely and unjumpable).
    fn build_palette_candidates(&self) -> Vec<PaletteCandidate> {
        let members = self.continuous_members();
        let mut out: Vec<PaletteCandidate> = Vec::new();
        for (wi, ws) in self.workspaces.iter().enumerate() {
            if ws.is_closed {
                continue;
            }
            out.push(PaletteCandidate {
                target: PaletteTarget::Workspace { ws_id: ws.id.clone() },
                display: ws.name.clone(),
            });
            for (si, ts) in ws.sessions.iter().enumerate() {
                let is_continuous = members.contains(&(wi, si));
                if is_continuous && !self.continuous_column_on {
                    continue;
                }
                let mut display = format!("{} / {}", ws.name, ts.label);
                if let Some(role) = ts.workflow_role.as_deref() {
                    display.push_str(&format!(" [{}]", role));
                }
                if let Some(task) = ts
                    .task_id
                    .as_deref()
                    .and_then(|tid| self.find_task_entry(tid))
                {
                    display.push_str(&format!(" \u{00b7} {}", task.name));
                }
                if is_continuous {
                    display.push_str(" \u{27f3}cont");
                }
                out.push(PaletteCandidate {
                    target: PaletteTarget::Session { uid: ts.uid.clone() },
                    display,
                });
            }
        }
        out
    }

    /// Apply an A-p palette pick: resolve the stable id against the live
    /// rows and move the cursor (recording the hop in the MRU history).
    pub(super) fn apply_palette_jump(&mut self, target: PaletteTarget) {
        let prev = self.cursor_session_uid();
        match target {
            PaletteTarget::Session { uid } => {
                if self.jump_cursor_to_session_uid(&uid) {
                    self.note_session_focus_change(prev);
                } else {
                    self.set_status_msg("Session is gone");
                }
            }
            PaletteTarget::Workspace { ws_id } => {
                let Some(wi) = resolve_workspace_by_id(&self.workspaces, &ws_id) else {
                    self.set_status_msg("Workspace is gone");
                    return;
                };
                // Headers are selectable only when the workspace has no
                // sessions (navigate()'s rule); otherwise land on the
                // first session that resolves to a visible row.
                let first_uid = self.workspaces[wi]
                    .sessions
                    .iter()
                    .map(|s| s.uid.clone())
                    .find(|u| self.resolve_session_target(u).is_some());
                match first_uid {
                    Some(uid) => {
                        if self.jump_cursor_to_session_uid(&uid) {
                            self.note_session_focus_change(prev);
                        }
                    }
                    None if self.workspaces[wi].sessions.is_empty() => {
                        if self.cursor_column == SidebarColumn::Continuous {
                            self.saved_continuous_uid = self.cursor_session_uid();
                        }
                        self.cursor_column = SidebarColumn::Main;
                        self.cursor = Cursor::Workspace(wi);
                        self.note_session_focus_change(prev);
                        self.needs_redraw = true;
                    }
                    None => {
                        self.set_status_msg(
                            "Workspace's sessions are hidden (continuous column off)",
                        );
                    }
                }
            }
        }
    }

    fn find_task_entry(&self, task_id: &str) -> Option<&TaskEntry> {
        self.tasks
            .iter()
            .find(|t| t.task_id.as_deref() == Some(task_id))
    }

    /// Open the A-i info overlay for the focused row. Content is
    /// assembled once at open (a read-only snapshot); `max_scroll` starts
    /// at 0 and is written back by the first draw.
    pub(super) fn open_task_peek(&mut self) {
        self.input_mode = InputMode::TaskPeek {
            lines: self.build_peek_lines(),
            scroll: 0,
            max_scroll: 0,
        };
    }

    /// Workspace-fallback peek body (no bound task): identity + roster.
    fn workspace_peek_lines(&self, ws: &Workspace) -> Vec<PeekLine> {
        let wt = ws.worktree_path.as_ref().map(|p| p.display().to_string());
        let main = ws.main_repo_path.as_ref().map(|p| p.display().to_string());
        let sessions: Vec<(String, String, String)> = ws
            .sessions
            .iter()
            .map(|ts| {
                let status = if ts.session.exited {
                    "exited".to_string()
                } else {
                    match ts.status {
                        SessionStatus::Running => "running".to_string(),
                        SessionStatus::Idle => "idle".to_string(),
                    }
                };
                (ts.label.clone(), ts.session_type.clone(), status)
            })
            .collect();
        peek_workspace_lines(
            &ws.name,
            wt.as_deref(),
            main.as_deref(),
            ws.repo_url.as_deref(),
            ws.host_id.as_str(),
            &sessions,
        )
    }

    /// Assemble the peek content for the focused row: bound-task detail
    /// when a task resolves (session's `task_id` or the Task cursor's),
    /// the workspace fallback otherwise; Session cursors get an identity
    /// header block on top either way.
    fn build_peek_lines(&self) -> Vec<PeekLine> {
        let mut out: Vec<PeekLine> = Vec::new();
        match &self.cursor {
            Cursor::Session(wi, si) => {
                let Some(ws) = self.workspaces.get(*wi) else {
                    return vec![PeekLine::Text("Nothing focused.".into())];
                };
                let Some(ts) = ws.sessions.get(*si) else {
                    return vec![PeekLine::Text("Nothing focused.".into())];
                };
                out.extend(peek_session_lines(
                    &ts.label,
                    &ts.session_type,
                    &ts.uid,
                    ts.seeded_from_snapshot.as_deref(),
                    ts.last_delivery.as_ref().map(|(s, _)| s.as_str()),
                ));
                out.push(PeekLine::Blank);
                match ts.task_id.as_deref().and_then(|t| self.find_task_entry(t)) {
                    Some(task) => {
                        let parent = task
                            .parent_task_id
                            .as_deref()
                            .and_then(|p| self.find_task_entry(p))
                            .map(|t| t.name.as_str());
                        out.extend(peek_task_lines(task, parent));
                    }
                    None => out.extend(self.workspace_peek_lines(ws)),
                }
            }
            Cursor::Task { ws_idx, task_id } => match self.find_task_entry(task_id) {
                Some(task) => {
                    let parent = task
                        .parent_task_id
                        .as_deref()
                        .and_then(|p| self.find_task_entry(p))
                        .map(|t| t.name.as_str());
                    out.extend(peek_task_lines(task, parent));
                }
                None => {
                    if let Some(ws) = self.workspaces.get(*ws_idx) {
                        out.extend(self.workspace_peek_lines(ws));
                    }
                }
            },
            Cursor::Workspace(wi) => {
                if let Some(ws) = self.workspaces.get(*wi) {
                    out.extend(self.workspace_peek_lines(ws));
                }
            }
            Cursor::Backtest(bc) => out.extend(self.backtest_peek_lines(bc)),
        }
        if out.is_empty() {
            out.push(PeekLine::Text("Nothing focused.".into()));
        }
        out
    }

    /// A-i peek body for the backtests group: per-run detail on a run row
    /// (status, VM, runtime, result pointer), a member roster on a fleet
    /// row, a rollup on the header.
    fn backtest_peek_lines(&self, bc: &BacktestCursor) -> Vec<PeekLine> {
        let now = Instant::now();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let state_label = |row: &BacktestRow| -> String {
            match &row.state {
                BacktestState::Queued => "queued".into(),
                BacktestState::Running { vm: Some(vm) } => {
                    format!("running on {}", vm)
                }
                BacktestState::Running { vm: None } => "dispatching".into(),
                BacktestState::Done => "complete".into(),
                BacktestState::Failed => "failed".into(),
            }
        };
        let run_lines = |row: &BacktestRow| -> Vec<PeekLine> {
            let mut out = vec![
                PeekLine::Title(format!("backtest: {}", row.label)),
                PeekLine::Field {
                    label: "Status".into(),
                    value: state_label(row),
                },
                PeekLine::Field {
                    label: "Branch".into(),
                    value: row.branch.clone(),
                },
                PeekLine::Field {
                    label: "Task".into(),
                    value: row.task_id.clone(),
                },
            ];
            if let Some(secs) = runtime_secs(row, now, now_ms) {
                out.push(PeekLine::Field {
                    label: "Runtime".into(),
                    value: fmt_runtime(secs),
                });
            }
            if let Some(rk) = &row.run_key {
                out.push(PeekLine::Field {
                    label: "Results".into(),
                    value: format!(
                        "get_backtest_result({}) · GCS backtests/{}/",
                        &row.task_id[..8.min(row.task_id.len())],
                        rk
                    ),
                });
            }
            out
        };
        match bc {
            BacktestCursor::Run(tid) => self
                .backtest_rows
                .iter()
                .find(|r| &r.task_id == tid)
                .map(run_lines)
                .unwrap_or_default(),
            BacktestCursor::Fleet(stem) => {
                let mut out =
                    vec![PeekLine::Title(format!("backtest fleet: {}", stem))];
                for row in self
                    .backtest_rows
                    .iter()
                    .filter(|r| fleet_stem(&r.label) == Some(stem.as_str()))
                {
                    let runtime = runtime_secs(row, now, now_ms)
                        .map(|s| format!(" · {}", fmt_runtime(s)))
                        .unwrap_or_default();
                    out.push(PeekLine::Field {
                        label: row.label.clone(),
                        value: format!("{}{}", state_label(row), runtime),
                    });
                }
                out
            }
            BacktestCursor::Group => {
                let mut out = vec![PeekLine::Title("cloud backtests".into())];
                for row in &self.backtest_rows {
                    let runtime = runtime_secs(row, now, now_ms)
                        .map(|s| format!(" · {}", fmt_runtime(s)))
                        .unwrap_or_default();
                    out.push(PeekLine::Field {
                        label: row.label.clone(),
                        value: format!("{}{}", state_label(row), runtime),
                    });
                }
                out.push(PeekLine::Blank);
                out.push(PeekLine::Text(
                    "Space/Enter folds the group; results stay readable via \
                     get_backtest_result."
                        .into(),
                ));
                out
            }
        }
    }
}

#[cfg(test)]
mod idle_attention_tests {
    use super::*;

    // ── idle_age_bucket: the pure age → display-bucket classifier ──

    #[test]
    fn unknown_idle_age_is_stale() {
        // None = "we don't know when it went idle" (e.g. the spell started
        // before a TUI restart) — must land in the OLDEST bucket, never in
        // afterglow, so a restart can't light up every idle row.
        assert_eq!(idle_age_bucket(None), IdleAgeBucket::Stale);
    }

    #[test]
    fn bucket_boundaries() {
        let cases = [
            (Duration::ZERO, IdleAgeBucket::Afterglow),
            (IDLE_AFTERGLOW_WINDOW - Duration::from_secs(1), IdleAgeBucket::Afterglow),
            // Boundary is exclusive on the young side: exactly 2 min = settled.
            (IDLE_AFTERGLOW_WINDOW, IdleAgeBucket::Settled),
            (IDLE_STALE_THRESHOLD - Duration::from_secs(1), IdleAgeBucket::Settled),
            // Exactly 30 min = stale.
            (IDLE_STALE_THRESHOLD, IdleAgeBucket::Stale),
            (Duration::from_secs(24 * 3600), IdleAgeBucket::Stale),
        ];
        for (age, want) in cases {
            assert_eq!(
                idle_age_bucket(Some(age)),
                want,
                "idle age {:?} must classify as {:?}",
                age,
                want,
            );
        }
    }

    #[test]
    fn future_idle_since_saturates_to_afterglow() {
        // A (clock-skew-ish) idle_since in the future saturates to zero age
        // rather than panicking or wrapping to a huge duration.
        let now = Instant::now();
        let future = now + Duration::from_secs(60);
        assert_eq!(
            idle_age_bucket_at(Some(future), now),
            IdleAgeBucket::Afterglow,
        );
    }

    // ── next_attention_index: the A-g candidate picker ──
    // Row tuples are (has_alert, is_idle, hidden) in sidebar visual order.

    const RUNNING: (bool, bool, bool) = (false, false, false);
    const IDLE: (bool, bool, bool) = (false, true, false);
    const IDLE_HIDDEN: (bool, bool, bool) = (false, true, true);
    const ALERT: (bool, bool, bool) = (true, false, false);
    const ALERT_HIDDEN: (bool, bool, bool) = (true, true, true);

    #[test]
    fn no_rows_or_no_candidates_yields_none() {
        assert_eq!(next_attention_index(&[], None), None);
        // Running rows and hidden alert-less idle rows are not candidates.
        assert_eq!(
            next_attention_index(&[RUNNING, IDLE_HIDDEN, RUNNING], Some(0)),
            None,
        );
    }

    #[test]
    fn alerts_win_over_idles_regardless_of_visual_position() {
        // Visual order: idle at 0, alert at 2. First jump must go to the
        // ALERT (priority bucket 1) even though the idle row is higher up.
        let rows = [IDLE, RUNNING, ALERT];
        assert_eq!(next_attention_index(&rows, None), Some(2));
        // Cursor parked on a non-candidate row also lands on the alert.
        assert_eq!(next_attention_index(&rows, Some(1)), Some(2));
    }

    #[test]
    fn cycles_through_priority_ring_and_wraps() {
        // Candidate ring: alerts in visual order (2), then idles (0, 3).
        let rows = [IDLE, RUNNING, ALERT, IDLE];
        assert_eq!(next_attention_index(&rows, Some(2)), Some(0));
        assert_eq!(next_attention_index(&rows, Some(0)), Some(3));
        // Wraps back to the head of the ring.
        assert_eq!(next_attention_index(&rows, Some(3)), Some(2));
    }

    #[test]
    fn hidden_is_skipped_unless_alerting() {
        // A hidden idle session is not a candidate, but an alert overrides
        // hidden (matching the sidebar indicator override).
        let rows = [IDLE_HIDDEN, ALERT_HIDDEN, IDLE];
        assert_eq!(next_attention_index(&rows, None), Some(1));
        assert_eq!(next_attention_index(&rows, Some(1)), Some(2));
        // The hidden idle row (0) is never visited: 2 → back to 1.
        assert_eq!(next_attention_index(&rows, Some(2)), Some(1));
    }

    #[test]
    fn single_candidate_self_cycles() {
        let rows = [RUNNING, IDLE];
        assert_eq!(next_attention_index(&rows, Some(1)), Some(1));
    }
}

#[cfg(test)]
mod pinned_sort_tests {
    use super::*;

    fn bare_ws(id: &str, pinned: bool) -> Workspace {
        Workspace {
            id: id.to_string(),
            name: id.to_string(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: cm_daemon::host_id::HostId::local(),
            color: None,
            pinned,
            sessions: vec![],
            tombstones: vec![],
            is_pushing: false,
        }
    }

    #[test]
    fn resort_floats_pinned_stably_and_preserves_cursor() {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let mut app = App::new(crate::config::Config {
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

        app.workspaces = vec![
            bare_ws("a", false),
            bare_ws("b", true),
            bare_ws("c", false),
            bare_ws("d", true),
        ];
        // Cursor on "a" — after the pinned float it should follow "a"
        // to its new index rather than staying on index 0 ("b").
        app.cursor = Cursor::Workspace(0);

        app.resort_workspaces_for_pin();

        let order: Vec<&str> =
            app.workspaces.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            order,
            ["b", "d", "a", "c"],
            "pinned float to the top; stable order within each group"
        );
        assert_eq!(
            app.cursor,
            Cursor::Workspace(2),
            "cursor must follow workspace 'a' across the reorder"
        );
    }
}

#[cfg(test)]
mod nav_quickswitch_tests {
    //! Feature tests for the three Sessions-view navigation additions:
    //! A-; MRU quick-switch (deque + walk semantics), the A-p fuzzy-find
    //! palette (pure match/rank + handler), and the A-i peek (pure line
    //! assembly + scroll clamping). All pure logic / free handlers — no
    //! App, no PTY.
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState};

    fn key(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    fn key_mod(code: KeyCode, mods: KeyModifiers) -> CrosstermEvent {
        CrosstermEvent::Key(KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        })
    }

    fn ctx<'a>() -> InputCtx<'a> {
        InputCtx { repo_urls: &[], host_ids: &[] }
    }

    // ── palette_match_indices ─────────────────────────────────────

    #[test]
    fn palette_empty_query_returns_all_in_order() {
        let displays = ["alpha", "beta", "gamma"];
        assert_eq!(palette_match_indices("", &displays), vec![0, 1, 2]);
    }

    #[test]
    fn palette_prefix_ranks_before_substring() {
        // "ba" is a prefix of "backend" (idx 2) and a substring of
        // "alpha-bar" (idx 0) and "rebase" (idx 3); "zzz" matches nothing.
        let displays = ["alpha-bar", "zzz", "backend", "rebase"];
        assert_eq!(palette_match_indices("ba", &displays), vec![2, 0, 3]);
    }

    #[test]
    fn palette_match_is_case_insensitive() {
        let displays = ["Fix Login", "deploy", "LOGIN page"];
        assert_eq!(palette_match_indices("login", &displays), vec![2, 0]);
    }

    #[test]
    fn palette_no_match_returns_empty() {
        let displays = ["one", "two"];
        assert!(palette_match_indices("xyz", &displays).is_empty());
    }

    #[test]
    fn palette_groups_preserve_sidebar_order() {
        // Two prefix matches keep their relative order; likewise substrings.
        let displays = ["ab-1", "xab", "ab-2", "yab"];
        assert_eq!(palette_match_indices("ab", &displays), vec![0, 2, 1, 3]);
    }

    // ── mru_record ────────────────────────────────────────────────

    #[test]
    fn mru_record_pushes_front_dedups_and_caps() {
        let mut mru: VecDeque<String> = VecDeque::new();
        mru_record(&mut mru, "a", 3);
        mru_record(&mut mru, "b", 3);
        mru_record(&mut mru, "c", 3);
        assert_eq!(mru, ["c", "b", "a"]);
        // Re-recording an existing uid moves it to the front (no dupes).
        mru_record(&mut mru, "a", 3);
        assert_eq!(mru, ["a", "c", "b"]);
        // Cap evicts the oldest.
        mru_record(&mut mru, "d", 3);
        assert_eq!(mru, ["d", "a", "c"]);
    }

    // ── mru_next_walk_target ──────────────────────────────────────

    fn uids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn walk_first_press_picks_most_recent_other() {
        // Ring: [current A, then MRU deque B, C]. First press → B.
        let list = uids(&["A", "B", "C"]);
        let got = mru_next_walk_target(&list, 0, Some("A"), |_| true);
        assert_eq!(got, Some(1));
    }

    #[test]
    fn walk_advances_deeper_on_repeat_and_wraps_through_start() {
        let list = uids(&["A", "B", "C"]);
        // After the first hop (pos 1, focused B), the next press goes
        // deeper to C — not back to A (classic alt-tab).
        assert_eq!(mru_next_walk_target(&list, 1, Some("B"), |_| true), Some(2));
        // And from C the walk wraps back through the start A.
        assert_eq!(mru_next_walk_target(&list, 2, Some("C"), |_| true), Some(0));
    }

    #[test]
    fn walk_skips_unresolvable_uids() {
        let list = uids(&["A", "dead", "C"]);
        let got = mru_next_walk_target(&list, 0, Some("A"), |u| u != "dead");
        assert_eq!(got, Some(2));
    }

    #[test]
    fn walk_skips_sentinel_start_slot() {
        // Walk started on a non-session row: slot 0 is the empty sentinel,
        // which never resolves — first press lands on the deque head.
        let list = uids(&["", "B", "C"]);
        let got = mru_next_walk_target(&list, 0, None, |u| !u.is_empty());
        assert_eq!(got, Some(1));
        // Wrapping from the last entry skips the sentinel too.
        let got = mru_next_walk_target(&list, 2, Some("C"), |u| !u.is_empty());
        assert_eq!(got, Some(1));
    }

    #[test]
    fn walk_none_when_nothing_resolves() {
        let list = uids(&["A", "B"]);
        assert_eq!(mru_next_walk_target(&list, 0, Some("A"), |_| false), None);
        assert_eq!(mru_next_walk_target(&[], 0, None, |_| true), None);
        // Only the current session in the ring → nowhere to go.
        let solo = uids(&["A"]);
        assert_eq!(mru_next_walk_target(&solo, 0, Some("A"), |_| true), None);
    }

    // ── peek line assembly ────────────────────────────────────────

    fn task_fixture(prompt: Option<&str>, parent: Option<&str>) -> TaskEntry {
        TaskEntry {
            task_id: Some("t1".into()),
            name: "Fix the frobnicator".into(),
            api_status: TaskStatus::Blocked,
            repo_url: None,
            prompt: prompt.map(str::to_string),
            wip_branch: Some("cm/frob".into()),
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            is_continuous: false,
            workspace_id: None,
            project: Some("claude-manager".into()),
            parent_task_id: parent.map(str::to_string),
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        }
    }

    #[test]
    fn peek_task_lines_has_title_status_and_full_prompt() {
        let task = task_fixture(Some("line one\nline two"), None);
        let lines = peek_task_lines(&task, None);
        assert_eq!(lines[0], PeekLine::Title("Fix the frobnicator".into()));
        assert_eq!(
            lines[1],
            PeekLine::Status { value: "blocked".into(), status: TaskStatus::Blocked }
        );
        // Full prompt payload, one Text line per prompt line.
        assert!(lines.contains(&PeekLine::Text("line one".into())));
        assert!(lines.contains(&PeekLine::Text("line two".into())));
        // Branch + project fields present.
        assert!(lines.contains(&PeekLine::Field {
            label: "Branch".into(),
            value: "cm/frob".into()
        }));
        assert!(lines.contains(&PeekLine::Field {
            label: "Project".into(),
            value: "claude-manager".into()
        }));
        // No parent → no Parent field.
        assert!(!lines
            .iter()
            .any(|l| matches!(l, PeekLine::Field { label, .. } if label == "Parent")));
    }

    #[test]
    fn peek_task_lines_parent_prefers_name_falls_back_to_id() {
        let task = task_fixture(None, Some("t0"));
        let with_name = peek_task_lines(&task, Some("Parent task"));
        assert!(with_name.contains(&PeekLine::Field {
            label: "Parent".into(),
            value: "Parent task".into()
        }));
        let without = peek_task_lines(&task, None);
        assert!(without.contains(&PeekLine::Field {
            label: "Parent".into(),
            value: "t0".into()
        }));
        // Missing prompt renders the placeholder rather than nothing.
        assert!(without.contains(&PeekLine::Text("(no prompt)".into())));
    }

    #[test]
    fn peek_workspace_lines_lists_identity_and_sessions() {
        let sessions = vec![
            ("worker".to_string(), "claude".to_string(), "running".to_string()),
            ("shell".to_string(), "bash".to_string(), "idle".to_string()),
        ];
        let lines = peek_workspace_lines(
            "my-ws",
            Some("/tmp/wt"),
            Some("/tmp/main"),
            Some("git@github.com:a/b.git"),
            "local",
            &sessions,
        );
        assert_eq!(lines[0], PeekLine::Title("my-ws".into()));
        assert!(lines.contains(&PeekLine::Field {
            label: "Worktree".into(),
            value: "/tmp/wt".into()
        }));
        assert!(lines.contains(&PeekLine::Field { label: "Host".into(), value: "local".into() }));
        assert!(lines.contains(&PeekLine::Text("worker (claude) \u{2014} running".into())));
        assert!(lines.contains(&PeekLine::Text("shell (bash) \u{2014} idle".into())));
        // Empty roster shows the placeholder.
        let empty = peek_workspace_lines("ws", None, None, None, "local", &[]);
        assert!(empty.contains(&PeekLine::Text("(none)".into())));
    }

    #[test]
    fn peek_session_lines_include_provenance_when_present() {
        let lines = peek_session_lines(
            "worker",
            "claude",
            "uid-1",
            Some("snap-a"),
            Some("do the thing"),
        );
        assert!(lines.contains(&PeekLine::Field {
            label: "Session".into(),
            value: "worker (claude)".into()
        }));
        assert!(lines.contains(&PeekLine::Field {
            label: "Seeded from".into(),
            value: "snap-a".into()
        }));
        assert!(lines.contains(&PeekLine::Field {
            label: "Last prompt".into(),
            value: "do the thing".into()
        }));
        // Absent provenance → the fields are omitted entirely.
        let bare = peek_session_lines("worker", "claude", "uid-1", None, None);
        assert_eq!(bare.len(), 2);
    }

    // ── handle_session_palette ────────────────────────────────────

    fn palette_candidates() -> Vec<PaletteCandidate> {
        vec![
            PaletteCandidate {
                target: PaletteTarget::Workspace { ws_id: "ws-1".into() },
                display: "alpha".into(),
            },
            PaletteCandidate {
                target: PaletteTarget::Session { uid: "u-beta".into() },
                display: "alpha / beta".into(),
            },
            PaletteCandidate {
                target: PaletteTarget::Session { uid: "u-gamma".into() },
                display: "alpha / gamma".into(),
            },
        ]
    }

    #[test]
    fn palette_typing_edits_query_and_resets_selection() {
        let cands = palette_candidates();
        let mut query = String::new();
        let mut selected = 2usize;
        // Plain 'g' types into the query (it must NOT move the selection).
        let o = handle_session_palette(
            &cands,
            &mut query,
            &mut selected,
            ctx(),
            &key(KeyCode::Char('g')),
        );
        assert!(matches!(o, InputOutcome::Consumed));
        assert_eq!(query, "g");
        assert_eq!(selected, 0);
        // Backspace pops.
        handle_session_palette(
            &cands,
            &mut query,
            &mut selected,
            ctx(),
            &key(KeyCode::Backspace),
        );
        assert_eq!(query, "");
    }

    #[test]
    fn palette_enter_submits_selected_filtered_row() {
        let cands = palette_candidates();
        let mut query = "gamma".to_string();
        let mut selected = 0usize;
        let o = handle_session_palette(
            &cands,
            &mut query,
            &mut selected,
            ctx(),
            &key(KeyCode::Enter),
        );
        match o {
            InputOutcome::Submit(SubmitAction::PaletteJump {
                target: PaletteTarget::Session { uid },
            }) => assert_eq!(uid, "u-gamma"),
            other => panic!("expected PaletteJump for u-gamma, got {:?}", other),
        }
    }

    #[test]
    fn palette_selection_moves_on_down_tab_not_plain_j() {
        let cands = palette_candidates();
        let mut query = String::new();
        let mut selected = 0usize;
        handle_session_palette(&cands, &mut query, &mut selected, ctx(), &key(KeyCode::Down));
        assert_eq!(selected, 1);
        handle_session_palette(&cands, &mut query, &mut selected, ctx(), &key(KeyCode::Tab));
        assert_eq!(selected, 2);
        // Wraps.
        handle_session_palette(&cands, &mut query, &mut selected, ctx(), &key(KeyCode::Down));
        assert_eq!(selected, 0);
        // Ctrl-k moves up (wrapping); plain 'j' types instead.
        handle_session_palette(
            &cands,
            &mut query,
            &mut selected,
            ctx(),
            &key_mod(KeyCode::Char('k'), KeyModifiers::CONTROL),
        );
        assert_eq!(selected, 2);
        assert_eq!(query, "");
        handle_session_palette(&cands, &mut query, &mut selected, ctx(), &key(KeyCode::Char('j')));
        assert_eq!(query, "j");
    }

    #[test]
    fn palette_esc_and_alt_p_cancel_enter_on_no_match_cancels() {
        let cands = palette_candidates();
        let mut query = String::new();
        let mut selected = 0usize;
        let o = handle_session_palette(&cands, &mut query, &mut selected, ctx(), &key(KeyCode::Esc));
        assert!(matches!(o, InputOutcome::Cancel));
        let o = handle_session_palette(
            &cands,
            &mut query,
            &mut selected,
            ctx(),
            &key_mod(KeyCode::Char('p'), KeyModifiers::ALT),
        );
        assert!(matches!(o, InputOutcome::Cancel));
        let mut no_match = "zzzz".to_string();
        let o = handle_session_palette(
            &cands,
            &mut no_match,
            &mut selected,
            ctx(),
            &key(KeyCode::Enter),
        );
        assert!(matches!(o, InputOutcome::Cancel));
    }

    // ── handle_task_peek ──────────────────────────────────────────

    #[test]
    fn task_peek_scroll_clamps_both_ends() {
        let mut scroll = 0u16;
        // Down past the max clamps at max.
        for _ in 0..5 {
            handle_task_peek(&mut scroll, 3, &key(KeyCode::Char('j')));
        }
        assert_eq!(scroll, 3);
        handle_task_peek(&mut scroll, 3, &key(KeyCode::PageDown));
        assert_eq!(scroll, 3);
        // Up past zero saturates.
        handle_task_peek(&mut scroll, 3, &key(KeyCode::PageUp));
        assert_eq!(scroll, 0);
        handle_task_peek(&mut scroll, 3, &key(KeyCode::Char('k')));
        assert_eq!(scroll, 0);
        // PgDn respects the max mid-flight.
        handle_task_peek(&mut scroll, 25, &key(KeyCode::PageDown));
        assert_eq!(scroll, 10);
    }

    #[test]
    fn task_peek_esc_and_alt_i_close() {
        let mut scroll = 0u16;
        let o = handle_task_peek(&mut scroll, 0, &key(KeyCode::Esc));
        assert!(matches!(o, InputOutcome::Cancel));
        let o = handle_task_peek(&mut scroll, 0, &key_mod(KeyCode::Char('i'), KeyModifiers::ALT));
        assert!(matches!(o, InputOutcome::Cancel));
        let o = handle_task_peek(&mut scroll, 0, &key(KeyCode::Char('q')));
        assert!(matches!(o, InputOutcome::Cancel));
    }
}
