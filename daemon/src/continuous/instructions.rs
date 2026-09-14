//! Standing instructions for a continuous task, materialized as the engine's
//! own project-instruction file in the task worktree.
//!
//! Why: every fire used to paste the whole orchestrator prompt (16–40 KB) into
//! the PTY, so a persistent session carried up to `compact_every` copies of the
//! same text between compactions. Both engines inject a per-checkout
//! instruction file into the prompt prefix instead — cached, re-injected after
//! compaction (verified on Codex 0.153.4: the first turn after every
//! `compacted` event re-sends the project doc), and never part of the
//! transcript. The fire prompt then shrinks to a one-paragraph dispatch.
//!
//! Engine mapping (verified 2026-09-14):
//! - Codex: `AGENTS.override.md` REPLACES `AGENTS.md` for the checkout, so the
//!   file carries the repository doc (resolved through the worktree's
//!   `AGENTS.md`, usually a symlink to `CLAUDE.md`) followed by the standing
//!   section. `project_doc_max_bytes` is raised at spawn (mcp_config) because
//!   the repo doc alone already exceeds the 32 KiB default.
//! - Claude Code: `CLAUDE.local.md` is additive to `CLAUDE.md`; only the
//!   standing section is written.
//! - Bash: nothing.
//!
//! The file is CM-owned: it starts with [`MARKER`], is rewritten only when its
//! content changes, is excluded through `.git/info/exclude` so the checkout
//! stays clean, and is never written over a file that lacks the marker.
use super::task::{ContinuousTask, Engine};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const MARKER: &str = "<!-- cm-standing-instructions v1 -->";

pub fn file_name(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::Codex => Some("AGENTS.override.md"),
        Engine::Claude => Some("CLAUDE.local.md"),
        Engine::Bash => None,
    }
}

/// Where the standing file lives for this task, if the engine has one.
pub fn path_for(task: &ContinuousTask) -> Option<PathBuf> {
    file_name(task.engine).map(|n| Path::new(&task.worktree_path).join(n))
}

/// The repository's own instructions that an override must carry along
/// (Codex only). Follows the worktree's `AGENTS.md`, which is normally a
/// symlink to `CLAUDE.md`; falls back to `CLAUDE.md` directly.
fn repo_doc(worktree: &Path, engine: Engine) -> Option<String> {
    if engine != Engine::Codex {
        return None;
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        if let Ok(s) = std::fs::read_to_string(worktree.join(name)) {
            if !s.trim().is_empty() {
                return Some(s);
            }
        }
    }
    None
}

pub fn render(task: &ContinuousTask, repo: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(MARKER);
    out.push_str(&format!(" task={} engine={:?} -->\n", task.task_id, task.engine));
    if let Some(repo) = repo {
        out.push_str(repo.trim_end());
        out.push_str("\n\n");
    }
    out.push_str(&format!(
        "# CM continuous task standing instructions — {id}\n\n\
         These are the durable instructions for the `{id}` continuous orchestrator. \
         Claude Manager regenerates this file before every fire; do not edit it. \
         If you are not that orchestrator (a worker running in its checkout), ignore this section.\n\n\
         Each scheduled fire delivers only a short dispatch message carrying the run identity \
         (task_id, run_seq, fire_token); when it arrives, execute the cycle procedure below and \
         close the run with `report_done`. A chat wake (`[cm-chat …]`) or a worker-monitor \
         notice is NOT a fire: handle it per the channel contract without starting a cycle.\n\n",
        id = task.task_id
    ));
    out.push_str(task.standing_instructions.as_deref().unwrap_or("").trim());
    out.push('\n');
    out
}

/// Write (or refresh) the standing file for `task`. Returns the path written
/// or kept, `Ok(None)` when the task has no standing instructions or the
/// engine has no instruction file. Never overwrites a file CM did not write.
pub fn materialize(task: &ContinuousTask) -> io::Result<Option<PathBuf>> {
    let Some(path) = path_for(task) else {
        return Ok(None);
    };
    let Some(standing) = task.standing_instructions.as_deref() else {
        return Ok(None);
    };
    if standing.trim().is_empty() {
        return Ok(None);
    }
    let worktree = Path::new(&task.worktree_path);
    if !worktree.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("worktree {} is not a directory", worktree.display()),
        ));
    }
    let repo = repo_doc(worktree, task.engine);
    let content = render(task, repo.as_deref());
    match std::fs::read_to_string(&path) {
        Ok(existing) if existing == content => return Ok(Some(path)),
        Ok(existing) if !existing.starts_with(MARKER) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists and was not written by CM; move it aside or set standing_instructions to empty",
                    path.display()
                ),
            ));
        }
        _ => {}
    }
    ensure_excluded(worktree, path.file_name().and_then(|n| n.to_str()).unwrap_or(""));
    write_atomic(&path, content.as_bytes())?;
    Ok(Some(path))
}

