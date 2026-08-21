use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Base directory for all worktrees.
fn worktree_base() -> PathBuf {
    dirs::home_dir()
        .expect("HOME unset; cannot locate worktree base")
        .join(".cm/worktrees")
}

/// `[scheduler] max_worktrees` disk guard, wired in at daemon startup via
/// [`set_max_worktrees`]. `0` = unguarded (the config-family "0 disables"
/// convention; also the default so library users — the TUI, tests — are
/// unaffected unless they opt in).
///
/// Enforced at worktree CREATION, inside [`create_worktree`] and
/// [`create_subtask_worktree`] after their reuse fast-paths — so every daemon
/// mint route (`continuous.create`, `create_subtask` branch mode,
/// `mint_task_worktree` for `mcp_start_session`) hits the guard, and reusing
/// an existing checkout never does. Before this existed the field was parsed
/// but enforced nowhere; 148 worktrees filled cm-manager's disk to 99% on
/// 2026-08-17 while the operator believed a guard was in place.
static MAX_WORKTREES: AtomicU32 = AtomicU32::new(0);

/// Install the worktree-count ceiling (from `[scheduler] max_worktrees`).
/// `None` or `Some(0)` = unguarded.
pub fn set_max_worktrees(limit: Option<u32>) {
    MAX_WORKTREES.store(limit.unwrap_or(0), Ordering::SeqCst);
}

/// Number of live worktree directories under `~/.cm/worktrees`.
pub fn count_worktrees() -> usize {
    std::fs::read_dir(worktree_base())
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .count()
        })
        .unwrap_or(0)
}

