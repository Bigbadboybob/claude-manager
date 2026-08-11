use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use alacritty_terminal::event::Event as TermEvent;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::api::Task;
use crate::session::Session;
use crate::terminal_widget::TerminalWidget;
use crate::theme;

// ── Data Types ──────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlanStatus {
    Done,
    InProgress,
    Backlog,
    Draft,
    Archived,
}

impl PlanStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "in_progress" | "running" => Self::InProgress,
            "backlog" | "blocked" => Self::Backlog,
            "archived" => Self::Archived,
            _ => Self::Draft,
        }
    }
    fn as_str(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::InProgress => "running",
            Self::Backlog => "backlog",
            Self::Draft => "draft",
            Self::Archived => "archived",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::InProgress => "in progress",
            Self::Backlog => "backlog",
            Self::Draft => "draft",
            Self::Archived => "archived",
        }
    }
    fn next(&self) -> Self {
        // Archived is reachable only via the bulk-archive shortcut, never via
        // normal alt+s cycling. Both ends of the cycle stay put.
        match self {
            Self::Draft => Self::Backlog,
            Self::Backlog => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Done,
            Self::Archived => Self::Archived,
        }
    }
    fn prev(&self) -> Self {
        match self {
            Self::Done => Self::InProgress,
            Self::InProgress => Self::Backlog,
            Self::Backlog => Self::Draft,
            Self::Draft => Self::Draft,
            Self::Archived => Self::Archived,
        }
    }
}

#[derive(Clone)]
pub struct PlanTask {
    pub id: String,
    pub slug: String,
    pub title: String,
    pub status: PlanStatus,
    pub difficulty: Option<u8>,
    pub depends: Vec<String>,
    pub branch: Option<String>,
    pub created: Option<String>,
    pub description: String,
    pub prompt: String,
    pub source: String,
    pub is_cloud: bool,
    pub repo_url: String,
    /// Sub-2a Finding #2: pinned at the planning row so launch
    /// actions can surface the parent edge in `LaunchTask` /
    /// `LaunchTaskIntoWorkspace`. Without it the launch site
    /// would have to look up the parent off the API row again
    /// — which is exactly the race the finding describes (local
    /// stub init happens BEFORE the next API reconcile).
    pub parent_task_id: Option<String>,
    /// Task kind from the API row ("oneshot" | "continuous" | "backtest").
    /// The `A-w` watch action fires only on `"backtest"`.
    pub kind: String,
    /// Worker VM name, once the backtest has been dispatched (`None` while
    /// still queued). Mirrors the API `worker_vm` column.
    pub worker_vm: Option<String>,
    /// GCP project the worker VM lives in, from `metadata.vm.project`.
    /// Backtest VMs run in a DIFFERENT project than the CM default
    /// (`prediction-market-scalper`, not `claude-manager-prod`), so the
    /// watch attach MUST use this rather than the config default.
    pub vm_project: Option<String>,
    /// GCP zone of the worker VM, from `metadata.vm.zone` (falls back to
    /// the API `worker_zone` column).
    pub vm_zone: Option<String>,
    /// Backtest run key (`metadata.backtest.run_key`) — a stable
    /// human-readable id used to title the watch session.
    pub run_key: Option<String>,
    /// Backtest label (`metadata.backtest.label`) — preferred over
    /// `run_key` for the watch session title when present.
    pub bt_label: Option<String>,
}

/// Pull a `"vm"` / `"backtest"` string field out of a task's free-form
/// `metadata` JSONB bag. Returns `None` for absent / non-string values.
fn meta_str(metadata: &Option<serde_json::Value>, group: &str, key: &str) -> Option<String> {
    metadata
        .as_ref()
        .and_then(|m| m.get(group))
        .and_then(|g| g.get(key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

impl PlanTask {
    fn from_api(task: &Task) -> Self {
        PlanTask {
            id: task.id.clone(),
            slug: task.slug.clone().unwrap_or_else(|| task.id[..8].to_string()),
            title: task.name.clone().unwrap_or_else(|| {
                task.slug.clone().unwrap_or_else(|| "untitled".to_string())
            }),
            status: PlanStatus::from_str(&task.status),
            difficulty: task.difficulty.map(|d| d as u8),
            depends: task.depends.clone().unwrap_or_default(),
            branch: Some(task.repo_branch.clone()),
            created: Some(task.created_at.clone()),
            description: task.description.clone().unwrap_or_default(),
            prompt: task.prompt.clone().unwrap_or_default(),
            source: task.source.clone(),
            is_cloud: task.is_cloud,
            repo_url: task.repo_url.clone(),
            parent_task_id: task.parent_task_id.clone(),
            kind: task.kind.clone(),
            worker_vm: task.worker_vm.clone().filter(|s| !s.is_empty()),
            vm_project: meta_str(&task.metadata, "vm", "project"),
            vm_zone: meta_str(&task.metadata, "vm", "zone")
                .or_else(|| task.worker_zone.clone().filter(|s| !s.is_empty())),
            run_key: meta_str(&task.metadata, "backtest", "run_key"),
            bt_label: meta_str(&task.metadata, "backtest", "label"),
        }
    }
}

#[derive(Clone)]
pub struct PlanProject {
    pub name: String,
    pub path: PathBuf,
    /// Persisted by `create_project` to `<path>/repo_url`. Empty when the
    /// project predates that file or was hydrated purely from API tasks
    /// without a backing local directory. `create_task` prefers this
    /// over `repo_url_for_project` so non-github / forked / renamed
    /// remotes don't get silently rewritten to the github default.
    pub repo_url: String,
}

#[derive(Clone, Debug, PartialEq)]
enum GridItem {
    Task(String),
    Separator,
    Empty,
    Header(String),
}

#[derive(Clone, Debug, Default)]
struct GridLayout {
    columns: Vec<Vec<GridItem>>,
}

/// A row in a column's tree-aware visible projection. Both cursor
/// (`cursor.row` indexes a `Vec<VisibleRow>`) and rendering work in
/// this space, not in raw `GridLayout`. Synthetic `Subtask` rows are
/// rebuilt each frame from `parent_task_id` + `expanded_tasks`.
#[derive(Clone, Debug)]
struct VisibleRow {
    kind: VisibleRowKind,
    /// 0 = top-level row from raw column (or a task whose parent
    /// isn't in this project). 1+ = a subtask under an expanded parent.
    depth: u8,
    /// True when the underlying task has at least one in-project
    /// child. Drives the ▶/▼ fold glyph.
    has_children: bool,
    /// `expanded_tasks` membership at compute time.
    expanded: bool,
    /// Transitive descendant count, for the "(N)" badge on collapsed
    /// parents. 0 when `has_children` is false.
    descendant_count: u32,
}

#[derive(Clone, Debug)]
enum VisibleRowKind {
    /// An entry that exists in the persisted raw `GridLayout`.
    /// `raw_idx` allows raw-layout mutations (reorder, move, insert)
    /// to look up the original position.
    Layout { raw_idx: usize, item: GridItem },
    /// A synthetic row for a parented subtask. Doesn't exist in
    /// `pd.layout.columns[ci]` — its persistence is the
    /// `parent_task_id` field on the API row.
    Subtask { slug: String },
}

/// Slug at a visible row, regardless of layout vs. synthetic origin.
/// Returns `None` for non-task layout items (Separator/Empty/Header).
/// Truncate `s` to at most `max` display bytes with a trailing "...",
/// cutting on a char boundary so multibyte chars (e.g. '≤') never panic.
pub(crate) fn truncate_with_ellipsis(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let take = max.saturating_sub(3);
    let truncated: String = s
        .char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= take)
        .map(|(_, c)| c)
        .collect();
    format!("{}...", truncated)
}

fn visible_row_slug(row: &VisibleRow) -> Option<&str> {
    match &row.kind {
        VisibleRowKind::Layout { item: GridItem::Task(slug), .. } => Some(slug.as_str()),
        VisibleRowKind::Subtask { slug } => Some(slug.as_str()),
        _ => None,
    }
}

/// Case-insensitive substring match over the fields `/` search covers:
/// title + description.
fn task_matches_query(task: &PlanTask, query_lower: &str) -> bool {
    !query_lower.is_empty()
        && (task.title.to_lowercase().contains(query_lower)
            || task.description.to_lowercase().contains(query_lower))
}

/// Wrap-around step through a match list. `current` is the cursor's
/// position within the list when it already sits on a match; `None`
/// (cursor elsewhere) enters the list at the first match going forward
/// and the last going backward. `None` result = empty list.
fn next_search_index(len: usize, current: Option<usize>, direction: i32) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        Some(cur) => (cur as i32 + direction).rem_euclid(len as i32) as usize,
        None if direction >= 0 => 0,
        None => len - 1,
    })
}

/// Byte ranges of every case-insensitive occurrence of `needle_lower`
/// (pre-lowercased, non-empty) in `haystack`. Lowercasing is done per
/// char with an offset map back to the original string, so multibyte
/// and case-expanding chars can't produce out-of-bounds or mid-char
/// ranges — degenerate (empty) ranges are dropped instead.
fn find_ci_ranges(haystack: &str, needle_lower: &str) -> Vec<(usize, usize)> {
    if needle_lower.is_empty() {
        return vec![];
    }
    let mut lowered = String::with_capacity(haystack.len());
    // For each byte of `lowered`, the original byte offset its source
    // char started at. One extra sentinel entry maps end→end.
    let mut back: Vec<usize> = Vec::with_capacity(haystack.len() + 1);
    for (oi, ch) in haystack.char_indices() {
        for lch in ch.to_lowercase() {
            let before = lowered.len();
            lowered.push(lch);
            for _ in before..lowered.len() {
                back.push(oi);
            }
        }
    }
    back.push(haystack.len());
    let mut ranges = Vec::new();
    let mut from = 0usize;
    while let Some(pos) = lowered[from..].find(needle_lower) {
        let ls = from + pos;
        let le = ls + needle_lower.len();
        let os = back[ls];
        // End maps to the start of the char AFTER the match; when the
        // match ends mid-expansion this still lands on a boundary of
        // the original string because `back` repeats the source offset.
        let oe = if le < back.len() { back[le] } else { haystack.len() };
        if oe > os {
            ranges.push((os, oe));
        }
        from = le;
    }
    ranges
}

/// One-shot env gate for the grid `[debug]` footer line. Development
/// aid only: set `CM_PLANNING_DEBUG=1` to get it back.
fn planning_debug_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("CM_PLANNING_DEBUG").map(|v| v == "1").unwrap_or(false))
}

#[derive(Clone, Debug)]
struct GridCursor {
    col: usize,
    /// Index into the column's `Vec<VisibleRow>` (NOT raw layout).
    /// When no in-project parenting exists, visible rows ≈ raw items
    /// and this is identity with the old semantics.
    row: usize,
}

struct ProjectData {
    project: PlanProject,
    tasks: Vec<PlanTask>,
    layout: GridLayout,
}

#[derive(Clone, Copy, PartialEq)]
enum NewProjectField { Name, RepoUrl }

/// Agent engine chosen in the planning launch dialogs (`A-l` / `A-f`).
/// Claude is the default; `←/→` cycles. Only the two agent engines are
/// offered — a planning launch always delivers the task prompt to an
/// agent, so `bash` (the third session type elsewhere in the TUI) has
/// no meaning here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LaunchEngine {
    Claude,
    Codex,
}

impl LaunchEngine {
    /// Internal TUI session-type vocabulary (`"claude"` / `"codex"`) —
    /// what `try_spawn_via_daemon` and `TerminalSession::session_type`
    /// expect. NOT the public MCP wire form (`"claude-code"`).
    pub fn as_session_type(self) -> &'static str {
        match self {
            LaunchEngine::Claude => "claude",
            LaunchEngine::Codex => "codex",
        }
    }

    fn cycle(self) -> Self {
        match self {
            LaunchEngine::Claude => LaunchEngine::Codex,
            LaunchEngine::Codex => LaunchEngine::Claude,
        }
    }
}

impl Default for LaunchEngine {
    fn default() -> Self {
        LaunchEngine::Claude
    }
}

/// The `Engine: [claude]  codex` row shared by both launch dialogs.
/// Selected option is bracketed + bold so it reads at a glance in the
/// same style as the other in-place cyclers (A-e color pickers).
fn engine_line(engine: LaunchEngine) -> Line<'static> {
    let dim = Style::default().fg(theme::DIM);
    let mut spans = vec![Span::styled("  Engine: ", dim)];
    for (i, opt) in [LaunchEngine::Claude, LaunchEngine::Codex].iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ", dim));
        }
        let is_sel = *opt == engine;
        let label = if is_sel {
            format!("[{}]", opt.as_session_type())
        } else {
            format!(" {} ", opt.as_session_type())
        };
        let style = if is_sel {
            Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::MUTED)
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

enum PlanInputMode {
    Normal,
    Editing,
    Searching { query: String },
    /// Top-level task creation (`A-n`) leaves `parent_task_id = None`.
    /// Subtask creation (`A-S`) sets it to the focused task's id; the
    /// resulting `CreateTask` action threads it through so the API row
    /// is persisted with the parent link set. The same input form
    /// renders both modes; the overlay just adds a "Subtask of: …"
    /// header when the parent is present.
    NewTask {
        title: String,
        parent_task_id: Option<String>,
        parent_name: Option<String>,
    },
    NewHeader { text: String },
    EditingHeader { text: String },
    BulkArchiveConfirm { project_idx: usize, count: usize },
    NewProject { name: String, repo_url: String, field: NewProjectField },
    ProjectPicker { selected: usize },
    /// Before LaunchConfirm: pick either "new workspace" or an existing one.
    /// Selected is an index into [NewWorkspace, ...workspace_candidates].
    /// `engine` is the agent to spawn (←/→ cycles); it rides through to
    /// LaunchConfirm when "New workspace" is picked, so the choice is made
    /// once regardless of which of the two launch routes is taken.
    WorkspacePicker { project_idx: usize, task_idx: usize, selected: usize, engine: LaunchEngine },
    LaunchConfirm { project_idx: usize, task_idx: usize, branch_text: String, engine: LaunchEngine },
}

/// An open workspace the planning view can offer as a launch target.
/// Populated by the App from its current workspace list each event cycle.
#[derive(Clone, Debug)]
pub struct WorkspaceCandidate {
    pub workspace_id: String,
    pub name: String,
    pub repo_url: Option<String>,
}

/// Reduce a repo URL to a comparable form so SSH and HTTPS pointers to the
/// same repo match. `git@github.com:org/repo.git` and
/// `https://github.com/org/repo.git` both collapse to `github.com/org/repo`.
/// Compose the activation text delivered to a worker agent on planning
/// launch. The agent only sees ONE string at session-spawn time, but
/// tasks have two natural fields: `description` (background/motivation
/// for the user reading the planning queue) and `prompt` (instructions
/// for the agent). We combine both so the agent always has the WHY plus
/// the HOW, separated by a clear delimiter so a smart worker can ignore
/// the background if it's irrelevant.
///
/// Precedence:
///   - Both present: "{description}\n\n---\n\n{prompt}"
///   - Only prompt: prompt verbatim (most common today; older tasks)
///   - Only description: description verbatim (description-only tasks
///     are rare but the agent should still get something useful)
///   - Neither: title fallback (prevents the empty-prompt class of
///     bug where the worker spawns with nothing to do)
///
/// Whitespace-only fields are treated as empty.
pub(crate) fn compose_launch_prompt(
    description: &str,
    prompt: &str,
    title: &str,
) -> String {
    let description = description.trim();
    let prompt = prompt.trim();
    if !description.is_empty() && !prompt.is_empty() {
        format!("{}\n\n---\n\n{}", description, prompt)
    } else if !prompt.is_empty() {
        prompt.to_string()
    } else if !description.is_empty() {
        description.to_string()
    } else {
        title.to_string()
    }
}

/// Used when matching task→workspace at launch time, where the task's URL
/// often comes from `git remote get-url origin` (SSH) and the workspace's
/// from `~/.cm/projects/*/repo_url` (HTTPS).
fn normalize_repo_url(url: &str) -> String {
    let s = url.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .or_else(|| s.strip_prefix("ssh://"))
        .or_else(|| s.strip_prefix("git@"))
        .unwrap_or(s);
    // SSH form `host:org/repo` — turn the first ':' into '/' so it lines up
    // with HTTPS form `host/org/repo`.
    let s = s.replacen(':', "/", 1);
    let s = s.strip_suffix(".git").unwrap_or(&s).to_string();
    let s = s.trim_end_matches('/').to_string();
    s.to_lowercase()
}

pub enum PlanAction {
    Consumed,
    Ignored,
    LaunchTask {
        project: String,
        slug: String,
        prompt: String,
        branch: Option<String>,
        autostart: bool,
        task_id: String,
        /// Sub-2a Finding #2: subtask launches must surface
        /// `parent_task_id` from the planning row so the
        /// launch site can initialize the local TaskEntry
        /// stub with the correct edge. Pre-fix the stub set
        /// `parent_task_id: None` and `push_task_tree_to_daemon`
        /// published the wrong tree until the next API
        /// reconcile patched it — opening a window where the
        /// daemon saw a subtask as top-level and the
        /// descendant-task auth walk could not authorize a
        /// parent → subtask action.
        parent_task_id: Option<String>,
        /// `true` when the branch field held the `.` sentinel: launch
        /// in-place in the main repo (no worktree, no branch).
        in_place: bool,
        /// Agent to spawn into the new worktree (launch-dialog choice,
        /// Claude by default).
        engine: LaunchEngine,
    },
    /// Bind a task to an existing workspace and spawn a session there
    /// (no new worktree, no branch input).
    LaunchTaskIntoWorkspace {
        workspace_id: String,
        task_id: String,
        task_title: String,
        task_repo_url: String,
        /// Project name from the planning row. Pinned at action time so
        /// the launch site can persist it on the local TaskEntry stub
        /// even when the API row hasn't been reconciled yet — fixes
        /// the disappearing-subtask race when an agent calls
        /// `create_subtask` before reconcile backfills `project`.
        project: String,
        prompt: String,
        /// Sub-2a Finding #2: same backstory as LaunchTask. The
        /// "into existing workspace" variant carries it too so
        /// the stub at the call site lands with the correct
        /// parent edge on first push.
        parent_task_id: Option<String>,
        /// Agent to spawn into the existing workspace (picker
        /// choice, Claude by default).
        engine: LaunchEngine,
    },
    /// Clear a task's `workspace_id`. Task status is not affected.
    UnbindTask {
        task_id: String,
    },
    /// Send a running task back to backlog: PATCH status, clear workspace_id
    /// on the local TaskEntry, and close the bound workspace. Worktree and
    /// branch are left on disk.
    UnlaunchTask {
        task_id: String,
    },
    /// Reopen a done task: validate the worktree is still on disk, PATCH the
    /// task back to `running`, un-archive the workspace, and switch to the
    /// Sessions view. Gracefully fails when the worktree has been removed.
    ReopenTask {
        task_id: String,
    },
    SwitchToSessions,
    Quit,
    CreateTask {
        project: String,
        repo_url: String,
        name: String,
        description: String,
        status: String,
        /// `Some(parent_id)` → this task is a subtask, persisted with
        /// `parent_task_id` set on the API row. `None` → top-level
        /// (the existing `A-n` flow).
        parent_task_id: Option<String>,
        /// Worktree mode for subtasks (`inherit` | `branch`). Ignored
        /// when `parent_task_id` is `None`. Defaults to `inherit` so
        /// the subtask shares the parent's workspace once launched.
        worktree_mode: Option<String>,
    },
    UpdateTask {
        id: String,
        fields: HashMap<String, serde_json::Value>,
        /// Optional toast to surface alongside the PATCH dispatch — used
        /// when editor save partially succeeded (e.g. parent slug
        /// resolution failed; other fields still go out).
        status_msg: Option<String>,
    },
    /// Apply the same field update to a list of tasks. Used by bulk-archive.
    /// The app loop iterates and PATCHes each id with `fields.clone()`.
    BulkUpdateTasks {
        ids: Vec<String>,
        fields: HashMap<String, serde_json::Value>,
    },
    DeleteTask {
        id: String,
    },
    /// Attach a live, READ-ONLY terminal view to a cloud backtest's
    /// worker tmux. Emitted by `A-w` on a focused task. The app-side
    /// handler validates (`kind`, `worker_vm`), builds the `gcloud
    /// compute ssh … -t "sudo tmux attach -r -t backtest"` command,
    /// spawns a local session bound to the task's workspace, and
    /// switches to the Sessions view. All fields are snapshotted from
    /// the planning row so the handler needs no re-lookup.
    WatchBacktest {
        task_id: String,
        /// Task kind — the handler messages (not spawns) when this
        /// isn't `"backtest"`, keeping `A-w` discoverable on any row.
        kind: String,
        /// `None`/empty until the dispatcher assigns a VM — handler
        /// shows a "not dispatched yet" status instead of spawning.
        worker_vm: Option<String>,
        /// `metadata.vm.project` — the backtest VM's GCP project.
        vm_project: Option<String>,
        /// `metadata.vm.zone` (or the `worker_zone` column).
        vm_zone: Option<String>,
        /// Human-readable title for the session (label → run_key → vm).
        title: String,
    },
    RefreshTasks,
}

// ── Temp File Editing ──────────────────────────────────────

/// Frontmatter schema for the temp-file editor. `serde_yaml` handles
/// quoting/escaping, so titles with `:`, branches with `#`, depends
/// containing `,`, and other YAML-sensitive characters round-trip.
#[derive(serde::Serialize, serde::Deserialize)]
struct PlanTaskFrontmatter {
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    difficulty: Option<u8>,
    /// `Option<Vec<_>>` (not bare `Vec<_>`) because the user can clear
    /// dependencies in the editor by leaving `depends:` with no list
    /// items — that deserializes as `null`, which would fail to coerce
    /// into `Vec<String>` and make the whole parse return `None`,
    /// silently dropping every other edit on the same save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    depends: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// Parent task by slug (same project only). Same three-valued
    /// presence semantics as `depends`/`branch`: absent means no
    /// change, empty/null means clear, set means reparent. Slug → id
    /// resolution and cycle/cross-project validation happen at save
    /// time in `stop_editor`, since `build_patch_fields` doesn't have
    /// access to the task list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
}

/// Write a task to a temp file for editing, returns the temp path.
///
/// `parent_slug` should be the slug of the task's current parent (if any),
/// pre-resolved from the project's task list by the caller.
fn write_temp_task(task: &PlanTask, parent_slug: Option<&str>) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cm-planning");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.md", task.slug));

    let front = PlanTaskFrontmatter {
        title: task.title.clone(),
        status: Some(task.status.as_str().to_string()),
        difficulty: task.difficulty,
        depends: if task.depends.is_empty() {
            None
        } else {
            Some(task.depends.clone())
        },
        branch: task.branch.clone(),
        parent: parent_slug.map(|s| s.to_string()),
    };
    let yaml = serde_yaml::to_string(&front).ok()?;
    let yaml = yaml.trim_end_matches('\n');

    let body = if task.description.is_empty() && task.prompt.is_empty() {
        "## Description\n\n\n\n## Prompt\n".to_string()
    } else {
        let mut body = String::new();
        if !task.description.is_empty() {
            body.push_str(&task.description);
        } else {
            body.push_str("## Description\n");
        }
        if !task.prompt.is_empty() {
            if !body.contains("## Prompt") {
                body.push_str("\n\n## Prompt\n");
            }
            body.push_str(&task.prompt);
        }
        body
    };

    let content = format!("---\n{}\n---\n\n{}", yaml, body);
    std::fs::write(&path, content).ok()?;
    Some(path)
}

/// Per-field edit intent, three-valued so the PATCH builder can tell
/// "user didn't touch this" apart from "user explicitly cleared this".
/// Without this distinction a cleared field silently reverts to the
/// stored value on the next refresh.
#[derive(Debug, Clone, PartialEq)]
enum FieldUpdate<T> {
    /// Key absent from the YAML — omit from PATCH.
    Absent,
    /// Key present but empty/null/empty-string — PATCH null (or [] for lists).
    Cleared,
    /// Key present with a value — PATCH that value.
    Set(T),
}

/// Parse a temp task file back into field updates.
fn parse_temp_task(path: &Path) -> Option<TempTaskParsed> {
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    let after_first = &trimmed[3..];
    let end_idx = after_first.find("\n---")?;
    let yaml_str = &after_first[..end_idx];
    let body = after_first[end_idx + 4..].trim().to_string();

    let front: PlanTaskFrontmatter = serde_yaml::from_str(yaml_str).ok()?;
    // Parse a second time as an untyped mapping so we can detect key
    // presence — the typed view collapses "absent" and "cleared" into
    // the same Option::None for nullable scalars.
    let raw: serde_yaml::Value = serde_yaml::from_str(yaml_str).ok()?;
    let key_present = |name: &str| -> bool {
        raw.as_mapping()
            .map(|m| m.contains_key(serde_yaml::Value::String(name.to_string())))
            .unwrap_or(false)
    };

    let difficulty = if !key_present("difficulty") {
        FieldUpdate::Absent
    } else {
        match front.difficulty {
            Some(d) => FieldUpdate::Set(d),
            None => FieldUpdate::Cleared,
        }
    };
    let depends = if !key_present("depends") {
        FieldUpdate::Absent
    } else {
        match front.depends {
            Some(v) if !v.is_empty() => FieldUpdate::Set(v),
            _ => FieldUpdate::Cleared,
        }
    };
    let branch = if !key_present("branch") {
        FieldUpdate::Absent
    } else {
        match front.branch {
            Some(s) if !s.is_empty() => FieldUpdate::Set(s),
            _ => FieldUpdate::Cleared,
        }
    };
    let parent = if !key_present("parent") {
        FieldUpdate::Absent
    } else {
        match front.parent {
            Some(s) if !s.is_empty() => FieldUpdate::Set(s),
            _ => FieldUpdate::Cleared,
        }
    };

    // Extract prompt section from body.
    let mut in_prompt = false;
    let mut prompt_lines = vec![];
    let mut desc_lines = vec![];
    for line in body.lines() {
        if line.starts_with("## Prompt") {
            in_prompt = true;
            continue;
        }
        if in_prompt {
            if line.starts_with("## ") {
                in_prompt = false;
                desc_lines.push(line);
            } else {
                prompt_lines.push(line);
            }
        } else {
            desc_lines.push(line);
        }
    }

    Some(TempTaskParsed {
        title: front.title,
        status: front.status.unwrap_or_else(|| "draft".to_string()),
        difficulty,
        depends,
        branch,
        parent,
        description: desc_lines.join("\n").trim().to_string(),
        prompt: prompt_lines.join("\n").trim().to_string(),
    })
}

struct TempTaskParsed {
    title: String,
    status: String,
    difficulty: FieldUpdate<u8>,
    depends: FieldUpdate<Vec<String>>,
    branch: FieldUpdate<String>,
    /// Parent slug. Resolved to `parent_task_id` in `stop_editor`, which
    /// has access to the project's task list for slug→id lookup and
    /// cycle/scope validation.
    parent: FieldUpdate<String>,
    description: String,
    prompt: String,
}

/// Build the PATCH map sent to the API for an editor save. Cleared
/// nullable fields go out as JSON null; cleared `depends` goes out as
/// `[]`. Fields the user didn't touch (absent from YAML) are omitted.
/// The API end of the clear (F17) must accept null/[] for these keys.
fn build_patch_fields(parsed: &TempTaskParsed) -> HashMap<String, serde_json::Value> {
    let mut fields = HashMap::new();
    fields.insert("name".to_string(), serde_json::json!(parsed.title));
    fields.insert("status".to_string(), serde_json::json!(parsed.status));
    fields.insert(
        "description".to_string(),
        serde_json::json!(parsed.description),
    );
    fields.insert("prompt".to_string(), serde_json::json!(parsed.prompt));

    match &parsed.difficulty {
        FieldUpdate::Absent => {}
        FieldUpdate::Cleared => {
            fields.insert("difficulty".to_string(), serde_json::Value::Null);
        }
        FieldUpdate::Set(d) => {
            fields.insert("difficulty".to_string(), serde_json::json!(d));
        }
    }
    match &parsed.depends {
        FieldUpdate::Absent => {}
        FieldUpdate::Cleared => {
            fields.insert("depends".to_string(), serde_json::json!([] as [String; 0]));
        }
        FieldUpdate::Set(v) => {
            fields.insert("depends".to_string(), serde_json::json!(v));
        }
    }
    match &parsed.branch {
        FieldUpdate::Absent => {}
        FieldUpdate::Cleared => {
            fields.insert("repo_branch".to_string(), serde_json::Value::Null);
        }
        FieldUpdate::Set(b) => {
            fields.insert("repo_branch".to_string(), serde_json::json!(b));
        }
    }
    fields
}

