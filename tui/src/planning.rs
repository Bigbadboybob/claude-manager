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

#[derive(Clone, Debug, PartialEq)]
pub enum PlanStatus {
    Done,
    InProgress,
    Backlog,
    Draft,
}

impl PlanStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "done" => Self::Done,
            "in_progress" | "running" => Self::InProgress,
            "backlog" | "blocked" => Self::Backlog,
            _ => Self::Draft,
        }
    }
    fn as_str(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::InProgress => "in_progress",
            Self::Backlog => "backlog",
            Self::Draft => "draft",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::InProgress => "in progress",
            Self::Backlog => "backlog",
            Self::Draft => "draft",
        }
    }
    fn next(&self) -> Self {
        match self {
            Self::Draft => Self::Backlog,
            Self::Backlog => Self::InProgress,
            Self::InProgress => Self::Done,
            Self::Done => Self::Done,
        }
    }
    fn prev(&self) -> Self {
        match self {
            Self::Done => Self::InProgress,
            Self::InProgress => Self::Backlog,
            Self::Backlog => Self::Draft,
            Self::Draft => Self::Draft,
        }
    }
}

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
}

#[derive(Clone, Debug)]
enum GridItem {
    Task(String),
    Separator,
    Empty,
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
    NewTask { title: String },
    NewProject { name: String, repo_url: String, field: NewProjectField },
    ProjectPicker { selected: usize },
    LaunchConfirm { project_idx: usize, task_idx: usize, branch_text: String },
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
    },
    SwitchToSessions,
    Quit,
    CreateTask {
        project: String,
        repo_url: String,
        name: String,
        description: String,
        status: String,
    },
    UpdateTask {
        id: String,
        fields: HashMap<String, serde_json::Value>,
    },
    DeleteTask {
        id: String,
    },
    RefreshTasks,
}

// ── Temp File Editing ──────────────────────────────────────

/// Write a task to a temp file for editing, returns the temp path.
fn write_temp_task(task: &PlanTask) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("cm-planning");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{}.md", task.slug));

    let mut yaml_parts = vec![
        format!("title: {}", task.title),
        format!("status: {}", task.status.as_str()),
    ];
    if let Some(d) = task.difficulty {
        yaml_parts.push(format!("difficulty: {}", d));
    }
    if !task.depends.is_empty() {
        yaml_parts.push(format!("depends: [{}]", task.depends.join(", ")));
    }
    if let Some(ref branch) = task.branch {
        yaml_parts.push(format!("branch: {}", branch));
    }

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

    let content = format!("---\n{}\n---\n\n{}", yaml_parts.join("\n"), body);
    std::fs::write(&path, content).ok()?;
    Some(path)
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

    #[derive(serde::Deserialize)]
    struct Frontmatter {
        title: String,
        #[serde(default)]
        status: Option<String>,
        #[serde(default)]
        difficulty: Option<u8>,
        #[serde(default)]
        depends: Option<Vec<String>>,
        #[serde(default)]
        branch: Option<String>,
    }

    let front: Frontmatter = serde_yaml::from_str(yaml_str).ok()?;

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
        difficulty: front.difficulty,
        depends: front.depends.unwrap_or_default(),
        branch: front.branch,
        description: desc_lines.join("\n").trim().to_string(),
        prompt: prompt_lines.join("\n").trim().to_string(),
    })
}

