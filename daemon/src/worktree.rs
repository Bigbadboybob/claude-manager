use std::path::{Path, PathBuf};
use std::process::Command;

/// Base directory for all worktrees.
fn worktree_base() -> PathBuf {
    dirs::home_dir()
        .expect("HOME unset; cannot locate worktree base")
        .join(".cm/worktrees")
}

/// Convert a task name into a branch-safe slug.
pub fn slugify(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
        .chars()
        .take(40)
        .collect()
}

/// Extract repo name from a URL like "https://github.com/user/repo.git".
fn repo_name(repo_url: &str) -> String {
    repo_url
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .to_string()
}

/// Render a list of per-attempt stderrs into a single bail message so
/// the caller sees every variant's failure, not just the last one.
fn fmt_attempt_failures(errors: &[String]) -> String {
    let mut msg = format!(
        "git worktree add failed; tried {} variants:",
        errors.len()
    );
    for (i, e) in errors.iter().enumerate() {
        msg.push_str(&format!("\n  {}) {}", i + 1, e));
    }
    msg
}

/// Run `git -C <main_repo> worktree add <worktree_path> <args...>` for
/// each attempt in turn. Returns `Ok(())` on the first success;
/// otherwise bails with every attempt's stderr concatenated.
fn try_worktree_add_attempts(
    main_repo: &Path,
    worktree_path: &Path,
    attempts: &[&[&str]],
) -> anyhow::Result<()> {
    let mut errors: Vec<String> = Vec::new();
    for args in attempts {
        let out = Command::new("git")
            .arg("-C")
            .arg(main_repo)
            .args(["worktree", "add"])
            .arg(worktree_path)
            .args(*args)
            .output()?;
        if out.status.success() {
            return Ok(());
        }
        errors.push(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    anyhow::bail!("{}", fmt_attempt_failures(&errors))
}

/// Create a git worktree for a task.
///
/// If `start_branch` is provided, the worktree starts from that branch
/// (fetched from origin first). Otherwise creates a new `cm/<slug>` branch from HEAD.
///
/// Returns `(worktree_path, created)`. `created` is `true` when this call
/// freshly created the worktree, and `false` when it reused a pre-existing
/// valid worktree already on the matching `cm/<slug>` branch (a slug/dir
/// collision). Callers that clean up on a later failure MUST NOT remove a
/// reused (`created == false`) worktree — it may contain work this call
/// didn't create.
pub fn create_worktree(
    main_repo: &Path,
    task_slug: &str,
    start_branch: Option<&str>,
) -> anyhow::Result<(PathBuf, bool)> {
    let base = worktree_base();
    std::fs::create_dir_all(&base)?;

    let dir_name = format!(
        "{}-{}",
        main_repo
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo"),
        task_slug
    );
    let worktree_path = base.join(&dir_name);
    let branch_name = format!("cm/{}", task_slug);

    if worktree_path.exists() {
        // Validate the existing dir is a git worktree on the expected
        // branch. A stale dir or slug collision would otherwise
        // silently attach this task to an unrelated checkout.
        let inside = Command::new("git")
            .arg("-C")
            .arg(&worktree_path)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false);
        if !inside {
            anyhow::bail!(
                "worktree path {} exists but is not a git worktree",
                worktree_path.display()
            );
        }
        return match worktree_current_branch(&worktree_path) {
            Some(b) if b == branch_name => Ok((worktree_path, false)),
            Some(b) => anyhow::bail!(
                "worktree path {} exists but is on branch {} (expected {})",
                worktree_path.display(),
                b,
                branch_name
            ),
            None => anyhow::bail!(
                "worktree path {} exists but its branch could not be determined",
                worktree_path.display()
            ),
        };
    }

    let add_result = if let Some(start) = start_branch {
        // Fetch the branch first; OK if it fails (offline / no remote).
        let _ = Command::new("git")
            .arg("-C")
            .arg(main_repo)
            .args(["fetch", "origin", start])
            .output();

        let origin_ref = format!("origin/{}", start);
        let attempts: [&[&str]; 3] = [
            // Create new branch from origin/<start>.
            &["-b", &branch_name, &origin_ref],
            // Maybe the branch exists locally already, try that.
            &["-b", &branch_name, start],
            // Last resort: just check out <start> directly.
            &[start],
        ];
        try_worktree_add_attempts(main_repo, &worktree_path, &attempts)
    } else {
        let attempts: [&[&str]; 2] = [
            // Create new branch from HEAD.
            &["-b", &branch_name],
            // Branch already exists, just attach a worktree to it.
            &[&branch_name],
        ];
        try_worktree_add_attempts(main_repo, &worktree_path, &attempts)
    };

    // On failure, clean up any partial worktree git may have left behind
    // (a half-created dir / stale admin record) so a retry with the same
    // slug isn't blocked, and a caller like the daemon's `create_session`
    // is never handed an orphaned worktree to clean up itself. Best-effort:
    // the original add error is what we surface.
    if let Err(e) = add_result {
        if worktree_path.exists() {
            let _ = std::fs::remove_dir_all(&worktree_path);
        }
        let _ = Command::new("git")
            .arg("-C")
            .arg(main_repo)
            .args(["worktree", "prune"])
            .output();
        return Err(e);
    }

    Ok((worktree_path, true))
}

/// Create a worktree for a subtask. Differs from `create_worktree` in:
///   - branch name is fully specified by the caller (e.g.
///     `cm-sub/<slug-chain>-<short_id>` per AGENT_ORCHESTRATION.md), not
///     derived from the slug. The flat-with-id form sidesteps the git
///     ref-prefix collision that hierarchical names hit.
///   - dir name under `~/.cm/worktrees/` is derived from the branch
///     name with `/` mapped to `-` so it's a valid path component.
///   - `start_branch` is the parent's wip_branch; the new branch is
///     created off it (with origin fetch as the first attempt).
///
/// Returns the path to the new worktree directory.
pub fn create_subtask_worktree(
    main_repo: &Path,
    branch_name: &str,
    start_branch: &str,
) -> anyhow::Result<PathBuf> {
    let base = worktree_base();
    std::fs::create_dir_all(&base)?;

    // Worktree dir name: replace `/` with `-` so the branch
    // `cm-sub/foo-bar-abc1234` produces `cm-sub-foo-bar-abc1234`.
    let dir_name = branch_name.replace('/', "-");
    let worktree_path = base.join(&dir_name);

    if worktree_path.exists() {
        return Ok(worktree_path);
    }

    // Order matters: try the LOCAL ref first, then origin/<start>.
    //
    // Local-first is the right default for worktree-based dev: in
    // local mode users typically have commits on the parent branch
    // that are ahead of (or never destined for) `origin/<start>`. If
    // we forked from `origin/<start>` first, the subtask would
    // silently miss the parent's local-only work — exactly the
    // class of bug worktree-mode subtasks exist to support.
    //
    // We still fall back to `origin/<start>` to handle the case
    // where the parent branch only exists on the remote (e.g. fresh
    // clone, branch was pushed by a collaborator). We fetch first
    // in that case to maximize the chance of resolving it.
    let attempt_local = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["worktree", "add"])
        .arg(&worktree_path)
        .args(["-b", branch_name, start_branch])
        .output()?;
    if attempt_local.status.success() {
        return Ok(worktree_path);
    }

    // Local ref didn't resolve — try origin. Fetch first; ignore
    // failure (offline / no remote / branch genuinely doesn't exist
    // on origin). If the fetch fails we still try the add — git
    // may still have a stale `origin/<start>` from a prior fetch.
    let _ = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["fetch", "origin", start_branch])
        .output();
    let attempt_remote = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["worktree", "add"])
        .arg(&worktree_path)
        .args(["-b", branch_name, &format!("origin/{}", start_branch)])
        .output()?;
    if attempt_remote.status.success() {
        return Ok(worktree_path);
    }

    // Both attempts failed. Surface the local-attempt stderr — that's
    // the path the user expected to work in local-dev mode, and its
    // error message ("invalid reference", "not a valid object name")
    // is the actionable diagnostic. The remote failure is usually a
    // downstream symptom of the same root cause.
    let stderr = String::from_utf8_lossy(&attempt_local.stderr);
    anyhow::bail!(
        "git worktree add failed for subtask branch {}: {}",
        branch_name,
        stderr.trim()
    );
}