/// Walk cap shared by `task_is_self_or_descendant_of` — the same value
/// is reused here so behaviour stays consistent across the auth check
/// and the planning editor's cycle-detection walk.
const MAX_PARENT_WALK: usize = 64;

/// Resolve a parsed `parent` slug to a `parent_task_id` PATCH entry,
/// applying same-project + cycle validation.
///
/// Returns:
/// - `Ok(None)` — caller should not include `parent_task_id` in the PATCH.
///   Used for `FieldUpdate::Absent` (user didn't touch the field).
/// - `Ok(Some(Value::Null))` — caller should PATCH `parent_task_id: null`.
///   Used for `FieldUpdate::Cleared`.
/// - `Ok(Some(Value::String(id)))` — caller should PATCH with the resolved id.
/// - `Err(msg)` — validation failed; caller should surface `msg` and
///   omit the parent change. Other PATCH fields can still be sent.
///
/// `tasks` must be the *current project's* task list. Slugs are resolved
/// within that list only; a cross-project parent is reported as
/// "unknown slug" since it isn't visible here.
fn resolve_parent_patch(
    tasks: &[PlanTask],
    current_task_id: &str,
    field: &FieldUpdate<String>,
) -> Result<Option<serde_json::Value>, String> {
    match field {
        FieldUpdate::Absent => Ok(None),
        FieldUpdate::Cleared => Ok(Some(serde_json::Value::Null)),
        FieldUpdate::Set(slug) => {
            let parent = tasks.iter().find(|t| t.slug == *slug).ok_or_else(|| {
                format!("parent: no task with slug '{}' in this project", slug)
            })?;
            if parent.id == current_task_id {
                return Err("parent: a task cannot be its own parent".to_string());
            }
            // Walk up from the candidate parent. If we ever hit the
            // current task, the candidate is a descendant of us and
            // accepting the edit would form a cycle.
            let mut cur = parent.parent_task_id.clone();
            for _ in 0..MAX_PARENT_WALK {
                let Some(id) = cur else { break };
                if id == current_task_id {
                    return Err(format!(
                        "parent: '{}' is a descendant of this task (would form a cycle)",
                        slug
                    ));
                }
                cur = tasks
                    .iter()
                    .find(|t| t.id == id)
                    .and_then(|t| t.parent_task_id.clone());
            }
            Ok(Some(serde_json::json!(parent.id)))
        }
    }
}

/// Promote an `Absent` parent edit to `Cleared` when the task currently
/// has a parent.
///
/// `write_temp_task` only emits a `parent: <slug>` line when the task has
/// a parent, so the only way an edit of a parented task reaches `Absent`
/// is for the user to delete that line — which we read as an explicit
/// "detach from parent". (Re-saving without touching it keeps the line, so
/// that path is `Set`, not `Absent`, and never trips this.) A task with no
/// parent stays `Absent` → no-op, so this never accidentally rewrites a
/// field the user didn't touch. Emptying the value still clears via the
/// normal `Cleared` path; this just makes deleting the line behave the same.
fn effective_parent_update(
    field: &FieldUpdate<String>,
    current_has_parent: bool,
) -> FieldUpdate<String> {
    match field {
        FieldUpdate::Absent if current_has_parent => FieldUpdate::Cleared,
        other => other.clone(),
    }
}

// ── Layout Persistence ──────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize)]
struct LayoutRaw {
    columns: Vec<Vec<String>>,
}

fn load_layout(project_path: &Path) -> GridLayout {
    let path = project_path.join("layout.json");
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(raw) = serde_json::from_str::<LayoutRaw>(&s) {
            return GridLayout {
                columns: raw.columns.into_iter().map(|col| {
                    col.into_iter().map(|s| {
                        if s == "---" { GridItem::Separator }
                        else if s == "___" { GridItem::Empty }
                        else if let Some(text) = s.strip_prefix("# ") { GridItem::Header(text.to_string()) }
                        else { GridItem::Task(s) }
                    }).collect()
                }).collect(),
            };
        }
    }
    let order_path = project_path.join("order.json");
    if let Ok(s) = std::fs::read_to_string(&order_path) {
        if let Ok(slugs) = serde_json::from_str::<Vec<String>>(&s) {
            return GridLayout { columns: vec![slugs.into_iter().map(GridItem::Task).collect()] };
        }
    }
    GridLayout::default()
}

fn save_layout(layout: &GridLayout, project_path: &Path) {
    let raw = LayoutRaw {
        columns: layout.columns.iter().map(|col| {
            col.iter().map(|item| match item {
                GridItem::Task(slug) => slug.clone(),
                GridItem::Separator => "---".to_string(),
                GridItem::Empty => "___".to_string(),
                GridItem::Header(text) => format!("# {}", text),
            }).collect()
        }).collect(),
    };
    let path = project_path.join("layout.json");
    if let Ok(json) = serde_json::to_string_pretty(&raw) {
        let _ = std::fs::write(path, json);
    }
}

// ── Helpers ─────────────────────────────────────────────────

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn projects_dir() -> PathBuf {
    home_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join(".cm/projects")
}

/// Parse a dependency reference: "slug" (same project) or "project/slug" (cross-project).
fn parse_dep_ref<'a>(dep: &'a str, current_project: &'a str) -> (&'a str, &'a str) {
    if let Some((project, slug)) = dep.split_once('/') {
        (project, slug)
    } else {
        (current_project, dep)
    }
}

fn sync_layout_with_tasks(layout: &mut GridLayout, tasks: &[PlanTask]) {
    let task_slugs: HashSet<&str> = tasks.iter().map(|t| t.slug.as_str()).collect();
    for col in &mut layout.columns {
        col.retain(|item| match item {
            GridItem::Task(slug) => task_slugs.contains(slug.as_str()),
            GridItem::Separator | GridItem::Empty | GridItem::Header(_) => true,
        });
        // Trim trailing Empty padding. Moving an item down past the end
        // of a column (`A-J`/visual move) pushes a fresh Empty each time,
        // and once the content below them is deleted those empties become
        // a dead blank tail that grows without bound — new tasks then
        // render dozens of rows below the last real item. Mid-column
        // empties are deliberate spacing and stay.
        while matches!(col.last(), Some(GridItem::Empty)) {
            col.pop();
        }
    }
    let mut in_layout: HashSet<String> = HashSet::new();
    for col in &layout.columns {
        for item in col {
            if let GridItem::Task(slug) = item { in_layout.insert(slug.clone()); }
        }
    }
    let missing: Vec<&PlanTask> = tasks.iter()
        .filter(|t| !in_layout.contains(&t.slug))
        .collect();
    if !missing.is_empty() {
        if layout.columns.is_empty() { layout.columns.push(vec![]); }
        // Add user tasks first, then claude-proposed tasks at the bottom.
        let (user_tasks, claude_tasks): (Vec<_>, Vec<_>) = missing
            .into_iter()
            .partition(|t| t.source != "claude");
        // Insert after the last NON-EMPTY cell, not the raw vector end:
        // trailing `Empty` placeholders would otherwise push new tasks
        // below blank space, off the bottom of the visible column.
        let col = &mut layout.columns[0];
        let mut at = col.iter()
            .rposition(|item| !matches!(item, GridItem::Empty))
            .map_or(0, |i| i + 1);
        for t in user_tasks {
            col.insert(at, GridItem::Task(t.slug.clone()));
            at += 1;
        }
        for t in claude_tasks {
            col.insert(at, GridItem::Task(t.slug.clone()));
            at += 1;
        }
    }
    layout.columns.retain(|col| !col.is_empty());
}