struct TempTaskParsed {
    title: String,
    status: String,
    difficulty: Option<u8>,
    depends: Vec<String>,
    branch: Option<String>,
    description: String,
    prompt: String,
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
                    col.into_iter().map(|s| match s.as_str() {
                        "---" => GridItem::Separator,
                        "___" => GridItem::Empty,
                        _ => GridItem::Task(s),
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
            GridItem::Separator | GridItem::Empty => true,
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
    scroll_offset: usize,
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
}

impl PlanningView {
    pub fn new() -> Self {
        PlanningView {
            projects: vec![],
            project_data: vec![],
            project_filter: None,
            unified_cols: vec![],
            cursor: GridCursor { col: 0, row: 0 },
            scroll_offset: 0,
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
        }
    }

    /// Update planning data from API tasks. Called when BackendEvent::PlanTasksUpdated arrives.
    pub fn update_from_api(&mut self, api_tasks: Vec<Task>) {
        // Group tasks by project.
        let mut by_project: HashMap<String, Vec<PlanTask>> = HashMap::new();
        for task in &api_tasks {
            if let Some(ref project) = task.project {
                by_project.entry(project.clone())
                    .or_default()
                    .push(PlanTask::from_api(task));
            }
        }

        // Discover projects from the API data.
        let mut project_names: Vec<String> = by_project.keys().cloned().collect();
        project_names.sort();

        self.projects = project_names.iter().map(|name| {
            let path = ensure_project_dir(name);
            PlanProject { name: name.clone(), path }
        }).collect();

        self.project_data.clear();
        for project in &self.projects {
            let tasks = by_project.remove(&project.name).unwrap_or_default();
            let mut layout = load_layout(&project.path);
            sync_layout_with_tasks(&mut layout, &tasks);
            save_layout(&layout, &project.path);
            self.project_data.push(ProjectData { project: project.clone(), tasks, layout });
        }

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
                    let project = PlanProject { name: project_name.clone(), path };
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
            self.needs_redraw = true;
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
    pub fn mark_task_done(&mut self, project_name: &str, slug: &str) {
        for pd in &mut self.project_data {
            if pd.project.name == project_name {
                if let Some(task) = pd.tasks.iter_mut().find(|t| t.slug == slug) {
                    task.status = PlanStatus::Done;
                }
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
            GridItem::Separator | GridItem::Empty => None,
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

    fn snap_cursor_to_selectable(&mut self, direction: i32) {
        if let Some(col) = self.cursor_column() {
            if col.is_empty() { return; }
            let len = col.len() as i32;
            let start = (self.cursor.row as i32).min(len - 1);
            let mut pos = start;
            for _ in 0..col.len() {
                if !matches!(col.get(pos as usize), Some(GridItem::Empty)) {
                    self.cursor.row = pos as usize;
                    return;
                }
                pos = (pos + direction).rem_euclid(len);
            }
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
        let is_selectable = |item: &GridItem| !matches!(item, GridItem::Empty);

        if self.linear_mode && !in_visual {
            let selectable_positions: Vec<(usize, usize)> = self.unified_cols.iter().enumerate()
                .flat_map(|(gi, (pi, ci))| {
                    let col = &self.project_data[*pi].layout.columns[*ci];
                    col.iter().enumerate()
                        .filter(|(_, item)| is_selectable(item))
                        .map(move |(ri, _)| (gi, ri))
                }).collect();
            if selectable_positions.is_empty() { return; }
            let cur = selectable_positions.iter()
                .position(|&(c, r)| c == self.cursor.col && r == self.cursor.row)
                .unwrap_or(0);
            let next = (cur as i32 + direction).rem_euclid(selectable_positions.len() as i32) as usize;
            self.cursor.col = selectable_positions[next].0;
            self.cursor.row = selectable_positions[next].1;
        } else {
            let col = match self.cursor_column() {
                Some(c) if !c.is_empty() => c,
                _ => return,
            };
            let len = col.len() as i32;
            if in_visual {
                let next = self.cursor.row as i32 + direction;
                if next < 0 || next >= len { return; }
                self.cursor.row = next as usize;
            } else {
                let mut next = self.cursor.row as i32;
                for _ in 0..col.len() {
                    next = (next + direction).rem_euclid(len);
                    if is_selectable(&col[next as usize]) {
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
        if self.cursor.row < self.scroll_offset {
            self.scroll_offset = self.cursor.row;
        } else if self.cursor.row >= self.scroll_offset + h {
            self.scroll_offset = self.cursor.row.saturating_sub(h - 1);
        }
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
            PlanInputMode::NewProject { .. } => self.handle_new_project_event(event),
            PlanInputMode::ProjectPicker { .. } => self.handle_project_picker_event(event),
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
                    KeyCode::Char('e') => { self.cancel_visual(); return self.start_editor(); }
                    KeyCode::Char('n') => {
                        self.cancel_visual();
                        if self.projects.is_empty() {
                            self.input_mode = PlanInputMode::NewProject { name: String::new(), repo_url: String::new(), field: NewProjectField::Name };
                        } else {
                            self.input_mode = PlanInputMode::NewTask { title: String::new() };
                        }
                        return PlanAction::Consumed;
                    }
                    KeyCode::Char('o') => { self.sort_column_by_status(); return PlanAction::Consumed; }
                    KeyCode::Char('s') => { return self.cycle_status(true); }
                    KeyCode::Char('a') => { return self.accept_proposal(); }
                    KeyCode::Char('d') => { self.cancel_visual(); return self.delete_task(); }
                    KeyCode::Char('x') => { self.cancel_visual(); return self.start_launch(); }
                    KeyCode::Char('l') if self.linear_mode => { self.cancel_visual(); return self.start_launch(); }
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
        if self.editor.as_ref().map_or(false, |e| e.exited) {
            return self.stop_editor();
        }
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
                    editor.write(&data);
                    return PlanAction::Consumed;
                }
                let term_mode = *editor.term.lock().mode();
                if let Some(bytes) = crate::input::event_to_bytes(event, &term_mode) {
                    editor.write(&bytes);
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
            let mut title = match &self.input_mode {
                PlanInputMode::NewTask { title } => title.clone(),
                _ => return PlanAction::Consumed,
            };
            match key.code {
                KeyCode::Esc => self.input_mode = PlanInputMode::Normal,
                KeyCode::Enter => {
                    if !title.trim().is_empty() {
                        self.input_mode = PlanInputMode::Normal;
                        return self.create_task(&title);
                    }
                }
                KeyCode::Backspace => { title.pop(); self.input_mode = PlanInputMode::NewTask { title }; }
                KeyCode::Char(c) => { title.push(c); self.input_mode = PlanInputMode::NewTask { title }; }
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
                            let prompt = if task.prompt.is_empty() {
                                task.title.clone()
                            } else {
                                task.prompt.clone()
                            };
                            let slug = task.slug.clone();
                            let branch = if branch_text.trim().is_empty() {
                                None
                            } else {
                                Some(branch_text.trim().to_string())
                            };
                            task.status = PlanStatus::InProgress;
                            return PlanAction::LaunchTask { project, slug, prompt, branch, autostart: false };
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
                        })
                        .unwrap_or(4);
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
        match self.project_data[src_pi].layout.columns[src_ci].get(self.cursor.row) {
            Some(GridItem::Task(_)) => {}
            _ => return,
        }
        let target_gcol = self.cursor.col as i32 + direction;
        if target_gcol < 0 || target_gcol >= self.unified_cols.len() as i32 { return; }
        let target_gcol = target_gcol as usize;
        let (dst_pi, dst_ci) = self.unified_cols[target_gcol];
        if src_pi != dst_pi { return; }

        let item = self.project_data[src_pi].layout.columns[src_ci].remove(self.cursor.row);
        let insert_at = self.cursor.row.min(self.project_data[dst_pi].layout.columns[dst_ci].len());
        self.project_data[dst_pi].layout.columns[dst_ci].insert(insert_at, item);
        self.cursor.col = target_gcol;
        self.cursor.row = insert_at;
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

    fn remove_separator(&mut self) {
        let (pi, ci) = match self.unified_cols.get(self.cursor.col) {
            Some(v) => *v,
            None => return,
        };
        if matches!(self.project_data[pi].layout.columns[ci].get(self.cursor.row), Some(GridItem::Separator | GridItem::Empty)) {
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

    fn create_task(&mut self, title: &str) -> PlanAction {
        // Determine the project for the new task.
        let pi = self.cursor_project_idx()
            .or_else(|| self.unified_cols.first().map(|(pi, _)| *pi))
            .unwrap_or(0);

        if pi >= self.project_data.len() {
            return PlanAction::Consumed;
        }

        let project = self.project_data[pi].project.name.clone();
        let repo_url = repo_url_for_project(&project);

        PlanAction::CreateTask {
            project,
            repo_url,
            name: title.to_string(),
            description: String::new(),
            status: "draft".to_string(),
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
        if let Ok(s) = Session::new(program, &args, cols, rows, None, Default::default()) {
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
                    // Update local state.
                    task.title = parsed.title.clone();
                    task.status = PlanStatus::from_str(&parsed.status);
                    task.difficulty = parsed.difficulty;
                    task.depends = parsed.depends.clone();
                    task.branch = parsed.branch.clone();
                    task.description = parsed.description.clone();
                    task.prompt = parsed.prompt.clone();

                    // Build fields for API update.
                    let mut fields = HashMap::new();
                    fields.insert("name".to_string(), serde_json::json!(parsed.title));
                    fields.insert("status".to_string(), serde_json::json!(parsed.status));
                    fields.insert("description".to_string(), serde_json::json!(parsed.description));
                    fields.insert("prompt".to_string(), serde_json::json!(parsed.prompt));
                    if let Some(d) = parsed.difficulty {
                        fields.insert("difficulty".to_string(), serde_json::json!(d));
                    }
                    if !parsed.depends.is_empty() {
                        fields.insert("depends".to_string(), serde_json::json!(parsed.depends));
                    }
                    if let Some(ref branch) = parsed.branch {
                        fields.insert("repo_branch".to_string(), serde_json::json!(branch));
                    }

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

    fn start_launch(&mut self) -> PlanAction {
        if let Some((pi, ti)) = self.selected_task_loc() {
            let branch_text = self.project_data[pi].tasks[ti].branch.clone().unwrap_or_default();
            self.input_mode = PlanInputMode::LaunchConfirm { project_idx: pi, task_idx: ti, branch_text };
        }
        PlanAction::Consumed
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
        let project = PlanProject { name: name.to_string(), path };
        self.projects.push(project.clone());
        let layout = load_layout(&project.path);
        self.project_data.push(ProjectData { project, tasks: vec![], layout });
        self.rebuild_unified_cols();
        self.recompute_conflicts();
        self.clamp_cursor();
        self.needs_redraw = true;
    }

    pub fn drain_editor_events(&mut self) -> bool {
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
            // Note: stop_editor returns a PlanAction but we can't return it from here.
            // The action will be picked up on the next handle_event call via handle_editing_event.
            had_event = true;
        }
        had_event
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
            PlanInputMode::NewTask { title } => self.draw_new_task_overlay(frame, area, title),
            PlanInputMode::NewProject { name, repo_url, field } => self.draw_new_project_overlay(frame, area, name, repo_url, *field),
            PlanInputMode::ProjectPicker { selected } => self.draw_project_picker(frame, area, *selected),
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
                " A-e edit \u{00b7} A-n new \u{00b7} A-s status \u{00b7} A-d del \u{00b7} A-x launch \u{00b7} A-c col \u{00b7} A-r refresh \u{00b7} A-q quit",
                dim,
            )),
        ]), help_area);
    }

    fn build_column_items<'a>(
        &'a self, col_idx: usize, project_name: &str, column: &[GridItem], width: usize, max_rows: usize,
    ) -> Vec<ListItem<'a>> {
        let mut items = Vec::new();
        let start = self.scroll_offset;
        let end = (start + max_rows).min(column.len());

        for ri in start..end {
            let is_selected = self.cursor.col == col_idx && self.cursor.row == ri;
            let in_visual = self.is_in_visual_range(col_idx, ri);
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
            ("A-j/k  nav", "A-d  delete"),
            ("A-J/K  reorder", "A-x  launch"),
            ("A-e    edit", "A-a  accept"),
            ("A-n    new", "A-p  filter"),
            ("A-s/S  status", "A-/  search"),
            ("A-g    grid", "A-t  sessions"),
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
                if flat_idx >= self.scroll_offset && items.len() < list_height {
                    let sep = "\u{2550}".repeat(inner.width.saturating_sub(2) as usize);
                    items.push(ListItem::new(Line::from(vec![
                        Span::styled(" ", dim),
                        Span::styled(sep, Style::default().fg(Color::Cyan)),
                    ])));
                }
                flat_idx += 1;
            }

            if self.is_first_col_of_project(gi) && self.project_filter.is_none() {
                if flat_idx >= self.scroll_offset && items.len() < list_height {
                    items.push(ListItem::new(Line::from(Span::styled(
                        format!(" {}", pd.project.name),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ))));
                }
                flat_idx += 1;
            }

            for (ri, grid_item) in column.iter().enumerate() {
                if flat_idx < self.scroll_offset { flat_idx += 1; continue; }
                if items.len() >= list_height { break; }
                let is_selected = self.cursor.col == gi && self.cursor.row == ri;

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

    fn draw_new_task_overlay(&self, frame: &mut Frame, area: Rect, title: &str) {
        let (w, h) = (60u16.min(area.width.saturating_sub(4)), 5u16);
        let dialog = Rect::new((area.width - w) / 2, (area.height - h) / 2, w, h);
        frame.render_widget(Clear, dialog);
        let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(Color::White))
            .title(Span::styled(" New Task ", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)));
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        frame.render_widget(Paragraph::new(vec![
            Line::from(vec![
                Span::styled("  Title: ", Style::default().fg(Color::DarkGray)),
                Span::styled(title, Style::default().fg(Color::White)),
                Span::styled("\u{2588}", Style::default().fg(Color::White)),
            ]),
            Line::from(""),
            Line::from(Span::styled("Enter create \u{00b7} Esc cancel", Style::default().fg(Color::DarkGray))),
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
