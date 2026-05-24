//! Sub-2b-3 (10d-mcp-surface-2b-3): daemon-local minimal MCP
//! config helper.
//!
//! ## Scope
//!
//! Only the bits `methods::mcp_start_session` needs:
//!   - Build env block (`CM_TUI_SESSION_ID`, `CM_DAEMON_SOCKET`,
//!     authoritative-empty `CM_TUI_SOCKET`).
//!   - Write per-session Claude `--mcp-config <path>` JSON file at
//!     `~/.cm/mcp/<uid>/claude.json`.
//!   - Build Codex inline `-c` overrides.
//!   - Map session_type → `(program, argv)` for the three engines
//!     the MCP `start_session` tool supports (`claude-code`,
//!     `codex`, `bash`).
//!
//! ## Why this duplicates `tui/src/mcp_config.rs`
//!
//! The TUI module keeps growing with workflow-aware logic,
//! plan-mode handling, resume flags, etc. — none of which the
//! agent-driven `mcp_start_session` path needs. Relocating the
//! whole module to a shared crate (or pulling the daemon into a
//! dep on the TUI crate) sweeps in unrelated concerns and would
//! force every other TUI consumer to re-route. Sub-2b-3 takes the
//! bounded-duplication path; a later cleanup slice
//! (10d-workflow-controller likely) can unify if the maintenance
//! cost grows.
//!
//! ## Slice 10c-e-3b lift restriction
//!
//! Slice 10c-e-3b deliberately removed type→argv mapping from the
//! daemon's general `start_session` path — the TUI is the source
//! of truth for `argv` because it knows resume flags, memory-cap
//! wraps, etc. Sub-2b-3 brings a MINIMAL mapping back, but ONLY
//! on the `mcp_start_session` route (called by the Python MCP
//! tool with `{type, label, prompt?, task_id?}`). The general
//! `start_session` route keeps the strict "caller supplies argv"
//! shape. Document this exception in `methods::mcp_start_session`'s
//! doc comment.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

/// Per-session MCP config dir. `~/.cm/mcp/<session_uid>/`.
fn mcp_config_dir(session_uid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cm/mcp").join(session_uid))
}

/// Build the env block injected into the MCP server child.
/// Mirrors `tui/src/mcp_config.rs::build_env` for
/// `SpawnTarget::Daemon` (the only target relevant here — agents
/// spawned through the daemon's MCP-tool path route their own
/// MCP callbacks back to the daemon socket).
///
/// Wire shape:
///   - `CM_TUI_SESSION_ID = <new session uid>` — caller scope
///     identity for the new agent's own MCP calls.
///   - `CM_DAEMON_SOCKET = <absolute path to daemon.sock>` — pin.
///   - `CM_TUI_SOCKET = ""` — authoritative empty (overrides any
///     inherited `CM_TUI_SOCKET` from the daemon's env so the
///     resolver lands on the daemon socket, not a legacy TUI
///     pin). Same pattern as the TUI's build_env.
pub fn build_env(session_uid: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CM_TUI_SESSION_ID".into(), session_uid.to_string());
    env.insert(
        "CM_DAEMON_SOCKET".into(),
        absolutized_socket_or_raw(&crate::default_socket_path()),
    );
    env.insert("CM_TUI_SOCKET".into(), String::new());
    env
}

fn absolutized_socket_or_raw(p: &Path) -> String {
    crate::path::absolutize_socket_path(p)
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned())
}

/// Write the per-session Claude MCP config JSON. Returns the
/// path the caller threads through `--mcp-config <path>`.
pub fn write_claude_mcp_config(session_uid: &str) -> std::io::Result<PathBuf> {
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
    let env = build_env(session_uid);
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

/// Minimal TOML-string escape — backslashes + double-quotes.
fn escape_toml(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build Codex inline `-c mcp_servers.claude-manager.*` overrides.
/// Returns a flat `[..., "-c", "k=v", ...]` list ready to splice
/// into argv. Codex doesn't take a per-session config file —
/// everything is inline.
pub fn codex_overrides(session_uid: &str) -> Vec<String> {
    let server = crate::workflow::spawn::mcp_server_path()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let env = build_env(session_uid);
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

/// Map session_type → `(program, argv-tail)` for
/// `mcp_start_session`. The argv-tail is appended to a Vec that
/// starts with `program` — i.e. the caller does
/// `let mut argv = vec![program]; argv.extend(argv_tail);`.
///
/// Restricted to the three Python MCP tool types. Anything else
/// → `Err(io::Error::Other)` so callers surface `InvalidParams`
/// at the wire boundary.
pub fn build_args(
    session_type: &str,
    session_uid: &str,
) -> std::io::Result<(String, Vec<String>)> {
    match session_type {
        "claude-code" => {
            let cfg = write_claude_mcp_config(session_uid)?;
            let mut args = Vec::new();
            args.push("--dangerously-skip-permissions".to_string());
            args.push("--mcp-config".to_string());
            args.push(cfg.to_string_lossy().to_string());
            Ok(("claude".to_string(), args))
        }
        "codex" => {
            let mut args = Vec::new();
            args.push("--dangerously-bypass-approvals-and-sandbox".into());
            // Same update-check disable the TUI applies — prevents
            // codex's popup from tearing down the PTY.
            args.push("-c".into());
            args.push("check_for_update_on_startup=false".into());
            args.extend(codex_overrides(session_uid));
            Ok(("codex".to_string(), args))
        }
        "bash" => {
            // Raw shell. No MCP injection — bash sessions have
            // no agent, so there's nothing to wire up. The
            // session uid is still tracked by the daemon for
            // sidebar / kill-session / send_input purposes.
            Ok(("/bin/bash".to_string(), Vec::new()))
        }
        other => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "unsupported session_type '{}' for mcp_start_session; \
                 expected claude-code | codex | bash",
                other
            ),
        )),
    }
}