/// Refuse a NEW worktree when the guard is armed and the base dir is at
/// capacity. The message is the operator's remediation, since this surfaces
/// mid-`create_subtask` / `continuous.create` where the caller is often an
/// agent relaying it.
fn enforce_worktree_capacity() -> anyhow::Result<()> {
    let max = MAX_WORKTREES.load(Ordering::SeqCst);
    if max == 0 {
        return Ok(());
    }
    let count = count_worktrees();
    if count >= max as usize {
        anyhow::bail!(
            "worktree limit reached: {} live worktrees under {} >= \
             [scheduler] max_worktrees = {} — remove stale worktrees \
             (`git worktree remove <path>` from the main repo; dirty trees \
             refuse, protecting uncommitted work) or raise max_worktrees in \
             daemon.toml",
            count,
            worktree_base().display(),
            max,
        );
    }
    Ok(())
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

    // Disk guard — only a NEW worktree counts against the ceiling (the
    // reuse fast-path above already returned).
    enforce_worktree_capacity()?;

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

/// Where a subtask worktree's new branch is cut from.
///
/// Two shapes because the resolution rules genuinely differ:
///   - `ParentBranch` is a BRANCH NAME the system derived itself (the
///     parent task's `wip_branch` / worktree HEAD). It gets the
///     local-then-`origin/<name>` dance below, because the parent branch
///     may live only on the remote in a fresh clone.
///   - `Base` is a caller-supplied committish (`create_subtask(base=…)`):
///     a sha, tag, `main`, `origin/main`, … It is `rev-parse`-verified to
///     a concrete commit FIRST (see `resolve_base_commit`), so an
///     unresolvable base fails with a precise message instead of a raw
///     `git worktree add` stderr, and there is no `origin/<x>` guessing
///     on top of what the caller literally asked for.
pub enum SubtaskStart<'a> {
    /// The parent task's branch — local ref first, then `origin/<ref>`.
    ParentBranch(&'a str),
    /// An explicit committish supplied by the caller.
    Base(&'a str),
}

/// `git rev-parse --verify --quiet <rev>^{commit}` inside `repo`.
/// `None` when the rev doesn't resolve (or doesn't peel to a commit).
fn rev_parse_commit(repo: &Path, rev: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{}^{{commit}}", rev))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

/// The commit a worktree's HEAD points at. `None` when the path isn't a
/// git checkout or HEAD is unborn. Used by `create_subtask` to report
/// `base_sha` — the exact commit the subtask's checkout sits on.
pub fn worktree_head_sha(worktree_path: &Path) -> Option<String> {
    rev_parse_commit(worktree_path, "HEAD")
}

/// Is `base` shaped like a SINGLE revision, as opposed to a git option,
/// refspec, or glob?
///
/// This matters because `resolve_base_commit` hands the caller's string
/// to `git fetch origin <base>`, where the argument is a REFSPEC, not a
/// rev: `<src>:<dst>` (or `+<src>:<dst>`) makes git WRITE `<dst>` in the
/// operator's main repo, so an unvalidated `base` of
/// `+refs/heads/main:refs/heads/some-branch` would create or
/// force-overwrite local branches there — and the `FETCH_HEAD` fallback
/// would then resolve, so `create_subtask` would report success and the
/// clobber would never surface. `create_subtask`'s `base` is a
/// precondition check with zero side effects, so refspec syntax has to
/// die before it reaches git's argv.
///
/// No legitimate committish needs any of the rejected characters: shas,
/// tags, `main`, `origin/main`, `HEAD~2`, `main^{commit}` and `HEAD@{1}`
/// all pass.
fn is_single_revision(base: &str) -> bool {
    // `-` is an option marker, `+` the refspec force marker; `:`
    // separates a refspec's src from its dst; `?`/`*`/`[` are refspec
    // globs; `\` and whitespace/control bytes are never valid in a ref
    // name (git check-ref-format rejects them).
    !base.starts_with('-')
        && !base.starts_with('+')
        && !base.chars().any(|c| {
            matches!(c, ':' | '?' | '*' | '[' | '\\') || c.is_whitespace() || c.is_control()
        })
}

/// Resolve a caller-supplied `base` committish to a concrete commit sha
/// inside `main_repo`. Accepts anything git accepts — a full or short
/// sha, a tag, a local branch (`main`), or a remote-tracking ref
/// (`origin/main`) — as long as it's a single revision and not refspec
/// or option syntax (see `is_single_revision`).
///
/// Order:
///   1. Local `rev-parse --verify` — covers shas, tags, local branches,
///      and already-fetched `origin/<branch>` refs. Local-first for the
///      same reason `create_subtask_worktree` prefers local refs: the
///      operator's local commits are usually the point.
///   2. If that misses, `git fetch origin <base>` once, then retry the
///      literal ref, then `origin/<base>`, then `FETCH_HEAD` — the last
///      only when THAT fetch succeeded, so a stale FETCH_HEAD left by an
///      unrelated earlier fetch can never silently become the base.
///
/// Errors with `base '<x>' does not resolve to a commit` when nothing
/// resolves; callers surface that message verbatim.
pub fn resolve_base_commit(main_repo: &Path, base: &str) -> anyhow::Result<String> {
    let base = base.trim();
    if base.is_empty() || !is_single_revision(base) {
        anyhow::bail!("base '{}' does not resolve to a commit", base);
    }

    if let Some(sha) = rev_parse_commit(main_repo, base) {
        return Ok(sha);
    }

    let fetched = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["fetch", "origin", base])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let mut candidates = vec![base.to_string(), format!("origin/{}", base)];
    if fetched {
        candidates.push("FETCH_HEAD".to_string());
    }
    for cand in candidates {
        if let Some(sha) = rev_parse_commit(main_repo, &cand) {
            return Ok(sha);
        }
    }

    anyhow::bail!("base '{}' does not resolve to a commit", base)
}

/// Trunk refs `resolve_project_main` tries, in order. Local first
/// (matching `resolve_base_commit`'s local-first rule: the operator's own
/// commits are usually the point), then the remote-tracking mirror.
const PROJECT_MAIN_CANDIDATES: [&str; 2] = ["main", "master"];

/// The commit a workspace-less task's minted worktree is cut from: the
/// project's trunk. Returns `(ref_used, sha)`.
///
/// Resolution order is `main`, `origin/main`, `master`, `origin/master`,
/// all `rev-parse`-local; only if every one misses does it fall through
/// to [`resolve_base_commit`]`(main_repo, "main")`, which spends one
/// `git fetch`. That ordering means the common case (a repo with a local
/// trunk) costs no network at all, and a `master`-trunk repo resolves
/// without the caller having to say so.
///
/// Errors when nothing resolves — the caller surfaces that as a clean
/// refusal, having created nothing.
pub fn resolve_project_main(main_repo: &Path) -> anyhow::Result<(String, String)> {
    for cand in PROJECT_MAIN_CANDIDATES {
        if let Some(sha) = rev_parse_commit(main_repo, cand) {
            return Ok((cand.to_string(), sha));
        }
        let remote = format!("origin/{}", cand);
        if let Some(sha) = rev_parse_commit(main_repo, &remote) {
            return Ok((remote, sha));
        }
    }
    // Nothing local. One fetch, through the same resolver `create_subtask`
    // uses for an explicit `base`.
    if let Ok(sha) = resolve_base_commit(main_repo, "main") {
        return Ok(("main".to_string(), sha));
    }
    anyhow::bail!(
        "cannot resolve the project's main branch in {} (tried main, origin/main, \
         master, origin/master, and a fetch of origin main)",
        main_repo.display()
    )
}

/// Branch name for a task's minted worktree:
/// `cm-sub/<name-slug>-<7 chars of the task id>`.
///
/// The suffix is DERIVED FROM THE TASK ID rather than random (which is
/// what `create_subtask` does), so minting is idempotent: a second mint
/// for the same task computes the same branch, `create_subtask_worktree`
/// finds the directory already there, and the task gets its original
/// checkout back instead of a second one. That matters because the
/// task→workspace binding lives in daemon state, which a restart can
/// outlive the worktree of.
///
/// The `cm-sub/` prefix is deliberate reuse: `recover_worktree_path` and
/// the TUI's reconcile recovery both already map that prefix back to a
/// worktree directory, so a minted checkout survives a manifest loss for
/// free.
pub fn task_worktree_branch(task_id: &str, task_name: &str) -> String {
    let mut slug = slugify(task_name);
    if slug.is_empty() {
        slug = "task".to_string();
    }
    let short: String = task_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(7)
        .collect::<String>()
        .to_ascii_lowercase();
    let short = if short.is_empty() {
        "nosuffix".to_string()
    } else {
        short
    };
    format!("cm-sub/{}-{}", slug, short)
}

/// A worktree minted for a task that had none.
#[derive(Debug)]
pub struct MintedTaskWorktree {
    pub branch: String,
    pub worktree_path: PathBuf,
    /// The trunk ref the cut came from (`main`, `origin/master`, …) for a
    /// fresh cut; `"existing branch"` when the mint re-attached a branch
    /// the task already had (see [`mint_task_worktree`]).
    pub base_ref: String,
    /// The commit the checkout actually sits on. Equals the trunk tip for
    /// a fresh mint; for a RE-mint that found the directory already there
    /// (same task ⇒ same branch) it's wherever that checkout has since
    /// moved to, which is the honest answer to "what am I working on top
    /// of".
    pub base_sha: String,
    /// `Some` when the mint found the task's branch but NOT its directory
    /// (a reaped worktree whose binding was lost too) and re-created the
    /// checkout instead of cutting a fresh zero-commit branch.
    pub recreated: Option<RecreatedWorktree>,
    /// Non-fatal observations for the caller's return value (a
    /// zero-commit decoy, a `wip_branch` that named a branch with no
    /// work, …). Empty for an ordinary fresh cut.
    pub warnings: Vec<String>,
}

/// Mint the checkout a workspace-less task's first session runs in: a
/// fresh branch cut from the project's trunk, in its own worktree.
///
/// This is the shared implementation behind BOTH `start_session` routes
/// (the daemon's `mcp_start_session` and the TUI's twin) so the two can't
/// drift on branch naming or cut point. It reuses `create_subtask`'s
/// machinery wholesale — [`resolve_project_main`] feeds a concrete sha to
/// [`create_subtask_worktree`] via [`SubtaskStart::Base`], so an
/// unresolvable trunk fails with the "does not resolve to a commit"
/// message BEFORE any git state is touched and the caller is never left
/// registering a workspace for a checkout that doesn't exist.
///
/// **Re-mint defensiveness.** "Workspace-less" means the daemon has no
/// binding for the task — which is ALSO what a task looks like after its
/// worktree was reaped and the binding dropped with it. Cutting a fresh
/// branch from trunk in that state manufactures a zero-commit decoy
/// (`cm-sub/<slug>-<task-id-prefix>`, an unrelated NOTES.md, none of the
/// task's commits) and overwrites the planning row's `wip_branch` with
/// it — the exact pointer rot the bug-triage orchestrator hit. So before
/// cutting anything:
///   1. if the task's `wip_branch_hint` names a CM-managed branch that
///      exists locally, the task HAD a checkout: re-attach that branch
///      (re-creating its directory if reaped) and return it;
///   2. else if the mint branch itself already exists (a prior mint whose
///      directory was reaped), re-attach it rather than failing on
///      `-b`'s "branch already exists";
///   3. else cut fresh from trunk, as before.
/// Whatever is attached, a branch with no commits beyond trunk is
/// reported in `warnings` rather than silently checked out.
pub fn mint_task_worktree(
    main_repo: &Path,
    task_id: &str,
    task_name: &str,
    wip_branch_hint: Option<&str>,
) -> anyhow::Result<MintedTaskWorktree> {
    let branch = task_worktree_branch(task_id, task_name);

    // (1) The task already has a branch on record → re-attach it.
    let hint = wip_branch_hint.map(str::trim).filter(|h| !h.is_empty());
    if let Some(h) = hint {
        if h != branch && local_branch_exists(main_repo, h) {
            if let Some(path) = worktree_dir_for_branch(main_repo, h) {
                return reattach_task_branch(main_repo, h, &path, Some(h));
            }
        }
    }
    // (2) A prior mint's branch survives its directory → re-attach it.
    if local_branch_exists(main_repo, &branch) {
        let path = worktree_dir_for_branch(main_repo, &branch)
            .expect("task_worktree_branch always yields a cm-sub/ branch");
        return reattach_task_branch(main_repo, &branch, &path, hint);
    }

    // (3) Fresh cut from trunk.
    let (base_ref, trunk_sha) = resolve_project_main(main_repo)?;
    let worktree_path = create_subtask_worktree(main_repo, &branch, SubtaskStart::Base(&trunk_sha))?;
    setup_worktree(main_repo, &worktree_path);
    let base_sha = worktree_head_sha(&worktree_path).unwrap_or(trunk_sha);
    Ok(MintedTaskWorktree {
        branch,
        worktree_path,
        base_ref,
        base_sha,
        recreated: None,
        warnings: Vec::new(),
    })
}

/// `mint_task_worktree`'s re-attach arm: the task's branch exists; make
/// sure its directory does too (re-creating it when reaped), provisioned
/// like a first-time worktree, and report what was found.
fn reattach_task_branch(
    main_repo: &Path,
    branch: &str,
    worktree_path: &Path,
    wip_branch_hint: Option<&str>,
) -> anyhow::Result<MintedTaskWorktree> {
    let health = ensure_worktree_materialized(main_repo, worktree_path, wip_branch_hint)?;
    let (recreated, mut warnings) = match health {
        WorktreeHealth::Recreated(r) => {
            let w = r.warnings.clone();
            (Some(r), w)
        }
        WorktreeHealth::Present => {
            // Directory intact, only the binding was lost. Still say so
            // if what's there is a zero-commit branch.
            let trunk = resolve_local_trunk(main_repo);
            let ahead = trunk.as_deref().and_then(|t| commits_ahead_of(main_repo, t, branch));
            (None, branch_warnings(main_repo, branch, trunk.as_deref(), ahead))
        }
        WorktreeHealth::PresentUnmanaged(w) => (None, vec![w]),
    };
    let base_sha = worktree_head_sha(worktree_path).ok_or_else(|| {
        anyhow::anyhow!(
            "worktree {} for branch '{}' has no resolvable HEAD",
            worktree_path.display(),
            branch
        )
    })?;
    if let Some(actual) = worktree_current_branch(worktree_path) {
        if actual != branch {
            warnings.push(format!(
                "worktree {} is checked out on '{}', not the task's branch '{}'",
                worktree_path.display(),
                actual,
                branch
            ));
        }
    }
    Ok(MintedTaskWorktree {
        branch: branch.to_string(),
        worktree_path: worktree_path.to_path_buf(),
        base_ref: "existing branch".to_string(),
        base_sha,
        recreated,
        warnings,
    })
}

/// Create a worktree for a subtask. Differs from `create_worktree` in:
///   - branch name is fully specified by the caller (e.g.
///     `cm-sub/<slug-chain>-<short_id>` per AGENT_ORCHESTRATION.md), not
///     derived from the slug. The flat-with-id form sidesteps the git
///     ref-prefix collision that hierarchical names hit.
///   - dir name under `~/.cm/worktrees/` is derived from the branch
///     name with `/` mapped to `-` so it's a valid path component.
///   - the new branch is cut from `start`: either the parent's
///     wip_branch (local ref first, origin fetch second) or an explicit
///     caller-supplied committish. See `SubtaskStart`.
///
/// Returns the path to the new worktree directory.
pub fn create_subtask_worktree(
    main_repo: &Path,
    branch_name: &str,
    start: SubtaskStart<'_>,
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

    // Disk guard — only a NEW worktree counts against the ceiling (the
    // reuse fast-path above already returned).
    enforce_worktree_capacity()?;

    // Explicit base: resolve to a sha up front so an unresolvable ref
    // fails with the precise "base '<x>' does not resolve to a commit"
    // message BEFORE any git state is touched, and so the cut is
    // pinned to exactly one commit (no `origin/<base>` second-guessing
    // of what the caller literally asked for).
    let start_branch = match start {
        SubtaskStart::ParentBranch(b) => b,
        SubtaskStart::Base(b) => {
            let sha = resolve_base_commit(main_repo, b)?;
            let out = Command::new("git")
                .arg("-C")
                .arg(main_repo)
                .args(["worktree", "add"])
                .arg(&worktree_path)
                .args(["-b", branch_name, &sha])
                .output()?;
            if out.status.success() {
                return Ok(worktree_path);
            }
            anyhow::bail!(
                "git worktree add failed for subtask branch {} at base {} ({}): {}",
                branch_name,
                b,
                sha,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    };

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

// ───── worktree materialization (reaped-worktree self-heal) ─────

/// Outcome of [`ensure_worktree_materialized`].
#[derive(Debug)]
pub enum WorktreeHealth {
    /// On disk and a git working tree rooted at the path; nothing done.
    Present,
    /// On disk but NOT a git working tree (a plain directory with no
    /// `.git`), and no branch could be found to re-check out into it.
    /// The path is still a real cwd, so a spawn may proceed — the
    /// string is a warning for the caller's return value.
    PresentUnmanaged(String),
    /// The directory was missing (or an empty leftover) and has been
    /// re-created from the branch named inside.
    Recreated(RecreatedWorktree),
}

/// A worktree [`ensure_worktree_materialized`] had to re-create.
#[derive(Debug, Clone)]
pub struct RecreatedWorktree {
    /// The branch checked out into the re-created directory.
    pub branch: String,
    /// Where `branch` came from: `"git-registration"` (git still listed
    /// the path as a prunable worktree on that branch), `"dir-name"`
    /// (the `cm-sub-…` / `<repo>-<slug>` directory name mapped back to
    /// its `cm-sub/…` / `cm/…` branch), or `"wip-branch"` (the task's
    /// planning-row pointer).
    pub branch_source: &'static str,
    /// HEAD of the re-created checkout.
    pub head_sha: Option<String>,
    /// Commits on `branch` beyond the project's trunk; `None` when no
    /// local trunk ref resolved to count against.
    pub commits_ahead: Option<u64>,
    /// Anything the caller should relay rather than swallow: a
    /// zero-commit decoy, a `wip_branch` that disagreed with the
    /// directory, a sibling branch that looks like the real work.
    pub warnings: Vec<String>,
}

impl RecreatedWorktree {
    /// One line for logs / a response field.
    pub fn summary(&self) -> String {
        format!(
            "re-created from branch '{}' (via {}) at {}{}",
            self.branch,
            self.branch_source,
            self.head_sha.as_deref().unwrap_or("?"),
            match self.commits_ahead {
                Some(n) => format!(", {} commit(s) ahead of trunk", n),
                None => String::new(),
            },
        )
    }
}

/// Is `path` a git working tree whose TOPLEVEL is `path` itself (not a
/// subdirectory of some other checkout)? False for a missing path, a
/// plain directory, or a worktree whose admin record has been pruned
/// (its `.git` file then points at a gitdir that no longer exists and
/// `rev-parse` fails).
pub fn is_git_worktree_root(path: &Path) -> bool {
    let out = match Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };
    let top = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    };
    same(&top, path)
}

/// `git rev-parse --verify --quiet refs/heads/<branch>` — does the
/// branch exist LOCALLY in `repo`?
pub fn local_branch_exists(repo: &Path, branch: &str) -> bool {
    rev_parse_commit(repo, &format!("refs/heads/{}", branch)).is_some()
}

/// The branch git's own worktree registry says `worktree_path` is on,
/// if git still has an entry for that path (a reaped directory that was
/// never `worktree prune`d stays listed, flagged `prunable`, with its
/// branch intact — the most authoritative answer to "what was checked
/// out here").
pub fn registered_branch_for_path(main_repo: &Path, worktree_path: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let wanted = worktree_path.canonicalize().unwrap_or_else(|_| worktree_path.to_path_buf());
    let mut current_matches = false;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            let listed = PathBuf::from(p);
            let listed_c = listed.canonicalize().unwrap_or_else(|_| listed.clone());
            current_matches = listed == worktree_path || listed_c == wanted;
        } else if current_matches {
            if let Some(r) = line.strip_prefix("branch ") {
                return Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string());
            }
        }
    }
    None
}

