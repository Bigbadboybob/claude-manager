use std::collections::HashMap;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::api::{ApiClient, Task, TaskCreateBody};
use crate::config::Config;


/// Commands sent from the main thread to the background thread.
pub enum BackendCommand {
    Refresh,
    UpdateTask {
        id: String,
        fields: HashMap<String, serde_json::Value>,
    },
    DeleteTask {
        id: String,
    },
    /// Create a planning task in the DB.
    CreatePlanTask {
        project: String,
        repo_url: String,
        name: String,
        description: String,
        status: String,
        /// `Some(parent_id)` → posted as a subtask (parent_task_id flows
        /// to the API row). `None` → top-level task.
        parent_task_id: Option<String>,
        /// Worktree mode for subtasks; persisted on the API row so a
        /// future launch flow can branch correctly. Ignored when
        /// `parent_task_id` is None.
        worktree_mode: Option<String>,
    },
    /// Update a planning task in the DB.
    UpdatePlanTask {
        id: String,
        fields: HashMap<String, serde_json::Value>,
    },
    /// Delete a planning task from the DB.
    DeletePlanTask {
        id: String,
    },
    /// Fetch all planning tasks (tasks that have a project set).
    RefreshPlanTasks,
    Shutdown,
}

/// Events sent from the background thread to the main thread.
pub enum BackendEvent {
    TasksUpdated(Vec<Task>),
    ApiError(String),
    Connected,
    Disconnected,
    /// Progress message for multi-step operations.
    Progress(String),
    /// Planning tasks updated (all tasks with a project field).
    PlanTasksUpdated(Vec<Task>),
    /// A single planning task was updated — merge into local state.
    PlanTaskUpdated(Task),
    /// A planning task was created — return the full task.
    PlanTaskCreated(Task),
    /// A planning task was deleted.
    PlanTaskDeleted(String),
}

/// Handle to the background polling thread.
pub struct BackendHandle {
    pub cmd_tx: mpsc::Sender<BackendCommand>,
    pub event_rx: mpsc::Receiver<BackendEvent>,
    thread: Option<JoinHandle<()>>,
}

impl BackendHandle {
    pub fn spawn(config: &Config) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
        let (event_tx, event_rx) = mpsc::channel::<BackendEvent>();

        let client = ApiClient::new(config);
        let gcp_project = config.gcp_project.clone();
        let gcp_zone = config.gcp_zone.clone();

        let thread = thread::spawn(move || {
            backend_loop(client, cmd_rx, event_tx, &gcp_project, &gcp_zone);
        });

