use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

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
        let attempts: [&[&str]; 4] = [
            // Create new branch from origin/<start>.
            &["-b", &branch_name, &origin_ref],
            // Maybe the branch exists locally already, try that.
            &["-b", &branch_name, start],
            // cm/<slug> already exists (leftover from a prior task with
            // the same slug whose worktree is gone) — attach to it, same
            // as the no-start_branch path below. Without this, both -b
            // attempts fail with "branch already exists" and the <start>
            // last resort fails too whenever <start> is checked out in
            // the main repo (main usually is).
            &[&branch_name],
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

/// True if `worktree_path` has uncommitted or untracked changes (a
/// non-empty `git status --porcelain`).
///
/// Used to guard subtask teardown: `remove_worktree` runs `git worktree
/// remove --force`, which silently destroys a dirty working tree, so
/// `mark_subtask_done` refuses (absent `force=true`) when this returns
/// true. Only the WORKING TREE is at risk — committed work lives on the
/// branch ref, which the remove preserves — so an uncommitted/untracked
/// change is exactly what we protect.
///
/// A missing worktree dir is treated as clean (`Ok(false)`): there's
/// nothing to lose, and `remove_worktree` already handles the
/// already-gone case idempotently, so the guard must not block it.
pub fn worktree_is_dirty(worktree_path: &Path) -> Result<bool, String> {
    if !worktree_path.exists() {
        return Ok(false);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|e| format!("spawn git status: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "git status failed in {}: {}",
            worktree_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
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

/// An allowlist entry the daemon may clone (borrowed view of
/// `config::RepoAllowEntry`, so this module stays free of a config
/// dependency).
pub struct RepoAllow<'a> {
    pub name: &'a str,
    pub url: &'a str,
}

/// Why `resolve_repo` couldn't produce a local checkout.
#[derive(Debug)]
pub enum RepoResolveError {
    /// Not present on disk and cloning isn't permitted — the URL is not
    /// in the allowlist and `allow_clone` is false. Carries the repo
    /// name. (Phase 2 default-deny: cloning runs code-fetch on the host.)
    NotPermitted(String),
    /// Cloning was permitted and attempted, but `git clone` failed.
    /// Carries the repo name + git's stderr.
    CloneFailed { repo: String, detail: String },
}

/// Serializes first-resolve clones in [`resolve_repo`] so two concurrent
/// `create_session` calls for the same not-yet-cloned URL can't both
/// `git clone` into the same target (which would let the loser's cleanup
/// delete the winner's checkout). Coarse on purpose — see the lock-acquire
/// site for the invariant. The reuse fast path stays outside the lock, so
/// only genuine first-resolves ever contend.
static CLONE_LOCK: Mutex<()> = Mutex::new(());

/// Resolve a repo shortname/URL to a local checkout, cloning on demand
/// when permitted (Phase 2, remote-session-execution).
///
/// Resolution order:
///   1. `find_local_repo` fast path — `~/code/projects/<name>` + cwd
///      (today's behavior). A repo already on disk is used as-is.
///   2. Reuse a prior clone at `repos_dir/<name>`. Checked independently
///      of `find_local_repo`'s hardcoded paths, so the no-re-clone
///      guarantee holds even when `repos_dir` isn't one of them.
///   3. On a miss, cloning is permitted iff the request matches an
///      allowlist entry (by `name` or `url`) OR `allow_clone` is true.
///      If permitted, `git clone <url> <repos_dir>/<name>` then return
///      that path. Otherwise `NotPermitted` — no clone is attempted.
///
/// With an empty allowlist and `allow_clone == false` (no repos config),
/// this is exactly `find_local_repo` + a NotPermitted on miss — i.e.
/// today's behavior, never cloning.
pub fn resolve_repo(
    repo_url: &str,
    repos_dir: &Path,
    allow_clone: bool,
    allowlist: &[RepoAllow],
) -> Result<PathBuf, RepoResolveError> {
    let name = repo_name(repo_url);

    // Safety gate FIRST, before any filesystem access. `repo_name` can
    // derive an unsafe component (empty, ".", "..", or one containing a
    // path separator) from a malformed URL — e.g. one ending in `/..`.
    // Then `repos_dir.join(name)` escapes repos_dir (".." → its parent,
    // `~/.cm` by default), and the failure-cleanup `remove_dir_all` below
    // could delete it. It would also let `find_local_repo` match an
    // unintended dir. A `name` MUST be a single safe path component.
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
    {
        return Err(RepoResolveError::CloneFailed {
            repo: name.clone(),
            detail: format!(
                "refusing to resolve unsafe repo name '{}' derived from URL '{}' \
                 — must be a single safe path component",
                name, repo_url
            ),
        });
    }

    // 1. Local fast path.
    if let Some(p) = find_local_repo(repo_url) {
        return Ok(p);
    }

    // 2. Reuse a prior clone (fast path, NO lock). A `.git` under
    // repos_dir/<name> means a previous resolve already cloned it — never
    // re-clone (criterion #1). Already-cloned repos thus never contend on
    // the clone lock below.
    let target = repos_dir.join(&name);
    if target.join(".git").exists() {
        return Ok(target);
    }

    // Serialize first-resolve clones. Two concurrent create_session calls
    // for the same not-yet-cloned URL run on separate daemon handler
    // threads (the state lock is released before resolve_repo); without
    // this, both would pass the reuse check above, both `git clone` into
    // `target`, and the loser's clone would fail on the now-non-empty dir
    // and its cleanup could delete the winner's fresh checkout. A single
    // coarse lock is fine: clones happen only on first-resolve (rare,
    // I/O-bound) and the common reuse path above stays outside it.
    // Poison is recovered so one panicking clone can't wedge all future
    // clones.
    let _clone_guard = CLONE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // Double-checked reuse: a concurrent winner may have finished cloning
    // into `target` while we waited for the lock. If so, reuse it — no
    // second clone. (Holding the lock, the only states are: `.git` present
    // → winner finished, reuse; or `target` absent / a non-clone leftover
    // → proceed. A concurrent clone can't be mid-flight here.)
    if target.join(".git").exists() {
        return Ok(target);
    }

    // 3. Permission gate. An allowlist entry matches by name or URL (or
    // by the URL's derived shortname, so a shortname request resolves an
    // entry keyed by full URL). When matched, clone the entry's URL;
    // under open `allow_clone`, clone the requested URL verbatim.
    let allow_match = allowlist.iter().find(|e| {
        e.name == name || e.url == repo_url || repo_name(e.url) == name
    });
    let clone_url: String = match allow_match {
        Some(e) => e.url.to_string(),
        None if allow_clone => repo_url.to_string(),
        None => return Err(RepoResolveError::NotPermitted(name)),
    };

    // 4. Clone into repos_dir/<name>.
    if let Err(e) = std::fs::create_dir_all(repos_dir) {
        return Err(RepoResolveError::CloneFailed {
            repo: name,
            detail: format!("create repos_dir {}: {}", repos_dir.display(), e),
        });
    }
    // Capture whether `target` already existed BEFORE our clone, so the
    // failure-cleanup only ever removes a directory THIS call created.
    // Same-URL race: two threads can both pass the step-2 reuse check and
    // both `git clone` into `target`; the loser's clone fails on the
    // now-non-empty dir and must NOT delete the winner's fresh checkout
    // out from under its in-flight worktree build.
    let pre_existed = target.exists();
    let out = Command::new("git")
        .args(["clone", &clone_url])
        .arg(&target)
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            // git failed to spawn — no clone dir was created by us.
            return Err(RepoResolveError::CloneFailed {
                repo: name,
                detail: format!("spawn git clone: {}", e),
            });
        }
    };
    if !out.status.success() || !target.join(".git").exists() {
        // Clean up a partial clone so a retry isn't blocked and the reuse
        // check (step 2) can't latch onto a broken checkout — but ONLY if
        // this call created `target` (`!pre_existed`). Never delete a
        // pre-existing dir (a concurrent winner's clone, or a leftover).
        if !pre_existed && target.exists() {
            let _ = std::fs::remove_dir_all(&target);
        }
        return Err(RepoResolveError::CloneFailed {
            repo: name,
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(target)
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

    // === Phase 2: resolve_repo (registry + clone-on-demand) ===

    /// Build a real git repo (one commit) at `path` so `git clone` has a
    /// source. Test helper.
    fn make_git_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(path.join("README.md"), "x").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
    }

    /// Criterion: an allowlisted URL absent from disk clones into
    /// `repos_dir/<name>` then resolves; a second resolve reuses it (no
    /// re-clone). The sentinel proves reuse — a re-clone would `git clone`
    /// into a non-empty dir and fail (or wipe the sentinel).
    #[test]
    fn resolve_repo_clones_allowlisted_url_then_reuses() {
        with_home(|_home| {
            let src = tempfile::tempdir().unwrap();
            let src_repo = src.path().join("myrepo");
            make_git_repo(&src_repo);
            let src_url = src_repo.to_string_lossy().into_owned();

            let repos = tempfile::tempdir().unwrap();
            let repos_dir = repos.path();
            let allow = [RepoAllow { name: "myrepo", url: &src_url }];

            // First resolve: not on disk → allowlisted → clone.
            let got = resolve_repo("myrepo", repos_dir, false, &allow)
                .expect("allowlisted repo clones");
            assert_eq!(got, repos_dir.join("myrepo"));
            assert!(got.join(".git").exists(), "clone produced a real checkout");

            // Sentinel survives the second resolve iff it reuses.
            let sentinel = got.join("SENTINEL");
            std::fs::write(&sentinel, "keep").unwrap();

            let got2 = resolve_repo("myrepo", repos_dir, false, &allow)
                .expect("second resolve reuses");
            assert_eq!(got2, repos_dir.join("myrepo"));
            assert!(
                sentinel.exists(),
                "second resolve must reuse the clone, not re-clone"
            );
        });
    }

    /// Criterion: a non-allowlisted URL with `allow_clone=false` returns
    /// NotPermitted WITHOUT cloning.
    #[test]
    fn resolve_repo_not_permitted_does_not_clone() {
        with_home(|_home| {
            let repos = tempfile::tempdir().unwrap();
            let repos_dir = repos.path();
            let err = resolve_repo(
                "https://github.com/u/ghostrepo.git",
                repos_dir,
                false,
                &[],
            )
            .expect_err("not permitted");
            match err {
                RepoResolveError::NotPermitted(name) => assert_eq!(name, "ghostrepo"),
                other => panic!("expected NotPermitted, got {:?}", other),
            }
            assert!(
                !repos_dir.join("ghostrepo").exists(),
                "must not attempt a clone when not permitted"
            );
        });
    }

    /// Criterion: no repos config (empty allowlist, `allow_clone=false`)
    /// → find_local_repo only. A repo present in ~/code/projects resolves
    /// via the fast path and nothing is cloned.
    #[test]
    fn resolve_repo_uses_find_local_repo_fast_path() {
        with_home(|home| {
            let local = home.join("code/projects/foundrepo");
            make_git_repo(&local);
            let repos = tempfile::tempdir().unwrap();
            let got = resolve_repo("foundrepo", repos.path(), false, &[])
                .expect("resolved via find_local_repo");
            assert_eq!(got, local);
            assert!(
                !repos.path().join("foundrepo").exists(),
                "fast path must not clone"
            );
        });
    }

    /// `allow_clone=true` clones a non-allowlisted URL (the open-cloning
    /// flag).
    #[test]
    fn resolve_repo_allow_clone_open_clones_unlisted_url() {
        with_home(|_home| {
            let src = tempfile::tempdir().unwrap();
            let src_repo = src.path().join("openrepo");
            make_git_repo(&src_repo);
            let src_url = src_repo.to_string_lossy().into_owned();
            let repos = tempfile::tempdir().unwrap();
            let got = resolve_repo(&src_url, repos.path(), true, &[])
                .expect("open clone");
            assert_eq!(got, repos.path().join("openrepo"));
            assert!(got.join(".git").exists());
        });
    }

    /// A permitted-but-failing clone returns CloneFailed and cleans up the
    /// partial target (so a retry isn't blocked and the reuse check can't
    /// latch a broken checkout).
    #[test]
    fn resolve_repo_clone_failure_returns_error_and_cleans_up() {
        with_home(|_home| {
            let repos = tempfile::tempdir().unwrap();
            let err = resolve_repo(
                "/nonexistent/path/to/brokenrepo",
                repos.path(),
                true,
                &[],
            )
            .expect_err("clone of a missing source fails");
            match err {
                RepoResolveError::CloneFailed { repo, .. } => {
                    assert_eq!(repo, "brokenrepo")
                }
                other => panic!("expected CloneFailed, got {:?}", other),
            }
            assert!(
                !repos.path().join("brokenrepo").exists(),
                "partial clone must be cleaned up"
            );
        });
    }

    /// Safety: a URL whose derived name is unsafe (here `..`, from a URL
    /// ending in `/..`) is rejected BEFORE any filesystem op — so the
    /// failure-cleanup can never `remove_dir_all` an escaped path
    /// (`repos_dir/..` = its parent). A canary file in repos_dir's parent
    /// must survive, and repos_dir itself must remain.
    #[test]
    fn resolve_repo_unsafe_name_is_rejected_without_touching_parent() {
        with_home(|_home| {
            let root = tempfile::tempdir().unwrap();
            let repos_dir = root.path().join("repos");
            std::fs::create_dir_all(&repos_dir).unwrap();
            // Sibling of repos_dir == repos_dir/.. (the dir the unsafe
            // name would resolve to and the cleanup would delete).
            let canary = root.path().join("CANARY");
            std::fs::write(&canary, "do not delete").unwrap();

            // URL ending in `/..` → repo_name() == "..".
            let err = resolve_repo(
                "https://github.com/u/..",
                &repos_dir,
                true, // even with cloning enabled, the name check blocks first
                &[],
            )
            .expect_err("unsafe derived name must error");
            match err {
                RepoResolveError::CloneFailed { repo, detail } => {
                    assert_eq!(repo, "..");
                    assert!(
                        detail.contains("unsafe repo name"),
                        "detail must name the cause: {}",
                        detail
                    );
                }
                other => panic!("expected CloneFailed(unsafe name), got {:?}", other),
            }
            assert!(
                canary.exists(),
                "an unsafe name must NEVER let cleanup escape repos_dir"
            );
            assert!(repos_dir.exists(), "repos_dir itself must survive");
        });
    }

    /// Safety: when the clone fails but `target` PRE-EXISTED (e.g. a
    /// concurrent winner's checkout), the failure-cleanup must NOT delete
    /// it. A sentinel inside the pre-existing target must survive.
    #[test]
    fn resolve_repo_clone_failure_leaves_preexisting_target_untouched() {
        with_home(|_home| {
            let src = tempfile::tempdir().unwrap();
            let src_repo = src.path().join("racerepo");
            make_git_repo(&src_repo);
            let src_url = src_repo.to_string_lossy().into_owned();

            let repos = tempfile::tempdir().unwrap();
            // Pre-create target as a NON-empty, non-git dir (stands in for
            // a concurrent winner's in-progress / different checkout). git
            // clone into it fails ("destination exists / not empty").
            let target = repos.path().join("racerepo");
            std::fs::create_dir_all(&target).unwrap();
            let sentinel = target.join("WINNER");
            std::fs::write(&sentinel, "winner's work").unwrap();

            let err = resolve_repo(&src_url, repos.path(), true, &[])
                .expect_err("clone into a non-empty pre-existing target fails");
            assert!(
                matches!(err, RepoResolveError::CloneFailed { .. }),
                "expected CloneFailed, got {:?}",
                err
            );
            assert!(
                sentinel.exists(),
                "failure-cleanup must NOT delete a pre-existing target \
                 (a concurrent winner's checkout)"
            );
        });
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            status.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&status.stderr)
        );
    }

    /// Regression: launching a task whose `cm/<slug>` branch survives a
    /// prior run (worktree dir gone, branch left behind) with a
    /// `start_branch` must attach to that branch instead of failing.
    /// Pre-fix, the start_branch path had no attach variant: both `-b`
    /// attempts died on "branch already exists" and the `<start>` last
    /// resort died on "already checked out" (main is checked out in the
    /// main repo), surfacing the three-variant error this test pins.
    #[test]
    fn create_worktree_start_branch_attaches_to_leftover_slug_branch() {
        with_home(|_home| {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("myrepo");
            make_git_repo(&repo);
            // Deterministic default-branch name, left checked out.
            git(&repo, &["branch", "-M", "main"]);
            // Leftover branch from a prior task with the same slug.
            git(&repo, &["branch", "cm/fix-start-session"]);

            let (path, created) =
                create_worktree(&repo, "fix-start-session", Some("main"))
                    .expect("must attach to the existing cm/<slug> branch");
            assert!(created, "fresh worktree dir → created == true");
            assert_eq!(
                worktree_current_branch(&path).as_deref(),
                Some("cm/fix-start-session")
            );
        });
    }

    #[test]
    fn worktree_is_dirty_tracks_working_tree_state() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "t@t"]);
        git(repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "hi").unwrap();
        git(repo, &["add", "a.txt"]);
        git(repo, &["commit", "-q", "-m", "init"]);

        // Committed + clean → not dirty.
        assert_eq!(worktree_is_dirty(repo), Ok(false));

        // Untracked file → dirty (this is exactly what --force would nuke).
        std::fs::write(repo.join("scratch.txt"), "wip").unwrap();
        assert_eq!(worktree_is_dirty(repo), Ok(true));

        // Stage + commit it → clean again.
        git(repo, &["add", "scratch.txt"]);
        git(repo, &["commit", "-q", "-m", "wip"]);
        assert_eq!(worktree_is_dirty(repo), Ok(false));

        // Uncommitted modification → dirty.
        std::fs::write(repo.join("a.txt"), "changed").unwrap();
        assert_eq!(worktree_is_dirty(repo), Ok(true));

        // A path that doesn't exist is treated as clean (nothing to lose),
        // so the teardown guard never blocks an already-gone worktree.
        assert_eq!(worktree_is_dirty(&repo.join("nope")), Ok(false));
    }
}
