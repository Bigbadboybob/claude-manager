//! Sidebar sections (doc/sidebar-sections.md): Owner-created, collapsible
//! groups of workspaces in the Sessions view's Task sub-view.
//!
//! A section is pure display state — it lives in the manifest sidecar
//! (`Manifest::sections` + `Manifest::workspace_sections`), separate from the
//! planning API. `sidebar_remote` publishes discovery and accepts scoped,
//! durable agent assignment requests from the daemon. Membership is explicit per
//! workspace; a workspace with no explicit assignment INHERITS its section
//! through the task tree (a subtask renders under its parent task's
//! section, a `propose_task` row under its proposer's), and otherwise
//! renders loose below every section. In the nested work tree, an explicit
//! assignment that differs from the block's inherited root section detaches
//! that workspace as a new top-level block; an explicit loose assignment
//! detaches it into the loose list. An explicit assignment equal to the root
//! section leaves the block nested.

use super::*;
use ratatui::style::Color;

pub(crate) use cm_daemon::manifest::SidebarSection;

/// Sentinel stored in `workspace_sections` meaning "explicitly loose": the
/// Owner pulled this workspace OUT of the section it would otherwise
/// inherit. Distinct from an absent key, which means "auto / inherit".
pub(crate) const SECTION_NONE: &str = "";

/// Resolve an explicit member assignment, then its manager's, with the
/// empty sentinel stopping inheritance. Deleted section ids act as auto.
pub(super) fn member_section(
    snap: &super::org_model::OrgSnapshot,
    assignments: &HashMap<String, String>,
    sections: &[SidebarSection],
    member: &str,
) -> Option<String> {
    let mut seen = HashSet::new();
    let mut current = Some(member);
    while let Some(id) = current.filter(|id| seen.insert(*id)) {
        if let Some(choice) = assignments.get(id) {
            if choice == SECTION_NONE { return None; }
            if sections.iter().any(|s| s.id == *choice) { return Some(choice.clone()); }
        }
        current = snap.member(id).and_then(|m| m.manager_member_id.as_deref());
    }
    None
}

/// Evidence for a legacy workspace assignment. `complete` is false if even
/// one session could not be attributed, including an offline host.
struct MigrationWorkspace {
    mains: HashSet<String>,
    members: HashSet<String>,
    boxed_members: HashSet<String>,
    complete: bool,
}

#[derive(Default)]
struct SectionMigration {
    members: HashMap<String, String>,
    overrides: HashMap<String, String>,
    /// (workspace id, old choice, explanation), in deterministic order.
    unresolved: Vec<(String, String, String)>,
}

fn migrate_sections(
    old: &HashMap<String, String>,
    existing: &HashMap<String, String>,
    evidence: &HashMap<String, MigrationWorkspace>,
) -> SectionMigration {
    let mut result = SectionMigration { members: existing.clone(), ..Default::default() };
    let mut candidates: HashMap<&str, Vec<(&str, &str)>> = HashMap::new();
    for (ws, choice) in old {
        let Some(info) = evidence.get(ws) else {
            result.unresolved.push((ws.clone(), choice.clone(), "workspace unavailable".into()));
            continue;
        };
        if !info.mains.is_empty() {
            for member in &info.mains {
                candidates.entry(member).or_default().push((ws, choice));
            }
        } else if info.complete && info.members.len() == 1 {
            result.overrides.insert(ws.clone(), choice.clone());
            for member in &info.boxed_members {
                result.unresolved.push((ws.clone(), choice.clone(), format!("member {member}: kept in continuous box")));
            }
        } else {
            result.unresolved.push((ws.clone(), choice.clone(), "workspace membership is ambiguous".into()));
        }
    }
    for (member, rows) in candidates {
        if let Some(current) = existing.get(member) {
            for (ws, choice) in rows {
                if choice != current {
                    result.unresolved.push((ws.into(), choice.into(), format!("member {member} already has an assignment")));
                }
            }
        } else if rows.iter().all(|(_, choice)| *choice == rows[0].1) {
            result.members.insert(member.into(), rows[0].1.into());
        } else {
            for (ws, choice) in rows {
                result.unresolved.push((ws.into(), choice.into(), format!("conflicting assignments for member {member}")));
            }
        }
    }
    result.unresolved.sort();
    result
}

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

/// Step a section CHOICE one slot for the ←/→ picker on the workspace
/// settings form. The cycle is `None` (auto: inherit through the task
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

/// What section inheritance reads, indexed once ([`App::section_lookup`])
/// so that resolving a workspace costs its task chain rather than a scan of
/// every workspace: the laptop keeps thousands of closed ones, and the
/// sidebar resolves a section for each visible block.
pub(crate) struct SectionLookup<'a> {
    /// The first workspace with each id.
    workspace: HashMap<&'a str, usize>,
    /// Work-list rows (indices into `App::tasks`) bound to each workspace.
    tasks_by_workspace: HashMap<&'a str, Vec<usize>>,
    /// Work-list rows by task id, in list order.
    tasks_by_id: HashMap<&'a str, Vec<usize>>,
    /// Workspaces with a session on each task, in workspace order.
    session_workspaces: HashMap<&'a str, Vec<usize>>,
}

