//! Build script: bake the git short sha into the binary as
//! `CM_BUILD_GIT_HASH`, consumed by `cm_daemon::build_id()` and
//! surfaced on `daemon.health` as half of `build_id`
//! (DESIGN_SEAMLESS_RESTART phase 6, restart-sequence step 7:
//! cm-redeploy's fire-and-verify polls health after `daemon.restart`
//! and needs a compile-time identity to compare).
//!
//! Honesty rules:
//! - `git` absent, not a repo, or any failure → the literal
//!   `"unknown"` — never a fabricated sha, never a build failure
//!   (release tarballs and vendored builds must still compile).
//! - A dirty tree keeps the HEAD sha unchanged, so two builds of the
//!   same commit (the common local iterate-uncommitted loop — this
//!   project deliberately leaves work uncommitted) share a build_id.
//!   That is why cm-redeploy's verify keys on `reexec_generation`
//!   (which increments on EVERY committed swap) and only REPORTS the
//!   build_id transition, warning when it is unchanged.
//!
//! Rerun discipline: printing any `rerun-if-changed` narrows cargo's
//! default (rerun on any package change) to exactly the named paths,
//! so we name the git files whose content the emitted value depends
//! on — `HEAD` plus the current branch ref when one resolves
//! (`--git-path` answers correctly from worktrees, where `.git` is a
//! file). Source edits don't move the sha, so not rerunning on them
//! is correct, and `build.rs` itself is always an implicit trigger.

use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn main() {
    let hash = git_output(&["rev-parse", "--short", "HEAD"])
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CM_BUILD_GIT_HASH={}", hash);

    // Re-run when HEAD moves (checkout) or the checked-out branch's
    // ref advances (commit). Best-effort: if git can't answer, no
    // rerun-if lines are emitted for it and the "unknown" fallback
    // stays honest.
    if let Some(head) = git_output(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={}", head);
    }
    if let Some(sym) = git_output(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(ref_path) = git_output(&["rev-parse", "--git-path", &sym]) {
            println!("cargo:rerun-if-changed={}", ref_path);
        }
    }
}
