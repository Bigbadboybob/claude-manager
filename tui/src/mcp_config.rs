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

/// Which spawn path is generating this MCP config — slice 10c-e-3a.
///
/// Some spawn paths are daemon-side (A-n/A-s user sessions of
/// daemon-eligible engines: `claude`, `codex`) and others are
/// TUI-local (workflow participant respawns, attach-active,
/// ad-hoc bash, gcloud). The MCP server inside the spawned
/// process must route its callbacks to the socket that *actually
/// owns* the session — a workflow role calling
/// `workflow_transition` against the daemon (which has no
/// workflow state) would surface as `UnknownMethod`, not a clean
/// abort.
///
/// Pre-10c-e-3a the env routing was keyed off a global flag,
/// conflating "the daemon exists" with "this particular spawn is
/// daemon-side." Per-spawn routing is the structural fix.
///
/// Each call site explicitly declares which it's spawning, and
/// `build_env` consumes that parameter to populate the right
/// socket pins. 10f: with daemon-mandatory, the choice is purely
/// per-session — eligible engines target `Daemon`, others stay
/// `TuiLocal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnTarget {
    /// The spawned process is a TUI-local PTY (workflow respawns,
    /// attach-active sessions, ad-hoc bash, gcloud — anything
    /// outside the daemon-eligible engine set). Its MCP server
    /// child resolves to `~/.cm/tui.sock`.
    TuiLocal,
    /// The spawned process is a daemon-owned PTY (A-n / A-s for
    /// `claude` / `codex` session types — 10f flipped these to
    /// always route through the daemon). Its MCP server child
    /// resolves to `~/.cm/daemon.sock`.
    Daemon,
}

/// Per-session MCP config dir.
fn mcp_config_dir(session_uid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cm/mcp").join(session_uid))
}

/// Build the env map shared between the Claude config file's `env` block
/// and Codex's `-c` overrides. Putting `CM_*` into the MCP child's env
/// (rather than the parent process env) is the path that's known to
/// work — Claude Code doesn't reliably propagate parent env to MCP
/// children.
///
/// ## Per-spawn routing (sub-2c)
///
/// **Both socket env vars are populated with real paths for
/// `SpawnTarget::Daemon`** so the agent can call BOTH daemon-routed
/// methods (`mcp_start_session`, `list_sessions`, `send_input`,
/// `propose_task`, etc.) and TUI-routed methods
/// (`workflow_transition`, `workflow_done`, `create_subtask`, etc.)
/// from inside the daemon-spawned session. The Python
/// `control_client.call(method, ...)` resolver picks the socket
/// per-method by consulting `DAEMON_METHODS` — daemon-supported
/// methods route to `CM_DAEMON_SOCKET`, everything else routes to
/// `CM_TUI_SOCKET`.
///
/// Both `SpawnTarget::Daemon` and `SpawnTarget::TuiLocal` now
/// produce the same env: real paths for BOTH `CM_DAEMON_SOCKET`
/// and `CM_TUI_SOCKET`. The distinction between Daemon and
/// TuiLocal is about who owns the PTY, NOT which sockets the
/// spawned agent can reach.
///
/// **Pre-fix** TuiLocal blanked `CM_DAEMON_SOCKET` under the
/// assumption that "TUI-local agents don't talk to the daemon."
/// That assumption is stale since:
///   - `workflow_transition`, `workflow_done`, and
///     `workflow_reject_finding` were relocated to daemon
///     dispatch (slices 10d-2b and 11e). The TUI socket no
///     longer implements them.
///   - The daemon is mandatory since slice 10f — auto-launched at
///     TUI startup, always reachable.
///   - The daemon authenticates TUI-owned callers via the pushed
///     `state.tui_sessions` snapshot (10d-1) — so a TUI-local
///     workflow participant can legitimately call daemon-routed
///     workflow methods.
///
/// With the blank, agents spawned as TuiLocal (e.g. workflow
/// respawns of `fresh`-context roles like the reviewer/manager)
/// would fall back to the TUI socket for workflow_transition,
/// hit the `other =>` arm of `dispatch_control`, and surface
/// `unknown_method` to the agent.
///
/// ## Authoritative pin (slice 10b review fix — still load-bearing)
///
/// **Both** socket env vars are always written with real paths.
/// The env block Claude / Codex receive is *additive* — merged
/// with the agent's inherited env — so without the explicit
/// entries an inherited `CM_DAEMON_SOCKET=...` from the
/// developer's shell could silently route to the wrong socket.
/// Always writing the real path overrides any leaked inheritance.
fn build_env(
    target: SpawnTarget,
    session_uid: &str,
    workflow: Option<&WorkflowMeta>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CM_TUI_SESSION_ID".into(), session_uid.to_string());
    // Absolutize both candidate paths up-front. MCP agents run as
    // children of `claude` / `codex`, which inherit a cwd we don't
    // control; a relative path in our env would resolve against
    // their cwd instead of the bound path. Routing through
    // `cm_daemon::path::absolutize_socket_path` keeps this in
    // lockstep with `daemon_launch.rs`'s probe-path
    // absolutization — both consumers share one definition.
    let daemon_sock_abs =
        absolutized_socket_or_raw(&cm_daemon::default_socket_path());
    let tui_sock_abs =
        absolutized_socket_or_raw(&crate::control::server::default_socket_path());
    // Both SpawnTarget variants get the same env: real paths for
    // both sockets. The Python resolver routes per-method via
    // `DAEMON_METHODS`. SpawnTarget still exists as a meaningful
    // concept at call sites (whose PTY owns the session) but no
    // longer drives env divergence.
    let _ = target;
    env.insert("CM_DAEMON_SOCKET".into(), daemon_sock_abs);
    env.insert("CM_TUI_SOCKET".into(), tui_sock_abs);
    if let Some(wf) = workflow {
        env.insert("CM_WORKFLOW_RUN_ID".into(), wf.run_id.into());
        env.insert("CM_ROLE".into(), wf.role.into());
    }
    env
}

