//! Shared MCP config + args builder used by every agent spawn path.
//! Generalizes what `workflow/spawn.rs` previously did just for workflow
//! participants so regular `A-n` and planning `A-l` sessions also
//! register the `claude-manager` MCP server — and through it can reach
//! the TUI's control socket.
//!
//! Per-session config goes under `~/.cm/mcp/<session_uid>/claude.json`.
//! The env block carries:
//!   - `CM_TUI_SESSION_ID`  — the calling session's stable UID. The
//!     server uses this to attribute every tool call. Forgeable from
//!     inside the agent (it can read its own env), but we treat it as
//!     a soft capability token, not a security boundary; see the
//!     "Authorization model" section of `AGENT_ORCHESTRATION.md`.
//!   - `CM_TUI_SOCKET`      — path to `~/.cm/tui.sock` (override-able
//!     for tests via the env var of the same name on the TUI side).
//!   - `CM_WORKFLOW_RUN_ID` / `CM_ROLE` — only when the session is a
//!     workflow participant. Used by the existing workflow tools
//!     (`workflow_transition`, `workflow_done`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::workflow::toml_schema::Engine;

/// Workflow metadata, passed when the session is a workflow participant.
#[derive(Clone, Debug)]
pub struct WorkflowMeta<'a> {
    pub run_id: &'a str,
    pub role: &'a str,
}

/// Per-session MCP config dir.
fn mcp_config_dir(session_uid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cm/mcp").join(session_uid))
}

/// Build the env map shared between the Claude config file's `env` block
/// and Codex's `-c` overrides. Putting CM_TUI_* into the MCP child's env
/// (rather than the parent process env) is the path that's known to work
/// — Claude Code doesn't reliably propagate parent env to MCP children.
fn build_env(session_uid: &str, workflow: Option<&WorkflowMeta>) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CM_TUI_SESSION_ID".into(), session_uid.to_string());
    env.insert(
        "CM_TUI_SOCKET".into(),
        crate::control::server::default_socket_path()
            .to_string_lossy()
            .to_string(),
    );
    if let Some(wf) = workflow {
        env.insert("CM_WORKFLOW_RUN_ID".into(), wf.run_id.into());
        env.insert("CM_ROLE".into(), wf.role.into());
    }
    env
}

/// Write the per-session Claude MCP config file. Returns the path,
/// which callers pass via `--mcp-config <path>`.
pub fn write_claude_mcp_config(
    session_uid: &str,
    workflow: Option<WorkflowMeta>,
) -> std::io::Result<PathBuf> {
    let server = crate::workflow::spawn::mcp_server_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not locate mcp_server/server.py",
        )
    })?;
    let dir = mcp_config_dir(session_uid).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set")
    })?;
    fs::create_dir_all(&dir)?;
    let path = dir.join("claude.json");
    let env = build_env(session_uid, workflow.as_ref());
    let config = json!({
        "mcpServers": {
            "claude-manager": {
                "command": "python",
                "args": [server.to_string_lossy()],
                "env": env,
            }
        }
    });
    fs::write(
        &path,
        serde_json::to_string_pretty(&config).unwrap_or_default(),
    )?;
    Ok(path)
}

/// Build Codex `-c mcp_servers.claude-manager.* = ...` overrides. Returned
/// as a flat `[..., "-c", "k=v", "-c", "k=v"]` list ready to splice into
/// argv. Codex doesn't have a per-session config file like Claude — its
/// MCP registration is inline.
pub fn codex_overrides(
    session_uid: &str,
    workflow: Option<&WorkflowMeta>,
) -> Vec<String> {
    let server = crate::workflow::spawn::mcp_server_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let env = build_env(session_uid, workflow);
    let env_toml = env
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_toml(v)))
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "-c".into(),
        r#"mcp_servers.claude-manager.command="python""#.into(),
        "-c".into(),
        format!(
            r#"mcp_servers.claude-manager.args=["{}"]"#,
            escape_toml(&server)
        ),
        "-c".into(),
        format!(r#"mcp_servers.claude-manager.env={{{}}}"#, env_toml),
    ]
}

/// Build the full Claude argv (after the `claude` program name) for an
/// agent session. `resume_session_id` resumes an existing transcript;
/// `extra` is appended verbatim for any caller-specific flags.
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

