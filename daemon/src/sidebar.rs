//! Durable agent requests for Owner's viewer-owned sidebar organization.
//! Publish/ack is Operator-only; assignments use ordinary session scope.
use crate::control::{
    auth,
    protocol::{Caller, ErrorCode, Request, Response},
};
use crate::{
    manifest::{ManifestDiff, SidebarSection},
    state::DaemonState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf};

type Result<T> = std::result::Result<T, (ErrorCode, String)>;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Assignment {
    pub id: String,
    pub workspace_id: String,
    pub session_uid: String,
    /// null = Auto; empty string = None; otherwise a stable section ID.
    pub choice: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Receipt {
    pub request_id: String,
    pub status: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub session_ids: Vec<String>,
    pub choice: Option<String>,
    pub effective_section_id: Option<String>,
    pub receipt: Option<Receipt>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub sections: Vec<SidebarSection>,
    pub workspaces: BTreeMap<String, Workspace>,
}
#[derive(Default, Serialize, Deserialize)]
struct Store {
    publication: Option<Publication>,
    pending: BTreeMap<String, Assignment>,
}
fn path(state: &DaemonState) -> PathBuf {
    state
        .daemon_sessions_path
        .clone()
        .unwrap_or_else(crate::state::default_daemon_sessions_path)
        .with_file_name("sidebar-sections.json")
}
fn read(state: &DaemonState) -> Result<Store> {
    match std::fs::read(path(state)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| (ErrorCode::Internal, format!("read sidebar state: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(e) => Err((ErrorCode::Internal, format!("read sidebar state: {e}"))),
    }
}
fn save(state: &DaemonState, store: &Store) -> Result<()> {
    let data = serde_json::to_string(store).map_err(|e| (ErrorCode::Internal, e.to_string()))?;
    if data.len() > 2 * 1024 * 1024 {
        return Err((
            ErrorCode::Conflict,
            "Sidebar state is full; existing requests retained".into(),
        ));
    }
    crate::state::write_json_atomic(&path(state), &data, true)
        .map_err(|e| (ErrorCode::Internal, format!("persist sidebar state: {e}")))
}
pub fn snapshot(state: &DaemonState) -> Result<BTreeMap<String, Assignment>> {
    Ok(read(state)?.pending)
}

fn live(state: &DaemonState, uid: &str) -> bool {
    state.lookup_session_any(uid).is_some()
        && !state
            .sessions
            .get(uid)
            .is_some_and(|s| s.last_exit.kernel_set())
}
// The legacy unified view omits TUI-owned workspace IDs; read that snapshot's
// actual field. Native sessions remain authoritative when both maps contain UID.
fn workspace(state: &DaemonState, uid: &str) -> Option<String> {
    if let Some(s) = state.sessions.get(uid) {
        return Some(s.workspace_id.clone()).filter(|id| !id.is_empty());
    }
    state
        .tui_sessions
        .get(uid)
        .and_then(|s| s.workspace_id.clone())
        .filter(|id| !id.is_empty())
}
fn allowed(state: &DaemonState, caller: &Caller, uid: &str) -> bool {
    if !live(state, uid) {
        return false;
    }
    let Caller::Session(c) = caller else {
        return true;
    };
    if !live(state, &c.session_uid) {
        return false;
    }
    let cview = state.lookup_session_any(&c.session_uid).unwrap();
    let target = state.lookup_session_any(uid).unwrap();
    if c.session_uid == uid || cview.global_perms {
        return true;
    }
    match cview.task_id {
        Some(task) => target
            .task_id
            .as_ref()
            .is_some_and(|tid| auth::task_is_self_or_descendant_of(&state.task_tree, tid, &task)),
        None => {
            workspace(state, &c.session_uid).is_some()
                && workspace(state, &c.session_uid) == workspace(state, uid)
        }
    }
}
fn session_ids(state: &DaemonState) -> Vec<String> {
    let mut ids: Vec<_> = state
        .sessions
        .keys()
        .chain(state.tui_sessions.keys())
        .cloned()
        .collect();
    ids.sort();
    ids.dedup();
    ids
}
fn workspace_ids(state: &DaemonState, ws: &str) -> Vec<String> {
    session_ids(state)
        .into_iter()
        .filter(|uid| live(state, uid) && workspace(state, uid).as_deref() == Some(ws))
        .collect()
}
fn authenticate(state: &DaemonState, caller: &Caller) -> Result<()> {
    if let Caller::Session(c) = caller {
        if !live(state, &c.session_uid) {
            return Err((
                ErrorCode::Unauthorized,
                "sidebar tools require a live session".into(),
            ));
        }
    }
    Ok(())
}
fn list(state: &DaemonState, req: &Request) -> Result<Value> {
    authenticate(state, &req.caller)?;
    let store = read(state)?;
    let mut rows = BTreeMap::new();
    for uid in session_ids(state)
        .into_iter()
        .filter(|uid| allowed(state, &req.caller, uid))
    {
        let Some(ws) = workspace(state, &uid) else {
            continue;
        };
        let members = workspace_ids(state, &ws);
        let observed = store.publication.as_ref().and_then(|p| {
            p.workspaces
                .get(&ws)
                .or_else(|| p.workspaces.values().find(|w| w.session_ids.contains(&uid)))
        });
        rows.insert(ws.clone(), json!({
            "session_ids": members.iter().filter(|uid| allowed(state, &req.caller, uid)).collect::<Vec<_>>(),
            "can_assign": members.iter().all(|uid| allowed(state, &req.caller, uid)),
            "observed": observed.map(|w| json!({"choice":w.choice,"effective_section_id":w.effective_section_id,"receipt":w.receipt})),
            "pending": store.pending.get(&ws).map(|a| json!({"id":a.id,"choice":a.choice})),
        }));
    }
    Ok(json!({"viewer_published":store.publication.is_some(),
        "sections":store.publication.as_ref().map(|p| &p.sections).cloned().unwrap_or_default(),
        "workspaces":rows, "scope":"this daemon; workspace-level membership",
        "note":"Observed choices are the last viewer publication; pending requests apply when the updated viewer connects."}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignParams {
    session_id: Option<String>,
    section: String,
}
fn assign(state: &DaemonState, req: &Request) -> Result<Value> {
    authenticate(state, &req.caller)?;
    let p: AssignParams = serde_json::from_value(req.params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, e.to_string()))?;
    let uid = p
        .session_id
        .or_else(|| match &req.caller {
            Caller::Session(c) => Some(c.session_uid.clone()),
            _ => None,
        })
        .ok_or((
            ErrorCode::InvalidParams,
            "session_id is required for Operator".into(),
        ))?;
    if !allowed(state, &req.caller, &uid) {
        return Err((
            ErrorCode::Unauthorized,
            "Target is not a live session in your scope on this daemon".into(),
        ));
    }
    let ws =
        workspace(state, &uid).ok_or((ErrorCode::Conflict, "Target has no workspace".into()))?;
    let members = workspace_ids(state, &ws);
    if !members.iter().all(|id| allowed(state, &req.caller, id)) {
        return Err((ErrorCode::Unauthorized, "Section membership affects the whole workspace, which includes sessions outside your scope".into()));
    }
    let mut store = read(state)?;
    let publication = store.publication.as_ref().ok_or((ErrorCode::Conflict,
        "No updated viewer has published sections yet. Install/reopen the updated laptop TUI first.".into()))?;
    let choice = match p.section.as_str() {
        "auto" => None,
        "none" => Some(String::new()),
        reference => {
            let by_id = publication.sections.iter().find(|s| s.id == reference);
            let matches: Vec<_> = publication
                .sections
                .iter()
                .filter(|s| s.name == reference)
                .collect();
            let section = by_id.or_else(|| if matches.len() == 1 { Some(matches[0]) } else { None })
                .ok_or((ErrorCode::InvalidParams, "Unknown or ambiguous section; use an ID from list_sidebar_sections, or auto/none".into()))?;
            Some(section.id.clone())
        }
    };
    if let Some(old) = store.pending.get(&ws).filter(|a| a.choice == choice) {
        return Ok(json!({"status":"queued", "assignment":old, "workspace_session_ids":members}));
    }
    let assignment = Assignment {
        id: uuid::Uuid::new_v4().to_string(),
        workspace_id: ws.clone(),
        session_uid: uid.clone(),
        choice,
    };
    store.pending.insert(ws, assignment.clone());
    save(state, &store)?;
    state.manifest_watcher.broadcast(ManifestDiff::Updated {
        uid,
        entry: json!({"sidebar_assignment":assignment}),
    });
    Ok(
        json!({"status":"queued", "assignment":assignment, "workspace_session_ids":members,
        "note":"Workspace-level display change; the viewer applies it without restarting sessions. Auto descendants follow existing inheritance."}),
    )
}
fn publish(state: &DaemonState, req: &Request) -> Result<Value> {
    let p: Publication = serde_json::from_value(req.params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, e.to_string()))?;
    let mut ids = std::collections::HashSet::new();
    if p.sections.len() > 1024
        || p.sections.iter().any(|s| {
            s.id.is_empty() || s.id.len() > 256 || s.name.len() > 1024 || !ids.insert(&s.id)
        })
    {
        return Err((
            ErrorCode::InvalidParams,
            "Invalid or duplicate section IDs".into(),
        ));
    }
    let mut store = read(state)?;
    // Compare request IDs: a delayed publication cannot acknowledge a newer move.
    let before = store.pending.len();
    store.pending.retain(|_, a| {
        !p.workspaces.values().any(|w| {
            w.receipt.as_ref().is_some_and(|r| {
                r.request_id == a.id && matches!(r.status.as_str(), "applied" | "section_deleted")
            })
        })
    });
    if store.publication.as_ref() != Some(&p) || store.pending.len() != before {
        store.publication = Some(p);
        save(state, &store)?;
    }
    Ok(json!({"ok":true}))
}
/// Dispatch holds the state mutex and the restart mutation barrier.
pub fn dispatch(state: &DaemonState, req: &Request) -> Response {
    let result = match req.method.as_str() {
        "sidebar.list" => list(state, req),
        "sidebar.assign" => assign(state, req),
        "sidebar.publish" => publish(state, req),
        _ => unreachable!(),
    };
    match result {
        Ok(v) => Response::ok(req.id.clone(), v),
        Err((code, message)) => Response::err(req.id.clone(), code, message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::dispatch::dispatch_request;
    use std::sync::{Arc, Mutex};
    fn fixture() -> (tempfile::TempDir, Arc<Mutex<DaemonState>>) {
        let dir = tempfile::tempdir().unwrap();
        let mut s = DaemonState::new();
        s.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        for (uid, ws, task) in [
            ("parent", "parent-ws", "p"),
            ("scout", "scout-ws", "c"),
            ("other", "other-ws", "x"),
        ] {
            s.tui_sessions.insert(
                uid.into(),
                serde_json::from_value(json!({"uid":uid,"workspace_id":ws,"task_id":task}))
                    .unwrap(),
            );
        }
        s.task_tree.insert("c".into(), Some("p".into()));
        (dir, Arc::new(Mutex::new(s)))
    }
    fn call(s: &Arc<Mutex<DaemonState>>, method: &str, caller: Caller, params: Value) -> Response {
        dispatch_request(
            s,
            &Request {
                id: "test".into(),
                method: method.into(),
                caller,
                params,
            },
        )
        .into_response()
    }
    fn publication() -> Publication {
        Publication {
            sections: vec![
                SidebarSection {
                    id: "s1".into(),
                    name: "Swarm".into(),
                    ..Default::default()
                },
                SidebarSection {
                    id: "s2".into(),
                    name: "Other".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }
    fn publish_fixture(s: &Arc<Mutex<DaemonState>>) {
        assert!(
            call(
                s,
                "sidebar.publish",
                Caller::operator("owner"),
                json!(publication())
            )
            .ok
        );
    }
    fn assign_to(s: &Arc<Mutex<DaemonState>>, section: &str) -> Response {
        call(
            s,
            "sidebar.assign",
            Caller::session("parent"),
            json!({"session_id":"scout","section":section}),
        )
    }
    #[test]
    fn sidebar_scope_and_workspace_side_effects() {
        let _guard = crate::test_support::env_lock();
        let (_dir, s) = fixture();
        publish_fixture(&s);
        assert!(assign_to(&s, "Swarm").ok);
        let listing = call(&s, "sidebar.list", Caller::session("parent"), json!({}))
            .result
            .unwrap();
        assert!(listing["workspaces"].get("scout-ws").is_some());
        assert!(listing["workspaces"].get("other-ws").is_none());
        for caller in ["other", "unknown"] {
            assert_eq!(
                call(
                    &s,
                    "sidebar.assign",
                    Caller::session(caller),
                    json!({"session_id":"scout","section":"none"})
                )
                .error
                .unwrap()
                .code,
                ErrorCode::Unauthorized
            );
        }
        // A shared workspace cannot be rearranged through one permitted child
        // when that also changes an unrelated sibling's organization.
        s.lock()
            .unwrap()
            .tui_sessions
            .get_mut("other")
            .unwrap()
            .workspace_id = Some("scout-ws".into());
        assert_eq!(
            assign_to(&s, "none").error.unwrap().code,
            ErrorCode::Unauthorized
        );
        s.lock()
            .unwrap()
            .tui_sessions
            .get_mut("parent")
            .unwrap()
            .global_perms = true;
        assert!(assign_to(&s, "none").ok);
    }
    #[test]
    fn sidebar_catalogue_gate_validation_and_operator_only_publication() {
        let _guard = crate::test_support::env_lock();
        let (_dir, s) = fixture();
        assert_eq!(
            assign_to(&s, "auto").error.unwrap().code,
            ErrorCode::Conflict
        );
        assert_eq!(
            call(
                &s,
                "sidebar.publish",
                Caller::session("parent"),
                json!(publication())
            )
            .error
            .unwrap()
            .code,
            ErrorCode::Unauthorized
        );
        publish_fixture(&s);
        assert_eq!(
            assign_to(&s, "missing").error.unwrap().code,
            ErrorCode::InvalidParams
        );
        let mut p = publication();
        p.sections[1].name = "Swarm".into();
        assert!(call(&s, "sidebar.publish", Caller::operator("owner"), json!(p)).ok);
        assert_eq!(
            assign_to(&s, "Swarm").error.unwrap().code,
            ErrorCode::InvalidParams
        );
        assert!(assign_to(&s, "s1").ok);
        assert_eq!(
            call(
                &s,
                "sidebar.assign",
                Caller::session("scout"),
                json!({"section":"none","global_perms":true})
            )
            .error
            .unwrap()
            .code,
            ErrorCode::InvalidParams
        );
    }
    #[test]
    fn sidebar_pending_survives_restart_and_watch_reconnect() {
        let _guard = crate::test_support::env_lock();
        let (dir, s) = fixture();
        publish_fixture(&s);
        let result = assign_to(&s, "Swarm").result.unwrap();
        assert_eq!(result["status"], "queued");
        assert_eq!(result["assignment"]["choice"], "s1");
        // Identical pending retries coalesce without a new request identity.
        assert_eq!(
            assign_to(&s, "s1").result.unwrap()["assignment"]["id"],
            result["assignment"]["id"]
        );
        let mut restored = DaemonState::new();
        restored.daemon_sessions_path = Some(dir.path().join("daemon-sessions.json"));
        assert_eq!(
            snapshot(&restored).unwrap()["scout-ws"].id,
            result["assignment"]["id"].as_str().unwrap()
        );
        let restored = Arc::new(Mutex::new(restored));
        match dispatch_request(
            &restored,
            &Request {
                id: "watch".into(),
                method: "manifest.watch".into(),
                caller: Caller::operator("owner"),
                params: json!({}),
            },
        ) {
            crate::control::dispatch::DispatchOutcome::ManifestWatchStream { handle, .. } => {
                assert_eq!(
                    handle.initial_snapshot["sidebar_assignments"]["scout-ws"]["id"],
                    result["assignment"]["id"]
                );
            }
            _ => panic!("watch did not open"),
        }
    }
    #[test]
    fn sidebar_stale_publication_cannot_ack_newer_move_and_deleted_section_is_reported() {
        let _guard = crate::test_support::env_lock();
        let (_dir, s) = fixture();
        publish_fixture(&s);
        let first: Assignment =
            serde_json::from_value(assign_to(&s, "s1").result.unwrap()["assignment"].clone())
                .unwrap();
        let second: Assignment =
            serde_json::from_value(assign_to(&s, "none").result.unwrap()["assignment"].clone())
                .unwrap();
        let mut p = publication();
        // Wrapper ID deliberately differs from daemon workspace ID.
        p.workspaces.insert(
            "viewer-wrapper".into(),
            Workspace {
                session_ids: vec!["scout".into()],
                choice: Some("s1".into()),
                effective_section_id: Some("s1".into()),
                receipt: Some(Receipt {
                    request_id: first.id,
                    status: "applied".into(),
                }),
            },
        );
        assert!(call(&s, "sidebar.publish", Caller::operator("owner"), json!(p)).ok);
        assert_eq!(
            snapshot(&s.lock().unwrap()).unwrap()["scout-ws"].id,
            second.id
        );
        p.workspaces.get_mut("viewer-wrapper").unwrap().receipt = Some(Receipt {
            request_id: second.id,
            status: "section_deleted".into(),
        });
        assert!(call(&s, "sidebar.publish", Caller::operator("owner"), json!(p)).ok);
        assert!(snapshot(&s.lock().unwrap()).unwrap().is_empty());
        let list = call(&s, "sidebar.list", Caller::session("parent"), json!({}))
            .result
            .unwrap();
        assert_eq!(
            list["workspaces"]["scout-ws"]["observed"]["receipt"]["status"],
            "section_deleted"
        );
        assert!(assign_to(&s, "auto").result.unwrap()["assignment"]["choice"].is_null());
    }
    #[test]
    fn sidebar_corrupt_storage_fails_without_false_success() {
        let _guard = crate::test_support::env_lock();
        let (_dir, s) = fixture();
        publish_fixture(&s);
        let p = path(&s.lock().unwrap());
        std::fs::write(&p, b"broken").unwrap();
        assert_eq!(
            assign_to(&s, "none").error.unwrap().code,
            ErrorCode::Internal
        );
        assert_eq!(std::fs::read(p).unwrap(), b"broken");
    }
    #[test]
    fn sidebar_native_cloud_continuous_and_local_self_keep_processes_untouched() {
        let _guard = crate::test_support::env_lock();
        let (_dir, s) = fixture();
        publish_fixture(&s);
        let mut spawn =
            crate::session::SpawnParams::new("native", "Cloud continuous", "/bin/sleep");
        spawn.args = vec!["30".into()];
        spawn.workspace_id = "native-ws".into();
        spawn.continuous_task_id = Some("continuous".into());
        s.lock().unwrap().sessions.insert(
            "native".into(),
            crate::session::DaemonSession::spawn(spawn).unwrap(),
        );
        for uid in ["native", "parent"] {
            assert!(
                call(
                    &s,
                    "sidebar.assign",
                    Caller::session(uid),
                    json!({"section":"none"})
                )
                .ok
            );
            assert!(live(&s.lock().unwrap(), uid));
        }
    }
}
