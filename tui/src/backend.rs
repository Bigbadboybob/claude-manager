use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::api::{ApiClient, Task, TaskCreateBody};
use crate::config::Config;

const GCS_BUCKET: &str = "gs://cm-sessions";

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
    /// Push a local session to the cloud.
    Push {
        worktree_path: PathBuf,
        repo_url: String,
        name: String,
        task_id: Option<String>,
        /// Echoed back on `PushComplete` / `PushFailed` so the main thread
        /// can locate the originating workspace without guessing.
        workspace_id: String,
    },
    /// Pull a cloud session to local.
    Pull {
        task_id: String,
        main_repo: PathBuf,
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
    /// Pull completed — main thread should spawn a local claude --resume session.
    PullComplete {
        task_id: String,
        worktree_path: PathBuf,
        main_repo: PathBuf,
        session_id: String,
        repo_url: String,
        prompt: String,
    },
    /// Push completed successfully. The main thread gates its
    /// destructive local cleanup (tombstone sessions, clear
    /// `worktree_path`, flip `is_cloud`) on this event so a failed push
    /// doesn't leave the UI claiming a non-existent cloud workspace.
    /// `task_id` is the cloud task that was updated/created — `None`
    /// only if the API create failed mid-flow (callers should treat
    /// that as a partial success and surface a status message rather
    /// than re-pushing).
    PushComplete {
        workspace_id: String,
        task_id: Option<String>,
    },
    /// Push failed at any stage (git push, GCS upload, API call). The
    /// main thread should clear the workspace's `is_pushing` flag and
    /// surface `error` to the user; local state stays intact so retry
    /// is a no-op away.
    PushFailed {
        workspace_id: String,
        error: String,
    },
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

    pub fn push(
        &self,
        worktree_path: PathBuf,
        repo_url: String,
        name: String,
        task_id: Option<String>,
        workspace_id: String,
    ) {
        self.try_send_or_warn(
            BackendCommand::Push {
                worktree_path,
                repo_url,
                name,
                task_id,
                workspace_id,
            },
            "push",
        );
    }

    pub fn pull(&self, task_id: String, main_repo: PathBuf) {
        self.try_send_or_warn(BackendCommand::Pull { task_id, main_repo }, "pull");
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
    gcp_project: &str,
    gcp_zone: &str,
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
            Ok(BackendCommand::Push {
                worktree_path,
                repo_url,
                name,
                task_id,
                workspace_id,
            }) => {
                do_push(
                    &client,
                    &event_tx,
                    &worktree_path,
                    &repo_url,
                    &name,
                    task_id.as_deref(),
                    &workspace_id,
                );
                do_refresh(&client, &event_tx, &mut was_connected);
            }
            Ok(BackendCommand::Pull { task_id, main_repo }) => {
                do_pull(
                    &client,
                    &event_tx,
                    &task_id,
                    &main_repo,
                    gcp_project,
                    gcp_zone,
                );
                do_refresh(&client, &event_tx, &mut was_connected);
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
fn get_project_path(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .replace('/', "-")
        .replace('.', "-")
}

/// Find the most recent Claude session JSONL for a working directory.
fn find_latest_session(cwd: &Path) -> Option<(String, PathBuf)> {
    let project_path = get_project_path(cwd);
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let project_dir = home.join(".claude/projects").join(&project_path);
    if !project_dir.exists() {
        return None;
    }

    let mut jsonl_files: Vec<_> = std::fs::read_dir(&project_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map_or(false, |ext| ext == "jsonl")
        })
        .collect();

    jsonl_files.sort_by(|a, b| {
        let ma = a.metadata().and_then(|m| m.modified()).ok();
        let mb = b.metadata().and_then(|m| m.modified()).ok();
        mb.cmp(&ma)
    });

    let entry = jsonl_files.first()?;
    let session_id = entry.path().file_stem()?.to_string_lossy().to_string();
    Some((session_id, entry.path()))
}

/// Push a local session to the cloud.
///
/// Terminates with exactly one of `PushComplete` / `PushFailed` so the
/// main thread can gate destructive local cleanup on a real success.
/// `Progress` events are advisory only — they used to be the *only*
/// signal, which was the bug: a status string saying "Pushed" left the
/// UI tombstoning sessions even when git/gcloud/API had silently
/// failed earlier in the flow.
fn do_push(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    worktree_path: &Path,
    repo_url: &str,
    name: &str,
    task_id: Option<&str>,
    workspace_id: &str,
) {
    let progress = |msg: &str| {
        let _ = event_tx.send(BackendEvent::Progress(msg.to_string()));
    };
    let fail = |error: String| {
        let _ = event_tx.send(BackendEvent::PushFailed {
            workspace_id: workspace_id.to_string(),
            error,
        });
    };

    // 1. Find session file.
    let (session_id, jsonl_path) = match find_latest_session(worktree_path) {
        Some(s) => s,
        None => {
            fail("no Claude session found".to_string());
            return;
        }
    };

    progress("Committing WIP...");

    // 2. Git commit + push.
    let branch = format!("cm/push-{}", &session_id[..8.min(session_id.len())]);
    let cwd = worktree_path;

    let _ = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["checkout", "-b", &branch])
        .output();

    let _ = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["add", "-A"])
        .output();

    let _ = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["commit", "-m", &format!("WIP: {}", name)])
        .output();

    let push_result = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["push", "-u", "origin", &branch])
        .output();

    match &push_result {
        Ok(r) if !r.status.success() => {
            let stderr = String::from_utf8_lossy(&r.stderr);
            fail(format!("git push: {}", stderr.trim()));
            return;
        }
        Err(e) => {
            fail(format!("git push: {}", e));
            return;
        }
        _ => {}
    }

    progress("Uploading session...");

    // 3. Upload session to GCS.
    let gcs_path = format!("{}/{}/{}.jsonl", GCS_BUCKET, session_id, session_id);
    let gcs_result = Command::new("gcloud")
        .args(["storage", "cp"])
        .arg(&jsonl_path)
        .arg(&gcs_path)
        .output();

    match &gcs_result {
        Ok(r) if !r.status.success() => {
            let stderr = String::from_utf8_lossy(&r.stderr);
            fail(format!("gcs upload: {}", stderr.trim()));
            return;
        }
        Err(e) => {
            fail(format!("gcs upload: {}", e));
            return;
        }
        _ => {}
    }

    // Upload subagent files if they exist. Best-effort: a missing
    // subdir or a transient gcloud failure here doesn't invalidate
    // the main session file, so we stay non-fatal.
    let subdir = jsonl_path.parent().unwrap().join(&session_id);
    if subdir.exists() {
        let _ = Command::new("gcloud")
            .args(["storage", "cp", "-r"])
            .arg(&subdir)
            .arg(&format!("{}/{}/", GCS_BUCKET, session_id))
            .output();
    }

    // 4. Create or update task via API.
    let mut fields = HashMap::new();
    fields.insert(
        "session_id".to_string(),
        serde_json::Value::String(session_id),
    );
    fields.insert(
        "wip_branch".to_string(),
        serde_json::Value::String(branch.clone()),
    );
    fields.insert(
        "repo_branch".to_string(),
        serde_json::Value::String(branch),
    );
    if let Some(id) = task_id {
        progress("Updating cloud task...");
        match client.update_task(id, &fields) {
            Ok(_) => {
                progress(&format!("Pushed to cloud: {}", &id[..8.min(id.len())]));
                let _ = event_tx.send(BackendEvent::PushComplete {
                    workspace_id: workspace_id.to_string(),
                    task_id: Some(id.to_string()),
                });
            }
            Err(e) => fail(format!("API update: {}", e)),
        }
    } else {
        progress("Creating cloud task...");
        let body = TaskCreateBody {
            repo_url: repo_url.to_string(),
            repo_branch: "main".to_string(),
            name: Some(name.to_string()),
            prompt: None,
            priority: 0,
            status: None,
            project: None,
            slug: None,
            description: None,
            difficulty: None,
            depends: None,
            source: None,
            is_cloud: Some(true),
            parent_task_id: None,
            worktree_mode: None,
            wip_branch: None,
        };
        match client.create_task(&body) {
            Ok(task) => {
                // Second PATCH writes session_id/wip_branch/repo_branch
                // onto the freshly-created row. If it fails, the cloud
                // task exists but is missing the fields a `pull` would
                // need — same "UI lies" failure mode as the other
                // failure paths, so treat it as PushFailed rather than
                // tombstoning the local workspace.
                match client.update_task(&task.id, &fields) {
                    Ok(_) => {
                        progress(&format!("Pushed to cloud: {}", &task.id[..8]));
                        let _ = event_tx.send(BackendEvent::PushComplete {
                            workspace_id: workspace_id.to_string(),
                            task_id: Some(task.id),
                        });
                    }
                    Err(e) => fail(format!("API update: {}", e)),
                }
            }
            Err(e) => fail(format!("API: {}", e)),
        }
    }
}