/// Wrapper around [`cm_daemon::path::absolutize_socket_path`] that
/// falls back to the raw lossy string if absolutization fails. The
/// only failure mode is `current_dir()` erroring (typical cause:
/// the TUI's cwd was unlinked) — extremely rare, and even in that
/// case we'd rather inject the operator's literal value than fail
/// the spawn outright. The resulting env is still better than the
/// pre-fix behavior for the overwhelmingly common case.
fn absolutized_socket_or_raw(p: &Path) -> String {
    cm_daemon::path::absolutize_socket_path(p)
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned())
}

/// Write the per-session Claude MCP config file. Returns the path,
/// which callers pass via `--mcp-config <path>`. `target` flows
/// into `build_env` so the MCP server child resolves to the right
/// socket — see [`SpawnTarget`] for the per-spawn routing rationale.
pub fn write_claude_mcp_config(
    target: SpawnTarget,
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
    let env = build_env(target, session_uid, workflow.as_ref());
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
/// MCP registration is inline. `target` flows into `build_env` for
/// per-spawn socket routing.
pub fn codex_overrides(
    target: SpawnTarget,
    session_uid: &str,
    workflow: Option<&WorkflowMeta>,
) -> Vec<String> {
    let server = crate::workflow::spawn::mcp_server_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let env = build_env(target, session_uid, workflow);
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
/// so it isn't consumed as the value of a preceding flag. `target`
/// flows into `codex_overrides`/`build_env` for per-spawn socket
/// routing.
pub fn codex_args(
    target: SpawnTarget,
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
    args.extend(codex_overrides(target, session_uid, workflow.as_ref()));
    if let Some(sid) = resume_session_id {
        args.push(sid.to_string());
    }
    args
}

/// Build (program, argv) for an engine. Generates the per-session MCP
/// config file (Claude) or inline overrides (Codex). Used by every
/// spawn path. `target` declares whether this spawn is daemon-side or
/// TUI-local — see [`SpawnTarget`] for the per-spawn routing rationale.
pub fn build_args(
    target: SpawnTarget,
    engine: &Engine,
    session_uid: &str,
    workflow: Option<WorkflowMeta>,
    resume_session_id: Option<&str>,
) -> std::io::Result<(String, Vec<String>)> {
    match engine {
        Engine::ClaudeCode => {
            let cfg = write_claude_mcp_config(target, session_uid, workflow)?;
            let args = claude_args(&cfg, resume_session_id, &[]);
            Ok(("claude".to_string(), args))
        }
        Engine::Codex => {
            let args = codex_args(target, session_uid, workflow, resume_session_id);
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

    // 10f: deleted `DaemonOptInGuard` — the
    // `CM_USE_DAEMON_SOCKET` env var no longer affects routing
    // (build_env decides per `SpawnTarget`, not per env). Tests
    // below no longer set/restore the var.

    /// Save+restore guard for an arbitrary env var, so a test that
    /// pollutes the process env going in can't leak that pollution
    /// into adjacent tests sharing the env lock.
    struct EnvVarGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe { std::env::set_var(key, value); }
            Self { key, prev }
        }
    }
    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn build_env_tui_local_pins_both_sockets_with_real_paths() {
        // Post-fix: TuiLocal spawns now get real paths for BOTH
        // sockets, same as Daemon. The Python resolver routes
        // per-method via DAEMON_METHODS. Pre-fix CM_DAEMON_SOCKET
        // was authoritative-empty, which made TUI-local workflow
        // participants (e.g. fresh-context manager/reviewer
        // respawns) unable to reach daemon-dispatched
        // workflow_transition / workflow_done / workflow_reject_finding
        // — they fell back to the TUI socket which doesn't
        // implement those methods, surfacing `unknown_method`.
        let _lock = crate::test_support::home_lock();
        let env = build_env(SpawnTarget::TuiLocal, "uid-x", None);
        assert_eq!(env.get("CM_TUI_SESSION_ID").map(String::as_str), Some("uid-x"));

        let tui_sock = env.get("CM_TUI_SOCKET").expect("CM_TUI_SOCKET present");
        assert!(
            !tui_sock.is_empty(),
            "CM_TUI_SOCKET must be a real path for SpawnTarget::TuiLocal",
        );
        let daemon_sock = env.get("CM_DAEMON_SOCKET").expect("CM_DAEMON_SOCKET present");
        assert!(
            !daemon_sock.is_empty(),
            "CM_DAEMON_SOCKET must be a real path for SpawnTarget::TuiLocal so \
             daemon-dispatched workflow methods are reachable; got empty",
        );
        assert!(!env.contains_key("CM_WORKFLOW_RUN_ID"));
    }

    #[test]
    fn build_env_daemon_pins_both_sockets_with_real_paths() {
        // Sub-2c: Daemon-spawned agents need reach to BOTH
        // sockets — daemon for `mcp_start_session` /
        // `list_sessions` / `propose_task` / etc., and TUI for
        // `workflow_transition` / `workflow_done` /
        // `create_subtask` / etc. The Python control_client
        // routes per-method via DAEMON_METHODS; both env vars
        // must carry real paths.
        let _lock = crate::test_support::home_lock();
        let env = build_env(SpawnTarget::Daemon, "uid-x", None);

        let daemon_sock = env.get("CM_DAEMON_SOCKET").expect("CM_DAEMON_SOCKET present");
        assert!(
            !daemon_sock.is_empty(),
            "CM_DAEMON_SOCKET must be a real path for SpawnTarget::Daemon",
        );
        let tui_sock = env
            .get("CM_TUI_SOCKET")
            .expect("CM_TUI_SOCKET present");
        assert!(
            !tui_sock.is_empty(),
            "Sub-2c: CM_TUI_SOCKET must be a real path for SpawnTarget::Daemon \
             so agents can reach workflow_transition/workflow_done from \
             daemon-spawned sessions; got empty",
        );
    }

    // ----- Per-spawn routing acceptance (slice 10c-e-3a) ----------------

    #[test]
    fn build_env_tui_local_pins_both_sockets_independent_of_opt_in() {
        // Slice 10c-e-3a invariant: per-spawn routing doesn't
        // depend on CM_USE_DAEMON_SOCKET. Post-fix the assertion
        // shape changes — both sockets are real paths for both
        // SpawnTarget variants — but the opt-in independence
        // still holds.
        let _lock = crate::test_support::home_lock();
        let env = build_env(SpawnTarget::TuiLocal, "uid-x", None);

        let tui_sock = env
            .get("CM_TUI_SOCKET")
            .expect("CM_TUI_SOCKET present");
        assert!(
            !tui_sock.is_empty(),
            "TUI-local spawn must pin CM_TUI_SOCKET regardless of CM_USE_DAEMON_SOCKET",
        );
        let daemon_sock = env
            .get("CM_DAEMON_SOCKET")
            .expect("CM_DAEMON_SOCKET present");
        assert!(
            !daemon_sock.is_empty(),
            "TUI-local spawn must pin CM_DAEMON_SOCKET regardless of CM_USE_DAEMON_SOCKET; \
             got empty",
        );
    }

    #[test]
    fn build_env_daemon_pins_daemon_socket_even_when_opt_in_is_off() {
        // Symmetric: an explicit SpawnTarget::Daemon (e.g. a
        // ClientSession::new call site that's been migrated) must
        // route to the daemon even if CM_USE_DAEMON_SOCKET isn't
        // set in the env. The opt-in flag is a gate on WHETHER a
        // call site is allowed to choose Daemon, but per-spawn
        // routing is what build_env consumes. This makes the
        // SpawnTarget the single source of truth.
        let _lock = crate::test_support::home_lock();
        let env = build_env(SpawnTarget::Daemon, "uid-x", None);

        let daemon_sock = env
            .get("CM_DAEMON_SOCKET")
            .expect("CM_DAEMON_SOCKET present");
        assert!(
            !daemon_sock.is_empty(),
            "Daemon spawn must pin CM_DAEMON_SOCKET regardless of CM_USE_DAEMON_SOCKET; \
             got empty",
        );
        let tui_sock = env
            .get("CM_TUI_SOCKET")
            .expect("CM_TUI_SOCKET present");
        // Sub-2c: both sockets carry real paths under
        // SpawnTarget::Daemon. Pre-2c the TUI socket was the
        // authoritative empty here; that broke
        // workflow_transition from daemon-spawned sessions.
        assert!(
            !tui_sock.is_empty(),
            "Daemon spawn must carry a real CM_TUI_SOCKET path (sub-2c); \
             got {:?}",
            tui_sock,
        );
    }

    // 2026-05 fix: deleted `build_env_authoritative_override_when_wrong_var_in_process_env`.
    // The pre-fix design used an empty-string CM_DAEMON_SOCKET as
    // an "authoritative override" to defend against a developer's
    // exported env leaking into a TuiLocal spawn. Post-fix
    // TuiLocal carries the real daemon socket too (because daemon
    // is mandatory and dispatches the workflow write methods),
    // and `default_socket_path()` itself reads CM_DAEMON_SOCKET
    // from env. So the spawn env now agrees with whatever the
    // parent's env said — which is what we want in production
    // (TUI, daemon, and agent all use the same path). The old
    // test simulated mid-process env mutation, which doesn't
    // happen in real launches.

    #[test]
    fn build_env_daemon_target_keeps_daemon_socket_authoritative_over_env() {
        // Sub-2c: under SpawnTarget::Daemon, BOTH sockets carry
        // real paths. We can't defend against operator-set
        // `CM_TUI_SOCKET` overrides in env (those go through
        // `default_socket_path()` which is intentionally env-
        // aware — operators expect their override to win), but
        // we DO defend against accidental leak of
        // `CM_DAEMON_SOCKET` from a TUI launched without the
        // opt-in: build_env's emitted daemon path comes from
        // `cm_daemon::default_socket_path()` which is also
        // env-aware. As long as the operator's TUI is correctly
        // configured, the emitted paths are the right ones.
        //
        // What this test pins: both env vars are non-empty
        // under Daemon target, regardless of inherited values.
        // The agent's resolver will see both and route
        // per-method.
        let _lock = crate::test_support::home_lock();
        let env = build_env(SpawnTarget::Daemon, "uid-x", None);
        assert!(
            env.get("CM_DAEMON_SOCKET").map(|v| !v.is_empty()).unwrap_or(false),
            "daemon socket must be a real pin",
        );
        assert!(
            env.get("CM_TUI_SOCKET").map(|v| !v.is_empty()).unwrap_or(false),
            "TUI socket must be a real pin (sub-2c — both sockets populated)",
        );
    }

    #[test]
    fn build_env_includes_workflow_when_present() {
        let _lock = crate::test_support::home_lock();
        let env = build_env(
            SpawnTarget::TuiLocal,
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
        let args = codex_overrides(SpawnTarget::TuiLocal, "uid-x", None);
        assert!(args.iter().any(|a| a.contains("CM_TUI_SESSION_ID=\"uid-x\"")));
        assert_eq!(args.iter().filter(|a| *a == "-c").count(), 3);
    }

    #[test]
    fn codex_args_no_resume_subcommand_when_none() {
        let args = codex_args(SpawnTarget::TuiLocal, "uid-x", None, None);
        assert!(!args.iter().any(|a| a == "resume"));
    }

    #[test]
    fn codex_args_resume_subcommand_when_some() {
        let sid = "01234567-89ab-cdef-0123-456789abcdef";
        let args = codex_args(
            SpawnTarget::TuiLocal,
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
        let args = codex_args(SpawnTarget::TuiLocal, "uid-x", None, None);
        assert!(args
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn codex_args_disable_update_check() {
        let args = codex_args(SpawnTarget::TuiLocal, "uid-x", None, None);
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

    // ============================================================
    // Slice-10c-b review: build_env must absolutize the socket
    // path before injecting it into the agent's MCP env. Without
    // this, a relative `CM_DAEMON_SOCKET` in the TUI's env would
    // be passed verbatim to `claude` / `codex`, which would
    // resolve it against their own cwd (inherited from us, but
    // not guaranteed identical across multi-step spawn chains)
    // and dial a different inode than the daemon bound.
    // ============================================================

    #[test]
    fn build_env_absolutizes_relative_daemon_socket_under_daemon_target() {
        let _lock = crate::test_support::home_lock();
        // Relative override — set via env so default_socket_path
        // picks it up. The test must observe an absolute result.
        let _polluted = EnvVarGuard::set("CM_DAEMON_SOCKET", "rel/cm/daemon.sock");

        let env = build_env(SpawnTarget::Daemon, "uid-x", None);
        let daemon_sock = env
            .get("CM_DAEMON_SOCKET")
            .expect("CM_DAEMON_SOCKET in env");
        assert!(
            !daemon_sock.is_empty(),
            "SpawnTarget::Daemon must produce a non-empty CM_DAEMON_SOCKET",
        );
        // Load-bearing: the value MCP agents inherit must be
        // absolute, regardless of what their cwd ends up being.
        assert!(
            Path::new(daemon_sock).is_absolute(),
            "CM_DAEMON_SOCKET must be absolute when injected into agent env (got {:?})",
            daemon_sock,
        );
        // And it must point at the same absolute inode the
        // daemon-launch helper would absolutize to (i.e. cwd-joined),
        // not at some arbitrary other path. This is the
        // cross-consumer "they match" property.
        let expected = cm_daemon::path::absolutize_socket_path(Path::new(
            "rel/cm/daemon.sock",
        ))
        .expect("absolutize ok");
        assert_eq!(
            Path::new(daemon_sock),
            expected.as_path(),
            "build_env must use the same absolutize helper as daemon_launch",
        );
    }

    #[test]
    fn build_env_absolutizes_relative_tui_socket_under_tui_local_target() {
        // Symmetric: SpawnTarget::TuiLocal → CM_TUI_SOCKET must
        // also be absolutized. The TUI socket override path is
        // `crate::control::server::default_socket_path` which
        // reads `CM_TUI_SOCKET`.
        let _lock = crate::test_support::home_lock();
        let _polluted = EnvVarGuard::set("CM_TUI_SOCKET", "rel/cm/tui.sock");

        let env = build_env(SpawnTarget::TuiLocal, "uid-x", None);
        let tui_sock = env.get("CM_TUI_SOCKET").expect("CM_TUI_SOCKET in env");
        assert!(
            !tui_sock.is_empty(),
            "SpawnTarget::TuiLocal must produce a non-empty CM_TUI_SOCKET",
        );
        assert!(
            Path::new(tui_sock).is_absolute(),
            "CM_TUI_SOCKET must be absolute when injected (got {:?})",
            tui_sock,
        );
    }

    // ============================================================
    // Slice 10c-e-3b: argv parity between SpawnTarget::TuiLocal and
    // SpawnTarget::Daemon.
    //
    // The named acceptance for slice 10c-e-3b: the daemon spawn
    // must build argv using the same code path as the local spawn,
    // so the spawned child sees `--mcp-config` / Codex MCP
    // overrides / `--resume` tokens identically. The only
    // expected difference is the *value* of the env-routing pin
    // (CM_DAEMON_SOCKET vs CM_TUI_SOCKET), which:
    //   - Claude: lives in the MCP config FILE referenced by
    //     `--mcp-config <path>`, not in argv → argv is byte-identical.
    //   - Codex: lives in argv via `-c mcp_servers.…env={…}`, so
    //     the env-override element differs but every other argv
    //     element is identical.
    // ============================================================

    #[test]
    fn build_args_claude_argv_byte_identical_between_spawn_targets() {
        // Claude carries env in the MCP config file, not argv.
        // Argv must be element-wise identical regardless of
        // SpawnTarget for both unresumed and resumed shapes.
        let _lock = crate::test_support::home_lock();

        for sid in [None, Some("01234567-89ab-cdef-0123-456789abcdef")] {
            let (local_prog, local_args) = build_args(
                SpawnTarget::TuiLocal,
                &Engine::ClaudeCode,
                "uid-parity-cl",
                None,
                sid,
            )
            .expect("build_args local");
            let (daemon_prog, daemon_args) = build_args(
                SpawnTarget::Daemon,
                &Engine::ClaudeCode,
                "uid-parity-cl",
                None,
                sid,
            )
            .expect("build_args daemon");
            assert_eq!(local_prog, daemon_prog, "claude program (resume={:?})", sid);
            assert_eq!(
                local_args, daemon_args,
                "claude argv must be element-wise identical between local + daemon \
                 (resume={:?}); the only target-specific bit lives in the MCP \
                 config FILE, not in argv",
                sid,
            );
        }
    }

    #[test]
    fn build_args_codex_argv_is_identical_across_targets() {
        // Post-fix: SpawnTarget no longer drives env divergence —
        // both TuiLocal and Daemon get real paths for both
        // sockets, so codex argv is fully identical across
        // targets. Pre-fix the env override element differed
        // (one had CM_DAEMON_SOCKET blank), driven by the
        // workflow_transition routing model that's now obsolete.
        let _lock = crate::test_support::home_lock();

        let (local_prog, local_args) = build_args(
            SpawnTarget::TuiLocal,
            &Engine::Codex,
            "uid-parity-cx",
            None,
            None,
        )
        .expect("build_args local");
        let (daemon_prog, daemon_args) = build_args(
            SpawnTarget::Daemon,
            &Engine::Codex,
            "uid-parity-cx",
            None,
            None,
        )
        .expect("build_args daemon");
        assert_eq!(local_prog, daemon_prog, "codex program");
        assert_eq!(
            local_args, daemon_args,
            "codex argv must be element-wise identical between TuiLocal and Daemon \
             (both targets produce the same env now)",
        );

        // Spot-check the env override carries both sockets with
        // real paths so a future regression of the routing-by-
        // env-divergence design surfaces here.
        let env_kv_prefix = "mcp_servers.claude-manager.env=";
        let env_arg = local_args
            .iter()
            .find(|a| a.starts_with(env_kv_prefix))
            .expect("env override present in argv");
        assert!(
            env_arg.contains("CM_TUI_SOCKET=\""),
            "env override must carry CM_TUI_SOCKET: {}",
            env_arg
        );
        assert!(
            env_arg.contains("CM_DAEMON_SOCKET=\"")
                && !env_arg.contains("CM_DAEMON_SOCKET=\"\""),
            "env override must carry CM_DAEMON_SOCKET with a real path: {}",
            env_arg
        );
    }

    #[test]
    fn build_args_bash_argv_is_trivially_identical() {
        // Bash sessions don't go through `build_args` (the daemon
        // path uses bare `/bin/bash`, no MCP config). This test
        // exists as documentation of the no-op case and to keep
        // the parity story symmetric across Claude/Codex/Bash.
        //
        // For bash, both spawn paths construct the same SpawnParams
        // directly without engine-specific shaping. The argv on
        // the wire is `["/bin/bash"]` regardless of SpawnTarget.
        // Asserted in `try_spawn_via_daemon`'s match arm.
    }

    #[test]
    fn build_env_passes_through_absolute_paths_unchanged() {
        // Regression guard against the absolutization helper
        // doing something surprising to an already-absolute path
        // (the helper's contract is passthrough, but build_env's
        // wrapper could plausibly drift).
        let _lock = crate::test_support::home_lock();
        let _polluted = EnvVarGuard::set("CM_DAEMON_SOCKET", "/abs/cm/daemon.sock");

        let env = build_env(SpawnTarget::Daemon, "uid-x", None);
        let daemon_sock = env
            .get("CM_DAEMON_SOCKET")
            .expect("CM_DAEMON_SOCKET in env");
        assert_eq!(
            daemon_sock, "/abs/cm/daemon.sock",
            "absolute path must pass through unchanged",
        );
    }
}