/// Ensure project directory exists for layout persistence.
fn ensure_project_dir(project_name: &str) -> PathBuf {
    let path = projects_dir().join(project_name);
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Read the persisted `repo_url` for a project from
/// `<project_path>/repo_url`. Returns an empty string when the file is
/// absent or unreadable; callers fall back to `repo_url_for_project`
/// in that case.
fn read_project_repo_url(project_path: &Path) -> String {
    std::fs::read_to_string(project_path.join("repo_url"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// ── REPOS mapping (matches dispatch/config.py) ─────────────

fn repo_url_for_project(project: &str) -> String {
    // Known repos — keep in sync with dispatch/config.py REPOS.
    match project {
        "predictionTrading" => "https://github.com/Bigbadboybob/predictionTrading.git".to_string(),
        "claude-manager" => "https://github.com/Bigbadboybob/claude-manager.git".to_string(),
        _ => format!("https://github.com/Bigbadboybob/{}.git", project),
    }
}

// ── PlanningView ────────────────────────────────────────────

pub struct PlanningView {
    projects: Vec<PlanProject>,
    project_data: Vec<ProjectData>,
    /// None = show all projects, Some(idx) = show one project.
    project_filter: Option<usize>,
    /// Maps global column index → (project_data_idx, col_within_project).
    unified_cols: Vec<(usize, usize)>,
    cursor: GridCursor,
    /// Per-column vertical scroll for grid mode, indexed by `unified_cols`
    /// position. Kept in sync with `unified_cols` by `rebuild_unified_cols`.
    /// A single shared offset doesn't work: scrolling a tall column past a
    /// shorter column's end would make the shorter column render empty.
    grid_col_scroll: Vec<usize>,
    /// Flat-list scroll for linear mode.
    linear_scroll: usize,
    /// Visible row count of each grid column's list area. Updated from
    /// `draw_grid` (which has `&self`) via interior mutability, and read by
    /// `ensure_cursor_visible` / detail scroll keybindings to decide when
    /// to scroll. Previously this was a plain `usize` mutated only by an
    /// `update_layout()` method that no caller invoked — so it stayed at
    /// the `new()` default of 20, making columns "feel" 20 rows tall even
    /// in tall terminals.
    grid_rows_visible: Cell<usize>,
    linear_mode: bool,
    /// Qualified conflict slugs: "project_name/slug".
    conflict_slugs: HashSet<String>,
    /// Scroll offset for the detail panel.
    detail_scroll: u16,
    /// Visual selection anchor row (within current column). None = not in visual mode.
    visual_anchor: Option<usize>,
    editor: Option<Session>,
    editing_slug: Option<String>,
    editing_project_idx: Option<usize>,
    editing_temp_path: Option<PathBuf>,
    input_mode: PlanInputMode,
    pub needs_redraw: bool,
    last_editor_size: (u16, u16),
    initialized: bool,
    /// Open workspaces the user can launch a task into. Populated by App
    /// before each planning event / render.
    workspace_candidates: Vec<WorkspaceCandidate>,
    /// When false, archived tasks are skipped from rendering and cursor
    /// navigation. Toggled with alt+shift+v.
    show_archived: bool,
    /// Task IDs whose direct children are unfolded in the planning
    /// tree. Anything not in this set renders as collapsed. In-memory
    /// only; resets across TUI restarts. Default empty so the user
    /// always opens to a tidy top-level view.
    expanded_tasks: HashSet<String>,
    /// Committed search query (`/` + Enter). Bare `n`/`N` in normal
    /// mode step through its matches; matching rows highlight their
    /// title. `None` = no active search.
    last_search: Option<String>,
    /// Task IDs matching `last_search`, in board walk order (columns
    /// left-to-right, rows top-to-bottom, folded subtasks included).
    /// A cache: `jump_search` recomputes it from `last_search` on
    /// every press, so stale entries (deleted tasks, project switch)
    /// can never send the cursor somewhere wrong — the stored copy
    /// only feeds the `match i/n` echo and tests.
    search_matches: Vec<String>,
    /// Footer echo like `match 2/5: query`, rendered inside the help
    /// separator line. Set by n/N jumps and search accept.
    search_status: Option<String>,
    /// Cursor + fold-state snapshot taken when the `/` prompt opens.
    /// Esc restores it (incremental search may have moved the cursor
    /// and auto-unfolded parents); Enter drops it.
    search_return: Option<(GridCursor, HashSet<String>)>,
}

impl PlanningView {
    pub fn new() -> Self {
        PlanningView {
            projects: vec![],
            project_data: vec![],
            project_filter: None,
            unified_cols: vec![],
            cursor: GridCursor { col: 0, row: 0 },
            grid_col_scroll: vec![],
            linear_scroll: 0,
            grid_rows_visible: Cell::new(20),
            linear_mode: false,
            conflict_slugs: HashSet::new(),
            detail_scroll: 0,
            visual_anchor: None,
            editor: None,
            editing_slug: None,
            editing_project_idx: None,
            editing_temp_path: None,
            input_mode: PlanInputMode::Normal,
            needs_redraw: true,
            last_editor_size: (80, 24),
            initialized: false,
            workspace_candidates: vec![],
            show_archived: false,
            expanded_tasks: HashSet::new(),
            last_search: None,
            search_matches: vec![],
            search_status: None,
            search_return: None,
        }
    }

    /// Update planning data from API tasks. Called when BackendEvent::PlanTasksUpdated arrives.
    /// Merges API state into existing local state rather than rebuilding from scratch.
    pub fn update_from_api(&mut self, api_tasks: Vec<Task>) {
        // Group incoming tasks by project.
        let mut by_project: HashMap<String, Vec<PlanTask>> = HashMap::new();
        for task in &api_tasks {
            if let Some(ref project) = task.project {
                by_project.entry(project.clone())
                    .or_default()
                    .push(PlanTask::from_api(task));
            }
        }

        // Collect all project names (union of existing and incoming).
        let mut project_names: HashSet<String> = by_project.keys().cloned().collect();
        for pd in &self.project_data {
            project_names.insert(pd.project.name.clone());
        }
        let mut project_names: Vec<String> = project_names.into_iter().collect();
        project_names.sort();

        // Build new project_data by merging.
        let mut new_data: Vec<ProjectData> = Vec::new();
        for name in &project_names {
            let path = ensure_project_dir(name);
            let api_tasks_for_project = by_project.remove(name).unwrap_or_default();
            // Fallback chain: disk file → API task → previously
            // hydrated value → empty. The disk file (written by
            // `create_project`) is the source of truth for locally-
            // created projects. The API-task fallback covers projects
            // discovered purely from the API where no local file
            // exists. Preserving the previously-hydrated value matters
            // because a later refresh that returns no tasks for this
            // project (or tasks whose URLs are blank) would otherwise
            // clobber a known-good URL — sending the next
            // `create_task` back to the hardcoded github fallback.
            // Empty stays empty as a final fallback so `create_task`
            // can defer to `repo_url_for_project` for genuinely fresh
            // projects with no remote yet.
            let mut repo_url = read_project_repo_url(&path);
            if repo_url.is_empty() {
                if let Some(t) = api_tasks_for_project
                    .iter()
                    .find(|t| !t.repo_url.trim().is_empty())
                {
                    repo_url = t.repo_url.trim().to_string();
                }
            }
            if repo_url.is_empty() {
                if let Some(prev) = self
                    .project_data
                    .iter()
                    .find(|pd| pd.project.name == *name)
                    .map(|pd| pd.project.repo_url.trim().to_string())
                {
                    if !prev.is_empty() {
                        repo_url = prev;
                    }
                }
            }
            let project = PlanProject { name: name.clone(), path: path.clone(), repo_url };

            // Find existing project data if we have it.
            let existing = self.project_data.iter()
                .position(|pd| pd.project.name == *name);

            if let Some(ei) = existing {
                let pd = &mut self.project_data[ei];

                // Merge: update existing tasks, add new ones, remove deleted ones.
                let api_ids: HashSet<String> = api_tasks_for_project.iter()
                    .map(|t| t.id.clone()).collect();

                // Update existing tasks from API data.
                for api_task in &api_tasks_for_project {
                    if let Some(local_task) = pd.tasks.iter_mut().find(|t| t.id == api_task.id) {
                        // Overwrite local fields with API (DB is source of truth).
                        local_task.title = api_task.title.clone();
                        local_task.status = api_task.status;
                        local_task.difficulty = api_task.difficulty;
                        local_task.depends = api_task.depends.clone();
                        local_task.branch = api_task.branch.clone();
                        local_task.description = api_task.description.clone();
                        local_task.prompt = api_task.prompt.clone();
                        local_task.source = api_task.source.clone();
                        local_task.is_cloud = api_task.is_cloud;
                        local_task.slug = api_task.slug.clone();
                        // Without this, an API-side reparent (e.g. an
                        // agent's `update_task(parent_task_id=...)`) never
                        // reached the planning grid: the merge silently
                        // dropped the field, the `children_of` walk read
                        // stale `None` from `pd.tasks`, and the subtask
                        // never appeared under its parent.
                        local_task.parent_task_id = api_task.parent_task_id.clone();
                    } else {
                        // New task from API — add it.
                        pd.tasks.push(api_task.clone());
                    }
                }

                // Remove tasks that no longer exist in the API.
                let removed_slugs: Vec<String> = pd.tasks.iter()
                    .filter(|t| !api_ids.contains(&t.id))
                    .map(|t| t.slug.clone())
                    .collect();
                pd.tasks.retain(|t| api_ids.contains(&t.id));
                for slug in &removed_slugs {
                    for col in &mut pd.layout.columns {
                        col.retain(|item| !matches!(item, GridItem::Task(s) if s == slug));
                    }
                }

                // Sync layout with current tasks (adds new tasks to layout).
                sync_layout_with_tasks(&mut pd.layout, &pd.tasks);
                save_layout(&pd.layout, &path);

                new_data.push(ProjectData {
                    project,
                    tasks: pd.tasks.drain(..).collect(),
                    layout: pd.layout.clone(),
                });
            } else {
                // Brand new project — load layout from disk and build fresh.
                let mut layout = load_layout(&path);
                sync_layout_with_tasks(&mut layout, &api_tasks_for_project);
                save_layout(&layout, &path);
                new_data.push(ProjectData { project, tasks: api_tasks_for_project, layout });
            }
        }

        self.projects = new_data.iter().map(|pd| pd.project.clone()).collect();
        self.project_data = new_data;

        self.rebuild_unified_cols();
        self.recompute_conflicts();
        // Board contents changed wholesale: recompute the search-match
        // cache so deleted/renamed tasks drop out instead of lingering
        // as stale IDs (n/N re-resolves position at jump time anyway).
        self.refresh_search_matches();
        if !self.initialized {
            self.cursor = GridCursor { col: 0, row: 0 };
            self.snap_cursor_to_selectable(1);
            self.initialized = true;
        }
        self.clamp_cursor();
        self.needs_redraw = true;
    }

    /// Handle a single task being created via the API.
    pub fn on_task_created(&mut self, task: Task) {
        if let Some(ref project_name) = task.project {
            let plan_task = PlanTask::from_api(&task);
            let slug = plan_task.slug.clone();

            // Find or create the project.
            let pi = match self.project_data.iter().position(|pd| pd.project.name == *project_name) {
                Some(i) => i,
                None => {
                    let path = ensure_project_dir(project_name);
                    // Disk file wins; otherwise inherit from the
                    // just-created task so API-discovered projects
                    // pick up their actual remote (forked / renamed
                    // / non-github URLs) instead of being silently
                    // rewritten to the github default at task-create
                    // time.
                    let mut repo_url = read_project_repo_url(&path);
                    if repo_url.is_empty() && !task.repo_url.trim().is_empty() {
                        repo_url = task.repo_url.trim().to_string();
                    }
                    let project = PlanProject { name: project_name.clone(), path, repo_url };
                    self.projects.push(project.clone());
                    self.project_data.push(ProjectData {
                        project,
                        tasks: vec![],
                        layout: GridLayout::default(),
                    });
                    self.project_data.len() - 1
                }
            };

            self.project_data[pi].tasks.push(plan_task);

            // Add to layout at cursor position if cursor is in this project.
            let ci = self.unified_cols.get(self.cursor.col)
                .filter(|(p, _)| *p == pi)
                .map(|(_, c)| *c)
                .unwrap_or_else(|| {
                    if self.project_data[pi].layout.columns.is_empty() {
                        self.project_data[pi].layout.columns.push(vec![]);
                    }
                    0
                });
            let insert_at = if self.cursor_project_idx() == Some(pi) {
                let raw = self.insert_anchor_raw_idx().unwrap_or_else(|| {
                    self.project_data[pi].layout.columns[ci].len().saturating_sub(1)
                });
                (raw + 1).min(self.project_data[pi].layout.columns[ci].len())
            } else {
                self.project_data[pi].layout.columns[ci].len()
            };
            self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Task(slug.clone()));
            save_layout(&self.project_data[pi].layout, &self.project_data[pi].project.path);
            self.rebuild_unified_cols();
            self.recompute_conflicts();

            // Move cursor to the newly created task and open editor.
            // After rebuild_unified_cols, the visible-rows projection
            // includes the new task at some visible-row index; find it
            // by slug so cursor.row lands on it (not on the raw idx).
            if let Some(uc) = self.unified_cols.iter().position(|(p, c)| *p == pi && *c == ci) {
                self.cursor.col = uc;
                if let Some(rows) = self.cursor_visible_column() {
                    if let Some(idx) = rows.iter().position(|r| visible_row_slug(r) == Some(slug.as_str())) {
                        self.cursor.row = idx;
                    }
                }
            }
            self.start_editor();

            self.needs_redraw = true;
        }
    }

    /// Handle a single task being updated via the API response.
    /// Merges the API state into the existing local task without rebuilding.
    pub fn on_task_updated(&mut self, api_task: Task) {
        for pd in &mut self.project_data {
            if let Some(task) = pd.tasks.iter_mut().find(|t| t.id == api_task.id) {
                task.title = api_task.name.clone().unwrap_or_else(|| {
                    api_task.slug.clone().unwrap_or_else(|| "untitled".to_string())
                });
                task.status = PlanStatus::from_str(&api_task.status);
                task.difficulty = api_task.difficulty.map(|d| d as u8);
                task.depends = api_task.depends.clone().unwrap_or_default();
                task.branch = Some(api_task.repo_branch.clone());
                task.description = api_task.description.clone().unwrap_or_default();
                task.prompt = api_task.prompt.clone().unwrap_or_default();
                task.source = api_task.source.clone();
                task.is_cloud = api_task.is_cloud;
                task.parent_task_id = api_task.parent_task_id.clone();
                self.recompute_conflicts();
                self.needs_redraw = true;
                return;
            }
        }
    }

    /// Handle a task being deleted via the API.
    pub fn on_task_deleted(&mut self, task_id: &str) {
        for pd in &mut self.project_data {
            if let Some(ti) = pd.tasks.iter().position(|t| t.id == task_id) {
                let slug = pd.tasks[ti].slug.clone();
                pd.tasks.remove(ti);
                for col in &mut pd.layout.columns {
                    col.retain(|item| !matches!(item, GridItem::Task(s) if s == &slug));
                }
                save_layout(&pd.layout, &pd.project.path);
                break;
            }
        }
        self.rebuild_unified_cols();
        self.recompute_conflicts();
        self.clamp_cursor();
        self.needs_redraw = true;
    }

    /// Mark a task as done by project name and slug. Called from sessions view.
    pub fn mark_task_done_by_id(&mut self, task_id: &str) {
        for pd in &mut self.project_data {
            if let Some(task) = pd.tasks.iter_mut().find(|t| t.id == task_id) {
                task.status = PlanStatus::Done;
                return;
            }
        }
    }

    /// Optimistically flip a task back to `InProgress` in the in-memory plan.
    /// The grid reconciles on the next `refresh_plan_tasks` tick.
    pub fn mark_task_running_by_id(&mut self, task_id: &str) {
        for pd in &mut self.project_data {
            if let Some(task) = pd.tasks.iter_mut().find(|t| t.id == task_id) {
                task.status = PlanStatus::InProgress;
                return;
            }
        }
    }

    fn rebuild_unified_cols(&mut self) {
        self.unified_cols.clear();
        for (pi, pd) in self.project_data.iter().enumerate() {
            if let Some(filter) = self.project_filter {
                if pi != filter { continue; }
            }
            for ci in 0..pd.layout.columns.len() {
                self.unified_cols.push((pi, ci));
            }
        }
        // Per-column scroll offsets shadow `unified_cols`. We don't try to
        // remap by (pi, ci) on rebuild — resetting new entries to 0 is fine;
        // entries below the new length keep their existing scroll.
        self.grid_col_scroll.resize(self.unified_cols.len(), 0);
    }

    fn recompute_conflicts(&mut self) {
        self.conflict_slugs.clear();
        let mut positions: HashMap<(String, String), usize> = HashMap::new();
        for pd in &self.project_data {
            for col in &pd.layout.columns {
                for (ri, item) in col.iter().enumerate() {
                    if let GridItem::Task(slug) = item {
                        positions.insert((pd.project.name.clone(), slug.clone()), ri);
                    }
                }
            }
        }
        for pd in &self.project_data {
            for task in &pd.tasks {
                if task.depends.is_empty() { continue; }
                let task_row = match positions.get(&(pd.project.name.clone(), task.slug.clone())) {
                    Some(r) => *r,
                    None => continue,
                };
                for dep_ref in &task.depends {
                    let (dep_proj, dep_slug) = parse_dep_ref(dep_ref, &pd.project.name);
                    let dep_row = match positions.get(&(dep_proj.to_string(), dep_slug.to_string())) {
                        Some(r) => *r,
                        None => continue,
                    };
                    if task_row < dep_row {
                        self.conflict_slugs.insert(format!("{}/{}", pd.project.name, task.slug));
                        self.conflict_slugs.insert(format!("{}/{}", dep_proj, dep_slug));
                    }
                }
            }
        }
    }

    fn is_conflict(&self, project_name: &str, slug: &str) -> bool {
        self.conflict_slugs.contains(&format!("{}/{}", project_name, slug))
    }

    // ── Cursor helpers ──────────────────────────────────────

    fn cursor_project_idx(&self) -> Option<usize> {
        self.unified_cols.get(self.cursor.col).map(|(pi, _)| *pi)
    }

    /// Raw layout column for the cursor's project/col. Used by raw-layout
    /// mutations (reorder, move, insert, delete) which need to operate
    /// on the persisted `Vec<GridItem>`. Read-only callers should prefer
    /// `cursor_visible_column` so subtasks line up with their parent.
    fn cursor_raw_column(&self) -> Option<&Vec<GridItem>> {
        let (pi, ci) = *self.unified_cols.get(self.cursor.col)?;
        self.project_data.get(pi)?.layout.columns.get(ci)
    }

    /// Tree-aware visible projection of the cursor's column. Each
    /// frame, this is what cursor.row indexes.
    fn cursor_visible_column(&self) -> Option<Vec<VisibleRow>> {
        let (pi, ci) = *self.unified_cols.get(self.cursor.col)?;
        Some(self.visible_rows_for_column(pi, ci))
    }

    fn cursor_visible_row(&self) -> Option<VisibleRow> {
        let col = self.cursor_visible_column()?;
        col.into_iter().nth(self.cursor.row)
    }

    /// Resolve cursor.row (visible) to the raw `Vec<GridItem>` index
    /// for the cursor's column. `None` when the cursor is on a
    /// synthetic subtask row (which has no raw-layout slot).
    fn cursor_raw_idx(&self) -> Option<usize> {
        match self.cursor_visible_row()?.kind {
            VisibleRowKind::Layout { raw_idx, .. } => Some(raw_idx),
            VisibleRowKind::Subtask { .. } => None,
        }
    }

    fn selected_slug(&self) -> Option<String> {
        let row = self.cursor_visible_row()?;
        visible_row_slug(&row).map(str::to_string)
    }

    fn selected_header_text(&self) -> Option<String> {
        let row = self.cursor_visible_row()?;
        match row.kind {
            VisibleRowKind::Layout { item: GridItem::Header(text), .. } => Some(text),
            _ => None,
        }
    }

    /// Returns (project_data_idx, task_idx_within_project).
    fn selected_task_loc(&self) -> Option<(usize, usize)> {
        let slug = self.selected_slug()?;
        let pi = self.cursor_project_idx()?;
        let ti = self.project_data[pi].tasks.iter().position(|t| t.slug == slug)?;
        Some((pi, ti))
    }

    fn selected_task(&self) -> Option<(&PlanTask, &str)> {
        let slug = self.selected_slug()?;
        let pi = self.cursor_project_idx()?;
        let pd = &self.project_data[pi];
        let task = pd.tasks.iter().find(|t| t.slug == slug)?;
        Some((task, &pd.project.name))
    }

    fn save_project_layout(&self, pi: usize) {
        if let Some(pd) = self.project_data.get(pi) {
            save_layout(&pd.layout, &pd.project.path);
        }
    }

    fn clamp_cursor(&mut self) {
        if self.unified_cols.is_empty() {
            self.cursor = GridCursor { col: 0, row: 0 };
            return;
        }
        if self.cursor.col >= self.unified_cols.len() {
            self.cursor.col = self.unified_cols.len() - 1;
        }
        if let Some(col) = self.cursor_visible_column() {
            if col.is_empty() {
                self.cursor.row = 0;
            } else if self.cursor.row >= col.len() {
                self.cursor.row = col.len() - 1;
            }
        }
    }

    /// Whether the cursor is allowed to land on a visible row.
    /// Skips Empty layout rows always; archived tasks are filtered at
    /// `visible_rows_for_column` build time so any row that reaches
    /// here is non-archived (or `show_archived` is on).
    fn is_visible_row_selectable(&self, row: &VisibleRow) -> bool {
        match &row.kind {
            VisibleRowKind::Layout { item: GridItem::Empty, .. } => false,
            _ => true,
        }
    }

    fn snap_cursor_to_selectable(&mut self, direction: i32) {
        let rows = match self.cursor_visible_column() {
            Some(r) if !r.is_empty() => r,
            _ => return,
        };
        let len = rows.len() as i32;
        let start = (self.cursor.row as i32).min(len - 1);
        let mut pos = start;
        for _ in 0..rows.len() {
            if rows.get(pos as usize)
                .map(|r| self.is_visible_row_selectable(r))
                .unwrap_or(false)
            {
                self.cursor.row = pos as usize;
                return;
            }
            pos = (pos + direction).rem_euclid(len);
        }
    }

    fn is_first_col_of_project(&self, global_col: usize) -> bool {
        if global_col == 0 { return true; }
        let (pi, _) = self.unified_cols[global_col];
        let (prev_pi, _) = self.unified_cols[global_col - 1];
        pi != prev_pi
    }

    fn visual_range(&self) -> Option<(usize, usize)> {
        let anchor = self.visual_anchor?;
        Some((anchor.min(self.cursor.row), anchor.max(self.cursor.row)))
    }

    fn is_in_visual_range(&self, col: usize, row: usize) -> bool {
        if col != self.cursor.col { return false; }
        match self.visual_range() {
            Some((start, end)) => row >= start && row <= end,
            None => false,
        }
    }

    fn cancel_visual(&mut self) {
        self.visual_anchor = None;
    }

    /// Toggle the fold state of the task under the cursor. Only acts
    /// when the cursor is on a task row that has at least one child;
    /// otherwise a no-op. Repositions cursor.row to keep the same task
    /// focused after the visible-row list re-shuffles.
    fn toggle_fold_at_cursor(&mut self) {
        let row = match self.cursor_visible_row() { Some(r) => r, None => return };
        if !row.has_children { return; }
        let slug = match visible_row_slug(&row) { Some(s) => s.to_string(), None => return };
        let pi = match self.cursor_project_idx() { Some(p) => p, None => return };
        let task_id = match self.project_data.get(pi).and_then(|pd| pd.tasks.iter().find(|t| t.slug == slug)) {
            Some(t) => t.id.clone(),
            None => return,
        };
        if self.expanded_tasks.contains(&task_id) {
            self.expanded_tasks.remove(&task_id);
        } else {
            self.expanded_tasks.insert(task_id);
        }
        // After fold flip, the visible-row list changes length. Walk it
        // to find the same task again so the cursor stays put visually.
        if let Some(rows) = self.cursor_visible_column() {
            if let Some(idx) = rows.iter().position(|r| visible_row_slug(r) == Some(slug.as_str())) {
                self.cursor.row = idx;
            }
        }
        self.ensure_cursor_visible();
    }

    // ── Navigation ──────────────────────────────────────────

    fn navigate_vertical(&mut self, direction: i32) {
        if self.unified_cols.is_empty() { return; }
        let prev_slug = self.selected_slug();
        let in_visual = self.visual_anchor.is_some();

        if self.linear_mode && !in_visual {
            let mut selectable_positions: Vec<(usize, usize)> = Vec::new();
            for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
                let rows = self.visible_rows_for_column(pi, ci);
                for (ri, row) in rows.iter().enumerate() {
                    if self.is_visible_row_selectable(row) {
                        selectable_positions.push((gi, ri));
                    }
                }
            }
            if selectable_positions.is_empty() { return; }
            let cur = selectable_positions.iter()
                .position(|&(c, r)| c == self.cursor.col && r == self.cursor.row)
                .unwrap_or(0);
            let next = (cur as i32 + direction).rem_euclid(selectable_positions.len() as i32) as usize;
            self.cursor.col = selectable_positions[next].0;
            self.cursor.row = selectable_positions[next].1;
        } else {
            let rows = match self.cursor_visible_column() {
                Some(r) if !r.is_empty() => r,
                _ => return,
            };
            let len = rows.len() as i32;
            if in_visual {
                let next = self.cursor.row as i32 + direction;
                if next < 0 || next >= len { return; }
                self.cursor.row = next as usize;
            } else {
                let mut next = self.cursor.row as i32;
                for _ in 0..rows.len() {
                    next = (next + direction).rem_euclid(len);
                    if rows.get(next as usize)
                        .map(|r| self.is_visible_row_selectable(r))
                        .unwrap_or(false)
                    {
                        break;
                    }
                }
                self.cursor.row = next as usize;
            }
        }
        self.ensure_cursor_visible();
        if self.selected_slug() != prev_slug {
            self.detail_scroll = 0;
        }
    }

    fn navigate_horizontal(&mut self, direction: i32) {
        if self.linear_mode || self.unified_cols.is_empty() { return; }
        self.cancel_visual();
        let len = self.unified_cols.len() as i32;
        let next = (self.cursor.col as i32 + direction).rem_euclid(len) as usize;
        self.cursor.col = next;
        if let Some(col) = self.cursor_visible_column() {
            if col.is_empty() { self.cursor.row = 0; }
            else if self.cursor.row >= col.len() { self.cursor.row = col.len() - 1; }
        }
        self.snap_cursor_to_selectable(direction);
        self.ensure_cursor_visible();
    }

    fn ensure_cursor_visible(&mut self) {
        let h = self.grid_rows_visible.get();
        if h == 0 { return; }
        if self.linear_mode {
            let flat = self.cursor_flat_index_linear();
            if flat < self.linear_scroll {
                self.linear_scroll = flat;
            } else if flat >= self.linear_scroll + h {
                self.linear_scroll = flat.saturating_sub(h - 1);
            }
            return;
        }
        if self.cursor.col >= self.grid_col_scroll.len() { return; }
        let off = self.grid_col_scroll[self.cursor.col];
        if self.cursor.row < off {
            self.grid_col_scroll[self.cursor.col] = self.cursor.row;
            return;
        }
        // cursor.row already indexes the visible-row projection, so
        // the count "off..=cursor.row" is cursor.row - off + 1.
        let visible = self.cursor.row.saturating_sub(off).saturating_add(1);
        if visible <= h { return; }
        self.grid_col_scroll[self.cursor.col] =
            self.cursor.row.saturating_sub(h - 1);
    }

    /// Build the tree-aware visible-row list for one column. Top-level
    /// entries come from raw layout; expanded parents' children get
    /// emitted as synthetic `Subtask` rows immediately after their
    /// parent. Parented tasks that already appear in their own raw
    /// column are filtered out from there (they only show under the
    /// parent).
    ///
    /// Archived filtering still happens at this layer — archived tasks
    /// and their subtrees are skipped when `show_archived` is off.
    fn visible_rows_for_column(&self, pi: usize, ci: usize) -> Vec<VisibleRow> {
        self.visible_rows_for_column_opts(pi, ci, false)
    }

    /// `force_expand` treats every parent as unfolded regardless of
    /// `expanded_tasks` — used by search-match computation so folded
    /// subtasks still count as matches (the jump auto-unfolds them).
    fn visible_rows_for_column_opts(&self, pi: usize, ci: usize, force_expand: bool) -> Vec<VisibleRow> {
        let pd = match self.project_data.get(pi) {
            Some(p) => p,
            None => return Vec::new(),
        };
        let column = match pd.layout.columns.get(ci) {
            Some(c) => c,
            None => return Vec::new(),
        };

        let task_by_id: HashMap<&str, &PlanTask> = pd.tasks.iter()
            .map(|t| (t.id.as_str(), t))
            .collect();
        let task_by_slug: HashMap<&str, &PlanTask> = pd.tasks.iter()
            .map(|t| (t.slug.as_str(), t))
            .collect();

        // children_of[parent_id] = child slugs in column-walk order
        // (left-to-right across raw columns, top-to-bottom within each).
        // Only includes children whose parent is in this project.
        let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
        for col in &pd.layout.columns {
            for item in col {
                if let GridItem::Task(slug) = item {
                    let t = match task_by_slug.get(slug.as_str()) { Some(t) => *t, None => continue };
                    let pid = match &t.parent_task_id { Some(p) => p, None => continue };
                    if !task_by_id.contains_key(pid.as_str()) { continue; }
                    // Archived subtasks invisible when show_archived is off.
                    if !self.show_archived && t.status == PlanStatus::Archived { continue; }
                    children_of.entry(pid.clone()).or_default().push(slug.clone());
                }
            }
        }

        // Transitive descendant count per task id, memoized DFS.
        fn count_desc(
            task_id: &str,
            children_of: &HashMap<String, Vec<String>>,
            task_by_slug: &HashMap<&str, &PlanTask>,
            memo: &mut HashMap<String, u32>,
        ) -> u32 {
            if let Some(c) = memo.get(task_id) { return *c; }
            // Insert a placeholder to short-circuit cycles defensively.
            memo.insert(task_id.to_string(), 0);
            let kids = match children_of.get(task_id) {
                Some(v) => v.clone(),
                None => { return 0; }
            };
            let mut total: u32 = kids.len() as u32;
            for child_slug in &kids {
                if let Some(child) = task_by_slug.get(child_slug.as_str()) {
                    total += count_desc(&child.id, children_of, task_by_slug, memo);
                }
            }
            memo.insert(task_id.to_string(), total);
            total
        }
        let mut desc_memo: HashMap<String, u32> = HashMap::new();
        for t in &pd.tasks {
            count_desc(&t.id, &children_of, &task_by_slug, &mut desc_memo);
        }

        // DFS subtree emitter for synthetic child rows. Iterative to
        // sidestep borrow gymnastics with self.expanded_tasks.
        let emit_subtree = |
            root_slug: &str,
            start_depth: u8,
            out: &mut Vec<VisibleRow>,
        | {
            let mut stack: Vec<(String, u8)> = vec![(root_slug.to_string(), start_depth)];
            while let Some((slug, depth)) = stack.pop() {
                let task = match task_by_slug.get(slug.as_str()) { Some(t) => *t, None => continue };
                let kids = children_of.get(&task.id);
                let has_children = kids.map_or(false, |v| !v.is_empty());
                let expanded = force_expand || self.expanded_tasks.contains(&task.id);
                let dcount = desc_memo.get(&task.id).copied().unwrap_or(0);
                out.push(VisibleRow {
                    kind: VisibleRowKind::Subtask { slug: slug.clone() },
                    depth,
                    has_children,
                    expanded,
                    descendant_count: dcount,
                });
                if expanded {
                    if let Some(kids) = kids {
                        for child_slug in kids.iter().rev() {
                            stack.push((child_slug.clone(), depth + 1));
                        }
                    }
                }
            }
        };

        let mut out: Vec<VisibleRow> = Vec::new();
        for (raw_idx, item) in column.iter().enumerate() {
            match item {
                GridItem::Task(slug) => {
                    // Skip rendering parented tasks here — they appear
                    // under their parent's row instead.
                    if let Some(t) = task_by_slug.get(slug.as_str()) {
                        if let Some(pid) = &t.parent_task_id {
                            if task_by_id.contains_key(pid.as_str()) {
                                continue;
                            }
                        }
                        // Hide archived top-level tasks when the
                        // show_archived toggle (A-V) is off.
                        if !self.show_archived && t.status == PlanStatus::Archived {
                            continue;
                        }
                    }
                    let task = task_by_slug.get(slug.as_str()).copied();
                    let (has_children, expanded, dcount) = match task {
                        Some(t) => (
                            children_of.get(&t.id).map_or(false, |v| !v.is_empty()),
                            force_expand || self.expanded_tasks.contains(&t.id),
                            desc_memo.get(&t.id).copied().unwrap_or(0),
                        ),
                        None => (false, false, 0),
                    };
                    out.push(VisibleRow {
                        kind: VisibleRowKind::Layout { raw_idx, item: item.clone() },
                        depth: 0,
                        has_children,
                        expanded,
                        descendant_count: dcount,
                    });
                    if expanded {
                        if let Some(t) = task {
                            let kids = children_of.get(&t.id).cloned().unwrap_or_default();
                            for child_slug in kids {
                                emit_subtree(&child_slug, 1, &mut out);
                            }
                        }
                    }
                }
                _ => {
                    out.push(VisibleRow {
                        kind: VisibleRowKind::Layout { raw_idx, item: item.clone() },
                        depth: 0,
                        has_children: false,
                        expanded: false,
                        descendant_count: 0,
                    });
                }
            }
        }
        out
    }

    /// Flat-list index of the cursor in linear mode, matching the order the
    /// linear renderer walks: project header + separator rows are counted.
    fn cursor_flat_index_linear(&self) -> usize {
        let mut flat = 0usize;
        for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
            let rows = self.visible_rows_for_column(pi, ci);
            if gi > 0 && self.is_first_col_of_project(gi) && !rows.is_empty() {
                flat += 1;
            }
            if self.is_first_col_of_project(gi) && self.project_filter.is_none() {
                flat += 1;
            }
            if gi == self.cursor.col {
                return flat + self.cursor.row;
            }
            flat += rows.len();
        }
        flat
    }

    // ── Event Handling ──────────────────────────────────────

    pub fn handle_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            if key.kind == crossterm::event::KeyEventKind::Release {
                return PlanAction::Ignored;
            }
        }
        self.needs_redraw = true;
        match self.input_mode {
            PlanInputMode::Editing => self.handle_editing_event(event),
            PlanInputMode::Searching { .. } => self.handle_search_event(event),
            PlanInputMode::NewTask { .. } => self.handle_new_task_event(event),
            PlanInputMode::NewHeader { .. } => self.handle_new_header_event(event),
            PlanInputMode::EditingHeader { .. } => self.handle_editing_header_event(event),
            PlanInputMode::BulkArchiveConfirm { .. } => self.handle_bulk_archive_confirm_event(event),
            PlanInputMode::NewProject { .. } => self.handle_new_project_event(event),
            PlanInputMode::ProjectPicker { .. } => self.handle_project_picker_event(event),
            PlanInputMode::WorkspacePicker { .. } => self.handle_workspace_picker_event(event),
            PlanInputMode::LaunchConfirm { .. } => self.handle_launch_confirm_event(event),
            PlanInputMode::Normal => self.handle_normal_event(event),
        }
    }

    fn handle_normal_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let has_alt = key.modifiers.contains(KeyModifiers::ALT);
            let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);
            let alt_shift = has_alt && has_shift;

            if alt_shift {
                match key.code {
                    KeyCode::Char('j') | KeyCode::Char('J') => { self.reorder_task(1); return PlanAction::Consumed; }
                    KeyCode::Char('k') | KeyCode::Char('K') => { self.reorder_task(-1); return PlanAction::Consumed; }
                    KeyCode::Char('h') | KeyCode::Char('H') => { self.move_task_to_column(-1); return PlanAction::Consumed; }
                    KeyCode::Char('l') | KeyCode::Char('L') => { self.move_task_to_column(1); return PlanAction::Consumed; }
                    KeyCode::Char('c') | KeyCode::Char('C') => { self.remove_column(); return PlanAction::Consumed; }
                    KeyCode::Char('p') | KeyCode::Char('P') => {
                        self.input_mode = PlanInputMode::NewProject { name: String::new(), repo_url: String::new(), field: NewProjectField::Name };
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('s') | KeyCode::Char('S') => { return self.cycle_status(false); }
                    // `A-O` (Alt+Shift+O): reopen a done task's worktree.
                    // Only fires on a focused task whose status is Done.
                    KeyCode::Char('o') | KeyCode::Char('O') => {
                        self.cancel_visual();
                        if let Some((pi, ti)) = self.selected_task_loc() {
                            if let Some(task) = self.project_data.get(pi).and_then(|pd| pd.tasks.get(ti)) {
                                if task.status == PlanStatus::Done {
                                    return PlanAction::ReopenTask { task_id: task.id.clone() };
                                }
                            }
                        }
                        return PlanAction::Consumed;
                    }
                    // `A-N` (Alt+Shift+N): create a subtask of the focused
                    // task. Mirrors `A-n` (top-level new) but the action
                    // carries `parent_task_id` so the API row is persisted
                    // with the parent link from creation. `list_subtasks`
                    // and `mark_subtask_done` then see it as a child of
                    // the parent.
                    KeyCode::Char('n') | KeyCode::Char('N') => {
                        self.cancel_visual();
                        if let Some((pi, ti)) = self.selected_task_loc() {
                            let parent = &self.project_data[pi].tasks[ti];
                            self.input_mode = PlanInputMode::NewTask {
                                title: String::new(),
                                parent_task_id: Some(parent.id.clone()),
                                parent_name: Some(parent.title.clone()),
                            };
                        }
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('u') | KeyCode::Char('U') => { return self.unlaunch_task(); }
                    KeyCode::Char('a') | KeyCode::Char('A') => {
                        self.cancel_visual();
                        if let Some(pi) = self.cursor_project_idx() {
                            let count = self.project_data[pi].tasks.iter()
                                .filter(|t| t.status == PlanStatus::Done).count();
                            if count > 0 {
                                self.input_mode = PlanInputMode::BulkArchiveConfirm { project_idx: pi, count };
                            }
                        }
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('v') | KeyCode::Char('V') => {
                        self.cancel_visual();
                        self.show_archived = !self.show_archived;
                        if !self.show_archived {
                            self.snap_cursor_to_selectable(1);
                        }
                        // Archived visibility changes which rows can
                        // match; keep the cached match list honest.
                        self.refresh_search_matches();
                        return PlanAction::Consumed;
                    }
                    _ => {}
                }
            }

            if has_alt && !has_shift {
                match key.code {
                    KeyCode::Char('q') => return PlanAction::Quit,
                    KeyCode::Char('t') => return PlanAction::SwitchToSessions,
                    KeyCode::Char('j') => { self.navigate_vertical(1); return PlanAction::Consumed; }
                    KeyCode::Char('k') => { self.navigate_vertical(-1); return PlanAction::Consumed; }
                    KeyCode::Char('h') => { self.navigate_horizontal(-1); return PlanAction::Consumed; }
                    KeyCode::Char('l') if !self.linear_mode => { self.navigate_horizontal(1); return PlanAction::Consumed; }
                    KeyCode::Enter => { self.insert_separator(); return PlanAction::Consumed; }
                    KeyCode::Char(' ') => { self.insert_empty(); return PlanAction::Consumed; }
                    KeyCode::Backspace => { self.remove_separator(); return PlanAction::Consumed; }
                    KeyCode::Char('c') => { self.add_column(); return PlanAction::Consumed; }
                    KeyCode::Char('v') => {
                        if self.visual_anchor.is_some() {
                            self.cancel_visual();
                        } else {
                            self.visual_anchor = Some(self.cursor.row);
                        }
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('g') => {
                        self.cancel_visual();
                        self.linear_mode = !self.linear_mode;
                        self.clamp_cursor();
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('e') => {
                        self.cancel_visual();
                        if let Some(text) = self.selected_header_text() {
                            self.input_mode = PlanInputMode::EditingHeader { text };
                            return PlanAction::Consumed;
                        }
                        return self.start_editor();
                    }
                    KeyCode::Char('i') => {
                        self.cancel_visual();
                        self.input_mode = PlanInputMode::NewHeader { text: String::new() };
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('n') => {
                        self.cancel_visual();
                        if self.projects.is_empty() {
                            self.input_mode = PlanInputMode::NewProject { name: String::new(), repo_url: String::new(), field: NewProjectField::Name };
                        } else {
                            self.input_mode = PlanInputMode::NewTask {
                                title: String::new(),
                                parent_task_id: None,
                                parent_name: None,
                            };
                        }
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('o') => { self.sort_column_by_status(); return PlanAction::Consumed; }
                    KeyCode::Char('s') => { return self.cycle_status(true); }
                    KeyCode::Char('a') => { return self.accept_proposal(); }
                    KeyCode::Char('x') => {
                        self.cancel_visual();
                        // `cursor.row` indexes the visible-row projection, not
                        // the raw layout — match on the visible row's kind so
                        // a focused subtask (synthetic) still routes through
                        // `delete_task` instead of being mis-detected as
                        // whatever sits at the same raw index.
                        if let Some(row) = self.cursor_visible_row() {
                            if matches!(
                                row.kind,
                                VisibleRowKind::Layout {
                                    item: GridItem::Separator | GridItem::Empty | GridItem::Header(_),
                                    ..
                                }
                            ) {
                                self.remove_separator();
                                return PlanAction::Consumed;
                            }
                        }
                        return self.delete_task();
                    }
                    KeyCode::Char('d') => { return self.cycle_status_to_done(); }
                    KeyCode::Char('f') => { self.cancel_visual(); return self.start_launch(); }
                    KeyCode::Char('u') => {
                        self.cancel_visual();
                        if let Some((pi, ti)) = self.selected_task_loc() {
                            if let Some(task) = self.project_data.get(pi).and_then(|pd| pd.tasks.get(ti)) {
                                return PlanAction::UnbindTask { task_id: task.id.clone() };
                            }
                        }
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('r') => { return PlanAction::RefreshTasks; }
                    // `A-w` (Watch): attach a live READ-ONLY view of the
                    // focused backtest task's worker tmux. Snapshots the VM
                    // coords off the row; the app-side handler validates
                    // kind/dispatch and spawns the gcloud attach session.
                    KeyCode::Char('w') => {
                        self.cancel_visual();
                        if let Some((pi, ti)) = self.selected_task_loc() {
                            if let Some(task) =
                                self.project_data.get(pi).and_then(|pd| pd.tasks.get(ti))
                            {
                                let title = task
                                    .bt_label
                                    .clone()
                                    .or_else(|| task.run_key.clone())
                                    .or_else(|| task.worker_vm.clone())
                                    .unwrap_or_else(|| task.slug.clone());
                                return PlanAction::WatchBacktest {
                                    task_id: task.id.clone(),
                                    kind: task.kind.clone(),
                                    worker_vm: task.worker_vm.clone(),
                                    vm_project: task.vm_project.clone(),
                                    vm_zone: task.vm_zone.clone(),
                                    title,
                                };
                            }
                        }
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('p') => {
                        self.cancel_visual();
                        let current = self.project_filter.map(|i| i + 1).unwrap_or(0);
                        self.input_mode = PlanInputMode::ProjectPicker { selected: current };
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('/') => {
                        self.cancel_visual();
                        // Snapshot cursor + fold state so Esc can fully
                        // undo whatever incremental search moved/unfolded.
                        self.search_return = Some((self.cursor.clone(), self.expanded_tasks.clone()));
                        self.search_status = None;
                        self.input_mode = PlanInputMode::Searching { query: String::new() };
                        return PlanAction::Consumed;
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::Char(' ') if key.modifiers.is_empty() => {
                    self.toggle_fold_at_cursor();
                    return PlanAction::Consumed;
                }
                // Bare n/N: step through the stored `/` search matches.
                // Only bound while a search is active so the keys stay
                // inert (Ignored) otherwise.
                KeyCode::Char('n') if key.modifiers.is_empty() && self.last_search.is_some() => {
                    self.jump_search(1);
                    return PlanAction::Consumed;
                }
                KeyCode::Char('N') if !has_alt && self.last_search.is_some() => {
                    self.jump_search(-1);
                    return PlanAction::Consumed;
                }
                KeyCode::PageDown => {
                    self.detail_scroll = self.detail_scroll.saturating_add(
                        (self.grid_rows_visible.get() as u16 / 3).max(1)
                    );
                    return PlanAction::Consumed;
                }
                KeyCode::PageUp => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(
                        (self.grid_rows_visible.get() as u16 / 3).max(1)
                    );
                    return PlanAction::Consumed;
                }
                KeyCode::Home => {
                    self.cursor.row = 0;
                    self.snap_cursor_to_selectable(1);
                    self.ensure_cursor_visible();
                    return PlanAction::Consumed;
                }
                KeyCode::End => {
                    if let Some(col) = self.cursor_visible_column() {
                        self.cursor.row = col.len().saturating_sub(1);
                    }
                    self.snap_cursor_to_selectable(-1);
                    self.ensure_cursor_visible();
                    return PlanAction::Consumed;
                }
                _ => {}
            }

            if self.visual_anchor.is_some() {
                if let KeyCode::Esc = key.code {
                    self.cancel_visual();
                    return PlanAction::Consumed;
                }
            }
        }
        PlanAction::Ignored
    }

    fn handle_editing_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) {
                match key.code {
                    KeyCode::Char('t') => return PlanAction::SwitchToSessions,
                    KeyCode::Char('q') => return PlanAction::Quit,
                    _ => {}
                }
            }
        }
        if let Some(ref mut editor) = self.editor {
            if !editor.exited {
                if let CrosstermEvent::Paste(text) = event {
                    let term_mode = *editor.term.lock().mode();
                    let data = if term_mode.contains(alacritty_terminal::term::TermMode::BRACKETED_PASTE) {
                        format!("\x1b[200~{}\x1b[201~", text).into_bytes()
                    } else {
                        text.as_bytes().to_vec()
                    };
                    let _ = editor.write(&data);
                    return PlanAction::Consumed;
                }
                let term_mode = *editor.term.lock().mode();
                if let Some(bytes) = crate::input::event_to_bytes(event, &term_mode) {
                    let _ = editor.write(&bytes);
                    return PlanAction::Consumed;
                }
            }
        }
        PlanAction::Consumed
    }

    fn handle_search_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let mut query = match &self.input_mode {
                PlanInputMode::Searching { query } => query.clone(),
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => {
                    // Cancel: restore the pre-search cursor + fold state.
                    // A previously committed search (if any) stays live.
                    if let Some((cursor, folds)) = self.search_return.take() {
                        self.cursor = cursor;
                        self.expanded_tasks = folds;
                        self.clamp_cursor();
                        self.ensure_cursor_visible();
                    }
                    self.input_mode = PlanInputMode::Normal;
                }
                KeyCode::Enter => {
                    // Accept: keep the cursor where incremental search
                    // left it, drop the restore snapshot, store the query
                    // for n/N + highlight.
                    self.search_return = None;
                    self.input_mode = PlanInputMode::Normal;
                    self.commit_search(&query);
                }
                KeyCode::Backspace => {
                    query.pop();
                    self.incremental_search(&query);
                    self.input_mode = PlanInputMode::Searching { query };
                }
                KeyCode::Char(c) => {
                    query.push(c);
                    self.incremental_search(&query);
                    self.input_mode = PlanInputMode::Searching { query };
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_new_task_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            // Snapshot the current state — title accumulates per keystroke,
            // parent_task_id / parent_name are stable for the lifetime of
            // the input session and just need to flow through to the
            // resulting `CreateTask` action.
            let (mut title, parent_task_id, parent_name) = match &self.input_mode {
                PlanInputMode::NewTask {
                    title,
                    parent_task_id,
                    parent_name,
                } => (title.clone(), parent_task_id.clone(), parent_name.clone()),
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => self.input_mode = PlanInputMode::Normal,
                KeyCode::Enter => {
                    if !title.trim().is_empty() {
                        self.input_mode = PlanInputMode::Normal;
                        return self.create_task(&title, parent_task_id);
                    }
                }
                KeyCode::Backspace => {
                    title.pop();
                    self.input_mode = PlanInputMode::NewTask {
                        title,
                        parent_task_id,
                        parent_name,
                    };
                }
                KeyCode::Char(c) => {
                    title.push(c);
                    self.input_mode = PlanInputMode::NewTask {
                        title,
                        parent_task_id,
                        parent_name,
                    };
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_new_header_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let mut text = match &self.input_mode {
                PlanInputMode::NewHeader { text } => text.clone(),
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => self.input_mode = PlanInputMode::Normal,
                KeyCode::Enter => {
                    if !text.trim().is_empty() {
                        self.input_mode = PlanInputMode::Normal;
                        self.insert_header(text.trim().to_string());
                    }
                }
                KeyCode::Backspace => { text.pop(); self.input_mode = PlanInputMode::NewHeader { text }; }
                KeyCode::Char(c) => { text.push(c); self.input_mode = PlanInputMode::NewHeader { text }; }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_editing_header_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let mut text = match &self.input_mode {
                PlanInputMode::EditingHeader { text } => text.clone(),
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => self.input_mode = PlanInputMode::Normal,
                KeyCode::Enter => {
                    if !text.trim().is_empty() {
                        self.input_mode = PlanInputMode::Normal;
                        self.update_header_at_cursor(text.trim().to_string());
                    }
                }
                KeyCode::Backspace => { text.pop(); self.input_mode = PlanInputMode::EditingHeader { text }; }
                KeyCode::Char(c) => { text.push(c); self.input_mode = PlanInputMode::EditingHeader { text }; }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_bulk_archive_confirm_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let project_idx = match &self.input_mode {
                PlanInputMode::BulkArchiveConfirm { project_idx, .. } => *project_idx,
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => {
                    self.input_mode = PlanInputMode::Normal;
                }
                KeyCode::Enter => {
                    self.input_mode = PlanInputMode::Normal;
                    let mut ids: Vec<String> = Vec::new();
                    if let Some(pd) = self.project_data.get_mut(project_idx) {
                        for task in &mut pd.tasks {
                            if task.status == PlanStatus::Done {
                                task.status = PlanStatus::Archived;
                                ids.push(task.id.clone());
                            }
                        }
                    }
                    if ids.is_empty() {
                        return PlanAction::Consumed;
                    }
                    // Cursor may have been on one of the just-archived tasks; snap to a still-visible row.
                    self.snap_cursor_to_selectable(1);
                    let mut fields = HashMap::new();
                    fields.insert("status".to_string(), serde_json::json!("archived"));
                    return PlanAction::BulkUpdateTasks { ids, fields };
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_new_project_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let (mut name, mut repo_url, field) = match &self.input_mode {
                PlanInputMode::NewProject { name, repo_url, field } => (name.clone(), repo_url.clone(), *field),
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => { self.input_mode = PlanInputMode::Normal; }
                KeyCode::Tab | KeyCode::BackTab => {
                    let next = if field == NewProjectField::Name { NewProjectField::RepoUrl } else { NewProjectField::Name };
                    // Auto-fill repo_url when tabbing away from name if repo_url is empty.
                    if field == NewProjectField::Name && repo_url.is_empty() && !name.trim().is_empty() {
                        repo_url = format!("https://github.com/Bigbadboybob/{}.git", name.trim());
                    }
                    self.input_mode = PlanInputMode::NewProject { name, repo_url, field: next };
                }
                KeyCode::Enter => {
                    let trimmed = name.trim().to_string();
                    if !trimmed.is_empty() {
                        // Default repo_url if still empty.
                        if repo_url.trim().is_empty() {
                            repo_url = format!("https://github.com/Bigbadboybob/{}.git", trimmed);
                        }
                        self.input_mode = PlanInputMode::Normal;
                        self.create_project(&trimmed, repo_url.trim());
                    }
                }
                KeyCode::Backspace => {
                    match field {
                        NewProjectField::Name => { name.pop(); }
                        NewProjectField::RepoUrl => { repo_url.pop(); }
                    }
                    self.input_mode = PlanInputMode::NewProject { name, repo_url, field };
                }
                KeyCode::Char(c) => {
                    match field {
                        NewProjectField::Name => { name.push(c); }
                        NewProjectField::RepoUrl => { repo_url.push(c); }
                    }
                    self.input_mode = PlanInputMode::NewProject { name, repo_url, field };
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_project_picker_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let selected = match self.input_mode {
                PlanInputMode::ProjectPicker { selected } => selected,
                _ => return PlanAction::Consumed,
            };
            let max = self.projects.len();
            match key.code {
                KeyCode::Esc => self.input_mode = PlanInputMode::Normal,
                KeyCode::Char('j') | KeyCode::Down => {
                    self.input_mode = PlanInputMode::ProjectPicker { selected: (selected + 1) % (max + 1) };
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.input_mode = PlanInputMode::ProjectPicker {
                        selected: if selected == 0 { max } else { selected - 1 },
                    };
                }
                KeyCode::Enter => {
                    self.input_mode = PlanInputMode::Normal;
                    if selected == 0 {
                        self.project_filter = None;
                    } else {
                        self.project_filter = Some(selected - 1);
                    }
                    self.rebuild_unified_cols();
                    self.clamp_cursor();
                    // The filter changes which columns exist, so the
                    // match list (and its ordering) must be rebuilt.
                    self.refresh_search_matches();
                    self.search_status = None;
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_workspace_picker_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        let (project_idx, task_idx, mut selected, mut engine) = match self.input_mode {
            PlanInputMode::WorkspacePicker {
                project_idx,
                task_idx,
                selected,
                engine,
            } => (project_idx, task_idx, selected, engine),
            _ => return PlanAction::Consumed,
        };
        let num_candidates = self.candidates_for(project_idx, task_idx).len();
        // Option 0 is always "New workspace"; options 1..=num_candidates are
        // existing workspaces.
        let total = num_candidates + 1;

        if let CrosstermEvent::Key(key) = event {
            match key.code {
                KeyCode::Esc => {
                    self.input_mode = PlanInputMode::Normal;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    selected = (selected + total - 1) % total;
                    self.input_mode = PlanInputMode::WorkspacePicker {
                        project_idx,
                        task_idx,
                        selected,
                        engine,
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1) % total;
                    self.input_mode = PlanInputMode::WorkspacePicker {
                        project_idx,
                        task_idx,
                        selected,
                        engine,
                    };
                }
                // Engine picker: only two options, so either arrow
                // toggles. j/k stay reserved for the workspace list.
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    engine = engine.cycle();
                    self.input_mode = PlanInputMode::WorkspacePicker {
                        project_idx,
                        task_idx,
                        selected,
                        engine,
                    };
                }
                KeyCode::Enter => {
                    if selected == 0 {
                        // Fall through to branch-input dialog (current flow),
                        // carrying the engine picked here.
                        let branch_text = self.project_data[project_idx].tasks[task_idx]
                            .branch
                            .clone()
                            .unwrap_or_default();
                        self.input_mode = PlanInputMode::LaunchConfirm {
                            project_idx,
                            task_idx,
                            branch_text,
                            engine,
                        };
                    } else {
                        // Bind task to the selected existing workspace.
                        let ws = {
                            let cands = self.candidates_for(project_idx, task_idx);
                            cands.get(selected - 1).map(|c| (*c).clone())
                        };
                        let Some(ws) = ws else {
                            self.input_mode = PlanInputMode::Normal;
                            return PlanAction::Consumed;
                        };
                        self.input_mode = PlanInputMode::Normal;
                        if let Some(pd) = self.project_data.get_mut(project_idx) {
                            if let Some(task) = pd.tasks.get_mut(task_idx) {
                                let prompt = compose_launch_prompt(
                                    &task.description,
                                    &task.prompt,
                                    &task.title,
                                );
                                task.status = PlanStatus::InProgress;
                                return PlanAction::LaunchTaskIntoWorkspace {
                                    workspace_id: ws.workspace_id,
                                    task_id: task.id.clone(),
                                    task_title: task.title.clone(),
                                    task_repo_url: task.repo_url.clone(),
                                    project: pd.project.name.clone(),
                                    prompt,
                                    // Sub-2a Finding #2: pin the parent
                                    // at action time so the launch site
                                    // initializes the local stub with
                                    // the correct edge.
                                    parent_task_id: task.parent_task_id.clone(),
                                    engine,
                                };
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_launch_confirm_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        if let CrosstermEvent::Key(key) = event {
            let (project_idx, task_idx, mut branch_text, mut engine) = match &self.input_mode {
                PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text, engine } => {
                    (*project_idx, *task_idx, branch_text.clone(), *engine)
                }
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => self.input_mode = PlanInputMode::Normal,
                KeyCode::Enter => {
                    self.input_mode = PlanInputMode::Normal;
                    if let Some(pd) = self.project_data.get_mut(project_idx) {
                        if let Some(task) = pd.tasks.get_mut(task_idx) {
                            let project = pd.project.name.clone();
                            let prompt = compose_launch_prompt(
                                &task.description,
                                &task.prompt,
                                &task.title,
                            );
                            let slug = task.slug.clone();
                            // Branch field: "." → in-place (main repo, no
                            // worktree/branch); "" → new worktree from HEAD;
                            // other → new worktree from that base branch.
                            let trimmed = branch_text.trim();
                            let in_place = trimmed == ".";
                            let branch = if trimmed.is_empty() || in_place {
                                None
                            } else {
                                Some(trimmed.to_string())
                            };
                            let task_id = task.id.clone();
                            let parent_task_id = task.parent_task_id.clone();
                            task.status = PlanStatus::InProgress;
                            return PlanAction::LaunchTask {
                                project,
                                slug,
                                prompt,
                                branch,
                                autostart: false,
                                task_id,
                                // Sub-2a Finding #2: see LaunchTaskIntoWorkspace.
                                parent_task_id,
                                in_place,
                                engine,
                            };
                        }
                    }
                }
                // Branch is a free-text field, so the engine toggle can't
                // use letters — arrows (unused by this dialog) and Tab do it.
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    engine = engine.cycle();
                    self.input_mode = PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text, engine };
                }
                KeyCode::Backspace => {
                    branch_text.pop();
                    self.input_mode = PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text, engine };
                }
                KeyCode::Char(c) => {
                    branch_text.push(c);
                    self.input_mode = PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text, engine };
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    // ── Task / Grid Operations ──────────────────────────────

    fn sort_column_by_status(&mut self) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };

        let tasks = &self.project_data[pi].tasks;
        let col = &self.project_data[pi].layout.columns[ci];

        let task_entries: Vec<(usize, u8, GridItem)> = col.iter().enumerate()
            .filter_map(|(ri, item)| {
                if let GridItem::Task(slug) = item {
                    let key = tasks.iter()
                        .find(|t| t.slug == *slug)
                        .map(|t| match t.status {
                            PlanStatus::Done => 0,
                            PlanStatus::InProgress => 1,
                            PlanStatus::Backlog => 2,
                            PlanStatus::Draft => 3,
                            PlanStatus::Archived => 4,
                        })
                        .unwrap_or(5);
                    Some((ri, key, item.clone()))
                } else {
                    None
                }
            })
            .collect();

        if task_entries.windows(2).all(|w| w[0].1 <= w[1].1) {
            return;
        }

        let task_positions: Vec<usize> = task_entries.iter().map(|(ri, _, _)| *ri).collect();

        let mut indices: Vec<usize> = (0..task_entries.len()).collect();
        indices.sort_by_key(|&i| task_entries[i].1);
        let sorted: Vec<GridItem> = indices.iter().map(|&i| task_entries[i].2.clone()).collect();

        let col = &mut self.project_data[pi].layout.columns[ci];
        for (slot, item) in task_positions.iter().zip(sorted) {
            col[*slot] = item;
        }

        self.save_project_layout(pi);
        self.recompute_conflicts();
    }

    fn cycle_status(&mut self, forward: bool) -> PlanAction {
        if let Some((pi, ti)) = self.selected_task_loc() {
            let task = &mut self.project_data[pi].tasks[ti];
            let new_status = if forward { task.status.next() } else { task.status.prev() };
            if new_status != task.status {
                task.status = new_status;
                let id = task.id.clone();
                let status_str = task.status.as_str().to_string();
                let mut fields = HashMap::new();
                fields.insert("status".to_string(), serde_json::json!(status_str));
                return PlanAction::UpdateTask { id, fields, status_msg: None };
            }
        }
        PlanAction::Consumed
    }

    fn unlaunch_task(&mut self) -> PlanAction {
        let (pi, ti) = match self.selected_task_loc() {
            Some(v) => v,
            None => return PlanAction::Consumed,
        };
        let task = &mut self.project_data[pi].tasks[ti];
        if task.status != PlanStatus::InProgress {
            return PlanAction::Consumed;
        }
        task.status = PlanStatus::Backlog;
        PlanAction::UnlaunchTask { task_id: task.id.clone() }
    }

    fn cycle_status_to_done(&mut self) -> PlanAction {
        if let Some((pi, ti)) = self.selected_task_loc() {
            let task = &mut self.project_data[pi].tasks[ti];
            if task.status != PlanStatus::Done {
                task.status = PlanStatus::Done;
                let id = task.id.clone();
                let mut fields = HashMap::new();
                fields.insert("status".to_string(), serde_json::json!("done"));
                return PlanAction::UpdateTask { id, fields, status_msg: None };
            }
        }
        PlanAction::Consumed
    }

    fn accept_proposal(&mut self) -> PlanAction {
        if let Some((pi, ti)) = self.selected_task_loc() {
            let task = &mut self.project_data[pi].tasks[ti];
            if task.source == "claude" {
                task.source = "user".to_string();
                let id = task.id.clone();
                let mut fields = HashMap::new();
                fields.insert("source".to_string(), serde_json::json!("user"));
                return PlanAction::UpdateTask { id, fields, status_msg: None };
            }
        }
        PlanAction::Consumed
    }

    fn reorder_task(&mut self, direction: i32) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let col = &self.project_data[pi].layout.columns[ci];
        if col.is_empty() { return; }

        if let Some((range_start, range_end)) = self.visual_range() {
            self.move_visual_block(pi, ci, range_start, range_end, direction);
        } else {
            match self.cursor_raw_idx() {
                Some(ri) => {
                    let target = match self.next_visible_row(pi, ci, ri, direction) {
                        Some(t) => t,
                        None => {
                            if direction <= 0 { return; }
                            let col = &mut self.project_data[pi].layout.columns[ci];
                            col.push(GridItem::Empty);
                            col.len() - 1
                        }
                    };
                    self.project_data[pi].layout.columns[ci].swap(ri, target);
                    // Recompute cursor row in visible-row space — find the
                    // swapped item's new raw_idx in the new visible list.
                    if let Some(rows) = self.cursor_visible_column() {
                        if let Some(idx) = rows.iter().position(|r| matches!(&r.kind, VisibleRowKind::Layout { raw_idx, .. } if *raw_idx == target)) {
                            self.cursor.row = idx;
                        }
                    }
                }
                None => {
                    // Synthetic subtask row — swap with adjacent sibling
                    // under the same parent. Bails (no layout change) at
                    // the sibling-list boundary so subtasks stay confined
                    // to under-parent slots.
                    if !self.reorder_subtask_within_siblings(pi, direction) {
                        return;
                    }
                }
            }
        }
        self.save_project_layout(pi);
        self.recompute_conflicts();
        self.ensure_cursor_visible();
    }

    /// Swap the focused subtask with its adjacent sibling under the same
    /// parent. The swap is performed on the raw `GridItem::Task` slots in
    /// the layout (across columns if needed) since sibling order is encoded
    /// by column-walk position. Returns true on swap, false when the
    /// subtask is at the sibling-list boundary in `direction` or when a
    /// state lookup fails.
    fn reorder_subtask_within_siblings(&mut self, pi: usize, direction: i32) -> bool {
        let row = match self.cursor_visible_row() { Some(r) => r, None => return false };
        let slug = match row.kind {
            VisibleRowKind::Subtask { slug } => slug,
            _ => return false,
        };
        let pd = match self.project_data.get(pi) { Some(p) => p, None => return false };
        let parent_id = match pd.tasks.iter().find(|t| t.slug == slug)
            .and_then(|t| t.parent_task_id.clone())
        {
            Some(p) => p,
            None => return false,
        };
        if !pd.tasks.iter().any(|t| t.id == parent_id) { return false; }

        // Build sibling slug list in column-walk order. Mirrors the
        // children_of construction in visible_rows_for_column, and
        // honours show_archived so the swap target matches what the
        // user sees.
        let task_by_slug: HashMap<&str, &PlanTask> = pd.tasks.iter()
            .map(|t| (t.slug.as_str(), t))
            .collect();
        let mut siblings: Vec<String> = Vec::new();
        for col in &pd.layout.columns {
            for item in col {
                if let GridItem::Task(s) = item {
                    let t = match task_by_slug.get(s.as_str()) { Some(t) => *t, None => continue };
                    if t.parent_task_id.as_deref() != Some(&parent_id) { continue; }
                    if !self.show_archived && t.status == PlanStatus::Archived { continue; }
                    siblings.push(s.clone());
                }
            }
        }

        let cur_idx = match siblings.iter().position(|s| s == &slug) {
            Some(i) => i,
            None => return false,
        };
        let tgt_idx = cur_idx as i32 + direction;
        if tgt_idx < 0 || tgt_idx as usize >= siblings.len() { return false; }
        let other = siblings[tgt_idx as usize].clone();

        let find_pos = |target: &str| -> Option<(usize, usize)> {
            for (ci, col) in pd.layout.columns.iter().enumerate() {
                for (ri, item) in col.iter().enumerate() {
                    if let GridItem::Task(s) = item {
                        if s == target { return Some((ci, ri)); }
                    }
                }
            }
            None
        };
        let cur_pos = match find_pos(&slug) { Some(p) => p, None => return false };
        let oth_pos = match find_pos(&other) { Some(p) => p, None => return false };

        // Drop the immutable borrow before mutating.
        let _ = pd;
        let cols = &mut self.project_data[pi].layout.columns;
        if cur_pos.0 == oth_pos.0 {
            cols[cur_pos.0].swap(cur_pos.1, oth_pos.1);
        } else {
            let cur_item = std::mem::replace(&mut cols[cur_pos.0][cur_pos.1], GridItem::Empty);
            let oth_item = std::mem::replace(&mut cols[oth_pos.0][oth_pos.1], GridItem::Empty);
            cols[cur_pos.0][cur_pos.1] = oth_item;
            cols[oth_pos.0][oth_pos.1] = cur_item;
        }

        // Refocus the moved subtask in the rebuilt visible-row list.
        if let Some(rows) = self.cursor_visible_column() {
            if let Some(idx) = rows.iter().position(|r| matches!(&r.kind, VisibleRowKind::Subtask { slug: s } if s == &slug)) {
                self.cursor.row = idx;
            }
        }
        true
    }

    /// Walk `direction` from `from_row` and return the first row whose item
    /// would actually render — i.e. skip archived task rows when
    /// `show_archived` is off, and skip subtasks that render under a
    /// parent. Returns `None` if there is no such row in that direction.
    fn next_visible_row(&self, pi: usize, ci: usize, from_row: usize, direction: i32) -> Option<usize> {
        if direction == 0 { return None; }
        let pd = self.project_data.get(pi)?;
        let col = pd.layout.columns.get(ci)?;
        let len = col.len() as i32;
        let mut t = from_row as i32 + direction;
        let task_by_id: std::collections::HashSet<&str> = pd.tasks.iter().map(|t| t.id.as_str()).collect();
        while t >= 0 && t < len {
            let item = &col[t as usize];
            let visible = match item {
                GridItem::Task(slug) => {
                    let task = pd.tasks.iter().find(|tt| tt.slug == *slug);
                    let archived = task.map_or(false, |tt| tt.status == PlanStatus::Archived);
                    let parented = task.and_then(|tt| tt.parent_task_id.as_deref())
                        .map_or(false, |pid| task_by_id.contains(pid));
                    (self.show_archived || !archived) && !parented
                }
                _ => true,
            };
            if visible {
                return Some(t as usize);
            }
            t += direction;
        }
        None
    }

    fn move_visual_block(&mut self, pi: usize, ci: usize, start: usize, end: usize, direction: i32) {
        // Visual block move operates on a contiguous run of raw layout
        // items. If any row in [start..=end] is a synthetic subtask
        // (or a parented task hidden from raw view), the run isn't
        // contiguous in raw space and the operation can't be defined
        // unambiguously — bail. Translate visible bounds to raw.
        let rows = self.visible_rows_for_column(pi, ci);
        let raw_start = match rows.get(start).map(|r| &r.kind) {
            Some(VisibleRowKind::Layout { raw_idx, .. }) => *raw_idx,
            _ => return,
        };
        let raw_end = match rows.get(end).map(|r| &r.kind) {
            Some(VisibleRowKind::Layout { raw_idx, .. }) => *raw_idx,
            _ => return,
        };
        if raw_end - raw_start != end - start { return; }
        if direction > 0 {
            let below = match self.next_visible_row(pi, ci, raw_end, 1) {
                Some(b) => b,
                None => {
                    let col = &mut self.project_data[pi].layout.columns[ci];
                    col.push(GridItem::Empty);
                    col.len() - 1
                }
            };
            let col = &mut self.project_data[pi].layout.columns[ci];
            let item = col.remove(below);
            col.insert(raw_start, item);
            self.cursor.row += 1;
            if let Some(ref mut anchor) = self.visual_anchor {
                *anchor += 1;
            }
        } else {
            let above = match self.next_visible_row(pi, ci, raw_start, -1) {
                Some(a) => a,
                None => return,
            };
            let col = &mut self.project_data[pi].layout.columns[ci];
            let item = col.remove(above);
            col.insert(raw_end, item);
            self.cursor.row -= 1;
            if let Some(ref mut anchor) = self.visual_anchor {
                *anchor -= 1;
            }
        }
    }

    fn move_task_to_column(&mut self, direction: i32) {
        if self.linear_mode || self.unified_cols.is_empty() { return; }
        let (src_pi, src_ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let target_gcol = self.cursor.col as i32 + direction;
        if target_gcol < 0 || target_gcol >= self.unified_cols.len() as i32 { return; }
        let target_gcol = target_gcol as usize;
        let (dst_pi, dst_ci) = self.unified_cols[target_gcol];
        if src_pi != dst_pi { return; }

        if let Some((range_start, range_end)) = self.visual_range() {
            // Translate visible-row visual range to raw indices. Bail
            // if any row is a synthetic subtask or the raw indices
            // aren't contiguous.
            let rows = self.visible_rows_for_column(src_pi, src_ci);
            let raw_start = match rows.get(range_start).map(|r| &r.kind) {
                Some(VisibleRowKind::Layout { raw_idx, .. }) => *raw_idx,
                _ => return,
            };
            let raw_end = match rows.get(range_end).map(|r| &r.kind) {
                Some(VisibleRowKind::Layout { raw_idx, .. }) => *raw_idx,
                _ => return,
            };
            if raw_end - raw_start != range_end - range_start { return; }
            let src_len = self.project_data[src_pi].layout.columns[src_ci].len();
            if raw_end >= src_len { return; }
            let items: Vec<GridItem> = self.project_data[src_pi].layout.columns[src_ci]
                .drain(raw_start..=raw_end)
                .collect();
            let dst_len = self.project_data[dst_pi].layout.columns[dst_ci].len();
            let insert_at = raw_start.min(dst_len);
            for (offset, item) in items.into_iter().enumerate() {
                self.project_data[dst_pi].layout.columns[dst_ci].insert(insert_at + offset, item);
            }
            // Reposition cursor/anchor in the new visible-rows projection.
            self.cursor.col = target_gcol;
            self.cursor.row = range_start;
            if let Some(ref mut anchor) = self.visual_anchor {
                let anchor_offset = anchor.saturating_sub(range_start);
                *anchor = range_start + anchor_offset;
            }
        } else {
            // Single-row move: only valid on a raw Layout row of
            // kind Task or Header. Subtasks (synthetic) bail.
            let raw_idx = match self.cursor_raw_idx() { Some(r) => r, None => return };
            match self.project_data[src_pi].layout.columns[src_ci].get(raw_idx) {
                Some(GridItem::Task(_)) | Some(GridItem::Header(_)) => {}
                _ => return,
            }
            let item = self.project_data[src_pi].layout.columns[src_ci].remove(raw_idx);
            let insert_at = raw_idx.min(self.project_data[dst_pi].layout.columns[dst_ci].len());
            self.project_data[dst_pi].layout.columns[dst_ci].insert(insert_at, item);
            self.cursor.col = target_gcol;
            // After insertion, recompute cursor.row in the new column.
            if let Some(rows) = self.cursor_visible_column() {
                if let Some(idx) = rows.iter().position(|r| matches!(&r.kind, VisibleRowKind::Layout { raw_idx, .. } if *raw_idx == insert_at)) {
                    self.cursor.row = idx;
                }
            }
        }

        self.save_project_layout(src_pi);
        self.recompute_conflicts();
        self.clamp_cursor();
    }

    /// Anchor for ops that target the cursor's exact raw item (e.g.
    /// `remove_separator`, `update_header_at_cursor`). Returns `None`
    /// for synthetic subtask rows — they have no raw slot, so the
    /// op should no-op rather than silently mutate something else.
    fn anchor_raw_idx(&self) -> Option<usize> {
        let row = self.cursor_visible_row()?;
        match row.kind {
            VisibleRowKind::Layout { raw_idx, .. } => Some(raw_idx),
            VisibleRowKind::Subtask { .. } => None,
        }
    }

    /// Anchor for inserts (new task / separator / header land at
    /// `result + 1`). For a synthetic subtask cursor row, walks up the
    /// `parent_task_id` chain to the top-level ancestor in the
    /// cursor's column and returns ITS raw idx, so inserts drop in
    /// just after the parent's subtree instead of falling through to
    /// end-of-column. By construction, the top-level ancestor of any
    /// visible subtask row in column `ci` lives in `ci` — that's why
    /// the subtree renders there at all.
    fn insert_anchor_raw_idx(&self) -> Option<usize> {
        let row = self.cursor_visible_row()?;
        let slug = match row.kind {
            VisibleRowKind::Layout { raw_idx, .. } => return Some(raw_idx),
            VisibleRowKind::Subtask { slug } => slug,
        };
        let (pi, ci) = *self.unified_cols.get(self.cursor.col)?;
        let pd = self.project_data.get(pi)?;
        let by_id: HashMap<&str, &PlanTask> =
            pd.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
        let mut cur = pd.tasks.iter().find(|t| t.slug == slug)?;
        while let Some(pid) = cur.parent_task_id.as_deref() {
            match by_id.get(pid).copied() {
                Some(parent) => cur = parent,
                None => break,
            }
        }
        let top_slug = cur.slug.as_str();
        pd.layout.columns.get(ci)?
            .iter()
            .position(|item| matches!(item, GridItem::Task(s) if s == top_slug))
    }

    fn insert_separator(&mut self) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let raw = match self.anchor_raw_idx() { Some(r) => r, None => return };
        let insert_at = (raw + 1).min(self.project_data[pi].layout.columns[ci].len());
        self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Separator);
        self.save_project_layout(pi);
    }

    fn insert_empty(&mut self) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let raw = match self.anchor_raw_idx() { Some(r) => r, None => return };
        let insert_at = (raw + 1).min(self.project_data[pi].layout.columns[ci].len());
        self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Empty);
        self.save_project_layout(pi);
    }

    fn insert_header(&mut self, text: String) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let raw = match self.anchor_raw_idx() { Some(r) => r, None => return };
        let insert_at = (raw + 1).min(self.project_data[pi].layout.columns[ci].len());
        self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Header(text));
        self.save_project_layout(pi);
    }

    fn update_header_at_cursor(&mut self, text: String) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let raw = match self.anchor_raw_idx() { Some(r) => r, None => return };
        if let Some(item) = self.project_data[pi].layout.columns[ci].get_mut(raw) {
            if matches!(item, GridItem::Header(_)) {
                *item = GridItem::Header(text);
                self.save_project_layout(pi);
            }
        }
    }

    fn remove_separator(&mut self) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let raw = match self.anchor_raw_idx() { Some(r) => r, None => return };
        if matches!(self.project_data[pi].layout.columns[ci].get(raw), Some(GridItem::Separator | GridItem::Empty | GridItem::Header(_))) {
            self.project_data[pi].layout.columns[ci].remove(raw);
            self.save_project_layout(pi);
            self.clamp_cursor();
        }
    }

    fn add_column(&mut self) {
        if self.linear_mode { return; }
        let pi = self.cursor_project_idx().unwrap_or(0);
        if pi < self.project_data.len() {
            self.project_data[pi].layout.columns.push(vec![]);
            self.save_project_layout(pi);
            self.rebuild_unified_cols();
        }
    }

    fn remove_column(&mut self) {
        if self.linear_mode || self.unified_cols.is_empty() { return; }
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        if self.project_data[pi].layout.columns[ci].is_empty() {
            self.project_data[pi].layout.columns.remove(ci);
            self.save_project_layout(pi);
            self.rebuild_unified_cols();
            self.clamp_cursor();
        }
    }

    fn create_task(
        &mut self,
        title: &str,
        parent_task_id: Option<String>,
    ) -> PlanAction {
        // Determine the project for the new task. For subtasks we honor
        // the parent's project explicitly so `parent_task_id`-walks line
        // up; for top-level tasks fall back to the cursor's column.
        let pi = if let Some(parent_id) = parent_task_id.as_deref() {
            self.project_data
                .iter()
                .position(|pd| pd.tasks.iter().any(|t| t.id == parent_id))
                .or_else(|| self.cursor_project_idx())
        } else {
            self.cursor_project_idx()
                .or_else(|| self.unified_cols.first().map(|(pi, _)| *pi))
        }
        .unwrap_or(0);

        if pi >= self.project_data.len() {
            return PlanAction::Consumed;
        }

        let project = self.project_data[pi].project.name.clone();
        // Prefer the project's persisted repo_url (written by
        // `create_project` to `<projects_dir>/<name>/repo_url`) so
        // non-github / forked / renamed remotes survive task
        // creation. Fall back to the hardcoded github default only
        // when nothing is stored — mostly for fresh projects that
        // pre-date the stored field.
        let stored = self.project_data[pi].project.repo_url.trim();
        let repo_url = if stored.is_empty() {
            repo_url_for_project(&project)
        } else {
            stored.to_string()
        };

        // Subtasks default to inherit-mode worktree (share parent's). The
        // user can change this on the API row later if they want a
        // separate branch worktree before launch.
        let worktree_mode = if parent_task_id.is_some() {
            Some("inherit".to_string())
        } else {
            None
        };

        PlanAction::CreateTask {
            project,
            repo_url,
            name: title.to_string(),
            description: title.to_string(),
            status: "draft".to_string(),
            parent_task_id,
            worktree_mode,
        }
    }

    fn delete_task(&mut self) -> PlanAction {
        let (pi, ti) = match self.selected_task_loc() {
            Some(v) => v,
            None => return PlanAction::Consumed,
        };
        let task = &self.project_data[pi].tasks[ti];
        let id = task.id.clone();
        let slug = task.slug.clone();

        // Remove from local layout immediately for responsive UI.
        for col in &mut self.project_data[pi].layout.columns {
            col.retain(|item| !matches!(item, GridItem::Task(s) if s == &slug));
        }
        self.save_project_layout(pi);
        self.project_data[pi].tasks.remove(ti);
        self.rebuild_unified_cols();
        self.recompute_conflicts();
        self.clamp_cursor();

        PlanAction::DeleteTask { id }
    }

    fn start_editor(&mut self) -> PlanAction {
        let (pi, ti) = match self.selected_task_loc() {
            Some(v) => v,
            None => return PlanAction::Consumed,
        };
        let task = &self.project_data[pi].tasks[ti];
        let slug = task.slug.clone();

        // Resolve current parent's slug for pre-fill. Look up within the
        // same project — cross-project parents aren't allowed and we
        // ignore any orphan link (parent_task_id pointing outside the
        // current project's task list) here.
        let parent_slug = task.parent_task_id.as_ref().and_then(|pid| {
            self.project_data[pi]
                .tasks
                .iter()
                .find(|t| t.id == *pid)
                .map(|t| t.slug.clone())
        });

        // Write task to temp file for editing.
        let temp_path = match write_temp_task(task, parent_slug.as_deref()) {
            Some(p) => p,
            None => return PlanAction::Consumed,
        };

        let editor_cmd = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
        let parts: Vec<&str> = editor_cmd.split_whitespace().collect();
        let program = parts[0];
        let mut args: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
        args.push(temp_path.to_string_lossy().to_string());

        let (cols, rows) = self.last_editor_size;
        if let Ok(s) = Session::new(program, &args, cols, rows, None, Default::default(), None) {
            self.editing_slug = Some(slug);
            self.editing_project_idx = Some(pi);
            self.editing_temp_path = Some(temp_path);
            self.editor = Some(s);
            self.input_mode = PlanInputMode::Editing;
        }
        PlanAction::Consumed
    }

    fn stop_editor(&mut self) -> PlanAction {
        self.editor = None;
        self.input_mode = PlanInputMode::Normal;

        let mut action = PlanAction::Consumed;

        if let (Some(slug), Some(pi), Some(ref temp_path)) = (
            self.editing_slug.clone(),
            self.editing_project_idx,
            &self.editing_temp_path.clone(),
        ) {
            if let Some(parsed) = parse_temp_task(temp_path) {
                // Resolve the parent reference first (immutable borrow on
                // the project's task list) before taking the mutable
                // borrow on the focused task to update local state.
                let current_id = self.project_data.get(pi)
                    .and_then(|pd| pd.tasks.iter().find(|t| t.slug == slug))
                    .map(|t| t.id.clone());

                // Deleting the `parent:` line on a task that has a parent
                // is an explicit detach — promote it to a clear. (Without
                // this, line-deletion parses as `Absent`/"no change", so the
                // parent stuck and re-rendered on the next refresh.)
                let current_has_parent = current_id.as_deref().map_or(false, |id| {
                    self.project_data
                        .get(pi)
                        .and_then(|pd| pd.tasks.iter().find(|t| t.id == id))
                        .map_or(false, |t| t.parent_task_id.is_some())
                });
                let parent_update =
                    effective_parent_update(&parsed.parent, current_has_parent);

                let parent_patch = current_id.as_deref().and_then(|id| {
                    self.project_data.get(pi).map(|pd| {
                        resolve_parent_patch(&pd.tasks, id, &parent_update)
                    })
                });

                let (parent_field, status_msg): (Option<serde_json::Value>, Option<String>) =
                    match parent_patch {
                        Some(Ok(v)) => (v, None),
                        Some(Err(msg)) => (None, Some(msg)),
                        None => (None, None),
                    };

                // Resolve the new parent_task_id for local state. We need
                // the actual `Option<String>` to write back onto the
                // PlanTask — Cleared → None, Set → Some(id), Absent →
                // leave untouched.
                let local_parent_update: Option<Option<String>> = match (&parent_update, &parent_field) {
                    (FieldUpdate::Absent, _) => None,
                    (_, None) => None, // validation error: don't touch local state
                    (FieldUpdate::Cleared, Some(_)) => Some(None),
                    (FieldUpdate::Set(_), Some(serde_json::Value::String(id))) => {
                        Some(Some(id.clone()))
                    }
                    (FieldUpdate::Set(_), Some(_)) => None,
                };

                if let Some(task) = self.project_data.get_mut(pi)
                    .and_then(|pd| pd.tasks.iter_mut().find(|t| t.slug == slug))
                {
                    // Update local state. For FieldUpdate-tracked fields,
                    // Absent leaves the previous value untouched so opening
                    // the editor on a row where these fields weren't even
                    // emitted (because they were already empty) doesn't
                    // accidentally rewrite them.
                    task.title = parsed.title.clone();
                    task.status = PlanStatus::from_str(&parsed.status);
                    match &parsed.difficulty {
                        FieldUpdate::Absent => {}
                        FieldUpdate::Cleared => task.difficulty = None,
                        FieldUpdate::Set(d) => task.difficulty = Some(*d),
                    }
                    match &parsed.depends {
                        FieldUpdate::Absent => {}
                        FieldUpdate::Cleared => task.depends.clear(),
                        FieldUpdate::Set(v) => task.depends = v.clone(),
                    }
                    match &parsed.branch {
                        FieldUpdate::Absent => {}
                        FieldUpdate::Cleared => task.branch = None,
                        FieldUpdate::Set(b) => task.branch = Some(b.clone()),
                    }
                    if let Some(p) = local_parent_update {
                        task.parent_task_id = p;
                    }
                    task.description = parsed.description.clone();
                    task.prompt = parsed.prompt.clone();

                    let mut fields = build_patch_fields(&parsed);
                    if let Some(v) = parent_field {
                        fields.insert("parent_task_id".to_string(), v);
                    }

                    action = PlanAction::UpdateTask {
                        id: task.id.clone(),
                        fields,
                        status_msg,
                    };
                }
            }
            // Clean up temp file.
            let _ = std::fs::remove_file(temp_path);
        }

        self.editing_slug = None;
        self.editing_project_idx = None;
        self.editing_temp_path = None;
        self.recompute_conflicts();
        action
    }

    /// Workspace candidates visible for launching `task_idx` of `project_idx`
    /// — filtered to those in the same repo so the worktree is meaningful.
    fn candidates_for(&self, project_idx: usize, task_idx: usize) -> Vec<&WorkspaceCandidate> {
        let Some(pd) = self.project_data.get(project_idx) else {
            return vec![];
        };
        let Some(task) = pd.tasks.get(task_idx) else {
            return vec![];
        };
        let task_repo = normalize_repo_url(&task.repo_url);
        self.workspace_candidates
            .iter()
            .filter(|c| {
                c.repo_url
                    .as_deref()
                    .map_or(false, |u| normalize_repo_url(u) == task_repo)
            })
            .collect()
    }

    fn start_launch(&mut self) -> PlanAction {
        if let Some((pi, ti)) = self.selected_task_loc() {
            self.input_mode = PlanInputMode::WorkspacePicker {
                project_idx: pi,
                task_idx: ti,
                selected: 0,
                // Fresh default per launch — the engine choice is
                // deliberately not sticky.
                engine: LaunchEngine::default(),
            };
        }
        PlanAction::Consumed
    }

    /// Sync the list of open workspaces for the picker. Called by App each
    /// event cycle before the picker is drawn or handled.
    pub fn set_workspace_candidates(&mut self, candidates: Vec<WorkspaceCandidate>) {
        self.workspace_candidates = candidates;
    }

    // ── Search ──────────────────────────────────────────────

    /// Task IDs matching `query`, in board walk order: unified columns
    /// left-to-right, rows top-to-bottom within each, with every parent
    /// treated as unfolded so folded subtasks still participate (the
    /// jump auto-unfolds their ancestors). Rows the projection hides
    /// for real (archived without A-V, filtered-out projects) can't
    /// match. Case-insensitive over title + description.
    fn compute_search_matches(&self, query: &str) -> Vec<String> {
        let q = query.to_lowercase();
        if q.is_empty() {
            return vec![];
        }
        let mut out = Vec::new();
        for &(pi, ci) in &self.unified_cols {
            let rows = self.visible_rows_for_column_opts(pi, ci, true);
            for row in &rows {
                let slug = match visible_row_slug(row) { Some(s) => s, None => continue };
                if let Some(task) = self.project_data[pi].tasks.iter().find(|t| t.slug == slug) {
                    if task_matches_query(task, &q) {
                        out.push(task.id.clone());
                    }
                }
            }
        }
        out
    }

    /// Recompute `search_matches` from `last_search` over the current
    /// board. Called at every use site (n/N press) and after board
    /// mutations (API refresh, project-filter change, archive toggle),
    /// so stale task IDs from deleted rows or a project switch never
    /// linger — invalidation is recompute, not clearing, which keeps
    /// n/N working across refreshes.
    fn refresh_search_matches(&mut self) {
        self.search_matches = match &self.last_search {
            Some(q) => self.compute_search_matches(&q.clone()),
            None => vec![],
        };
    }

    /// Live jump while typing in the `/` prompt: cursor follows the
    /// first match of the draft query; an empty or unmatched draft
    /// snaps back to the pre-search position.
    fn incremental_search(&mut self, query: &str) {
        let first = self.compute_search_matches(query).into_iter().next();
        match first {
            Some(id) => { self.jump_to_task_id(&id); }
            None => {
                if let Some((cursor, folds)) = self.search_return.clone() {
                    self.cursor = cursor;
                    self.expanded_tasks = folds;
                    self.clamp_cursor();
                    self.ensure_cursor_visible();
                }
            }
        }
    }

    /// Enter in the `/` prompt: persist the query for n/N + highlight.
    /// An empty query clears any previous search.
    fn commit_search(&mut self, query: &str) {
        if query.is_empty() {
            self.last_search = None;
            self.search_matches.clear();
            self.search_status = None;
            return;
        }
        self.last_search = Some(query.to_string());
        self.refresh_search_matches();
        let len = self.search_matches.len();
        if len == 0 {
            self.search_status = Some(format!("no matches: {}", query));
            return;
        }
        // Incremental search normally already parked the cursor on the
        // first match; if not (e.g. data shifted mid-prompt), jump now.
        let pos = match self.cursor_task_id().and_then(|id| self.search_matches.iter().position(|m| *m == id)) {
            Some(p) => p,
            None => {
                let id = self.search_matches[0].clone();
                self.jump_to_task_id(&id);
                0
            }
        };
        self.search_status = Some(format!("match {}/{}: {}", pos + 1, len, query));
    }

    /// Bare n/N: wrap-step through the committed search's matches.
    fn jump_search(&mut self, direction: i32) {
        let query = match &self.last_search {
            Some(q) => q.clone(),
            None => return,
        };
        self.cancel_visual();
        self.refresh_search_matches();
        let len = self.search_matches.len();
        let current = self.cursor_task_id().and_then(|id| self.search_matches.iter().position(|m| *m == id));
        match next_search_index(len, current, direction) {
            Some(next) => {
                let id = self.search_matches[next].clone();
                if self.jump_to_task_id(&id) {
                    self.search_status = Some(format!("match {}/{}: {}", next + 1, len, query));
                }
            }
            None => {
                self.search_status = Some(format!("no matches: {}", query));
            }
        }
    }

    /// ID of the task under the cursor, if the cursor is on a task row.
    fn cursor_task_id(&self) -> Option<String> {
        let (task, _) = self.selected_task()?;
        Some(task.id.clone())
    }

    /// Move the cursor to a task by ID, auto-unfolding its ancestor
    /// chain first so folded subtasks become reachable (`Space`'s
    /// `expanded_tasks` mechanism). Returns false when the task isn't
    /// on the visible board (deleted, filtered project, hidden
    /// archived) — the cursor stays put.
    fn jump_to_task_id(&mut self, task_id: &str) -> bool {
        // Resolve project + slug + ancestor IDs (cycle-guarded).
        let mut target: Option<(usize, String)> = None;
        let mut ancestors: Vec<String> = Vec::new();
        for (pi, pd) in self.project_data.iter().enumerate() {
            if let Some(task) = pd.tasks.iter().find(|t| t.id == task_id) {
                let mut seen: HashSet<String> = HashSet::new();
                let mut cur = task.parent_task_id.clone();
                while let Some(pid) = cur {
                    if !seen.insert(pid.clone()) {
                        break;
                    }
                    match pd.tasks.iter().find(|p| p.id == pid) {
                        Some(parent) => {
                            ancestors.push(parent.id.clone());
                            cur = parent.parent_task_id.clone();
                        }
                        None => break,
                    }
                }
                target = Some((pi, task.slug.clone()));
                break;
            }
        }
        let (pi, slug) = match target {
            Some(t) => t,
            None => return false,
        };
        for id in ancestors {
            self.expanded_tasks.insert(id);
        }
        for (gi, &(p, c)) in self.unified_cols.iter().enumerate() {
            if p != pi {
                continue;
            }
            let rows = self.visible_rows_for_column(p, c);
            if let Some(ri) = rows.iter().position(|r| visible_row_slug(r) == Some(slug.as_str())) {
                if (gi, ri) != (self.cursor.col, self.cursor.row) {
                    self.detail_scroll = 0;
                }
                self.cursor.col = gi;
                self.cursor.row = ri;
                self.ensure_cursor_visible();
                return true;
            }
        }
        false
    }

    /// The query rows should highlight against: the live draft while
    /// the `/` prompt is open, else the committed search. Lowercased;
    /// `None` when neither is active.
    fn active_search_query(&self) -> Option<String> {
        match &self.input_mode {
            PlanInputMode::Searching { query } if !query.is_empty() => Some(query.to_lowercase()),
            _ => self.last_search.as_deref().filter(|q| !q.is_empty()).map(str::to_lowercase),
        }
    }

    /// Build the title spans for a (possibly) search-matched row.
    /// Matching substrings inside the visible title render ATTN+bold;
    /// a task that matches only via its description (or whose title
    /// match got truncated away) gets a whole-title ATTN tint so it's
    /// still discoverable. Non-matches inherit the row style via a
    /// plain span.
    fn push_title_spans(
        &self,
        spans: &mut Vec<Span<'static>>,
        title_display: String,
        task: Option<&PlanTask>,
        query_lower: Option<&str>,
    ) {
        let matched = match (query_lower, task) {
            (Some(q), Some(t)) => task_matches_query(t, q),
            _ => false,
        };
        if !matched {
            spans.push(Span::raw(title_display));
            return;
        }
        let q = query_lower.unwrap_or("");
        let ranges = find_ci_ranges(&title_display, q);
        if ranges.is_empty() {
            spans.push(Span::styled(title_display, Style::default().fg(theme::ATTN)));
            return;
        }
        let hl = Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD);
        let mut pos = 0usize;
        for (s, e) in ranges {
            if s > pos {
                spans.push(Span::raw(title_display[pos..s].to_string()));
            }
            spans.push(Span::styled(title_display[s..e].to_string(), hl));
            pos = e;
        }
        if pos < title_display.len() {
            spans.push(Span::raw(title_display[pos..].to_string()));
        }
    }

    /// Footer separator line for grid/linear; carries the search echo
    /// (`match 2/5: query`) inline when one is pending, so no list row
    /// is spent on it.
    fn footer_sep_line(&self, width: usize, dim: Style) -> Line<'static> {
        match &self.search_status {
            Some(status) => {
                let label = truncate_with_ellipsis(&format!(" {} ", status), width.saturating_sub(2));
                let used = 2 + label.chars().count();
                Line::from(vec![
                    Span::styled("\u{2500}\u{2500}", dim),
                    Span::styled(label, Style::default().fg(theme::ATTN)),
                    Span::styled("\u{2500}".repeat(width.saturating_sub(used)), dim),
                ])
            }
            None => Line::from(Span::styled("\u{2500}".repeat(width), dim)),
        }
    }

    fn create_project(&mut self, name: &str, repo_url: &str) {
        let path = projects_dir().join(name);
        if std::fs::create_dir_all(path.join("tasks")).is_err() { return; }
        let _ = std::fs::write(path.join("repo_url"), repo_url);
        let project = PlanProject {
            name: name.to_string(),
            path,
            repo_url: repo_url.to_string(),
        };
        self.projects.push(project.clone());
        let layout = load_layout(&project.path);
        self.project_data.push(ProjectData { project, tasks: vec![], layout });
        self.rebuild_unified_cols();
        self.recompute_conflicts();
        self.clamp_cursor();
        self.needs_redraw = true;
    }

    pub fn drain_editor_events(&mut self) -> Option<PlanAction> {
        let mut had_event = false;
        if let Some(ref mut editor) = self.editor {
            while let Ok(event) = editor.event_rx.try_recv() {
                had_event = true;
                match event {
                    TermEvent::Exit | TermEvent::ChildExit(_) => editor.exited = true,
                    _ => {}
                }
            }
        }
        if self.editor.as_ref().map_or(false, |e| e.exited) {
            self.needs_redraw = true;
            return Some(self.stop_editor());
        }
        if had_event {
            self.needs_redraw = true;
        }
        None
    }

    pub fn update_layout(&mut self, area_width: u16, area_height: u16) {
        let left_width = if self.linear_mode { 30 } else { area_width / 2 };

        let editor_cols = area_width.saturating_sub(left_width + 2);
        let editor_rows = area_height.saturating_sub(2);
        if (editor_cols, editor_rows) != self.last_editor_size {
            self.last_editor_size = (editor_cols, editor_rows);
            if let Some(ref editor) = self.editor {
                editor.resize(editor_cols, editor_rows);
            }
        }
    }

    // ── Drawing ─────────────────────────────────────────────

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        let left_width = if self.linear_mode { Constraint::Length(30) } else { Constraint::Percentage(50) };
        let cols = Layout::horizontal([left_width, Constraint::Min(30)]).split(area);

        if self.linear_mode { self.draw_linear(frame, cols[0]); }
        else { self.draw_grid(frame, cols[0]); }

        if matches!(self.input_mode, PlanInputMode::Editing) { self.draw_editor(frame, cols[1]); }
        else { self.draw_detail(frame, cols[1]); }

        match &self.input_mode {
            PlanInputMode::Searching { query } => self.draw_search_overlay(frame, area, query),
            PlanInputMode::NewTask { title, parent_name, .. } => {
                self.draw_new_task_overlay(frame, area, title, parent_name.as_deref())
            }
            PlanInputMode::NewHeader { text } => self.draw_new_header_overlay(frame, area, text, false),
            PlanInputMode::EditingHeader { text } => self.draw_new_header_overlay(frame, area, text, true),
            PlanInputMode::BulkArchiveConfirm { project_idx, count } => self.draw_bulk_archive_confirm(frame, area, *project_idx, *count),
            PlanInputMode::NewProject { name, repo_url, field } => self.draw_new_project_overlay(frame, area, name, repo_url, *field),
            PlanInputMode::ProjectPicker { selected } => self.draw_project_picker(frame, area, *selected),
            PlanInputMode::WorkspacePicker { project_idx, task_idx, selected, engine } => self.draw_workspace_picker(frame, area, *project_idx, *task_idx, *selected, *engine),
            PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text, engine } => self.draw_launch_confirm(frame, area, *project_idx, *task_idx, branch_text, *engine),
            _ => {}
        }
    }

    fn draw_grid(&self, frame: &mut Frame, area: Rect) {
        let filter_label = match self.project_filter {
            None => " All Projects ".to_string(),
            Some(pi) => self.project_data.get(pi)
                .map(|pd| format!(" {} ", pd.project.name))
                .unwrap_or_else(|| " ? ".to_string()),
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::DIM))
            .title(Span::styled(filter_label, Style::default().fg(theme::TEXT)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 4 || inner.width < 8 { return; }

        // The `[debug]` row is a dev aid, off by default (reclaims a
        // list row); CM_PLANNING_DEBUG=1 brings it back.
        let debug_on = planning_debug_enabled();
        let help_h = if debug_on { 4u16 } else { 3u16 };
        let grid_height = inner.height.saturating_sub(help_h) as usize;
        let num_cols = self.unified_cols.len().max(1);
        let col_width = inner.width / num_cols as u16;
        let dim = Style::default().fg(theme::DIM);

        // Publish the actual list area height so ensure_cursor_visible
        // scrolls based on the real terminal size, not the stale default.
        let header_h: u16 = if self.project_filter.is_none() { 1 } else { 0 };
        self.grid_rows_visible.set((grid_height as u16).saturating_sub(header_h) as usize);

        for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
            let pd = &self.project_data[pi];
            let column = &pd.layout.columns[ci];
            let x = inner.x + gi as u16 * col_width;
            let w = if gi == num_cols - 1 {
                inner.width.saturating_sub(gi as u16 * col_width)
            } else {
                col_width.saturating_sub(1)
            };

            let show_headers = self.project_filter.is_none();
            if show_headers && self.is_first_col_of_project(gi) {
                let name_display: String = pd.project.name.chars().take(w as usize).collect();
                frame.render_widget(
                    Paragraph::new(Span::styled(name_display, Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD))),
                    Rect::new(x, inner.y, w, 1),
                );
            }

            let header_h: u16 = if show_headers { 1 } else { 0 };
            let col_area = Rect::new(x, inner.y + header_h, w, (grid_height as u16).saturating_sub(header_h));
            let _ = column;
            let items = self.build_column_items(gi, &pd.project.name, pi, ci, w as usize, col_area.height as usize);
            frame.render_widget(List::new(items), col_area);
        }

        if self.unified_cols.is_empty() {
            let msg = if self.projects.is_empty() { "Alt+Shift+p to create project" } else { "Alt+n to create task" };
            frame.render_widget(
                Paragraph::new(Span::styled(format!(" {}", msg), dim)),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        }

        // Vertical separators.
        {
            let buf = frame.buffer_mut();
            for gi in 0..self.unified_cols.len().saturating_sub(1) {
                let is_project_boundary = {
                    let (pi_a, _) = self.unified_cols[gi];
                    let (pi_b, _) = self.unified_cols[gi + 1];
                    pi_a != pi_b
                };
                let sep_x = inner.x + (gi as u16 + 1) * col_width - 1;
                let ch = if is_project_boundary { '\u{2503}' } else { '\u{2502}' };
                let color = if is_project_boundary { theme::HEADER } else { theme::DIM };
                if sep_x < inner.right() {
                    for y in inner.y..inner.y + grid_height as u16 {
                        if let Some(cell) = buf.cell_mut((sep_x, y)) {
                            cell.set_char(ch);
                            cell.set_fg(color);
                        }
                    }
                }
            }
        }

        // Help.
        let help_y = inner.y + inner.height.saturating_sub(help_h);
        let help_area = Rect::new(inner.x, help_y, inner.width, help_h);
        let mut help_lines = vec![self.footer_sep_line(inner.width as usize, dim)];
        if debug_on {
            help_lines.push(Line::from(Span::styled(self.grid_debug_line(), Style::default().fg(theme::ATTN))));
        }
        help_lines.push(Line::from(Span::styled(
            " A-j/k nav \u{00b7} A-h/l cols \u{00b7} A-J/K reorder \u{00b7} A-H/L move \u{00b7} A-v visual \u{00b7} A-g linear \u{00b7} / search \u{00b7} n/N match",
            dim,
        )));
        help_lines.push(Line::from(Span::styled(
            " A-e edit \u{00b7} A-n new \u{00b7} A-i header \u{00b7} A-Ent sep \u{00b7} A-Spc empty \u{00b7} Spc fold \u{00b7} A-s status \u{00b7} A-d done \u{00b7} A-a accept \u{00b7} A-A archive done \u{00b7} A-V show arch \u{00b7} A-x del \u{00b7} A-f launch \u{00b7} A-U unlaunch \u{00b7} A-c col \u{00b7} A-r refresh \u{00b7} A-q quit",
            dim,
        )));
        frame.render_widget(Paragraph::new(help_lines), help_area);
    }

    fn grid_debug_line(&self) -> String {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return " [debug] no column".to_string(),
        };
        let pd = match self.project_data.get(pi) {
            Some(p) => p,
            None => return " [debug] no project".to_string(),
        };
        let rows = self.visible_rows_for_column(pi, ci);
        let off = self.grid_col_scroll.get(self.cursor.col).copied().unwrap_or(0);
        let h = self.grid_rows_visible.get();
        let len = rows.len();
        let row = self.cursor.row;
        let item_str = match rows.get(row).map(|r| (&r.kind, r.depth)) {
            Some((VisibleRowKind::Layout { item: GridItem::Task(slug), .. }, d)) => {
                let archived = pd.tasks.iter().find(|t| t.slug == *slug)
                    .map(|t| t.status == PlanStatus::Archived).unwrap_or(false);
                if archived { format!("Task({}) d{} ARCHIVED", slug, d) } else { format!("Task({}) d{}", slug, d) }
            }
            Some((VisibleRowKind::Subtask { slug }, d)) => format!("Subtask({}) d{}", slug, d),
            Some((VisibleRowKind::Layout { item: GridItem::Empty, .. }, _)) => "Empty".to_string(),
            Some((VisibleRowKind::Layout { item: GridItem::Separator, .. }, _)) => "Separator".to_string(),
            Some((VisibleRowKind::Layout { item: GridItem::Header(t), .. }, _)) => format!("Header({})", t),
            None => "<oob>".to_string(),
        };
        let visual = match self.visual_anchor {
            Some(a) => format!(" visual={}", a),
            None => String::new(),
        };
        format!(
            " [debug] col={} row={} off={} h={} len={} item={} show_arch={}{}",
            self.cursor.col, row, off, h, len, item_str, self.show_archived, visual,
        )
    }

    fn build_column_items<'a>(
        &'a self, col_idx: usize, project_name: &str, pi: usize, ci: usize, width: usize, max_rows: usize,
    ) -> Vec<ListItem<'a>> {
        let rows = self.visible_rows_for_column(pi, ci);
        let mut items = Vec::new();
        let search_q = self.active_search_query();
        let start = self.grid_col_scroll.get(col_idx).copied().unwrap_or(0).min(rows.len());

        for ri in start..rows.len() {
            if items.len() >= max_rows { break; }
            let is_selected = self.cursor.col == col_idx && self.cursor.row == ri;
            let in_visual = self.is_in_visual_range(col_idx, ri);
            let row = &rows[ri];
            // Slug for this row, if any (task layout or subtask).
            let slug_opt: Option<&str> = visible_row_slug(row);

            // Non-task rows render verbatim from the raw layout side.
            let raw_item_opt: Option<&GridItem> = match &row.kind {
                VisibleRowKind::Layout { item, .. } => Some(item),
                VisibleRowKind::Subtask { .. } => None,
            };

            match (raw_item_opt, slug_opt) {
                (_, Some(slug)) => {
                    let task = self.project_data.iter().find_map(|pd| {
                        if pd.project.name == project_name { pd.tasks.iter().find(|t| t.slug == *slug) } else { None }
                    });
                    let (title_str, status, is_claude) = match task {
                        Some(t) => (t.title.as_str(), Some(&t.status), t.source == "claude"),
                        None => (slug, None, false),
                    };
                    let indicator = match status {
                        Some(PlanStatus::Done) => "\u{2713}",
                        Some(PlanStatus::InProgress) => "\u{25c9}",
                        Some(PlanStatus::Backlog) => " ",
                        Some(PlanStatus::Draft) => "\u{25cb}",
                        Some(PlanStatus::Archived) => "\u{25a1}",
                        None => "?",
                    };
                    let indicator_style = if is_claude {
                        Style::default().fg(theme::REMOTE)
                    } else {
                        match status {
                            Some(PlanStatus::Done) => Style::default().fg(theme::OK),
                            Some(PlanStatus::InProgress) => Style::default().fg(theme::ATTN),
                            Some(PlanStatus::Backlog) => Style::default(),
                            Some(PlanStatus::Draft) => Style::default().fg(theme::DIM),
                            Some(PlanStatus::Archived) => Style::default().fg(theme::DIM),
                            None => Style::default(),
                        }
                    };
                    // Tree prefix: indent (2 cells per depth level) + fold
                    // glyph or 2-cell pad. `▼ ` expanded, `▶ ` collapsed,
                    // `  ` leaf. Children-count badge appears as a "(N)"
                    // suffix when collapsed.
                    let indent: String = "  ".repeat(row.depth as usize);
                    let fold_glyph = if row.has_children {
                        if row.expanded { "\u{25bc} " } else { "\u{25b6} " }
                    } else {
                        "  "
                    };
                    let count_suffix = if row.has_children && !row.expanded {
                        format!(" ({})", row.descendant_count)
                    } else {
                        String::new()
                    };

                    let claude_prefix = if is_claude { "[C] " } else { "" };
                    let prefix_len = indent.len() + 2 /*fold*/ + 2 /*ind+space*/ + claude_prefix.len();
                    let max_title = width.saturating_sub(prefix_len + count_suffix.len());
                    let title_display = truncate_with_ellipsis(&title_str, max_title);

                    let mut spans = Vec::new();
                    if !indent.is_empty() {
                        spans.push(Span::raw(indent.clone()));
                    }
                    spans.push(Span::styled(fold_glyph.to_string(), Style::default().fg(theme::DIM)));
                    spans.push(Span::styled(format!("{} ", indicator), indicator_style));
                    if is_claude {
                        spans.push(Span::styled(claude_prefix, Style::default().fg(theme::REMOTE)));
                    }
                    self.push_title_spans(&mut spans, title_display, task, search_q.as_deref());
                    if !count_suffix.is_empty() {
                        spans.push(Span::styled(count_suffix, Style::default().fg(theme::DIM)));
                    }
                    let line = Line::from(spans);
                    let conflict = self.is_conflict(project_name, slug);
                    let base_fg = if is_claude { theme::REMOTE } else { theme::MUTED };
                    let style = if is_selected && in_visual {
                        Style::default().fg(theme::TEXT).bg(theme::SELECT_BG).add_modifier(Modifier::BOLD)
                    } else if is_selected {
                        Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
                    } else if in_visual {
                        Style::default().fg(theme::TEXT).bg(theme::SELECT_BG)
                    } else {
                        Style::default().fg(base_fg)
                    };
                    let style = if conflict && is_selected {
                        style.bg(theme::ERROR).fg(theme::TEXT)
                    } else if conflict {
                        style.bg(theme::CONFLICT_BG)
                    } else { style };
                    items.push(ListItem::new(line).style(style));
                }
                (Some(GridItem::Separator), None) => {
                    let ch = if is_selected { "\u{2501}" } else { "\u{2500}" };
                    let st = if is_selected { Style::default().fg(theme::TEXT) } else { Style::default().fg(theme::DIM) };
                    items.push(ListItem::new(Line::from(Span::styled(ch.repeat(width.saturating_sub(1)), st))));
                }
                (Some(GridItem::Empty), None) => {
                    items.push(ListItem::new(Line::from("")));
                }
                (Some(GridItem::Header(text)), None) => {
                    let base_style = Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD);
                    let style = if is_selected && in_visual {
                        base_style.bg(theme::SELECT_BG)
                    } else if is_selected {
                        base_style.bg(theme::HEADER_SELECT_BG)
                    } else if in_visual {
                        base_style.bg(theme::SELECT_BG)
                    } else {
                        base_style
                    };
                    let max_text = width.saturating_sub(1);
                    let display = if text.len() > max_text {
                        truncate_with_ellipsis(&text, max_text)
                    } else {
                        text.clone()
                    };
                    items.push(ListItem::new(Line::from(Span::styled(display, style))));
                }
                _ => {}
            }
        }
        items
    }

    fn draw_linear(&self, frame: &mut Frame, area: Rect) {
        let title = match self.project_filter {
            None => " All [linear] ".to_string(),
            Some(pi) => self.project_data.get(pi)
                .map(|pd| format!(" {} [linear] ", pd.project.name))
                .unwrap_or_else(|| " ? ".to_string()),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::DIM))
            .title(Span::styled(title, Style::default().fg(theme::TEXT)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 4 || inner.width < 4 { return; }

        let help_entries: Vec<(&str, &str)> = vec![
            ("A-j/k  nav", "A-d    done"),
            ("A-J/K  reorder", "A-f    launch"),
            ("A-e    edit", "A-a    accept"),
            ("A-n    new", "A-x    delete"),
            ("A-N    subtask", "A-O    reopen done"),
            ("A-u    unbind", "A-U    unlaunch"),
            ("A-Ent  sep", "A-Spc  empty"),
            ("A-i    header", "A-A    archive done"),
            ("A-s/S  status", "A-V    show arch"),
            ("A-g    grid", "A-t    sessions"),
            ("A-p    filter", "/      search"),
            ("n      next match", "N      prev match"),
        ];
        let help_rows = help_entries.len() as u16;
        let list_height = inner.height.saturating_sub(help_rows + 2) as usize;
        let dim = Style::default().fg(theme::DIM);
        self.grid_rows_visible.set(list_height);

        let mut items: Vec<ListItem> = Vec::new();
        let mut flat_idx = 0usize;
        let search_q = self.active_search_query();

        for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
            let pd = &self.project_data[pi];
            let column = &pd.layout.columns[ci];

            if gi > 0 && self.is_first_col_of_project(gi) && !column.is_empty() {
                if flat_idx >= self.linear_scroll && items.len() < list_height {
                    let sep = "\u{2550}".repeat(inner.width.saturating_sub(2) as usize);
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled(" ", dim),
                        Span::styled(sep, Style::default().fg(theme::HEADER)),
                    ])));
                }
                flat_idx += 1;
            }

            if self.is_first_col_of_project(gi) && self.project_filter.is_none() {
                if flat_idx >= self.linear_scroll && items.len() < list_height {
                    items.push(ListItem::new(Line::from(Span::styled(
                        format!(" {}", pd.project.name),
                        Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD),
                    ))));
                }
                flat_idx += 1;
            }

            for (ri, grid_item) in column.iter().enumerate() {
                if flat_idx < self.linear_scroll { flat_idx += 1; continue; }
                if items.len() >= list_height { break; }
                let is_selected = self.cursor.col == gi && self.cursor.row == ri;

                // Skip archived task rows when show_archived is off.
                if !self.show_archived {
                    if let GridItem::Task(slug) = grid_item {
                        let archived = pd.tasks.iter().find(|t| t.slug == *slug)
                            .map(|t| t.status == PlanStatus::Archived).unwrap_or(false);
                        if archived { flat_idx += 1; continue; }
                    }
                }

                match grid_item {
                    GridItem::Task(slug) => {
                        let task = pd.tasks.iter().find(|t| t.slug == *slug);
                        let (title_str, status, is_claude) = match task {
                            Some(t) => (t.title.as_str(), Some(&t.status), t.source == "claude"),
                            None => (slug.as_str(), None, false),
                        };
                        let indicator = match status {
                            Some(PlanStatus::Done) => "\u{2713}",
                            Some(PlanStatus::InProgress) => "\u{25c9}",
                            Some(PlanStatus::Backlog) => " ",
                            Some(PlanStatus::Draft) => "\u{25cb}",
                            Some(PlanStatus::Archived) => "\u{25a1}",
                            None => "?",
                        };
                        let indicator_style = if is_claude {
                            Style::default().fg(theme::REMOTE)
                        } else {
                            match status {
                                Some(PlanStatus::Done) => Style::default().fg(theme::OK),
                                Some(PlanStatus::InProgress) => Style::default().fg(theme::ATTN),
                                _ => Style::default().fg(theme::DIM),
                            }
                        };
                        let claude_prefix = if is_claude { "[C] " } else { "" };
                        let max_title = (inner.width as usize).saturating_sub(5 + claude_prefix.len());
                        let title_display = truncate_with_ellipsis(&title_str, max_title);

                        let mut spans = vec![
                            Span::styled(format!(" {} ", indicator), indicator_style),
                        ];
                        if is_claude {
                            spans.push(Span::styled(claude_prefix, Style::default().fg(theme::REMOTE)));
                        }
                        self.push_title_spans(&mut spans, title_display, task, search_q.as_deref());
                        let line = Line::from(spans);
                        let conflict = self.is_conflict(&pd.project.name, slug);
                        let base_fg = if is_claude { theme::REMOTE } else { theme::MUTED };
                        let style = if is_selected {
                            Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(base_fg)
                        };
                        let style = if conflict && is_selected {
                            style.bg(theme::ERROR).fg(theme::TEXT)
                        } else if conflict {
                            style.bg(theme::CONFLICT_BG)
                        } else { style };
                        items.push(ListItem::new(line).style(style));
                    }
                    GridItem::Separator => {
                        let ch = if is_selected { "\u{2501}" } else { "\u{2500}" };
                        let st = if is_selected { Style::default().fg(theme::TEXT) } else { dim };
                        items.push(ListItem::new(Line::from(Span::styled(
                            format!(" {}", ch.repeat((inner.width as usize).saturating_sub(2))), st,
                        ))));
                    }
                    GridItem::Empty => {
                        items.push(ListItem::new(Line::from("")));
                    }
                    GridItem::Header(text) => {
                        let base_style = Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD);
                        let style = if is_selected {
                            base_style.bg(theme::HEADER_SELECT_BG)
                        } else {
                            base_style
                        };
                        let max_text = (inner.width as usize).saturating_sub(2);
                        let display = if text.len() > max_text {
                            truncate_with_ellipsis(&text, max_text)
                        } else {
                            text.clone()
                        };
                        items.push(ListItem::new(Line::from(Span::styled(format!(" {}", display), style))));
                    }
                }
                flat_idx += 1;
            }
        }

        frame.render_widget(List::new(items), Rect { x: inner.x, y: inner.y, width: inner.width, height: list_height as u16 });

        let help_y = inner.y + inner.height.saturating_sub(help_rows + 1);
        let help_area = Rect { x: inner.x, y: help_y, width: inner.width, height: help_rows + 1 };
        let sep = self.footer_sep_line(inner.width as usize, dim);
        let col = inner.width / 2;
        let mut lines = vec![sep];
        for (left, right) in &help_entries {
            lines.push(Line::from(vec![
                Span::styled(format!("{:<w$}", left, w = col as usize), dim),
                Span::styled(*right, dim),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), help_area);
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect) {
        let selected = self.selected_task();
        let title = selected.as_ref()
            .map(|(t, _)| format!(" {} ", t.title))
            .unwrap_or_else(|| " No task selected ".to_string());
        let title_style = if selected.is_some() { Style::default().fg(theme::TEXT) } else { Style::default().fg(theme::DIM) };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::DIM))
            .title(Span::styled(title, title_style));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some((task, project_name)) = selected {
            let mut lines: Vec<Line> = vec![];
            lines.push(Line::from(vec![
                Span::styled("  Slug: ", Style::default().fg(theme::DIM)),
                Span::styled(
                    format!("{}/{}", project_name, task.slug),
                    Style::default().fg(theme::TEXT),
                ),
            ]));

            let status_color = match task.status {
                PlanStatus::Done => theme::OK, PlanStatus::InProgress => theme::ATTN,
                PlanStatus::Backlog => theme::TEXT, PlanStatus::Draft => theme::DIM,
                PlanStatus::Archived => theme::DIM,
            };
            let mut meta = vec![
                Span::styled("  Status: ", Style::default().fg(theme::DIM)),
                Span::styled(task.status.label(), Style::default().fg(status_color)),
            ];
            if let Some(d) = task.difficulty {
                meta.push(Span::styled("    Difficulty: ", Style::default().fg(theme::DIM)));
                meta.push(Span::styled(d.to_string(), Style::default().fg(theme::TEXT)));
            }
            lines.push(Line::from(meta));

            if !task.depends.is_empty() {
                let dep_color = if self.is_conflict(project_name, &task.slug) { theme::ERROR } else { theme::TEXT };
                lines.push(Line::from(vec![
                    Span::styled("  Depends: ", Style::default().fg(theme::DIM)),
                    Span::styled(task.depends.join(", "), Style::default().fg(dep_color)),
                ]));
            }
            if let Some(ref created) = task.created {
                lines.push(Line::from(vec![
                    Span::styled("  Created: ", Style::default().fg(theme::DIM)),
                    Span::styled(created.as_str(), Style::default().fg(theme::TEXT)),
                ]));
            }
            if task.source == "claude" {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(" PROPOSED ", Style::default().fg(theme::TEXT).bg(theme::REMOTE).add_modifier(Modifier::BOLD)),
                    Span::styled("  Alt+a to accept, Alt+d to reject", Style::default().fg(theme::DIM)),
                ]));
            }
            lines.push(Line::from(""));
            let sep_w = inner.width.saturating_sub(4) as usize;
            lines.push(Line::from(Span::styled(format!("  {}", "\u{2500}".repeat(sep_w)), Style::default().fg(theme::DIM))));
            lines.push(Line::from(""));

            let body = if !task.description.is_empty() {
                &task.description
            } else if !task.prompt.is_empty() {
                &task.prompt
            } else {
                ""
            };

            if body.is_empty() {
                lines.push(Line::from(Span::styled("  No description. Press Alt+e to edit.", Style::default().fg(theme::DIM))));
            } else {
                for line in body.lines() {
                    let style = if line.starts_with("## ") {
                        Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)
                    } else { Style::default().fg(theme::MUTED) };
                    lines.push(Line::from(Span::styled(format!("  {}", line), style)));
                }
            }
            frame.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((self.detail_scroll, 0)),
                inner,
            );
        } else if self.projects.is_empty() {
            frame.render_widget(Paragraph::new(vec![
                Line::from(""),
                Line::from(Span::styled("  No tasks yet. Press Alt+Shift+p to create a project.", Style::default().fg(theme::DIM))),
                Line::from(Span::styled("  Or press Alt+r to refresh from API.", Style::default().fg(theme::DIM))),
            ]), inner);
        } else if self.project_data.iter().all(|pd| pd.tasks.is_empty()) {
            frame.render_widget(Paragraph::new(Span::styled(
                "  No tasks. Press Alt+n to create one.", Style::default().fg(theme::DIM),
            )), inner);
        }
    }

    fn draw_editor(&self, frame: &mut Frame, area: Rect) {
        let title = self.editing_slug.as_ref()
            .map(|s| format!(" Editing: {} ", s))
            .unwrap_or_else(|| " Editor ".to_string());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::DIM))
            .title(Span::styled(title, Style::default().fg(theme::TEXT)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if let Some(ref editor) = self.editor {
            frame.render_widget(TerminalWidget::new(&editor.term, true), inner);
        }
    }

    fn draw_search_overlay(&self, frame: &mut Frame, area: Rect, query: &str) {
        let (w, h) = (50u16.min(area.width.saturating_sub(4)), 5u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(" Search ", Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  > ", Style::default().fg(theme::DIM)),
                Span::styled(query, Style::default().fg(theme::TEXT)),
                Span::styled("\u{2588}", Style::default().fg(theme::TEXT)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Enter accept \u{00b7} Esc cancel \u{00b7} then n/N next/prev", Style::default().fg(theme::DIM))),
        ]), inner);
    }

    fn draw_new_task_overlay(
        &self,
        frame: &mut Frame,
        area: Rect,
        title: &str,
        parent_name: Option<&str>,
    ) {
        // Subtask overlay is one row taller so we can show the parent
        // task's name + the worktree-mode default. The fields all stay
        // read-only here; the only typed input is the title.
        let h = if parent_name.is_some() { 7u16 } else { 5u16 };
        let w = 60u16.min(area.width.saturating_sub(4));
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let title_label = if parent_name.is_some() {
            " New Subtask "
        } else {
            " New Task "
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                title_label,
                Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let mut lines: Vec<Line> = Vec::new();
        if let Some(p) = parent_name {
            // Truncate long parent names so the dialog stays inside its
            // 60-cell box; we keep ~46 chars of room after "  Parent: ".
            let max = 46;
            let shown: String = if p.chars().count() > max {
                format!("{}…", p.chars().take(max).collect::<String>())
            } else {
                p.to_string()
            };
            lines.push(Line::from(vec![
                Span::styled("  Parent: ", Style::default().fg(theme::DIM)),
                Span::styled(shown, Style::default().fg(theme::HEADER)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  Mode:   ", Style::default().fg(theme::DIM)),
                Span::styled("inherit", Style::default().fg(theme::MUTED)),
                Span::styled(
                    "  (subtask shares parent's worktree on launch)",
                    Style::default().fg(theme::DIM),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("  Title:  ", Style::default().fg(theme::DIM)),
            Span::styled(title, Style::default().fg(theme::TEXT)),
            Span::styled("\u{2588}", Style::default().fg(theme::TEXT)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Enter create \u{00b7} Esc cancel",
            Style::default().fg(theme::DIM),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_new_header_overlay(&self, frame: &mut Frame, area: Rect, text: &str, editing: bool) {
        let title = if editing { " Edit Header " } else { " New Header " };
        let (w, h) = (60u16.min(area.width.saturating_sub(4)), 5u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(title, Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Text: ", Style::default().fg(theme::DIM)),
                Span::styled(text, Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)),
                Span::styled("\u{2588}", Style::default().fg(theme::TEXT)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Enter save \u{00b7} Esc cancel", Style::default().fg(theme::DIM))),
        ]), inner);
    }

    fn draw_new_project_overlay(&self, frame: &mut Frame, area: Rect, name: &str, repo_url: &str, field: NewProjectField) {
        let (w, h) = (70u16.min(area.width.saturating_sub(4)), 7u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(" New Project ", Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let cursor = "\u{2588}";
        let name_cursor = if field == NewProjectField::Name { cursor } else { "" };
        let url_cursor = if field == NewProjectField::RepoUrl { cursor } else { "" };

        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("     Name: ", Style::default().fg(theme::DIM)),
                Span::styled(name, Style::default().fg(theme::TEXT)),
                Span::styled(name_cursor, Style::default().fg(theme::TEXT)),
            ]),
            Line::from(vec![
                Span::styled("  Repo URL: ", Style::default().fg(theme::DIM)),
                Span::styled(repo_url, Style::default().fg(theme::TEXT)),
                Span::styled(url_cursor, Style::default().fg(theme::TEXT)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Tab switch \u{00b7} Enter create \u{00b7} Esc cancel", Style::default().fg(theme::DIM))),
        ]), inner);
    }

    fn draw_project_picker(&self, frame: &mut Frame, area: Rect, selected: usize) {
        let w = 40u16.min(area.width.saturating_sub(4));
        let h = (self.projects.len() as u16 + 5).min(area.height.saturating_sub(4));
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(" Filter Projects ", Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let mut lines: Vec<Line> = vec![];
        let all_style = if selected == 0 { Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme::MUTED) };
        let all_ind = if selected == 0 { ">" } else { " " };
        lines.push(Line::from(Span::styled(format!("  {} All projects", all_ind), all_style)));

        for (i, project) in self.projects.iter().enumerate() {
            let idx = i + 1;
            let ind = if selected == idx { ">" } else { " " };
            let st = if selected == idx { Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD) } else { Style::default().fg(theme::MUTED) };
            lines.push(Line::from(Span::styled(format!("  {} {}", ind, project.name), st)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("j/k navigate \u{00b7} Enter select \u{00b7} Esc cancel", Style::default().fg(theme::DIM))));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_workspace_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        project_idx: usize,
        task_idx: usize,
        selected: usize,
        engine: LaunchEngine,
    ) {
        let task_name = self
            .project_data
            .get(project_idx)
            .and_then(|pd| pd.tasks.get(task_idx))
            .map(|t| t.title.as_str())
            .unwrap_or("?");
        let candidates = self.candidates_for(project_idx, task_idx);
        // +2 for the engine row and its blank separator.
        let rows = 5 + candidates.len() as u16 + 2 + 2;
        let (w, h) = (60u16.min(area.width.saturating_sub(4)), rows);
        let dialog = Rect::new(
            (area.width - w) / 2,
            (area.height.saturating_sub(h)) / 2,
            w,
            h,
        );
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(
                " Launch Into ",
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let display_name: String = task_name
            .chars()
            .take((w as usize).saturating_sub(10))
            .collect();
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("  Task: ", Style::default().fg(theme::DIM)),
            Span::styled(display_name, Style::default().fg(theme::TEXT)),
        ]));
        lines.push(Line::from(""));
        let row = |label: &str, idx: usize| -> Line<'static> {
            let is_sel = idx == selected;
            let ind = if is_sel { ">" } else { " " };
            let st = if is_sel {
                Style::default()
                    .fg(theme::TEXT)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme::MUTED)
            };
            Line::from(Span::styled(format!("  {} {}", ind, label), st))
        };
        lines.push(row("+ New workspace (create worktree)", 0));
        for (i, c) in candidates.iter().enumerate() {
            let label = if c.name.len() > (w as usize).saturating_sub(8) {
                truncate_with_ellipsis(&c.name, (w as usize).saturating_sub(8))
            } else {
                c.name.clone()
            };
            lines.push(row(&label, i + 1));
        }
        lines.push(Line::from(""));
        lines.push(engine_line(engine));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  j/k navigate \u{00b7} \u{2190}/\u{2192} engine \u{00b7} Enter select \u{00b7} Esc cancel",
            Style::default().fg(theme::DIM),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_bulk_archive_confirm(&self, frame: &mut Frame, area: Rect, project_idx: usize, count: usize) {
        let project_name = self.project_data.get(project_idx)
            .map(|pd| pd.project.name.as_str())
            .unwrap_or("?");
        let (w, h) = (62u16.min(area.width.saturating_sub(4)), 7u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(" Archive Done Tasks ", Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("  Archive {} done task{} in ", count, if count == 1 { "" } else { "s" }), Style::default().fg(theme::TEXT)),
                Span::styled(project_name, Style::default().fg(theme::HEADER).add_modifier(Modifier::BOLD)),
                Span::styled("?", Style::default().fg(theme::TEXT)),
            ]),
            Line::from(""),
            Line::from(Span::styled("  Enter confirm \u{00b7} Esc cancel", Style::default().fg(theme::DIM))),
        ]), inner);
    }

    fn draw_launch_confirm(&self, frame: &mut Frame, area: Rect, project_idx: usize, task_idx: usize, branch_text: &str, engine: LaunchEngine) {
        let task_name = self.project_data.get(project_idx)
            .and_then(|pd| pd.tasks.get(task_idx))
            .map(|t| t.title.as_str())
            .unwrap_or("?");
        // +2 rows for the engine line and its blank separator.
        let (w, h) = (60u16.min(area.width.saturating_sub(4)), 11u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(theme::TEXT))
            .title(Span::styled(" Launch Task ", Style::default().fg(theme::TEXT).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let display_name: String = task_name.chars().take((w as usize).saturating_sub(10)).collect();
        let branch_hint = if branch_text.trim() == "." {
            "  in-place (main repo, no worktree)"
        } else if branch_text.is_empty() {
            "main"
        } else {
            ""
        };
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("    Task: ", Style::default().fg(theme::DIM)),
                Span::styled(display_name, Style::default().fg(theme::TEXT)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Branch: ", Style::default().fg(theme::DIM)),
                Span::styled(branch_text, Style::default().fg(theme::TEXT)),
                Span::styled("\u{2588}", Style::default().fg(theme::TEXT)),
                Span::styled(branch_hint, Style::default().fg(theme::DIM)),
            ]),
            Line::from(""),
            engine_line(engine),
            Line::from(""),
            Line::from(Span::styled("  \u{2190}/\u{2192} engine \u{00b7} Enter launch \u{00b7} Esc cancel", Style::default().fg(theme::DIM))),
        ]), inner);
    }
}