/// Sub-2b-3 review-fix #1: cap-aware argv wrapper. Mirrors
/// `tui/src/session.rs::wrap_with_systemd_run` (the `pub(crate)`
/// helper that the TUI-side `try_spawn_via_daemon` uses to wrap
/// `claude`/`codex`/`bash` in `systemd-run --user --scope` with
/// `MemoryHigh` + `MemoryMax`). Duplicated daemon-side per
/// sub-2b-3's "daemon-local minimal mcp_config" decision so
/// `mcp_start_session` can inherit the caller's cap onto child
/// spawns without depending on the TUI crate.
///
/// **When called with `cap: None` it's a passthrough** — returns
/// `(shell, args, None)` unchanged. Production callers from
/// `mcp_start_session` pass `None` for uncapped callers; tests
/// exercise both paths.
///
/// Wire shape:
///   - `program` → `systemd-run`
///   - prepended args: `--user --scope --quiet --unit=<name>`,
///     `-p MemoryHigh=<soft>`, `-p MemoryMax=<hard>`,
///     `-p MemorySwapMax=0`, `--`, then the original argv.
///   - unit name format: `cm-sess-<session_uid>-<nonce>-<gen>`
///     (matches the TUI's format so the cgroup-OOM watcher
///     finds it).
pub fn wrap_with_systemd_run(
    shell: &str,
    args: &[String],
    cap: Option<&CapSpec>,
) -> (String, Vec<String>, Option<PathBuf>) {
    let cap = match cap {
        Some(c) => c,
        None => return (shell.to_string(), args.to_vec(), None),
    };
    let gen = SCOPE_GENERATION.fetch_add(1, Ordering::Relaxed);
    let unit_name = format!("cm-sess-{}-{}-{}", cap.session_uid, run_nonce(), gen);
    let cgroup_path = cap.cgroup_prefix.join(format!("{}.scope", unit_name));
    let mut wrapped: Vec<String> = vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        format!("--unit={}", unit_name),
        "-p".into(),
        format!("MemoryHigh={}", cap.soft_bytes),
        "-p".into(),
        format!("MemoryMax={}", cap.hard_bytes),
        "-p".into(),
        "MemorySwapMax=0".into(),
        "--".into(),
        shell.to_string(),
    ];
    wrapped.extend(args.iter().cloned());
    ("systemd-run".to_string(), wrapped, Some(cgroup_path))
}

/// Cap parameters needed to wrap argv. Mirrors
/// `tui/src/memory_cap.rs::MemoryCap` minimally — sub-2b-3
/// duplicates only the fields the daemon-side wrap needs.
pub struct CapSpec<'a> {
    pub soft_bytes: u64,
    pub hard_bytes: u64,
    pub session_uid: &'a str,
    pub cgroup_prefix: &'a Path,
}

