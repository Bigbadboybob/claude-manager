//! Build the spawn arguments and per-session MCP configuration for a
//! workflow-participant session (Claude Code or Codex).
//!
//! The TUI's existing spawn paths (`Session::new`) are unchanged. This module
//! just produces the right CLI args so the spawned agent has the workflow MCP
//! tools available with the right env vars for its role.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::workflow::toml_schema::Engine;

/// Find the absolute path to `mcp_server/server.py` alongside the workflows dir.
///
/// Resolution:
///   1. `$CM_MCP_SERVER` if set and the file exists
///   2. `<workflows_dir>/../mcp_server/server.py`
///   3. `./mcp_server/server.py` relative to cwd
pub fn mcp_server_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CM_MCP_SERVER") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    let wf_dir = crate::workflow::toml_schema::workflows_dir();
    if let Some(parent) = wf_dir.parent() {
        let candidate = parent.join("mcp_server").join("server.py");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let cwd_candidate = std::env::current_dir()
        .ok()
        .map(|p| p.join("mcp_server").join("server.py"));
    if let Some(p) = cwd_candidate {
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

fn mcp_config_dir(run_id: &str) -> PathBuf {
    crate::workflow::run::run_dir(run_id).join("mcp-configs")
}

/// Write a Claude-compatible MCP config JSON for `(run_id, role)`.
/// Returns the path we wrote, which should be passed via `--mcp-config`.
pub fn write_claude_mcp_config(run_id: &str, role: &str) -> std::io::Result<PathBuf> {
    let server = mcp_server_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not locate mcp_server/server.py",
        )
    })?;
    let dir = mcp_config_dir(run_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}-claude.json", role));
    let config = json!({
        "mcpServers": {
            "claude-manager": {
                "command": "python",
                "args": [server.to_string_lossy()],
                "env": {
                    "CM_WORKFLOW_RUN_ID": run_id,
                    "CM_ROLE": role,
                }
            }
        }
    });
    fs::write(&path, serde_json::to_string_pretty(&config).unwrap_or_default())?;
    Ok(path)
}

/// Build the full argv (after the `claude` program name) for a workflow-participant
/// Claude Code session.
///
/// `resume_session_id`: pass Some(id) for a persistent-context role that should resume,
/// None for a fresh-context role (or a brand-new session).
pub fn claude_args(
    mcp_config_path: &Path,
    resume_session_id: Option<&str>,
    extra: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push("--dangerously-skip-permissions".to_string());
    args.push("--mcp-config".to_string());
    args.push(mcp_config_path.to_string_lossy().to_string());
    if let Some(sid) = resume_session_id {
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }
    for e in extra {
        args.push(e.clone());
    }
    args
}

/// Build the full argv for a workflow-participant Codex session.
///
/// Uses `-c` overrides to register our workflow MCP server alongside whatever
/// MCP servers the user has configured globally. No isolated CODEX_HOME —
/// codex uses the user's `~/.codex/config.toml` as usual so its trusted-
/// projects list, auth, and settings carry over.
pub fn codex_args(run_id: &str, role: &str, _resume_session_id: Option<&str>) -> Vec<String> {
    let server = mcp_server_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut args: Vec<String> = Vec::new();
    args.push("--dangerously-bypass-approvals-and-sandbox".into());
    args.push("-c".into());
    args.push(r#"mcp_servers.claude-manager.command="python""#.into());
    args.push("-c".into());
    args.push(format!(r#"mcp_servers.claude-manager.args=["{}"]"#, server));
    args.push("-c".into());
    args.push(format!(
        r#"mcp_servers.claude-manager.env={{CM_WORKFLOW_RUN_ID="{}",CM_ROLE="{}"}}"#,
        run_id, role
    ));
    args
}

/// Build (program, argv, env) for an engine, dispatching on type.
///
/// `env` is any extra environment variables that should be passed to the
/// child process — currently only Codex uses this (for `CODEX_HOME`).
pub fn build_args(
    engine: &Engine,
    run_id: &str,
    role: &str,
    resume_session_id: Option<&str>,
) -> std::io::Result<(String, Vec<String>)> {
    match engine {
        Engine::ClaudeCode => {
            let cfg = write_claude_mcp_config(run_id, role)?;
            let args = claude_args(&cfg, resume_session_id, &[]);
            Ok(("claude".to_string(), args))
        }
        Engine::Codex => {
            let args = codex_args(run_id, role, resume_session_id);
            Ok(("codex".to_string(), args))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_args_include_mcp_config() {
        let args = claude_args(Path::new("/tmp/x.json"), None, &[]);
        assert!(args.contains(&"--mcp-config".to_string()));
        assert!(args.contains(&"/tmp/x.json".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    #[test]
    fn claude_args_include_resume() {
        let args = claude_args(Path::new("/tmp/x.json"), Some("sid-123"), &[]);
        assert!(args.windows(2).any(|w| w[0] == "--resume" && w[1] == "sid-123"));
    }

    #[test]
    fn codex_args_bypass_trust_prompt() {
        let args = codex_args("wf_abc", "worker", None);
        assert!(
            args.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"),
            "codex_args must bypass approvals + sandbox"
        );
    }

    #[test]
    fn codex_args_register_mcp_via_overrides() {
        // Workflow MCP registered via -c overrides alongside user's global
        // config; env carries run_id/role for the tool handlers.
        let args = codex_args("wf_abc", "worker", None);
        let c_count = args.iter().filter(|a| *a == "-c").count();
        assert!(c_count >= 3);
        assert!(args.iter().any(|a| a.contains("wf_abc")));
        assert!(args.iter().any(|a| a.contains(r#"CM_ROLE="worker""#)));
    }
}
