//! Always-on auto-pre-trust for daemon-spawned `codex` sessions.
//!
//! ## Why this exists
//!
//! codex (verified on 0.149.1) records directory trust per project root in
//! `~/.codex/config.toml` as `[projects."<abs path>"] trust_level = "trusted"`
//! — and `--dangerously-bypass-approvals-and-sandbox` does NOT bypass the
//! "Do you trust the contents of this directory?" startup screen for an
//! untrusted root. A daemon-spawned codex in a fresh directory (a minted
//! subtask worktree whose main repo isn't trusted, a scratch dir, a new
//! host) boots into that dialog, and the headless prompt delivery then
//! lands in it: the pasted body is swallowed by the dialog, the trailing
//! kitty Enter ANSWERS the dialog instead of submitting, and the dialog's
//! dismiss-repaint defeats the delivery's PTY-quiet submit verification —
//! the daemon logs `submitted=true` while the agent sits promptless
//! (reproduced end-to-end in a sandbox daemon on 2026-08-25; the stray
//! Enter even persisted a trust entry the operator never chose).
//!
//! Pre-seeding the trust entry BEFORE spawn closes the wedge, exactly like
//! [`crate::claude_trust`] does for `claude`'s `~/.claude.json`. It is
//! UNCONDITIONAL for the same reason: the operator only ever spawns their
//! own trusted repos through the daemon.
//!
//! ## Which path is the trust key
//!
//! codex keys trust off the PROJECT ROOT it resolves for the cwd, and for a
//! git WORKTREE that resolution lands on the MAIN repository checkout (a
//! codex spawned in `~/.cm/worktrees/<x>` of a trusted `~/code/projects/<r>`
//! shows no dialog even though the worktree path is absent from
//! `config.toml` — verified live). We can't call codex's resolver, so we
//! trust BOTH candidates: the working dir itself and, when `<wd>/.git` is a
//! worktree pointer file, the main checkout it points at. Trusting a path
//! codex ends up not using is harmless (it's an inert extra entry for a
//! directory the operator's own daemon minted).
//!
//! ## Safety contract (mirrors `claude_trust`)
//!
//! - **Merge, never clobber.** `config.toml` is hand-edited by the operator
//!   and full of unrelated tables (`mcp_servers`, model config). We never
//!   reserialize it: an entry is APPENDED as a new `[projects."…"]` table
//!   at EOF, which is valid TOML and preserves every existing byte,
//!   comments included.
//! - **Parse before append.** The existing file must parse as TOML and the
//!   project key must be absent (appending a duplicate table is a TOML
//!   error that would break codex's config load). Malformed file → log and
//!   skip, leave it untouched.
//! - **Atomic.** Temp sibling + rename, preserving the existing file's
//!   mode (default 0644 for a fresh file — `config.toml` is not a
//!   credentials file; `auth.json` is separate).
//! - **Best-effort.** Any error logs and returns; the spawn is never
//!   blocked or failed.
//! - **Idempotent.** An existing `trust_level = "trusted"` entry (or any
//!   existing entry for the key — we never rewrite an operator's explicit
//!   choice, including an explicit untrusted marking) skips the append.

use std::path::{Path, PathBuf};