/// Reconstruct the on-disk worktree path for a CM-managed branch, mirroring
/// the naming used by `create_worktree` / `create_subtask_worktree`. Returns
/// `Some(path)` only if the path actually exists — used by reconcile recovery
/// to re-bind a task to its workspace after a TUI crash drops the manifest
/// binding.
///
/// Two layouts, both flat under `~/.cm/worktrees/`:
///   - `cm/<slug>` → `<repo-name>-<slug>` (the original launch flow).
///   - `cm-sub/<chain>-<short>` → `cm-sub-<chain>-<short>` (subtasks; `/`
///     replaced with `-` so it's a valid path component, see
///     `create_subtask_worktree`). The repo name isn't part of the dir
///     name here because the branch already encodes parentage.
///
/// Anything else returns `None`. The caller should treat that as "not a
/// CM-managed local worktree" and skip recovery.
pub fn recover_worktree_path(repo_url: &str, branch: &str) -> Option<PathBuf> {
    let base = worktree_base();
    let candidate = if let Some(slug) = branch.strip_prefix("cm/") {
        let name = repo_name(repo_url);
        base.join(format!("{}-{}", name, slug))
    } else if branch.starts_with("cm-sub/") {
        // Same `/` → `-` mapping as `create_subtask_worktree`.
        base.join(branch.replace('/', "-"))
    } else {
        return None;
    };
    candidate.exists().then_some(candidate)
}

