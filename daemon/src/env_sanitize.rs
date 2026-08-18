//! Startup env hygiene: scrub inherited Claude-session identity vars.
//!
//! The daemon is sometimes (re)started from INSIDE a Claude Code session —
//! an agent arming a detached restart, an operator running cm-redeploy from
//! a claude Bash tool. The new daemon then inherits the launching session's
//! identity/IPC environment, and every child it spawns inherits that in
//! turn. A claude child that comes up with a foreign
//! `CLAUDE_CODE_SESSION_ID`, `CLAUDE_CODE_CHILD_SESSION=1`, and the
//! messaging socket of a (dead) parent believes it is that session's child:
//! its turns stop landing in its own project-dir transcript, so every
//! programmatic observer — MCP transcript reads, statuses, monitors, the
//! Stop hook's resume-key re-stamp — reads a file frozen at the restart
//! moment while the PTY keeps rendering the live conversation. That split
//! (sessions alive and steerable, all observers blind) is the 2026-08-18
//! "orphaned session I/O" incident.
//!
//! Scrubbing ONCE at daemon startup covers every spawn path — agent
//! sessions, revives, workflow respawns, raw bash PTYs — because children
//! inherit the daemon's environment. Per-child injection (e.g. the spawn
//! path's own `CM_TUI_SESSION_ID`) happens after inheritance and is
//! unaffected.
//!
//! Deliberate configuration is NOT scrubbed: knobs like
//! `CLAUDE_CODE_RESUME_THRESHOLD_MINUTES` come from
//! `~/.claude/settings.json` env (claude re-applies them itself), and auth
//! material (`ANTHROPIC_*`, OAuth tokens) is left alone. The list below is
//! strictly "this process is / is inside a particular claude session"
//! identity plus its IPC endpoints.

/// Session-identity and IPC vars a claude session stamps on its children.
/// Any of these reaching a daemon-spawned claude makes it misidentify
/// itself; any reaching a bash PTY leaks the launcher's MCP identity.
const LEAKED_SESSION_VARS: &[&str] = &[
    // "You are inside a claude session" markers.
    "CLAUDECODE",
    "CLAUDE_PID",
    "AI_AGENT",
    // The launching session's identity — the var that redirected every
    // respawned session's transcript in the 2026-08-18 incident.
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    // IPC endpoints of the (likely dead) launching process.
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_SSE_PORT",
    // Process-launch details of the launcher, not of our children.
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_EFFORT",
    // cm's own per-session identity: a bash PTY child inheriting the
    // LAUNCHER's uid would authenticate to the MCP as the launcher.
    // Agent spawns always inject their own value after inheritance.
    "CM_TUI_SESSION_ID",
];

/// Remove every leaked session-identity var from this process's
/// environment. Returns the names that were actually present (for the
/// startup log), so a daemon launched from a clean shell logs nothing.
pub fn scrub_inherited_session_env() -> Vec<&'static str> {
    let mut scrubbed = Vec::new();
    for name in LEAKED_SESSION_VARS {
        if std::env::var_os(name).is_some() {
            std::env::remove_var(name);
            scrubbed.push(*name);
        }
    }
    scrubbed
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Env-mutating test: exercises the scrub end-to-end in this
    /// process. Serialized by using var names no other test touches.
    #[test]
    fn scrubs_only_present_identity_vars_and_reports_them() {
        std::env::set_var("CLAUDE_CODE_SESSION_ID", "64b95f94-dead-beef");
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
        std::env::remove_var("CLAUDE_CODE_MESSAGING_SOCKET");

        let scrubbed = scrub_inherited_session_env();

        assert!(scrubbed.contains(&"CLAUDE_CODE_SESSION_ID"));
        assert!(scrubbed.contains(&"CLAUDE_CODE_CHILD_SESSION"));
        assert!(!scrubbed.contains(&"CLAUDE_CODE_MESSAGING_SOCKET"));
        assert!(std::env::var_os("CLAUDE_CODE_SESSION_ID").is_none());
        assert!(std::env::var_os("CLAUDE_CODE_CHILD_SESSION").is_none());

        // Idempotent: a second scrub finds nothing.
        assert!(scrub_inherited_session_env().is_empty());
    }
}
