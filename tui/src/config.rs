use std::collections::HashMap;
use std::env;
use std::fs;

use crate::memory_cap::parse_bytes;

pub struct Config {
    pub api_url: String,
    pub api_token: String,
    pub gcp_project: String,
    pub gcp_zone: String,
    pub repos: HashMap<String, String>,
}

impl Config {
    /// Per-session memory limits keyed by `session_type`. Returns
    /// `Some((soft_bytes, hard_bytes))` only when *both*
    /// `CM_SESSION_MEM_SOFT_<TYPE>` and `CM_SESSION_MEM_HARD_<TYPE>`
    /// are set to non-empty, parseable values. Unset, empty, or
    /// partially-set → `None` (caps off for this type). See
    /// DESIGN_MEMORY_CAP.md § Configuration.
    pub fn memory_cap_for(&self, session_type: &str) -> Option<(u64, u64)> {
        // gcloud and other infra sessions are never capped.
        let upper = session_type.to_uppercase();
        let soft_var = format!("CM_SESSION_MEM_SOFT_{}", upper);
        let hard_var = format!("CM_SESSION_MEM_HARD_{}", upper);
        let soft = env::var(&soft_var).ok().filter(|s| !s.trim().is_empty())?;
        let hard = env::var(&hard_var).ok().filter(|s| !s.trim().is_empty())?;
        let soft_bytes = parse_bytes(&soft)?;
        let hard_bytes = parse_bytes(&hard)?;
        // Sanity: hard >= soft, otherwise treat as misconfigured and disable.
        if hard_bytes < soft_bytes {
            return None;
        }
        Some((soft_bytes, hard_bytes))
    }
}

impl Config {
    pub fn load() -> Self {
        // Load .env file (same logic as dispatch/config.py).
        let env_file = dirs::home_dir()
            .unwrap_or_default()
            .join(".config/claude-manager/.env");

        if let Ok(contents) = fs::read_to_string(&env_file) {
            for line in contents.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    let key = key.trim();
                    let value = value.trim();
                    // setdefault: only set if not already in env.
                    if env::var(key).is_err() {
                        env::set_var(key, value);
                    }
                }
            }
        }

        // Discover repos from ~/.cm/projects/*/repo_url files.
        let mut repos = HashMap::new();
        let projects_dir = dirs::home_dir()
            .unwrap_or_default()
            .join(".cm/projects");
        if let Ok(entries) = fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let url_file = path.join("repo_url");
                    if let Ok(url) = fs::read_to_string(&url_file) {
                        let url = url.trim().to_string();
                        if !url.is_empty() {
                            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                repos.insert(name.to_string(), url);
                            }
                        }
                    }
                }
            }
        }

        Config {
            api_url: env::var("CM_API_URL").unwrap_or_else(|_| "http://localhost:8000".into()),
            api_token: env::var("CM_API_TOKEN").unwrap_or_else(|_| "dev-token".into()),
            gcp_project: env::var("CM_GCP_PROJECT")
                .unwrap_or_else(|_| "claude-manager-prod".into()),
            gcp_zone: env::var("CM_GCP_ZONE").unwrap_or_else(|_| "us-east4-a".into()),
            repos,
        }
    }
}

/// Get the user's home directory.
mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}