/// Read the current branch of a worktree via `git rev-parse --abbrev-ref HEAD`.
/// Returns `None` for detached HEAD or any git failure. Used by
/// `create_subtask` as the fallback start ref when the parent task's
/// `wip_branch` field is `None` (common for tasks launched into an
/// existing workspace) — branching from "main" in that case would
/// silently drop the parent's actual work.
pub fn worktree_current_branch(worktree_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8(output.stdout).ok()?.trim().to_string();
    // `HEAD` literal == detached HEAD; treat as no usable branch.
    if name.is_empty() || name == "HEAD" {
        return None;
    }
    Some(name)
}

/// Run setup_worktree.sh if it exists in the main repo, otherwise do nothing.
///
/// The script receives MAIN_REPO and WORKTREE as environment variables.
pub fn setup_worktree(main_repo: &Path, worktree_path: &Path) {
    let script = main_repo.join("setup_worktree.sh");
    if !script.exists() {
        return;
    }

    let _ = Command::new("bash")
        .arg(&script)
        .env("MAIN_REPO", main_repo)
        .env("WORKTREE", worktree_path)
        .current_dir(worktree_path)
        .output();
}

/// Remove a git worktree. Returns `Ok(())` on success, `Err` with
/// stderr if the git command failed or its output couldn't be read.
/// Callers MUST check the result before clearing local state that
/// depends on the worktree path — without this, a failed remove plus
/// a wiped `worktree_path` leaves the manifest unable to find the
/// worktree to retry.
///
/// Idempotent: if `worktree_path` doesn't exist on disk we treat the
/// worktree as already gone, run `git worktree prune` to drop any
/// stale admin records, and return `Ok(())`. Without this, A-x on a
/// workspace whose directory was deleted out-of-band gets stuck —
/// git errors with "is not a working tree" and the manifest entry
/// never gets cleaned up.
pub fn remove_worktree(
    main_repo: &Path,
    worktree_path: &Path,
) -> Result<(), String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output()
        .map_err(|e| format!("spawn git worktree remove: {}", e))?;
    if output.status.success() {
        return Ok(());
    }
    if !worktree_path.exists() {
        let _ = Command::new("git")
            .arg("-C")
            .arg(main_repo)
            .args(["worktree", "prune"])
            .output();
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(stderr.trim().to_string())
}

