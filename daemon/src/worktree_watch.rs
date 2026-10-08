//! Missing-checkout watch: a live agent session whose worktree disappeared
//! (typically it ran `git worktree remove` on its own cwd) gets the checkout
//! re-created from its branch and a loud notice telling it not to do that.
//!
//! Pre-fix (2026-10-08) such a session's turns failed with an invalid cwd,
//! its provider auth helper hit ENOENT, and a later A-R failed silently.
//! `send_input` and revive also heal on demand; this catches a session that
//! nobody is talking to yet. Once a minute; one `stat` per session.

use crate::state::DaemonState;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const EVERY: Duration = Duration::from_secs(60);

/// The notice an affected session receives (its text starts with its marker).
pub(crate) fn notice(uid: &str, path: &std::path::Path, healed: &serde_json::Value) -> (String, String) {
    let marker = format!("[cm-worktree {uid}]");
    let how = healed["worktree_recreated"]["summary"]
        .as_str()
        .map(|s| format!("CM re-created it ({s})."))
        .or_else(|| healed["worktree_warning"].as_str().map(|w| format!("CM could not re-create it: {w}.")))
        .unwrap_or_else(|| "CM re-created it.".into());
    let text = format!(
        "{marker} Your checkout {} was deleted while this session was running in it. {how} \
         Never remove the worktree your own session runs in (no `git worktree remove` on your cwd, \
         even under the disk rule): ask Owner to close the workspace with Alt+Shift+w, which \
         cleans up safely after the session ends.",
        path.display()
    );
    (marker, text)
}

pub fn start(state: &Arc<Mutex<DaemonState>>) {
    let state = Arc::clone(state);
    let _ = std::thread::Builder::new().name("worktree-watch".into()).spawn(move || loop {
        std::thread::sleep(EVERY);
        // Capture under the lock, then release it before any filesystem or Git work.
        let (root, sessions): (PathBuf, Vec<(String, PathBuf)>) = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            let sessions = s
                .sessions
                .values()
                .filter(|v| matches!(v.session_type.as_str(), "claude-code" | "codex"))
                .filter_map(|v| {
                    let path = s.workspaces.get(&v.workspace_id)?.worktree_path.clone()?;
                    Some((v.uid.clone(), path))
                })
                .collect();
            (s.messaging_root.clone(), sessions)
        };
        for (uid, path) in sessions {
            if path.is_dir() {
                continue;
            }
            let Some(healed) = crate::control::methods::heal_missing_session_worktree(&state, &uid) else {
                continue;
            };
            let (marker, text) = notice(&uid, &path, &healed);
            let id = format!("worktree-missing:{uid}:{}", path.display());
            if let Err(e) = crate::notifications::publish(&root, &uid, &id, "cm", &text, &marker) {
                eprintln!("cm-daemon: worktree notice to {uid}: {e}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_notice_starts_with_its_marker_and_says_what_happened() {
        let (marker, text) = notice("u1", std::path::Path::new("/w/x"),
                                    &json!({"worktree_recreated": {"summary": "branch cm-sub/x at abc"}}));
        assert_eq!(marker, "[cm-worktree u1]");
        assert!(text.starts_with(&marker));
        assert!(text.contains("re-created it (branch cm-sub/x at abc)"));
        assert!(text.contains("Alt+Shift+w"));
        let (_, failed) = notice("u1", std::path::Path::new("/w/x"), &json!({"worktree_warning": "no branch"}));
        assert!(failed.contains("could not re-create it: no branch"));
    }
}