impl App {
    pub(super) fn workspace_override_conflicts(&self, ws: &str, choice: Option<&str>) -> Vec<(String, String, String)> {
        let (Some(snap), Some(choice)) = (self.sidebar_org_live(), choice) else { return Vec::new(); };
        let evidence = self.section_migration_evidence(snap);
        let Some(info) = evidence.get(ws) else { return Vec::new(); };
        let wanted = (!choice.is_empty()).then_some(choice);
        let mut issues = Vec::new();
        for id in &info.mains {
            let effective = member_section(snap, &self.member_sections, &self.sections, id);
            if effective.as_deref() != wanted {
                issues.push((ws.into(), choice.into(), format!("override ignored: home checkout of member {id}")));
            }
        }
        let box_owners = self.org_placement(snap).box_owners(snap);
        for id in &info.boxed_members {
            if let Some(lead) = box_owners.get(id).and_then(|team| snap.team(team))
                .and_then(|team| team.continuous.as_ref()?.lead_member_id.as_deref()) {
                if member_section(snap, &self.member_sections, &self.sections, lead).as_deref() != wanted {
                    issues.push((ws.into(), choice.into(), format!("member {id}: kept in continuous box")));
                }
            }
        }
        issues.sort();
        issues.dedup();
        issues
    }

    pub(super) fn member_section_form(&self, wi: usize, si: usize) -> Option<super::input::MemberSectionForm> {
        let snap = self.sidebar_org_live()?;
        let placement = self.org_placement(snap);
        let (id, _) = placement.members.iter().find(|(_, rows)| rows.contains(&(wi, si)))?;
        let member = snap.member(id)?;
        let ts = self.workspaces.get(wi)?.sessions.get(si)?;
        let editable = member.main.as_ref().is_some_and(|main|
            main.session_uid.as_deref() == Some(ts.uid.as_str())
                && main.daemon_id.as_deref() == snap.host_daemons.get(ts.host_id.as_str()).map(String::as_str));
        let effective = member_section(snap, &self.member_sections, &self.sections, id);
        Some(super::input::MemberSectionForm {
            member_id: id.clone(),
            choice: self.member_sections.get(id).cloned(),
            effective_label: effective.as_deref().map_or_else(|| "none".into(), |sid| self.section_choice_label(Some(sid))),
            editable,
        })
    }