/// Build the full Codex argv. When `resume_session_id` is `Some(sid)`,
/// the argv is built for the `codex resume` subcommand — `resume`
/// becomes the first arg and the SESSION_ID is positional at the end
/// so it isn't consumed as the value of a preceding flag.
pub fn codex_args(
    session_uid: &str,
    workflow: Option<WorkflowMeta>,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    if resume_session_id.is_some() {
        args.push("resume".into());
    }
    args.push("--dangerously-bypass-approvals-and-sandbox".into());
    // Disable codex's startup update check: when a new version is published,
    // accepting the popup tears down the TUI and exits with "Please restart
    // Codex", which inside our PTY looks like a blank/dead session.
    args.push("-c".into());
    args.push("check_for_update_on_startup=false".into());
    args.extend(codex_overrides(session_uid, workflow.as_ref()));
    if let Some(sid) = resume_session_id {
        args.push(sid.to_string());
    }
    args
}

/// Build (program, argv) for an engine. Generates the per-session MCP
/// config file (Claude) or inline overrides (Codex). Used by every
/// spawn path.
pub fn build_args(
    engine: &Engine,
    session_uid: &str,
    workflow: Option<WorkflowMeta>,
    resume_session_id: Option<&str>,
) -> std::io::Result<(String, Vec<String>)> {
    match engine {
        Engine::ClaudeCode => {
            let cfg = write_claude_mcp_config(session_uid, workflow)?;
            let args = claude_args(&cfg, resume_session_id, &[]);
            Ok(("claude".to_string(), args))
        }
        Engine::Codex => {
            let args = codex_args(session_uid, workflow, resume_session_id);
            Ok(("codex".to_string(), args))
        }
    }
}

/// Minimal TOML-string escape: backslashes and double-quotes. Paths under
/// `~/.cm/` are alphanumeric + `-` + `/` so this is mostly defensive.
fn escape_toml(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_env_includes_session_uid_and_socket() {
        let env = build_env("uid-x", None);
        assert_eq!(env.get("CM_TUI_SESSION_ID").map(|s| s.as_str()), Some("uid-x"));
        assert!(env.contains_key("CM_TUI_SOCKET"));
        assert!(!env.contains_key("CM_WORKFLOW_RUN_ID"));
    }

    #[test]
    fn build_env_includes_workflow_when_present() {
        let env = build_env(
            "uid-x",
            Some(&WorkflowMeta {
                run_id: "wf_1",
                role: "worker",
            }),
        );
        assert_eq!(env.get("CM_WORKFLOW_RUN_ID").map(|s| s.as_str()), Some("wf_1"));
        assert_eq!(env.get("CM_ROLE").map(|s| s.as_str()), Some("worker"));
    }

    #[test]
    fn codex_overrides_contains_session_uid() {
        let args = codex_overrides("uid-x", None);
        assert!(args.iter().any(|a| a.contains("CM_TUI_SESSION_ID=\"uid-x\"")));
        assert_eq!(args.iter().filter(|a| *a == "-c").count(), 3);
    }

    #[test]
    fn codex_args_no_resume_subcommand_when_none() {
        let args = codex_args("uid-x", None, None);
        assert!(!args.iter().any(|a| a == "resume"));
    }

    #[test]
    fn codex_args_resume_subcommand_when_some() {
        let sid = "01234567-89ab-cdef-0123-456789abcdef";
        let args = codex_args(
            "uid-x",
            Some(WorkflowMeta {
                run_id: "wf",
                role: "manager",
            }),
            Some(sid),
        );
        assert_eq!(args.first().map(|s| s.as_str()), Some("resume"));
        let sid_pos = args.iter().position(|a| a == sid).unwrap();
        let last_dash_c = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-c")
            .map(|(i, _)| i)
            .last()
            .unwrap();
        assert!(sid_pos > last_dash_c);
        // Workflow env present.
        assert!(args.iter().any(|a| a.contains(r#"CM_ROLE="manager""#)));
    }

    #[test]
    fn codex_args_bypass_trust_prompt() {
        let args = codex_args("uid-x", None, None);
        assert!(args
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn codex_args_disable_update_check() {
        let args = codex_args("uid-x", None, None);
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-c" && w[1] == "check_for_update_on_startup=false"));
    }

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
}