/// Pull a cloud session to local.
fn do_pull(
    client: &ApiClient,
    event_tx: &mpsc::Sender<BackendEvent>,
    task_id: &str,
    main_repo: &Path,
    gcp_project: &str,
    gcp_zone: &str,
) {
    let progress = |msg: &str| {
        let _ = event_tx.send(BackendEvent::Progress(msg.to_string()));
    };

    // 1. Fetch task.
    let task = match client.get_task(task_id) {
        Ok(t) => t,
        Err(e) => {
            progress(&format!("Pull failed: {}", e));
            return;
        }
    };

    let session_id = match &task.session_id {
        Some(s) if !s.is_empty() => s.clone(),
        _ => {
            progress("Pull failed: no session on this task");
            return;
        }
    };

    let task_label = task.name.as_deref().or(task.prompt.as_deref()).unwrap_or("task");
    let slug = cm_daemon::worktree::slugify(task_label);
    let slug = if slug.is_empty() {
        session_id[..8.min(session_id.len())].to_string()
    } else {
        slug
    };

    // 2. Create worktree on the WIP branch.
    progress("Creating worktree...");
    let branch = task.wip_branch.as_deref().unwrap_or("main");

    // Fetch the branch first.
    let _ = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["fetch", "origin", branch])
        .output();

    let worktree_path = match cm_daemon::worktree::create_worktree(main_repo, &slug, Some(branch)) {
        Ok(p) => p,
        Err(e) => {
            progress(&format!("Pull failed: worktree: {}", e));
            return;
        }
    };

    // Checkout the WIP branch in the worktree (may already be on it).
    let _ = Command::new("git")
        .arg("-C")
        .arg(&worktree_path)
        .args(["checkout", branch])
        .output();

    cm_daemon::worktree::setup_worktree(main_repo, &worktree_path);

    // 3. Download session from GCS.
    progress("Downloading session...");
    let project_path = get_project_path(&worktree_path);
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let local_project_dir = home.join(".claude/projects").join(&project_path);
    let _ = std::fs::create_dir_all(&local_project_dir);
    let local_jsonl = local_project_dir.join(format!("{}.jsonl", session_id));

    let gcs_path = format!("{}/{}/{}.jsonl", GCS_BUCKET, session_id, session_id);
    let result = Command::new("gcloud")
        .args(["storage", "cp", &gcs_path])
        .arg(&local_jsonl)
        .output();

    let gcs_ok = result.map_or(false, |r| r.status.success());

    if !gcs_ok {
        // Fallback: SSH into VM.
        if let Some(ref vm) = task.worker_vm {
            progress("Pulling from VM...");
            let zone = task.worker_zone.as_deref().unwrap_or(gcp_zone);
            let ssh_cmd = format!(
                "sudo cat /home/worker/.claude/projects/-workspace/{}.jsonl",
                session_id
            );
            let result = Command::new("gcloud")
                .args([
                    "compute", "ssh", vm,
                    &format!("--zone={}", zone),
                    &format!("--project={}", gcp_project),
                    "--command", &ssh_cmd,
                ])
                .output();

            match result {
                Ok(r) if r.status.success() => {
                    let content: String = String::from_utf8_lossy(&r.stdout)
                        .lines()
                        .filter(|l| !l.starts_with("Pseudo-terminal"))
                        .collect::<Vec<_>>()
                        .join("\n");
                    let _ = std::fs::write(&local_jsonl, content);
                }
                _ => {
                    progress("Pull failed: could not download session");
                    return;
                }
            }
        } else {
            progress("Pull failed: not in GCS and no VM to pull from");
            return;
        }
    }

    // Download subagent files (best effort).
    let _ = Command::new("gcloud")
        .args(["storage", "cp", "-r"])
        .arg(&format!("{}/{}/{}/", GCS_BUCKET, session_id, session_id))
        .arg(&format!("{}/{}/", local_project_dir.display(), session_id))
        .output();

    // 4. Mark cloud task as done.
    if task.status == "running" || task.status == "blocked" {
        let mut fields = HashMap::new();
        fields.insert(
            "status".to_string(),
            serde_json::Value::String("done".to_string()),
        );
        let _ = client.update_task(task_id, &fields);
    }

    progress("Pull complete — resuming locally");

    // 5. Tell the main thread to spawn a local session.
    let _ = event_tx.send(BackendEvent::PullComplete {
        task_id: task_id.to_string(),
        worktree_path,
        main_repo: main_repo.to_path_buf(),
        session_id,
        repo_url: task.repo_url,
        prompt: task.name.or(task.prompt).unwrap_or_default(),
    });
}

/// Create a planning task in the DB. `parent_task_id` and `worktree_mode`
/// flow through to the API row so subtasks land with their parent link
/// and worktree intent persisted from creation. Top-level tasks pass
/// `None` for both and the API treats them as ordinary planning rows.
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
        wip_branch: None,
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
