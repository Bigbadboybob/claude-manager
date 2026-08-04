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
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

/// Per-session MCP config dir. `~/.cm/mcp/<session_uid>/`.
fn mcp_config_dir(session_uid: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cm/mcp").join(session_uid))
}

/// Machine-global MCP launcher script. Per-session configs point their
/// `command` here instead of baking a concrete interpreter + checkout
/// path — see [`ensure_launcher`].
fn launcher_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cm/mcp/launcher.sh"))
}

/// Sidecar recording the launcher's candidate (python, server) pairs,
/// most-recently-written first. Kept separate from the script so
/// [`ensure_launcher`] can merge new resolutions with prior ones
/// without parsing shell.
fn launcher_candidates_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    Some(home.join(".cm/mcp/launcher-candidates.json"))
}

/// Most candidate pairs the launcher retains. One live checkout plus a
/// few recent fallbacks is plenty; an unbounded list would accumulate
/// every dev worktree ever spawned from.
const LAUNCHER_MAX_CANDIDATES: usize = 4;

/// Write (or refresh) `~/.cm/mcp/launcher.sh` so it launches mcp_server
/// scripts with the freshest working (python, server) pair, and return
/// its path.
///
/// ## Why per-session configs go through a launcher
///
/// A per-session `claude.json` is written ONCE at spawn time and then
/// frozen for the life of the agent process — `claude` keeps the parsed
/// config in memory and re-spawns the MCP server from it on every
/// `/mcp` reconnect. Baking a concrete interpreter + checkout path into
/// it made long-lived sessions break whenever the machine drifted
/// underneath them, observed three separate times: bare `python`
/// vanished (conda shim), bare `python3` lost the `mcp` module (conda
/// removal), and dev-worktree server paths outlived their worktrees.
/// Each time, every affected session's reconnect died with `-32000`
/// until the session itself was respawned.
///
/// The launcher breaks that coupling: the frozen config holds only this
/// script's path (stable forever) and a server-dir-relative script name
/// (`server.py`, or `hooks/cm_stop_hook.py` for the Stop hook), and the
/// script re-resolves a WORKING pair at every launch. It is rewritten
/// at daemon/TUI startup and on every config write, so restarting cm
/// after machine drift heals every old session's next reconnect with no
/// per-session surgery.
///
/// The freshly resolved pair is prepended to the retained candidate
/// list (deduped, dead server paths pruned, capped) rather than
/// replacing it: a dev-worktree TUI legitimately points the head at its
/// own checkout, and the prior stable pair stays behind it as the
/// fallback for when that worktree is deleted. Runtime validation picks
/// the first candidate whose files exist and whose interpreter can
/// `import mcp` (for `server.py`; other scripts only need a runnable
/// interpreter).
///
/// Fail-open contract: callers fall back to writing the direct
/// (python, server) pair on `Err`, so a launcher-write failure can
/// never block a spawn.
pub fn ensure_launcher(server: &Path) -> std::io::Result<PathBuf> {
    let launcher = launcher_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set")
    })?;
    let sidecar = launcher_candidates_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set")
    })?;
    if let Some(dir) = launcher.parent() {
        fs::create_dir_all(dir)?;
    }
    let python = resolve_python_interpreter(server);
    let head = (python, server.to_string_lossy().into_owned());

    // Merge: head first, then retained pairs that still exist on disk.
    let mut pairs: Vec<(String, String)> = vec![head.clone()];
    if let Ok(raw) = fs::read_to_string(&sidecar) {
        if let Ok(serde_json::Value::Array(items)) = serde_json::from_str(&raw) {
            for it in items {
                let (Some(py), Some(srv)) = (
                    it.get("python").and_then(|v| v.as_str()),
                    it.get("server").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                let pair = (py.to_string(), srv.to_string());
                // The head is kept unconditionally (it may be an operator-
                // configured path that doesn't exist on this box — the P-2
                // remote-config case); retained tails must still exist.
                if pair != head && Path::new(srv).is_file() {
                    pairs.push(pair);
                }
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    pairs.retain(|p| seen.insert(p.clone()));
    pairs.truncate(LAUNCHER_MAX_CANDIDATES);

    let sidecar_json = serde_json::Value::Array(
        pairs
            .iter()
            .map(|(py, srv)| json!({"python": py, "server": srv}))
            .collect(),
    );
    write_if_changed(
        &sidecar,
        &format!("{}\n", serde_json::to_string_pretty(&sidecar_json).unwrap_or_default()),
        0o644,
    )?;
    write_if_changed(&launcher, &render_launcher_script(&pairs), 0o755)?;
    Ok(launcher)
}

/// Render the launcher script body for a candidate list. Split out so
/// tests can assert on content without touching HOME.
fn render_launcher_script(pairs: &[(String, String)]) -> String {
    let mut s = String::from(
        "#!/bin/sh\n\
         # Generated by claude-manager (cm). DO NOT EDIT — rewritten at every\n\
         # cm-daemon / TUI startup and session spawn.\n\
         #\n\
         # Launches an mcp_server script ($1, relative to the mcp_server dir;\n\
         # default server.py) with the first candidate pair below that still\n\
         # works. Per-session MCP configs point here instead of freezing an\n\
         # interpreter + checkout path, so a session's /mcp reconnect survives\n\
         # deleted worktrees, rebuilt venvs, and interpreter drift.\n\
         REL=\"${1:-server.py}\"\n\
         [ \"$#\" -gt 0 ] && shift\n\
         try() {\n\
         \tt_py=\"$1\"; t_dir=\"$2\"; shift 2\n\
         \t[ -f \"$t_dir/$REL\" ] || return 1\n\
         \tif [ \"$REL\" = \"server.py\" ]; then\n\
         \t\t\"$t_py\" -c 'import mcp' >/dev/null 2>&1 || return 1\n\
         \telse\n\
         \t\t\"$t_py\" -c '' >/dev/null 2>&1 || return 1\n\
         \tfi\n\
         \texec \"$t_py\" \"$t_dir/$REL\" \"$@\"\n\
         }\n\
         # Ad-hoc override for testing a checkout without rewriting configs.\n\
         if [ -n \"$CM_MCP_SERVER\" ]; then\n\
         \ttry \"${CM_MCP_PYTHON:-python3}\" \"$(dirname \"$CM_MCP_SERVER\")\" \"$@\"\n\
         fi\n",
    );
    for (py, srv) in pairs {
        let dir = Path::new(srv)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| srv.clone());
        s.push_str(&format!(
            "try '{}' '{}' \"$@\"\n",
            sh_squote(py),
            sh_squote(&dir)
        ));
    }
    s.push_str(
        "echo \"cm mcp launcher: no working (python, mcp_server) candidate — \
         restart the cm TUI/daemon to refresh $0\" >&2\nexit 1\n",
    );
    s
}

/// Escape a string for inclusion inside single quotes in sh. cm never
/// produces paths with single quotes; this is purely defensive.
fn sh_squote(s: &str) -> String {
    s.replace('\'', r"'\''")
}

/// Write `content` to `path` atomically (temp + rename), skipping the
/// write when the on-disk content already matches — the launcher is
/// refreshed on every spawn and startup, and churning mtimes for
/// identical bytes is noise. `mode` is applied to fresh writes.
fn write_if_changed(path: &Path, content: &str, mode: u32) -> std::io::Result<()> {
    if let Ok(existing) = fs::read_to_string(path) {
        if existing == content {
            // Content already right — still enforce the mode so a
            // hand-chmodded launcher can't stay non-executable.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            }
            return Ok(());
        }
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    }
    fs::rename(&tmp, path)
}

/// Workflow participant identity threaded into the MCP server's config `env`
/// block. Mirrors `tui/src/mcp_config.rs::WorkflowMeta`.
///
/// ## Why this MUST live in the MCP config env (not just the agent's env)
///
/// `workflow_transition` / `workflow_done` / `workflow_reject_finding` read
/// `CM_WORKFLOW_RUN_ID` + `CM_ROLE` from `os.environ` and hard-fail when
/// absent (mcp_server/server.py). The MCP server is a *child of the agent
/// process* (`claude` / `codex`), and Claude Code does NOT reliably propagate
/// the agent's parent env to its MCP children (documented at
/// `tui/src/mcp_config.rs`). So setting these on the agent's own
/// `spawn_params.env` is insufficient — they must be written into the MCP
/// server's config `env` block, which is the only env the child is guaranteed
/// to receive. Without this, every daemon-launched reviewer/manager fails to
/// transition or finish and the headless run stalls.
pub struct WorkflowMeta<'a> {
    pub run_id: &'a str,
    pub role: &'a str,
}

/// Build the env block injected into the MCP server child.
/// Mirrors `tui/src/mcp_config.rs::build_env` for
/// `SpawnTarget::Daemon` (the only target relevant here — agents
/// spawned through the daemon's `mcp_start_session` path).
///
/// Sub-2c wire shape:
///   - `CM_TUI_SESSION_ID = <new session uid>` — caller scope
///     identity for the new agent's own MCP calls.
///   - `CM_DAEMON_SOCKET = <absolute path to daemon.sock>` — pin
///     used by daemon-supported methods
///     (`mcp_start_session`, `list_sessions`, `propose_task`,
///     etc.) via the Python `DAEMON_METHODS` resolver.
///   - `CM_TUI_SOCKET = <absolute path to tui.sock>` — pin used
///     by TUI-only methods (`workflow_transition`,
///     `workflow_done`, `create_subtask`, etc.). Pre-sub-2c this
///     was authoritative-empty; the daemon-spawned agent
///     couldn't reach TUI-only methods then. The workflow
///     controller stays TUI-side until slice 10d-workflow-
///     controller relocates it, so daemon-spawned agents need
///     both sockets in flight.
pub fn build_env(
    session_uid: &str,
    workflow: Option<&WorkflowMeta>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("CM_TUI_SESSION_ID".into(), session_uid.to_string());
    env.insert(
        "CM_DAEMON_SOCKET".into(),
        absolutized_socket_or_raw(&crate::default_socket_path()),
    );
    // CM_TUI_SOCKET pins where tui-routed MCP methods go (workflow_transition /
    // workflow_done / create_subtask / list_subtasks / mark_subtask_done). With
    // a TUI present (laptop) that's `tui.sock` — the TUI serves them. On a
    // HEADLESS daemon host (e.g. cm-manager) there is NO `tui.sock`, so pinning
    // to it makes every such call ENOENT; the daemon itself now serves these
    // methods (API-backed subtask CRUD + daemon-side workflow controller), so
    // pin to the daemon socket instead. "Is there a TUI?" is detected by the
    // presence of `tui.sock` at spawn time: the TUI creates it at startup and
    // spawns sessions only while running, so a laptop always sees it; a remote
    // daemon host never does. This keeps laptop routing byte-for-byte unchanged
    // while unblocking daemon-spawned agents (continuous orchestrators,
    // MCP/remote workers) on headless hosts.
    let tui_sock = crate::default_tui_socket_path();
    let tui_sock_pin = if tui_sock.exists() {
        absolutized_socket_or_raw(&tui_sock)
    } else {
        absolutized_socket_or_raw(&crate::default_socket_path())
    };
    env.insert("CM_TUI_SOCKET".into(), tui_sock_pin);
    // Workflow participant identity — see `WorkflowMeta`. Must be in the MCP
    // server's config env (this block), NOT just the agent's process env,
    // because the MCP child doesn't inherit the agent's env.
    if let Some(wf) = workflow {
        env.insert("CM_WORKFLOW_RUN_ID".into(), wf.run_id.to_string());
        env.insert("CM_ROLE".into(), wf.role.to_string());
    }
    env
}

fn absolutized_socket_or_raw(p: &Path) -> String {
    crate::path::absolutize_socket_path(p)
        .map(|abs| abs.to_string_lossy().into_owned())
        .unwrap_or_else(|_| p.to_string_lossy().into_owned())
}

/// Resolve the path to `mcp_server/server.py`, PREFERRING the daemon's
/// configured `mcp_server_path` (from `daemon.toml`) when set.
///
/// P-2: the env/repo-relative `spawn::mcp_server_path()` fallback does NOT
/// resolve on a configured remote deployment like cm-manager (`/opt/cm-daemon`,
/// no repo tree, daemon process may lack `CM_MCP_SERVER`). Since this phase's
/// whole point is headless execution there, a participant whose MCP config
/// pointed at a missing/empty server path could never start its MCP server and
/// so could never call `workflow_transition` / `workflow_done`. Threading the
/// configured path through fixes that; the env/repo resolution stays as the
/// local-workstation fallback.
pub fn resolve_server_path(server_path_override: Option<&str>) -> Option<PathBuf> {
    server_path_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(crate::workflow::spawn::mcp_server_path)
}

/// Build the inline-JSON value for a claude-code spawn's `--settings`
/// flag, injecting the cm Stop hook (`mcp_server/hooks/cm_stop_hook.py`
/// beside the resolved server.py). The hook (a) reports turn-ends to
/// the daemon (`session.turn_ended` → `semantic_idle`) and (b) drains
/// the session's monitor inbox at turn boundaries via Stop-hook
/// block+reason. Returns `None` (spawn proceeds hook-less) when the
/// script isn't present at the resolved location — e.g. a deployment
/// that hasn't shipped it yet. Public so the TUI's `claude_args` twin
/// injects the identical settings.
pub fn claude_settings_hook_arg(server_path_override: Option<&str>) -> Option<String> {
    let server = resolve_server_path(server_path_override)?;
    let hook = server.parent()?.join("hooks").join("cm_stop_hook.py");
    if !hook.is_file() {
        return None;
    }
    // The hook `command` runs via `sh -c`; single-quote each word so
    // spaces in paths survive. (Paths with single quotes in them are
    // not a layout cm produces.) Same launcher indirection as the MCP
    // config: the `--settings` argv is frozen for the life of the
    // claude process, so a baked interpreter path here goes stale the
    // same way — route through the launcher, direct pair as fallback.
    let command = match ensure_launcher(&server) {
        Ok(launcher) => format!(
            "'{}' 'hooks/cm_stop_hook.py'",
            launcher.to_string_lossy()
        ),
        Err(_) => format!(
            "'{}' '{}'",
            resolve_python_interpreter(&server),
            hook.to_string_lossy()
        ),
    };
    Some(
        serde_json::json!({
            "hooks": {
                "Stop": [
                    {"hooks": [
                        {"type": "command", "command": command, "timeout": 15}
                    ]}
                ]
            }
        })
        .to_string(),
    )
}

/// Resolve the Python interpreter that runs `server.py`, in priority order:
///
///   1. `.venv/bin/python` adjacent to the server (the cm-manager / any
///      venv-based deployment layout — `/opt/cm-daemon/mcp_server/.venv/`).
///   2. `.venv/bin/python` at the repo root, i.e. the server dir's PARENT
///      (the local-workstation layout — `<repo>/.venv` + `<repo>/mcp_server/`).
///   3. The PRIMARY checkout's venv when the server lives in a linked git
///      worktree that has no venv of its own (see `primary_checkout_venv`).
///   4. Bare `python3` when it resolves on PATH, else bare `python`.
///
/// Why this exists: the MCP config used a hardcoded `command: "python"`, but
/// cm-manager has NO `python` on PATH (only `python3` + the venv) — so the
/// participant's MCP server silently failed to start and the manager role's
/// `workflow_done` / `workflow_transition` calls returned "No such tool
/// available", wedging every headless run on the manager (observed in the
/// cm-manager e2e). The repo-root venv + `python3` tiers were added after the
/// same failure recurred LOCALLY: bare `python` there came from a conda base
/// env, and removing conda made every spawned session's MCP server ENOENT.
/// A bare interpreter name is a machine-state dependency; the venvs are not.
///
/// `is_file()` follows symlinks, so a venv whose interpreter symlink dangles
/// (its base python was uninstalled) is skipped rather than returned.
///
/// Shared with the TUI (`tui/src/mcp_config.rs`), which previously hardcoded
/// `"python"` — hence `pub`.
pub fn resolve_python_interpreter(server: &std::path::Path) -> String {
    let candidates = [
        server.parent(),
        server.parent().and_then(|d| d.parent()),
    ];
    for dir in candidates.into_iter().flatten() {
        let venv = dir.join(".venv/bin/python");
        if venv.is_file() {
            return venv.to_string_lossy().into_owned();
        }
    }
    if let Some(python) = primary_checkout_venv(server) {
        return python;
    }
    if path_has_python3() {
        "python3".to_string()
    } else {
        "python".to_string()
    }
}

/// `<primary checkout>/mcp_server/.venv/bin/python` when `server` lives in a
/// LINKED GIT WORKTREE that has no venv of its own.
///
/// This tier exists because the bare-interpreter fallback below is a
/// machine-state guess, and pairing it with a perfectly valid server path
/// produces a silently broken spawn. A fresh cm worktree has no `.venv` —
/// nothing creates one — so any daemon whose `mcp_server_path` resolved into a
/// worktree fell straight through tiers 1-2 to bare `python3`. On a host where
/// the mcp deps live only in the checkout's venv (the normal local layout,
/// since conda was removed) that interpreter cannot `import mcp`, so:
/// preflight failed, EVERY spawned session got an MCP server that would not
/// start, and — because `claude_settings_hook_arg` runs the cm Stop hook
/// through the same interpreter — `session.turn_ended` never reached the
/// daemon. Those sessions kept `transcript_id: null` forever, so the TUI never
/// marked them ready: a live, working agent that is invisible in the sidebar,
/// with the only signal a preflight warning in the daemon log. Observed
/// 2026-08-03 with the daemon started from a worktree by `cm-redeploy`.
///
/// Worktrees share their primary checkout's git common dir, which is where a
/// built venv reliably lives, so borrowing it is both cheap and correct: the
/// server.py being run still comes from the worktree, only the interpreter is
/// borrowed. Returns `None` outside a git tree, or when the primary checkout
/// has no venv either — the bare fallback still applies there.
fn primary_checkout_venv(server: &std::path::Path) -> Option<String> {
    let server_dir = server.parent()?;
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(server_dir)
        .arg("rev-parse")
        .arg("--path-format=absolute")
        .arg("--git-common-dir")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common_dir = std::str::from_utf8(&out.stdout).ok()?.trim().to_string();
    if common_dir.is_empty() {
        return None;
    }
    // `<primary>/.git` → `<primary>`.
    let primary = std::path::Path::new(&common_dir).parent()?;
    // Same two layouts tiers 1-2 accept, rooted at the primary checkout.
    for rel in ["mcp_server/.venv/bin/python", ".venv/bin/python"] {
        let venv = primary.join(rel);
        if venv.is_file() {
            return Some(venv.to_string_lossy().into_owned());
        }
    }
    None
}

/// Does `python3` resolve on this process's PATH?
fn path_has_python3() -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|p| p.join("python3").is_file())
        })
        .unwrap_or(false)
}