    /// Apply the legacy migration once a complete org/host view and restored
    /// sessions are available. Persist the results and the Owner's unresolved
    /// list atomically; a failed save leaves the old assignments in place.
    pub(super) fn member_section_migration_tick(&mut self) {
        if !self.sessions_restored || self.defer_manifest_save.get() { return; }
        if self.member_section_migration_open && !matches!(self.input_mode, InputMode::TaskPeek { .. }) {
            let issues = std::mem::take(&mut self.member_section_migration_issues);
            if !self.try_save_session_manifest() { self.member_section_migration_issues = issues; }
            self.member_section_migration_open = false;
        }
        if !self.member_section_migration_done || !self.pending_member_sections.is_empty() {
            if let Some(snap) = self.sidebar_org_live().cloned() {
                let complete_hosts = self.workspaces.iter().filter(|w| !w.is_closed)
                    .flat_map(|w| &w.sessions).filter(|s| super::tree::is_agent_engine(&s.session_type))
                    .all(|s| snap.host_daemons.contains_key(s.host_id.as_str()));
                if complete_hosts {
                    let evidence = self.section_migration_evidence(&snap);
                    let old_workspaces = self.workspace_sections.clone();
                    let old_members = self.member_sections.clone();
                    let old_pending = self.pending_member_sections.clone();
                    let old_fallback = self.fallback_only_workspace_sections.clone();
                    let old_issues = self.member_section_migration_issues.clone();
                    let old_done = self.member_section_migration_done;
                    let mut changed = false;
                    if !self.member_section_migration_done {
                        // A-n workspaces awaiting their main's stamp are new,
                        // not legacy assignments; keep their temporary choices
                        // until the member is known.
                        let mut legacy = self.workspace_sections.clone();
                        for ws in self.pending_member_sections.keys() { legacy.remove(ws); }
                        let mut migrated = migrate_sections(&legacy, &self.member_sections, &evidence);
                        for ws in self.pending_member_sections.keys() {
                            if let Some(choice) = self.workspace_sections.get(ws) {
                                migrated.overrides.insert(ws.clone(), choice.clone());
                            }
                        }
                        self.member_sections = migrated.members;
                        // Retain migrated choices for offline/old-viewer fallback;
                        // only the genuine single-member stream overrides stay live.
                        self.fallback_only_workspace_sections.extend(legacy.keys()
                            .filter(|ws| !migrated.overrides.contains_key(*ws)).cloned());
                        self.member_section_migration_issues.extend(migrated.unresolved);
                        self.member_section_migration_done = true;
                        changed = true;
                    }
                    for (ws, choice) in old_pending.iter() {
                        if old_workspaces.get(ws) != Some(choice) {
                            self.pending_member_sections.remove(ws);
                            changed = true;
                            continue;
                        }
                        let Some(info) = evidence.get(ws).filter(|info| !info.mains.is_empty()) else { continue; };
                        for member in &info.mains {
                            if let Some(existing) = self.member_sections.get(member) {
                                if existing != choice {
                                    self.member_section_migration_issues.push((ws.clone(), choice.clone(), format!("member {member} already has an assignment")));
                                }
                            } else {
                                self.member_sections.insert(member.clone(), choice.clone());
                            }
                        }
                        if self.workspace_sections.get(ws) == Some(choice) { self.workspace_sections.remove(ws); }
                        self.fallback_only_workspace_sections.remove(ws);
                        self.pending_member_sections.remove(ws);
                        changed = true;
                    }
                    if changed {
                        self.member_section_migration_issues.sort();
                        self.member_section_migration_issues.dedup();
                        if !self.try_save_session_manifest() {
                            self.workspace_sections = old_workspaces;
                            self.member_sections = old_members;
                            self.pending_member_sections = old_pending;
                            self.fallback_only_workspace_sections = old_fallback;
                            self.member_section_migration_issues = old_issues;
                            self.member_section_migration_done = old_done;
                        } else { self.needs_redraw = true; }
                    }
                }
            }
        }
        if !self.member_section_migration_open && !self.member_section_migration_issues.is_empty()
            && self.focused && self.view_mode == ViewMode::Sessions && matches!(self.input_mode, InputMode::Normal)
        {
            let mut lines = vec![PeekLine::Title("Sidebar section assignments needing review".into()), PeekLine::Blank];
            for (workspace, choice, reason) in &self.member_section_migration_issues {
                let name = self.workspaces.iter().find(|ws| ws.id == *workspace).map_or(workspace.as_str(), |ws| ws.name.as_str());
                lines.push(PeekLine::Text(format!("{name} ({workspace}) · {} · {reason}", self.section_choice_label(Some(choice)))));
            }
            self.input_mode = InputMode::TaskPeek { lines, scroll: 0, max_scroll: 0 };
            self.member_section_migration_open = true;
            self.needs_redraw = true;
        }
    }

    /// Evidence is tied to the daemon UUID, not the viewer's host alias.
    /// Shells follow the workspace's agent member and do not themselves
    /// become members. Missing identities make an override ambiguous.
    fn section_migration_evidence(&self, snap: &super::org_model::OrgSnapshot) -> HashMap<String, MigrationWorkspace> {
        let mut result = HashMap::new();
        for ws in &self.workspaces {
            let mut info = MigrationWorkspace { mains: HashSet::new(), members: HashSet::new(), boxed_members: HashSet::new(), complete: true };
            for ts in &ws.sessions {
                if !super::tree::is_agent_engine(&ts.session_type) { continue; }
                let member = snap.host_daemons.get(ts.host_id.as_str())
                    .and_then(|daemon| snap.member_by_session(daemon, &ts.uid));
                let Some(member) = member else { info.complete = false; continue; };
                info.members.insert(member.id.clone());
                if snap.teams.iter().filter(|team| !team.archived()).filter_map(|team|
                    team.continuous.as_ref()?.lead_member_id.as_deref()).any(|lead|
                    snap.in_member_subtree(&member.id, lead)) {
                    info.boxed_members.insert(member.id.clone());
                }
                if member.main.as_ref().is_some_and(|main|
                    main.session_uid.as_deref() == Some(ts.uid.as_str())
                        && main.daemon_id.as_deref() == snap.host_daemons.get(ts.host_id.as_str()).map(String::as_str)) {
                    info.mains.insert(member.id.clone());
                }
            }
            result.insert(ws.id.clone(), info);
        }
        result
    }

