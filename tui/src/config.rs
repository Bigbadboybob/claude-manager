use std::collections::HashMap;
use std::env;
use std::fs;

pub struct Config {
    pub api_url: String,
    pub api_token: String,
    pub gcp_project: String,
    pub gcp_zone: String,
    pub repos: HashMap<String, String>,
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

        let mut repos = HashMap::new();
        repos.insert(
            "predictionTrading".to_string(),
            "https://github.com/Bigbadboybob/predictionTrading.git".to_string(),
        );

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