/// Remove a CM-written standing file (task deleted or instructions cleared).
pub fn remove(task: &ContinuousTask) {
    if let Some(path) = path_for(task) {
        if let Ok(existing) = std::fs::read_to_string(&path) {
            if existing.starts_with(MARKER) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Append `/<name>` to the checkout's `info/exclude` (worktree-aware via
/// `git rev-parse --git-path`) when it is not already listed. Best-effort.
fn ensure_excluded(worktree: &Path, name: &str) {
    if name.is_empty() {
        return;
    }
    let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let exclude = if Path::new(&rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        worktree.join(rel)
    };
    let pattern = format!("/{name}");
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == pattern || l.trim() == name)
    {
        return;
    }
    if let Some(dir) = exclude.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&pattern);
    next.push('\n');
    let _ = std::fs::write(&exclude, next);
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuous::task::{RunMode, Schedule};

    fn task(engine: Engine, worktree: &Path) -> ContinuousTask {
        let mut t = ContinuousTask::new(
            "bug-triage".into(),
            "Bug".into(),
            "ws".into(),
            worktree.to_string_lossy().into_owned(),
            engine,
            RunMode::Persistent,
            Schedule::OnDemand,
            "Run one cycle now.".into(),
        );
        t.standing_instructions = Some("# Lane\nYou own bugs.\n\n## Cycle\n1. scan\n2. report_done".into());
        t
    }

    fn git_init(dir: &Path) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["init", "-q", "."])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init");
    }

    #[test]
    fn codex_override_carries_repo_doc_and_is_excluded_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Repo rules\nUse uv.\n").unwrap();
        std::os::unix::fs::symlink("CLAUDE.md", tmp.path().join("AGENTS.md")).unwrap();
        let t = task(Engine::Codex, tmp.path());
        let path = materialize(&t).unwrap().unwrap();
        assert_eq!(path.file_name().unwrap(), "AGENTS.override.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with(MARKER));
        assert!(body.contains("# Repo rules\nUse uv."), "{body}");
        assert!(body.contains("standing instructions — bug-triage"));
        assert!(body.contains("You own bugs."));
        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|l| l == "/AGENTS.override.md"), "{exclude}");
        // Clean tree: the override is ignored.
        let status = Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["status", "--porcelain", "--ignored=no"])
            .output()
            .unwrap();
        let st = String::from_utf8_lossy(&status.stdout);
        assert!(!st.contains("AGENTS.override.md"), "{st}");
        // Idempotent: same content, no rewrite (mtime-independent check via bytes).
        let before = std::fs::read(&path).unwrap();
        materialize(&t).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Exclude is not duplicated.
        materialize(&t).unwrap();
        let exclude = std::fs::read_to_string(tmp.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("/AGENTS.override.md").count(), 1);
        // Repo doc change is picked up on the next materialize.
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Repo rules v2\n").unwrap();
        materialize(&t).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("Repo rules v2"));
        // Remove only touches CM-written files.
        remove(&t);
        assert!(!path.exists());
    }

    #[test]
    fn claude_local_is_additive_and_foreign_files_are_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        git_init(tmp.path());
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Repo rules\n").unwrap();
        let t = task(Engine::Claude, tmp.path());
        let path = materialize(&t).unwrap().unwrap();
        assert_eq!(path.file_name().unwrap(), "CLAUDE.local.md");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("# Repo rules"), "Claude's local file must not duplicate CLAUDE.md");
        assert!(body.contains("You own bugs."));
        // A user-authored file at the same path is refused, not clobbered.
        std::fs::write(&path, "my own notes\n").unwrap();
        let err = materialize(&t).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "my own notes\n");
        remove(&t);
        assert!(path.exists(), "foreign file survives remove");
    }

    #[test]
    fn no_instructions_or_bash_engine_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mut t = task(Engine::Codex, tmp.path());
        t.standing_instructions = None;
        assert!(materialize(&t).unwrap().is_none());
        t.standing_instructions = Some("   ".into());
        assert!(materialize(&t).unwrap().is_none());
        let b = task(Engine::Bash, tmp.path());
        assert!(materialize(&b).unwrap().is_none());
        assert!(!tmp.path().join("AGENTS.override.md").exists());
    }
}
