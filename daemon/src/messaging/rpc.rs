use super::{ChatError, Person, Store};
use crate::{
    control::protocol::{Caller, ErrorCode, Request, Response},
    state::DaemonState,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub fn default_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cm")
}
pub fn initialize(state: &Arc<Mutex<DaemonState>>) -> Result<(), ChatError> {
    let (handle, root) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (s.messaging.clone(), s.messaging_root.clone())
    };
    let delivery_gate = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging_delivery
        .clone();
    let _delivery_guard = delivery_gate.lock().unwrap_or_else(|p| p.into_inner());
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(Store::open(&root)?);
    }
    project_names(state, slot.as_ref().unwrap(), false);
    Ok(())
}
pub fn project_names(state: &Arc<Mutex<DaemonState>>, store: &Store, persist: bool) {
    let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
    let mut any_changed = false;
    for (actor, n) in &store.names {
        if !actor.starts_with(&format!("agent:{}:", store.daemon_id)) {
            continue;
        }
        let name_changed = s
            .messaging_names
            .get(&n.session_uid)
            .is_none_or(|old| old.revision != n.revision || old.name != n.name);
        s.messaging_names.insert(n.session_uid.clone(), n.clone());
        let mut changed = false;
        if let Some(session) = s.sessions.get_mut(&n.session_uid) {
            if session.title != n.name {
                session.title = n.name.clone();
                changed = true;
            }
        }
        for ws in s.workspaces.values_mut() {
            for entry in &mut ws.sessions {
                if entry.uid == n.session_uid {
                    entry.label = n.name.clone();
                }
            }
        }
        if let Some(t) = s.tui_sessions.get_mut(&n.session_uid) {
            t.label = Some(n.name.clone());
        }
        if changed || name_changed {
            any_changed = true;
            s.manifest_watcher.broadcast(crate::manifest::ManifestDiff::Updated{uid:n.session_uid.clone(),entry:json!({"uid":n.session_uid,"label":n.name,"name_revision":n.revision,"messaging_daemon_id":store.daemon_id})});
        }
    }
    if any_changed && persist {
        s.persist_sessions_best_effort();
    }
}
pub fn dispatch(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Response {
    match execute(state, req) {
        Ok(v) => {
            if matches!(
                req.method.as_str(),
                "messaging.send"
                    | "messaging.read"
                    | "messaging.follow"
                    | "messaging.monitor"
                    | "messaging.monitors"
            ) {
                super::delivery::signal(state);
            }
            Response::ok(req.id.clone(), v)
        }
        Err(e) => Response::err(
            req.id.clone(),
            match e.code.as_str() {
                "not_found" => ErrorCode::NotFound,
                "unauthorized" => ErrorCode::Unauthorized,
                "invalid_params" | "invalid_target" => ErrorCode::InvalidParams,
                _ => ErrorCode::Conflict,
            },
            format!("{}: {}", e.code, e.message),
        ),
    }
}

/// Label migration is permitted only after legacy workflow bindings have a
/// unique run/role association. Labels are never an identity fallback.
fn repair_legacy_binding(state: &Arc<Mutex<DaemonState>>, uid: &str) -> Result<(), ChatError> {
    let (run_id, role, candidates) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let view = s.lookup_session_any(uid).ok_or_else(|| ChatError {
            code: "not_found".into(),
            message: "Session not found".into(),
        })?;
        let (Some(run_id), Some(role)) = (view.workflow_run_id.clone(), view.workflow_role.clone())
        else {
            return Ok(());
        };
        let ids: std::collections::BTreeSet<_> = s
            .sessions
            .values()
            .filter(|v| {
                v.workflow_run_id.as_deref() == Some(&run_id)
                    && v.workflow_role.as_deref() == Some(&role)
            })
            .map(|v| v.uid.clone())
            .chain(
                s.tui_sessions
                    .values()
                    .filter(|v| {
                        v.workflow_run_id.as_deref() == Some(&run_id)
                            && v.workflow_role.as_deref() == Some(&role)
                    })
                    .map(|v| v.uid.clone()),
            )
            .collect();
        (run_id, role, ids)
    };
    let error = || ChatError {
        code: "workflow_binding_unresolved".into(),
        message: "Repair the session's missing or ambiguous workflow role binding before renaming"
            .into(),
    };
    let run = crate::workflow::run::load_one(&run_id).ok_or_else(error)?;
    let binding = run.role_sessions.get(&role).ok_or_else(error)?;
    if binding.daemon_session_uid.is_some() {
        return Ok(());
    }
    if candidates.len() != 1 || !candidates.contains(uid) {
        return Err(error());
    }
    let updated = crate::workflow::run::modify(&run_id, |r| {
        if let Some(b) = r.role_sessions.get_mut(&role) {
            if b.daemon_session_uid.is_none() {
                b.daemon_session_uid = Some(uid.into());
            }
        }
    })
    .map_err(|e| ChatError {
        code: "workflow_binding_unresolved".into(),
        message: e.to_string(),
    })?;
    let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(run) = s.workflow_runs.get_mut(&run_id) {
        if let Some(b) = run.role_sessions.get_mut(&role) {
            if b.daemon_session_uid.is_none() {
                b.daemon_session_uid = updated
                    .role_sessions
                    .get(&role)
                    .and_then(|b| b.daemon_session_uid.clone());
            }
        }
    }
    Ok(())
}