    /// Resolve each displayed row independently so filtering or scrolling away
    /// a section heading cannot lose its tint. Only gaps within the same
    /// section share the surface; boundaries and loose rows stay transparent.
    /// `section_of` is the section a workspace's rows draw in: its root
    /// block's, as the work tree resolved it.
    pub(super) fn sidebar_section_backgrounds<'s>(
        &self,
        rows: &'s [VisualItem],
        section_of: &dyn Fn(usize) -> Option<&'s str>,
        row_sections: &'s HashMap<VisualItem, String>,
        boxed_rows: &HashSet<VisualItem>,
    ) -> Vec<Option<Color>> {
        if !self.tree_view() {
            return vec![None; rows.len()];
        }
        let mut sections: Vec<Option<&str>> = rows.iter().map(|row| row_sections.get(row).map(String::as_str).or_else(|| match row {
            VisualItem::SectionHeader(id) => Some(id.as_str()),
            VisualItem::WorkspaceHeader(wi) | VisualItem::Session(wi, _)
            | VisualItem::WorkflowHeader { ws_idx: wi, .. } => section_of(*wi),
            _ => None,
        })).collect();
        // A division header sits inside the section of the rows before it.
        for i in 0..rows.len() {
            if matches!(rows[i], VisualItem::DivisionHeader(_)) {
                sections[i] = rows[..i]
                    .iter()
                    .zip(&sections[..i])
                    .rev()
                    .find(|(r, _)| !matches!(r, VisualItem::Separator))
                    .and_then(|(r, s)| (!matches!(r, VisualItem::SectionEnd)).then_some(*s).flatten());
            }
        }
        rows.iter().enumerate().map(|(i, row)| {
            let section = if matches!(row, VisualItem::Separator) {
                let before = i.checked_sub(1).and_then(|i| sections[i]);
                let after = sections.get(i + 1).copied().flatten();
                before.filter(|_| before == after)
            } else {
                sections[i]
            };
            if matches!(row, VisualItem::ContinuousTeamHeader(_) | VisualItem::ContinuousTeamSession(..)) || boxed_rows.contains(row) {
                Some(theme::sidebar_section_bg(Some("magenta"), self.global_settings.tint_strength()))
            } else {
                section.and_then(|id| self.section_by_id(id))
                    .map(|s| theme::sidebar_section_bg(s.color.as_deref(), self.global_settings.tint_strength()))
            }
        }).collect()
    }

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
    /// Builds the index each call; resolve many workspaces with
    /// [`App::section_of_workspace_in`] and one [`App::section_lookup`].
    pub(crate) fn section_of_workspace(&self, wi: usize) -> Option<String> {
        self.section_of_workspace_in(&self.section_lookup(), wi)
    }

    /// [`App::section_of_workspace`] against a prebuilt index.
    pub(crate) fn section_of_workspace_in(&self, lookup: &SectionLookup<'_>, wi: usize) -> Option<String> {
        let ws = self.workspaces.get(wi)?;
        let mut seen = HashSet::new();
        self.section_of_workspace_id(lookup, &ws.id, &mut seen)
    }

    /// Index what section inheritance reads: one pass over the workspaces
    /// and the work list.
    pub(crate) fn section_lookup(&self) -> SectionLookup<'_> {
        let mut lookup = SectionLookup {
            workspace: HashMap::new(),
            tasks_by_workspace: HashMap::new(),
            tasks_by_id: HashMap::new(),
            session_workspaces: HashMap::new(),
        };
        for (wi, ws) in self.workspaces.iter().enumerate() {
            // Only a workspace's sessions are read once it is found, so a
            // closed one without sessions (thousands, kept for A-O) resolves
            // exactly as a missing one.
            if ws.is_closed && ws.sessions.is_empty() {
                continue;
            }
            lookup.workspace.entry(ws.id.as_str()).or_insert(wi);
            for tid in ws.sessions.iter().filter_map(|s| s.task_id.as_deref()) {
                let homes = lookup.session_workspaces.entry(tid).or_default();
                if homes.last() != Some(&wi) {
                    homes.push(wi);
                }
            }
        }
        for (i, t) in self.tasks.iter().enumerate() {
            if let Some(ws) = t.workspace_id.as_deref() {
                lookup.tasks_by_workspace.entry(ws).or_default().push(i);
            }
            if let Some(id) = t.task_id.as_deref() {
                lookup.tasks_by_id.entry(id).or_default().push(i);
            }
        }
        lookup
    }

    fn section_of_workspace_id(
        &self,
        lookup: &SectionLookup<'_>,
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
        let workspace = lookup.workspace.get(ws_id).map(|&wi| &self.workspaces[wi]);
        let session_tasks: Vec<&str> = workspace
            .into_iter()
            .flat_map(|w| &w.sessions)
            .filter_map(|s| s.task_id.as_deref())
            .collect();
        // The work-list rows bound to the workspace or to one of its
        // sessions' tasks, in list order.
        let mut rows: Vec<usize> = lookup.tasks_by_workspace.get(ws_id).cloned().unwrap_or_default();
        for tid in &session_tasks {
            rows.extend(lookup.tasks_by_id.get(tid).into_iter().flatten());
        }
        rows.sort_unstable();
        rows.dedup();
        let mut parent_ids: Vec<String> =
            rows.iter().filter_map(|&i| inherited_parent_task(&self.tasks[i])).collect();
        // `self.tasks` drops terminal tasks. A live session bound to a done
        // task (a long-lived lead, e.g. F-Lead on "F-Plan-Lead") inherits
        // through the planning snapshot's parent edge instead, so its block
        // keeps its section after a viewer restart.
        for tid in session_tasks {
            if lookup.tasks_by_id.contains_key(tid) {
                continue;
            }
            if let Some(Some(parent)) = self.planning_task_parent(tid) {
                if !parent_ids.contains(&parent) {
                    parent_ids.push(parent);
                }
            }
        }
        for parent_id in parent_ids {
            let bound_parent = lookup
                .tasks_by_id
                .get(parent_id.as_str())
                .and_then(|rows| rows.first())
                .and_then(|&i| self.tasks[i].workspace_id.as_deref());
            let parent_workspaces = bound_parent.into_iter().chain(
                lookup
                    .session_workspaces
                    .get(parent_id.as_str())
                    .into_iter()
                    .flatten()
                    .map(|&wi| self.workspaces[wi].id.as_str()),
            );
            for parent_ws in parent_workspaces {
                if parent_ws == ws_id {
                    continue;
                }
                if let Some(sid) = self.section_of_workspace_id(lookup, parent_ws, seen) {
                    return Some(sid);
                }
            }
        }
        None
    }

    /// The section the cursor is "inside": the header itself, or the
    /// resolved section of the row's workspace. The tree's block roots have
    /// already applied explicit detachments, so this also handles a detached
    /// nested workspace. Drives A-n inheritance (a workspace created while
    /// focused inside a section joins it).
    pub(crate) fn cursor_section_id(&self) -> Option<String> {
        match &self.cursor {
            Cursor::Section(id) => Some(id.clone()),
            Cursor::Session(wi, si) => {
                let tree = self.task_tree_shared();
                let row = VisualItem::Session(*wi, *si);
                let boxed = VisualItem::ContinuousTeamSession(*wi, *si);
                match tree.row_sections.get(&row).or_else(|| tree.row_sections.get(&boxed)) {
                    Some(section) => (!section.is_empty()).then(|| section.clone()),
                    None => {
                        let root = tree.block_root.get(wi).copied().unwrap_or(*wi);
                        self.section_of_workspace(root)
                    }
                }
            }
            // The tree's root reflects explicit section detachments.
            Cursor::Workspace(wi) => {
                let root = self.task_tree_shared().block_root.get(wi).copied().unwrap_or(*wi);
                self.section_of_workspace(root)
            }
            Cursor::Org(key) if key.starts_with("sidebar:") => self.task_tree_shared().row_sections.get(&VisualItem::OrgHeader { key: key.clone(), depth: 0 })
                .filter(|section| !section.is_empty()).cloned(),
            Cursor::Division(key) => key.section.clone(),
            Cursor::Backtest(_) | Cursor::Org(_) => None,
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
        self.fallback_only_workspace_sections.retain(|ws| self.workspace_sections.contains_key(ws));
        self.member_sections.retain(|_, sid| sid != id);
        self.pending_member_sections.retain(|_, sid| sid != id);
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
        _members: &HashSet<(usize, usize)>,
    ) -> Vec<usize> {
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(wi, ws)| {
                !ws.is_closed && !self.is_past_workspace(*wi)
            })
            .map(|(wi, _)| wi)
            .collect()
    }

    /// Visible member workspaces of a section, for the folded header's
    /// rollup and the A-i peek.
    pub(crate) fn section_members(&self, id: &str) -> Vec<usize> {
        self.section_members_in(id, &self.task_tree_shared())
    }

    /// `section_members` from an already-built tree, which resolved each
    /// visible workspace's section (a nested block's is its root
    /// block's).
    pub(super) fn section_members_in(&self, id: &str, tree: &super::tree::TaskTree) -> Vec<usize> {
        tree.section_members.get(id).cloned().unwrap_or_default()
    }

    /// `(workspaces, running, idle)` counts for a section's folded header.
    pub(crate) fn section_rollup(&self, id: &str) -> (usize, usize, usize) {
        self.section_rollup_in(id, &self.task_tree_shared())
    }

    /// `section_rollup` against an already-built tree (one per frame).
    pub(super) fn section_rollup_in(&self, id: &str, tree: &super::tree::TaskTree) -> (usize, usize, usize) {
        let members = self.section_members_in(id, tree);
        let (_, running, idle, _) = match tree.section_sessions.get(id) {
            Some(rows) => self.rollup(rows.iter().map(|(wi, si)| &self.workspaces[*wi].sessions[*si])),
            None => self.rollup(members.iter().flat_map(|wi| &self.workspaces[*wi].sessions)),
        };
        (members.len(), running, idle)
    }
}