/// Inverse of the dir-naming in `create_worktree` /
/// `create_subtask_worktree` (see `recover_worktree_path`): the branch a
/// `~/.cm/worktrees/<dir>` directory was cut for.
///   - `cm-sub-<rest>`        → `cm-sub/<rest>`
///   - `<repo-name>-<slug>`   → `cm/<slug>`
/// `None` for anything else (not a CM-managed layout).
pub fn branch_for_worktree_dir(main_repo: &Path, worktree_path: &Path) -> Option<String> {
    let dir = worktree_path.file_name()?.to_str()?;
    if let Some(rest) = dir.strip_prefix("cm-sub-") {
        if !rest.is_empty() {
            return Some(format!("cm-sub/{}", rest));
        }
        return None;
    }
    let repo = main_repo.file_name()?.to_str()?;
    let slug = dir.strip_prefix(repo)?.strip_prefix('-')?;
    if slug.is_empty() {
        return None;
    }
    Some(format!("cm/{}", slug))
}

/// The directory a CM-managed branch's worktree lives in (the forward
/// map of [`branch_for_worktree_dir`]). `None` for a branch outside the
/// `cm/` / `cm-sub/` namespaces.
pub fn worktree_dir_for_branch(main_repo: &Path, branch: &str) -> Option<PathBuf> {
    let base = worktree_base();
    if let Some(slug) = branch.strip_prefix("cm/") {
        let repo = main_repo.file_name()?.to_str()?;
        return Some(base.join(format!("{}-{}", repo, slug)));
    }
    if branch.starts_with("cm-sub/") {
        return Some(base.join(branch.replace('/', "-")));
    }
    None
}

/// The project's trunk, LOCAL refs only (`main`, `origin/main`,
/// `master`, `origin/master`) — never a fetch. Used to count how far a
/// branch has moved; a heal must not spend network on a warning.
fn resolve_local_trunk(repo: &Path) -> Option<String> {
    for cand in PROJECT_MAIN_CANDIDATES {
        if rev_parse_commit(repo, cand).is_some() {
            return Some(cand.to_string());
        }
        let remote = format!("origin/{}", cand);
        if rev_parse_commit(repo, &remote).is_some() {
            return Some(remote);
        }
    }
    None
}

/// `git rev-list --count <trunk>..<branch>`.
pub fn commits_ahead_of(repo: &Path, trunk: &str, branch: &str) -> Option<u64> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-list", "--count"])
        .arg(format!("{}..{}", trunk, branch))
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// CM-managed branches (other than `exclude`) whose name carries `slug`
/// and which have commits beyond `trunk` — the "differently-suffixed
/// branch where the real work lives" a zero-commit decoy hides. Capped
/// at a handful; this feeds a warning, not a decision.
fn sibling_branches_with_work(
    repo: &Path,
    slug: &str,
    exclude: &str,
    trunk: &str,
) -> Vec<(String, u64)> {
    if slug.is_empty() {
        return Vec::new();
    }
    let out = match Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "for-each-ref",
            "--format=%(refname:short)",
            "refs/heads/cm/",
            "refs/heads/cm-sub/",
        ])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut found = Vec::new();
    for name in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if name == exclude || !name.contains(slug) {
            continue;
        }
        if let Some(n) = commits_ahead_of(repo, trunk, name).filter(|n| *n > 0) {
            found.push((name.to_string(), n));
            if found.len() >= 3 {
                break;
            }
        }
    }
    found
}

/// The slug portion of a CM-managed branch name, for sibling matching:
/// `cm/<slug>` → `<slug>`; `cm-sub/<slug>-<7-char suffix>` → `<slug>`.
fn branch_slug(branch: &str) -> &str {
    if let Some(s) = branch.strip_prefix("cm/") {
        return s;
    }
    let s = branch.strip_prefix("cm-sub/").unwrap_or(branch);
    // Strip the trailing `-<suffix>` (random short id or task-id prefix).
    match s.rfind('-') {
        Some(i) if i > 0 => &s[..i],
        _ => s,
    }
}

/// Non-fatal observations about the branch a heal (or re-mint) is about
/// to check out. A zero-commit branch is the decoy signature the
/// bug-triage orchestrator hit: the planning row's `wip_branch` pointed
/// at a branch cut from trunk that nothing was ever committed to, while
/// the task's real commits sat on a differently-suffixed sibling. We
/// surface that loudly rather than silently checking the decoy out.
fn branch_warnings(
    repo: &Path,
    branch: &str,
    trunk: Option<&str>,
    commits_ahead: Option<u64>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let (Some(trunk), Some(0)) = (trunk, commits_ahead) else {
        return warnings;
    };
    let siblings = sibling_branches_with_work(repo, branch_slug(branch), branch, trunk);
    let mut msg = format!(
        "branch '{}' has no commits beyond {} — it may be a zero-commit decoy rather \
         than the branch this task's work is on",
        branch, trunk,
    );
    if !siblings.is_empty() {
        let list: Vec<String> = siblings
            .iter()
            .map(|(b, n)| format!("'{}' ({} commit(s))", b, n))
            .collect();
        msg.push_str(&format!(
            "; sibling branch(es) with the same slug DO carry work: {}. Check which one \
             the task's commits are on before building on this checkout",
            list.join(", "),
        ));
    }
    warnings.push(msg);
    warnings
}

