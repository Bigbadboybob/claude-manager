//! A-n "Reports to": a new workspace whose task reports to an existing task.
//!
//! A-n workspaces are otherwise taskless. With a parent chosen, the form
//! first creates the workspace's task row under that parent (the API
//! inherits its initiative/division), then launches it through the
//! planning-launch path, so the session is task-bound from its first spawn
//! and the reporting line (`parent_task_id`) holds everywhere.
use super::*;
use crate::task_picker::TaskChoice;

/// An A-n submit waiting for its task row.
pub(super) struct PendingReportLaunch {
    pub project: String,
    pub branch: Option<String>,
    pub in_place: bool,
    pub engine: LaunchEngine,
    pub host_id: cm_daemon::host_id::HostId,
    pub parent_task_id: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl App {
    /// Candidate bosses for the A-n picker: open tasks the viewer knows,
    /// live ones first, then the planning board's.
    pub(super) fn reports_to_choices(&self) -> Vec<TaskChoice> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        let live = self.tasks.iter().filter(|t| t.api_status != TaskStatus::Done).filter_map(|t| {
            t.task_id.as_ref().map(|id| TaskChoice { task_id: id.clone(), name: t.name.clone() })
        });
        for choice in live.chain(self.planning.open_task_choices()) {
            if seen.insert(choice.task_id.clone()) {
                out.push(choice);
            }
        }
        out
    }

    pub(super) fn create_reporting_workspace(
        &mut self,
        engine: LaunchEngine,
        repo_url: &str,
        label: &str,
        branch: Option<String>,
        in_place: bool,
        host_id: cm_daemon::host_id::HostId,
        parent: TaskChoice,
        model: Option<String>,
        effort: Option<String>,
    ) {
        let Some(project) = self
            .config
            .repos
            .iter()
            .filter(|(_, url)| url.as_str() == repo_url)
            .map(|(name, _)| name.clone())
            .min()
        else {
            self.set_status_msg(&format!("Reports to: no project is configured for {repo_url}"));
            return;
        };
        let request_id = format!("report-{}", new_session_uid());
        self.pending_report_launches.insert(
            request_id.clone(),
            PendingReportLaunch {
                project: project.clone(),
                branch,
                in_place,
                engine,
                host_id,
                parent_task_id: parent.task_id.clone(),
                model, effort,
            },
        );
        self.backend.create_reporting_task(
            request_id,
            project,
            repo_url.to_string(),
            label.trim().to_string(),
            parent.task_id,
        );
        self.set_status_msg(&format!("Creating '{}' under {}…", label.trim(), parent.name));
    }

    /// The task row for a parked A-n submit exists (or failed): launch it.
    pub(super) fn on_reporting_task_created(
        &mut self,
        request_id: &str,
        result: Result<crate::api::Task, String>,
    ) {
        let Some(pending) = self.pending_report_launches.remove(request_id) else {
            return;
        };
        match result {
            Err(e) => self.set_status_msg(&format!("Reports to: could not create the task: {e}")),
            Ok(task) => {
                let slug = task.slug.clone().unwrap_or_else(|| task.id.clone());
                self.launch_from_plan(
                    &pending.project,
                    &slug,
                    "",
                    pending.branch.as_deref(),
                    false,
                    &task.id,
                    Some(&pending.parent_task_id),
                    pending.in_place,
                    pending.engine.as_session_type(),
                    &pending.host_id,
                    pending.model.as_deref(), pending.effort.as_deref(),
                );
            }
        }
    }
}