#[cfg(test)]
mod section_choice_tests {
    use super::*;

    #[test]
    fn member_sections_inherit_from_nearest_manager_and_stop_at_none() {
        use super::super::org_model::{fixtures::{member, owner, snapshot}, OrgSnapshot};
        let snap: OrgSnapshot = serde_json::from_value(snapshot(1, vec![
            owner(), member("lead", "Lead", Some("mem-owner"), None, None),
            member("report", "Report", Some("lead"), None, None),
            member("leaf", "Leaf", Some("report"), None, None),
        ], vec![])).unwrap();
        let sections = vec![SidebarSection { id: "one".into(), ..Default::default() }, SidebarSection { id: "two".into(), ..Default::default() }];
        let mut assignments = HashMap::from([("lead".into(), "one".into())]);
        assert_eq!(member_section(&snap, &assignments, &sections, "leaf").as_deref(), Some("one"));
        assignments.insert("report".into(), "two".into());
        assert_eq!(member_section(&snap, &assignments, &sections, "leaf").as_deref(), Some("two"));
        assignments.insert("report".into(), SECTION_NONE.into());
        assert_eq!(member_section(&snap, &assignments, &sections, "leaf"), None);
        assignments.insert("report".into(), "deleted".into());
        assert_eq!(member_section(&snap, &assignments, &sections, "leaf").as_deref(), Some("one"));
    }