#[cfg(test)]
mod truncate_tests {
    use super::truncate_with_ellipsis;

    #[test]
    fn truncates_on_char_boundary_with_multibyte() {
        // Regression: byte-index slicing panicked when the cut landed
        // inside a multibyte char (e.g. '≤' in a task name). Must never
        // panic and must produce valid UTF-8.
        let s = "Backtest: does AC-impact calib day-snapping (≤24h tail staleness) cost anything?";
        for max in 0..s.len() + 2 {
            let out = truncate_with_ellipsis(s, max);
            assert!(out.is_char_boundary(0));
            // round-trips as valid UTF-8 (String guarantees it; the point
            // is that we got here without panicking)
            let _ = out.chars().count();
        }
    }

    #[test]
    fn short_string_is_unchanged() {
        assert_eq!(truncate_with_ellipsis("hi", 10), "hi");
    }
}

#[cfg(test)]
mod tests {
    //! Pins down that task creation honors the project's persisted
    //! `repo_url` (written by `create_project` to
    //! `<projects_dir>/<name>/repo_url`) instead of always rewriting
    //! it from the project name via `repo_url_for_project`. Pre-fix,
    //! a project pointing at a non-github / forked / renamed remote
    //! silently produced new tasks with the wrong URL.
    use super::*;
    use std::path::PathBuf;

