//! Always-on auto-pre-trust for daemon-spawned `claude-code` sessions.
//!
//! ## Why this exists
//!
//! `claude` records folder trust per-path in `~/.claude.json` under
//! `projects["<abs cwd>"].hasTrustDialogAccepted` (a bool).
//! `--dangerously-skip-permissions` does NOT bypass it. A daemon-spawned
//! interactive `claude` launched in an UNTRUSTED directory sits at the
//! "Do you trust the files in this folder?" dialog forever — the session's
//! state stays `pending`, no transcript is ever written, and a workflow
//! wedges at iteration 1 with no human present to answer the prompt.
//!
//! Pre-seeding the trust entry BEFORE spawn fixes it (proven end-to-end on
//! cm-manager: a feedback workflow then drove worker -> reviewer -> manager to
//! done). This is UNCONDITIONAL — no config flag — because the operator only
//! ever spawns their own trusted repos through the daemon.
//!
//! ## The second trust gate: project MCP servers
//!
//! Folder-trust is NOT the only prompt a headless `claude` can wedge on. When
//! the working dir contains a `.mcp.json` (project-scoped MCP servers), `claude`
//! asks a SEPARATE "Enable this project's MCP servers?" question on first run —
//! a gate that `--dangerously-skip-permissions` / `hasTrustDialogAccepted` do
//! NOT cover. With no human to answer, the process spawns the MCP child but
//! never writes a transcript and hangs (observed: 227s on cm-manager in a repo
//! whose `.mcp.json` declared `postgres-remote` + `claude-manager`).
//!
//! Modern `claude` reads that per-project approval from
//! `<working_dir>/.claude/settings.local.json`'s `enabledMcpjsonServers` array
//! — NOT from `~/.claude.json`. (A live headless test confirmed that writing
//! `enabledMcpjsonServers` into `~/.claude.json` does NOT skip the prompt;
//! `claude` still hung. The operator's working local config carries the
//! approval in `<repo>/.claude/settings.local.json`.) `settings.local.json` is
//! gitignored, so a freshly cloned worktree lacks it and headless `claude`
//! prompts and hangs. So alongside folder-trust we read `<working_dir>/.mcp.json`
//! and union its declared server names into the `enabledMcpjsonServers` array of
//! `<working_dir>/.claude/settings.local.json`, creating that file (and the
//! `.claude/` dir) when absent.
//!
//! ## Safety contract (every point is load-bearing)
//!
//! - **Two independent, decoupled writes.** Folder-trust touches `~/.claude.json`
//!   ONLY; project-MCP approval touches `<working_dir>/.claude/settings.local.json`
//!   ONLY. `~/.claude.json` never receives `enabledMcpjsonServers` from this
//!   code. Either write can no-op or fail without affecting the other.
//! - **Merge, never clobber.** For `~/.claude.json`, preserve every other
//!   top-level key and every other project entry; only add/update the one
//!   project's entry. For `settings.local.json`, preserve every other top-level
//!   key (`permissions`, `hooks`, …); only union `enabledMcpjsonServers`. Both
//!   results stay valid JSON.
//! - **Atomic.** Write to a temp file in the SAME directory as the target, then
//!   `rename` over it — a crash or a concurrent reader never sees a partial
//!   file. `~/.claude.json` is written 0600 (it can hold oauth credentials);
//!   `settings.local.json` is NOT a credentials file, so it matches an existing
//!   file's mode or defaults to 0644.
//! - **Best-effort.** On ANY error (read / parse / write / mkdir) we log and
//!   continue. We NEVER fail or block the spawn. The public entry points return
//!   `()`. Reading `.mcp.json` is itself best-effort: absent / unreadable /
//!   malformed / no `mcpServers` => the MCP part is a no-op (no
//!   `settings.local.json` created/changed) and only folder-trust is written.
//! - **Refuse to clobber a malformed file.** If the existing `~/.claude.json`
//!   or `settings.local.json` does not parse as a JSON object, log and SKIP that
//!   write (leave it untouched) rather than overwriting it.
//! - **MCP: union, never remove; never touch `disabledMcpjsonServers`.** We
//!   only ADD `.mcp.json` server names to `enabledMcpjsonServers`, preserving
//!   any already there, and never modify the user's explicit disables.
//! - **Idempotent.** Folder-trust skips its write when the project entry already
//!   has `hasTrustDialogAccepted == true`. The MCP write skips when
//!   `settings.local.json`'s `enabledMcpjsonServers` already contains every
//!   `.mcp.json` server name.

use std::path::{Path, PathBuf};