    #[test]
    fn migration_keeps_unambiguous_choices_and_lists_conflicts() {
        let old = HashMap::from([
            ("mains".into(), "a".into()), ("stream".into(), "b".into()),
            ("mixed".into(), "a".into()), ("conflict1".into(), "a".into()),
            ("conflict2".into(), "b".into()), ("existing".into(), "b".into()),
        ]);
        let info = |mains: &[&str], members: &[&str], complete| MigrationWorkspace {
            mains: mains.iter().map(|s| (*s).into()).collect(),
            members: members.iter().map(|s| (*s).into()).collect(), boxed_members: HashSet::new(), complete,
        };
        let evidence = HashMap::from([
            ("mains".into(), info(&["one", "two"], &["one", "two"], true)),
            ("stream".into(), info(&[], &["one"], true)),
            ("mixed".into(), info(&[], &["one", "two"], true)),
            ("conflict1".into(), info(&["three"], &["three"], true)),
            ("conflict2".into(), info(&["three"], &["three"], true)),
            ("existing".into(), info(&["four"], &["four"], true)),
        ]);
        let existing = HashMap::from([("four".into(), "a".into())]);
        let result = migrate_sections(&old, &existing, &evidence);
        assert_eq!(result.members.get("one").map(String::as_str), Some("a"));
        assert_eq!(result.members.get("two").map(String::as_str), Some("a"));
        assert_eq!(result.members.get("four").map(String::as_str), Some("a"));
        assert!(!result.members.contains_key("three"));
        assert_eq!(result.overrides.get("stream").map(String::as_str), Some("b"));
        assert_eq!(result.unresolved.iter().map(|(ws, _, _)| ws.as_str()).collect::<Vec<_>>(),
            ["conflict1", "conflict2", "existing", "mixed"]);

        let mut boxed_evidence = evidence;
        boxed_evidence.get_mut("stream").unwrap().boxed_members.insert("one".into());
        let boxed = migrate_sections(&old, &existing, &boxed_evidence);
        assert_eq!(boxed.overrides.get("stream").map(String::as_str), Some("b"));
        assert!(boxed.unresolved.iter().any(|(ws, choice, reason)|
            ws == "stream" && choice == "b" && reason == "member one: kept in continuous box"));
    }

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
pub(crate) mod layout_tests {
    use super::*;

