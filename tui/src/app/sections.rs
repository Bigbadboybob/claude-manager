//! Sidebar sections (doc/sidebar-sections.md): Owner-created, collapsible
//! groups of workspaces in the Sessions view's Task sub-view.
//!
//! A section is pure display state — it lives in the manifest sidecar
//! (`Manifest::sections` + `Manifest::workspace_sections`), never on the
//! planning API, the daemon, or the MCP surface. Membership is explicit per
//! workspace; a workspace with no explicit assignment INHERITS its section
//! through the task tree (a subtask renders under its parent task's
//! section, a `propose_task` row under its proposer's), and otherwise
//! renders loose below every section.

use super::*;

pub(crate) use cm_daemon::manifest::SidebarSection;

/// Sentinel stored in `workspace_sections` meaning "explicitly loose": the
/// Owner pulled this workspace OUT of the section it would otherwise
/// inherit. Distinct from an absent key, which means "auto / inherit".
pub(crate) const SECTION_NONE: &str = "";

/// Generate a fresh section id — same collision-avoidance idiom as
/// `new_workspace_id`.
pub(crate) fn new_section_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("sec-{:x}", nanos)
}

/// Step a section CHOICE one slot for the ←/→ pickers on the workspace /
/// task settings forms. The cycle is `None` (auto: inherit through the task
/// tree) → `Some("")` (explicitly loose) → each section id in display
/// order → back to `None`. An unknown stored id behaves like `None` so the
/// picker recovers from a deleted section.
pub(crate) fn cycle_section_choice(
    current: Option<&str>,
    ids: &[String],
    forward: bool,
) -> Option<String> {
    // Positions: 0 = auto, 1 = loose, 2.. = sections.
    let len = ids.len() + 2;
    let pos = match current {
        None => 0,
        Some(SECTION_NONE) => 1,
        Some(id) => ids.iter().position(|s| s == id).map(|i| i + 2).unwrap_or(0),
    };
    let next = if forward { (pos + 1) % len } else { (pos + len - 1) % len };
    match next {
        0 => None,
        1 => Some(SECTION_NONE.to_string()),
        n => Some(ids[n - 2].clone()),
    }
}