fn execute(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Result<Value, ChatError> {
    if req.method == "messaging.sync" {
        return execute_sync_admin(state, req);
    }
    // Fence stale task subscription owners before they can acknowledge hits.
    super::tasks::refresh(state)?;
    let mut result = execute_with_freshness(state, req, false)?;
    if matches!(req.caller, Caller::Operator(_)) && req.params["claim_bell"] == true {
        let (handle, sync) = {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            (s.messaging.clone(), s.messaging_sync.clone())
        };
        let remote_bell = {
            let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
            slot.as_ref().is_some_and(|s| {
                !s.is_coordinator()
                    && s.cached_owner_snapshot()
                        .is_some_and(|v| v["preferences"]["bell"] == true)
            })
        };
        if remote_bell {
            if let Some(sync) = sync {
                match sync.request("owner", "sync.owner_attention", &json!({})) {
                    Ok(attention) => result["attention"] = attention,
                    Err(_) => result["attention"] = json!({"bell":false,"pending":true}),
                }
            }
        }
    }
    Ok(result)
}
fn execute_sync_admin(state: &Arc<Mutex<DaemonState>>, req: &Request) -> Result<Value, ChatError> {
    if !matches!(req.caller, Caller::Operator(_)) {
        return Err(ChatError {
            code: "unauthorized".into(),
            message: "Messaging host administration is operator-only".into(),
        });
    }
    crate::control::operator::validate_operator(&req.caller).map_err(|e| ChatError {
        code: "unauthorized".into(),
        message: e.into(),
    })?;
    initialize(state)?;
    let restart = matches!(
        req.params["action"].as_str(),
        Some("enable_hub" | "join_space" | "install_seed" | "activate_replica")
    );
    if restart {
        let runtime = state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .messaging_sync
            .take();
        if let Some(runtime) = runtime {
            runtime.stop();
        }
    }
    let (handle, people) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging.clone(),
            s.sessions
                .values()
                .map(|s| (s.uid.clone(), s.title.clone(), s.task_id.clone()))
                .chain(
                    s.tui_sessions
                        .values()
                        .filter(|t| !s.sessions.contains_key(&t.uid))
                        .map(|t| {
                            (
                                t.uid.clone(),
                                t.label.clone().unwrap_or_else(|| t.uid.clone()),
                                t.task_id.clone(),
                            )
                        }),
                )
                .collect::<Vec<_>>(),
        )
    };
    let result = {
        let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        let store = slot.as_mut().unwrap();
        let people = people
            .into_iter()
            .map(|(uid, name, task)| Person {
                id: store.participant_id(&uid),
                name,
                session_uid: uid,
                task,
                present: true,
                kind: "agent".into(),
            })
            .collect::<Vec<_>>();
        store.sync_admin(&req.params, &people)
    };
    if restart {
        super::sync::start(state);
    }
    if matches!(
        req.params["action"].as_str(),
        Some("revoke" | "rotate_peer")
    ) {
        let sync = state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .messaging_sync
            .clone();
        if let (Some(sync), Some(host)) = (sync, req.params["host_id"].as_str()) {
            sync.disconnect_peer(host);
        }
    }
    result
}
fn execute_with_freshness(
    state: &Arc<Mutex<DaemonState>>,
    req: &Request,
    caught_up: bool,
) -> Result<Value, ChatError> {
    if matches!(req.caller, Caller::Operator(_)) {
        crate::control::operator::validate_operator(&req.caller).map_err(|e| ChatError {
            code: "unauthorized".into(),
            message: e.into(),
        })?;
    }
    let (handle, root, uid, kind, live, draining) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let (uid, kind) = match &req.caller {
            Caller::Operator(_) => (String::new(), "owner"),
            Caller::Session(c) => {
                if !s.sessions.contains_key(&c.session_uid)
                    && !s.tui_sessions.contains_key(&c.session_uid)
                {
                    return Err(ChatError {
                        code: "unauthorized".into(),
                        message: "Messaging caller is not a registered session".into(),
                    });
                }
                (c.session_uid.clone(), "agent")
            }
        };
        let live = s
            .sessions
            .values()
            .map(|s| (s.uid.clone(), s.title.clone(), s.task_id.clone()))
            .chain(
                s.tui_sessions
                    .values()
                    .filter(|t| !s.sessions.contains_key(&t.uid))
                    .map(|t| {
                        (
                            t.uid.clone(),
                            t.label.clone().unwrap_or_else(|| t.uid.clone()),
                            t.task_id.clone(),
                        )
                    }),
            )
            .collect::<Vec<_>>();
        (
            s.messaging.clone(),
            s.messaging_root.clone(),
            uid,
            kind,
            live,
            s.draining,
        )
    };
    if draining {
        return Err(ChatError {
            code: "draining".into(),
            message: "Daemon is restarting; retry the same request".into(),
        });
    }
    // Full-message reads advance the wake batch boundary. Serialize their
    // snapshot with publication; native adapter I/O runs outside this gate.
    let coordinates_delivery = kind != "owner"
        && (req.method == "messaging.read"
            || req.method == "messaging.follow"
                && matches!(req.params["action"].as_str(), Some("set" | "remove"))
            || req.method == "messaging.monitors"
                && matches!(
                    req.params["action"].as_str(),
                    Some("ack" | "cancel" | "cancel_all" | "dismiss")
                )
            || matches!(
                req.method.as_str(),
                "messaging.read" | "messaging.dms" | "messaging.send"
            ) && req.params["ack_receipt"].is_object());
    let delivery_gate = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging_delivery
        .clone();
    let _delivery_guard =
        coordinates_delivery.then(|| delivery_gate.lock().unwrap_or_else(|p| p.into_inner()));
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(Store::open(&root)?);
    }
    let store = slot.as_mut().unwrap();
    let actor = if kind == "owner" {
        "owner".into()
    } else {
        store.participant_id(&uid)
    };
    let mut people = live
        .into_iter()
        .map(|(uid, name, task)| Person {
            id: store.participant_id(&uid),
            name,
            session_uid: uid,
            task,
            present: true,
            kind: "agent".into(),
        })
        .collect::<Vec<_>>();
    people.push(Person {
        id: "owner".into(),
        name: "Owner".into(),
        session_uid: String::new(),
        task: None,
        present: true,
        kind: "owner".into(),
    });
    people.extend(store.remote_people());
    let p = &req.params;
    if !p.is_object() {
        return Err(ChatError {
            code: "invalid_params".into(),
            message: "Expected object parameters".into(),
        });
    }
    if p.get("from").is_some() || p.get("_authenticated_actor").is_some() {
        return Err(ChatError {
            code: "invalid_params".into(),
            message: "Sender is authenticated; do not supply from".into(),
        });
    }
    if kind == "owner"
        && !store.is_coordinator()
        && store
            .host_info(&store.daemon_id)
            .is_none_or(|h| h["owner_access"] != true || h["active"] != true)
    {
        return Err(ChatError {
            code: "unauthorized".into(),
            message: "This messaging host is not enrolled to serve Owner".into(),
        });
    }
    store.enroll_participants(&people)?;
    if store.sync_enabled() && store.is_coordinator() {
        let local = people
            .iter()
            .filter(|p| p.id.starts_with(&format!("agent:{}:", store.daemon_id)))
            .cloned()
            .collect::<Vec<_>>();
        store.register_host(&store.daemon_id.clone(), true, true, &local)?;
    }
    if req.method == "messaging.send"
        && p["origin_daemon_id"]
            .as_str()
            .is_some_and(|o| o != store.daemon_id)
    {
        match store.resolve_pinned_retry(&actor, p, &people) {
            Ok(mut value) => {
                store.decorate_sync_response(p, &mut value);
                return Ok(value);
            }
            Err(e) if e.code == "retry_origin_unavailable" && !store.is_coordinator() => {}
            Err(e) => return Err(e),
        }
    }
    let sync = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging_sync
        .clone();
    if !store.is_coordinator() {
        let forward = store.needs_coordinator(&actor, &req.method, p, &people)?;
        let fresh = !caught_up && p["freshness"] == "hub" && p["cursor"].is_null();
        if let Some(sync) = &sync {
            if matches!(req.method.as_str(), "messaging.open" | "messaging.read") {
                if let Some(interest) = store.query_interest(p) {
                    sync.view(&interest);
                }
            }
        }
        if forward || fresh {
            let sync = sync.ok_or_else(|| ChatError {
                code: "coordinator_unavailable".into(),
                message: "Messaging coordinator is not connected; keep the draft and request ID"
                    .into(),
            })?;
            if kind != "owner"
                && (req.method == "session.set_name" || !store.names.contains_key(&actor))
            {
                repair_legacy_binding(state, &uid)?;
            }
            store.acknowledge(&actor, &p["ack_receipt"])?;
            let interest = store.query_interest(p);
            let mut forwarded = p.clone();
            // Local receipts are acknowledged here, never submitted as a hub cursor.
            if let Some(obj) = forwarded.as_object_mut() {
                obj.remove("ack_receipt");
            }
            if actor == "owner" && req.method == "messaging.monitor" && p["after"].is_object() {
                forwarded["after"] = store.owner_execution_position(&p["after"])?;
            }
            // A coordinated first send still gives this host a usable monitor
            // fence. Capture it before the round trip so even a fast response
            // delivered ahead of bulk history cannot be skipped.
            let submission_position = store.position_token(store.publication_position());
            drop(slot);
            drop(_delivery_guard);
            if forward {
                let mut result = sync.request(&actor, &req.method, &forwarded)?;
                let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
                let store = slot.as_mut().unwrap();
                if req.method == "messaging.send" {
                    result["coordinator_position"] = result["position"].clone();
                    result["position"] = submission_position;
                }
                if let Some(snapshot) = result.get("owner_snapshot").cloned() {
                    store.apply_owner_snapshot(&snapshot)?;
                }
                project_names(state, store, true);
                result = store.context_response(&actor, p, result, false);
                store.decorate_sync_response(p, &mut result);
                return Ok(result);
            }
            sync.request(
                &actor,
                "sync.barrier",
                &json!({"interests":interest.into_iter().collect::<Vec<_>>()}),
            )?;
            return execute_with_freshness(state, req, true);
        }
    }
    let result = match req.method.as_str() {
        "messaging.send" => {
            if kind != "owner" && !store.names.contains_key(&actor) {
                repair_legacy_binding(state, &uid)?;
            }
            store.acknowledge(&actor, &p["ack_receipt"])?;
            store.send(&actor, &uid, kind, p, &people)
        }
        "messaging.read" => {
            // A proactive read may beat the maintenance scan. Include watch
            // hits in this snapshot so they cannot create a later duplicate wake.
            store.advance_monitors_at(chrono::Utc::now())?;
            store.read(&actor, p, &people)
        }
        "messaging.dms" => store.dms_page(&actor, p),
        "messaging.norms" => {
            if matches!(p["action"].as_str(), Some("publish" | "revert")) {
                let name = store
                    .names
                    .get(&actor)
                    .map(|n| n.name.as_str())
                    .or_else(|| {
                        people
                            .iter()
                            .find(|person| person.id == actor)
                            .map(|person| person.name.as_str())
                    })
                    .unwrap_or("Participant")
                    .to_owned();
                store.change_norms(&actor, kind, &name, p)
            } else {
                store.norms_document(&actor, p)
            }
        }
        "messaging.monitor" => store.register_monitor(&actor, p, &people),
        "messaging.monitors" => store.monitors(&actor, p),
        "messaging.follow" => store.follow(&actor, p, &people),
        "messaging.people" => {
            let mut value = store.people(&people);
            if let Some(items) = value.as_array_mut() {
                items.retain(|v| {
                    (p["include_exited"] == true || v["present"] == true)
                        && p["query"].as_str().is_none_or(|q| {
                            v.to_string().to_lowercase().contains(&q.to_lowercase())
                        })
                });
            }
            store.directory_page(&actor, p, value.as_array().cloned().unwrap_or_default())
        }
        "messaging.channels" => store.channel_action(&actor, p, &people),
        "messaging.pins" => store.pins(&actor, p, &people),
        "messaging.open" => {
            let mut query = p.clone();
            if ["channel", "dm", "conversation"]
                .iter()
                .all(|k| query.get(*k).is_none_or(Value::is_null))
            {
                query["channel"] = json!("general");
            }
            query["limit"] = json!(10);
            query["newest_first"] = json!(true);
            let recent = store.read(&actor, &query, &people)?;
            Ok(
                json!({"actor_id":actor,"daemon_id":store.daemon_id,"space_id":store.space_id,"name":store.names.get(&actor),"self":people.iter().find(|p|p.id==actor),"target":recent["target"],"norms":store.norms,"recent":recent,"dms":store.dms(&actor,true)?,"task_subscriptions":store.task_orientation(&actor),"capabilities":["open","read","send","dms","people","channels","norms","monitor","monitors","follow","pins"],"features":["group_dms","channel_admins","pins","channel_membership","channel_mentions","channel_norms"],"dm_max_members":32,"message_max_chars":3000}),
            )
        }
        "session.set_name" => {
            let target = p["uid"].as_str().unwrap_or(&uid);
            if kind != "owner" && target != uid {
                return Err(ChatError {
                    code: "unauthorized".into(),
                    message: "An agent can rename only itself".into(),
                });
            }
            repair_legacy_binding(state, target)?;
            let target_actor = store.participant_id(target);
            let mut params = p.clone();
            params["_authenticated_actor"] = json!(actor);
            store.rename(&target_actor, target, &params)
        }
        _ => Err(ChatError {
            code: "unsupported_feature".into(),
            message: "Messaging method not implemented".into(),
        }),
    };
    project_names(state, store, true);
    let mut retraction_note = None;
    if kind != "owner" && req.method == "messaging.read" {
        if let Ok(value) = &result {
            if let Err(error) = super::delivery::read_boundary(store, &uid, value) {
                eprintln!("cm messaging: wake read boundary: {error}");
                retraction_note = Some("Messages supplied, but the notification boundary could not be saved; retry the read.".into());
            }
        }
    }
    if coordinates_delivery {
        if let Err(error) = super::delivery::reconcile(&root, store) {
            eprintln!("cm messaging: pending wake reconciliation: {error}");
            retraction_note = Some(format!(
                "Change saved, but a queued hook could not yet be retracted: {error}"
            ));
        }
    }
    result.map(|value| {
        if !req.method.starts_with("messaging.") {
            return value;
        }
        let mut value = store.context_response(&actor, p, value, req.method == "messaging.open");
        value["monitor_status"] = if kind == "owner" && !store.is_coordinator() {
            store
                .cached_owner_snapshot()
                .map(|s| s["monitor_status"].clone())
                .unwrap_or(json!({"state":"uncached","execution_host":store.coordinator_id()}))
        } else {
            store.monitor_status(&actor)
        };
        store.decorate_sync_response(p, &mut value);
        if let Some(note) = retraction_note {
            value["delivery_note"] = json!(note);
        }
        if kind == "owner" {
            match store.attention(
                &actor,
                p["claim_bell"] == true && (store.is_coordinator() || !store.sync_enabled()),
            ) {
                Ok(attention) => value["attention"] = attention,
                Err(error) => value["attention"] = json!({"error":error.to_string()}),
            }
        }
        value
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(root: &std::path::Path) -> Arc<Mutex<DaemonState>> {
        let mut state = DaemonState::default();
        state.messaging_root = root.into();
        for uid in ["a", "b", "outsider"] {
            state.tui_sessions.insert(
                uid.into(),
                serde_json::from_value(
                    json!({"uid":uid,"label":"default","task_id":format!("task-{uid}")}),
                )
                .unwrap(),
            );
        }
        Arc::new(Mutex::new(state))
    }
    fn call(
        state: &Arc<Mutex<DaemonState>>,
        uid: &str,
        method: &str,
        p: Value,
    ) -> Result<Value, ChatError> {
        execute(
            state,
            &Request {
                id: "rpc-test".into(),
                caller: Caller::session(uid),
                method: format!("messaging.{method}"),
                params: p,
            },
        )
    }
    #[test]
    fn messaging_full_read_covers_unscanned_watch_but_preview_and_invalid_read_do_not() {
        let root = tempfile::tempdir().unwrap();
        let state = setup(root.path());
        call(
            &state,
            "b",
            "monitor",
            json!({"scope":{"channel":"general"},"mode":"continuous","request_id":"watch"}),
        )
        .unwrap();
        call(
            &state,
            "a",
            "send",
            json!({"channel":"general","name":"Publisher","body":"Ready","request_id":"one"}),
        )
        .unwrap();
        call(&state, "b", "open", json!({"channel":"general"})).unwrap();
        assert!(call(
            &state,
            "b",
            "read",
            json!({"channel":"general","cursor":{"invalid":true}})
        )
        .is_err());
        use sha2::Digest;
        let uid_hash = format!("{:x}", sha2::Sha256::digest(b"b"));
        let ledger = root
            .path()
            .join("messages/main/_delivery")
            .join(format!("{uid_hash}.json"));
        // Neither a preview nor a failed read releases a wake.
        if ledger.exists() {
            let q: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
            assert!(q["batches"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b["checked"].as_array().unwrap().is_empty()));
        }
        let page = call(
            &state,
            "b",
            "read",
            json!({"inbox":true,"unread_only":true}),
        )
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["items"][0]["read"], false);
        let q: Value = serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
        assert_eq!(q["batches"][0]["released"], true);
        assert_eq!(q["batches"][0]["checked"].as_array().unwrap().len(), 1);
        // The watch result and unread message survive the notification boundary.
        assert_eq!(page["monitor_status"]["unacknowledged"], 1);
        assert_eq!(
            call(
                &state,
                "b",
                "read",
                json!({"inbox":true,"unread_only":true})
            )
            .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn messaging_channel_norms_authentication_and_scoped_context_through_rpc() {
        let root = tempfile::tempdir().unwrap();
        let state = setup(root.path());
        call(&state,"a","channels",json!({"action":"create","path":"norms-rpc","request_id":"new"})).unwrap();
        let global = call(&state,"a","norms",json!({})).unwrap()["revision"].clone();
        let initial = call(&state,"b","norms",json!({"channel":"norms-rpc"})).unwrap();
        let scope = initial["scope"].as_str().unwrap();
        let p = json!({"action":"publish","channel":"norms-rpc","expected_revision":initial["revision"],"text":"Local agreement.","summary":"Agreement","request_id":"rules","role":"admin","created_by":"owner"});
        assert_eq!(call(&state,"b","norms",p.clone()).unwrap_err().code,"unauthorized");
        let changed = call(&state,"a","norms",p).unwrap();
        let opened = call(&state,"b","open",json!({"channel":"norms-rpc"})).unwrap();
        assert_eq!(opened["channel_norms"]["text"],"Local agreement.");
        assert_eq!(opened["context_status"]["current"][scope],changed["revision"]);
        assert_eq!(opened["context_status"]["current"]["global"],global);
        assert_eq!(opened["target"]["joined"],false); // Public context does not join.
        call(&state,"b","channels",json!({"action":"join","path":"norms-rpc","request_id":"join"})).unwrap();
        let sent = call(&state,"b","send",json!({"channel":"norms-rpc","body":"Acknowledged","name":"Reader","request_id":"send","norms_seen":{"global":global,(scope):changed["revision"]}})).unwrap();
        assert_eq!(sent["context_status"]["changed"],false);
        assert!(sent["event_id"].is_string());
    }
    #[test]
    fn messaging_channel_admin_uses_authenticated_caller_not_supplied_roles() {
        let root = tempfile::tempdir().unwrap();
        let state = setup(root.path());
        let channel = call(
            &state,
            "a",
            "channels",
            json!({"action":"create","path":"roles","request_id":"create"}),
        )
        .unwrap()["channel"]
            .clone();
        let p = json!({"action":"update","path":"roles","description":"Hijack","admins":[],"created_by":"owner","role":"admin","expected_revision":channel["revision"],"request_id":"bad"});
        assert_eq!(
            call(&state, "b", "channels", p).unwrap_err().code,
            "unauthorized"
        );
        let opened = call(&state, "b", "open", json!({"channel":"roles"})).unwrap();
        assert_eq!(opened["target"]["can_edit"], false);
        assert_eq!(opened["target"]["created_by"], channel["created_by"]);
    }
    #[test]
    fn messaging_reads_and_cancellation_serialize_with_wake_publication_but_sends_do_not() {
        let tmp = tempfile::tempdir().unwrap();
        let state = setup(tmp.path());
        let monitor = call(
            &state,
            "b",
            "monitor",
            json!({"scope":{"channel":"general"},"request_id":"watch"}),
        )
        .unwrap();
        let gate = state.lock().unwrap().messaging_delivery.clone();
        let guard = gate.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let clone = state.clone();
        let reader = std::thread::spawn(move || {
            tx.send(call(&clone, "a", "read", json!({"channel":"general"})))
                .unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "Read crossed an in-progress wake publication boundary"
        );
        let read_rx = rx;
        let (tx, rx) = std::sync::mpsc::channel();
        let clone = state.clone();
        let sender = std::thread::spawn(move || {
            tx.send(call(&clone,"a","send",json!({"channel":"general","name":"Boundary Builder","body":"Ready","request_id":"send"}))).unwrap();
        });
        assert!(rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("A send waited for PTY delivery")
            .is_ok());
        sender.join().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let clone = state.clone();
        let canceller = std::thread::spawn(move || {
            tx.send(call(
                &clone,
                "b",
                "monitors",
                json!({"action":"cancel","monitor_id":monitor["id"],"request_id":"cancel"}),
            ))
            .unwrap();
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "Cancellation crossed an in-progress delivery boundary"
        );
        drop(guard);
        assert!(read_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .is_ok());
        reader.join().unwrap();
        assert!(rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .is_ok());
        canceller.join().unwrap();
    }
    #[test]
    fn messaging_b_registration_race_catches_every_arrival_after_read_position() {
        let tmp = tempfile::tempdir().unwrap();
        let state = setup(tmp.path());
        let after =
            call(&state, "b", "read", json!({"channel":"general"})).unwrap()["position"].clone();
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let mut threads = vec![];
        for n in 0..8 {
            let state = state.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move ||{barrier.wait();call(&state,"a","send",json!({"channel":"general","name":"Race Builder","body":format!("message {n}"),"request_id":format!("message-{n}")})).unwrap();}));
        }
        barrier.wait();
        let monitor=call(&state,"b","monitor",json!({"scope":{"channel":"general"},"mode":"continuous","after":after,"request_id":"watch"})).unwrap();
        for thread in threads {
            thread.join().unwrap();
        }
        let result = call(
            &state,
            "b",
            "monitors",
            json!({"action":"get","monitor_id":monitor["id"]}),
        )
        .unwrap();
        assert_eq!(result["monitor"]["hits"], 8);
        assert_eq!(result["items"].as_array().unwrap().len(), 8);
        assert!(call(
            &state,
            "outsider",
            "monitors",
            json!({"action":"get","monitor_id":monitor["id"]})
        )
        .is_err());
        assert!(call(&state,"b","send",json!({"channel":"general","name":"Race Reader","body":"Stale context still sends","request_id":"stale","norms_seen":{"global":"missing"}})).unwrap()["event_id"].is_string());
    }
    #[test]
    fn messaging_two_sessions_and_owner_vertical_slice_auth_and_name_convergence() {
        let tmp = tempfile::tempdir().unwrap();
        let state = setup(tmp.path());
        let people = call(&state, "a", "people", json!({})).unwrap();
        let id = |uid: &str| {
            people["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|v| v["session_uid"] == uid)
                .unwrap()["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        assert_eq!(
            call(&state, "a", "open", json!({})).unwrap()["name"],
            Value::Null
        );
        call(
            &state,
            "a",
            "channels",
            json!({"action":"create","path":"work/parser","request_id":"channel"}),
        )
        .unwrap();
        let a=call(&state,"a","send",json!({"channel":"work/parser","name":"Parser Scout","body":"Ready.","tags":["needs-owner"],"request_id":"one"})).unwrap();
        call(
            &state,
            "b",
            "channels",
            json!({"action":"join","path":"work/parser","request_id":"join"}),
        )
        .unwrap();
        call(&state,"b","send",json!({"channel":"work/parser","name":"parser scout","body":"Checking.","request_id":"two"})).unwrap();
        let dm = call(
            &state,
            "a",
            "send",
            json!({"dm":id("b"),"body":"Quick check?","request_id":"dm"}),
        )
        .unwrap();
        assert_eq!(
            call(&state, "b", "dms", json!({"unread_only":true})).unwrap()["items"][0]["unread"],
            1
        );
        assert!(call(
            &state,
            "outsider",
            "read",
            json!({"conversation":dm["event"]["conversation_id"]})
        )
        .is_err());
        assert!(call(&state, "outsider", "dms", json!({})).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(
            call(
                &state,
                "b",
                "read",
                json!({"channel":"*","tags":["needs-owner"],"time":{"since":"10m"}})
            )
            .unwrap()["items"][0]["id"],
            a["event_id"]
        );
        let handle = state.lock().unwrap().messaging.clone();
        {
            let mut slot = handle.lock().unwrap();
            let store = slot.as_mut().unwrap();
            store
                .channel_action(
                    "owner",
                    &json!({"action":"join","path":"work/parser","request_id":"owner-join"}),
                    &[],
                )
                .unwrap();
            store
                .send(
                    "owner",
                    "",
                    "owner",
                    &json!({"channel":"work/parser","body":"Thanks.","request_id":"owner"}),
                    &[],
                )
                .unwrap();
            assert_eq!(
                store.read("owner", &json!({"inbox":true}), &[]).unwrap()["items"],
                json!([])
            );
            assert!(store
                .notifications()
                .iter()
                .all(|(uid, _, _)| uid != "owner"));
        }
        let s = state.lock().unwrap();
        assert_eq!(s.tui_sessions["a"].label.as_deref(), Some("Parser-Scout"));
        assert_ne!(s.messaging_names["a"].name, s.messaging_names["b"].name);
        assert_eq!(s.tui_sessions["a"].task_id.as_deref(), Some("task-a"));
        drop(s);
        assert_eq!(
            call(&state, "fake", "people", json!({})).unwrap_err().code,
            "unauthorized"
        );
        assert_eq!(
            call(&state, "a", "send", json!({"from":"owner"}))
                .unwrap_err()
                .code,
            "invalid_params"
        );
        assert_eq!(
            call(&state, "a", "read", Value::Null).unwrap_err().code,
            "invalid_params"
        );
    }
    #[test]
    fn messaging_concurrent_first_sends_renames_and_retries_are_serialized() {
        let tmp = tempfile::tempdir().unwrap();
        let state = setup(tmp.path());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut threads = vec![];
        for n in 0..8 {
            let s = state.clone();
            let b = barrier.clone();
            threads.push(std::thread::spawn(move || {
                b.wait();
                call(
                    &s,
                    if n % 2 == 0 { "a" } else { "b" },
                    "send",
                    json!({"channel":"general","name":"Scout","body":"Hi","request_id":"same"}),
                )
                .unwrap()
            }));
        }
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        for n in 2..8 {
            assert_eq!(results[n]["event_id"], results[n % 2]["event_id"]);
        }
        let req = |uid: &str, rev: u64, name: &str| Request {
            id: "rename".into(),
            caller: Caller::session("a"),
            method: "session.set_name".into(),
            params: json!({"uid":uid,"name":name,"expected_name_revision":rev,"request_id":format!("rename-{rev}")}),
        };
        assert_eq!(
            execute(&state, &req("b", 1, "Other")).unwrap_err().code,
            "unauthorized"
        );
        execute(&state, &req("a", 1, "New Scout")).unwrap();
        assert_eq!(
            execute(&state, &req("a", 1, "Stale form"))
                .unwrap_err()
                .code,
            "idempotency_conflict"
        );
        assert_eq!(state.lock().unwrap().messaging_names["a"].name, "New-Scout");
    }
    #[test]
    fn messaging_boot_name_recovery_does_not_clobber_headless_restore_records() {
        let tmp = tempfile::tempdir().unwrap();
        let state = setup(tmp.path());
        call(
            &state,
            "a",
            "send",
            json!({"channel":"general","name":"Scout","body":"Hi","request_id":"hello"}),
        )
        .unwrap();
        drop(state);
        let state = setup(tmp.path());
        let manifest = tmp.path().join("daemon-sessions.json");
        let original = b"headless registry must survive initialization";
        std::fs::write(&manifest, original).unwrap();
        state.lock().unwrap().daemon_sessions_path = Some(manifest.clone());
        initialize(&state).unwrap();
        assert_eq!(std::fs::read(&manifest).unwrap(), original);
        assert_eq!(state.lock().unwrap().messaging_names["a"].name, "Scout");
        let stale = tmp.path().join("stale-tui.json");
        std::fs::write(&stale,serde_json::to_vec(&json!({"workspaces":{"w":{"id":"w","sessions":[{"uid":"a","label":"old","session_type":"claude-code","transcript_id":null,"workflow_run_id":"run","workflow_role":"worker","task_id":"task-a"}]}},"bindings":{}})).unwrap()).unwrap();
        state
            .lock()
            .unwrap()
            .load_manifest_from_disk(&stale)
            .unwrap();
        let s = state.lock().unwrap();
        let entry = &s.workspaces["w"].sessions[0];
        assert_eq!(entry.label, "Scout");
        assert_eq!(entry.workflow_role.as_deref(), Some("worker"));
        assert_eq!(entry.task_id.as_deref(), Some("task-a"));
    }
}
