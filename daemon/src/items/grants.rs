//! Launch grants (doc/org-model.md §7.4, S3).
//!
//! The owner-role holder of a placed item may `start_session(task_id=…)` /
//! `item_resolve(launch)` an assistant for that task even where the scope
//! rule would refuse (a taskless caller, or a task in another tree). The API
//! writes the grant when the owner changes; every board heartbeat reply
//! carries this daemon's full live set, which replaces the cache here and is
//! persisted beside the messaging store, so a brain restart admits the same
//! launches before its first beat (fail-static). It is not a task-parent or
//! creator edge: a TUI tree push or a restart cannot drop it, and it grants
//! launch only, not control of the task's other sessions.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchGrant {
    pub task_id: String,
    pub session_uid: String,
    #[serde(default)]
    pub pid: String,
    #[serde(default)]
    pub board: Option<String>,
    #[serde(default)]
    pub n: Option<i64>,
}

pub fn path(root: &Path) -> PathBuf {
    root.join("launch-grants.json")
}

/// The persisted set; empty when there is none or it cannot be read.
pub fn load(root: &Path) -> Vec<LaunchGrant> {
    std::fs::read(path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The grants a heartbeat reply carries, or None from an API that predates
/// them (the cache is then left as it is).
pub fn from_reply(reply: &Value) -> Option<Vec<LaunchGrant>> {
    let rows = reply.get("launch_grants")?.as_array()?;
    Some(
        rows.iter()
            .filter_map(|r| {
                Some(LaunchGrant {
                    task_id: r["task_id"].as_str()?.to_string(),
                    session_uid: r["session_uid"].as_str()?.to_string(),
                    pid: r["pid"].as_str().unwrap_or_default().to_string(),
                    board: r["board"].as_str().map(str::to_string),
                    n: r["n"].as_i64(),
                })
            })
            .collect(),
    )
}

/// Replace the cache; persist when it changed. Returns whether it did.
pub fn replace(cache: &mut Vec<LaunchGrant>, root: &Path, grants: Vec<LaunchGrant>) -> bool {
    if *cache == grants {
        return false;
    }
    *cache = grants;
    let tmp = path(root).with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec_pretty(cache) {
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path(root));
        }
    }
    true
}

/// The grant letting `session_uid` launch for `task_id`, if any.
pub fn find<'a>(cache: &'a [LaunchGrant], session_uid: &str, task_id: &str) -> Option<&'a LaunchGrant> {
    cache.iter().find(|g| g.session_uid == session_uid && g.task_id == task_id)
}

/// One line per launch admitted by a grant, like the §6.3 bootstrap rules.
pub fn log_use(grant: &LaunchGrant, caller: &str) {
    eprintln!(
        "cm-daemon: launch grant: {caller} launched for task {} as the owner of {}#{}",
        grant.task_id,
        grant.board.as_deref().unwrap_or("?"),
        grant.n.map(|n| n.to_string()).unwrap_or_else(|| "?".into()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_reply_replaces_the_cache_and_persists_it() {
        let root = tempfile::tempdir().unwrap();
        let mut cache = Vec::new();
        let reply = json!({"launch_grants": [
            {"task_id": "t-1", "session_uid": "me-uid", "pid": "agent:d1:me-uid", "board": "sfd", "n": 4}
        ]});
        assert!(replace(&mut cache, root.path(), from_reply(&reply).unwrap()));
        assert!(find(&cache, "me-uid", "t-1").is_some());
        assert!(find(&cache, "me-uid", "t-2").is_none());
        assert_eq!(load(root.path()), cache, "a restart reads the same set");
        // An unchanged set writes nothing; a revoke is a row that disappears.
        assert!(!replace(&mut cache, root.path(), from_reply(&reply).unwrap()));
        assert!(replace(&mut cache, root.path(), from_reply(&json!({"launch_grants": []})).unwrap()));
        assert!(load(root.path()).is_empty());
        // An older API sends no field: leave the cache alone.
        assert!(from_reply(&json!({"pushes": []})).is_none());
    }
}
