use std::path::{Path, PathBuf};
use std::process::Command;

/// Base directory for all worktrees.
fn worktree_base() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
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

/// Create a git worktree for a task.
///
/// Returns the path to the new worktree directory.
/// Create a git worktree for a task.
///
/// If `start_branch` is provided, the worktree starts from that branch
/// (fetched from origin first). Otherwise creates a new `cm/<slug>` branch from HEAD.
///
/// Returns the path to the new worktree directory.
pub fn create_worktree(
    main_repo: &Path,
    task_slug: &str,
    start_branch: Option<&str>,
) -> anyhow::Result<PathBuf> {
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
        return Ok(worktree_path);
    }

    if let Some(start) = start_branch {
        // Fetch the branch first.
        let _ = Command::new("git")
            .arg("-C")
            .arg(main_repo)
            .args(["fetch", "origin", start])
            .output();

        // Create worktree on a new branch starting from the specified branch.
        let output = Command::new("git")
            .arg("-C")
            .arg(main_repo)
            .args(["worktree", "add"])
            .arg(&worktree_path)
            .args(["-b", &branch_name, &format!("origin/{}", start)])
            .output()?;

        if !output.status.success() {
            // Maybe the branch exists locally already, try that.
            let output2 = Command::new("git")
                .arg("-C")
                .arg(main_repo)
                .args(["worktree", "add"])
                .arg(&worktree_path)
                .args(["-b", &branch_name, start])
                .output()?;

            if !output2.status.success() {
                // Try just checking out the start branch directly.
                let output3 = Command::new("git")
                    .arg("-C")
                    .arg(main_repo)
                    .args(["worktree", "add"])
                    .arg(&worktree_path)
                    .arg(start)
                    .output()?;

                if !output3.status.success() {
                    let stderr = String::from_utf8_lossy(&output3.stderr);
                    anyhow::bail!("git worktree add failed: {}", stderr.trim());
                }
            }
        }
    } else {
        // Create new branch from HEAD.
        let output = Command::new("git")
            .arg("-C")
            .arg(main_repo)
            .args(["worktree", "add"])
            .arg(&worktree_path)
            .args(["-b", &branch_name])
            .output()?;

        if !output.status.success() {
            let output2 = Command::new("git")
                .arg("-C")
                .arg(main_repo)
                .args(["worktree", "add"])
                .arg(&worktree_path)
                .arg(&branch_name)
                .output()?;

            if !output2.status.success() {
                let stderr = String::from_utf8_lossy(&output2.stderr);
                anyhow::bail!("git worktree add failed: {}", stderr.trim());
            }
        }
    }

    Ok(worktree_path)
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

/// Remove a git worktree.
pub fn remove_worktree(main_repo: &Path, worktree_path: &Path) {
    let _ = Command::new("git")
        .arg("-C")
        .arg(main_repo)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_path)
        .output();
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