    fn make_project(name: &str, repo_url: &str) -> ProjectData {
        ProjectData {
            project: PlanProject {
                name: name.to_string(),
                path: PathBuf::from("/dev/null"),
                repo_url: repo_url.to_string(),
            },
            tasks: vec![],
            layout: GridLayout::default(),
        }
    }

    fn extract_create(action: PlanAction) -> (String, String, Option<String>) {
        match action {
            PlanAction::CreateTask {
                project,
                repo_url,
                parent_task_id,
                ..
            } => (project, repo_url, parent_task_id),
            other => panic!("expected CreateTask, got {:?}", std::mem::discriminant(&other)),
        }
    }

    fn make_task(id: &str, slug: &str, parent: Option<&str>) -> PlanTask {
        PlanTask {
            id: id.to_string(),
            slug: slug.to_string(),
            title: slug.to_string(),
            status: PlanStatus::Backlog,
            difficulty: None,
            depends: vec![],
            branch: None,
            created: None,
            description: String::new(),
            prompt: String::new(),
            source: "user".to_string(),
            is_cloud: false,
            repo_url: String::new(),
            parent_task_id: parent.map(str::to_string),
            kind: "oneshot".to_string(),
            worker_vm: None,
            vm_project: None,
            vm_zone: None,
            run_key: None,
            bt_label: None,
        }
    }