/// Resolve a repo shortname or URL to a local path.
///
/// Checks ~/code/projects/<name> and the current directory.
pub fn find_local_repo(repo_url: &str) -> Option<PathBuf> {
    let name = repo_name(repo_url);

    // Check ~/code/projects/<name>
    if let Some(home) = dirs::home_dir() {
        let path = home.join("code/projects").join(&name);
        if path.join(".git").exists() {
            return Some(path);
        }
    }

    // Check current directory
    if let Ok(cwd) = std::env::current_dir() {
        let path = cwd.join(&name);
        if path.join(".git").exists() {
            return Some(path);
        }
        // Maybe we're already in the repo
        if cwd.file_name().and_then(|n| n.to_str()) == Some(&name) && cwd.join(".git").exists() {
            return Some(cwd);
        }
    }

    None
}

mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    //! Pins down the layout that `recover_worktree_path` is expected
    //! to find. Pre-fix, the reconcile recovery only knew the
    //! `<repo>-<slug>` layout produced by `create_worktree`, so a
    //! reload after a manifest loss left subtask tasks
    //! (`cm-sub/<chain>-<short>` branches) without a workspace
    //! binding — `start_session`, workflow launch, and cleanup all
    //! couldn't find them.
    use super::*;

    /// `recover_worktree_path` reads `$HOME` via the in-module `dirs`
    /// helper. We MUST share the crate-wide env mutex
    /// (`test_support::env_lock`) so HOME-mutating tests in this
    /// module don't race against the env-var / umask tests in
    /// `lib.rs`. A module-local mutex would only serialize
    /// within-file. Reviewer caught the cross-module gap when this
    /// crate first split out of the TUI; one mutex covers everything
    /// that touches process-global state.
    fn with_home<F: FnOnce(&Path)>(f: F) {
        let _g = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());
        f(tmp.path());
        if let Some(p) = prev {
            std::env::set_var("HOME", p);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn cm_branch_resolves_to_repo_slug_layout() {
        with_home(|home| {
            // Layout written by create_worktree: <repo>-<slug>.
            let dir = home.join(".cm/worktrees/myrepo-fix-bug");
            std::fs::create_dir_all(&dir).unwrap();

            let got = recover_worktree_path(
                "https://github.com/u/myrepo.git",
                "cm/fix-bug",
            );
            assert_eq!(got, Some(dir));
        });
    }

    #[test]
    fn cm_sub_branch_resolves_to_flat_dash_layout() {
        with_home(|home| {
            // Layout written by create_subtask_worktree: branch with
            // `/` replaced by `-`, no repo-name prefix.
            let dir = home.join(".cm/worktrees/cm-sub-parent-child-abc1234");
            std::fs::create_dir_all(&dir).unwrap();

            let got = recover_worktree_path(
                "https://github.com/u/anyrepo.git",
                "cm-sub/parent-child-abc1234",
            );
            assert_eq!(got, Some(dir));
        });
    }

    #[test]
    fn missing_path_returns_none() {
        with_home(|_home| {
            // The branch matches the convention but the dir doesn't
            // exist on disk — caller must not treat that as a hit
            // (otherwise reconcile would re-bind to a phantom path).
            let got = recover_worktree_path(
                "https://github.com/u/r.git",
                "cm-sub/never-built-deadbeef",
            );
            assert_eq!(got, None);
        });
    }

    #[test]
    fn unknown_prefix_returns_none() {
        with_home(|home| {
            // Even if a path happens to exist, a non-CM-managed
            // branch name (e.g. `main`, `feature/x`) must not be
            // treated as recoverable.
            let dir = home.join(".cm/worktrees/main");
            std::fs::create_dir_all(&dir).unwrap();

            assert_eq!(recover_worktree_path("r", "main"), None);
            assert_eq!(recover_worktree_path("r", "feature/x"), None);
        });
    }
}