        BackendHandle {
            cmd_tx,
            event_rx,
            thread: Some(thread),
        }
    }

    /// Send a command to the backend, logging any SendError to stderr.
    /// `ctx` identifies the operation in the log line so a dropped
    /// command surfaces as more than a silent no-op.
    fn try_send_or_warn(&self, cmd: BackendCommand, ctx: &str) {
        if let Err(e) = self.cmd_tx.send(cmd) {
            eprintln!("backend send dropped ({}): {}", ctx, e);
        }
    }

    pub fn refresh(&self) {
        self.try_send_or_warn(BackendCommand::Refresh, "refresh");
    }

    pub fn update_task(&self, id: String, fields: HashMap<String, serde_json::Value>) {
        self.try_send_or_warn(BackendCommand::UpdateTask { id, fields }, "update_task");
    }

    pub fn delete_task(&self, id: String) {
        self.try_send_or_warn(BackendCommand::DeleteTask { id }, "delete_task");
    }

    pub fn create_plan_task(
        &self,
        project: String,
        repo_url: String,
        name: String,
        description: String,
        status: String,
        parent_task_id: Option<String>,
        worktree_mode: Option<String>,
    ) {
        self.try_send_or_warn(
            BackendCommand::CreatePlanTask {
                project,
                repo_url,
                name,
                description,
                status,
                parent_task_id,
                worktree_mode,
            },
            "create_plan_task",
        );
    }

    pub fn update_plan_task(&self, id: String, fields: HashMap<String, serde_json::Value>) {
        self.try_send_or_warn(
            BackendCommand::UpdatePlanTask { id, fields },
            "update_plan_task",
        );
    }

    pub fn delete_plan_task(&self, id: String) {
        self.try_send_or_warn(BackendCommand::DeletePlanTask { id }, "delete_plan_task");
    }

    pub fn refresh_plan_tasks(&self) {
        self.try_send_or_warn(BackendCommand::RefreshPlanTasks, "refresh_plan_tasks");
    }

    pub fn shutdown(&mut self) {
        // SendError here is expected if the receiver thread already
        // exited (clean shutdown race); skip the warn helper.
        let _ = self.cmd_tx.send(BackendCommand::Shutdown);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for BackendHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn backend_loop(
    client: ApiClient,
    cmd_rx: mpsc::Receiver<BackendCommand>,
    event_tx: mpsc::Sender<BackendEvent>,
    _gcp_project: &str,
    _gcp_zone: &str,
) {
    let mut was_connected = false;

    do_refresh(&client, &event_tx, &mut was_connected);

    loop {
        match cmd_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(BackendCommand::Shutdown) => break,
            Ok(BackendCommand::Refresh) => {
                do_refresh(&client, &event_tx, &mut was_connected);
            }
            Ok(BackendCommand::UpdateTask { id, fields }) => {
                match client.update_task(&id, &fields) {
                    Ok(_) => do_refresh(&client, &event_tx, &mut was_connected),
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::DeleteTask { id }) => {
                match client.delete_task(&id) {
                    Ok(_) => do_refresh(&client, &event_tx, &mut was_connected),
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::CreatePlanTask {
                project,
                repo_url,
                name,
                description,
                status,
                parent_task_id,
                worktree_mode,
            }) => {
                do_create_plan_task(
                    &client,
                    &event_tx,
                    &project,
                    &repo_url,
                    &name,
                    &description,
                    &status,
                    parent_task_id.as_deref(),
                    worktree_mode.as_deref(),
                );
            }
            Ok(BackendCommand::UpdatePlanTask { id, fields }) => {
                match client.update_task(&id, &fields) {
                    Ok(task) => {
                        let _ = event_tx.send(BackendEvent::PlanTaskUpdated(task));
                        do_refresh(&client, &event_tx, &mut was_connected);
                    }
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::DeletePlanTask { id }) => {
                match client.delete_task(&id) {
                    Ok(_) => {
                        let _ = event_tx.send(BackendEvent::PlanTaskDeleted(id));
                        do_refresh_plan_tasks(&client, &event_tx);
                    }
                    Err(e) => {
                        let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
                    }
                }
            }
            Ok(BackendCommand::RefreshPlanTasks) => {
                do_refresh_plan_tasks(&client, &event_tx);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                do_refresh(&client, &event_tx, &mut was_connected);
                do_refresh_plan_tasks(&client, &event_tx);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn do_refresh(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    was_connected: &mut bool,
) {
    match client.list_tasks(None) {
        Ok(tasks) => {
            if !*was_connected {
                let _ = event_tx.send(BackendEvent::Connected);
                *was_connected = true;
            }
            let _ = event_tx.send(BackendEvent::TasksUpdated(tasks));
        }
        Err(e) => {
            if *was_connected {
                let _ = event_tx.send(BackendEvent::Disconnected);
                *was_connected = false;
            }
            let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
        }
    }
}

/// Get the Claude project path for a directory.
/// Claude encodes: '/' and '.' both become '-', leading dash kept.
/// Create a planning task, preserving parent binding and worktree intent.
fn do_create_plan_task(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    project: &str,
    repo_url: &str,
    name: &str,
    description: &str,
    status: &str,
    parent_task_id: Option<&str>,
    worktree_mode: Option<&str>,
) {
    let body = TaskCreateBody {
        repo_url: repo_url.to_string(),
        repo_branch: "main".to_string(),
        name: Some(name.to_string()),
        prompt: None,
        priority: 0,
        status: Some(status.to_string()),
        project: Some(project.to_string()),
        slug: None, // auto-generated by API
        description: Some(description.to_string()),
        difficulty: None,
        depends: None,
        source: None,
        is_cloud: Some(false),
        parent_task_id: parent_task_id.map(str::to_string),
        worktree_mode: worktree_mode.map(str::to_string),
        initiative_id: None,
        wip_branch: None,
        metadata: None,
    };

    match client.create_task(&body) {
        Ok(task) => {
            let _ = event_tx.send(BackendEvent::PlanTaskCreated(task));
        }
        Err(e) => {
            let _ = event_tx.send(BackendEvent::ApiError(format!(
                "Create plan task: {}",
                e
            )));
        }
    }
}

/// Refresh planning tasks (all tasks that have a project field).
fn do_refresh_plan_tasks(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
) {
    // Fetch all tasks and filter to those with a project.
    // The API doesn't have a "has project" filter, so we fetch all and filter client-side.
    match client.list_tasks(None) {
        Ok(tasks) => {
            let plan_tasks: Vec<Task> = tasks
                .into_iter()
                .filter(|t| t.project.is_some())
                .collect();
            let _ = event_tx.send(BackendEvent::PlanTasksUpdated(plan_tasks));
        }
        Err(e) => {
            let _ = event_tx.send(BackendEvent::ApiError(e.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_send_or_warn_does_not_panic_when_receiver_dropped() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<BackendCommand>();
        let (_event_tx, event_rx) = mpsc::channel::<BackendEvent>();
        drop(cmd_rx);
        let handle = BackendHandle {
            cmd_tx,
            event_rx,
            thread: None,
        };
        handle.refresh();
        handle.delete_task("nope".to_string());
        handle.refresh_plan_tasks();
    }
}