    #[test]
    fn visible_rows_skip_parented_tasks_from_own_column_when_collapsed() {
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("a", "root", None),
            make_task("b", "child", Some("a")),
        ];
        pd.layout.columns = vec![vec![
            GridItem::Task("root".to_string()),
            GridItem::Task("child".to_string()),
        ]];
        view.project_data.push(pd);

        let rows = view.visible_rows_for_column(0, 0);
        assert_eq!(rows.len(), 1, "child should be hidden under collapsed parent");
        assert!(rows[0].has_children, "parent should advertise children");
        assert_eq!(rows[0].descendant_count, 1);
        assert!(matches!(rows[0].kind, VisibleRowKind::Layout { .. }));
    }

    #[test]
    fn visible_rows_inject_children_when_expanded() {
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("a", "root", None),
            make_task("b", "child", Some("a")),
            make_task("c", "grand", Some("b")),
        ];
        pd.layout.columns = vec![vec![
            GridItem::Task("root".to_string()),
            GridItem::Task("child".to_string()),
            GridItem::Task("grand".to_string()),
        ]];
        view.project_data.push(pd);
        view.expanded_tasks.insert("a".to_string());

        let rows = view.visible_rows_for_column(0, 0);
        assert_eq!(rows.len(), 2, "only parent + direct child (grand still folded)");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        match &rows[1].kind {
            VisibleRowKind::Subtask { slug } => assert_eq!(slug, "child"),
            other => panic!("expected synthetic Subtask, got {:?}", other),
        }
        // Now expand the child too.
        view.expanded_tasks.insert("b".to_string());
        let rows = view.visible_rows_for_column(0, 0);
        assert_eq!(rows.len(), 3, "grand should now be visible");
        assert_eq!(rows[2].depth, 2);
    }

    #[test]
    fn visible_rows_hide_archived_top_level_unless_show_archived() {
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        let mut t = make_task("a", "live", None);
        t.status = PlanStatus::Backlog;
        let mut t2 = make_task("b", "old", None);
        t2.status = PlanStatus::Archived;
        pd.tasks = vec![t, t2];
        pd.layout.columns = vec![vec![
            GridItem::Task("live".to_string()),
            GridItem::Task("old".to_string()),
        ]];
        view.project_data.push(pd);

        let rows = view.visible_rows_for_column(0, 0);
        assert_eq!(rows.len(), 1, "archived row should be hidden by default");
        assert!(matches!(&rows[0].kind, VisibleRowKind::Layout { item: GridItem::Task(s), .. } if s == "live"));

        view.show_archived = true;
        let rows = view.visible_rows_for_column(0, 0);
        assert_eq!(rows.len(), 2, "archived row should appear when toggle is on");
    }

    #[test]
    fn visible_rows_treat_orphan_parent_as_top_level() {
        // parent_task_id pointing outside the project shouldn't hide
        // the row — there's no parent to render it under.
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![make_task("a", "stray", Some("missing"))];
        pd.layout.columns = vec![vec![GridItem::Task("stray".to_string())]];
        view.project_data.push(pd);

        let rows = view.visible_rows_for_column(0, 0);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].has_children);
        assert_eq!(rows[0].depth, 0);
    }

    #[test]
    fn alt_x_on_synthetic_subtask_routes_to_delete_not_remove_separator() {
        // Regression: A-x checked `layout.columns[ci].get(self.cursor.row)`,
        // but cursor.row now indexes the visible-row projection. With a
        // synthetic Subtask row whose visible index lines up with a raw
        // Separator (or Empty/Header) further down the column, the handler
        // mis-routed to `remove_separator` and the focused subtask never
        // got deleted.
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("a", "parent", None),
            make_task("b", "child", Some("a")),
        ];
        // raw column: [Task(parent), Separator, Task(child)]
        // visible (parent expanded): [Layout(parent, raw=0),
        //                             Subtask(child),
        //                             Layout(Separator, raw=1)]
        pd.layout.columns = vec![vec![
            GridItem::Task("parent".to_string()),
            GridItem::Separator,
            GridItem::Task("child".to_string()),
        ]];
        view.project_data.push(pd);
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();
        view.expanded_tasks.insert("a".to_string());
        view.rebuild_unified_cols();
        view.cursor = GridCursor { col: 0, row: 1 };

        // Sanity: cursor is actually on the synthetic Subtask row.
        match view.cursor_visible_row().expect("visible row").kind {
            VisibleRowKind::Subtask { ref slug } => assert_eq!(slug, "child"),
            other => panic!("expected Subtask row at cursor, got {:?}", other),
        }

        let event = CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::ALT,
        ));
        match view.handle_event(&event) {
            PlanAction::DeleteTask { id } => assert_eq!(id, "b"),
            other => panic!(
                "expected DeleteTask for child, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn sync_layout_trims_trailing_empties_and_inserts_after_content() {
        // Regression: repeated move-down-past-end (`A-J`) accumulates
        // Empty padding; once the content below it disappears the column
        // ends in a dead blank tail and newly synced (e.g. claude-
        // proposed) tasks rendered dozens of rows below the last real
        // item. The tail must be trimmed and new tasks must land
        // directly after the last content cell.
        let mut layout = GridLayout {
            columns: vec![vec![
                GridItem::Task("existing".to_string()),
                GridItem::Empty, // mid-column spacing — must survive
                GridItem::Task("kept".to_string()),
                GridItem::Empty,
                GridItem::Empty,
                GridItem::Empty,
            ]],
        };
        let mut proposed = make_task("c", "proposed", None);
        proposed.source = "claude".to_string();
        let tasks = vec![
            make_task("a", "existing", None),
            make_task("b", "kept", None),
            proposed,
        ];
        sync_layout_with_tasks(&mut layout, &tasks);

        assert_eq!(
            layout.columns[0],
            vec![
                GridItem::Task("existing".to_string()),
                GridItem::Empty,
                GridItem::Task("kept".to_string()),
                GridItem::Task("proposed".to_string()),
            ],
        );
    }

    #[test]
    fn sync_layout_drops_column_reduced_to_empties() {
        // A column whose tasks were all deleted, leaving only Empty
        // padding, trims to nothing and is removed entirely.
        let mut layout = GridLayout {
            columns: vec![
                vec![GridItem::Task("gone".to_string()), GridItem::Empty],
                vec![GridItem::Task("alive".to_string())],
            ],
        };
        let tasks = vec![make_task("a", "alive", None)];
        sync_layout_with_tasks(&mut layout, &tasks);

        assert_eq!(layout.columns, vec![vec![GridItem::Task("alive".to_string())]]);
    }

    #[test]
    fn create_task_uses_stored_repo_url() {
        let mut view = PlanningView::new();
        let stored = "git@gitlab.example.com:org/repo.git";
        view.project_data.push(make_project("repo", stored));
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();

        let (project, repo_url, parent) = extract_create(view.create_task("Task title", None));

        assert_eq!(project, "repo");
        assert_eq!(repo_url, stored);
        assert_eq!(parent, None);
    }

    #[test]
    fn create_task_falls_back_when_stored_repo_url_is_empty() {
        let mut view = PlanningView::new();
        // Empty stored URL → fallback to `repo_url_for_project`,
        // which derives a github URL from the project name. The
        // hardcoded helper is the right answer for fresh projects
        // that don't yet have a remote on disk.
        view.project_data.push(make_project("brand-new", ""));
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();

        let (_, repo_url, _) = extract_create(view.create_task("Task title", None));

        assert_eq!(repo_url, repo_url_for_project("brand-new"));
        assert!(repo_url.starts_with("https://github.com/"));
    }

    /// Build a minimal `Task` with the planning fields the loader cares
    /// about. Anything outside that path is filled with sensible defaults.
    fn api_task(id: &str, project: &str, repo_url: &str) -> crate::api::Task {
        crate::api::Task {
            id: id.to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            repo_url: repo_url.to_string(),
            repo_branch: "main".to_string(),
            name: Some(format!("Task {}", id)),
            prompt: None,
            status: "backlog".to_string(),
            worker_vm: None,
            worker_zone: None,
            blocked_at: None,
            session_id: None,
            wip_branch: None,
            project: Some(project.to_string()),
            slug: Some(format!("task-{}", id)),
            description: None,
            difficulty: None,
            depends: None,
            source: "user".to_string(),
            is_cloud: false,
            kind: "oneshot".to_string(),
            parent_task_id: None,
            worktree_mode: "inherit".to_string(),
            metadata: None,
        }
    }

    #[test]
    fn api_loaded_project_inherits_repo_url_from_task_when_disk_file_missing() {
        // Projects discovered via API tasks (no local `create_project`
        // run) have no `~/.cm/projects/<name>/repo_url` file. Pre-fix,
        // `repo_url` stayed empty and `create_task` silently rewrote
        // the URL via `repo_url_for_project` — defeating the goal for
        // exactly the case it was meant to fix. Verify the fall-back
        // chain: disk → API task → empty.
        let _lock = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let stored = "git@gitlab.example.com:org/api-only.git";
        let mut view = PlanningView::new();
        view.update_from_api(vec![api_task("t1", "api-only", stored)]);

        // The freshly-built project record carries the API URL.
        let pd = view
            .project_data
            .iter()
            .find(|pd| pd.project.name == "api-only")
            .expect("project loaded from api");
        assert_eq!(pd.project.repo_url, stored);

        // create_task on this project must propagate the API URL
        // (NOT the github fallback derived from the project name).
        let (project, repo_url, _) = extract_create(view.create_task("New", None));
        assert_eq!(project, "api-only");
        assert_eq!(repo_url, stored);
        assert_ne!(repo_url, repo_url_for_project("api-only"));

        if let Some(p) = prev {
            std::env::set_var("HOME", p);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn update_from_api_preserves_previously_hydrated_repo_url() {
        // A project hydrated earlier with a good URL must not lose it
        // when a later API refresh returns no tasks for it AND there
        // is no disk file. Pre-fix, the in-memory URL got clobbered
        // to empty and the next `create_task` fell back to the
        // hardcoded github helper — re-introducing the original bug
        // for the same project the prior round had just fixed.
        let _lock = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let stored = "git@example.com:x/y.git";
        let mut view = PlanningView::new();
        view.project_data.push(make_project("y", stored));
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();

        // Refresh with no tasks — and crucially, no disk file under
        // the temp HOME. Without the existing-value fallback, the
        // freshly-built project record would land with an empty URL.
        view.update_from_api(vec![]);

        let pd = view
            .project_data
            .iter()
            .find(|pd| pd.project.name == "y")
            .expect("project survived refresh");
        assert_eq!(pd.project.repo_url, stored);

        let (_, repo_url, _) = extract_create(view.create_task("New", None));
        assert_eq!(repo_url, stored);

        if let Some(p) = prev {
            std::env::set_var("HOME", p);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn update_from_api_propagates_parent_task_id_change() {
        // Regression: an API-side reparent (e.g. an agent calling
        // `update_task(parent_task_id=...)`) must take effect on the
        // next refresh. Pre-fix, the merge block overwrote every other
        // PlanTask field from the API row but silently dropped
        // `parent_task_id`, so `children_of` kept reading stale `None`
        // from `pd.tasks` and the subtask never appeared under its
        // parent in the grid.
        let _lock = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        // Two tasks already hydrated locally with the child unparented.
        pd.tasks.push(make_task("parent-id", "parent", None));
        pd.tasks.push(make_task("child-id", "child", None));
        view.project_data.push(pd);
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();

        // API now reports the child reparented under "parent-id".
        let mut parent_api = api_task("parent-id", "p", "");
        parent_api.slug = Some("parent".to_string());
        let mut child_api = api_task("child-id", "p", "");
        child_api.slug = Some("child".to_string());
        child_api.parent_task_id = Some("parent-id".to_string());

        view.update_from_api(vec![parent_api, child_api]);

        let pd = view
            .project_data
            .iter()
            .find(|pd| pd.project.name == "p")
            .expect("project survived refresh");
        let child = pd
            .tasks
            .iter()
            .find(|t| t.id == "child-id")
            .expect("child still present");
        assert_eq!(
            child.parent_task_id.as_deref(),
            Some("parent-id"),
            "API-side parent_task_id must propagate to pd.tasks",
        );

        if let Some(p) = prev {
            std::env::set_var("HOME", p);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn create_subtask_uses_parent_projects_stored_repo_url() {
        // Subtask path: pi is resolved by walking project_data for
        // the task whose id == parent_task_id. We stage a parent task
        // in a project with a custom stored repo_url and confirm the
        // subtask inherits that URL (not the github fallback).
        let mut view = PlanningView::new();
        let stored = "git@gitlab.example.com:org/forked-repo.git";
        let mut pd = make_project("forked-repo", stored);
        pd.tasks.push(PlanTask {
            id: "parent-id-123".to_string(),
            slug: "parent".to_string(),
            title: "Parent task".to_string(),
            status: PlanStatus::Backlog,
            difficulty: None,
            depends: vec![],
            branch: None,
            created: None,
            description: String::new(),
            prompt: String::new(),
            source: "user".to_string(),
            is_cloud: false,
            repo_url: stored.to_string(),
            parent_task_id: None,
            kind: "oneshot".to_string(),
            worker_vm: None,
            vm_project: None,
            vm_zone: None,
            run_key: None,
            bt_label: None,
        });
        view.project_data.push(pd);
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();

        let (project, repo_url, parent) =
            extract_create(view.create_task("Sub task", Some("parent-id-123".to_string())));

        assert_eq!(project, "forked-repo");
        assert_eq!(repo_url, stored);
        assert_eq!(parent, Some("parent-id-123".to_string()));
    }

    // ── Sub-2a Finding #2: launch actions carry parent_task_id ───────
    //
    // Without this, a subtask launched via A-l / A-f publishes a
    // local TaskEntry stub with `parent_task_id: None`. The first
    // `push_task_tree_to_daemon` then sends the subtask to the
    // daemon as top-level — daemon's auth walk can't see the
    // parent → subtask edge until the next API reconcile, leaving
    // a window where descendant-task auth fails for actions that
    // SHOULD succeed.

    /// Pin: `LaunchTask` carries `parent_task_id` derived from the
    /// PlanTask. Drives the LaunchConfirm input mode through Enter
    /// and inspects the returned action.
    #[test]
    fn launch_task_action_carries_parent_task_id_for_subtask() {
        use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
        let mut view = PlanningView::new();
        let mut pd = make_project("repo", "git@example.com:org/repo.git");
        pd.tasks.push(PlanTask {
            id: "sub-id".to_string(),
            slug: "subtask".to_string(),
            title: "Subtask".to_string(),
            status: PlanStatus::Backlog,
            difficulty: None,
            depends: vec![],
            branch: None,
            created: None,
            description: String::new(),
            prompt: String::new(),
            source: "user".to_string(),
            is_cloud: false,
            repo_url: "git@example.com:org/repo.git".to_string(),
            parent_task_id: Some("parent-id".to_string()),
            kind: "oneshot".to_string(),
            worker_vm: None,
            vm_project: None,
            vm_zone: None,
            run_key: None,
            bt_label: None,
        });
        view.project_data.push(pd);
        view.projects = view
            .project_data
            .iter()
            .map(|pd| pd.project.clone())
            .collect();
        view.input_mode = PlanInputMode::LaunchConfirm {
            project_idx: 0,
            task_idx: 0,
            branch_text: String::new(),
            engine: LaunchEngine::Claude,
        };
        let enter = CrosstermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let action = view.handle_event(&enter);
        match action {
            PlanAction::LaunchTask { parent_task_id, task_id, .. } => {
                assert_eq!(task_id, "sub-id");
                assert_eq!(
                    parent_task_id,
                    Some("parent-id".to_string()),
                    "launch action must carry parent_task_id from PlanTask",
                );
            }
            other => panic!(
                "expected LaunchTask, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Top-level task (no parent) → `parent_task_id: None` rides
    /// through unchanged.
    #[test]
    fn launch_task_action_carries_none_for_top_level_task() {
        use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
        let mut view = PlanningView::new();
        let mut pd = make_project("repo", "git@example.com:org/repo.git");
        pd.tasks.push(PlanTask {
            id: "top-id".to_string(),
            slug: "toplevel".to_string(),
            title: "Top".to_string(),
            status: PlanStatus::Backlog,
            difficulty: None,
            depends: vec![],
            branch: None,
            created: None,
            description: String::new(),
            prompt: String::new(),
            source: "user".to_string(),
            is_cloud: false,
            repo_url: "git@example.com:org/repo.git".to_string(),
            parent_task_id: None,
            kind: "oneshot".to_string(),
            worker_vm: None,
            vm_project: None,
            vm_zone: None,
            run_key: None,
            bt_label: None,
        });
        view.project_data.push(pd);
        view.projects = view
            .project_data
            .iter()
            .map(|pd| pd.project.clone())
            .collect();
        view.input_mode = PlanInputMode::LaunchConfirm {
            project_idx: 0,
            task_idx: 0,
            branch_text: String::new(),
            engine: LaunchEngine::Claude,
        };
        let enter = CrosstermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let action = view.handle_event(&enter);
        match action {
            PlanAction::LaunchTask { parent_task_id, .. } => {
                assert_eq!(parent_task_id, None);
            }
            other => panic!(
                "expected LaunchTask, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// The `.` sentinel in the LaunchConfirm branch field launches
    /// in-place: `in_place: true`, `branch: None`. Empty branch stays
    /// `in_place: false`.
    #[test]
    fn launch_confirm_dot_branch_sets_in_place() {
        use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
        let enter = CrosstermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        let mk = || {
            let mut view = PlanningView::new();
            let mut pd = make_project("repo", "git@example.com:org/repo.git");
            pd.tasks.push(PlanTask {
                id: "top-id".to_string(),
                slug: "toplevel".to_string(),
                title: "Top".to_string(),
                status: PlanStatus::Backlog,
                difficulty: None,
                depends: vec![],
                branch: None,
                created: None,
                description: String::new(),
                prompt: String::new(),
                source: "user".to_string(),
                is_cloud: false,
                repo_url: "git@example.com:org/repo.git".to_string(),
                parent_task_id: None,
                kind: "oneshot".to_string(),
                worker_vm: None,
                vm_project: None,
                vm_zone: None,
                run_key: None,
                bt_label: None,
            });
            view.project_data.push(pd);
            view.projects = view
                .project_data
                .iter()
                .map(|pd| pd.project.clone())
                .collect();
            view
        };

        // "." → in-place.
        let mut view = mk();
        view.input_mode = PlanInputMode::LaunchConfirm {
            project_idx: 0,
            task_idx: 0,
            branch_text: ".".to_string(),
            engine: LaunchEngine::Claude,
        };
        match view.handle_event(&enter) {
            PlanAction::LaunchTask { in_place, branch, .. } => {
                assert!(in_place, "`.` must launch in-place");
                assert!(branch.is_none(), "in-place carries no branch");
            }
            other => panic!("expected LaunchTask, got {:?}", std::mem::discriminant(&other)),
        }

        // empty → normal worktree launch.
        let mut view = mk();
        view.input_mode = PlanInputMode::LaunchConfirm {
            project_idx: 0,
            task_idx: 0,
            branch_text: String::new(),
            engine: LaunchEngine::Claude,
        };
        match view.handle_event(&enter) {
            PlanAction::LaunchTask { in_place, .. } => {
                assert!(!in_place, "empty branch is not in-place");
            }
            other => panic!("expected LaunchTask, got {:?}", std::mem::discriminant(&other)),
        }
    }

    // ── Launch engine picker (claude | codex) ────────────────

    /// Build a one-task planning view for the launch-dialog tests.
    #[cfg(test)]
    fn view_with_one_task() -> PlanningView {
        let mut view = PlanningView::new();
        let mut pd = make_project("repo", "git@example.com:org/repo.git");
        pd.tasks.push(PlanTask {
            id: "top-id".to_string(),
            slug: "toplevel".to_string(),
            title: "Top".to_string(),
            status: PlanStatus::Backlog,
            difficulty: None,
            depends: vec![],
            branch: None,
            created: None,
            description: String::new(),
            prompt: String::new(),
            source: "user".to_string(),
            is_cloud: false,
            repo_url: "git@example.com:org/repo.git".to_string(),
            parent_task_id: None,
            kind: "oneshot".to_string(),
            worker_vm: None,
            vm_project: None,
            vm_zone: None,
            run_key: None,
            bt_label: None,
        });
        view.project_data.push(pd);
        view.projects = view
            .project_data
            .iter()
            .map(|pd| pd.project.clone())
            .collect();
        view
    }

    /// Default is Claude, and ←/→ toggles to Codex. The chosen engine
    /// rides out on the `LaunchTask` action — the launch site spawns
    /// that session_type instead of the old hardcoded "claude".
    #[test]
    fn launch_confirm_engine_defaults_claude_and_arrow_toggles() {
        use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
        let key = |c: KeyCode| CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE));

        // Untouched → claude.
        let mut view = view_with_one_task();
        view.input_mode = PlanInputMode::LaunchConfirm {
            project_idx: 0,
            task_idx: 0,
            branch_text: String::new(),
            engine: LaunchEngine::default(),
        };
        match view.handle_event(&key(KeyCode::Enter)) {
            PlanAction::LaunchTask { engine, .. } => {
                assert_eq!(engine, LaunchEngine::Claude, "default engine is claude");
                assert_eq!(engine.as_session_type(), "claude");
            }
            other => panic!("expected LaunchTask, got {:?}", std::mem::discriminant(&other)),
        }

        // One → (or ←) → codex.
        for toggle in [KeyCode::Right, KeyCode::Left, KeyCode::Tab] {
            let mut view = view_with_one_task();
            view.input_mode = PlanInputMode::LaunchConfirm {
                project_idx: 0,
                task_idx: 0,
                branch_text: String::new(),
                engine: LaunchEngine::default(),
            };
            view.handle_event(&key(toggle));
            match view.handle_event(&key(KeyCode::Enter)) {
                PlanAction::LaunchTask { engine, .. } => {
                    assert_eq!(engine, LaunchEngine::Codex, "{:?} must select codex", toggle);
                    assert_eq!(engine.as_session_type(), "codex");
                }
                other => panic!("expected LaunchTask, got {:?}", std::mem::discriminant(&other)),
            }
        }
    }

    /// The engine toggle must not steal characters from the branch
    /// field — it's a free-text input, so `h`/`l`/`j`/`k` are branch
    /// name characters, not navigation.
    #[test]
    fn launch_confirm_letters_still_edit_branch_text() {
        use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
        let key = |c: char| {
            CrosstermEvent::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
        };
        let mut view = view_with_one_task();
        view.input_mode = PlanInputMode::LaunchConfirm {
            project_idx: 0,
            task_idx: 0,
            branch_text: String::new(),
            engine: LaunchEngine::default(),
        };
        for c in "hjkl".chars() {
            view.handle_event(&key(c));
        }
        let enter = CrosstermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        match view.handle_event(&enter) {
            PlanAction::LaunchTask { branch, engine, .. } => {
                assert_eq!(branch.as_deref(), Some("hjkl"));
                assert_eq!(engine, LaunchEngine::Claude, "letters must not cycle engine");
            }
            other => panic!("expected LaunchTask, got {:?}", std::mem::discriminant(&other)),
        }
    }

    /// The picker's engine choice survives the hop into the branch
    /// dialog ("New workspace" route) — the operator picks once.
    #[test]
    fn workspace_picker_engine_rides_into_launch_confirm() {
        use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
        let key = |c: KeyCode| CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE));
        let mut view = view_with_one_task();
        view.input_mode = PlanInputMode::WorkspacePicker {
            project_idx: 0,
            task_idx: 0,
            selected: 0,
            engine: LaunchEngine::default(),
        };
        view.handle_event(&key(KeyCode::Right));
        view.handle_event(&key(KeyCode::Enter));
        match view.input_mode {
            PlanInputMode::LaunchConfirm { engine, .. } => {
                assert_eq!(engine, LaunchEngine::Codex, "engine carries into LaunchConfirm");
            }
            _ => panic!("expected LaunchConfirm after selecting 'New workspace'"),
        }
    }

    /// The existing-workspace route honors the picker's engine too, so
    /// both launch paths can start a codex worker.
    #[test]
    fn workspace_picker_engine_rides_into_existing_workspace_launch() {
        use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
        let key = |c: KeyCode| CrosstermEvent::Key(KeyEvent::new(c, KeyModifiers::NONE));
        let mut view = view_with_one_task();
        view.set_workspace_candidates(vec![WorkspaceCandidate {
            workspace_id: "ws-1".to_string(),
            name: "repo-toplevel".to_string(),
            repo_url: Some("https://example.com/org/repo.git".to_string()),
        }]);
        view.input_mode = PlanInputMode::WorkspacePicker {
            project_idx: 0,
            task_idx: 0,
            // 0 is "New workspace"; 1 is the candidate above.
            selected: 1,
            engine: LaunchEngine::default(),
        };
        view.handle_event(&key(KeyCode::Right));
        match view.handle_event(&key(KeyCode::Enter)) {
            PlanAction::LaunchTaskIntoWorkspace { engine, workspace_id, .. } => {
                assert_eq!(workspace_id, "ws-1");
                assert_eq!(engine, LaunchEngine::Codex);
            }
            other => panic!(
                "expected LaunchTaskIntoWorkspace, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    // ── Frontmatter round-trip tests ─────────────────────────
    //
    // These pin the temp-file editor's YAML pipeline. Pre-fix, the
    // formatter built frontmatter via `format!("title: {}", ...)` and
    // any value containing a YAML metacharacter (`:`, `#`, `,`, `[`,
    // quotes, newlines, …) either silently mangled or made the file
    // un-parseable. Each test exercises a class of dangerous input.

    fn make_plan_task(slug: &str) -> PlanTask {
        PlanTask {
            id: format!("id-{}", slug),
            slug: slug.to_string(),
            title: "default title".to_string(),
            status: PlanStatus::Backlog,
            difficulty: None,
            depends: vec![],
            branch: None,
            created: None,
            description: String::new(),
            prompt: String::new(),
            source: "user".to_string(),
            is_cloud: false,
            repo_url: String::new(),
            parent_task_id: None,
            kind: "oneshot".to_string(),
            worker_vm: None,
            vm_project: None,
            vm_zone: None,
            run_key: None,
            bt_label: None,
        }
    }

    fn round_trip(task: &PlanTask) -> TempTaskParsed {
        let path = write_temp_task(task, None).expect("write_temp_task");
        let parsed = parse_temp_task(&path).expect("parse_temp_task");
        let _ = std::fs::remove_file(&path);
        parsed
    }

    #[test]
    fn frontmatter_title_with_colon_round_trips() {
        let mut task = make_plan_task("yaml-rt-colon");
        task.title = "feature: refactor X".to_string();
        let parsed = round_trip(&task);
        assert_eq!(parsed.title, "feature: refactor X");
    }

    #[test]
    fn frontmatter_branch_with_slash_round_trips() {
        let mut task = make_plan_task("yaml-rt-slash");
        task.branch = Some("user/bar".to_string());
        let parsed = round_trip(&task);
        assert_eq!(parsed.branch, FieldUpdate::Set("user/bar".to_string()));
    }

    #[test]
    fn frontmatter_depends_with_comma_round_trips() {
        let mut task = make_plan_task("yaml-rt-comma");
        task.depends = vec!["foo,bar".to_string(), "baz".to_string()];
        let parsed = round_trip(&task);
        assert_eq!(
            parsed.depends,
            FieldUpdate::Set(vec!["foo,bar".to_string(), "baz".to_string()])
        );
    }

    #[test]
    fn body_multiline_description_round_trips() {
        let mut task = make_plan_task("yaml-rt-multiline");
        task.description = "line one\nline two\nline three".to_string();
        let parsed = round_trip(&task);
        assert_eq!(parsed.description, "line one\nline two\nline three");
    }

    #[test]
    fn frontmatter_title_with_hash_round_trips() {
        let mut task = make_plan_task("yaml-rt-hash");
        task.title = "feature # not a comment".to_string();
        let parsed = round_trip(&task);
        assert_eq!(parsed.title, "feature # not a comment");
    }

    #[test]
    fn frontmatter_already_quoted_title_round_trips() {
        let mut task = make_plan_task("yaml-rt-quoted");
        task.title = "\"already quoted\"".to_string();
        let parsed = round_trip(&task);
        assert_eq!(parsed.title, "\"already quoted\"");
    }

    #[test]
    fn parse_temp_task_tolerates_null_depends() {
        // User cleared dependencies in the editor and left `depends:`
        // behind with no list items — YAML parses that as null. Pre-fix,
        // a bare `Vec<String>` field would fail to deserialize null and
        // the whole parse returned None, silently dropping every edit
        // on the same save. Lock in tolerance.
        let dir = std::env::temp_dir().join("cm-planning");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("yaml-rt-null-depends.md");
        let raw = "---\ntitle: still here\nstatus: backlog\ndepends:\n---\n\nbody\n";
        std::fs::write(&path, raw).unwrap();

        let parsed = parse_temp_task(&path).expect("parse must succeed with null depends");
        let _ = std::fs::remove_file(&path);

        assert_eq!(parsed.title, "still here");
        assert_eq!(parsed.status, "backlog");
        assert_eq!(parsed.depends, FieldUpdate::Cleared);
    }

    // ── PATCH-shape tests for the cleared-field fix (F16) ─────
    //
    // The temp-file editor used to gate every nullable field behind
    // `if let Some(...)` / `!is_empty()`, so deleting a field's value in
    // the editor produced no PATCH key for it — the next refresh
    // happily reloaded the stale stored value and the user's clear
    // was silently undone. Each test here pins one corner of the
    // fix: the PATCH must carry the user's intent, including clears.
    //
    // Known dependency: F17 lands the API end (PATCH must accept
    // `null` / `[]` and propagate the clear down to the DB row).
    // These tests assert the wire shape only.
    fn write_temp(name: &str, raw: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("cm-planning");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, raw).unwrap();
        path
    }

    #[test]
    fn cleared_difficulty_in_patch_is_null() {
        // User opened the editor on a task with difficulty: 3 and
        // erased the value, leaving `difficulty:` (null).
        let path = write_temp(
            "f16-clear-difficulty.md",
            "---\ntitle: t\nstatus: backlog\ndifficulty:\n---\n\nbody\n",
        );
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);

        assert_eq!(parsed.difficulty, FieldUpdate::Cleared);
        let fields = build_patch_fields(&parsed);
        assert_eq!(
            fields.get("difficulty"),
            Some(&serde_json::Value::Null),
            "cleared difficulty must PATCH as JSON null, not be omitted"
        );
    }

    #[test]
    fn cleared_depends_in_patch_is_empty_array() {
        // User had depends: [a, b] and emptied it to depends: [].
        let path = write_temp(
            "f16-clear-depends-empty-list.md",
            "---\ntitle: t\nstatus: backlog\ndepends: []\n---\n\nbody\n",
        );
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);

        assert_eq!(parsed.depends, FieldUpdate::Cleared);
        let fields = build_patch_fields(&parsed);
        assert_eq!(
            fields.get("depends"),
            Some(&serde_json::json!([] as [String; 0])),
            "cleared depends must PATCH as empty array, not be omitted"
        );
    }

    #[test]
    fn cleared_branch_in_patch_is_null() {
        // User had branch: main and erased the value.
        let path = write_temp(
            "f16-clear-branch.md",
            "---\ntitle: t\nstatus: backlog\nbranch:\n---\n\nbody\n",
        );
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);

        assert_eq!(parsed.branch, FieldUpdate::Cleared);
        let fields = build_patch_fields(&parsed);
        assert_eq!(
            fields.get("repo_branch"),
            Some(&serde_json::Value::Null),
            "cleared branch must PATCH repo_branch as JSON null, not be omitted"
        );
    }

    #[test]
    fn untouched_difficulty_omitted_from_patch() {
        // User saves the editor without touching difficulty: 3 — the
        // YAML still says `difficulty: 3`, so the PATCH should reassert
        // the same value and NOT spuriously include null.
        let path = write_temp(
            "f16-keep-difficulty.md",
            "---\ntitle: t\nstatus: backlog\ndifficulty: 3\n---\n\nbody\n",
        );
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);

        assert_eq!(parsed.difficulty, FieldUpdate::Set(3));
        let fields = build_patch_fields(&parsed);
        assert_eq!(fields.get("difficulty"), Some(&serde_json::json!(3)));

        // And: if difficulty is genuinely absent from the YAML (was
        // None to begin with, user didn't add it), it must be omitted
        // entirely so the API doesn't see a phantom update.
        let path2 = write_temp(
            "f16-absent-difficulty.md",
            "---\ntitle: t\nstatus: backlog\n---\n\nbody\n",
        );
        let parsed2 = parse_temp_task(&path2).expect("parse");
        let _ = std::fs::remove_file(&path2);

        assert_eq!(parsed2.difficulty, FieldUpdate::Absent);
        let fields2 = build_patch_fields(&parsed2);
        assert!(
            !fields2.contains_key("difficulty"),
            "absent difficulty must be omitted from PATCH (no spurious update)"
        );
        assert!(!fields2.contains_key("depends"));
        assert!(!fields2.contains_key("repo_branch"));
    }

    #[test]
    fn cleared_depends_via_null_in_patch_is_empty_array() {
        // Sibling of cleared_depends_in_patch_is_empty_array: when the
        // user empties the YAML by leaving `depends:` (null) — same
        // pre-fix path as the original null-depends regression — the
        // PATCH must still surface as `[]`.
        let path = write_temp(
            "f16-clear-depends-null.md",
            "---\ntitle: t\nstatus: backlog\ndepends:\n---\n\nbody\n",
        );
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);

        assert_eq!(parsed.depends, FieldUpdate::Cleared);
        let fields = build_patch_fields(&parsed);
        assert_eq!(
            fields.get("depends"),
            Some(&serde_json::json!([] as [String; 0])),
        );
    }

    // ── parent field round-trip + resolver tests ─────────────
    //
    // The `parent` field lives in the editor's YAML frontmatter, follows
    // the same Absent/Cleared/Set semantics as depends/branch, and gets
    // resolved from a slug to a `parent_task_id` PATCH key by
    // `resolve_parent_patch` (which also enforces same-project scope and
    // no-cycle).

    fn task_with_parent(slug: &str, id: &str, parent_id: Option<&str>) -> PlanTask {
        let mut t = make_plan_task(slug);
        t.id = id.to_string();
        t.parent_task_id = parent_id.map(|s| s.to_string());
        t
    }

    #[test]
    fn frontmatter_parent_round_trips() {
        let mut task = make_plan_task("child");
        task.parent_task_id = Some("id-parent".to_string());
        let path = write_temp_task(&task, Some("parent")).expect("write");
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(parsed.parent, FieldUpdate::Set("parent".to_string()));
    }

    #[test]
    fn frontmatter_no_parent_emits_no_key() {
        let task = make_plan_task("orphan");
        let path = write_temp_task(&task, None).expect("write");
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);
        // Without `parent:` in the YAML, parse must surface Absent so
        // the PATCH builder skips the key.
        assert_eq!(parsed.parent, FieldUpdate::Absent);
    }

    #[test]
    fn parent_cleared_via_empty_value_resolves_to_null() {
        let path = write_temp(
            "parent-clear.md",
            "---\ntitle: t\nstatus: backlog\nparent:\n---\n\nbody\n",
        );
        let parsed = parse_temp_task(&path).expect("parse");
        let _ = std::fs::remove_file(&path);
        assert_eq!(parsed.parent, FieldUpdate::Cleared);

        let tasks = vec![task_with_parent("me", "id-me", Some("id-old"))];
        let out = resolve_parent_patch(&tasks, "id-me", &parsed.parent)
            .expect("clear must succeed");
        assert_eq!(out, Some(serde_json::Value::Null));
    }

    #[test]
    fn parent_set_to_known_slug_resolves_to_id() {
        let tasks = vec![
            task_with_parent("me", "id-me", None),
            task_with_parent("other", "id-other", None),
        ];
        let field = FieldUpdate::Set("other".to_string());
        let out = resolve_parent_patch(&tasks, "id-me", &field).expect("ok");
        assert_eq!(out, Some(serde_json::json!("id-other")));
    }

    #[test]
    fn parent_unknown_slug_is_error() {
        let tasks = vec![task_with_parent("me", "id-me", None)];
        let field = FieldUpdate::Set("nope".to_string());
        let err = resolve_parent_patch(&tasks, "id-me", &field).unwrap_err();
        assert!(err.contains("nope"), "msg names the missing slug: {}", err);
    }

    #[test]
    fn parent_self_is_error() {
        let tasks = vec![task_with_parent("me", "id-me", None)];
        let field = FieldUpdate::Set("me".to_string());
        let err = resolve_parent_patch(&tasks, "id-me", &field).unwrap_err();
        assert!(err.contains("own parent"), "msg names self-cycle: {}", err);
    }

    #[test]
    fn parent_descendant_is_cycle_error() {
        // Tree: me → child → grandchild. Setting parent=grandchild on me
        // would form a cycle.
        let tasks = vec![
            task_with_parent("me", "id-me", None),
            task_with_parent("child", "id-child", Some("id-me")),
            task_with_parent("grand", "id-grand", Some("id-child")),
        ];
        let field = FieldUpdate::Set("grand".to_string());
        let err = resolve_parent_patch(&tasks, "id-me", &field).unwrap_err();
        assert!(err.contains("cycle"), "msg names cycle: {}", err);
    }

    #[test]
    fn parent_sibling_subtree_is_allowed() {
        // Tree:
        //   root-a (root)
        //   root-b (root)
        //   child-a (parent = root-a)
        // Reparenting child-a → root-b is allowed: root-b isn't a
        // descendant of child-a.
        let tasks = vec![
            task_with_parent("root-a", "id-a", None),
            task_with_parent("root-b", "id-b", None),
            task_with_parent("child-a", "id-ca", Some("id-a")),
        ];
        let field = FieldUpdate::Set("root-b".to_string());
        let out = resolve_parent_patch(&tasks, "id-ca", &field).expect("ok");
        assert_eq!(out, Some(serde_json::json!("id-b")));
    }

    #[test]
    fn parent_absent_yields_no_patch_entry() {
        let tasks = vec![task_with_parent("me", "id-me", None)];
        let out = resolve_parent_patch(&tasks, "id-me", &FieldUpdate::Absent)
            .expect("absent is ok");
        assert_eq!(out, None);
    }

    #[test]
    fn deleting_parent_line_on_parented_task_clears_it() {
        // The user opens a subtask, deletes the `parent:` line entirely,
        // and saves. Parse surfaces Absent; because the task currently has
        // a parent, that's an explicit detach → Cleared → PATCH null.
        let field = effective_parent_update(&FieldUpdate::Absent, true);
        assert_eq!(field, FieldUpdate::Cleared);

        let tasks = vec![task_with_parent("me", "id-me", Some("id-old"))];
        let out = resolve_parent_patch(&tasks, "id-me", &field)
            .expect("clear must succeed");
        assert_eq!(out, Some(serde_json::Value::Null));
    }

    #[test]
    fn absent_parent_on_orphan_stays_no_op() {
        // A task with no parent must keep Absent → no PATCH entry, so a
        // plain edit of an orphan never spuriously writes parent_task_id.
        let field = effective_parent_update(&FieldUpdate::Absent, false);
        assert_eq!(field, FieldUpdate::Absent);

        let tasks = vec![task_with_parent("me", "id-me", None)];
        let out = resolve_parent_patch(&tasks, "id-me", &field).expect("ok");
        assert_eq!(out, None);
    }

    #[test]
    fn effective_parent_update_passes_through_set_and_cleared() {
        // Promotion only touches Absent; Set/Cleared are returned verbatim
        // regardless of whether the task currently has a parent.
        let set = FieldUpdate::Set("other".to_string());
        assert_eq!(effective_parent_update(&set, true), set);
        assert_eq!(effective_parent_update(&set, false), set);
        assert_eq!(
            effective_parent_update(&FieldUpdate::Cleared, true),
            FieldUpdate::Cleared
        );
    }

    // -- compose_launch_prompt --

    #[test]
    fn compose_includes_both_when_both_present() {
        // The agent only sees ONE string at spawn. Description carries
        // background; prompt carries instructions. Combine with a
        // delimiter so both are visible and the worker can parse out
        // which is which.
        let out = compose_launch_prompt(
            "Background paragraph explaining motivation.",
            "Step 1: do this. Step 2: do that.",
            "Title",
        );
        assert!(out.starts_with("Background paragraph"), "{out}");
        assert!(out.contains("\n\n---\n\n"), "delimiter missing: {out}");
        assert!(out.ends_with("Step 2: do that."), "{out}");
    }

    #[test]
    fn compose_falls_back_to_prompt_only_when_no_description() {
        let out = compose_launch_prompt("", "Just instructions.", "T");
        assert_eq!(out, "Just instructions.");
    }

    #[test]
    fn compose_falls_back_to_description_when_no_prompt() {
        // Description-only is unusual but possible. Better to ship
        // SOMETHING to the worker than nothing.
        let out = compose_launch_prompt("Just background.", "", "T");
        assert_eq!(out, "Just background.");
    }

    #[test]
    fn compose_falls_back_to_title_when_neither() {
        // Last-resort safety net so the worker never spawns with an
        // empty prompt — the empty case used to send the agent in
        // with nothing to do.
        let out = compose_launch_prompt("", "", "Untitled task");
        assert_eq!(out, "Untitled task");
    }

    #[test]
    fn compose_treats_whitespace_as_empty() {
        // A description that's all whitespace shouldn't trigger the
        // "both present" branch and produce a delimiter with nothing
        // before it.
        let out = compose_launch_prompt("   \n  ", "real prompt", "T");
        assert_eq!(out, "real prompt");
        let out = compose_launch_prompt("real desc", "  \t", "T");
        assert_eq!(out, "real desc");
    }

    // ── A-J/K reorder on synthetic subtask rows ─────────────────
    //
    // Subtasks have no raw position in their own column (they render
    // under the parent). A-J/K used to no-op on synthetic rows; now
    // they swap with the adjacent sibling under the same parent, and
    // bail at the sibling-list boundary so subtasks never leak out of
    // the parent's slots.

    fn setup_subtask_reorder_view() -> PlanningView {
        // Layout: column 0 holds parent + three children in order
        // [c1, c2, c3]; child slugs are also their task ids for clarity.
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("p1", "parent", None),
            make_task("c1", "c1", Some("p1")),
            make_task("c2", "c2", Some("p1")),
            make_task("c3", "c3", Some("p1")),
        ];
        pd.layout.columns = vec![vec![
            GridItem::Task("parent".to_string()),
            GridItem::Task("c1".to_string()),
            GridItem::Task("c2".to_string()),
            GridItem::Task("c3".to_string()),
        ]];
        view.project_data.push(pd);
        view.rebuild_unified_cols();
        view.expanded_tasks.insert("p1".to_string());
        view
    }

    fn column_slugs(view: &PlanningView, pi: usize, ci: usize) -> Vec<String> {
        view.project_data[pi].layout.columns[ci]
            .iter()
            .filter_map(|item| match item {
                GridItem::Task(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn reorder_subtask_swaps_with_next_sibling() {
        let mut view = setup_subtask_reorder_view();
        // Visible rows in column 0: [parent, c1, c2, c3]. Cursor on c2.
        view.cursor = GridCursor { col: 0, row: 2 };
        view.reorder_task(1);
        assert_eq!(column_slugs(&view, 0, 0), vec!["parent", "c1", "c3", "c2"]);
        // Cursor follows the moved subtask: c2 is now at visible row 3.
        let row = view.cursor_visible_row().expect("row");
        assert!(matches!(row.kind, VisibleRowKind::Subtask { slug } if slug == "c2"));
    }

    #[test]
    fn reorder_subtask_swaps_with_previous_sibling() {
        let mut view = setup_subtask_reorder_view();
        // Cursor on c2.
        view.cursor = GridCursor { col: 0, row: 2 };
        view.reorder_task(-1);
        assert_eq!(column_slugs(&view, 0, 0), vec!["parent", "c2", "c1", "c3"]);
        let row = view.cursor_visible_row().expect("row");
        assert!(matches!(row.kind, VisibleRowKind::Subtask { slug } if slug == "c2"));
    }

    #[test]
    fn reorder_subtask_at_top_of_siblings_is_noop() {
        let mut view = setup_subtask_reorder_view();
        // Cursor on c1 (first sibling).
        view.cursor = GridCursor { col: 0, row: 1 };
        view.reorder_task(-1);
        // Layout unchanged — c1 stays at top of sibling list.
        assert_eq!(column_slugs(&view, 0, 0), vec!["parent", "c1", "c2", "c3"]);
    }

    #[test]
    fn reorder_subtask_at_bottom_of_siblings_is_noop() {
        let mut view = setup_subtask_reorder_view();
        // Cursor on c3 (last sibling).
        view.cursor = GridCursor { col: 0, row: 3 };
        view.reorder_task(1);
        // Crucially, c3 must NOT escape out of the parent's slots
        // (e.g. by appending an Empty and swapping past it).
        assert_eq!(column_slugs(&view, 0, 0), vec!["parent", "c1", "c2", "c3"]);
    }

    #[test]
    fn reorder_subtask_swaps_across_columns() {
        // Children of the same parent can live in different columns.
        // The sibling order is column-walk order (left→right, then
        // top→bottom), so swapping across columns is a real case.
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("p1", "parent", None),
            make_task("c1", "c1", Some("p1")),
            make_task("c2", "c2", Some("p1")),
        ];
        // Parent + c1 in col 0; c2 in col 1. Sibling list = [c1, c2].
        pd.layout.columns = vec![
            vec![
                GridItem::Task("parent".to_string()),
                GridItem::Task("c1".to_string()),
            ],
            vec![GridItem::Task("c2".to_string())],
        ];
        view.project_data.push(pd);
        view.rebuild_unified_cols();
        view.expanded_tasks.insert("p1".to_string());

        // Cursor on c1 (visible row 1 of col 0). Move down to swap
        // with c2, which lives in col 1.
        view.cursor = GridCursor { col: 0, row: 1 };
        view.reorder_task(1);
        // c1 and c2 have swapped raw positions across columns.
        assert_eq!(column_slugs(&view, 0, 0), vec!["parent", "c2"]);
        assert_eq!(column_slugs(&view, 0, 1), vec!["c1"]);
    }

    // ── insert_anchor_raw_idx ───────────────────────────────────
    //
    // When the cursor is on a synthetic subtask row, the old
    // `anchor_raw_idx` returned None and `on_task_created`'s fallback
    // appended to `column.len() - 1` — i.e. end of column, "way at the
    // bottom of the screen". The new insert anchor walks up to the
    // top-level ancestor in the cursor's column so new tasks land just
    // after the parent's subtree.

    #[test]
    fn insert_anchor_returns_top_level_ancestor_for_subtask_cursor() {
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("top", "top", None),
            make_task("child", "child", Some("top")),
            make_task("grand", "grand", Some("child")),
            make_task("tail", "tail", None),
        ];
        // Column 0 raw layout: [top, child, grand, tail].
        // With top expanded and child expanded, visible rows are
        // [top, child, grand, tail].
        pd.layout.columns = vec![vec![
            GridItem::Task("top".to_string()),
            GridItem::Task("child".to_string()),
            GridItem::Task("grand".to_string()),
            GridItem::Task("tail".to_string()),
        ]];
        view.project_data.push(pd);
        view.rebuild_unified_cols();
        view.expanded_tasks.insert("top".to_string());
        view.expanded_tasks.insert("child".to_string());

        // Cursor on `grand` (depth-2 subtask, visible row 2).
        view.cursor = GridCursor { col: 0, row: 2 };
        // Sanity: grand IS a synthetic row (no raw_idx).
        assert!(view.anchor_raw_idx().is_none());
        // Insert anchor walks up to `top` (raw idx 0), so inserts land
        // at raw idx 1 — directly after the parent's subtree once the
        // subtree is filtered out of visible rendering.
        assert_eq!(view.insert_anchor_raw_idx(), Some(0));
    }

    #[test]
    fn insert_anchor_passes_through_for_layout_cursor() {
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("a", "a", None),
            make_task("b", "b", None),
            make_task("c", "c", None),
        ];
        pd.layout.columns = vec![vec![
            GridItem::Task("a".to_string()),
            GridItem::Task("b".to_string()),
            GridItem::Task("c".to_string()),
        ]];
        view.project_data.push(pd);
        view.rebuild_unified_cols();

        view.cursor = GridCursor { col: 0, row: 1 };
        // Normal row → same as anchor_raw_idx.
        assert_eq!(view.insert_anchor_raw_idx(), Some(1));
        assert_eq!(view.anchor_raw_idx(), Some(1));
    }

    #[test]
    fn reorder_subtask_never_crosses_into_other_parents_children() {
        // Two parents, each with two children. Cursor on parent A's
        // last child — moving down must NOT swap it with parent B's
        // first child even though that's the next visible task in
        // column-walk order.
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        pd.tasks = vec![
            make_task("pa", "pa", None),
            make_task("pb", "pb", None),
            make_task("a1", "a1", Some("pa")),
            make_task("a2", "a2", Some("pa")),
            make_task("b1", "b1", Some("pb")),
            make_task("b2", "b2", Some("pb")),
        ];
        pd.layout.columns = vec![vec![
            GridItem::Task("pa".to_string()),
            GridItem::Task("a1".to_string()),
            GridItem::Task("a2".to_string()),
            GridItem::Task("pb".to_string()),
            GridItem::Task("b1".to_string()),
            GridItem::Task("b2".to_string()),
        ]];
        view.project_data.push(pd);
        view.rebuild_unified_cols();
        view.expanded_tasks.insert("pa".to_string());
        view.expanded_tasks.insert("pb".to_string());

        // Visible rows in col 0: [pa, a1, a2, pb, b1, b2]. Cursor on a2.
        view.cursor = GridCursor { col: 0, row: 2 };
        view.reorder_task(1);
        // a2 is the last child of pa — must stay put.
        assert_eq!(
            column_slugs(&view, 0, 0),
            vec!["pa", "a1", "a2", "pb", "b1", "b2"],
        );
    }

    // ── `/` search: match list + n/N navigation ──────────────────────

    /// Board for the search tests: two columns, a folded subtask, and
    /// titles/descriptions arranged so a query can hit each field.
    fn make_search_view() -> PlanningView {
        let mut view = PlanningView::new();
        let mut pd = make_project("p", "");
        let mut alpha = make_task("a", "alpha", None);
        alpha.title = "Fix Alpha widget".to_string();
        let mut child = make_task("b", "alpha-child", Some("a"));
        child.title = "alpha follow-up".to_string();
        let mut beta = make_task("c", "beta", None);
        beta.title = "Beta cleanup".to_string();
        beta.description = "also touches the ALPHA path".to_string();
        let mut gamma = make_task("d", "gamma", None);
        gamma.title = "Unrelated".to_string();
        pd.tasks = vec![alpha, child, beta, gamma];
        pd.layout.columns = vec![
            vec![
                GridItem::Task("alpha".to_string()),
                GridItem::Task("alpha-child".to_string()),
            ],
            vec![
                GridItem::Task("gamma".to_string()),
                GridItem::Task("beta".to_string()),
            ],
        ];
        view.project_data.push(pd);
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();
        view.rebuild_unified_cols();
        view
    }

    #[test]
    fn search_matches_ordered_by_board_walk_including_folded_subtasks() {
        let view = make_search_view();
        // "alpha" hits: alpha (title), alpha-child (title, FOLDED under
        // alpha), beta (description only). Board order: col 0 top-down
        // (parent then its subtask), then col 1.
        assert_eq!(view.compute_search_matches("alpha"), vec!["a", "b", "c"]);
        // Case-insensitive both ways: query uppercase, field uppercase.
        assert_eq!(view.compute_search_matches("ALPHA"), vec!["a", "b", "c"]);
        // No matches → empty; empty query → empty.
        assert!(view.compute_search_matches("zzz").is_empty());
        assert!(view.compute_search_matches("").is_empty());
    }

    #[test]
    fn search_matches_skip_archived_when_hidden() {
        let mut view = make_search_view();
        view.project_data[0].tasks.iter_mut()
            .find(|t| t.id == "c").unwrap().status = PlanStatus::Archived;
        assert_eq!(
            view.compute_search_matches("alpha"),
            vec!["a", "b"],
            "hidden archived rows must not match",
        );
        view.show_archived = true;
        assert_eq!(view.compute_search_matches("alpha"), vec!["a", "b", "c"]);
    }

    #[test]
    fn next_search_index_wraps_both_ways() {
        // Stepping from a known position wraps at both ends.
        assert_eq!(next_search_index(3, Some(0), 1), Some(1));
        assert_eq!(next_search_index(3, Some(2), 1), Some(0));
        assert_eq!(next_search_index(3, Some(0), -1), Some(2));
        // Cursor not on a match: n enters at the first, N at the last.
        assert_eq!(next_search_index(3, None, 1), Some(0));
        assert_eq!(next_search_index(3, None, -1), Some(2));
        // Empty list navigates nowhere.
        assert_eq!(next_search_index(0, Some(1), 1), None);
        assert_eq!(next_search_index(0, None, -1), None);
        // Single-element list is a fixed point.
        assert_eq!(next_search_index(1, Some(0), 1), Some(0));
    }

    #[test]
    fn find_ci_ranges_locates_case_insensitive_substrings() {
        assert_eq!(find_ci_ranges("Fix Alpha widget", "alpha"), vec![(4, 9)]);
        assert_eq!(find_ci_ranges("aAaA", "aa"), vec![(0, 2), (2, 4)]);
        assert!(find_ci_ranges("nothing here", "alpha").is_empty());
        assert!(find_ci_ranges("anything", "").is_empty());
        // Multibyte haystack: ranges stay on char boundaries.
        assert_eq!(find_ci_ranges("\u{2264}Alpha", "alpha"), vec![(3, 8)]);
    }

    #[test]
    fn bare_n_jumps_to_next_match_and_auto_unfolds_parent() {
        let mut view = make_search_view();
        view.last_search = Some("alpha".to_string());
        view.cursor = GridCursor { col: 0, row: 0 };

        let n_key = CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
        ));

        // Cursor sits on match #1 ("a"), so n steps to #2: the folded
        // subtask "b". Its parent must auto-unfold and the cursor land
        // on the synthetic subtask row.
        assert!(matches!(view.handle_event(&n_key), PlanAction::Consumed));
        assert!(view.expanded_tasks.contains("a"), "parent should auto-unfold");
        assert_eq!((view.cursor.col, view.cursor.row), (0, 1));
        match view.cursor_visible_row().expect("row").kind {
            VisibleRowKind::Subtask { ref slug } => assert_eq!(slug, "alpha-child"),
            other => panic!("expected Subtask row, got {:?}", other),
        }
        assert_eq!(view.search_status.as_deref(), Some("match 2/3: alpha"));

        // n again → beta in column 1; n once more wraps to alpha.
        assert!(matches!(view.handle_event(&n_key), PlanAction::Consumed));
        assert_eq!((view.cursor.col, view.cursor.row), (1, 1));
        assert_eq!(view.search_status.as_deref(), Some("match 3/3: alpha"));
        assert!(matches!(view.handle_event(&n_key), PlanAction::Consumed));
        assert_eq!((view.cursor.col, view.cursor.row), (0, 0));
        assert_eq!(view.search_status.as_deref(), Some("match 1/3: alpha"));

        // Shift+N steps backwards (wraps to beta).
        let shift_n = CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('N'),
            KeyModifiers::SHIFT,
        ));
        assert!(matches!(view.handle_event(&shift_n), PlanAction::Consumed));
        assert_eq!((view.cursor.col, view.cursor.row), (1, 1));
        assert_eq!(view.search_status.as_deref(), Some("match 3/3: alpha"));
    }

    #[test]
    fn bare_n_without_stored_search_stays_inert() {
        let mut view = make_search_view();
        view.cursor = GridCursor { col: 0, row: 0 };
        let n_key = CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::NONE,
        ));
        assert!(matches!(view.handle_event(&n_key), PlanAction::Ignored));
        assert_eq!((view.cursor.col, view.cursor.row), (0, 0));
    }

    #[test]
    fn incremental_search_moves_live_and_esc_restores() {
        let mut view = make_search_view();
        view.cursor = GridCursor { col: 1, row: 0 };

        let key = |code: KeyCode, mods: KeyModifiers| {
            CrosstermEvent::Key(crossterm::event::KeyEvent::new(code, mods))
        };

        // Open the prompt (A-/) and type "beta" — the cursor should
        // track the first match while typing.
        view.handle_event(&key(KeyCode::Char('/'), KeyModifiers::ALT));
        assert!(matches!(view.input_mode, PlanInputMode::Searching { .. }));
        for c in "beta".chars() {
            view.handle_event(&key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!((view.cursor.col, view.cursor.row), (1, 1), "live jump to Beta cleanup");

        // Esc cancels: cursor restored, no query committed.
        view.handle_event(&key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(view.input_mode, PlanInputMode::Normal));
        assert_eq!((view.cursor.col, view.cursor.row), (1, 0));
        assert!(view.last_search.is_none());

        // Same flow but Enter commits: cursor stays on the match and
        // the query + match list persist for n/N.
        view.handle_event(&key(KeyCode::Char('/'), KeyModifiers::ALT));
        for c in "beta".chars() {
            view.handle_event(&key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        view.handle_event(&key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(view.input_mode, PlanInputMode::Normal));
        assert_eq!((view.cursor.col, view.cursor.row), (1, 1));
        assert_eq!(view.last_search.as_deref(), Some("beta"));
        assert_eq!(view.search_matches, vec!["c"]);
        assert_eq!(view.search_status.as_deref(), Some("match 1/1: beta"));
    }

    #[test]
    fn refresh_search_matches_drops_stale_ids_after_reload() {
        let mut view = make_search_view();
        view.last_search = Some("alpha".to_string());
        view.refresh_search_matches();
        assert_eq!(view.search_matches, vec!["a", "b", "c"]);

        // Simulate a board reload that removed the beta task.
        view.project_data[0].tasks.retain(|t| t.id != "c");
        view.project_data[0].layout.columns[1].retain(|i| !matches!(i, GridItem::Task(s) if s == "beta"));
        view.refresh_search_matches();
        assert_eq!(view.search_matches, vec!["a", "b"], "stale ID must drop out");
    }
}

