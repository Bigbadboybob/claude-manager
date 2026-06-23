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
//! ## Safety contract (every point is load-bearing)
//!
//! - **Merge, never clobber.** Preserve every other top-level key and every
//!   other project entry; only add/update the one project's entry. The result
//!   stays valid JSON.
//! - **Atomic.** Write to a temp file in the SAME directory, then `rename` over
//!   `~/.claude.json` — a crash or a concurrent reader never sees a partial
//!   file.
//! - **Best-effort.** On ANY error (read / parse / write) we log and continue.
//!   We NEVER fail or block the spawn. The public entry points return `()`.
//! - **Refuse to clobber a malformed file.** If the existing `~/.claude.json`
//!   does not parse as a JSON object, log and SKIP the write (leave it
//!   untouched) rather than overwriting it.
//! - **Idempotent.** If the project entry already has
//!   `hasTrustDialogAccepted == true`, skip the write entirely.

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
pub fn maybe_pretrust_for_spawn(shell: &str, args: &[String], working_dir: Option<&Path>) {
    if !program_is_claude(shell, args) {
        return;
    }
    let Some(wd) = working_dir else {
        return;
    };
    ensure_folder_trusted(wd);
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

    // 3) Idempotent: an already-trusted entry needs no write at all.
    if entry.get("hasTrustDialogAccepted") == Some(&serde_json::Value::Bool(true)) {
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
    write_atomic_json(claude_json, &root)
}

/// Serialize `value` and replace `target` atomically: write a temp sibling in
/// the same directory, then `rename` it over `target`. Same-directory rename is
/// atomic on a POSIX filesystem, so a reader sees either the old file or the
/// fully-written new one — never a partial.
fn write_atomic_json(target: &Path, value: &serde_json::Value) -> Result<(), String> {
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
    // Create the temp with 0600 UP FRONT (not the umask default, typically
    // 0644) so the credential-bearing config is never even momentarily
    // world-readable in the window between creation and `preserve_mode` below.
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| format!("create temp {}: {}", tmp.display(), e))?;
        f.write_all(serialized.as_bytes())
            .map_err(|e| format!("write temp {}: {}", tmp.display(), e))?;
    }

    // Match the existing file's mode (it can hold oauth credentials — don't
    // loosen a 0600 file); a freshly created target keeps the 0600 above.
    // Best-effort: a perms failure must not abort the write.
    preserve_mode(&tmp, target);

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
/// `0600`. Keeps the credential-bearing config from being world-readable after
/// the rename. Ignores any failure — perms are a safety nicety, not a gate.
fn preserve_mode(tmp: &Path, target: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(target)
        .map(|m| m.permissions().mode() & 0o777)
        .unwrap_or(0o600);
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