    pub(crate) fn bare_ws(id: &str) -> Workspace {
        Workspace {
            org: None,
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

    pub(crate) fn task(id: &str, ws: &str, parent: Option<&str>) -> TaskEntry {
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

    pub(crate) fn test_app() -> App {
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
    /// c: is explicitly detached into the loose list. d: loose.
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
        // c's explicit none detaches its block from a, so only a and b are
        // section members.
        assert_eq!(app.section_rollup("s1").0, 2);
    }

    /// The tint test's fixture without nesting: two top-level blocks in s1
    /// (a, b), one in s2 (c), and d loose.
    fn flat_sectioned_app() -> App {
        let mut app = sectioned_app();
        for t in &mut app.tasks {
            t.parent_task_id = None;
        }
        app.workspace_sections.insert("b".into(), "s1".into());
        app
    }

    pub(crate) fn add_task_session(app: &mut App, wi: usize, tid: &str) {
        let session = Session::new("/bin/true", &[], 80, 24, None, HashMap::new(), None).unwrap();
        let mut ts = make_simple_session_with_uid(format!("uid-{tid}"), tid, "codex", session, None);
        ts.task_id = Some(tid.into());
        app.workspaces[wi].sessions.push(ts);
    }

    pub(crate) fn sidebar_buffer(app: &mut App, height: u16) -> ratatui::buffer::Buffer {
        use ratatui::{backend::TestBackend, Terminal};
        app.keybinding_helper_visible = false;
        let mut terminal = Terminal::new(TestBackend::new(40, height)).unwrap();
        terminal.draw(|frame| app.draw_session_list(frame, frame.area())).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn sidebar_tint_covers_section_members_but_not_boundaries_or_loose_rows() {
        let mut app = flat_sectioned_app();
        app.sections[0].color = Some("blue".into());
        app.sections.push(SidebarSection {
            id: "s2".into(), name: "Second".into(), color: Some("blue".into()), folded: false,
        });
        app.workspace_sections.insert("c".into(), "s2".into());
        add_task_session(&mut app, 0, "ta");
        add_task_session(&mut app, 1, "tb");
        app.cursor = Cursor::Workspace(1);
        let buffer = sidebar_buffer(&mut app, 24);
        let blue = theme::sidebar_section_bg(Some("blue"), 2.0);
        // Header, workspace, session, internal gap, second workspace/session.
        // The focused group (b's header and session) keeps the section
        // tint: only its left-edge strip marks it.
        for y in 1..=6 {
            for x in 1..=38 {
                assert_eq!(buffer[(x, y)].bg, blue, "section cell ({x}, {y})");
            }
            assert_eq!(buffer[(0, y)].bg, Color::Reset, "border stays clear");
        }
        assert!(buffer[(4, 5)].modifier.contains(Modifier::BOLD), "selected workspace stays bold");
        for y in [7, 10, 11, 12] {
            assert_eq!(buffer[(38, y)].bg, Color::Reset, "section boundaries and loose rows");
        }
        for y in [7, 10] {
            assert_eq!(buffer[(2, y)].symbol(), "━");
            assert_eq!(buffer[(2, y)].fg, theme::SECTION_BOUNDARY);
            assert!(buffer[(2, y)].modifier.contains(Modifier::BOLD));
        }
        assert_eq!(buffer[(2, 4)].symbol(), "─", "internal workspace divider stays thin");
        assert_eq!(buffer[(2, 4)].fg, theme::DIM);
        assert_eq!(buffer[(38, 8)].bg, blue, "same-colored next section still has a clear boundary");

        app.sections[0].folded = true;
        app.cursor = Cursor::Section("s1".into());
        let buffer = sidebar_buffer(&mut app, 24);
        assert_eq!(buffer[(38, 1)].bg, blue, "folded header keeps tint");
        assert_eq!(buffer[(38, 2)].bg, Color::Reset, "folded group does not bleed");

        app.sidebar_view = SidebarView::Status;
        let buffer = sidebar_buffer(&mut app, 24);
        assert!(buffer.content().iter().all(|cell| cell.bg == Color::Reset));
    }

    #[test]
    fn sidebar_tint_survives_filtered_and_scrolled_away_headings() {
        let mut app = sectioned_app();
        add_task_session(&mut app, 0, "ta");
        add_task_session(&mut app, 1, "tb");
        app.cursor = Cursor::Session(1, 0);
        let neutral = theme::sidebar_section_bg(None, 2.0);
        // The selected session's group keeps its section's tint.
        let lifted = neutral;
        let buffer = sidebar_buffer(&mut app, 5);
        assert!(app.sidebar_list_state.offset() > 0, "section heading scrolled away");
        assert_eq!(buffer[(38, 3)].bg, lifted, "selected inherited session keeps tint");

        app.sidebar_filter = Some("tb".into());
        app.sidebar_list_state = Default::default();
        let buffer = sidebar_buffer(&mut app, 10);
        assert_eq!(buffer[(38, 1)].bg, lifted, "filtered workspace (its task matches) keeps tint without its heading");
        assert_eq!(buffer[(38, 2)].bg, lifted, "filtered session keeps tint without its heading");
        assert_eq!(buffer[(38, 3)].bg, Color::Reset, "empty space stays transparent");
    }

    #[test]
    fn final_sidebar_section_has_a_closing_rule_without_loose_workspaces() {
        let mut app = sectioned_app();
        app.workspaces.truncate(2);
        app.cursor = Cursor::Section("s1".into());
        let buffer = sidebar_buffer(&mut app, 10);
        // Header, a, b (nested under a), then the rule.
        assert_eq!(buffer[(2, 4)].symbol(), "━");
        assert_eq!(buffer[(2, 4)].fg, theme::SECTION_BOUNDARY);
        assert_eq!(buffer[(38, 4)].bg, Color::Reset);
        assert_eq!(buffer[(2, 5)].symbol(), " ");

        app.sections[0].folded = true;
        let buffer = sidebar_buffer(&mut app, 10);
        assert_eq!(buffer[(2, 2)].symbol(), "━", "folded final section also closes");
    }

    #[test]
    fn global_settings_preview_changes_sidebar_and_cancel_restores_saved_tint() {
        use crossterm::event::{Event, KeyEvent};
        let mut app = sectioned_app();
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let original = sidebar_buffer(&mut app, 20)[(38, 1)].bg;
        assert_eq!(original, Color::Rgb(50, 58, 76), "default is twice the original neutral tint");
        assert!(app.handle_event(&key(KeyCode::F(9))));
        app.handle_event(&key(KeyCode::Right));
        assert_ne!(sidebar_buffer(&mut app, 20)[(38, 1)].bg, original);
        for _ in 0..30 { app.handle_event(&key(KeyCode::Left)); }
        let buffer = sidebar_buffer(&mut app, 20);
        assert_eq!(buffer[(38, 1)].bg, Color::Reset, "zero disables tint");
        // Header, a and b, then the rule; c is explicitly loose below it.
        assert_eq!(buffer[(2, 4)].symbol(), "━", "closing rule survives tint off");
        app.handle_event(&key(KeyCode::Esc));
        assert_eq!(sidebar_buffer(&mut app, 20)[(38, 1)].bg, original);
    }

    #[test]
    fn global_settings_opens_above_every_view_and_consumes_modal_keys() {
        use crossterm::event::{Event, KeyEvent};
        let mut app = test_app();
        for (mode, messaging) in [(ViewMode::Sessions, false), (ViewMode::Planning, false),
            (ViewMode::Sessions, true)] {
            app.view_mode = mode.clone();
            app.messages.visible = messaging;
            assert!(app.handle_event(&Event::Key(KeyEvent::new(KeyCode::F(9), KeyModifiers::NONE))));
            assert!(app.is_input_mode());
            app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT)));
            assert_eq!(app.view_mode, mode, "global modal consumes view-switch keys");
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 35)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let text: String = terminal.backend().buffer().content.iter().map(|c| c.symbol()).collect();
            assert!(text.contains("Global Settings"));
            assert!(text.contains("2.00×"));
            app.handle_event(&Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
            assert!(!app.is_input_mode());
            assert_eq!(app.messages.visible, messaging, "cancel preserves underlying view");
        }
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

    /// Laptop diag 2026-11-05: F-Lead (its own host, no explicit section)
    /// runs on its done task, which `self.tasks` drops, so after a viewer
    /// restart its block lost the section it inherits through that task's
    /// parent. The planning snapshot's parent edge carries it.
    #[test]
    fn a_live_session_on_a_done_task_inherits_through_its_planning_parent() {
        let mut app = sectioned_app();
        let mut f = bare_ws("f");
        f.host_id = cm_daemon::host_id::HostId::new("f");
        app.workspaces.push(f);
        add_task_session(&mut app, 4, "t-done");
        assert_eq!(app.section_of_workspace(4), None, "no parent edge known yet");
        // The full planning snapshot (done tasks included) names tb as the
        // parent; b inherits s1 through ta.
        app.sidebar_task_parents.insert("t-done".into(), Some("tb".into()));
        assert_eq!(app.section_of_workspace(4).as_deref(), Some("s1"));
        assert_eq!(layout(&app), ["#s1", "a", "b", "f", "c", "d"]);
        app.workspace_sections.insert("f".into(), SECTION_NONE.into());
        assert_eq!(app.section_of_workspace(4), None, "an explicit choice still wins");
    }

    /// The Section field moved from the task subheader's form to the
    /// workspace's (2026-11-05); a save with the task's fields present
    /// still lands on the workspace the form opened on.
    #[test]
    fn workspace_section_save_with_a_task_survives_cursor_change() {
        let mut app = flat_sectioned_app();
        app.workspace_sections.remove("b");
        app.cursor = Cursor::Workspace(1);
        app.open_session_settings();
        let InputMode::WorkspaceSettings { section, tasks, .. } = &mut app.input_mode else { panic!("workspace settings"); };
        assert_eq!(tasks.iter().map(|t| t.task_id.as_str()).collect::<Vec<_>>(), ["tb"]);
        *section = Some("s1".into());
        tasks[0].name.clear(); // Only the section changes; no API rename is needed.
        app.workspaces.swap(1, 3);
        app.cursor = Cursor::Workspace(0);
        app.handle_input_event(&CrosstermEvent::Key(crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(app.workspace_sections.get("b").map(String::as_str), Some("s1"));
        assert!(!app.workspace_sections.contains_key("d"));
    }

    #[test]
    fn workspace_section_save_survives_reorder() {
        // b top-level: a nested workspace's section is its root's (not saved).
        let mut app = flat_sectioned_app();
        app.workspace_sections.remove("b");
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
        // c explicitly detached into the loose list stays visible.
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
        app.cursor = Cursor::Workspace(0); // a, explicit
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
