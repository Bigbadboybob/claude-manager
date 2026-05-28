use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Task as returned by the API. `serde` ignores unknown fields by default, so
/// dropping fields here just means we stop deserializing them — the API
/// schema can keep returning them.
#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub id: String,
    pub created_at: String,
    pub repo_url: String,
    pub repo_branch: String,
    pub name: Option<String>,
    pub prompt: Option<String>,
    pub status: String,
    pub worker_vm: Option<String>,
    pub worker_zone: Option<String>,
    pub blocked_at: Option<String>,
    pub session_id: Option<String>,
    pub wip_branch: Option<String>,
    // Planning fields
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub difficulty: Option<i32>,
    #[serde(default)]
    pub depends: Option<Vec<String>>,
    #[serde(default = "default_source")]
    pub source: String,
    #[serde(default)]
    pub is_cloud: bool,
    /// FK to another task. Null for top-level tasks. Phase 5 subtask field.
    #[serde(default)]
    pub parent_task_id: Option<String>,
    /// "inherit" (default) or "branch". Only meaningful when
    /// `parent_task_id` is set. Phase 5 subtask field.
    #[serde(default = "default_worktree_mode")]
    pub worktree_mode: String,
    /// Free-form JSONB bag. Skills attach structured context here (e.g.
    /// `metadata.resume.design_doc_path` for the design-doc bundle) so
    /// the schema doesn't churn for every new shape. None = no bag.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

fn default_worktree_mode() -> String {
    "inherit".to_string()
}

fn default_source() -> String {
    "user".to_string()
}

/// Body for creating a task.
#[derive(Serialize)]
pub struct TaskCreateBody {
    pub repo_url: String,
    pub repo_branch: String,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    // Planning fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub difficulty: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_cloud: Option<bool>,
    // Subtask fields (Phase 5).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_mode: Option<String>,
    /// `wip_branch` at create time. Inherit-mode subtasks need this
    /// to persist their parent's branch through reconcile (without
    /// it the API row's `wip_branch` is NULL and the next reconcile
    /// blanks the local copy, so a branch-mode grandchild would
    /// fall back to "main" as its start ref).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wip_branch: Option<String>,
}

/// Blocking HTTP client for the Claude Manager API.
pub struct ApiClient {
    base_url: String,
    token: String,
    agent: ureq::Agent,
}

impl ApiClient {
    pub fn new(config: &Config) -> Self {
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .timeout_global(Some(std::time::Duration::from_secs(5)))
                .build(),
        );
        ApiClient {
            base_url: config.api_url.trim_end_matches('/').to_string(),
            token: config.api_token.clone(),
            agent,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token)
    }

    pub fn list_tasks(&self, status: Option<&str>) -> anyhow::Result<Vec<Task>> {
        let url = match status {
            Some(s) => format!("{}?status={}", self.url("/tasks"), s),
            None => self.url("/tasks"),
        };
        let body = self
            .agent
            .get(&url)
            .header("Authorization", &self.auth_header())
            .call()?
            .body_mut()
            .read_json::<Vec<Task>>()?;
        Ok(body)
    }

    pub fn get_task(&self, task_id: &str) -> anyhow::Result<Task> {
        let body = self
            .agent
            .get(&self.url(&format!("/tasks/{}", task_id)))
            .header("Authorization", &self.auth_header())
            .call()?
            .body_mut()
            .read_json::<Task>()?;
        Ok(body)
    }

    pub fn create_task(&self, body: &TaskCreateBody) -> anyhow::Result<Task> {
        let resp = self
            .agent
            .post(&self.url("/tasks"))
            .header("Authorization", &self.auth_header())
            .send_json(body)?
            .body_mut()
            .read_json::<Task>()?;
        Ok(resp)
    }

    pub fn update_task(
        &self,
        task_id: &str,
        fields: &HashMap<String, serde_json::Value>,
    ) -> anyhow::Result<Task> {
        let resp = self
            .agent
            .patch(&self.url(&format!("/tasks/{}", task_id)))
            .header("Authorization", &self.auth_header())
            .send_json(fields)?
            .body_mut()
            .read_json::<Task>()?;
        Ok(resp)
    }

    pub fn delete_task(&self, task_id: &str) -> anyhow::Result<()> {
        self.agent
            .delete(&self.url(&format!("/tasks/{}", task_id)))
            .header("Authorization", &self.auth_header())
            .call()?;
        Ok(())
    }
}
