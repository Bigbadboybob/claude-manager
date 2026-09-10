//! Remote subsection requests use the existing watch stream and background push worker.
use super::*;
use cm_daemon::{
    host_id::HostId,
    sidebar::{Assignment, Publication, Receipt, Workspace as PublishedWorkspace},
};
use std::collections::BTreeMap;

fn receipt_key(host: &HostId, workspace: &str) -> String {
    serde_json::to_string(&(host, workspace)).expect("sidebar receipt key")
}
impl App {
    pub(crate) fn sidebar_publications(&self) -> HashMap<HostId, Publication> {
        self.hosts
            .hosts
            .iter()
            .map(|h| {
                let workspaces = self
                    .workspaces
                    .iter()
                    .enumerate()
                    .filter(|(_, w)| w.host_id == h.id)
                    .map(|(wi, w)| {
                        (
                            w.id.clone(),
                            PublishedWorkspace {
                                session_ids: w
                                    .sessions
                                    .iter()
                                    .filter(|s| s.host_id == h.id)
                                    .map(|s| s.uid.clone())
                                    .collect(),
                                choice: self.workspace_sections.get(&w.id).cloned(),
                                effective_section_id: self.section_of_workspace(wi),
                                receipt: self
                                    .sidebar_receipts
                                    .get(&receipt_key(&h.id, &w.id))
                                    .cloned(),
                            },
                        )
                    })
                    .collect();
                (
                    h.id.clone(),
                    Publication {
                        sections: self.sections.clone(),
                        workspaces,
                    },
                )
            })
            .collect()
    }
    pub(crate) fn push_sidebar_to_daemon(&self) {
        if self.sessions_restored {
            self.push_worker.push_sidebar(self.sidebar_publications());
        }
    }
    pub(crate) fn receive_sidebar_snapshot(
        &mut self,
        host: &HostId,
        requests: &BTreeMap<String, Assignment>,
    ) {
        self.sidebar_pending.retain(|(h, _), _| h != host);
        for request in requests.values() {
            self.sidebar_pending.insert(
                (host.clone(), request.workspace_id.clone()),
                request.clone(),
            );
        }
        self.apply_pending_sidebar_assignments();
    }
    pub(crate) fn receive_sidebar_assignment(&mut self, host: &HostId, request: Assignment) {
        self.sidebar_pending
            .insert((host.clone(), request.workspace_id.clone()), request);
        self.apply_pending_sidebar_assignments();
    }
    pub(crate) fn apply_pending_sidebar_assignments(&mut self) {
        if Instant::now() < self.sidebar_retry_at
            || !self.sessions_restored
            || self.sidebar_pending.is_empty()
            || self.defer_manifest_save.get()
        {
            return;
        }
        let pending = self.sidebar_pending.clone();
        for ((host, key), a) in pending {
            // Same daemon identity only. A viewer adoption wrapper can have a
            // different ID, so match its retained session UID as a fallback.
            let wi = self.workspaces.iter().position(|w| {
                w.host_id == host
                    && (w.id == a.workspace_id
                        || w.sessions
                            .iter()
                            .any(|s| s.host_id == host && s.uid == a.session_uid))
            });
            let Some(wi) = wi else {
                continue;
            }; // adoption may arrive later
            let ws_id = self.workspaces[wi].id.clone();
            let receipt_key = receipt_key(&host, &ws_id);
            if self
                .sidebar_receipts
                .get(&receipt_key)
                .is_some_and(|r| r.request_id == a.id)
            {
                self.push_sidebar_to_daemon();
                self.sidebar_pending.remove(&(host, key));
                continue;
            }
            let valid = a
                .choice
                .as_deref()
                .is_none_or(|s| s.is_empty() || self.section_index(s).is_some());
            let previous = self.workspace_sections.get(&ws_id).cloned();
            let old_receipt = self.sidebar_receipts.get(&receipt_key).cloned();
            if valid {
                match &a.choice {
                    Some(section) => {
                        self.workspace_sections
                            .insert(ws_id.clone(), section.clone());
                    }
                    None => {
                        self.workspace_sections.remove(&ws_id);
                    }
                }
            }
            self.sidebar_receipts.insert(
                receipt_key.clone(),
                Receipt {
                    request_id: a.id,
                    status: if valid { "applied" } else { "section_deleted" }.into(),
                },
            );
            // Assignment and replay receipt must reach disk together BEFORE ack.
            // Otherwise a viewer crash could lose an acknowledged move, or replay
            // an old move over a later Owner edit.
            if self.try_save_session_manifest() {
                self.sidebar_pending.remove(&(host, key));
                self.clamp_cursor();
                self.needs_redraw = true;
            } else {
                self.sidebar_retry_at = Instant::now() + Duration::from_secs(5);
                match previous {
                    Some(s) => {
                        self.workspace_sections.insert(ws_id, s);
                    }
                    None => {
                        self.workspace_sections.remove(&ws_id);
                    }
                }
                match old_receipt {
                    Some(r) => {
                        self.sidebar_receipts.insert(receipt_key, r);
                    }
                    None => {
                        self.sidebar_receipts.remove(&receipt_key);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Home(std::ffi::OsString);
    impl Drop for Home {
        fn drop(&mut self) {
            unsafe {
                std::env::set_var("HOME", &self.0);
            }
        }
    }
    fn with_app(f: impl FnOnce(&mut App)) {
        let _lock = crate::test_support::home_lock();
        let dir = tempfile::tempdir().unwrap();
        let _home = Home(std::env::var_os("HOME").unwrap());
        unsafe {
            std::env::set_var("HOME", dir.path());
        }
        let mut app = App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        });
        app.workspaces = vec![Workspace {
            id: "ws".into(),
            name: "Scouts".into(),
            is_closed: false,
            is_cloud: false,
            repo_url: None,
            worktree_path: None,
            main_repo_path: None,
            worker_vm: None,
            worker_zone: None,
            host_id: HostId::local(),
            color: None,
            pinned: false,
            sessions: vec![],
            tombstones: vec![],
        }];
        app.sections = vec![SidebarSection {
            id: "s1".into(),
            name: "Swarm".into(),
            ..Default::default()
        }];
        app.sessions_restored = true;
        f(&mut app);
    }
    fn assignment(id: &str, choice: Option<&str>) -> Assignment {
        Assignment {
            id: id.into(),
            workspace_id: "ws".into(),
            session_uid: "scout".into(),
            choice: choice.map(str::to_string),
        }
    }
    #[test]
    fn sidebar_remote_applies_none_auto_and_explicit_then_persists_receipt() {
        with_app(|app| {
            for (id, choice) in [("1", Some("s1")), ("2", Some("")), ("3", None)] {
                app.receive_sidebar_assignment(&HostId::local(), assignment(id, choice));
                assert_eq!(app.workspace_sections.get("ws").map(String::as_str), choice);
                assert!(app.sidebar_pending.is_empty());
                let manifest = App::load_manifest();
                assert_eq!(
                    manifest.workspace_sections.get("ws").map(String::as_str),
                    choice
                );
                assert_eq!(
                    manifest.sidebar_receipts[&receipt_key(&HostId::local(), "ws")].request_id,
                    id
                );
                assert_eq!(
                    app.sidebar_publications()[&HostId::local()].workspaces["ws"]
                        .receipt
                        .as_ref()
                        .unwrap()
                        .status,
                    "applied"
                );
            }
        });
    }
    #[test]
    fn sidebar_remote_replay_after_restart_preserves_later_owner_choice() {
        with_app(|app| {
            let request = assignment("1", Some("s1"));
            app.receive_sidebar_assignment(&HostId::local(), request.clone());
            app.workspace_sections.insert("ws".into(), String::new()); // Owner changed to None
            app.save_session_manifest();
            app.sidebar_receipts = App::load_manifest().sidebar_receipts;
            app.receive_sidebar_snapshot(
                &HostId::local(),
                &BTreeMap::from([("ws".into(), request)]),
            );
            assert_eq!(
                app.workspace_sections.get("ws").map(String::as_str),
                Some("")
            );
        });
    }
    #[test]
    fn sidebar_remote_deleted_section_acknowledges_rejection_without_changing_choice() {
        with_app(|app| {
            app.workspace_sections.insert("ws".into(), String::new());
            app.receive_sidebar_assignment(&HostId::local(), assignment("1", Some("deleted")));
            assert_eq!(
                app.workspace_sections.get("ws").map(String::as_str),
                Some("")
            );
            assert_eq!(
                app.sidebar_publications()[&HostId::local()].workspaces["ws"]
                    .receipt
                    .as_ref()
                    .unwrap()
                    .status,
                "section_deleted"
            );
        });
    }
    #[test]
    fn sidebar_remote_host_scope_restore_gate_and_delayed_adoption() {
        with_app(|app| {
            let cloud = HostId::new("sessions");
            app.sessions_restored = false;
            app.receive_sidebar_assignment(&cloud, assignment("1", Some("s1")));
            assert!(app.workspace_sections.is_empty());
            app.sessions_restored = true;
            app.apply_pending_sidebar_assignments();
            assert!(
                app.workspace_sections.is_empty(),
                "same ID on another host must not match"
            );
            app.workspaces[0].host_id = cloud;
            app.apply_pending_sidebar_assignments();
            assert_eq!(app.workspace_sections["ws"], "s1");
        });
    }
    #[test]
    fn sidebar_remote_failed_manifest_save_never_acknowledges_or_loses_request() {
        with_app(|app| {
            let path =
                PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cm/tui-sessions.json");
            std::fs::create_dir_all(&path).unwrap(); // fail atomic rename
            app.receive_sidebar_assignment(&HostId::local(), assignment("1", Some("s1")));
            assert!(app.sidebar_receipts.is_empty());
            assert!(app.workspace_sections.is_empty());
            assert_eq!(app.sidebar_pending.len(), 1);
            std::fs::remove_dir(&path).unwrap();
            app.sidebar_retry_at = Instant::now();
            app.apply_pending_sidebar_assignments();
            assert_eq!(App::load_manifest().workspace_sections["ws"], "s1");
        });
    }
    #[test]
    fn sidebar_remote_socket_round_trip_from_agent_through_viewer_and_ack() {
        with_app(|app| {
            use crate::push_worker::tests::{
                local_pool_for, start_push_test_daemon, stop_push_test_daemon, wait_for,
            };
            use cm_daemon::control::{
                protocol::{Caller, Request},
                wire,
            };
            let (socket, state, stop, handle) = start_push_test_daemon();
            state.lock().unwrap().tui_sessions.insert(
                "scout".into(),
                serde_json::from_value(serde_json::json!({"uid":"scout","workspace_id":"ws"}))
                    .unwrap(),
            );
            app.push_worker = crate::push_worker::PushWorker::spawn(local_pool_for(&socket));
            app.push_sidebar_to_daemon();
            let state_path =
                PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cm/sidebar-sections.json");
            wait_for(|| state_path.exists(), "viewer catalogue publication");
            let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            wire::write_request(
                &mut stream,
                &Request {
                    id: "agent-move".into(),
                    method: "sidebar.assign".into(),
                    caller: Caller::session("scout"),
                    params: serde_json::json!({"section":"Swarm"}),
                },
            )
            .unwrap();
            let response = wire::read_response(&mut stream).unwrap().unwrap();
            assert!(response.ok, "{response:?}");
            assert_eq!(response.result.unwrap()["status"], "queued");
            let pending = cm_daemon::sidebar::snapshot(&state.lock().unwrap()).unwrap();
            app.receive_sidebar_snapshot(&HostId::local(), &pending);
            wait_for(
                || {
                    cm_daemon::sidebar::snapshot(&state.lock().unwrap())
                        .unwrap()
                        .is_empty()
                },
                "durable viewer acknowledgement",
            );
            assert_eq!(App::load_manifest().workspace_sections["ws"], "s1");
            let published: serde_json::Value =
                serde_json::from_slice(&std::fs::read(state_path).unwrap()).unwrap();
            assert_eq!(published["publication"]["workspaces"]["ws"]["choice"], "s1");
            app.push_worker.shutdown();
            stop_push_test_daemon(&socket, stop, handle);
        });
    }
}
