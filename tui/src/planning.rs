use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use alacritty_terminal::event::Event as TermEvent;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use crate::api::Task;
use crate::session::Session;
use crate::terminal_widget::TerminalWidget;

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

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
struct GridCursor {
    col: usize,
    row: usize,
}

struct ProjectData {
    project: PlanProject,
    tasks: Vec<PlanTask>,
    layout: GridLayout,
}

#[derive(Clone, Copy, PartialEq)]
enum NewProjectField { Name, RepoUrl }

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
    WorkspacePicker { project_idx: usize, task_idx: usize, selected: usize },
    LaunchConfirm { project_idx: usize, task_idx: usize, branch_text: String },
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
}

/// Write a task to a temp file for editing, returns the temp path.
fn write_temp_task(task: &PlanTask) -> Option<PathBuf> {
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
        for t in user_tasks { layout.columns[0].push(GridItem::Task(t.slug.clone())); }
        for t in claude_tasks { layout.columns[0].push(GridItem::Task(t.slug.clone())); }
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
    grid_rows_visible: usize,
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
            grid_rows_visible: 20,
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
                (self.cursor.row + 1).min(self.project_data[pi].layout.columns[ci].len())
            } else {
                self.project_data[pi].layout.columns[ci].len()
            };
            self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Task(slug));
            save_layout(&self.project_data[pi].layout, &self.project_data[pi].project.path);
            self.rebuild_unified_cols();
            self.recompute_conflicts();

            // Move cursor to the newly created task and open editor.
            if let Some(uc) = self.unified_cols.iter().position(|(p, c)| *p == pi && *c == ci) {
                self.cursor.col = uc;
                self.cursor.row = insert_at;
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

    fn cursor_column(&self) -> Option<&Vec<GridItem>> {
        let (pi, ci) = *self.unified_cols.get(self.cursor.col)?;
        self.project_data.get(pi)?.layout.columns.get(ci)
    }

    fn selected_slug(&self) -> Option<&str> {
        let col = self.cursor_column()?;
        match col.get(self.cursor.row)? {
            GridItem::Task(slug) => Some(slug),
            GridItem::Separator | GridItem::Empty | GridItem::Header(_) => None,
        }
    }

    fn selected_header_text(&self) -> Option<String> {
        let col = self.cursor_column()?;
        match col.get(self.cursor.row)? {
            GridItem::Header(text) => Some(text.clone()),
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
        if let Some(col) = self.cursor_column() {
            if col.is_empty() {
                self.cursor.row = 0;
            } else if self.cursor.row >= col.len() {
                self.cursor.row = col.len() - 1;
            }
        }
    }

    /// Whether the cursor is allowed to land on an item.
    /// Skips Empty rows always; skips archived task rows when show_archived is false.
    fn is_item_selectable(&self, pi: usize, item: &GridItem) -> bool {
        match item {
            GridItem::Empty => false,
            GridItem::Task(slug) => {
                if self.show_archived {
                    return true;
                }
                let pd = match self.project_data.get(pi) {
                    Some(pd) => pd,
                    None => return true,
                };
                match pd.tasks.iter().find(|t| t.slug == *slug) {
                    Some(t) => t.status != PlanStatus::Archived,
                    None => true,
                }
            }
            GridItem::Separator | GridItem::Header(_) => true,
        }
    }

    fn snap_cursor_to_selectable(&mut self, direction: i32) {
        let pi = match self.cursor_project_idx() {
            Some(p) => p,
            None => return,
        };
        let ci = match self.unified_cols.get(self.cursor.col) {
            Some((_, ci)) => *ci,
            None => return,
        };
        let col_len = match self.project_data.get(pi).and_then(|pd| pd.layout.columns.get(ci)) {
            Some(c) if !c.is_empty() => c.len(),
            _ => return,
        };
        let len = col_len as i32;
        let start = (self.cursor.row as i32).min(len - 1);
        let mut pos = start;
        for _ in 0..col_len {
            let selectable = self.project_data[pi].layout.columns[ci]
                .get(pos as usize)
                .cloned()
                .map(|item| self.is_item_selectable(pi, &item))
                .unwrap_or(false);
            if selectable {
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

    // ── Navigation ──────────────────────────────────────────

    fn navigate_vertical(&mut self, direction: i32) {
        if self.unified_cols.is_empty() { return; }
        let prev_slug = self.selected_slug().map(|s| s.to_string());
        let in_visual = self.visual_anchor.is_some();

        if self.linear_mode && !in_visual {
            let mut selectable_positions: Vec<(usize, usize)> = Vec::new();
            for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
                let col = &self.project_data[pi].layout.columns[ci];
                for (ri, item) in col.iter().enumerate() {
                    if self.is_item_selectable(pi, item) {
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
            let pi = match self.cursor_project_idx() {
                Some(p) => p,
                None => return,
            };
            let ci = match self.unified_cols.get(self.cursor.col) {
                Some((_, ci)) => *ci,
                None => return,
            };
            let col_len = match self.project_data.get(pi).and_then(|pd| pd.layout.columns.get(ci)) {
                Some(c) if !c.is_empty() => c.len(),
                _ => return,
            };
            let len = col_len as i32;
            if in_visual {
                let next = self.cursor.row as i32 + direction;
                if next < 0 || next >= len { return; }
                self.cursor.row = next as usize;
            } else {
                let mut next = self.cursor.row as i32;
                for _ in 0..col_len {
                    next = (next + direction).rem_euclid(len);
                    let item = self.project_data[pi].layout.columns[ci][next as usize].clone();
                    if self.is_item_selectable(pi, &item) {
                        break;
                    }
                }
                self.cursor.row = next as usize;
            }
        }
        self.ensure_cursor_visible();
        if self.selected_slug().map(|s| s.to_string()) != prev_slug {
            self.detail_scroll = 0;
        }
    }

    fn navigate_horizontal(&mut self, direction: i32) {
        if self.linear_mode || self.unified_cols.is_empty() { return; }
        self.cancel_visual();
        let len = self.unified_cols.len() as i32;
        let next = (self.cursor.col as i32 + direction).rem_euclid(len) as usize;
        self.cursor.col = next;
        if let Some(col) = self.cursor_column() {
            if col.is_empty() { self.cursor.row = 0; }
            else if self.cursor.row >= col.len() { self.cursor.row = col.len() - 1; }
        }
        self.snap_cursor_to_selectable(direction);
        self.ensure_cursor_visible();
    }

    fn ensure_cursor_visible(&mut self) {
        let h = self.grid_rows_visible;
        if h == 0 { return; }
        if self.linear_mode {
            let flat = self.cursor_flat_index_linear();
            if flat < self.linear_scroll {
                self.linear_scroll = flat;
            } else if flat >= self.linear_scroll + h {
                self.linear_scroll = flat.saturating_sub(h - 1);
            }
        } else {
            if self.cursor.col >= self.grid_col_scroll.len() { return; }
            let off = self.grid_col_scroll[self.cursor.col];
            if self.cursor.row < off {
                self.grid_col_scroll[self.cursor.col] = self.cursor.row;
            } else if self.cursor.row >= off + h {
                self.grid_col_scroll[self.cursor.col] = self.cursor.row.saturating_sub(h - 1);
            }
        }
    }

    /// Flat-list index of the cursor in linear mode, matching the order the
    /// linear renderer walks: project header + separator rows are counted.
    fn cursor_flat_index_linear(&self) -> usize {
        let mut flat = 0usize;
        for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
            let pd = match self.project_data.get(pi) { Some(p) => p, None => continue };
            let column = match pd.layout.columns.get(ci) { Some(c) => c, None => continue };
            if gi > 0 && self.is_first_col_of_project(gi) && !column.is_empty() {
                flat += 1;
            }
            if self.is_first_col_of_project(gi) && self.project_filter.is_none() {
                flat += 1;
            }
            if gi == self.cursor.col {
                return flat + self.cursor.row;
            }
            flat += column.len();
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
                        if let Some((pi, ci)) = self.unified_cols.get(self.cursor.col).copied() {
                            if matches!(
                                self.project_data[pi].layout.columns[ci].get(self.cursor.row),
                                Some(GridItem::Separator | GridItem::Empty | GridItem::Header(_))
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
                    KeyCode::Char('p') => {
                        self.cancel_visual();
                        let current = self.project_filter.map(|i| i + 1).unwrap_or(0);
                        self.input_mode = PlanInputMode::ProjectPicker { selected: current };
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('/') => {
                        self.cancel_visual();
                        self.input_mode = PlanInputMode::Searching { query: String::new() };
                        return PlanAction::Consumed;
                    }
                    _ => {}
                }
            }

            match key.code {
                KeyCode::PageDown => {
                    self.detail_scroll = self.detail_scroll.saturating_add(
                        (self.grid_rows_visible as u16 / 3).max(1)
                    );
                    return PlanAction::Consumed;
                }
                KeyCode::PageUp => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(
                        (self.grid_rows_visible as u16 / 3).max(1)
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
                    if let Some(col) = self.cursor_column() {
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
                KeyCode::Esc => self.input_mode = PlanInputMode::Normal,
                KeyCode::Enter => { self.input_mode = PlanInputMode::Normal; self.apply_search(&query); }
                KeyCode::Backspace => { query.pop(); self.input_mode = PlanInputMode::Searching { query }; }
                KeyCode::Char(c) => { query.push(c); self.input_mode = PlanInputMode::Searching { query }; }
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
                }
                _ => {}
            }
        }
        PlanAction::Consumed
    }

    fn handle_workspace_picker_event(&mut self, event: &CrosstermEvent) -> PlanAction {
        let (project_idx, task_idx, mut selected) = match self.input_mode {
            PlanInputMode::WorkspacePicker {
                project_idx,
                task_idx,
                selected,
            } => (project_idx, task_idx, selected),
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
                    };
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    selected = (selected + 1) % total;
                    self.input_mode = PlanInputMode::WorkspacePicker {
                        project_idx,
                        task_idx,
                        selected,
                    };
                }
                KeyCode::Enter => {
                    if selected == 0 {
                        // Fall through to branch-input dialog (current flow).
                        let branch_text = self.project_data[project_idx].tasks[task_idx]
                            .branch
                            .clone()
                            .unwrap_or_default();
                        self.input_mode = PlanInputMode::LaunchConfirm {
                            project_idx,
                            task_idx,
                            branch_text,
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
            let (project_idx, task_idx, mut branch_text) = match &self.input_mode {
                PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text } => {
                    (*project_idx, *task_idx, branch_text.clone())
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
                            let branch = if branch_text.trim().is_empty() {
                                None
                            } else {
                                Some(branch_text.trim().to_string())
                            };
                            let task_id = task.id.clone();
                            task.status = PlanStatus::InProgress;
                            return PlanAction::LaunchTask { project, slug, prompt, branch, autostart: false, task_id };
                        }
                    }
                }
                KeyCode::Backspace => {
                    branch_text.pop();
                    self.input_mode = PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text };
                }
                KeyCode::Char(c) => {
                    branch_text.push(c);
                    self.input_mode = PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text };
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
                return PlanAction::UpdateTask { id, fields };
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
                return PlanAction::UpdateTask { id, fields };
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
                return PlanAction::UpdateTask { id, fields };
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
            let ri = self.cursor.row;
            let target = ri as i32 + direction;
            if target < 0 { return; }
            let target = target as usize;
            while target >= self.project_data[pi].layout.columns[ci].len() {
                self.project_data[pi].layout.columns[ci].push(GridItem::Empty);
            }
            self.project_data[pi].layout.columns[ci].swap(ri, target);
            self.cursor.row = target;
        }
        self.save_project_layout(pi);
        self.recompute_conflicts();
        self.ensure_cursor_visible();
    }

    fn move_visual_block(&mut self, pi: usize, ci: usize, start: usize, end: usize, direction: i32) {
        let col = &mut self.project_data[pi].layout.columns[ci];

        if direction > 0 {
            let below = end + 1;
            while below >= col.len() {
                col.push(GridItem::Empty);
            }
            let item = col.remove(below);
            col.insert(start, item);
            self.cursor.row += 1;
            if let Some(ref mut anchor) = self.visual_anchor {
                *anchor += 1;
            }
        } else {
            if start == 0 { return; }
            let above = start - 1;
            let item = col.remove(above);
            col.insert(end, item);
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
            let src_len = self.project_data[src_pi].layout.columns[src_ci].len();
            if range_end >= src_len { return; }
            let items: Vec<GridItem> = self.project_data[src_pi].layout.columns[src_ci]
                .drain(range_start..=range_end)
                .collect();
            let dst_len = self.project_data[dst_pi].layout.columns[dst_ci].len();
            let insert_at = range_start.min(dst_len);
            for (offset, item) in items.into_iter().enumerate() {
                self.project_data[dst_pi].layout.columns[dst_ci].insert(insert_at + offset, item);
            }
            let cursor_offset = self.cursor.row.saturating_sub(range_start);
            self.cursor.col = target_gcol;
            self.cursor.row = insert_at + cursor_offset;
            if let Some(ref mut anchor) = self.visual_anchor {
                let anchor_offset = anchor.saturating_sub(range_start);
                *anchor = insert_at + anchor_offset;
            }
        } else {
            match self.project_data[src_pi].layout.columns[src_ci].get(self.cursor.row) {
                Some(GridItem::Task(_)) | Some(GridItem::Header(_)) => {}
                _ => return,
            }
            let item = self.project_data[src_pi].layout.columns[src_ci].remove(self.cursor.row);
            let insert_at = self.cursor.row.min(self.project_data[dst_pi].layout.columns[dst_ci].len());
            self.project_data[dst_pi].layout.columns[dst_ci].insert(insert_at, item);
            self.cursor.col = target_gcol;
            self.cursor.row = insert_at;
        }

        self.save_project_layout(src_pi);
        self.recompute_conflicts();
        self.clamp_cursor();
    }

    fn insert_separator(&mut self) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let insert_at = (self.cursor.row + 1).min(self.project_data[pi].layout.columns[ci].len());
        self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Separator);
        self.save_project_layout(pi);
    }

    fn insert_empty(&mut self) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let insert_at = (self.cursor.row + 1).min(self.project_data[pi].layout.columns[ci].len());
        self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Empty);
        self.save_project_layout(pi);
    }

    fn insert_header(&mut self, text: String) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        let insert_at = (self.cursor.row + 1).min(self.project_data[pi].layout.columns[ci].len());
        self.project_data[pi].layout.columns[ci].insert(insert_at, GridItem::Header(text));
        self.save_project_layout(pi);
    }

    fn update_header_at_cursor(&mut self, text: String) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        if let Some(item) = self.project_data[pi].layout.columns[ci].get_mut(self.cursor.row) {
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
        if matches!(self.project_data[pi].layout.columns[ci].get(self.cursor.row), Some(GridItem::Separator | GridItem::Empty | GridItem::Header(_))) {
            self.project_data[pi].layout.columns[ci].remove(self.cursor.row);
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

        // Write task to temp file for editing.
        let temp_path = match write_temp_task(task) {
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
                    task.description = parsed.description.clone();
                    task.prompt = parsed.prompt.clone();

                    let fields = build_patch_fields(&parsed);

                    action = PlanAction::UpdateTask {
                        id: task.id.clone(),
                        fields,
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
            };
        }
        PlanAction::Consumed
    }

    /// Sync the list of open workspaces for the picker. Called by App each
    /// event cycle before the picker is drawn or handled.
    pub fn set_workspace_candidates(&mut self, candidates: Vec<WorkspaceCandidate>) {
        self.workspace_candidates = candidates;
    }

    fn apply_search(&mut self, query: &str) {
        if query.is_empty() { return; }
        let q = query.to_lowercase();
        for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
            let col = &self.project_data[pi].layout.columns[ci];
            for (ri, item) in col.iter().enumerate() {
                if let GridItem::Task(slug) = item {
                    if let Some(task) = self.project_data[pi].tasks.iter().find(|t| t.slug == *slug) {
                        if task.title.to_lowercase().contains(&q) || task.description.to_lowercase().contains(&q) {
                            self.cursor.col = gi;
                            self.cursor.row = ri;
                            self.ensure_cursor_visible();
                            return;
                        }
                    }
                }
            }
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
        let help_h: u16 = if self.linear_mode { 7 } else { 3 };
        let inner_h = area_height.saturating_sub(2);
        self.grid_rows_visible = inner_h.saturating_sub(help_h + 1) as usize;

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
            PlanInputMode::WorkspacePicker { project_idx, task_idx, selected } => self.draw_workspace_picker(frame, area, *project_idx, *task_idx, *selected),
            PlanInputMode::LaunchConfirm { project_idx, task_idx, branch_text } => self.draw_launch_confirm(frame, area, *project_idx, *task_idx, branch_text),
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
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(filter_label, Style::default().fg(Color::White)));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 4 || inner.width < 8 { return; }

        let help_h = 3u16;
        let grid_height = inner.height.saturating_sub(help_h) as usize;
        let num_cols = self.unified_cols.len().max(1);
        let col_width = inner.width / num_cols as u16;
        let dim = Style::default().fg(Color::DarkGray);

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
                    Paragraph::new(Span::styled(name_display, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))),
                    Rect::new(x, inner.y, w, 1),
                );
            }

            let header_h: u16 = if show_headers { 1 } else { 0 };
            let col_area = Rect::new(x, inner.y + header_h, w, (grid_height as u16).saturating_sub(header_h));
            let items = self.build_column_items(gi, &pd.project.name, column, w as usize, col_area.height as usize);
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
                let color = if is_project_boundary { Color::Cyan } else { Color::DarkGray };
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
        let sep = Line::from(Span::styled("\u{2500}".repeat(inner.width as usize), dim));
        frame.render_widget(Paragraph::new(vec![
            sep,
            Line::from(Span::styled(
                " A-j/k nav \u{00b7} A-h/l cols \u{00b7} A-J/K reorder \u{00b7} A-H/L move \u{00b7} A-v visual \u{00b7} A-g linear",
                dim,
            )),
            Line::from(Span::styled(
                " A-e edit \u{00b7} A-n new \u{00b7} A-i header \u{00b7} A-Ent sep \u{00b7} A-Spc empty \u{00b7} A-s status \u{00b7} A-d done \u{00b7} A-a accept \u{00b7} A-A archive done \u{00b7} A-V show arch \u{00b7} A-x del \u{00b7} A-f launch \u{00b7} A-U unlaunch \u{00b7} A-c col \u{00b7} A-r refresh \u{00b7} A-q quit",
                dim,
            )),
        ]), help_area);
    }

    fn build_column_items<'a>(
        &'a self, col_idx: usize, project_name: &str, column: &[GridItem], width: usize, max_rows: usize,
    ) -> Vec<ListItem<'a>> {
        let mut items = Vec::new();
        let start = self.grid_col_scroll.get(col_idx).copied().unwrap_or(0).min(column.len());

        // Iterate to the end of the column, but stop once we've collected
        // max_rows real ListItems. Bounding by `start + max_rows` would
        // count `continue`-skipped (archived) items against the visible
        // budget, making columns with archived rows in their window render
        // shorter than their data — items at the bottom stay invisible
        // until the user scrolls.
        for ri in start..column.len() {
            if items.len() >= max_rows { break; }
            let is_selected = self.cursor.col == col_idx && self.cursor.row == ri;
            let in_visual = self.is_in_visual_range(col_idx, ri);
            // Skip archived task rows when show_archived is off.
            if !self.show_archived {
                if let GridItem::Task(slug) = &column[ri] {
                    let archived = self.project_data.iter().find_map(|pd| {
                        if pd.project.name == project_name { pd.tasks.iter().find(|t| t.slug == *slug) } else { None }
                    }).map(|t| t.status == PlanStatus::Archived).unwrap_or(false);
                    if archived { continue; }
                }
            }
            match &column[ri] {
                GridItem::Task(slug) => {
                    let task = self.project_data.iter().find_map(|pd| {
                        if pd.project.name == project_name { pd.tasks.iter().find(|t| t.slug == *slug) } else { None }
                    });
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
                        Style::default().fg(Color::Magenta)
                    } else {
                        match status {
                            Some(PlanStatus::Done) => Style::default().fg(Color::Green),
                            Some(PlanStatus::InProgress) => Style::default().fg(Color::Yellow),
                            Some(PlanStatus::Backlog) => Style::default(),
                            Some(PlanStatus::Draft) => Style::default().fg(Color::DarkGray),
                            Some(PlanStatus::Archived) => Style::default().fg(Color::DarkGray),
                            None => Style::default(),
                        }
                    };
                    let claude_prefix = if is_claude { "[C] " } else { "" };
                    let max_title = width.saturating_sub(4 + claude_prefix.len());
                    let title_display = if title_str.len() > max_title {
                        format!("{}...", &title_str[..max_title.saturating_sub(3)])
                    } else { title_str.to_string() };

                    let mut spans = vec![
                        Span::styled(format!("{} ", indicator), indicator_style),
                    ];
                    if is_claude {
                        spans.push(Span::styled(claude_prefix, Style::default().fg(Color::Magenta)));
                    }
                    spans.push(Span::raw(title_display));
                    let line = Line::from(spans);
                    let conflict = self.is_conflict(project_name, slug);
                    let base_fg = if is_claude { Color::Magenta } else { Color::Gray };
                    let style = if is_selected && in_visual {
                        Style::default().fg(Color::White).bg(Color::Rgb(50, 50, 80)).add_modifier(Modifier::BOLD)
                    } else if is_selected {
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                    } else if in_visual {
                        Style::default().fg(Color::White).bg(Color::Rgb(50, 50, 80))
                    } else {
                        Style::default().fg(base_fg)
                    };
                    let style = if conflict && is_selected {
                        style.bg(Color::Red).fg(Color::White)
                    } else if conflict {
                        style.bg(Color::Rgb(80, 0, 0))
                    } else { style };
                    items.push(ListItem::new(line).style(style));
                }
                GridItem::Separator => {
                    let ch = if is_selected { "\u{2501}" } else { "\u{2500}" };
                    let st = if is_selected { Style::default().fg(Color::White) } else { Style::default().fg(Color::DarkGray) };
                    items.push(ListItem::new(Line::from(Span::styled(ch.repeat(width.saturating_sub(1)), st))));
                }
                GridItem::Empty => {
                    items.push(ListItem::new(Line::from("")));
                }
                GridItem::Header(text) => {
                    let base_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
                    let style = if is_selected && in_visual {
                        base_style.bg(Color::Rgb(50, 50, 80))
                    } else if is_selected {
                        base_style.bg(Color::Rgb(40, 40, 50))
                    } else if in_visual {
                        base_style.bg(Color::Rgb(50, 50, 80))
                    } else {
                        base_style
                    };
                    let max_text = width.saturating_sub(1);
                    let display = if text.len() > max_text {
                        format!("{}...", &text[..max_text.saturating_sub(3)])
                    } else {
                        text.clone()
                    };
                    items.push(ListItem::new(Line::from(Span::styled(display, style))));
                }
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
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(title, Style::default().fg(Color::White)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 4 || inner.width < 4 { return; }

        let help_entries: Vec<(&str, &str)> = vec![
            ("A-j/k  nav", "A-d    done"),
            ("A-J/K  reorder", "A-f    launch"),
            ("A-e    edit", "A-a    accept"),
            ("A-n    new", "A-x    delete"),
            ("A-N    subtask", ""),
            ("A-u    unbind", "A-U    unlaunch"),
            ("A-Ent  sep", "A-Spc  empty"),
            ("A-i    header", "A-A    archive done"),
            ("A-s/S  status", "A-V    show arch"),
            ("A-g    grid", "A-t    sessions"),
            ("A-p    filter", ""),
        ];
        let help_rows = help_entries.len() as u16;
        let list_height = inner.height.saturating_sub(help_rows + 2) as usize;
        let dim = Style::default().fg(Color::DarkGray);

        let mut items: Vec<ListItem> = Vec::new();
        let mut flat_idx = 0usize;

        for (gi, &(pi, ci)) in self.unified_cols.iter().enumerate() {
            let pd = &self.project_data[pi];
            let column = &pd.layout.columns[ci];

            if gi > 0 && self.is_first_col_of_project(gi) && !column.is_empty() {
                if flat_idx >= self.linear_scroll && items.len() < list_height {
                    let sep = "\u{2550}".repeat(inner.width.saturating_sub(2) as usize);
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled(" ", dim),
                        Span::styled(sep, Style::default().fg(Color::Cyan)),
                    ])));
                }
                flat_idx += 1;
            }

            if self.is_first_col_of_project(gi) && self.project_filter.is_none() {
                if flat_idx >= self.linear_scroll && items.len() < list_height {
                    items.push(ListItem::new(Line::from(Span::styled(
                        format!(" {}", pd.project.name),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
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
                            Style::default().fg(Color::Magenta)
                        } else {
                            match status {
                                Some(PlanStatus::Done) => Style::default().fg(Color::Green),
                                Some(PlanStatus::InProgress) => Style::default().fg(Color::Yellow),
                                _ => Style::default().fg(Color::DarkGray),
                            }
                        };
                        let claude_prefix = if is_claude { "[C] " } else { "" };
                        let max_title = (inner.width as usize).saturating_sub(5 + claude_prefix.len());
                        let title_display = if title_str.len() > max_title {
                            format!("{}...", &title_str[..max_title.saturating_sub(3)])
                        } else { title_str.to_string() };

                        let mut spans = vec![
                            Span::styled(format!(" {} ", indicator), indicator_style),
                        ];
                        if is_claude {
                            spans.push(Span::styled(claude_prefix, Style::default().fg(Color::Magenta)));
                        }
                        spans.push(Span::raw(title_display));
                        let line = Line::from(spans);
                        let conflict = self.is_conflict(&pd.project.name, slug);
                        let base_fg = if is_claude { Color::Magenta } else { Color::Gray };
                        let style = if is_selected {
                            Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(base_fg)
                        };
                        let style = if conflict && is_selected {
                            style.bg(Color::Red).fg(Color::White)
                        } else if conflict {
                            style.bg(Color::Rgb(80, 0, 0))
                        } else { style };
                        items.push(ListItem::new(line).style(style));
                    }
                    GridItem::Separator => {
                        let ch = if is_selected { "\u{2501}" } else { "\u{2500}" };
                        let st = if is_selected { Style::default().fg(Color::White) } else { dim };
                        items.push(ListItem::new(Line::from(Span::styled(
                            format!(" {}", ch.repeat((inner.width as usize).saturating_sub(2))), st,
                        ))));
                    }
                    GridItem::Empty => {
                        items.push(ListItem::new(Line::from("")));
                    }
                    GridItem::Header(text) => {
                        let base_style = Style::default().fg(Color::White).add_modifier(Modifier::BOLD);
                        let style = if is_selected {
                            base_style.bg(Color::Rgb(40, 40, 50))
                        } else {
                            base_style
                        };
                        let max_text = (inner.width as usize).saturating_sub(2);
                        let display = if text.len() > max_text {
                            format!("{}...", &text[..max_text.saturating_sub(3)])
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
        let sep = Line::from(Span::styled("\u{2500}".repeat(inner.width as usize), dim));
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
        let title_style = if selected.is_some() { Style::default().fg(Color::White) } else { Style::default().fg(Color::DarkGray) };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(title, title_style));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some((task, project_name)) = selected {
            let mut lines: Vec<Line> = vec![];
            lines.push(Line::from(vec![
                Span::styled("  Slug: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}/{}", project_name, task.slug),
                    Style::default().fg(Color::White),
                ),
            ]));

            let status_color = match task.status {
                PlanStatus::Done => Color::Green, PlanStatus::InProgress => Color::Yellow,
                PlanStatus::Backlog => Color::White, PlanStatus::Draft => Color::DarkGray,
                PlanStatus::Archived => Color::DarkGray,
            };
            let mut meta = vec![
                Span::styled("  Status: ", Style::default().fg(Color::DarkGray)),
                Span::styled(task.status.label(), Style::default().fg(status_color)),
            ];
            if let Some(d) = task.difficulty {
                meta.push(Span::styled("    Difficulty: ", Style::default().fg(Color::DarkGray)));
                meta.push(Span::styled(d.to_string(), Style::default().fg(Color::White)));
            }
            lines.push(Line::from(meta));

            if !task.depends.is_empty() {
                let dep_color = if self.is_conflict(project_name, &task.slug) { Color::Red } else { Color::White };
                lines.push(Line::from(vec![
                    Span::styled("  Depends: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(task.depends.join(", "), Style::default().fg(dep_color)),
                ]));
            }
            if let Some(ref created) = task.created {
                lines.push(Line::from(vec![
                    Span::styled("  Created: ", Style::default().fg(Color::DarkGray)),
                    Span::styled(created.as_str(), Style::default().fg(Color::White)),
                ]));
            }
            if task.source == "claude" {
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(" PROPOSED ", Style::default().fg(Color::White).bg(Color::Magenta).add_modifier(Modifier::BOLD)),
                    Span::styled("  Alt+a to accept, Alt+d to reject", Style::default().fg(Color::DarkGray)),
                ]));
            }
            lines.push(Line::from(""));
            let sep_w = inner.width.saturating_sub(4) as usize;
            lines.push(Line::from(Span::styled(format!("  {}", "\u{2500}".repeat(sep_w)), Style::default().fg(Color::DarkGray))));
            lines.push(Line::from(""));

            let body = if !task.description.is_empty() {
                &task.description
            } else if !task.prompt.is_empty() {
                &task.prompt
            } else {
                ""
            };

            if body.is_empty() {
                lines.push(Line::from(Span::styled("  No description. Press Alt+e to edit.", Style::default().fg(Color::DarkGray))));
            } else {
                for line in body.lines() {
                    let style = if line.starts_with("## ") {
                        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                    } else { Style::default().fg(Color::Gray) };
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
                Line::from(Span::styled("  No tasks yet. Press Alt+Shift+p to create a project.", Style::default().fg(Color::DarkGray))),
                Line::from(Span::styled("  Or press Alt+r to refresh from API.", Style::default().fg(Color::DarkGray))),
            ]), inner);
        } else if self.project_data.iter().all(|pd| pd.tasks.is_empty()) {
            frame.render_widget(Paragraph::new(Span::styled(
                "  No tasks. Press Alt+n to create one.", Style::default().fg(Color::DarkGray),
            )), inner);
        }
    }

    fn draw_editor(&self, frame: &mut Frame, area: Rect) {
        let title = self.editing_slug.as_ref()
            .map(|s| format!(" Editing: {} ", s))
            .unwrap_or_else(|| " Editor ".to_string());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(title, Style::default().fg(Color::White)));
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
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::White))
            .title(Span::styled(" Search ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  > ", Style::default().fg(Color::DarkGray)),
                Span::styled(query, Style::default().fg(Color::White)),
                Span::styled("\u{2588}", Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Enter search \u{00b7} Esc cancel", Style::default().fg(Color::DarkGray))),
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
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                title_label,
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
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
                Span::styled("  Parent: ", Style::default().fg(Color::DarkGray)),
                Span::styled(shown, Style::default().fg(Color::Cyan)),
            ]));
            lines.push(Line::from(vec![
                Span::styled("  Mode:   ", Style::default().fg(Color::DarkGray)),
                Span::styled("inherit", Style::default().fg(Color::Gray)),
                Span::styled(
                    "  (subtask shares parent's worktree on launch)",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("  Title:  ", Style::default().fg(Color::DarkGray)),
            Span::styled(title, Style::default().fg(Color::White)),
            Span::styled("\u{2588}", Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Enter create \u{00b7} Esc cancel",
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_new_header_overlay(&self, frame: &mut Frame, area: Rect, text: &str, editing: bool) {
        let title = if editing { " Edit Header " } else { " New Header " };
        let (w, h) = (60u16.min(area.width.saturating_sub(4)), 5u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::White))
            .title(Span::styled(title, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Text: ", Style::default().fg(Color::DarkGray)),
                Span::styled(text, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::styled("\u{2588}", Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Enter save \u{00b7} Esc cancel", Style::default().fg(Color::DarkGray))),
        ]), inner);
    }

    fn draw_new_project_overlay(&self, frame: &mut Frame, area: Rect, name: &str, repo_url: &str, field: NewProjectField) {
        let (w, h) = (70u16.min(area.width.saturating_sub(4)), 7u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::White))
            .title(Span::styled(" New Project ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let cursor = "\u{2588}";
        let name_cursor = if field == NewProjectField::Name { cursor } else { "" };
        let url_cursor = if field == NewProjectField::RepoUrl { cursor } else { "" };

        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("     Name: ", Style::default().fg(Color::DarkGray)),
                Span::styled(name, Style::default().fg(Color::White)),
                Span::styled(name_cursor, Style::default().fg(Color::White)),
            ]),
            Line::from(vec![
                Span::styled("  Repo URL: ", Style::default().fg(Color::DarkGray)),
                Span::styled(repo_url, Style::default().fg(Color::White)),
                Span::styled(url_cursor, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Tab switch \u{00b7} Enter create \u{00b7} Esc cancel", Style::default().fg(Color::DarkGray))),
        ]), inner);
    }

    fn draw_project_picker(&self, frame: &mut Frame, area: Rect, selected: usize) {
        let w = 40u16.min(area.width.saturating_sub(4));
        let h = (self.projects.len() as u16 + 5).min(area.height.saturating_sub(4));
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::White))
            .title(Span::styled(" Filter Projects ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let mut lines: Vec<Line> = vec![];
        let all_style = if selected == 0 { Style::default().fg(Color::White).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) };
        let all_ind = if selected == 0 { ">" } else { " " };
        lines.push(Line::from(Span::styled(format!("  {} All projects", all_ind), all_style)));

        for (i, project) in self.projects.iter().enumerate() {
            let idx = i + 1;
            let ind = if selected == idx { ">" } else { " " };
            let st = if selected == idx { Style::default().fg(Color::White).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Gray) };
            lines.push(Line::from(Span::styled(format!("  {} {}", ind, project.name), st)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("j/k navigate \u{00b7} Enter select \u{00b7} Esc cancel", Style::default().fg(Color::DarkGray))));
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_workspace_picker(
        &self,
        frame: &mut Frame,
        area: Rect,
        project_idx: usize,
        task_idx: usize,
        selected: usize,
    ) {
        let task_name = self
            .project_data
            .get(project_idx)
            .and_then(|pd| pd.tasks.get(task_idx))
            .map(|t| t.title.as_str())
            .unwrap_or("?");
        let candidates = self.candidates_for(project_idx, task_idx);
        let rows = 5 + candidates.len() as u16 + 2;
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
            .border_style(Style::default().fg(Color::White))
            .title(Span::styled(
                " Launch Into ",
                Style::default()
                    .fg(Color::White)
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
            Span::styled("  Task: ", Style::default().fg(Color::DarkGray)),
            Span::styled(display_name, Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(""));
        let row = |label: &str, idx: usize| -> Line<'static> {
            let is_sel = idx == selected;
            let ind = if is_sel { ">" } else { " " };
            let st = if is_sel {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::from(Span::styled(format!("  {} {}", ind, label), st))
        };
        lines.push(row("+ New workspace (create worktree)", 0));
        for (i, c) in candidates.iter().enumerate() {
            let label = if c.name.len() > (w as usize).saturating_sub(8) {
                format!("{}...", &c.name[..(w as usize).saturating_sub(11)])
            } else {
                c.name.clone()
            };
            lines.push(row(&label, i + 1));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  j/k navigate \u{00b7} Enter select \u{00b7} Esc cancel",
            Style::default().fg(Color::DarkGray),
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
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::White))
            .title(Span::styled(" Archive Done Tasks ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(vec![
            Line::from(""),
            Line::from(vec![
                Span::styled(format!("  Archive {} done task{} in ", count, if count == 1 { "" } else { "s" }), Style::default().fg(Color::White)),
                Span::styled(project_name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled("?", Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(Span::styled("  Enter confirm \u{00b7} Esc cancel", Style::default().fg(Color::DarkGray))),
        ]), inner);
    }

    fn draw_launch_confirm(&self, frame: &mut Frame, area: Rect, project_idx: usize, task_idx: usize, branch_text: &str) {
        let task_name = self.project_data.get(project_idx)
            .and_then(|pd| pd.tasks.get(task_idx))
            .map(|t| t.title.as_str())
            .unwrap_or("?");
        let (w, h) = (60u16.min(area.width.saturating_sub(4)), 9u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::White))
            .title(Span::styled(" Launch Task ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);

        let display_name: String = task_name.chars().take((w as usize).saturating_sub(10)).collect();
        let branch_hint = if branch_text.is_empty() { "main" } else { "" };
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("    Task: ", Style::default().fg(Color::DarkGray)),
                Span::styled(display_name, Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::styled("  Branch: ", Style::default().fg(Color::DarkGray)),
                Span::styled(branch_text, Style::default().fg(Color::White)),
                Span::styled("\u{2588}", Style::default().fg(Color::White)),
                Span::styled(branch_hint, Style::default().fg(Color::DarkGray)),
            ]),
            Line::from(""),
            Line::from(Span::styled("  Enter launch \u{00b7} Esc cancel", Style::default().fg(Color::DarkGray))),
        ]), inner);
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
            parent_task_id: None,
            worktree_mode: "inherit".to_string(),
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
        });
        view.project_data.push(pd);
        view.projects = view.project_data.iter().map(|pd| pd.project.clone()).collect();

        let (project, repo_url, parent) =
            extract_create(view.create_task("Sub task", Some("parent-id-123".to_string())));

        assert_eq!(project, "forked-repo");
        assert_eq!(repo_url, stored);
        assert_eq!(parent, Some("parent-id-123".to_string()));
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
        }
    }

    fn round_trip(task: &PlanTask) -> TempTaskParsed {
        let path = write_temp_task(task).expect("write_temp_task");
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
}