static SCOPE_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Per-process nonce. The TUI uses millis-since-UNIX_EPOCH;
/// the daemon's wrap could collide with a concurrent
/// TUI-driven spawn if both used the same shape. Using
/// nanos here gives more headroom and the unit-name format
/// already mixes in the session uid (which is unique).
fn run_nonce() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Save+restore guard for HOME so a test that points it at a
    /// tempdir can't leak into adjacent tests.
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
    }
    impl HomeGuard {
        fn set(home: &Path) -> Self {
            let prev = std::env::var_os("HOME");
            unsafe { std::env::set_var("HOME", home) };
            Self { prev }
        }
    }
    impl Drop for HomeGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    /// Shared lock — HOME mutation races between tests in this
    /// module (and against any other module that mutates HOME).
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
            std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    #[test]
    fn build_env_pins_daemon_socket_and_blanks_tui_socket() {
        let env = build_env("ts-abc-1");
        assert_eq!(env.get("CM_TUI_SESSION_ID").map(String::as_str), Some("ts-abc-1"));
        let daemon = env.get("CM_DAEMON_SOCKET").expect("daemon socket present");
        assert!(!daemon.is_empty(), "daemon socket must be a non-empty path");
        let tui = env.get("CM_TUI_SOCKET").expect("tui socket present");
        assert_eq!(
            tui, "",
            "tui socket must be authoritative-empty so the MCP \
             resolver doesn't fall back to an inherited TUI pin",
        );
    }

    #[test]
    fn build_args_bash_returns_bin_bash_with_no_args() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) = build_args("bash", "ts-bash-1").expect("ok");
        assert_eq!(prog, "/bin/bash");
        assert!(args.is_empty(), "bash spawns raw with no args");
    }

    #[test]
    fn build_args_claude_writes_config_and_returns_mcp_config_flag() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) = build_args("claude-code", "ts-claude-1").expect("ok");
        assert_eq!(prog, "claude");
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        let mcp_idx = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        let cfg_path = &args[mcp_idx + 1];
        assert!(
            cfg_path.contains("ts-claude-1"),
            "config path must include session uid: {}",
            cfg_path,
        );
        // Config file landed on disk with the right contents.
        let content = std::fs::read_to_string(cfg_path).expect("config readable");
        let parsed: serde_json::Value = serde_json::from_str(&content).expect("json");
        assert_eq!(
            parsed["mcpServers"]["claude-manager"]["env"]["CM_TUI_SESSION_ID"],
            "ts-claude-1",
        );
        assert_eq!(
            parsed["mcpServers"]["claude-manager"]["env"]["CM_TUI_SOCKET"],
            "",
            "TUI socket pin must be authoritative-empty in the written JSON too",
        );
    }

    #[test]
    fn build_args_codex_returns_inline_overrides() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) = build_args("codex", "ts-codex-1").expect("ok");
        assert_eq!(prog, "codex");
        assert!(
            args.iter().any(|a| a == "--dangerously-bypass-approvals-and-sandbox"),
            "codex needs the approvals-and-sandbox bypass flag",
        );
        assert!(
            args.iter().any(|a| a.contains("check_for_update_on_startup=false")),
            "codex update-check disable must be present",
        );
        // Inline overrides surface the session uid in the env block.
        assert!(
            args.iter().any(|a| a.contains("ts-codex-1")),
            "codex overrides must reference session uid",
        );
    }

    #[test]
    fn wrap_with_systemd_run_none_passthrough() {
        let (shell, args, path) = wrap_with_systemd_run(
            "/bin/bash",
            &["-c".into(), "echo hi".into()],
            None,
        );
        assert_eq!(shell, "/bin/bash");
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
        assert!(path.is_none());
    }

    #[test]
    fn wrap_with_systemd_run_some_emits_scope_with_memory_limits() {
        let prefix = std::path::PathBuf::from("/sys/fs/cgroup/user.slice");
        let cap = CapSpec {
            soft_bytes: 100 * 1024 * 1024,
            hard_bytes: 200 * 1024 * 1024,
            session_uid: "ts-cap-test",
            cgroup_prefix: &prefix,
        };
        let (shell, args, path) =
            wrap_with_systemd_run("claude", &["--foo".into()], Some(&cap));
        assert_eq!(shell, "systemd-run");
        // Must contain --user --scope; MemoryHigh + MemoryMax;
        // a separator; the original argv; and a unit-name that
        // includes the session uid.
        assert!(args.contains(&"--user".to_string()));
        assert!(args.contains(&"--scope".to_string()));
        assert!(
            args.iter().any(|a| a == "MemoryHigh=104857600"),
            "MemoryHigh=<soft_bytes> must be present: {:?}",
            args,
        );
        assert!(
            args.iter().any(|a| a == "MemoryMax=209715200"),
            "MemoryMax=<hard_bytes> must be present: {:?}",
            args,
        );
        assert!(
            args.iter().any(|a| a.starts_with("--unit=cm-sess-ts-cap-test-")),
            "unit name must encode the session uid: {:?}",
            args,
        );
        // Inner argv preserved after the `--` separator.
        let sep_idx = args.iter().position(|a| a == "--").expect("--");
        assert_eq!(args[sep_idx + 1], "claude");
        assert_eq!(args[sep_idx + 2], "--foo");
        // cgroup path returned for the watcher to subscribe to.
        let cg = path.expect("cgroup_path");
        assert!(
            cg.to_string_lossy().contains("cm-sess-ts-cap-test-"),
            "cgroup path must match the unit: {:?}",
            cg,
        );
    }

    #[test]
    fn build_args_unsupported_type_errors() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let err = build_args("gcloud", "ts-x").expect_err("must reject");
        assert!(
            err.to_string().contains("claude-code | codex | bash"),
            "error must list supported types: {}",
            err,
        );
    }
}