/// The parent task a workspace inherits its section from, for one task
/// bound to it: the subtask edge first, then the `propose_task` filer
/// provenance (the proposer's task id), which is how an agent-proposed
/// top-level task still groups under the proposer's project.
fn inherited_parent_task(task: &TaskEntry) -> Option<String> {
    if let Some(p) = task.parent_task_id.as_deref() {
        return Some(p.to_string());
    }
    task.metadata
        .as_ref()
        .and_then(|m| m.get("filer"))
        .and_then(|f| f.get("task_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

impl App {
    pub(crate) fn section_index(&self, id: &str) -> Option<usize> {
        self.sections.iter().position(|s| s.id == id)
    }

    pub(crate) fn section_by_id(&self, id: &str) -> Option<&SidebarSection> {
        self.sections.iter().find(|s| s.id == id)
    }

    /// Section ids in display order (the picker cycle order).
    pub(crate) fn section_ids(&self) -> Vec<String> {
        self.sections.iter().map(|s| s.id.clone()).collect()
    }

    /// Human label for a section CHOICE as stored in `workspace_sections`
    /// (see `cycle_section_choice`): `None` = auto, `Some("")` = none,
    /// `Some(id)` = the section's name (or the raw id if it was deleted).
    pub(crate) fn section_choice_label(&self, choice: Option<&str>) -> String {
        match choice {
            None => "auto".to_string(),
            Some(SECTION_NONE) => "none".to_string(),
            Some(id) => self
                .section_by_id(id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| id.to_string()),
        }
    }

    /// Resolve the section a workspace renders under: the explicit
    /// assignment when present (a `SECTION_NONE` sentinel resolves to loose),
    /// else inherited through the task tree, else `None` (loose).
    pub(crate) fn section_of_workspace(&self, wi: usize) -> Option<String> {
        let ws = self.workspaces.get(wi)?;
        let mut seen = HashSet::new();
        self.section_of_workspace_id(&ws.id, &mut seen)
    }

    fn section_of_workspace_id(
        &self,
        ws_id: &str,
        seen: &mut HashSet<String>,
    ) -> Option<String> {
        if !seen.insert(ws_id.to_string()) {
            return None; // cycle guard
        }
        if let Some(sid) = self.workspace_sections.get(ws_id) {
            if sid == SECTION_NONE {
                return None;
            }
            if self.section_index(sid).is_some() {
                return Some(sid.clone());
            }
            // Dangling assignment (section deleted out from under it) —
            // fall through to inheritance.
        }
        // A cloud session can be adopted before the planning task's workspace
        // binding arrives; its retained task_id is still a valid membership
        // edge.
        let workspace = self.workspaces.iter().find(|w| w.id == ws_id);
        for task in self.tasks.iter().filter(|t| {
            t.workspace_id.as_deref() == Some(ws_id)
                || t.task_id.as_deref().is_some_and(|tid| {
                    workspace.is_some_and(|w| {
                        w.sessions.iter().any(|s| s.task_id.as_deref() == Some(tid))
                    })
                })
        }) {
            let Some(parent_id) = inherited_parent_task(task) else {
                continue;
            };
            let bound_parent = self
                .tasks
                .iter()
                .find(|t| t.task_id.as_deref() == Some(parent_id.as_str()))
                .and_then(|t| t.workspace_id.as_deref());
            let parent_workspaces = bound_parent.into_iter().chain(
                self.workspaces.iter().filter(|w| {
                    w.sessions
                        .iter()
                        .any(|s| s.task_id.as_deref() == Some(parent_id.as_str()))
                }).map(|w| w.id.as_str()),
            );
            for parent_ws in parent_workspaces {
                if parent_ws == ws_id {
                    continue;
                }
                if let Some(sid) = self.section_of_workspace_id(parent_ws, seen) {
                    return Some(sid);
                }
            }
        }
        None
    }

    /// The section the cursor is "inside": the header itself, or the
    /// resolved section of the row's workspace. Drives A-n inheritance
    /// (a workspace created while focused inside a section joins it).
    pub(crate) fn cursor_section_id(&self) -> Option<String> {
        match &self.cursor {
            Cursor::Section(id) => Some(id.clone()),
            Cursor::Workspace(wi) | Cursor::Session(wi, _) => self.section_of_workspace(*wi),
            Cursor::Task { ws_idx, .. } => self.section_of_workspace(*ws_idx),
            Cursor::Backtest(_) => None,
        }
    }

    /// Create a section and park the cursor on its header. Returns the id.
    pub(crate) fn create_section(&mut self, name: &str, color: Option<String>) -> String {
        let id = new_section_id();
        self.sections.push(SidebarSection {
            id: id.clone(),
            name: name.to_string(),
            color,
            folded: false,
        });
        self.cursor = Cursor::Section(id.clone());
        self.cursor_column = SidebarColumn::Main;
        self.save_session_manifest();
        self.needs_redraw = true;
        id
    }

    /// Rename / recolor. An empty name keeps the old one.
    pub(crate) fn update_section(&mut self, id: &str, name: &str, color: Option<String>) {
        if let Some(sec) = self.sections.iter_mut().find(|s| s.id == id) {
            if !name.is_empty() {
                sec.name = name.to_string();
            }
            sec.color = color;
        }
        self.save_session_manifest();
        self.needs_redraw = true;
    }

    /// Delete a section. Its explicit assignments are dropped (members
    /// become loose, or follow whatever their task tree resolves to);
    /// nothing about the workspaces or their sessions changes.
    pub(crate) fn delete_section(&mut self, id: &str) {
        self.sections.retain(|s| s.id != id);
        self.workspace_sections.retain(|_, sid| sid != id);
        if matches!(&self.cursor, Cursor::Section(cid) if cid == id) {
            self.cursor = Cursor::Workspace(0);
        }
        self.clamp_cursor();
        self.save_session_manifest();
        self.needs_redraw = true;
    }

    /// Fold/unfold the section under the cursor. Returns whether anything
    /// toggled (the input path uses this to decide key consumption).
    pub(crate) fn toggle_section_fold(&mut self) -> bool {
        let Cursor::Section(id) = &self.cursor else {
            return false;
        };
        let id = id.clone();
        let Some(sec) = self.sections.iter_mut().find(|s| s.id == id) else {
            return false;
        };
        sec.folded = !sec.folded;
        self.save_session_manifest();
        self.needs_redraw = true;
        true
    }

    /// Move the cursor's section one slot down (+1) or up (-1) in display
    /// order. Clamped at the ends; the cursor follows the section.
    pub(crate) fn move_section(&mut self, dir: i32) -> bool {
        let Cursor::Section(id) = &self.cursor else {
            return false;
        };
        let Some(i) = self.section_index(id) else {
            return false;
        };
        let j = i as i64 + dir as i64;
        if j < 0 || j >= self.sections.len() as i64 {
            return false;
        }
        self.sections.swap(i, j as usize);
        self.save_session_manifest();
        self.needs_redraw = true;
        true
    }

    /// Workspaces the Task sub-view renders (open, not past, not
    /// continuous-only), in `self.workspaces` order — the shared visibility
    /// filter for the layout builder and the folded-header rollup.
    pub(crate) fn task_view_visible_workspaces(
        &self,
        members: &HashSet<(usize, usize)>,
    ) -> Vec<usize> {
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(wi, ws)| {
                if ws.is_closed || self.is_past_workspace(*wi) {
                    return false;
                }
                // A workspace whose ONLY sessions are continuous members
                // renders in the continuous column, never here.
                !(!ws.sessions.is_empty()
                    && (0..ws.sessions.len()).all(|si| members.contains(&(*wi, si))))
            })
            .map(|(wi, _)| wi)
            .collect()
    }

    /// Visible member workspaces of a section, for the folded header's
    /// rollup and the A-i peek.
    pub(crate) fn section_members(&self, id: &str) -> Vec<usize> {
        let members = self.continuous_members();
        self.task_view_visible_workspaces(&members)
            .into_iter()
            .filter(|wi| self.section_of_workspace(*wi).as_deref() == Some(id))
            .collect()
    }

    /// `(workspaces, running, idle)` counts for a section's folded header.
    pub(crate) fn section_rollup(&self, id: &str) -> (usize, usize, usize) {
        let members = self.section_members(id);
        let mut running = 0;
        let mut idle = 0;
        for wi in &members {
            for ts in &self.workspaces[*wi].sessions {
                if ts.session.exited {
                    continue;
                }
                match ts.status {
                    SessionStatus::Running => running += 1,
                    SessionStatus::Idle => idle += 1,
                }
            }
        }
        (members.len(), running, idle)
    }
}

#[cfg(test)]
mod section_choice_tests {
    use super::*;

    #[test]
    fn cycle_walks_auto_none_sections_and_wraps() {
        let ids = vec!["s1".to_string(), "s2".to_string()];
        let a = cycle_section_choice(None, &ids, true);
        assert_eq!(a.as_deref(), Some(SECTION_NONE));
        let b = cycle_section_choice(a.as_deref(), &ids, true);
        assert_eq!(b.as_deref(), Some("s1"));
        let c = cycle_section_choice(b.as_deref(), &ids, true);
        assert_eq!(c.as_deref(), Some("s2"));
        let d = cycle_section_choice(c.as_deref(), &ids, true);
        assert_eq!(d, None, "wraps back to auto");
        // Backwards from auto lands on the last section.
        assert_eq!(cycle_section_choice(None, &ids, false).as_deref(), Some("s2"));
        // A deleted id behaves like auto.
        assert_eq!(cycle_section_choice(Some("gone"), &ids, true).as_deref(), Some(SECTION_NONE));
        // No sections: auto <-> none.
        assert_eq!(cycle_section_choice(None, &[], true).as_deref(), Some(SECTION_NONE));
        assert_eq!(cycle_section_choice(Some(SECTION_NONE), &[], true), None);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn bare_ws(id: &str) -> Workspace {
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
            pinned: false,
            sessions: vec![],
            tombstones: vec![],
        }
    }

    fn task(id: &str, ws: &str, parent: Option<&str>) -> TaskEntry {
        TaskEntry {
            task_id: Some(id.to_string()),
            name: id.to_string(),
            api_status: TaskStatus::Running,
            repo_url: None,
            prompt: None,
            wip_branch: None,
            session_id: None,
            blocked_at: None,
            is_cloud: false,
            is_continuous: false,
            workspace_id: Some(ws.to_string()),
            project: None,
            parent_task_id: parent.map(str::to_string),
            worktree_mode: WorktreeMode::Inherit,
            metadata: None,
        }
    }

    fn test_app() -> App {
        let _g = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
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

    #[test]
    fn helper_shortcut_never_starts_or_submits_search() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        let mut app = test_app();
        for mode in [ViewMode::Sessions, ViewMode::Planning] {
            app.view_mode = mode;
            for (code, modifiers) in [
                (KeyCode::Char('?'), KeyModifiers::ALT),
                (KeyCode::Char('?'), KeyModifiers::ALT | KeyModifiers::SHIFT),
                (KeyCode::Char('/'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            ] {
                app.input_mode = InputMode::Normal;
                app.sidebar_filter = Some("keep-filter".into());
                app.keybinding_helper_visible = true;
                assert!(app.handle_event(&Event::Key(KeyEvent::new(code, modifiers))));
                assert!(!app.keybinding_helper_visible);
                assert!(!app.planning.keybinding_helper_visible);
                assert!(matches!(app.input_mode, InputMode::Normal));
                assert_eq!(app.sidebar_filter.as_deref(), Some("keep-filter"));
                assert!(app.handle_event(&Event::Key(KeyEvent::new(code, modifiers))));
                assert!(app.keybinding_helper_visible);
            }
        }
        app.view_mode = ViewMode::Sessions;
        let slash = Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::ALT));
        app.handle_event(&slash);
        assert!(app.sidebar_filter.is_none(), "plain A-/ still clears the filter");
        app.handle_event(&slash);
        assert!(matches!(app.input_mode, InputMode::SidebarSearch { .. }));
        app.input_mode = InputMode::SidebarSearch { query: "unfinished".into() };
        app.handle_event(&Event::Key(KeyEvent::new(
            KeyCode::Char('/'), KeyModifiers::ALT | KeyModifiers::SHIFT,
        )));
        assert!(matches!(&app.input_mode, InputMode::SidebarSearch { query } if query == "unfinished"));
        assert!(app.sidebar_filter.is_none(), "helper toggle must not apply a search modal");
    }

    /// Workspace ids in the order the Task sub-view lays them out, with
    /// section headers rendered as `#<id>`.
    fn layout(app: &App) -> Vec<String> {
        app.visual_items_task()
            .iter()
            .filter_map(|vi| match vi {
                VisualItem::WorkspaceHeader(wi) => Some(app.workspaces[*wi].id.clone()),
                VisualItem::SectionHeader(id) => Some(format!("#{id}")),
                _ => None,
            })
            .collect()
    }

    /// a: explicit member. b: inherits via its task's parent (bound to a).
    /// c: would inherit too but is explicitly pulled out. d: loose.
    fn sectioned_app() -> App {
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = vec![bare_ws("a"), bare_ws("b"), bare_ws("c"), bare_ws("d")];
        app.tasks = vec![
            task("ta", "a", None),
            task("tb", "b", Some("ta")),
            task("tc", "c", Some("ta")),
        ];
        app.sections.push(SidebarSection {
            id: "s1".into(),
            name: "Project".into(),
            color: None,
            folded: false,
        });
        app.workspace_sections.insert("a".into(), "s1".into());
        app.workspace_sections.insert("c".into(), SECTION_NONE.into());
        app
    }

    #[test]
    fn layout_groups_explicit_and_inherited_members_then_loose() {
        let app = sectioned_app();
        assert_eq!(layout(&app), ["#s1", "a", "b", "c", "d"]);
        assert_eq!(app.section_of_workspace(1).as_deref(), Some("s1"), "b inherits via tb→ta");
        assert_eq!(app.section_of_workspace(2), None, "explicit none beats inheritance");
        assert_eq!(app.section_rollup("s1").0, 2);
    }

    fn add_task_session(app: &mut App, wi: usize, tid: &str) {
        let session = Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None).unwrap();
        let mut ts = make_simple_session_with_uid(format!("uid-{tid}"), tid, "codex", session, None);
        ts.task_id = Some(tid.into());
        app.workspaces[wi].sessions.push(ts);
    }

    #[test]
    fn adopted_child_inherits_when_both_task_bindings_are_missing() {
        let mut app = sectioned_app();
        app.tasks[0].workspace_id = None;
        app.tasks[1].workspace_id = None;
        add_task_session(&mut app, 0, "ta");
        add_task_session(&mut app, 1, "tb");
        assert_eq!(app.section_of_workspace(1).as_deref(), Some("s1"));
        app.workspace_sections.insert("b".into(), SECTION_NONE.into());
        assert_eq!(app.section_of_workspace(1), None);
    }

    #[test]
    fn task_section_save_survives_cursor_change_and_missing_binding() {
        let mut app = sectioned_app();
        app.tasks[1].workspace_id = None;
        app.cursor = Cursor::Task { ws_idx: 1, task_id: "tb".into() };
        app.open_session_settings();
        let InputMode::TaskSettings { section, name, .. } = &mut app.input_mode else { panic!("task settings"); };
        *section = Some("s1".into());
        name.clear(); // Only the section changes; no API rename is needed.
        app.workspaces.swap(1, 3);
        app.cursor = Cursor::Workspace(0);
        app.handle_input_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(app.workspace_sections.get("b").map(String::as_str), Some("s1"));
        assert!(!app.workspace_sections.contains_key("d"));
    }

    #[test]
    fn workspace_section_save_survives_reorder() {
        let mut app = sectioned_app();
        app.cursor = Cursor::Workspace(1);
        app.open_session_settings();
        let InputMode::WorkspaceSettings { section, .. } = &mut app.input_mode else { panic!("workspace settings"); };
        *section = Some("s1".into());
        app.workspaces.swap(1, 3);
        app.handle_input_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(app.workspace_sections.get("b").map(String::as_str), Some("s1"));
        assert!(!app.workspace_sections.contains_key("d"));
    }

    #[test]
    fn folded_section_hides_members_but_keeps_header() {
        let mut app = sectioned_app();
        app.sections[0].folded = true;
        assert_eq!(layout(&app), ["#s1", "c", "d"]);
        // Space on the header unfolds it again.
        app.cursor = Cursor::Section("s1".into());
        assert!(app.toggle_section_fold());
        assert_eq!(layout(&app), ["#s1", "a", "b", "c", "d"]);
    }

    #[test]
    fn empty_section_still_renders_its_header() {
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = vec![bare_ws("a")];
        app.sections.push(SidebarSection {
            id: "s1".into(),
            name: "Empty".into(),
            color: None,
            folded: false,
        });
        assert_eq!(layout(&app), ["#s1", "a"]);
    }

    #[test]
    fn no_sections_is_byte_identical_to_flat_layout() {
        let mut app = test_app();
        app.sidebar_view = SidebarView::Task;
        app.workspaces = vec![bare_ws("a"), bare_ws("b")];
        let items = app.visual_items_task();
        assert!(matches!(items[0], VisualItem::WorkspaceHeader(0)));
        assert!(matches!(items[1], VisualItem::Separator));
        assert!(matches!(items[2], VisualItem::WorkspaceHeader(1)));
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn cursor_section_id_covers_header_members_and_loose_rows() {
        let mut app = sectioned_app();
        app.cursor = Cursor::Section("s1".into());
        assert_eq!(app.cursor_section_id().as_deref(), Some("s1"));
        app.cursor = Cursor::Workspace(1); // b, inherited
        assert_eq!(app.cursor_section_id().as_deref(), Some("s1"));
        app.cursor = Cursor::Task { ws_idx: 0, task_id: "ta".into() };
        assert_eq!(app.cursor_section_id().as_deref(), Some("s1"));
        app.cursor = Cursor::Workspace(3); // d, loose
        assert_eq!(app.cursor_section_id(), None);
    }

    #[test]
    fn navigate_walks_through_the_section_header() {
        let mut app = sectioned_app();
        app.cursor = Cursor::Workspace(0);
        app.navigate(-1);
        assert_eq!(app.cursor, Cursor::Section("s1".into()));
        app.navigate(1);
        assert_eq!(app.cursor, Cursor::Workspace(0));
    }

    #[test]
    fn delete_section_drops_assignments_and_reparks_cursor() {
        let mut app = sectioned_app();
        app.cursor = Cursor::Section("s1".into());
        app.delete_section("s1");
        assert!(app.sections.is_empty());
        assert!(!app.workspace_sections.contains_key("a"), "explicit membership dropped");
        assert_eq!(
            app.workspace_sections.get("c").map(String::as_str),
            Some(SECTION_NONE),
            "an explicit-none stays (it names no section)"
        );
        assert_eq!(app.cursor, Cursor::Workspace(0));
        assert_eq!(layout(&app), ["a", "b", "c", "d"]);
    }

    #[test]
    fn move_section_reorders_and_clamps() {
        let mut app = sectioned_app();
        app.sections.push(SidebarSection {
            id: "s2".into(),
            name: "Second".into(),
            color: None,
            folded: false,
        });
        app.cursor = Cursor::Section("s2".into());
        assert!(app.move_section(-1));
        assert_eq!(app.section_ids(), ["s2", "s1"]);
        assert!(!app.move_section(-1), "already first");
        assert_eq!(layout(&app)[0], "#s2");
    }

    #[test]
    fn dangling_assignment_falls_back_to_inheritance() {
        let mut app = sectioned_app();
        // b explicitly points at a section that no longer exists.
        app.workspace_sections.insert("b".into(), "gone".into());
        assert_eq!(app.section_of_workspace(1).as_deref(), Some("s1"));
    }
}
