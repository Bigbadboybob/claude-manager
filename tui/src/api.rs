use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::Config;

/// Task as returned by the API.
#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub repo_url: String,
    pub repo_branch: String,
    pub prompt: String,
    pub status: String,
    pub priority: i32,
    pub worker_vm: Option<String>,
    pub worker_zone: Option<String>,
    pub ttyd_url: Option<String>,
    pub blocked_at: Option<String>,
    pub session_id: Option<String>,
    pub wip_branch: Option<String>,
}

/// Body for creating a task.
#[derive(Serialize)]
pub struct TaskCreateBody {
    pub repo_url: String,
    pub repo_branch: String,
    pub prompt: String,
    pub priority: i32,
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
                .timeout_global(Some(std::time::Duration::from_secs(10)))
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

    pub fn health(&self) -> anyhow::Result<()> {
        self.agent
            .get(&self.url("/health"))
            .header("Authorization", &self.auth_header())
            .call()?;
        Ok(())
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
