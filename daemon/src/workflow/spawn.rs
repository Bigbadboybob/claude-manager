//! Locate `mcp_server/server.py` for spawn paths.
//!
//! Args/MCP-config building previously lived here for workflow
//! participants only. As of Phase 1 of agent orchestration, every
//! agent spawn path goes through `crate::mcp_config` instead — so
//! regular `A-n` and planning `A-l` sessions also register the
//! `claude-manager` MCP server. This module is now just a thin
//! resolver for the server.py path.

use std::path::PathBuf;

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