/// Verify that a workspace's worktree is usable as a session cwd — it
/// exists on disk AND is a git working tree — and if it has been reaped,
/// re-create it from the task's branch and provision it the way a
/// first-time branch-mode subtask is provisioned ([`setup_worktree`]).
///
/// This is the guard behind every "re-spawn into an existing task" path.
/// Pre-fix `start_session(task_id=<task whose worktree was reaped>)`
/// returned success with a `worktree_path` that did not exist: the agent
/// was spawned into a nonexistent cwd, wrote an empty transcript, and
/// sat `awaiting_input` — indistinguishable from a healthy worker that
/// works only through tool calls, so the failure cost a full dispatch
/// round before anyone noticed. Re-spawning into an existing task is a
/// supported flow; a reaped worktree must self-heal, and when it can't
/// (branch gone, path collision, git error) the caller must FAIL with a
/// message naming the path and the reason — never hand out a cwd that
/// isn't there.
///
/// Branch resolution for a missing directory, most- to least-
/// authoritative, all LOCAL refs only:
///   1. git's own worktree registry — a reaped-but-unpruned entry still
///      names the branch that was checked out at exactly this path.
///   2. the directory name, mapped back through the CM naming scheme
///      (`cm-sub-…` → `cm-sub/…`, `<repo>-<slug>` → `cm/<slug>`).
///   3. `wip_branch_hint` — the task's planning-row pointer, which on a
///      real host is sometimes a zero-commit decoy, so it ranks last.
/// Among the candidates that exist, the first one with commits beyond
/// trunk wins; if none has any, the first candidate is used and a
/// zero-commit warning is attached (see [`branch_warnings`]).
///
/// An existing directory is never overwritten: a git working tree rooted
/// there is `Present`; an EMPTY plain directory is treated as missing
/// (`git worktree add` accepts it); a non-empty plain directory with no
/// `.git` is `PresentUnmanaged` (a real cwd, warned about, not failed);
/// a directory with a `.git` that git rejects (pruned admin record) is
/// an error — spawning an agent into a checkout where git doesn't work
/// is the same invisible failure in a different coat.
pub fn ensure_worktree_materialized(
    main_repo: &Path,
    worktree_path: &Path,
    wip_branch_hint: Option<&str>,
) -> anyhow::Result<WorktreeHealth> {
    let mut pre_existing_empty_dir = false;
    if worktree_path.is_dir() {
        if is_git_worktree_root(worktree_path) {
            return Ok(WorktreeHealth::Present);
        }
        let dot_git = worktree_path.join(".git");
        if dot_git.symlink_metadata().is_ok() {
            let stderr = Command::new("git")
                .arg("-C")
                .arg(worktree_path)
                .args(["rev-parse", "--show-toplevel"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                .unwrap_or_default();
            anyhow::bail!(
                "worktree {} exists and has a .git entry, but git does not recognise it \
                 as a working tree ({}); its registration in {} was probably pruned — \
                 refusing to spawn into a checkout where git does not work, and refusing \
                 to overwrite it (move it aside, then re-spawn to re-create it from its \
                 branch)",
                worktree_path.display(),
                if stderr.is_empty() { "no detail from git" } else { &stderr },
                main_repo.display(),
            );
        }
        let entries = std::fs::read_dir(worktree_path)
            .map(|rd| rd.count())
            .unwrap_or(usize::MAX);
        if entries > 0 {
            return Ok(WorktreeHealth::PresentUnmanaged(format!(
                "worktree {} exists but is not a git working tree ({} entr{}, no .git); \
                 spawning there as-is",
                worktree_path.display(),
                entries,
                if entries == 1 { "y" } else { "ies" },
            )));
        }
        pre_existing_empty_dir = true;
    } else if worktree_path.symlink_metadata().is_ok() {
        anyhow::bail!(
            "worktree path {} exists but is not a directory; refusing to spawn into it \
             or replace it",
            worktree_path.display(),
        );
    }

    // ── Missing (or an empty leftover): find the branch to re-create from. ──
    if !main_repo.is_dir() {
        anyhow::bail!(
            "worktree {} is missing and cannot be re-created: its main repo {} is not \
             a directory on this host",
            worktree_path.display(),
            main_repo.display(),
        );
    }
    let registered = registered_branch_for_path(main_repo, worktree_path);
    let from_dir = branch_for_worktree_dir(main_repo, worktree_path);
    let hint = wip_branch_hint.map(str::trim).filter(|h| !h.is_empty()).map(str::to_string);

    let mut tried: Vec<String> = Vec::new();
    let mut candidates: Vec<(String, &'static str)> = Vec::new();
    for (cand, source) in [
        (registered.clone(), "git-registration"),
        (from_dir.clone(), "dir-name"),
        (hint.clone(), "wip-branch"),
    ] {
        let Some(b) = cand else { continue };
        tried.push(format!("{} '{}'", source, b));
        if candidates.iter().any(|(c, _)| *c == b) {
            continue;
        }
        if local_branch_exists(main_repo, &b) {
            candidates.push((b, source));
        }
    }
    if candidates.is_empty() {
        if pre_existing_empty_dir {
            return Ok(WorktreeHealth::PresentUnmanaged(format!(
                "worktree {} is an empty directory that is not a git working tree, and \
                 no branch exists to check out into it ({}); spawning there as-is",
                worktree_path.display(),
                if tried.is_empty() {
                    "no candidate branch could even be derived".to_string()
                } else {
                    format!("tried {}", tried.join(", "))
                },
            )));
        }
        anyhow::bail!(
            "worktree {} does not exist and cannot be re-created: no local branch to \
             check out was found in {} ({})",
            worktree_path.display(),
            main_repo.display(),
            if tried.is_empty() {
                "the directory name is not a CM-managed layout and the task has no \
                 wip_branch"
                    .to_string()
            } else {
                format!("tried {}", tried.join(", "))
            },
        );
    }

    let trunk = resolve_local_trunk(main_repo);
    let scored: Vec<((String, &'static str), Option<u64>)> = candidates
        .into_iter()
        .map(|c| {
            let ahead = trunk.as_deref().and_then(|t| commits_ahead_of(main_repo, t, &c.0));
            (c, ahead)
        })
        .collect();
    let pick = scored
        .iter()
        .position(|(_, ahead)| ahead.map(|n| n > 0).unwrap_or(false))
        .unwrap_or(0);
    let ((branch, branch_source), commits_ahead) = scored[pick].clone();

    let mut warnings = branch_warnings(main_repo, &branch, trunk.as_deref(), commits_ahead);
    // A wip_branch that exists but lost to a better candidate (or is a
    // zero-commit decoy) is worth saying out loud: the planning row is
    // pointing somewhere other than where the work is.
    if let Some(h) = hint.as_deref() {
        if h != branch {
            let detail = match (local_branch_exists(main_repo, h), trunk.as_deref()) {
                (false, _) => "does not exist locally".to_string(),
                (true, Some(t)) => match commits_ahead_of(main_repo, t, h) {
                    Some(0) => format!("has no commits beyond {} (a zero-commit decoy)", t),
                    Some(n) => format!("has {} commit(s) beyond {}", n, t),
                    None => "could not be compared to trunk".to_string(),
                },
                (true, None) => "exists (no trunk to compare against)".to_string(),
            };
            warnings.push(format!(
                "the task's wip_branch '{}' {}; re-created from '{}' (via {}) instead",
                h, detail, branch, branch_source,
            ));
        }
    }

    // ── Re-create. Prune first: git refuses to `add` over a path it still
    // lists as a (prunable) worktree. ──
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["worktree", "prune"])
        .output();
    let out = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["worktree", "add"])
        .arg(worktree_path)
        .arg(&branch)
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Don't leave a half-made directory behind for the next attempt to
        // trip over — but never touch one we didn't create.
        if !pre_existing_empty_dir && worktree_path.exists() {
            let _ = std::fs::remove_dir_all(worktree_path);
            let _ = Command::new("git")
                .arg("-C")
                .arg(main_repo)
                .args(["worktree", "prune"])
                .output();
        }
        let reason = if stderr.contains("already checked out") || stderr.contains("is already used by worktree") {
            format!(
                "branch '{}' is already checked out in another worktree ({})",
                branch, stderr
            )
        } else {
            format!("`git worktree add {} {}` failed: {}", worktree_path.display(), branch, stderr)
        };
        anyhow::bail!(
            "worktree {} does not exist and could not be re-created: {}",
            worktree_path.display(),
            reason,
        );
    }
    if !is_git_worktree_root(worktree_path) {
        anyhow::bail!(
            "worktree {} could not be re-created: `git worktree add` reported success \
             but the path is not a git working tree afterwards",
            worktree_path.display(),
        );
    }

    // Same provisioning a first-time branch-mode subtask gets.
    setup_worktree(main_repo, worktree_path);

    let head_sha = worktree_head_sha(worktree_path);
    Ok(WorktreeHealth::Recreated(RecreatedWorktree {
        branch,
        branch_source,
        head_sha,
        commits_ahead,
        warnings,
    }))
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

/// Provision a freshly created (or re-created) worktree.
///
/// Runs the repo's `setup_worktree.sh` if it exists in the main repo
/// (the script receives MAIN_REPO and WORKTREE as environment variables),
/// then — whether or not a script ran — makes sure the worktree has a
/// `.venv` pointing at the main repo's shared env ([`ensure_venv_symlink`]).
///
/// This is the ONE provisioning step every worktree-creating path shares
/// (A-n, `create_subtask` branch mode, `mint_task_worktree`, and the
/// reaped-worktree heal in [`ensure_worktree_materialized`]), so a
/// re-created worktree is provisioned exactly like a first-time one.
/// Idempotent: safe to run again on an already-provisioned tree.
pub fn setup_worktree(main_repo: &Path, worktree_path: &Path) {
    let script = main_repo.join("setup_worktree.sh");
    if script.exists() {
        let _ = Command::new("bash")
            .arg(&script)
            .env("MAIN_REPO", main_repo)
            .env("WORKTREE", worktree_path)
            .current_dir(worktree_path)
            .output();
    }
    ensure_venv_symlink(main_repo, worktree_path);
}

/// Symlink `<worktree>/.venv` → `<main_repo>/.venv` when the main repo has
/// one and the worktree has nothing at `.venv` yet. Returns whether a link
/// was created by THIS call.
///
/// Every healthy worktree on a Python-project host carries this link
/// (normally via the repo's `setup_worktree.sh`); without it `mypy` and
/// `ruff` fail to spawn and async tests FAIL rather than skip — which
/// looks exactly like the branch being broken. The fallback exists so a
/// repo without a setup script, or a hand-made worktree, gets the minimum
/// provisioning regardless. Never overwrites: an existing `.venv` (real
/// dir, or a link the script already made) is left alone. No-op for an
/// in-place workspace (`worktree_path == main_repo`).
pub fn ensure_venv_symlink(main_repo: &Path, worktree_path: &Path) -> bool {
    let same = match (main_repo.canonicalize(), worktree_path.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => main_repo == worktree_path,
    };
    if same {
        return false;
    }
    let src = main_repo.join(".venv");
    if !src.exists() {
        return false;
    }
    let dst = worktree_path.join(".venv");
    if dst.symlink_metadata().is_ok() {
        return false;
    }
    match std::os::unix::fs::symlink(&src, &dst) {
        Ok(()) => true,
        Err(e) => {
            eprintln!(
                "cm: could not symlink {} -> {}: {}",
                dst.display(),
                src.display(),
                e
            );
            false
        }
    }
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

    // === create_subtask(base=…): explicit cut point ===

    /// Add a commit on top of whatever is checked out and return its sha.
    fn commit_file(repo: &Path, name: &str, body: &str) -> String {
        std::fs::write(repo.join(name), body).unwrap();
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", name]);
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("spawn git");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A two-commit repo on `main`, with `v1` tagging the first commit and
    /// a synthetic `refs/remotes/origin/main` also at the first commit
    /// (written directly so the test needs no network / real remote).
    /// Returns `(tempdir_guard, repo_path, first_sha, second_sha)` — the
    /// guard must stay alive for the repo to exist.
    fn repo_with_history() -> (tempfile::TempDir, PathBuf, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("myrepo");
        make_git_repo(&repo);
        git(&repo, &["branch", "-M", "main"]);
        let first = commit_file(&repo, "one.txt", "1");
        git(&repo, &["tag", "v1"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", &first]);
        let second = commit_file(&repo, "two.txt", "2");
        (tmp, repo, first, second)
    }

    /// `base` must accept every shape git accepts: full sha, short sha,
    /// tag, local branch, remote-tracking ref. Each resolves to the
    /// concrete commit the subtask will be cut from.
    #[test]
    fn resolve_base_commit_accepts_sha_tag_branch_and_remote_ref() {
        let (_tmp, repo, first, second) = repo_with_history();

        assert_eq!(resolve_base_commit(&repo, &first).unwrap(), first);
        assert_eq!(resolve_base_commit(&repo, &first[..8]).unwrap(), first);
        assert_eq!(resolve_base_commit(&repo, "v1").unwrap(), first);
        assert_eq!(resolve_base_commit(&repo, "origin/main").unwrap(), first);
        assert_eq!(resolve_base_commit(&repo, "main").unwrap(), second);
        // Surrounding whitespace is a paste artifact, not a different ref.
        assert_eq!(resolve_base_commit(&repo, "  main  ").unwrap(), second);
    }

    /// A base that resolves nowhere must fail with the actionable
    /// message the MCP tool documents — not a raw git stderr. The
    /// `-`-prefixed case would otherwise be parsed by git as an option.
    #[test]
    fn resolve_base_commit_rejects_unresolvable_ref() {
        let (_tmp, repo, _first, _second) = repo_with_history();

        for bad in ["no-such-ref-anywhere", "--force", ""] {
            let err = resolve_base_commit(&repo, bad)
                .expect_err("must not resolve")
                .to_string();
            assert!(
                err.contains("does not resolve to a commit"),
                "unexpected error for base {:?}: {}",
                bad,
                err
            );
        }
    }

    /// `base` lands in the REFSPEC argument of `git fetch origin <base>`,
    /// so `<src>:<dst>` / `+<src>:<dst>` would make git create or
    /// force-overwrite `<dst>` in the operator's main repo — and because
    /// that fetch succeeds, the `FETCH_HEAD` fallback then resolved and
    /// `create_subtask` reported success, hiding the clobber. Refspec /
    /// glob syntax must be rejected before git's argv, leaving the repo's
    /// refs untouched.
    #[test]
    fn resolve_base_commit_rejects_refspec_and_never_writes_refs() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        make_git_repo(&upstream);
        git(&upstream, &["branch", "-M", "main"]);
        commit_file(&upstream, "up.txt", "u");

        let clone = tmp.path().join("clone");
        let out = Command::new("git")
            .args(["clone", "-q"])
            .arg(&upstream)
            .arg(&clone)
            .output()
            .expect("spawn git clone");
        assert!(
            out.status.success(),
            "clone failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // `git clone` copies no identity, so the commit below would fall
        // back to the machine's GLOBAL git config — which the HOME-swapping
        // tests in this module (they share `env_lock`, this one doesn't)
        // can yank out from under it mid-run. Pin it locally like
        // `make_git_repo` does.
        git(&clone, &["config", "user.email", "t@t"]);
        git(&clone, &["config", "user.name", "t"]);

        // A local-only commit on a branch that is NOT checked out — git
        // refuses to fetch into a checked-out branch, so this is exactly
        // the ref a forced refspec could destroy.
        git(&clone, &["checkout", "-q", "-b", "victim"]);
        let victim = commit_file(&clone, "local-only.txt", "precious");
        git(&clone, &["checkout", "-q", "main"]);

        for bad in [
            "+refs/heads/main:refs/heads/victim",
            "refs/heads/main:refs/heads/brand-new",
            "+refs/heads/*:refs/heads/pwn/*",
            "+main",
            "main victim",
        ] {
            let err = resolve_base_commit(&clone, bad)
                .expect_err("refspec syntax must not resolve")
                .to_string();
            assert!(
                err.contains("does not resolve to a commit"),
                "unexpected error for base {:?}: {}",
                bad,
                err
            );
            assert_eq!(
                rev_parse_commit(&clone, "victim").as_deref(),
                Some(victim.as_str()),
                "base {:?} moved an existing local branch",
                bad
            );
            for forged in ["brand-new", "pwn/main"] {
                assert!(
                    rev_parse_commit(&clone, forged).is_none(),
                    "base {:?} created local branch {}",
                    bad,
                    forged
                );
            }
        }

        // Positive control: the fetch fallback really is live in this
        // fixture, so the rejections above are the validator's doing and
        // not a broken remote. `later` exists only on the upstream.
        git(&upstream, &["checkout", "-q", "-b", "later"]);
        let later = commit_file(&upstream, "later.txt", "l");
        assert_eq!(resolve_base_commit(&clone, "later").unwrap(), later);
    }

    /// `SubtaskStart::Base` cuts the subtask branch from the named
    /// commit, NOT from the parent branch's tip — the whole point of the
    /// knob (fork off clean upstream instead of inheriting WIP).
    #[test]
    fn create_subtask_worktree_base_cuts_from_explicit_commit() {
        with_home(|_home| {
            let (_tmp, repo, first, second) = repo_with_history();

            let wt = create_subtask_worktree(
                &repo,
                "cm-sub/base-test-abc1234",
                SubtaskStart::Base(&first),
            )
            .expect("cut from the explicit base");

            assert_eq!(
                worktree_current_branch(&wt).as_deref(),
                Some("cm-sub/base-test-abc1234")
            );
            // item 2b: the reported base_sha is the commit asked for.
            assert_eq!(worktree_head_sha(&wt).as_deref(), Some(first.as_str()));
            assert_ne!(worktree_head_sha(&wt).as_deref(), Some(second.as_str()));
            // The second commit's file must NOT be present — proof the
            // cut really happened at `first`.
            assert!(wt.join("one.txt").exists());
            assert!(!wt.join("two.txt").exists());
        });
    }

    /// An unresolvable base fails BEFORE any worktree is produced, so the
    /// caller's rollback path (delete the API row) isn't racing a
    /// half-built checkout.
    #[test]
    fn create_subtask_worktree_base_unresolvable_leaves_no_worktree() {
        with_home(|home| {
            let (_tmp, repo, _first, _second) = repo_with_history();

            let err = create_subtask_worktree(
                &repo,
                "cm-sub/base-missing-abc1234",
                SubtaskStart::Base("nope-not-a-ref"),
            )
            .expect_err("must fail")
            .to_string();

            assert!(
                err.contains("does not resolve to a commit"),
                "unexpected error: {}",
                err
            );
            assert!(!home
                .join(".cm/worktrees/cm-sub-base-missing-abc1234")
                .exists());
        });
    }

    /// Default (no `base`) behavior is untouched by the `SubtaskStart`
    /// refactor: the cut still follows the parent's branch tip.
    #[test]
    fn create_subtask_worktree_parent_branch_cuts_from_branch_tip() {
        with_home(|_home| {
            let (_tmp, repo, _first, second) = repo_with_history();

            let wt = create_subtask_worktree(
                &repo,
                "cm-sub/parent-test-abc1234",
                SubtaskStart::ParentBranch("main"),
            )
            .expect("cut from the parent branch");

            assert_eq!(worktree_head_sha(&wt).as_deref(), Some(second.as_str()));
            assert!(wt.join("two.txt").exists());
        });
    }

    // === ux-1a: minting a worktree for a workspace-less task ===

    /// The mint cuts from the project's trunk, NOT from whatever the
    /// caller happens to be sitting on — that's the whole point of the
    /// policy (the ux note's incident was a worker "branching off main"
    /// inside the proposer's WIP checkout). `repo_with_history` leaves
    /// `main` at `second`, so that's the commit the mint must land on.
    #[test]
    fn mint_task_worktree_cuts_from_project_main() {
        with_home(|_home| {
            let (_tmp, repo, first, second) = repo_with_history();
            // Move the main repo OFF main, to prove the cut follows the
            // trunk rather than the current checkout.
            git(&repo, &["checkout", "-q", "-b", "sidetrack", &first]);

            let minted = mint_task_worktree(&repo, "abc12345-dead-beef", "Fix the thing", None)
                .expect("mint");

            assert_eq!(minted.base_ref, "main");
            assert_eq!(minted.base_sha, second);
            assert_eq!(worktree_head_sha(&minted.worktree_path).as_deref(), Some(second.as_str()));
            assert_eq!(
                worktree_current_branch(&minted.worktree_path).as_deref(),
                Some(minted.branch.as_str()),
            );
            assert!(minted.worktree_path.exists());
        });
    }

    /// The branch (and therefore the directory) is derived from the TASK
    /// ID, so a second mint for the same task returns the same checkout
    /// instead of a second one. This is what keeps a daemon restart —
    /// which can outlive the task→workspace binding — from scattering
    /// duplicate worktrees.
    #[test]
    fn mint_task_worktree_is_idempotent_per_task() {
        with_home(|_home| {
            let (_tmp, repo, _first, _second) = repo_with_history();

            let a = mint_task_worktree(&repo, "abc12345-dead-beef", "Fix the thing", None).unwrap();
            let b = mint_task_worktree(&repo, "abc12345-dead-beef", "Fix the thing", None).unwrap();

            assert_eq!(a.branch, b.branch);
            assert_eq!(a.worktree_path, b.worktree_path);
            // A DIFFERENT task with the same name gets its own checkout.
            let c = mint_task_worktree(&repo, "99999999-dead-beef", "Fix the thing", None).unwrap();
            assert_ne!(a.worktree_path, c.worktree_path);
        });
    }

    /// The branch name follows the `cm-sub/` convention so
    /// `recover_worktree_path` (and the TUI's reconcile recovery, which
    /// keys off the same prefix) can map a minted checkout back to its
    /// task after a manifest loss.
    #[test]
    fn task_worktree_branch_is_recoverable() {
        with_home(|home| {
            let branch = task_worktree_branch("abc12345-dead-beef", "Fix the thing!");
            assert_eq!(branch, "cm-sub/fix-the-thing-abc1234");

            let dir = home.join(".cm/worktrees").join(branch.replace('/', "-"));
            std::fs::create_dir_all(&dir).unwrap();
            assert_eq!(recover_worktree_path("r", &branch), Some(dir));

            // A task whose name slugifies to nothing still yields a
            // usable branch rather than a bare `cm-sub/-<id>`.
            assert_eq!(task_worktree_branch("abc1234", "!!!"), "cm-sub/task-abc1234");
        });
    }

    /// A repo whose trunk is `master` resolves without the caller having
    /// to say so — and a repo with NO trunk at all fails cleanly, having
    /// created nothing. That second half is the invariant the mint
    /// callers depend on: they register a workspace only after this
    /// returns Ok.
    #[test]
    fn resolve_project_main_handles_master_and_refuses_when_absent() {
        with_home(|home| {
            let tmp = tempfile::tempdir().unwrap();
            let repo = tmp.path().join("masterrepo");
            make_git_repo(&repo);
            git(&repo, &["branch", "-M", "master"]);
            let head = commit_file(&repo, "one.txt", "1");
            assert_eq!(
                resolve_project_main(&repo).unwrap(),
                ("master".to_string(), head)
            );

            // Rename the trunk to something the resolver doesn't know and
            // there is no main to fall back to.
            git(&repo, &["branch", "-M", "trunkless"]);
            let err = resolve_project_main(&repo).expect_err("no main/master").to_string();
            assert!(
                err.contains("cannot resolve the project's main branch"),
                "unexpected error: {}",
                err
            );
            let err = mint_task_worktree(&repo, "abc1234", "no trunk", None)
                .expect_err("mint must refuse")
                .to_string();
            assert!(err.contains("cannot resolve the project's main branch"), "{}", err);
            assert!(
                !home.join(".cm/worktrees/cm-sub-no-trunk-abc1234").exists(),
                "a refused mint must leave no half-made worktree behind",
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

    /// RAII reset for the process-global worktree ceiling so a failing
    /// assertion can't leak an armed guard into other tests. Constructed
    /// under `with_home`'s env_lock, which also serializes the global.
    struct GuardReset;
    impl Drop for GuardReset {
        fn drop(&mut self) {
            set_max_worktrees(None);
        }
    }

    /// Wedge campaign F1: `[scheduler] max_worktrees` is enforced at worktree
    /// creation — at capacity a NEW mint refuses with an actionable message
    /// naming the knob, while REUSING an existing checkout still succeeds.
    #[test]
    fn max_worktrees_guard_blocks_new_mints_but_not_reuse() {
        with_home(|home| {
            let _reset = GuardReset;
            let base = home.join(".cm/worktrees");
            std::fs::create_dir_all(base.join("existing-one")).unwrap();

            set_max_worktrees(Some(1));
            assert_eq!(count_worktrees(), 1);

            // NEW subtask mint at capacity: refused BEFORE any git state is
            // touched (the repo path here doesn't even exist).
            let err = create_subtask_worktree(
                Path::new("/nonexistent-repo"),
                "cm-sub/blocked-abc1234",
                SubtaskStart::Base("main"),
            )
            .expect_err("guard must refuse at capacity");
            let msg = err.to_string();
            assert!(msg.contains("max_worktrees"), "names the knob: {}", msg);
            assert!(msg.contains("git worktree remove"), "remediation: {}", msg);
            assert!(
                !base.join("cm-sub-blocked-abc1234").exists(),
                "nothing half-made",
            );

            // Same refusal on the create_worktree route.
            let err = create_worktree(Path::new("/nonexistent-repo"), "blocked", None)
                .expect_err("guard must refuse at capacity");
            assert!(err.to_string().contains("max_worktrees"));

            // REUSE of an existing subtask worktree is always allowed.
            std::fs::create_dir_all(base.join("cm-sub-reused-abc1234")).unwrap();
            let reused = create_subtask_worktree(
                Path::new("/nonexistent-repo"),
                "cm-sub/reused-abc1234",
                SubtaskStart::Base("main"),
            )
            .expect("reuse path bypasses the guard");
            assert_eq!(reused, base.join("cm-sub-reused-abc1234"));

            // Raising the ceiling (or disabling) unblocks; the git failure
            // that follows is the ordinary unresolvable-repo error, proving
            // the guard itself stood down.
            set_max_worktrees(None);
            let err = create_subtask_worktree(
                Path::new("/nonexistent-repo"),
                "cm-sub/unblocked-abc1234",
                SubtaskStart::Base("main"),
            )
            .expect_err("repo still doesn't exist");
            assert!(
                !err.to_string().contains("max_worktrees"),
                "guard disarmed: {}",
                err,
            );
        });
    }

    // === reaped-worktree self-heal (`ensure_worktree_materialized`) ===

    /// A subtask worktree with one commit of its own on top of `main`,
    /// under `$HOME/.cm/worktrees/` (so `with_home` must wrap it).
    /// Returns `(worktree_path, branch, work_sha)`.
    fn subtask_worktree_with_work(repo: &Path, branch: &str) -> (PathBuf, String) {
        let wt = create_subtask_worktree(repo, branch, SubtaskStart::ParentBranch("main"))
            .expect("create subtask worktree");
        git(&wt, &["config", "user.email", "t@t"]);
        git(&wt, &["config", "user.name", "t"]);
        let sha = commit_file(&wt, "work.txt", "real work");
        (wt, sha)
    }

    fn assert_is_live_worktree(path: &Path, branch: &str, head: &str) {
        assert!(path.is_dir(), "worktree dir must exist: {}", path.display());
        assert!(is_git_worktree_root(path), "must be a git worktree root: {}", path.display());
        assert_eq!(worktree_current_branch(path).as_deref(), Some(branch));
        assert_eq!(worktree_head_sha(path).as_deref(), Some(head));
        assert!(path.join("work.txt").exists(), "the branch's work is checked out");
    }

    /// A healthy worktree is left alone — no git mutation, `Present`.
    #[test]
    fn materialize_present_worktree_is_a_noop() {
        with_home(|_home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let (wt, sha) = subtask_worktree_with_work(&repo, "cm-sub/fix-it-abc1234");
            let health = ensure_worktree_materialized(&repo, &wt, None).unwrap();
            assert!(matches!(health, WorktreeHealth::Present), "{:?}", health);
            assert_is_live_worktree(&wt, "cm-sub/fix-it-abc1234", &sha);
        });
    }

    /// THE incident: the directory was `rm -rf`'d (never pruned), the
    /// branch is intact. git still lists the path as a prunable worktree
    /// on that branch, which is the most authoritative source — the heal
    /// re-creates the checkout there, at the branch's tip.
    #[test]
    fn materialize_recreates_reaped_worktree_from_git_registration() {
        with_home(|_home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let (wt, sha) = subtask_worktree_with_work(&repo, "cm-sub/fix-it-abc1234");
            std::fs::remove_dir_all(&wt).unwrap();
            assert!(!wt.exists());

            let health = ensure_worktree_materialized(&repo, &wt, None).unwrap();
            let WorktreeHealth::Recreated(r) = health else {
                panic!("expected Recreated, got {:?}", health);
            };
            assert_eq!(r.branch, "cm-sub/fix-it-abc1234");
            assert_eq!(r.branch_source, "git-registration");
            assert_eq!(r.head_sha.as_deref(), Some(sha.as_str()));
            assert_eq!(r.commits_ahead, Some(1));
            assert!(r.warnings.is_empty(), "a branch with work warns about nothing: {:?}", r.warnings);
            assert_is_live_worktree(&wt, "cm-sub/fix-it-abc1234", &sha);
            // And a second check is now a plain Present.
            assert!(matches!(
                ensure_worktree_materialized(&repo, &wt, None).unwrap(),
                WorktreeHealth::Present
            ));
        });
    }

    /// Reaped AND pruned (`git worktree remove` / a prune sweep): git has
    /// forgotten the path, but the CM directory-naming scheme maps the
    /// dir name straight back to its branch.
    #[test]
    fn materialize_recreates_from_dir_name_after_prune() {
        with_home(|_home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let (wt, sha) = subtask_worktree_with_work(&repo, "cm-sub/fix-it-abc1234");
            git(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
            git(&repo, &["worktree", "prune"]);
            assert!(!wt.exists());
            assert!(registered_branch_for_path(&repo, &wt).is_none(), "git forgot the path");

            let WorktreeHealth::Recreated(r) =
                ensure_worktree_materialized(&repo, &wt, None).unwrap()
            else {
                panic!("expected Recreated");
            };
            assert_eq!(r.branch_source, "dir-name");
            assert_is_live_worktree(&wt, "cm-sub/fix-it-abc1234", &sha);

            // The `<repo>-<slug>` → `cm/<slug>` layout maps back too.
            let (wt2, created) = create_worktree(&repo, "launch-slug", None).unwrap();
            assert!(created);
            git(&wt2, &["config", "user.email", "t@t"]);
            git(&wt2, &["config", "user.name", "t"]);
            let sha2 = commit_file(&wt2, "work.txt", "w");
            git(&repo, &["worktree", "remove", "--force", wt2.to_str().unwrap()]);
            git(&repo, &["worktree", "prune"]);
            let WorktreeHealth::Recreated(r2) =
                ensure_worktree_materialized(&repo, &wt2, None).unwrap()
            else {
                panic!("expected Recreated");
            };
            assert_eq!(r2.branch, "cm/launch-slug");
            assert_eq!(r2.branch_source, "dir-name");
            assert_is_live_worktree(&wt2, "cm/launch-slug", &sha2);
        });
    }

    /// A directory whose name isn't a CM layout and which git has
    /// forgotten: the task's `wip_branch` is the only pointer left, and it
    /// is honored when it exists.
    #[test]
    fn materialize_falls_back_to_wip_branch_hint() {
        with_home(|home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            git(&repo, &["branch", "feature/real-work"]);
            let odd = home.join(".cm/worktrees/something-else-entirely");
            let WorktreeHealth::Recreated(r) =
                ensure_worktree_materialized(&repo, &odd, Some("feature/real-work")).unwrap()
            else {
                panic!("expected Recreated");
            };
            assert_eq!(r.branch, "feature/real-work");
            assert_eq!(r.branch_source, "wip-branch");
            assert!(is_git_worktree_root(&odd));
            assert_eq!(worktree_current_branch(&odd).as_deref(), Some("feature/real-work"));
        });
    }

    /// Unrecoverable: the branch is gone too. The call FAILS — naming the
    /// path, the repo, and every candidate it tried — and leaves nothing
    /// on disk. Returning a success payload here is the core defect.
    #[test]
    fn materialize_fails_loudly_when_no_branch_can_be_found() {
        with_home(|_home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let (wt, _sha) = subtask_worktree_with_work(&repo, "cm-sub/fix-it-abc1234");
            git(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
            git(&repo, &["worktree", "prune"]);
            git(&repo, &["branch", "-D", "cm-sub/fix-it-abc1234"]);

            let err = ensure_worktree_materialized(&repo, &wt, Some("cm-sub/also-gone-1234567"))
                .expect_err("no branch → must fail")
                .to_string();
            assert!(err.contains(&wt.display().to_string()), "names the path: {}", err);
            assert!(err.contains("does not exist and cannot be re-created"), "{}", err);
            assert!(err.contains("dir-name 'cm-sub/fix-it-abc1234'"), "lists what it tried: {}", err);
            assert!(err.contains("wip-branch 'cm-sub/also-gone-1234567'"), "{}", err);
            assert!(!wt.exists(), "a failed heal leaves nothing behind");
        });
    }

    /// The existing-directory cases, one by one:
    ///   - an EMPTY leftover dir is re-used by `git worktree add`;
    ///   - a non-empty dir with no `.git` is a real cwd → `PresentUnmanaged`
    ///     (warned, never overwritten);
    ///   - a dir whose `.git` git rejects (pruned admin record) is an error;
    ///   - a branch already checked out elsewhere is an error naming it.
    #[test]
    fn materialize_existing_dir_and_collision_cases() {
        with_home(|home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let (wt, sha) = subtask_worktree_with_work(&repo, "cm-sub/fix-it-abc1234");

            // Empty leftover.
            std::fs::remove_dir_all(&wt).unwrap();
            std::fs::create_dir_all(&wt).unwrap();
            let WorktreeHealth::Recreated(r) =
                ensure_worktree_materialized(&repo, &wt, None).unwrap()
            else {
                panic!("an empty dir is re-usable");
            };
            assert_eq!(r.branch_source, "git-registration");
            assert_is_live_worktree(&wt, "cm-sub/fix-it-abc1234", &sha);

            // Plain non-empty dir, no .git.
            let plain = home.join(".cm/worktrees/plain");
            std::fs::create_dir_all(&plain).unwrap();
            std::fs::write(plain.join("notes.txt"), "x").unwrap();
            match ensure_worktree_materialized(&repo, &plain, None).unwrap() {
                WorktreeHealth::PresentUnmanaged(w) => {
                    assert!(w.contains("not a git working tree"), "{}", w);
                }
                other => panic!("expected PresentUnmanaged, got {:?}", other),
            }
            assert!(plain.join("notes.txt").exists(), "never overwritten");

            // `.git` present but rejected by git (pruned admin record).
            let pruned = wt.clone();
            std::fs::remove_dir_all(repo.join(".git/worktrees")).unwrap();
            let err = ensure_worktree_materialized(&repo, &pruned, None)
                .expect_err("git rejects the checkout")
                .to_string();
            assert!(err.contains("has a .git entry"), "{}", err);
            assert!(err.contains(&pruned.display().to_string()), "{}", err);
            assert!(pruned.join("work.txt").exists(), "never overwritten");
            std::fs::remove_dir_all(&pruned).unwrap();
            git(&repo, &["worktree", "prune"]);

            // Branch already checked out in another worktree.
            let other = home.join(".cm/worktrees/elsewhere");
            git(&repo, &["worktree", "add", other.to_str().unwrap(), "cm-sub/fix-it-abc1234"]);
            let err = ensure_worktree_materialized(&repo, &wt, None)
                .expect_err("branch busy")
                .to_string();
            assert!(err.contains("already checked out"), "{}", err);
            assert!(err.contains("cm-sub/fix-it-abc1234"), "{}", err);
            assert!(!wt.exists(), "a failed add leaves no half-made dir");
        });
    }

    /// Requirement 2: a re-created worktree gets the same provisioning a
    /// first-time branch-mode subtask gets — at minimum `.venv` →
    /// `<main_repo>/.venv`. Pinned for BOTH the first-time path and the
    /// heal path, so they can't drift.
    #[test]
    fn materialize_provisions_venv_symlink_like_first_time_create() {
        with_home(|_home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            std::fs::create_dir_all(repo.join(".venv/bin")).unwrap();
            std::fs::write(repo.join(".venv/bin/python"), "").unwrap();

            // First-time create (no setup_worktree.sh in this repo): the
            // fallback alone provides the link.
            let wt = create_subtask_worktree(&repo, "cm-sub/venv-abc1234", SubtaskStart::ParentBranch("main"))
                .unwrap();
            setup_worktree(&repo, &wt);
            let link = wt.join(".venv");
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read_link(&link).unwrap(), repo.join(".venv"));

            // Reap (rm -rf takes the symlink with it) and heal.
            std::fs::remove_dir_all(&wt).unwrap();
            let WorktreeHealth::Recreated(_) =
                ensure_worktree_materialized(&repo, &wt, None).unwrap()
            else {
                panic!("expected Recreated");
            };
            assert!(
                link.symlink_metadata().unwrap().file_type().is_symlink(),
                "the healed worktree must carry the .venv link"
            );
            assert_eq!(std::fs::read_link(&link).unwrap(), repo.join(".venv"));
            assert!(link.join("bin/python").exists(), "link resolves to the shared env");

            // Idempotent + never overwrites an existing .venv.
            assert!(!ensure_venv_symlink(&repo, &wt));
            // In-place (worktree == main repo): nothing to do.
            assert!(!ensure_venv_symlink(&repo, &repo));
            assert!(!repo.join(".venv").symlink_metadata().unwrap().file_type().is_symlink());
        });
    }

    /// The decoy defense. The task's `wip_branch` names a branch with ZERO
    /// commits beyond trunk while the directory's own branch carries the
    /// work: the heal checks out the branch with work and SAYS that the
    /// wip_branch was a zero-commit decoy.
    #[test]
    fn materialize_prefers_branch_with_work_and_reports_zero_commit_hint() {
        with_home(|_home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let (wt, sha) = subtask_worktree_with_work(&repo, "cm-sub/bug-048-rand1234");
            // The decoy: same slug, task-id-prefix suffix, cut from main,
            // nothing committed.
            git(&repo, &["branch", "cm-sub/bug-048-50ae24e", "main"]);
            git(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
            git(&repo, &["worktree", "prune"]);

            let WorktreeHealth::Recreated(r) =
                ensure_worktree_materialized(&repo, &wt, Some("cm-sub/bug-048-50ae24e")).unwrap()
            else {
                panic!("expected Recreated");
            };
            assert_eq!(r.branch, "cm-sub/bug-048-rand1234", "the branch WITH work wins");
            assert_eq!(r.commits_ahead, Some(1));
            assert_is_live_worktree(&wt, "cm-sub/bug-048-rand1234", &sha);
            let joined = r.warnings.join("\n");
            assert!(joined.contains("wip_branch 'cm-sub/bug-048-50ae24e'"), "{}", joined);
            assert!(joined.contains("zero-commit decoy"), "{}", joined);
        });
    }

    /// When the ONLY candidate is a zero-commit branch it is still checked
    /// out (it may simply be a task nobody has committed on yet) — but the
    /// result carries a warning, and names any same-slug sibling branch
    /// that DOES carry commits, rather than silently proceeding.
    #[test]
    fn materialize_warns_on_zero_commit_branch_and_names_working_sibling() {
        with_home(|home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            // The sibling with the real work (a different random suffix).
            let (_real, _sha) = subtask_worktree_with_work(&repo, "cm-sub/bug-048-rand1234");
            // The decoy, never materialized.
            git(&repo, &["branch", "cm-sub/bug-048-50ae24e", "main"]);
            let decoy_wt = home.join(".cm/worktrees/cm-sub-bug-048-50ae24e");

            let WorktreeHealth::Recreated(r) =
                ensure_worktree_materialized(&repo, &decoy_wt, None).unwrap()
            else {
                panic!("expected Recreated");
            };
            assert_eq!(r.branch, "cm-sub/bug-048-50ae24e");
            assert_eq!(r.commits_ahead, Some(0));
            let joined = r.warnings.join("\n");
            assert!(joined.contains("has no commits beyond main"), "{}", joined);
            assert!(joined.contains("'cm-sub/bug-048-rand1234' (1 commit(s))"), "names the sibling: {}", joined);
        });
    }

    /// Re-mint after a reap. Pre-fix the second mint hit `git worktree add
    /// -b <branch>` with the branch already present and FAILED ("branch
    /// already exists") — so a task whose minted worktree was reaped could
    /// never be spawned on again. Now it re-attaches the branch.
    #[test]
    fn mint_task_worktree_reattaches_reaped_mint_branch() {
        with_home(|_home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let a = mint_task_worktree(&repo, "abc12345-dead-beef", "Fix the thing", None).unwrap();
            git(&a.worktree_path, &["config", "user.email", "t@t"]);
            git(&a.worktree_path, &["config", "user.name", "t"]);
            let sha = commit_file(&a.worktree_path, "work.txt", "w");
            std::fs::remove_dir_all(&a.worktree_path).unwrap();

            let b = mint_task_worktree(&repo, "abc12345-dead-beef", "Fix the thing", None)
                .expect("re-mint must re-attach, not fail");
            assert_eq!(b.branch, a.branch);
            assert_eq!(b.worktree_path, a.worktree_path);
            assert_eq!(b.base_sha, sha, "lands on the branch's tip, not trunk");
            assert_eq!(b.base_ref, "existing branch");
            let r = b.recreated.expect("the directory was re-created");
            assert_eq!(r.branch_source, "git-registration");
            assert!(b.warnings.is_empty(), "{:?}", b.warnings);
            assert_is_live_worktree(&b.worktree_path, &a.branch, &sha);
        });
    }

    /// The decoy's origin, closed: a task whose binding was lost but whose
    /// `wip_branch` (a `create_subtask` branch with real commits) still
    /// exists is RE-ATTACHED to that branch — not handed a fresh
    /// `cm-sub/<slug>-<task-id-prefix>` cut from trunk with none of its
    /// work, which is how the zero-commit decoys were being manufactured.
    #[test]
    fn mint_task_worktree_reattaches_wip_branch_instead_of_cutting_decoy() {
        with_home(|home| {
            let (_tmp, repo, _f, _s) = repo_with_history();
            let (real_wt, sha) = subtask_worktree_with_work(&repo, "cm-sub/fix-the-thing-rand1234");
            std::fs::remove_dir_all(&real_wt).unwrap();

            let m = mint_task_worktree(
                &repo,
                "abc12345-dead-beef",
                "Fix the thing",
                Some("cm-sub/fix-the-thing-rand1234"),
            )
            .unwrap();
            assert_eq!(m.branch, "cm-sub/fix-the-thing-rand1234");
            assert_eq!(m.worktree_path, real_wt);
            assert_eq!(m.base_sha, sha);
            assert!(m.recreated.is_some());
            assert_is_live_worktree(&real_wt, "cm-sub/fix-the-thing-rand1234", &sha);
            assert!(
                !local_branch_exists(&repo, "cm-sub/fix-the-thing-abc1234"),
                "no decoy branch is cut"
            );
            assert!(!home.join(".cm/worktrees/cm-sub-fix-the-thing-abc1234").exists());

            // A hint naming a branch that does NOT exist falls through to
            // the ordinary fresh cut.
            let fresh = mint_task_worktree(
                &repo,
                "99999999-dead-beef",
                "Other thing",
                Some("cm-sub/never-existed-0000000"),
            )
            .unwrap();
            assert_eq!(fresh.branch, "cm-sub/other-thing-9999999");
            assert!(fresh.recreated.is_none());
            assert_eq!(fresh.base_ref, "main");
        });
    }

    #[test]
    fn branch_dir_mapping_round_trips() {
        with_home(|home| {
            let repo = home.join("code/projects/myrepo");
            let base = home.join(".cm/worktrees");
            assert_eq!(
                branch_for_worktree_dir(&repo, &base.join("cm-sub-a-b-c-1234567")).as_deref(),
                Some("cm-sub/a-b-c-1234567")
            );
            assert_eq!(
                branch_for_worktree_dir(&repo, &base.join("myrepo-fix-bug")).as_deref(),
                Some("cm/fix-bug")
            );
            assert_eq!(branch_for_worktree_dir(&repo, &base.join("otherrepo-fix-bug")), None);
            assert_eq!(branch_for_worktree_dir(&repo, &base.join("cm-sub-")), None);
            assert_eq!(
                worktree_dir_for_branch(&repo, "cm-sub/a-b-c-1234567"),
                Some(base.join("cm-sub-a-b-c-1234567"))
            );
            assert_eq!(
                worktree_dir_for_branch(&repo, "cm/fix-bug"),
                Some(base.join("myrepo-fix-bug"))
            );
            assert_eq!(worktree_dir_for_branch(&repo, "feature/x"), None);
        });
    }
}