/// Resolve `~/.codex/config.toml` honoring `$CODEX_HOME` (codex's own
/// override, kept for parity) then `$HOME`. `None` when both are unset.
fn codex_config_path() -> Option<PathBuf> {
    if let Some(ch) = std::env::var_os("CODEX_HOME") {
        if !ch.is_empty() {
            return Some(PathBuf::from(ch).join("config.toml"));
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex").join("config.toml"))
}

/// Spawn-path hook: pre-trust `working_dir` IFF the program ultimately
/// exec'd is `codex`. Called from [`crate::session::PendingSession::spawn`]
/// alongside the claude hook, BEFORE the child is exec'd. A no-op for
/// `claude` / `bash` and for spawns without a `working_dir`.
pub fn maybe_pretrust_for_spawn(shell: &str, args: &[String], working_dir: Option<&Path>) {
    if !program_is_codex(shell, args) {
        return;
    }
    let Some(wd) = working_dir else {
        return;
    };
    let Some(config) = codex_config_path() else {
        eprintln!(
            "cm-daemon: codex pre-trust skipped (HOME and CODEX_HOME unset) for {}",
            wd.display(),
        );
        return;
    };
    for key in trust_keys_for(wd) {
        if let Err(reason) = ensure_project_trusted_at(&config, &key) {
            eprintln!(
                "cm-daemon: codex pre-trust for {} skipped: {}",
                key.display(),
                reason,
            );
        }
    }
}

/// True when the program that will actually `exec` is `codex` — either
/// directly or wrapped under `systemd-run` for a memory cap (token after
/// the `--` separator). Mirrors `claude_trust::program_is_claude`.
fn program_is_codex(shell: &str, args: &[String]) -> bool {
    match basename(shell) {
        Some("codex") => true,
        Some("systemd-run") => args
            .iter()
            .position(|a| a == "--")
            .and_then(|sep| args.get(sep + 1))
            .and_then(|prog| basename(prog))
            .map(|b| b == "codex")
            .unwrap_or(false),
        _ => false,
    }
}

fn basename(program: &str) -> Option<&str> {
    Path::new(program).file_name().and_then(|s| s.to_str())
}

/// The trust keys to seed for `wd`: the dir itself, plus the main repo
/// checkout when `wd` is a git worktree (`.git` is a pointer FILE of the
/// form `gitdir: /main/.git/worktrees/<name>`). codex resolves a
/// worktree's project root to the main checkout, so that second key is the
/// one that actually suppresses the dialog there; the first covers plain
/// dirs and any future resolution change. Best-effort: unreadable/odd
/// `.git` contents just yield the single-dir key.
fn trust_keys_for(wd: &Path) -> Vec<PathBuf> {
    let mut keys = vec![wd.to_path_buf()];
    let dotgit = wd.join(".git");
    if dotgit.is_file() {
        if let Ok(contents) = std::fs::read_to_string(&dotgit) {
            if let Some(gitdir) = contents.strip_prefix("gitdir:") {
                let gitdir = Path::new(gitdir.trim());
                // /main/.git/worktrees/<name> -> /main
                let main = gitdir
                    .ancestors()
                    .find(|a| a.file_name().map(|n| n == ".git").unwrap_or(false))
                    .and_then(|g| g.parent());
                if let Some(main) = main {
                    if !main.as_os_str().is_empty() && main != wd {
                        keys.push(main.to_path_buf());
                    }
                }
            }
        }
    }
    keys
}

/// Core check + append against an explicit `config.toml` path. Split out
/// for tests. Returns `Err(reason)` for the caller to log (best-effort).
fn ensure_project_trusted_at(config: &Path, project: &Path) -> Result<(), String> {
    let key = project.to_string_lossy().into_owned();

    // 1) Read + parse the existing file. Missing -> start empty. Refuse to
    //    touch a file that doesn't parse (appending to broken TOML can only
    //    make codex's config load worse).
    let contents = match std::fs::read_to_string(config) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("could not read {}: {}", config.display(), e)),
    };
    let parsed: toml::Value = if contents.trim().is_empty() {
        toml::Value::Table(toml::map::Map::new())
    } else {
        toml::from_str(&contents).map_err(|e| {
            format!(
                "existing {} does not parse as TOML ({}) — left untouched",
                config.display(),
                e,
            )
        })?
    };

    // 2) Idempotent / never-override: ANY existing entry for this project —
    //    trusted or an explicit operator choice of something else — skips
    //    the append. (Appending a duplicate table would also be a TOML
    //    parse error, so this check is correctness, not just politeness.)
    if parsed
        .get("projects")
        .and_then(|p| p.as_table())
        .map(|t| t.contains_key(&key))
        .unwrap_or(false)
    {
        return Ok(());
    }
    // `projects` present but not a table: refuse rather than append a
    // conflicting table definition.
    if parsed.get("projects").map(|p| !p.is_table()).unwrap_or(false) {
        return Err(format!(
            "{} `projects` is present but not a table — left untouched",
            config.display(),
        ));
    }

    // 3) Append a fresh `[projects."<key>"]` table at EOF. TOML basic-string
    //    escaping on the key (quotes + backslashes; paths are the only
    //    realistic content).
    let escaped = key.replace('\\', "\\\\").replace('"', "\\\"");
    let mut out = contents;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
        escaped,
    ));

    // 4) Sanity: the appended result must itself parse (belt + suspenders —
    //    protects codex's config load from any escaping edge we missed).
    toml::from_str::<toml::Value>(&out).map_err(|e| {
        format!(
            "appending trust entry for {} would corrupt {} ({}) — skipped",
            key,
            config.display(),
            e,
        )
    })?;

    // 5) Atomic write: temp sibling + rename. Create the parent dir when
    //    absent (fresh HOME with no ~/.codex yet).
    write_atomic(config, &out)
}