/// Resolve `~/.claude.json` from `$HOME`. `None` when `HOME` is unset (the
/// caller logs and skips).
fn claude_json_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude.json"))
}

/// Spawn-path hook: pre-trust `working_dir` IFF the program ultimately exec'd
/// is `claude`. Called from
/// [`crate::session::PendingSession::spawn`] BEFORE the child is exec'd. A
/// no-op for `codex` / `bash` and for spawns without a `working_dir`.
///
/// `shell` is `SpawnParams.shell` and `args` is `SpawnParams.args`. The gate
/// keys off the program's basename being `claude`, exactly as specified. The
/// one subtlety: a memory-capped session is wrapped as
/// `systemd-run --scope ... -- <program> ...` (see
/// [`crate::mcp_config::wrap_with_systemd_run`]), which rewrites `shell` to
/// `systemd-run`. We unwrap that case by reading the token right after the
/// `--` separator so a capped `claude` is STILL pre-trusted — otherwise the
/// "always-on, never wedge" guarantee would have a hole on exactly the
/// headless/capped host this feature targets.
///
/// Two independent best-effort writes: folder-trust into `~/.claude.json`, and
/// project-MCP approval into `<working_dir>/.claude/settings.local.json`. Either
/// may no-op or fail without affecting the other.
pub fn maybe_pretrust_for_spawn(shell: &str, args: &[String], working_dir: Option<&Path>) {
    if !program_is_claude(shell, args) {
        return;
    }
    let Some(wd) = working_dir else {
        return;
    };
    ensure_folder_trusted(wd);
    ensure_project_mcp_approved(wd);
}

/// True when the program that will actually `exec` is `claude` — either
/// directly (`shell` basename is `claude`) or wrapped under `systemd-run` for
/// a memory cap (`shell` basename is `systemd-run` and the token after the
/// `--` separator has basename `claude`).
fn program_is_claude(shell: &str, args: &[String]) -> bool {
    match basename(shell) {
        Some("claude") => true,
        Some("systemd-run") => args
            .iter()
            .position(|a| a == "--")
            .and_then(|sep| args.get(sep + 1))
            .and_then(|prog| basename(prog))
            .map(|b| b == "claude")
            .unwrap_or(false),
        _ => false,
    }
}

fn basename(program: &str) -> Option<&str> {
    Path::new(program).file_name().and_then(|s| s.to_str())
}

/// Best-effort: ensure `~/.claude.json` trusts `working_dir`. Resolves the path
/// from `$HOME`, then delegates to [`ensure_folder_trusted_at`]. Logs and
/// returns on ANY error; never panics, never blocks the spawn.
pub fn ensure_folder_trusted(working_dir: &Path) {
    let Some(path) = claude_json_path() else {
        eprintln!(
            "cm-daemon: claude pre-trust skipped (HOME unset) for {}",
            working_dir.display(),
        );
        return;
    };
    if let Err(reason) = ensure_folder_trusted_at(&path, working_dir) {
        eprintln!(
            "cm-daemon: claude pre-trust for {} skipped: {}",
            working_dir.display(),
            reason,
        );
    }
}

/// Core merge + atomic write against an explicit `~/.claude.json` path. Split
/// out from [`ensure_folder_trusted`] so tests can point at a tempdir without
/// mutating the process-global `HOME`. Returns `Err(reason)` for the caller to
/// log; the caller swallows it (best-effort).
fn ensure_folder_trusted_at(claude_json: &Path, working_dir: &Path) -> Result<(), String> {
    // The project key claude uses is the absolute cwd, verbatim. The spawn hook
    // gates on `working_dir.is_some()`, and production callers always pass an
    // absolute worktree path.
    let key = working_dir.to_string_lossy().into_owned();

    // 1) Read the existing file. Missing -> start from an empty object. Refuse
    //    to clobber a file that doesn't parse as a JSON object.
    let mut root = match std::fs::read_to_string(claude_json) {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                return Err(format!(
                    "existing {} is valid JSON but not an object — left untouched",
                    claude_json.display(),
                ));
            }
            Err(e) => {
                return Err(format!(
                    "existing {} does not parse as JSON ({}) — left untouched",
                    claude_json.display(),
                    e,
                ));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(e) => {
            return Err(format!("could not read {}: {}", claude_json.display(), e));
        }
    };

    // 2) Merge in the single project entry. Refuse to clobber non-object
    //    `projects` / project-entry values rather than overwrite them.
    let obj = root
        .as_object_mut()
        .expect("root is an object: checked is_object above / freshly created Map");
    let projects = obj
        .entry("projects")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let projects = projects.as_object_mut().ok_or_else(|| {
        format!(
            "{} `projects` is present but not an object — left untouched",
            claude_json.display(),
        )
    })?;
    let entry = projects
        .entry(key.clone())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let entry = entry.as_object_mut().ok_or_else(|| {
        format!(
            "{} projects[{}] is present but not an object — left untouched",
            claude_json.display(),
            key,
        )
    })?;

    // 3) Idempotent: skip the write entirely when folder-trust is already
    //    accepted. (Project-MCP approval is a SEPARATE write to
    //    `<working_dir>/.claude/settings.local.json` — see
    //    `ensure_project_mcp_approved` — and never appears in `~/.claude.json`.)
    let already_trusted =
        entry.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true));
    if already_trusted {
        return Ok(());
    }
    entry.insert(
        "hasTrustDialogAccepted".into(),
        serde_json::Value::Bool(true),
    );
    entry.insert(
        "hasCompletedProjectOnboarding".into(),
        serde_json::Value::Bool(true),
    );

    // 4) Atomic write: temp file in the same dir, then rename over the target.
    //    0600 — `~/.claude.json` can hold oauth credentials.
    write_atomic_json(claude_json, &root, 0o600)
}