/// Daemon-startup preflight: resolve the MCP server path + interpreter EXACTLY
/// as a spawned participant would, then actually run `<python> server.py
/// --selftest` to confirm it imports and registers its tools on THIS host.
///
/// This closes the "works locally, breaks headless" class that bit this project
/// repeatedly (bare `python` absent on the VM; `server.py` crashing on
/// `from cli.planning_client` when the `cli` package isn't deployed): each was a
/// per-spawn silent failure discovered only when a workflow's manager couldn't
/// call `workflow_done`. Running the same resolution + a real import once at
/// startup surfaces the gap with a single actionable error instead.
///
/// `Ok(summary)` on success (the selftest's stderr line); `Err(message)` with
/// the exact reproduction command + failure otherwise. The caller logs loudly;
/// it is intentionally NON-fatal (the daemon still serves non-workflow
/// sessions), but the error names precisely what an operator must fix.
pub fn run_mcp_preflight(server_path_override: Option<&str>) -> Result<String, String> {
    let server = resolve_server_path(server_path_override).ok_or_else(|| {
        "could not locate mcp_server/server.py (set `mcp_server_path` in \
         daemon.toml on a headless/remote host)"
            .to_string()
    })?;
    let python = resolve_python_interpreter(&server);
    let output = std::process::Command::new(&python)
        .arg(server.as_os_str())
        .arg("--selftest")
        .output()
        .map_err(|e| {
            format!(
                "could not run `{} {} --selftest`: {} — is the interpreter \
                 present on PATH (or is the adjacent .venv missing)?",
                python,
                server.display(),
                e,
            )
        })?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        Ok(format!(
            "{} {} --selftest → {}",
            python,
            server.display(),
            stderr.trim(),
        ))
    } else {
        Err(format!(
            "MCP server self-test FAILED — workflow participants cannot call \
             workflow tools on this host until fixed.\n  command: {} {} --selftest\
             \n  exit:    {}\n  stderr:  {}",
            python,
            server.display(),
            output.status,
            stderr.trim(),
        ))
    }
}