/// Write `contents` to `target` atomically (temp sibling + rename),
/// creating the parent directory when absent, matching an existing
/// target's mode (default 0644 — not a credentials file).
fn write_atomic(target: &Path, contents: &str) -> Result<(), String> {
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create dir {}: {}", dir.display(), e))?;

    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".config.toml.cm-trust.{}.{}.tmp", std::process::id(), n));

    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
            .map_err(|e| format!("create temp {}: {}", tmp.display(), e))?;
        f.write_all(contents.as_bytes())
            .map_err(|e| format!("write temp {}: {}", tmp.display(), e))?;
    }
    // Match the existing file's mode (best-effort; fresh keeps 0644).
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(target) {
            let _ = std::fs::set_permissions(
                &tmp,
                std::fs::Permissions::from_mode(meta.permissions().mode() & 0o777),
            );
        }
    }
    if let Err(e) = std::fs::rename(&tmp, target) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parse(path: &Path) -> toml::Value {
        toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn trust_of<'a>(v: &'a toml::Value, key: &str) -> Option<&'a str> {
        v.get("projects")?
            .as_table()?
            .get(key)?
            .get("trust_level")?
            .as_str()
    }

    #[test]
    fn missing_file_creates_config_with_entry() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("codex").join("config.toml");
        ensure_project_trusted_at(&cfg, Path::new("/w/repo")).expect("ok");
        let v = parse(&cfg);
        assert_eq!(trust_of(&v, "/w/repo"), Some("trusted"));
    }

    #[test]
    fn append_preserves_existing_bytes_and_comments() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("config.toml");
        let existing = "# operator comment\nmodel = \"gpt-5.6-sol\"\n\n\
                        [projects.\"/home/op/repo\"]\ntrust_level = \"trusted\"\n\n\
                        [mcp_servers.pg]\ncommand = \"bash\"\n";
        std::fs::write(&cfg, existing).unwrap();

        ensure_project_trusted_at(&cfg, Path::new("/w/new")).expect("ok");

        let after = std::fs::read_to_string(&cfg).unwrap();
        assert!(after.starts_with(existing), "existing bytes preserved verbatim");
        let v = parse(&cfg);
        assert_eq!(trust_of(&v, "/w/new"), Some("trusted"));
        assert_eq!(trust_of(&v, "/home/op/repo"), Some("trusted"));
        assert_eq!(
            v["mcp_servers"]["pg"]["command"].as_str(),
            Some("bash"),
            "unrelated tables intact",
        );
        assert_eq!(v["model"].as_str(), Some("gpt-5.6-sol"));
    }

    #[test]
    fn idempotent_when_entry_exists() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("config.toml");
        ensure_project_trusted_at(&cfg, Path::new("/w/a")).expect("first ok");
        let first = std::fs::read_to_string(&cfg).unwrap();
        ensure_project_trusted_at(&cfg, Path::new("/w/a")).expect("second ok");
        assert_eq!(first, std::fs::read_to_string(&cfg).unwrap(), "byte-identical");
    }

    #[test]
    fn operator_untrusted_choice_is_never_overridden() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("config.toml");
        std::fs::write(
            &cfg,
            "[projects.\"/w/sus\"]\ntrust_level = \"untrusted\"\n",
        )
        .unwrap();
        ensure_project_trusted_at(&cfg, Path::new("/w/sus")).expect("ok (skip)");
        let v = parse(&cfg);
        assert_eq!(trust_of(&v, "/w/sus"), Some("untrusted"), "left as the operator set it");
    }

    #[test]
    fn malformed_file_is_left_untouched() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("config.toml");
        let garbage = "model = [unclosed\n";
        std::fs::write(&cfg, garbage).unwrap();
        let err = ensure_project_trusted_at(&cfg, Path::new("/w")).expect_err("must skip");
        assert!(err.contains("does not parse"), "err: {err}");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), garbage);
    }

    #[test]
    fn key_with_quotes_is_escaped_and_reparses() {
        let dir = TempDir::new().unwrap();
        let cfg = dir.path().join("config.toml");
        ensure_project_trusted_at(&cfg, Path::new("/w/we\"ird")).expect("ok");
        let v = parse(&cfg);
        assert_eq!(trust_of(&v, "/w/we\"ird"), Some("trusted"));
    }

    #[test]
    fn trust_keys_include_main_checkout_for_worktree() {
        let dir = TempDir::new().unwrap();
        let main = dir.path().join("main-repo");
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(main.join(".git/worktrees/wt")).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()),
        )
        .unwrap();

        let keys = trust_keys_for(&wt);
        assert_eq!(keys[0], wt);
        assert_eq!(keys[1], main, "worktree resolves its main checkout: {keys:?}");
    }

    #[test]
    fn trust_keys_plain_dir_is_just_itself() {
        let dir = TempDir::new().unwrap();
        let keys = trust_keys_for(dir.path());
        assert_eq!(keys, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn trust_keys_regular_git_dir_is_just_itself() {
        // A normal checkout has a .git DIRECTORY — no second key.
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        let keys = trust_keys_for(dir.path());
        assert_eq!(keys, vec![dir.path().to_path_buf()]);
    }

    #[test]
    fn gating_codex_only() {
        // Hermetic: point CODEX_HOME at a tempdir (avoids process-global
        // HOME mutation; codex_config_path honors CODEX_HOME first).
        let _g = crate::test_support::env_lock();
        let home = TempDir::new().unwrap();
        struct Guard(Option<std::ffi::OsString>);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    match self.0.take() {
                        Some(v) => std::env::set_var("CODEX_HOME", v),
                        None => std::env::remove_var("CODEX_HOME"),
                    }
                }
            }
        }
        let guard = Guard(std::env::var_os("CODEX_HOME"));
        let _ = &guard;
        unsafe { std::env::set_var("CODEX_HOME", home.path()) };
        let cfg = home.path().join("config.toml");
        let wd = home.path().join("wd");
        std::fs::create_dir_all(&wd).unwrap();

        maybe_pretrust_for_spawn("codex", &[], Some(&wd));
        assert!(cfg.exists(), "codex spawn must write the trust entry");

        std::fs::remove_file(&cfg).unwrap();
        maybe_pretrust_for_spawn("claude", &[], Some(&wd));
        maybe_pretrust_for_spawn("/bin/bash", &[], Some(&wd));
        assert!(!cfg.exists(), "claude/bash spawns must not touch codex config");

        // systemd-run wrapped codex unwraps.
        let args: Vec<String> = vec!["--scope".into(), "--".into(), "codex".into()];
        maybe_pretrust_for_spawn("systemd-run", &args, Some(&wd));
        assert!(cfg.exists(), "capped codex under systemd-run must be pre-trusted");
    }
}