/// Best-effort: pre-approve the worktree's project-scoped MCP servers so a
/// headless `claude` doesn't wedge on the "Enable this project's MCP servers?"
/// prompt. Reads `<working_dir>/.mcp.json` for its declared server names and, if
/// any, unions them into `<working_dir>/.claude/settings.local.json`'s
/// `enabledMcpjsonServers` (creating the `.claude/` dir and the file when
/// absent). A NO-OP when there is no `.mcp.json` / it's unreadable / malformed /
/// declares no `mcpServers`. Logs and returns on ANY error; never panics, never
/// blocks the spawn — and never touches `~/.claude.json`.
pub fn ensure_project_mcp_approved(working_dir: &Path) {
    let names = read_mcp_server_names(working_dir);
    if names.is_empty() {
        // No `.mcp.json` (or absent / malformed / no `mcpServers`): do NOT
        // create or modify `settings.local.json`. Folder-trust still happened.
        return;
    }
    let settings = working_dir.join(".claude").join("settings.local.json");
    if let Err(reason) = ensure_project_mcp_approved_at(&settings, &names) {
        eprintln!(
            "cm-daemon: claude project-MCP pre-approval for {} skipped: {}",
            working_dir.display(),
            reason,
        );
    }
}

/// Core merge + atomic write of `settings.local.json` against an explicit path.
/// Split out from [`ensure_project_mcp_approved`] so tests can point at a
/// tempdir. Unions `names` into the top-level `enabledMcpjsonServers` array,
/// preserving every other key (`permissions`, `hooks`, …) and never touching
/// `disabledMcpjsonServers`. Creates the parent `.claude/` dir and the file when
/// absent. Refuses to clobber a file that doesn't parse as a JSON object.
/// Idempotent: skips the write when `enabledMcpjsonServers` already contains
/// every name. Returns `Err(reason)` for the caller to log (best-effort).
fn ensure_project_mcp_approved_at(settings_path: &Path, names: &[String]) -> Result<(), String> {
    // 1) Read the existing file. Missing -> start from an empty object. Refuse
    //    to clobber a file that doesn't parse as a JSON object.
    let mut root = match std::fs::read_to_string(settings_path) {
        Ok(contents) => match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                return Err(format!(
                    "existing {} is valid JSON but not an object — left untouched",
                    settings_path.display(),
                ));
            }
            Err(e) => {
                return Err(format!(
                    "existing {} does not parse as JSON ({}) — left untouched",
                    settings_path.display(),
                    e,
                ));
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(e) => {
            return Err(format!("could not read {}: {}", settings_path.display(), e));
        }
    };

    let obj = root
        .as_object_mut()
        .expect("root is an object: checked is_object above / freshly created Map");

    // 2) Idempotent: skip the write entirely when every `.mcp.json` name is
    //    already present in `enabledMcpjsonServers`.
    if enabled_contains_all(obj, names) {
        return Ok(());
    }

    // 3) Union the names into `enabledMcpjsonServers` (never removes existing
    //    names; never touches `disabledMcpjsonServers`; preserves all other
    //    keys). `names` is non-empty here (the wrapper short-circuits on empty).
    merge_enabled_mcp_servers(obj, names);

    // 4) Ensure the parent `.claude/` dir exists before the atomic temp+rename.
    if let Some(dir) = settings_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("create dir {}: {}", dir.display(), e))?;
    }

    // 5) Atomic write: temp file in the same dir, then rename over the target.
    //    NOT a credentials file — match an existing file's mode or default 0644.
    write_atomic_json(settings_path, &root, 0o644)
}