/// Write the per-session Claude MCP config JSON. Returns the path the caller
/// threads through `--mcp-config <path>`. `server_path_override` (the daemon's
/// configured `mcp_server_path`) wins over env/repo resolution — see
/// [`resolve_server_path`].
pub fn write_claude_mcp_config(
    session_uid: &str,
    workflow: Option<&WorkflowMeta>,
    server_path_override: Option<&str>,
) -> std::io::Result<PathBuf> {
    let server = resolve_server_path(server_path_override).ok_or_else(|| {
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
    let env = build_env(session_uid, workflow);
    // Route through the drift-proof launcher; fall back to the direct
    // (python, server) pair only if the launcher can't be written —
    // see `ensure_launcher` (fail-open: never block a spawn).
    let (command, first_arg) = match ensure_launcher(&server) {
        Ok(launcher) => (
            launcher.to_string_lossy().into_owned(),
            "server.py".to_string(),
        ),
        Err(_) => (
            resolve_python_interpreter(&server),
            server.to_string_lossy().into_owned(),
        ),
    };
    let config = json!({
        "mcpServers": {
            "claude-manager": {
                "command": command,
                "args": [first_arg],
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
pub fn codex_overrides(
    session_uid: &str,
    workflow: Option<&WorkflowMeta>,
    server_path_override: Option<&str>,
) -> Vec<String> {
    let server_path = resolve_server_path(server_path_override);
    // Same launcher indirection as the Claude config; direct pair as
    // the fail-open fallback. Codex freezes these in argv for the
    // session's lifetime, so the stable-command property matters just
    // as much here.
    let (python, server) = match server_path.as_deref().map(|s| (s, ensure_launcher(s))) {
        Some((_, Ok(launcher))) => (
            launcher.to_string_lossy().into_owned(),
            "server.py".to_string(),
        ),
        Some((s, Err(_))) => (
            resolve_python_interpreter(s),
            s.to_string_lossy().into_owned(),
        ),
        None => ("python".to_string(), String::new()),
    };
    let env = build_env(session_uid, workflow);
    let env_toml = env
        .iter()
        .map(|(k, v)| format!("{}=\"{}\"", k, escape_toml(v)))
        .collect::<Vec<_>>()
        .join(",");
    vec![
        "-c".into(),
        format!(
            r#"mcp_servers.claude-manager.command="{}""#,
            escape_toml(&python)
        ),
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
    workflow: Option<&WorkflowMeta>,
    server_path_override: Option<&str>,
    resume_session_id: Option<&str>,
) -> std::io::Result<(String, Vec<String>)> {
    match session_type {
        "claude-code" => {
            let cfg = write_claude_mcp_config(session_uid, workflow, server_path_override)?;
            let mut args = Vec::new();
            args.push("--dangerously-skip-permissions".to_string());
            args.push("--mcp-config".to_string());
            args.push(cfg.to_string_lossy().to_string());
            // S3 (async-wait branch): inject the cm Stop hook so the
            // session reports turn-ends to the daemon and drains its
            // monitor inbox at turn boundaries. `--settings` hooks
            // MERGE with project/user hooks (verified empirically,
            // claude CLI 2.1.214). Fail-open: no hook script found →
            // spawn without it.
            if let Some(settings) = claude_settings_hook_arg(server_path_override) {
                args.push("--settings".to_string());
                args.push(settings);
            }
            // P0 S3 (resume): continue the prior transcript. Mirrors
            // `tui/src/mcp_config.rs::claude_args` — `--resume <id>`
            // after `--mcp-config`. Claude APPENDS to the same `<id>.jsonl`
            // file, so the restored session's transcript_path is unchanged.
            if let Some(sid) = resume_session_id {
                args.push("--resume".to_string());
                args.push(sid.to_string());
            }
            Ok(("claude".to_string(), args))
        }
        "codex" => {
            let mut args = Vec::new();
            // P0 S3 (resume): codex resumes via the `resume` SUBCOMMAND
            // (must be the first arg) with the SESSION_ID positional at the
            // END (so it isn't swallowed as a flag value). Mirrors
            // `tui/src/mcp_config.rs::codex_args`. Unlike claude, codex
            // resume writes a NEW rollout file — the caller arms a detector
            // to rebind `transcript_path`.
            if resume_session_id.is_some() {
                args.push("resume".into());
            }
            args.push("--dangerously-bypass-approvals-and-sandbox".into());
            // Same update-check disable the TUI applies — prevents
            // codex's popup from tearing down the PTY.
            args.push("-c".into());
            args.push("check_for_update_on_startup=false".into());
            args.extend(codex_overrides(session_uid, workflow, server_path_override));
            if let Some(sid) = resume_session_id {
                args.push(sid.to_string());
            }
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

/// Cached outcome of the one-time `systemd-run --user --scope` capability probe.
/// `Ok(())` = capable; `Err(reason)` = not usable from this process, with
/// systemd-run's captured stderr/exit as the reason.
static USER_SCOPE_CAPABILITY: OnceLock<Result<(), String>> = OnceLock::new();

/// Probe (once per process) whether this process can actually create a
/// `systemd-run --user --scope` unit with a `MemoryMax` property — the exact
/// operation every memory-capped spawn depends on. Returns the authoritative
/// answer; the cheap "`app.slice` directory exists" check the cap resolvers do
/// is necessary but **not** sufficient.
///
/// ## Why the directory check isn't enough
///
/// `cm-daemon` may run as a **system** service (`cm-daemon.service`,
/// `User=lucas`, `WantedBy=multi-user.target`) with no user-session
/// environment — no `XDG_RUNTIME_DIR`, no `DBUS_SESSION_BUS_ADDRESS`, and often
/// no running `user@<uid>.service` manager at all. In that state
/// `systemd-run --user` fails at bus-connect ("Failed to connect to bus: No
/// medium found") and never creates the scope; the child stays in the daemon's
/// own `system.slice/cm-daemon.service` cgroup. `start_session`'s cgroup-scope
/// verification then rejects that child ("cm-sess-*.scope did not materialize")
/// and SIGKILLs it — failing the whole fire.
///
/// Crucially, the predicted `app.slice` cgroup path can **exist** (a live login
/// session transiently started the user manager) while the daemon process still
/// can't reach the bus (its own env lacks `XDG_RUNTIME_DIR`). So the directory
/// check passes but the spawn fails — the exact flaky failure this probe closes.
/// It runs the real command against `/bin/true` and reports what systemd-run
/// actually says.
///
/// ## Caching
///
/// The result is cached for the process lifetime: the daemon's bus reachability
/// is fixed by how the service was launched (its env is immutable), so
/// re-probing every fire would only add cost without changing the answer. A
/// daemon restart re-probes — so after an operator enables the capped path
/// (`loginctl enable-linger <user>` + `Environment=XDG_RUNTIME_DIR=/run/user/<uid>`
/// in the unit) a restart flips the cache to `Ok`.
///
/// Linux-only in practice (the crate root gates the memory-cap machinery to
/// Linux); on a host with no `systemd-run` the exec fails and we report `Err`,
/// which the resolvers treat as "run uncapped" — fail-closed to safe.
pub fn user_scope_capable() -> &'static Result<(), String> {
    USER_SCOPE_CAPABILITY.get_or_init(run_user_scope_probe)
}

fn run_user_scope_probe() -> Result<(), String> {
    use std::process::{Command, Stdio};
    // `--scope` runs `/bin/true` synchronously in a fresh transient scope
    // registered with the *user* manager and returns its exit status; the empty
    // scope auto-releases when the child exits (no `--collect` needed, which
    // keeps us compatible with older systemd). `-p MemoryMax=…` exercises the
    // memory-controller delegation the real cap relies on, not just bus
    // connectivity. 256 MiB is comfortably above anything `/bin/true` touches.
    let unit = format!("cm-sess-capprobe-{}", run_nonce());
    let output = Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--quiet",
            &format!("--unit={}", unit),
            "-p",
            "MemoryMax=268435456",
            "-p",
            "MemorySwapMax=0",
            "--",
            "/bin/true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let code = o
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "killed by signal".to_string());
            let detail = stderr.trim();
            Err(if detail.is_empty() {
                format!("systemd-run --user --scope exited {}", code)
            } else {
                format!("systemd-run --user --scope exited {}: {}", code, detail)
            })
        }
        Err(e) => Err(format!("could not exec `systemd-run`: {}", e)),
    }
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

    /// Shared lock for tests that mutate the process-global HOME. Aliases the
    /// crate-wide `test_support::env_lock` rather than declaring a private
    /// mutex: ~30 other modules serialize HOME/env on that one lock, and a
    /// second independent mutex here would let an mcp_config test flip HOME
    /// concurrently with a poller/state/transcript test (the "two-mutex stomp"
    /// test_support.rs explicitly warns against — the observed cm-daemon
    /// test-ordering flake).
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env_lock()
    }

    #[test]
    fn build_env_pins_both_sockets_with_real_paths() {
        // Sub-2c: daemon-spawned agents need reach to BOTH
        // sockets — daemon for `mcp_start_session` /
        // `list_sessions` / `propose_task` / etc., and TUI for
        // `workflow_transition` / `workflow_done` /
        // `create_subtask` / etc. The Python resolver routes
        // per-method via DAEMON_METHODS.
        let env = build_env("ts-abc-1", None);
        assert_eq!(env.get("CM_TUI_SESSION_ID").map(String::as_str), Some("ts-abc-1"));
        let daemon = env.get("CM_DAEMON_SOCKET").expect("daemon socket present");
        assert!(!daemon.is_empty(), "daemon socket must be a non-empty path");
        let tui = env.get("CM_TUI_SOCKET").expect("tui socket present");
        assert!(
            !tui.is_empty(),
            "sub-2c: tui socket must be a real path so the agent can \
             reach TUI-only methods (workflow_transition / workflow_done / \
             create_subtask / etc.) from the daemon-spawned session",
        );
    }

    /// Headless heuristic: with NO `tui.sock` present, `CM_TUI_SOCKET` pins to
    /// the DAEMON socket so tui-routed methods (create_subtask / list_subtasks /
    /// mark_subtask_done / workflow_*) reach the daemon on a remote host instead
    /// of ENOENT-ing on an absent `tui.sock`. With `tui.sock` present (laptop),
    /// it stays the tui socket. Determinism: pin HOME to a tempdir and clear the
    /// socket env overrides so path resolution is purely HOME-relative.
    #[test]
    fn build_env_tui_socket_pins_daemon_when_headless_else_tui() {
        let _g = home_lock();
        let prev_tui = std::env::var_os("CM_TUI_SOCKET");
        let prev_daemon = std::env::var_os("CM_DAEMON_SOCKET");
        unsafe {
            std::env::remove_var("CM_TUI_SOCKET");
            std::env::remove_var("CM_DAEMON_SOCKET");
        }
        let tmp = TempDir::new().unwrap();
        let _home = HomeGuard::set(tmp.path());
        std::fs::create_dir_all(tmp.path().join(".cm")).unwrap();

        // No tui.sock → headless → daemon socket.
        let pin = build_env("ts-headless", None)
            .remove("CM_TUI_SOCKET")
            .expect("present");
        assert!(
            pin.ends_with("daemon.sock") && !pin.ends_with("tui.sock"),
            "headless (no tui.sock) must pin CM_TUI_SOCKET to the daemon socket, got {:?}",
            pin,
        );

        // Create tui.sock → TUI present → tui socket.
        std::fs::write(tmp.path().join(".cm/tui.sock"), b"").unwrap();
        let pin2 = build_env("ts-laptop", None)
            .remove("CM_TUI_SOCKET")
            .expect("present");
        assert!(
            pin2.ends_with("tui.sock"),
            "with tui.sock present, CM_TUI_SOCKET must stay the tui socket, got {:?}",
            pin2,
        );

        unsafe {
            match prev_tui {
                Some(v) => std::env::set_var("CM_TUI_SOCKET", v),
                None => std::env::remove_var("CM_TUI_SOCKET"),
            }
            match prev_daemon {
                Some(v) => std::env::set_var("CM_DAEMON_SOCKET", v),
                None => std::env::remove_var("CM_DAEMON_SOCKET"),
            }
        }
    }

    #[test]
    fn claude_settings_hook_arg_injects_stop_hook_when_present() {
        let root = TempDir::new().unwrap();
        let srv_dir = root.path().join("mcp_server");
        std::fs::create_dir_all(srv_dir.join("hooks")).unwrap();
        let server = srv_dir.join("server.py");
        std::fs::write(&server, "").unwrap();
        std::fs::write(srv_dir.join("hooks/cm_stop_hook.py"), "").unwrap();

        let arg = claude_settings_hook_arg(Some(server.to_str().unwrap()))
            .expect("hook script present → settings emitted");
        let parsed: serde_json::Value = serde_json::from_str(&arg).unwrap();
        let entry = &parsed["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(entry["type"], "command");
        let cmd = entry["command"].as_str().unwrap();
        assert!(cmd.contains("cm_stop_hook.py"), "command: {cmd}");
        assert!(
            entry["timeout"].as_u64().is_some(),
            "hook must carry an explicit timeout"
        );
    }

    #[test]
    fn claude_settings_hook_arg_missing_script_is_none() {
        // No hooks/ dir beside server.py — spawn proceeds hook-less
        // (fail-open for deployments that haven't shipped the script).
        let root = TempDir::new().unwrap();
        let srv_dir = root.path().join("mcp_server");
        std::fs::create_dir_all(&srv_dir).unwrap();
        let server = srv_dir.join("server.py");
        std::fs::write(&server, "").unwrap();
        assert!(
            claude_settings_hook_arg(Some(server.to_str().unwrap())).is_none()
        );
    }

    #[test]
    fn build_args_claude_includes_settings_hook_when_present() {
        // build_args writes the per-session config under `~/.cm/mcp/` —
        // without the lock + HomeGuard every other build_args_* test
        // takes, a concurrent with_home_and_repo test's HOME flip lands
        // this write in a mid-teardown tempdir (observed NotFound flake).
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        let root = TempDir::new().unwrap();
        let srv_dir = root.path().join("mcp_server");
        std::fs::create_dir_all(srv_dir.join("hooks")).unwrap();
        let server = srv_dir.join("server.py");
        std::fs::write(&server, "").unwrap();
        std::fs::write(srv_dir.join("hooks/cm_stop_hook.py"), "").unwrap();

        let (_prog, args) = build_args(
            "claude-code",
            "ts-hooky",
            None,
            Some(server.to_str().unwrap()),
            None,
        )
        .expect("build_args");
        let pos = args
            .iter()
            .position(|a| a == "--settings")
            .expect("--settings injected when the hook script exists");
        assert!(
            args[pos + 1].contains("cm_stop_hook.py"),
            "settings JSON must reference the hook: {}",
            args[pos + 1],
        );
    }

    /// The 2026-08-03 invisible-session bug. A daemon whose `mcp_server_path`
    /// resolved into a LINKED GIT WORKTREE fell through tiers 1-2 (a fresh cm
    /// worktree has no `.venv` — nothing builds one) to bare `python3`, which
    /// on this layout cannot `import mcp`. Preflight failed, every spawned
    /// session got a dead MCP server AND a dead Stop hook, so
    /// `session.turn_ended` never landed, `transcript_id` stayed null, and the
    /// TUI never marked the session ready — a live agent invisible in the
    /// sidebar. The primary checkout's venv must be borrowed instead.
    #[test]
    fn resolve_python_borrows_primary_checkout_venv_for_a_worktree() {
        let root = TempDir::new().unwrap();
        let primary = root.path().join("primary");
        let worktrees = root.path().join("worktrees");
        std::fs::create_dir_all(primary.join("mcp_server")).unwrap();
        std::fs::create_dir_all(&worktrees).unwrap();

        // A real git repo with one commit, so `git worktree add` works.
        let git = |args: &[&str], cwd: &std::path::Path| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(cwd)
                .output()
                .expect("git")
        };
        git(&["init", "-q", "-b", "main"], &primary);
        git(&["config", "user.email", "t@t"], &primary);
        git(&["config", "user.name", "t"], &primary);
        std::fs::write(primary.join("mcp_server/server.py"), "").unwrap();
        git(&["add", "-A"], &primary);
        git(&["commit", "-qm", "seed"], &primary);

        // The primary checkout has the built venv; the worktree does not.
        let primary_venv_bin = primary.join("mcp_server/.venv/bin");
        std::fs::create_dir_all(&primary_venv_bin).unwrap();
        let primary_py = primary_venv_bin.join("python");
        std::fs::write(&primary_py, "").unwrap();

        let wt = worktrees.join("feature");
        let out = git(
            &["worktree", "add", "-q", wt.to_str().unwrap(), "main"],
            &primary,
        );
        if !out.status.success() {
            eprintln!("skipping: git worktree add unavailable here");
            return;
        }

        let wt_server = wt.join("mcp_server/server.py");
        assert!(wt_server.is_file(), "worktree must carry server.py");
        assert!(
            !wt.join("mcp_server/.venv/bin/python").exists(),
            "precondition: the worktree has no venv of its own",
        );

        let resolved = resolve_python_interpreter(&wt_server);
        assert_eq!(
            resolved,
            primary_py.to_string_lossy(),
            "a venv-less worktree must borrow the primary checkout's \
             interpreter, not fall through to a bare name that cannot \
             import mcp",
        );

        // The worktree's OWN venv still wins once it exists (tier 1).
        let wt_venv_bin = wt.join("mcp_server/.venv/bin");
        std::fs::create_dir_all(&wt_venv_bin).unwrap();
        let wt_py = wt_venv_bin.join("python");
        std::fs::write(&wt_py, "").unwrap();
        assert_eq!(
            resolve_python_interpreter(&wt_server),
            wt_py.to_string_lossy(),
            "an adjacent venv must still beat the borrowed one",
        );
    }

    #[test]
    fn resolve_python_prefers_adjacent_venv_then_repo_root_then_bare() {
        // Layout: <root>/mcp_server/server.py — mirrors both deployments.
        let root = TempDir::new().unwrap();
        let srv_dir = root.path().join("mcp_server");
        std::fs::create_dir_all(&srv_dir).unwrap();
        let server = srv_dir.join("server.py");
        std::fs::write(&server, "").unwrap();

        // No venv anywhere → a bare interpreter NAME (python3 on any host
        // that has it on PATH, python otherwise) — never a stale path.
        let bare = resolve_python_interpreter(&server);
        assert!(
            bare == "python3" || bare == "python",
            "no-venv fallback must be a bare interpreter name, got {bare}",
        );

        // Repo-root `.venv/bin/python` (local-workstation layout) → that path.
        let root_venv_bin = root.path().join(".venv/bin");
        std::fs::create_dir_all(&root_venv_bin).unwrap();
        let root_py = root_venv_bin.join("python");
        std::fs::write(&root_py, "").unwrap();
        assert_eq!(
            resolve_python_interpreter(&server),
            root_py.to_string_lossy(),
            "repo-root venv must beat the bare-name fallback",
        );

        // Adjacent `.venv/bin/python` (cm-manager layout) wins over repo root.
        let venv_bin = srv_dir.join(".venv/bin");
        std::fs::create_dir_all(&venv_bin).unwrap();
        let venv_py = venv_bin.join("python");
        std::fs::write(&venv_py, "").unwrap();
        assert_eq!(
            resolve_python_interpreter(&server),
            venv_py.to_string_lossy(),
            "must prefer the adjacent venv interpreter (cm-manager has no bare `python`)",
        );
    }

    /// A venv whose python is a DANGLING symlink (its base interpreter was
    /// uninstalled — the observed conda-removal failure) must be skipped,
    /// not returned: `is_file()` follows links.
    #[test]
    fn resolve_python_skips_venv_with_dangling_interpreter_symlink() {
        let root = TempDir::new().unwrap();
        let srv_dir = root.path().join("mcp_server");
        std::fs::create_dir_all(&srv_dir).unwrap();
        let server = srv_dir.join("server.py");
        std::fs::write(&server, "").unwrap();
        let venv_bin = srv_dir.join(".venv/bin");
        std::fs::create_dir_all(&venv_bin).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("gone/python3"),
            venv_bin.join("python"),
        )
        .unwrap();

        let resolved = resolve_python_interpreter(&server);
        assert!(
            resolved == "python3" || resolved == "python",
            "dangling venv symlink must fall through to a bare name, got {resolved}",
        );
    }

    #[test]
    fn claude_config_routes_through_launcher_with_adjacent_venv_python() {
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        // Server with an adjacent venv (mirrors /opt/cm-daemon/mcp_server).
        let srv_dir = TempDir::new().unwrap();
        let server = srv_dir.path().join("server.py");
        std::fs::write(&server, "").unwrap();
        let venv_bin = srv_dir.path().join(".venv/bin");
        std::fs::create_dir_all(&venv_bin).unwrap();
        std::fs::write(venv_bin.join("python"), "").unwrap();

        let path = write_claude_mcp_config(
            "ts-venv-1",
            None,
            Some(server.to_str().unwrap()),
        )
        .expect("write config");
        let cfg: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cmd = cfg["mcpServers"]["claude-manager"]["command"]
            .as_str()
            .unwrap();
        // The frozen per-session config must point at the STABLE
        // launcher, not a concrete interpreter — that's the whole
        // drift-immunity property.
        assert!(
            cmd.ends_with(".cm/mcp/launcher.sh"),
            "command must be the launcher, got {cmd}",
        );
        assert_eq!(
            cfg["mcpServers"]["claude-manager"]["args"][0], "server.py",
            "args must carry the server-dir-relative script name",
        );
        // The launcher itself must resolve to the adjacent venv python.
        let launcher = std::fs::read_to_string(cmd).expect("launcher on disk");
        assert!(
            launcher.contains(&*venv_bin.join("python").to_string_lossy()),
            "launcher must carry the adjacent venv python as a candidate:\n{launcher}",
        );
    }

    /// Preflight runs `<resolved python> <server.py> --selftest` and maps its
    /// exit status to Ok/Err. Hermetic: a fake `.venv/bin/python` shell script
    /// (picked up by `resolve_python_interpreter`) stands in for the real
    /// interpreter, so the test needs no system python. Covers the success
    /// path, the selftest-failure path (the cli-import / interpreter-dep class),
    /// and an unresolvable server path.
    #[test]
    fn mcp_preflight_maps_selftest_exit_status() {
        use std::os::unix::fs::PermissionsExt;
        // Returns (guard, server.py path); hold the guard so the dir survives.
        let make = |exit: i32| -> (TempDir, std::path::PathBuf) {
            let dir = TempDir::new().unwrap();
            std::fs::write(dir.path().join("server.py"), "").unwrap();
            let venv_bin = dir.path().join(".venv/bin");
            std::fs::create_dir_all(&venv_bin).unwrap();
            let py = venv_bin.join("python");
            std::fs::write(
                &py,
                format!(
                    "#!/bin/sh\necho \"selftest {} (fake)\" >&2\nexit {}\n",
                    if exit == 0 { "OK" } else { "FAILED" },
                    exit,
                ),
            )
            .unwrap();
            std::fs::set_permissions(&py, std::fs::Permissions::from_mode(0o755)).unwrap();
            let server = dir.path().join("server.py");
            (dir, server)
        };

        let (_ok_guard, ok_server) = make(0);
        let ok = run_mcp_preflight(Some(ok_server.to_str().unwrap()))
            .expect("exit 0 must map to Ok");
        assert!(ok.contains("selftest OK"), "summary carries the selftest line: {ok}");

        let (_bad_guard, bad_server) = make(1);
        let err = run_mcp_preflight(Some(bad_server.to_str().unwrap()))
            .expect_err("exit 1 must map to Err");
        assert!(err.contains("self-test FAILED"), "names the failure: {err}");
        assert!(err.contains("--selftest"), "includes the reproduction command: {err}");

        // Unresolvable server path (empty override + no repo fallback in a bare
        // temp HOME) → a clear locate error, not a panic.
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        let missing = run_mcp_preflight(Some("/nonexistent/does/not/exist/server.py"));
        // A nonexistent interpreter path still tries to spawn → Io error mapped
        // to Err (not a panic); either the locate or the spawn arm is fine.
        assert!(missing.is_err(), "a bogus server path must yield Err, got {missing:?}");
    }

    /// ensure_launcher merges the fresh resolution with retained pairs:
    /// new head first, prior pairs behind it, dead server paths pruned,
    /// duplicates collapsed.
    #[test]
    fn ensure_launcher_retains_prior_pairs_and_prunes_dead_ones() {
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());

        let mk_server = |name: &str| {
            let d = home.path().join(name);
            std::fs::create_dir_all(&d).unwrap();
            let s = d.join("server.py");
            std::fs::write(&s, "").unwrap();
            s
        };
        let a = mk_server("checkout-a/mcp_server");
        let b = mk_server("checkout-b/mcp_server");

        let launcher = ensure_launcher(&a).expect("write with a");
        ensure_launcher(&b).expect("write with b");
        let content = std::fs::read_to_string(&launcher).unwrap();
        let b_dir = b.parent().unwrap().to_string_lossy().into_owned();
        let a_dir = a.parent().unwrap().to_string_lossy().into_owned();
        let b_pos = content.find(&b_dir).expect("b present");
        let a_pos = content.find(&a_dir).expect("a retained as fallback");
        assert!(
            b_pos < a_pos,
            "freshest pair must be the head candidate:\n{content}",
        );

        // Idempotent re-write: same head, no duplicate lines.
        ensure_launcher(&b).expect("rewrite with b");
        let content2 = std::fs::read_to_string(&launcher).unwrap();
        assert_eq!(
            content2.matches(&b_dir).count(),
            1,
            "no duplicate candidates on rewrite:\n{content2}",
        );

        // Delete checkout-a (the deleted-worktree case) → pruned on the
        // next refresh.
        std::fs::remove_dir_all(home.path().join("checkout-a")).unwrap();
        ensure_launcher(&b).expect("rewrite after a died");
        let content3 = std::fs::read_to_string(&launcher).unwrap();
        assert!(
            !content3.contains(&a_dir),
            "dead server paths must be pruned:\n{content3}",
        );
    }

    /// The generated script, run under a real `/bin/sh`, must skip a
    /// candidate whose files are gone AND a candidate whose interpreter
    /// can't `import mcp`, then exec the first working pair — this is
    /// the exact conda-removal / deleted-worktree failure the launcher
    /// exists to survive.
    #[test]
    fn launcher_script_runtime_falls_back_past_broken_candidates() {
        use std::os::unix::fs::PermissionsExt;
        let root = TempDir::new().unwrap();

        // Working candidate: a fake "python" that accepts the
        // `-c 'import mcp'` probe and echoes what it was asked to run.
        let good_dir = root.path().join("good/mcp_server");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::write(good_dir.join("server.py"), "").unwrap();
        let good_py = root.path().join("good/python");
        std::fs::write(
            &good_py,
            "#!/bin/sh\nif [ \"$1\" = \"-c\" ]; then exit 0; fi\necho \"LAUNCHED:$1\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&good_py, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Broken candidate 1: server dir deleted (never created).
        let dead = ("/usr/bin/env python3".to_string(),
                    root.path().join("gone/mcp_server/server.py").to_string_lossy().into_owned());
        // Broken candidate 2: files exist, interpreter fails `import mcp`
        // (a bare python3 with no mcp module — the conda-removal case).
        let no_mcp_dir = root.path().join("nomcp/mcp_server");
        std::fs::create_dir_all(&no_mcp_dir).unwrap();
        std::fs::write(no_mcp_dir.join("server.py"), "").unwrap();
        let no_mcp_py = root.path().join("nomcp/python");
        std::fs::write(&no_mcp_py, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&no_mcp_py, std::fs::Permissions::from_mode(0o755))
            .unwrap();

        let pairs = vec![
            dead,
            (
                no_mcp_py.to_string_lossy().into_owned(),
                no_mcp_dir.join("server.py").to_string_lossy().into_owned(),
            ),
            (
                good_py.to_string_lossy().into_owned(),
                good_dir.join("server.py").to_string_lossy().into_owned(),
            ),
        ];
        let script = root.path().join("launcher.sh");
        std::fs::write(&script, render_launcher_script(&pairs)).unwrap();

        let out = std::process::Command::new("/bin/sh")
            .arg(&script)
            .env_remove("CM_MCP_SERVER")
            .output()
            .expect("run launcher");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "launcher must succeed via the good pair; stderr: {}",
            String::from_utf8_lossy(&out.stderr),
        );
        assert!(
            stdout.contains("LAUNCHED:") && stdout.contains("good/mcp_server/server.py"),
            "must exec the first WORKING candidate's server.py, got: {stdout}",
        );

        // Relative-script form (the Stop hook path): `-c ''` gate only,
        // no `import mcp` requirement.
        std::fs::write(good_dir.join("hook.py"), "").unwrap();
        let out2 = std::process::Command::new("/bin/sh")
            .arg(&script)
            .arg("hook.py")
            .env_remove("CM_MCP_SERVER")
            .output()
            .expect("run launcher with rel script");
        let stdout2 = String::from_utf8_lossy(&out2.stdout);
        assert!(
            stdout2.contains("good/mcp_server/hook.py"),
            "relative script arg must resolve against the candidate dir, got: {stdout2}",
        );

        // Nothing works → non-zero exit + a pointed stderr message.
        let none: Vec<(String, String)> = vec![];
        let empty_script = root.path().join("empty.sh");
        std::fs::write(&empty_script, render_launcher_script(&none)).unwrap();
        let out3 = std::process::Command::new("/bin/sh")
            .arg(&empty_script)
            .env_remove("CM_MCP_SERVER")
            .output()
            .expect("run empty launcher");
        assert!(!out3.status.success());
        assert!(
            String::from_utf8_lossy(&out3.stderr).contains("no working"),
            "stderr must name the failure",
        );
    }

    #[test]
    fn build_args_bash_returns_bin_bash_with_no_args() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) = build_args("bash", "ts-bash-1", None, None, None).expect("ok");
        assert_eq!(prog, "/bin/bash");
        assert!(args.is_empty(), "bash spawns raw with no args");
    }

    #[test]
    fn build_args_claude_writes_config_and_returns_mcp_config_flag() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) = build_args("claude-code", "ts-claude-1", None, None, None).expect("ok");
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
        // Sub-2c: TUI socket pin is now a real path so the
        // daemon-spawned agent can reach `workflow_transition` /
        // `workflow_done` etc. The Python resolver routes
        // per-method via DAEMON_METHODS.
        let tui_sock_in_json =
            &parsed["mcpServers"]["claude-manager"]["env"]["CM_TUI_SOCKET"];
        assert!(
            tui_sock_in_json.as_str().map(|s| !s.is_empty()).unwrap_or(false),
            "TUI socket pin must be a real path in the written JSON \
             (sub-2c): got {:?}",
            tui_sock_in_json,
        );
    }

    // ---- P0 S3 (resume): build_args resume argv -----------------------

    #[test]
    fn build_args_claude_resume_appends_resume_flag() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) =
            build_args("claude-code", "ts-rsm-1", None, None, Some("sid-abc")).expect("ok");
        assert_eq!(prog, "claude");
        assert!(
            args.windows(2).any(|w| w[0] == "--resume" && w[1] == "sid-abc"),
            "claude resume must append `--resume <id>`: {:?}",
            args,
        );
    }

    #[test]
    fn build_args_codex_resume_uses_subcommand_and_trailing_id() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) =
            build_args("codex", "ts-rsm-2", None, None, Some("sid-xyz")).expect("ok");
        assert_eq!(prog, "codex");
        assert_eq!(
            args.first().map(String::as_str),
            Some("resume"),
            "codex resume must be the FIRST arg (subcommand): {:?}",
            args,
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("sid-xyz"),
            "codex SESSION_ID must be the trailing positional: {:?}",
            args,
        );
    }

    #[test]
    fn build_args_no_resume_omits_resume_tokens() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (_p, claude) =
            build_args("claude-code", "ts-nr-1", None, None, None).expect("ok");
        assert!(
            !claude.iter().any(|a| a == "--resume"),
            "no `--resume` when not resuming: {:?}",
            claude,
        );
        let (_p2, codex) = build_args("codex", "ts-nr-2", None, None, None).expect("ok");
        assert_ne!(
            codex.first().map(String::as_str),
            Some("resume"),
            "no resume subcommand when not resuming: {:?}",
            codex,
        );
    }

    #[test]
    fn build_args_codex_returns_inline_overrides() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let (prog, args) = build_args("codex", "ts-codex-1", None, None, None).expect("ok");
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

    /// P-CRIT regression — a daemon-launched WORKFLOW PARTICIPANT's Claude MCP
    /// config env block MUST carry `CM_WORKFLOW_RUN_ID` + `CM_ROLE`, otherwise
    /// the reviewer/manager's `workflow_transition` / `workflow_done` calls
    /// hard-fail ("CM_WORKFLOW_RUN_ID is not set") and the headless run stalls.
    /// The MCP server is a child of the agent and does NOT inherit the agent's
    /// env — so these must be in the config block, not just `spawn_params.env`.
    /// (TUI analog: `tui/src/mcp_config.rs::build_env_includes_workflow_meta`.)
    #[test]
    fn build_args_claude_workflow_participant_carries_run_id_and_role() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let meta = WorkflowMeta { run_id: "wf_42", role: "reviewer" };
        let (_prog, args) =
            build_args("claude-code", "ts-wf-1", Some(&meta), None, None).expect("ok");
        let mcp_idx = args
            .iter()
            .position(|a| a == "--mcp-config")
            .expect("--mcp-config present");
        let cfg_path = &args[mcp_idx + 1];
        let content = std::fs::read_to_string(cfg_path).expect("config readable");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("json");
        let env = &parsed["mcpServers"]["claude-manager"]["env"];
        assert_eq!(
            env["CM_WORKFLOW_RUN_ID"], "wf_42",
            "MCP config env MUST carry the run id (P-CRIT)",
        );
        assert_eq!(
            env["CM_ROLE"], "reviewer",
            "MCP config env MUST carry the role (P-CRIT)",
        );
    }

    /// P-2: when the daemon's configured `mcp_server_path` is passed, the
    /// generated participant config MUST point Claude's MCP server at THAT path
    /// (preferred over env/repo resolution) — otherwise a headless run on a
    /// configured remote daemon (cm-manager, /opt/cm-daemon) writes a config
    /// pointing at a non-existent repo-relative path and can't start its MCP
    /// server. The configured path need not exist on disk for the writer to
    /// honor it (the remote deployment's path won't exist on the test box).
    #[test]
    fn write_claude_config_prefers_configured_server_path() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let configured = "/opt/cm-daemon/mcp_server/server.py";
        let cfg = write_claude_mcp_config("ts-cfg", None, Some(configured))
            .expect("writes config using configured path");
        let content = std::fs::read_to_string(&cfg).expect("config readable");
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("json");
        // The config routes through the launcher; the configured path
        // (which need not exist on this box) must be the launcher's
        // HEAD candidate — still honored over env/repo resolution (P-2).
        let cmd = parsed["mcpServers"]["claude-manager"]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.ends_with(".cm/mcp/launcher.sh"), "got {cmd}");
        let launcher = std::fs::read_to_string(cmd).expect("launcher on disk");
        assert!(
            launcher.contains("/opt/cm-daemon/mcp_server"),
            "launcher must carry the configured server dir (P-2):\n{launcher}",
        );
    }

    /// P-2 (Codex): the inline `-c` overrides route through the same
    /// launcher, which must carry the configured server path.
    #[test]
    fn codex_overrides_prefer_configured_server_path() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let configured = "/opt/cm-daemon/mcp_server/server.py";
        let args = codex_overrides("ts-cfg", None, Some(configured));
        assert!(
            args.iter().any(|a| a.contains("launcher.sh")),
            "codex command override must reference the launcher: {:?}",
            args,
        );
        assert!(
            args.iter().any(|a| a.contains(r#"args=["server.py"]"#)),
            "codex args override must be the relative script name: {:?}",
            args,
        );
        let launcher = launcher_path().unwrap();
        let content = std::fs::read_to_string(launcher).expect("launcher on disk");
        assert!(
            content.contains("/opt/cm-daemon/mcp_server"),
            "launcher must carry the configured server dir (P-2):\n{content}",
        );
    }

    /// P-CRIT regression for the Codex engine — the inline `-c` overrides that
    /// register the MCP server must put `CM_WORKFLOW_RUN_ID` + `CM_ROLE` into
    /// the server's env, same rationale as the Claude case.
    #[test]
    fn build_args_codex_workflow_participant_carries_run_id_and_role() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let meta = WorkflowMeta { run_id: "wf_99", role: "manager" };
        let (_prog, args) =
            build_args("codex", "ts-wf-codex", Some(&meta), None, None).expect("ok");
        assert!(
            args.iter().any(|a| a.contains("CM_WORKFLOW_RUN_ID=\"wf_99\"")),
            "codex overrides must register the run id in the MCP env: {:?}",
            args,
        );
        assert!(
            args.iter().any(|a| a.contains("CM_ROLE=\"manager\"")),
            "codex overrides must register the role in the MCP env: {:?}",
            args,
        );
    }

    /// Non-workflow daemon sessions (the `mcp_start_session` path) pass `None`
    /// and must NOT carry workflow vars — they aren't participants.
    #[test]
    fn build_env_without_workflow_meta_omits_run_id_and_role() {
        let env = build_env("ts-plain", None);
        assert!(env.get("CM_WORKFLOW_RUN_ID").is_none());
        assert!(env.get("CM_ROLE").is_none());
    }

    #[test]
    fn build_args_unsupported_type_errors() {
        let _g = home_lock();
        let dir = TempDir::new().unwrap();
        let _h = HomeGuard::set(dir.path());
        let err = build_args("gcloud", "ts-x", None, None, None).expect_err("must reject");
        assert!(
            err.to_string().contains("claude-code | codex | bash"),
            "error must list supported types: {}",
            err,
        );
    }
}
