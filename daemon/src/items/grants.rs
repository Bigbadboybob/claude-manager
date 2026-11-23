//! Launch grants (doc/org-model.md §7.4, S3).
//!
//! The owner-role holder of a placed item may `start_session(task_id=…)` /
//! `item_resolve(launch)` an assistant for that task even where the scope
//! rule would refuse (a taskless caller, or a task in another tree). The API
//! writes the grant when the owner changes; every board heartbeat reply
//! carries this daemon's full live set, which replaces API grants and is
//! persisted beside the messaging store, so a brain restart admits the same
//! launches before its first beat (fail-static). It is not a task-parent or
//! creator edge: a TUI tree push or a restart cannot drop it, and it grants
//! launch only, not control of the task's other sessions. Locally issued
//! proposal grants survive heartbeat replacement and share the same store.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchGrant {
    #[serde(default)]
    pub proposal: bool,
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
                    proposal: false,
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

/// Replace the cache and persist it. Failed writes remain retryable.
pub fn replace(cache: &mut Vec<LaunchGrant>, root: &Path, mut grants: Vec<LaunchGrant>) -> std::io::Result<bool> {
    grants.extend(cache.iter().filter(|g| g.proposal).cloned());
    persist(cache, root, grants)
}

/// Persist both API grants and locally issued proposal grants atomically.
pub fn persist(cache: &mut Vec<LaunchGrant>, root: &Path, grants: Vec<LaunchGrant>) -> std::io::Result<bool> {
    let changed = *cache != grants;
    *cache = grants;
    // A previous write may have failed after the in-memory cache changed.
    // Compare against the actual file, so an unchanged retry still repairs it.
    let saved = std::fs::read(path(root)).ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<LaunchGrant>>(&bytes).ok());
    if saved.as_ref() == Some(cache) {
        // A prior durable write may have renamed successfully and then failed
        // its file/directory sync. Matching bytes alone do not prove durability.
        std::fs::File::open(path(root))?.sync_all()?;
        std::fs::File::open(root)?.sync_all()?;
        return Ok(changed);
    }
    std::fs::create_dir_all(root)?;
    let data = serde_json::to_string_pretty(cache).map_err(std::io::Error::other)?;
    crate::state::write_json_atomic(&path(root), &data, true)?;
    Ok(changed)
}

/// The grant letting `session_uid` launch for `task_id`, if any.
pub fn find<'a>(cache: &'a [LaunchGrant], session_uid: &str, task_id: &str) -> Option<&'a LaunchGrant> {
    cache.iter().find(|g| g.session_uid == session_uid && g.task_id == task_id)
}

/// One line per launch admitted by a grant, like the §6.3 bootstrap rules.
pub fn log_use(grant: &LaunchGrant, caller: &str) {
    if grant.proposal {
        eprintln!("cm-daemon: launch grant: {caller} launched proposed task {}", grant.task_id);
        return;
    }
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
        assert!(replace(&mut cache, root.path(), from_reply(&reply).unwrap()).unwrap());
        assert!(find(&cache, "me-uid", "t-1").is_some());
        assert!(find(&cache, "me-uid", "t-2").is_none());
        assert_eq!(load(root.path()), cache, "a restart reads the same set");
        // An unchanged set writes nothing; a revoke is a row that disappears.
        assert!(!replace(&mut cache, root.path(), from_reply(&reply).unwrap()).unwrap());
        assert!(replace(&mut cache, root.path(), from_reply(&json!({"launch_grants": []})).unwrap()).unwrap());
        assert!(load(root.path()).is_empty());
        // An older API sends no field: leave the cache alone.
        assert!(from_reply(&json!({"pushes": []})).is_none());
    }

    #[test]
    fn failed_write_is_reported_and_an_unchanged_cache_retries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blocked");
        std::fs::write(&root, b"not a directory").unwrap();
        let mut cache = Vec::new();
        let grant = LaunchGrant { proposal: true, task_id: "task".into(), session_uid: "caller".into(), ..Default::default() };
        assert!(persist(&mut cache, &root, vec![grant.clone()]).is_err());
        assert_eq!(cache, vec![grant.clone()]);
        std::fs::remove_file(&root).unwrap();
        assert!(!persist(&mut cache, &root, vec![grant.clone()]).unwrap());
        assert_eq!(load(&root), vec![grant]);
    }
}