/// Best-effort: read `<working_dir>/.mcp.json` and return its declared MCP
/// server names (the keys of the top-level `mcpServers` object). Returns an
/// EMPTY vec on ANY failure — file absent, unreadable, not valid JSON, root not
/// an object, or no `mcpServers` object. The caller treats an empty result as
/// "no MCP part to do" and still writes folder-trust. NEVER fails / blocks the
/// spawn.
fn read_mcp_server_names(working_dir: &Path) -> Vec<String> {
    let path = working_dir.join(".mcp.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return Vec::new();
    };
    value
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .map(|servers| servers.keys().cloned().collect())
        .unwrap_or_default()
}

/// True when every name in `names` is already a string element of `obj`'s
/// `enabledMcpjsonServers` array (`obj` is the top-level `settings.local.json`
/// object). An empty `names` is trivially satisfied; an absent / non-array
/// `enabledMcpjsonServers` satisfies only an empty `names`. Used by the
/// idempotent short-circuit.
fn enabled_contains_all(
    entry: &serde_json::Map<String, serde_json::Value>,
    names: &[String],
) -> bool {
    if names.is_empty() {
        return true;
    }
    let existing: Vec<&str> = entry
        .get("enabledMcpjsonServers")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    names.iter().all(|n| existing.contains(&n.as_str()))
}

/// Union `names` into `obj`'s `enabledMcpjsonServers` array (`obj` is the
/// top-level `settings.local.json` object), preserving every name already
/// present (never removes) and never touching `disabledMcpjsonServers`. Creates
/// the key as an array when absent. If it is present but NOT an array (a
/// hand-corrupted value), leave it untouched rather than clobber it. A no-op
/// when `names` is empty.
fn merge_enabled_mcp_servers(
    entry: &mut serde_json::Map<String, serde_json::Value>,
    names: &[String],
) {
    if names.is_empty() {
        return;
    }
    let enabled = entry
        .entry("enabledMcpjsonServers")
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let Some(arr) = enabled.as_array_mut() else {
        return; // present but not an array — don't clobber it
    };
    for name in names {
        let present = arr.iter().any(|v| v.as_str() == Some(name.as_str()));
        if !present {
            arr.push(serde_json::Value::String(name.clone()));
        }
    }
}

/// Serialize `value` and replace `target` atomically: write a temp sibling in
/// the same directory, then `rename` it over `target`. Same-directory rename is
/// atomic on a POSIX filesystem, so a reader sees either the old file or the
/// fully-written new one — never a partial. `default_mode` is the file mode used
/// for a freshly created target (0600 for the credential-bearing
/// `~/.claude.json`, 0644 for the non-secret `settings.local.json`); an existing
/// target's mode is preserved via [`preserve_mode`].
fn write_atomic_json(
    target: &Path,
    value: &serde_json::Value,
    default_mode: u32,
) -> Result<(), String> {
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;

    // Serialize BEFORE creating any temp file so a serialization failure can't
    // strand a temp on disk.
    let mut serialized = serde_json::to_string_pretty(value)
        .map_err(|e| format!("serialize {}: {}", target.display(), e))?;
    serialized.push('\n');

    let tmp = unique_temp_sibling(dir, target);
    // Create the temp with `default_mode` UP FRONT (not the umask default) so a
    // credential-bearing config (0600) is never even momentarily world-readable
    // in the window between creation and `preserve_mode` below.
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(default_mode)
            .open(&tmp)
            .map_err(|e| format!("create temp {}: {}", tmp.display(), e))?;
        f.write_all(serialized.as_bytes())
            .map_err(|e| format!("write temp {}: {}", tmp.display(), e))?;
    }

    // Match the existing file's mode (don't loosen a 0600 `~/.claude.json`, and
    // respect whatever the operator set on an existing `settings.local.json`); a
    // freshly created target keeps `default_mode` from above. Best-effort: a
    // perms failure must not abort the write.
    preserve_mode(&tmp, target, default_mode);

    if let Err(e) = std::fs::rename(&tmp, target) {
        // Clean up the orphaned temp; ignore any cleanup error.
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "rename {} -> {}: {}",
            tmp.display(),
            target.display(),
            e,
        ));
    }
    Ok(())
}

/// Build a hidden temp path in `dir` that won't collide across concurrent
/// spawns. Uses the pid plus a process-wide atomic counter (no wall-clock /
/// RNG dependency). Kept in the same directory as `target` so the subsequent
/// `rename` stays on one filesystem and is therefore atomic.
fn unique_temp_sibling(dir: &Path, target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("claude.json");
    dir.join(format!(".{}.cm-trust.{}.{}.tmp", base, std::process::id(), n))
}

/// Best-effort: set `tmp`'s mode to match `target`'s (when it exists), else
/// `default_mode`. For `~/.claude.json` (0600) this keeps the credential-bearing
/// config from being world-readable after the rename; for `settings.local.json`
/// (0644) it respects whatever mode the operator already set. Ignores any
/// failure — perms are a safety nicety, not a gate.
fn preserve_mode(tmp: &Path, target: &Path, default_mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(target)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(default_mode);
    let _ = std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    // ---- Helper: ensure_folder_trusted_at (hermetic, explicit path) --------

    #[test]
    fn missing_file_creates_single_project_entry() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(".claude.json");
        let wd = Path::new("/home/op/work/repo-a");

        ensure_folder_trusted_at(&cfg, wd).expect("write ok");

        let v = read_json(&cfg);
        assert_eq!(
            v["projects"]["/home/op/work/repo-a"]["hasTrustDialogAccepted"],
            serde_json::json!(true),
        );
        assert_eq!(
            v["projects"]["/home/op/work/repo-a"]["hasCompletedProjectOnboarding"],
            serde_json::json!(true),
        );
        // Exactly the one project entry — nothing else invented.
        assert_eq!(v["projects"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn merge_preserves_other_top_level_keys_and_projects() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(".claude.json");
        // Pre-existing file with an unrelated top-level key, a nested object,
        // and an unrelated project entry carrying its own sub-keys.
        let existing = serde_json::json!({
            "numStartups": 7,
            "oauthAccount": { "emailAddress": "op@example.com" },
            "projects": {
                "/home/op/other": {
                    "hasTrustDialogAccepted": true,
                    "history": ["a", "b"]
                }
            }
        });
        std::fs::write(&cfg, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        ensure_folder_trusted_at(&cfg, Path::new("/home/op/work/repo-b")).expect("write ok");

        let v = read_json(&cfg);
        // New entry added.
        assert_eq!(
            v["projects"]["/home/op/work/repo-b"]["hasTrustDialogAccepted"],
            serde_json::json!(true),
        );
        // Other top-level keys untouched.
        assert_eq!(v["numStartups"], serde_json::json!(7));
        assert_eq!(
            v["oauthAccount"]["emailAddress"],
            serde_json::json!("op@example.com"),
        );
        // Other project entry + its sub-keys untouched.
        assert_eq!(
            v["projects"]["/home/op/other"]["history"],
            serde_json::json!(["a", "b"]),
        );
        assert_eq!(
            v["projects"]["/home/op/other"]["hasTrustDialogAccepted"],
            serde_json::json!(true),
        );
        // Two project entries now (ours + the pre-existing one).
        assert_eq!(v["projects"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn malformed_file_is_left_untouched() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(".claude.json");
        let garbage = "{ this is not valid json ]";
        std::fs::write(&cfg, garbage).unwrap();

        let err = ensure_folder_trusted_at(&cfg, Path::new("/w")).expect_err("must skip");
        assert!(err.contains("does not parse as JSON"), "err: {err}");
        // Bytes unchanged — never clobbered.
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), garbage);
    }

    #[test]
    fn valid_json_non_object_is_left_untouched() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(".claude.json");
        std::fs::write(&cfg, "[1, 2, 3]").unwrap();

        let err = ensure_folder_trusted_at(&cfg, Path::new("/w")).expect_err("must skip");
        assert!(err.contains("not an object"), "err: {err}");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "[1, 2, 3]");
    }

    #[test]
    fn idempotent_second_call_does_not_duplicate_or_corrupt() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(".claude.json");
        let wd = Path::new("/home/op/work/repo-c");

        ensure_folder_trusted_at(&cfg, wd).expect("first write ok");
        let after_first = std::fs::read_to_string(&cfg).unwrap();

        ensure_folder_trusted_at(&cfg, wd).expect("second call ok");
        let after_second = std::fs::read_to_string(&cfg).unwrap();

        // Idempotent short-circuit => byte-identical (no rewrite at all).
        assert_eq!(after_first, after_second);
        let v = read_json(&cfg);
        assert_eq!(v["projects"].as_object().unwrap().len(), 1);
        assert_eq!(
            v["projects"][wd.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            serde_json::json!(true),
        );
    }

    #[test]
    fn write_is_atomic_no_leftover_temp_sibling() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(".claude.json");

        ensure_folder_trusted_at(&cfg, Path::new("/w")).expect("write ok");

        // After a successful temp + rename, the directory holds exactly one
        // file (the target) — the temp sibling was renamed away, not stranded.
        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec![".claude.json".to_string()], "got {entries:?}");
        // And the result is valid JSON.
        let _ = read_json(&cfg);
    }

    #[test]
    fn write_failure_returns_err_without_panicking() {
        // Parent directory does not exist -> the temp write fails -> a clean
        // Err (which the best-effort caller swallows), NOT a panic.
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("does-not-exist").join(".claude.json");

        let err = ensure_folder_trusted_at(&target, Path::new("/w")).expect_err("must fail");
        assert!(err.contains("temp"), "err: {err}");
    }

    #[test]
    fn fresh_file_is_created_0600_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join(".claude.json");

        ensure_folder_trusted_at(&cfg, Path::new("/w")).expect("write ok");

        // The credential-bearing config must not be world/group readable.
        let mode = std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "got mode {:o}", mode);
    }

    // ---- Project MCP-server pre-approval (.mcp.json -> <wd>/.claude/settings.local.json) -

    /// Collect a settings.local.json's TOP-LEVEL `enabledMcpjsonServers` as
    /// owned strings. (Modern `claude` reads project-MCP approval from here, NOT
    /// from `~/.claude.json`.)
    fn settings_enabled(path: &Path) -> Vec<String> {
        read_json(path)["enabledMcpjsonServers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn spawn_writes_folder_trust_to_claude_json_and_mcp_to_settings_local() {
        // The headline acceptance: folder-trust lands in ~/.claude.json, the MCP
        // approval lands in <wd>/.claude/settings.local.json, and ~/.claude.json
        // NEVER receives enabledMcpjsonServers from this code.
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        let claude_json = home.path().join(".claude.json");

        let wt = home.path().join("work/repo");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".mcp.json"),
            r#"{"mcpServers": {"postgres-remote": {"url": "x"}, "claude-manager": {}}}"#,
        )
        .unwrap();

        maybe_pretrust_for_spawn("claude", &[], Some(&wt));

        // Folder-trust in ~/.claude.json (unchanged behavior)...
        let cj = read_json(&claude_json);
        let key = wt.to_string_lossy().into_owned();
        assert_eq!(
            cj["projects"][&key]["hasTrustDialogAccepted"],
            serde_json::json!(true),
        );
        // ...but ~/.claude.json must NOT carry enabledMcpjsonServers, anywhere.
        assert!(
            cj["projects"][&key].get("enabledMcpjsonServers").is_none(),
            "~/.claude.json project entry must NOT carry enabledMcpjsonServers",
        );
        assert!(
            !cj.as_object().unwrap().contains_key("enabledMcpjsonServers"),
            "~/.claude.json must have no top-level enabledMcpjsonServers either",
        );

        // The MCP approval lands in <wd>/.claude/settings.local.json.
        let settings = wt.join(".claude").join("settings.local.json");
        let enabled = settings_enabled(&settings);
        assert!(enabled.contains(&"postgres-remote".to_string()), "{enabled:?}");
        assert!(enabled.contains(&"claude-manager".to_string()), "{enabled:?}");
    }

    #[test]
    fn settings_local_created_with_enabled_servers() {
        // Neither the .claude/ dir nor the file exists: both are created.
        let wt = TempDir::new().unwrap();
        let settings = wt.path().join(".claude").join("settings.local.json");
        assert!(!settings.exists());

        ensure_project_mcp_approved_at(&settings, &["a".into(), "b".into()]).expect("write ok");

        let enabled = settings_enabled(&settings);
        assert!(enabled.contains(&"a".to_string()), "{enabled:?}");
        assert!(enabled.contains(&"b".to_string()), "{enabled:?}");
    }

    #[test]
    fn settings_local_union_merge_preserves_permissions_and_hooks() {
        let wt = TempDir::new().unwrap();
        let claude_dir = wt.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = claude_dir.join("settings.local.json");
        // Existing file carrying unrelated keys, a pre-existing enabled name and
        // an explicit disable.
        let existing = serde_json::json!({
            "permissions": { "allow": ["Bash(ls:*)"] },
            "hooks": { "Stop": [{ "hooks": [{ "type": "command", "command": "echo hi" }] }] },
            "enabledMcpjsonServers": ["keep-me", "already-here"],
            "disabledMcpjsonServers": ["nope"]
        });
        std::fs::write(&settings, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        ensure_project_mcp_approved_at(&settings, &["new-one".into(), "keep-me".into()])
            .expect("write ok");

        let v = read_json(&settings);
        // Every other key preserved verbatim.
        assert_eq!(v["permissions"]["allow"], serde_json::json!(["Bash(ls:*)"]));
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            serde_json::json!("echo hi"),
        );
        // enabledMcpjsonServers unioned, no duplicate.
        let enabled = settings_enabled(&settings);
        assert!(enabled.contains(&"keep-me".to_string()), "{enabled:?}");
        assert!(enabled.contains(&"already-here".to_string()), "{enabled:?}");
        assert!(enabled.contains(&"new-one".to_string()), "{enabled:?}");
        assert_eq!(
            enabled.iter().filter(|s| *s == "keep-me").count(),
            1,
            "no duplicate: {enabled:?}",
        );
        // disabledMcpjsonServers never touched.
        assert_eq!(v["disabledMcpjsonServers"], serde_json::json!(["nope"]));
    }

    #[test]
    fn malformed_settings_local_is_left_untouched() {
        let wt = TempDir::new().unwrap();
        let claude_dir = wt.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = claude_dir.join("settings.local.json");
        let garbage = "{ not valid json ]";
        std::fs::write(&settings, garbage).unwrap();

        let err =
            ensure_project_mcp_approved_at(&settings, &["a".into()]).expect_err("must skip");
        assert!(err.contains("does not parse as JSON"), "err: {err}");
        // Bytes unchanged — never clobbered.
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), garbage);
    }

    #[test]
    fn non_object_settings_local_is_left_untouched() {
        let wt = TempDir::new().unwrap();
        let claude_dir = wt.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = claude_dir.join("settings.local.json");
        std::fs::write(&settings, "[1, 2, 3]").unwrap();

        let err =
            ensure_project_mcp_approved_at(&settings, &["a".into()]).expect_err("must skip");
        assert!(err.contains("not an object"), "err: {err}");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "[1, 2, 3]");
    }

    #[test]
    fn settings_local_idempotent_when_already_superset() {
        let wt = TempDir::new().unwrap();
        let settings = wt.path().join(".claude").join("settings.local.json");

        ensure_project_mcp_approved_at(&settings, &["a".into(), "b".into()]).expect("first ok");
        let after_first = std::fs::read_to_string(&settings).unwrap();

        ensure_project_mcp_approved_at(&settings, &["a".into(), "b".into()])
            .expect("second ok");
        let after_second = std::fs::read_to_string(&settings).unwrap();

        // Already a superset => no rewrite at all (byte-identical).
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn fresh_settings_local_is_0644_not_credentials_mode() {
        use std::os::unix::fs::PermissionsExt;
        let wt = TempDir::new().unwrap();
        let settings = wt.path().join(".claude").join("settings.local.json");

        ensure_project_mcp_approved_at(&settings, &["a".into()]).expect("write ok");

        // Not a credentials file — defaults to 0644, deterministic regardless of umask.
        let mode = std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "got mode {:o}", mode);
    }

    #[test]
    fn settings_local_preserves_existing_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let wt = TempDir::new().unwrap();
        let claude_dir = wt.path().join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        let settings = claude_dir.join("settings.local.json");
        std::fs::write(&settings, r#"{"permissions": {}}"#).unwrap();
        std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o600)).unwrap();

        ensure_project_mcp_approved_at(&settings, &["a".into()]).expect("write ok");

        // An operator-set mode on the existing file is preserved across the merge.
        let mode = std::fs::metadata(&settings).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing mode must be preserved, got {:o}", mode);
    }

    #[test]
    fn wrapper_reads_mcp_json_and_writes_settings_local() {
        let wt = TempDir::new().unwrap();
        std::fs::write(
            wt.path().join(".mcp.json"),
            r#"{"mcpServers": {"x": {}, "y": {}}}"#,
        )
        .unwrap();

        ensure_project_mcp_approved(wt.path());

        let settings = wt.path().join(".claude").join("settings.local.json");
        let enabled = settings_enabled(&settings);
        assert!(enabled.contains(&"x".to_string()), "{enabled:?}");
        assert!(enabled.contains(&"y".to_string()), "{enabled:?}");
    }

    #[test]
    fn no_mcp_json_creates_no_settings_local() {
        let wt = TempDir::new().unwrap(); // empty: no .mcp.json
        ensure_project_mcp_approved(wt.path());
        assert!(
            !wt.path().join(".claude").exists(),
            "no .mcp.json => no .claude/ dir or settings.local.json created",
        );
    }

    #[test]
    fn malformed_mcp_json_creates_no_settings_local() {
        let wt = TempDir::new().unwrap();
        std::fs::write(wt.path().join(".mcp.json"), "{ not valid json ]").unwrap();
        // Best-effort: the malformed .mcp.json must not panic or create a file.
        ensure_project_mcp_approved(wt.path());
        assert!(!wt.path().join(".claude").exists());
    }

    #[test]
    fn mcp_json_without_mcpservers_creates_no_settings_local() {
        let wt = TempDir::new().unwrap();
        // Valid JSON object, but no `mcpServers` key.
        std::fs::write(wt.path().join(".mcp.json"), r#"{"other": 1}"#).unwrap();
        ensure_project_mcp_approved(wt.path());
        assert!(!wt.path().join(".claude").exists());
    }

    #[test]
    fn late_appearing_mcp_json_is_approved_on_rerun() {
        // First run has NO .mcp.json: nothing is created. A .mcp.json then
        // appears and a re-run approves it into settings.local.json.
        let wt = TempDir::new().unwrap();

        ensure_project_mcp_approved(wt.path());
        assert!(!wt.path().join(".claude").exists());

        std::fs::write(
            wt.path().join(".mcp.json"),
            r#"{"mcpServers": {"late": {}}}"#,
        )
        .unwrap();
        ensure_project_mcp_approved(wt.path());

        let settings = wt.path().join(".claude").join("settings.local.json");
        let enabled = settings_enabled(&settings);
        assert!(enabled.contains(&"late".to_string()), "{enabled:?}");
    }

    // ---- Process-global HOME guard (mirrors mcp_config.rs) -----------------

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
    /// Alias the crate-wide env lock (NOT a private mutex) so HOME-mutating
    /// tests here serialize with every other module's HOME/env tests — the
    /// "two-mutex stomp" `test_support.rs` warns against.
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env_lock()
    }

    // ---- Spawn-path gating: maybe_pretrust_for_spawn -----------------------

    #[test]
    fn gating_claude_writes_but_bash_and_codex_do_not() {
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        let cfg = home.path().join(".claude.json");
        let wd = home.path().join("work/repo");

        // claude -> pre-trusts.
        maybe_pretrust_for_spawn("claude", &[], Some(&wd));
        assert!(cfg.exists(), "claude spawn must create ~/.claude.json");
        let v = read_json(&cfg);
        assert_eq!(
            v["projects"][wd.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            serde_json::json!(true),
        );

        // Reset, then confirm bash + codex write NOTHING.
        std::fs::remove_file(&cfg).unwrap();
        maybe_pretrust_for_spawn("/bin/bash", &[], Some(&wd));
        maybe_pretrust_for_spawn("codex", &["--foo".into()], Some(&wd));
        assert!(
            !cfg.exists(),
            "bash/codex spawns must not touch ~/.claude.json",
        );
    }

    #[test]
    fn gating_unwraps_systemd_run_for_capped_claude_only() {
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        let cfg = home.path().join(".claude.json");
        let wd = home.path().join("work/repo");

        // Capped claude: `systemd-run ... -- claude ...` unwraps to claude.
        let claude_args: Vec<String> = vec![
            "--user".into(),
            "--scope".into(),
            "--".into(),
            "claude".into(),
            "--dangerously-skip-permissions".into(),
        ];
        maybe_pretrust_for_spawn("systemd-run", &claude_args, Some(&wd));
        assert!(
            cfg.exists(),
            "capped claude under systemd-run must still be pre-trusted",
        );

        // Capped bash: `systemd-run ... -- /bin/bash` unwraps to bash -> skip.
        std::fs::remove_file(&cfg).unwrap();
        let bash_args: Vec<String> = vec!["--scope".into(), "--".into(), "/bin/bash".into()];
        maybe_pretrust_for_spawn("systemd-run", &bash_args, Some(&wd));
        assert!(
            !cfg.exists(),
            "capped bash under systemd-run must not be pre-trusted",
        );
    }

    #[test]
    fn gating_without_working_dir_is_a_noop() {
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        let cfg = home.path().join(".claude.json");

        maybe_pretrust_for_spawn("claude", &[], None);
        assert!(!cfg.exists(), "no working_dir => nothing written");
    }

    #[test]
    fn public_wrapper_swallows_errors_and_leaves_malformed_file() {
        // The best-effort public wrapper must never panic, even when the
        // underlying file is malformed (the skip path), and must leave it
        // untouched.
        let _g = home_lock();
        let home = TempDir::new().unwrap();
        let _h = HomeGuard::set(home.path());
        let cfg = home.path().join(".claude.json");
        std::fs::write(&cfg, "not json").unwrap();

        ensure_folder_trusted(&home.path().join("work")); // must not panic
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "not json");
    }
}
