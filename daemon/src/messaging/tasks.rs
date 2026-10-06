//! Continuous-task channel ownership comes from scheduler records, never a
//! caller-supplied task slug. Network membership work runs outside delivery.
//!
//! One channel per continuous task (DESIGN_TASK_CHANNELS.md): the daemon
//! creates `ct/<slug>`, binds the task to it, keeps the current orchestrator
//! subscribed under the `<task>-orchestrator` name, and joins every worker
//! the daemon can attribute to the task. All of it is reconciled by
//! [`refresh`] every couple of seconds and on demand by [`ensure_for_task`],
//! so a replacement session, a restarted brain, or a newly spawned worker
//! converges without any agent-side registration.
use super::{ChatError, Store, TaskBinding};
use crate::{continuous::task, state::DaemonState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskChannel {
    pub subscription_id: String,
    pub space_id: String,
    pub channel_id: String,
    pub revision: u64,
    pub session_uid: Option<String>,
    /// Channel path for display (`ct/<slug>`); filled in lazily for bindings
    /// written before the field existed.
    #[serde(default)]
    pub path: Option<String>,
}
impl TaskChannel {
    pub fn handover(&mut self, uid: &str) {
        if self.session_uid.as_deref() != Some(uid) {
            self.revision = self.revision.saturating_add(1);
            self.session_uid = Some(uid.into());
        }
    }
}

/// Channel-path segment for a continuous task id: lowercase, `_` → `-`,
/// runs of dashes collapsed, edges trimmed. Task ids are `[A-Za-z0-9_-]`
/// so the result is always a legal channel segment.
pub fn channel_slug(task_id: &str) -> String {
    let mut out = String::with_capacity(task_id.len());
    for c in task_id.chars() {
        let c = if c == '_' { '-' } else { c.to_ascii_lowercase() };
        if c == '-' && out.ends_with('-') {
            continue;
        }
        if c.is_ascii_alphanumeric() || c == '-' {
            out.push(c);
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "task".into()
    } else {
        trimmed
    }
}
pub fn channel_path(task_id: &str) -> String {
    format!("ct/{}", channel_slug(task_id))
}
pub fn orchestrator_name(task_id: &str) -> String {
    let slug = channel_slug(task_id);
    // Names are capped at 40 graphemes; keep the suffix intact.
    let max = 40 - "-orchestrator".len();
    let base: String = slug.chars().take(max).collect();
    format!("{}-orchestrator", base.trim_end_matches('-'))
}

pub fn configuration(
    state: &Arc<Mutex<DaemonState>>,
    value: &Value,
) -> Result<Option<TaskChannel>, ChatError> {
    if value.is_null() {
        return Ok(None);
    }
    let id = value["channel_id"].as_str().ok_or_else(|| ChatError {
        code: "invalid_params".into(),
        message: "messaging needs channel_id".into(),
    })?;
    super::rpc::initialize(state)?;
    let store = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging
        .clone();
    let slot = store.lock().unwrap_or_else(|p| p.into_inner());
    let store = slot.as_ref().unwrap();
    let Some(path) = store.channel_path_of(id) else {
        return Err(ChatError {
            code: "not_found".into(),
            message: "Configure a known public channel for the task".into(),
        });
    };
    Ok(Some(TaskChannel {
        subscription_id: uuid::Uuid::new_v4().to_string(),
        space_id: store.space_id.clone(),
        channel_id: id.into(),
        revision: 0,
        session_uid: None,
        path: Some(path.to_string()),
    }))
}

/// Snapshot of the session graph the membership rule needs, captured under
/// the state lock so the store lock is never taken inside it.
#[derive(Clone, Debug, Default)]
pub struct SessionGraph {
    /// uid → (task_id, managed_by_uid, continuous_task_id)
    pub live: BTreeMap<String, (Option<String>, Option<String>, Option<String>)>,
    /// uid → (managed_by_uid, continuous_task_id) for recent tombstones
    pub exited: BTreeMap<String, (Option<String>, Option<String>)>,
    pub task_tree: HashMap<String, Option<String>>,
}
impl SessionGraph {
    pub fn capture(s: &DaemonState) -> Self {
        SessionGraph {
            live: s
                .sessions
                .values()
                .map(|p| {
                    (
                        p.uid.clone(),
                        (
                            p.task_id.clone(),
                            p.managed_by_uid.clone(),
                            p.continuous_task_id.clone(),
                        ),
                    )
                })
                .collect(),
            exited: s
                .recently_exited
                .iter()
                .map(|t| {
                    (
                        t.session_uid.clone(),
                        (t.managed_by_uid.clone(), t.continuous_task_id.clone()),
                    )
                })
                .collect(),
            task_tree: s.task_tree.clone(),
        }
    }
    /// Every LIVE session attributable to `task`: tagged with it, transitively
    /// managed by any session that is or was its orchestrator, or bound to
    /// its planning task or a planning descendant. Mirrors the closure the
    /// drain code uses, plus the planning-tree rule the TUI's continuous
    /// column applies (workers spawned by a dead orchestrator instance).
    pub fn members(&self, task: &task::ContinuousTask) -> BTreeSet<String> {
        let mut roots: BTreeSet<String> = BTreeSet::new();
        roots.extend(task.current_session_uid.iter().cloned());
        roots.extend(
            task.messaging
                .as_ref()
                .and_then(|m| m.session_uid.clone()),
        );
        roots.extend(task.last_run.as_ref().and_then(|r| r.session_uid.clone()));
        roots.extend(task.investigator_uid.iter().cloned());
        roots.extend(
            task.retirement
                .as_ref()
                .map(|r| r.session_uids.clone())
                .unwrap_or_default(),
        );
        for (uid, (_, _, ct)) in &self.live {
            if ct.as_deref() == Some(&task.task_id) {
                roots.insert(uid.clone());
            }
        }
        for (uid, (_, ct)) in &self.exited {
            if ct.as_deref() == Some(&task.task_id) {
                roots.insert(uid.clone());
            }
        }
        loop {
            let before = roots.len();
            for (uid, (_, managed_by, _)) in &self.live {
                if managed_by.as_ref().is_some_and(|m| roots.contains(m)) {
                    roots.insert(uid.clone());
                }
            }
            for (uid, (managed_by, _)) in &self.exited {
                if managed_by.as_ref().is_some_and(|m| roots.contains(m)) {
                    roots.insert(uid.clone());
                }
            }
            if roots.len() == before {
                break;
            }
        }
        let mut out: BTreeSet<String> = roots
            .iter()
            .filter(|uid| self.live.contains_key(*uid))
            .cloned()
            .collect();
        if let Some(parent) = &task.planning_task_id {
            for (uid, (task_id, _, _)) in &self.live {
                if task_id.as_ref().is_some_and(|t| {
                    crate::control::auth::task_is_self_or_descendant_of(&self.task_tree, t, parent)
                }) {
                    out.insert(uid.clone());
                }
            }
        }
        out
    }
}

/// What one reconciliation pass did for a task; surfaced by
/// `continuous.ensure_channel` and logged by the loop on change.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TaskReport {
    pub task_id: String,
    pub channel_id: Option<String>,
    pub path: Option<String>,
    pub created: bool,
    pub bound: bool,
    pub orchestrator: Option<String>,
    pub orchestrator_name: Option<String>,
    pub named: bool,
    pub members: Vec<String>,
    pub joined: Vec<String>,
    pub deferred: Vec<String>,
}

/// Create-or-attach the task's channel per its policy. Returns the binding
/// to persist when the record must change; `Ok(None)` when nothing changed.
/// Creation is a coordinator-only mutation; a replica gets
/// `coordinator_required` and the caller retries on a later pass.
pub fn ensure_channel(
    store: &mut Store,
    task: &task::ContinuousTask,
) -> Result<Option<TaskChannel>, ChatError> {
    if task.task_channel == task::TaskChannelPolicy::Off {
        return Ok(None);
    }
    let path = channel_path(&task.task_id);
    if let Some(binding) = &task.messaging {
        if binding.space_id == store.space_id {
            if let Some(current) = store.channel_path_of(&binding.channel_id) {
                // `auto` owns the binding: a legacy record bound to a shared
                // channel (the 2026-09 `#orchestrators` stopgap) migrates to
                // its own `ct/<slug>` below. `manual` keeps whatever the
                // operator configured.
                let keep = task.task_channel == task::TaskChannelPolicy::Manual || current == path;
                if keep {
                    if binding.path.as_deref() == Some(current) {
                        return Ok(None);
                    }
                    let mut next = binding.clone();
                    next.path = Some(current.to_string());
                    return Ok(Some(next));
                }
            } else if task.task_channel == task::TaskChannelPolicy::Manual {
                // Configured channel is gone; leave the operator's binding
                // untouched so nothing silently rebinds.
                return Ok(None);
            }
        } else if task.task_channel == task::TaskChannelPolicy::Manual {
            return Ok(None);
        }
    }
    if task.task_channel == task::TaskChannelPolicy::Manual {
        return Ok(None);
    }
    let channel_id = match store.channel_id_at_path(&path) {
        Some(id) => id.to_string(),
        None => {
            let description = format!(
                "Continuous task {} — orchestrator and worker coordination. Planning task: {}. Mention the orchestrator for handoffs; the operator posts dispositions here.",
                task.task_id,
                task.planning_task_id.as_deref().unwrap_or("none")
            );
            let params = json!({
                "action": "create",
                "path": path,
                "name": task.label,
                "description": description,
                "request_id": format!("task-channel:{}", task.task_id),
            });
            let created = store.channel_action("owner", &params, &[])?;
            created["channel"]["id"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| store.channel_id_at_path(&path).map(str::to_owned))
                .ok_or_else(|| ChatError {
                    code: "internal".into(),
                    message: "channel create returned no id".into(),
                })?
        }
    };
    let mut next = TaskChannel {
        subscription_id: uuid::Uuid::new_v4().to_string(),
        space_id: store.space_id.clone(),
        channel_id,
        revision: 0,
        session_uid: None,
        path: Some(path),
    };
    if let Some(uid) = &task.current_session_uid {
        next.handover(uid);
    }
    Ok(Some(next))
}

/// Reconcile one task against the store: channel, subscription, orchestrator
/// name, member joins. Returns the report plus any joins a replica must
/// forward to the coordinator.
/// Coordinator joins performed per reconcile pass before the rest is left to
/// the next tick. Each join is a journal publish + projection under the store
/// lock; the first migration pass joined ~60 workers in one go and starved
/// RPCs and replica sync for minutes (2026-09-14).
const JOINS_PER_PASS: usize = 6;

fn reconcile_task(
    store: &mut Store,
    graph: &SessionGraph,
    mut task: task::ContinuousTask,
    active: &mut BTreeSet<String>,
    joins: &mut Vec<(String, Value)>,
    budget: &mut usize,
) -> Result<TaskReport, ChatError> {
    let mut report = TaskReport {
        task_id: task.task_id.clone(),
        ..TaskReport::default()
    };
    if !task.enabled {
        return Ok(report);
    }
    if store.messaging_frozen() {
        return Ok(report);
    }
    match ensure_channel(store, &task) {
        Ok(Some(next)) => {
            report.created = task
                .messaging
                .as_ref()
                .map(|m| m.channel_id != next.channel_id)
                .unwrap_or(true);
            let persisted = task::modify(&task.task_id, |t| {
                t.messaging = Some(next.clone());
                if let (Some(binding), Some(uid)) = (&mut t.messaging, &t.current_session_uid) {
                    binding.handover(uid);
                }
            })
            .map_err(|e| ChatError {
                code: "internal".into(),
                message: format!("persist task channel binding: {e}"),
            })?;
            task = persisted;
        }
        Ok(None) => {}
        Err(e) if e.code == "coordinator_required" || e.code == "coordinator_unavailable" => {
            report.deferred.push(format!("channel: {}", e.message));
            return Ok(report);
        }
        Err(e) => return Err(e),
    }
    let Some(binding) = task.messaging.clone() else {
        return Ok(report);
    };
    report.channel_id = Some(binding.channel_id.clone());
    report.path = binding
        .path
        .clone()
        .or_else(|| store.channel_path_of(&binding.channel_id).map(str::to_owned));
    if binding.space_id != store.space_id {
        return Ok(report);
    }
    let members = graph.members(&task);
    // Orchestrator: subscription + join + name.
    if let Some(uid) = binding
        .session_uid
        .clone()
        .filter(|uid| task.current_session_uid.as_ref() == Some(uid))
    {
        let actor = store.participant_id(&uid);
        store.bind_task_subscription(TaskBinding {
            id: binding.subscription_id.clone(),
            task_id: task.task_id.clone(),
            channel_id: binding.channel_id.clone(),
            actor: actor.clone(),
            revision: binding.revision,
            active: true,
            acknowledged: BTreeSet::new(),
        })?;
        active.insert(binding.subscription_id.clone());
        report.bound = true;
        report.orchestrator = Some(actor.clone());
        if !store.is_member(&binding.channel_id, &actor) {
            let params = json!({"action":"join","conversation":binding.channel_id,"request_id":format!("task-join:{}:{}",binding.subscription_id,binding.revision)});
            if store.is_coordinator() {
                store.channel_action(&actor, &params, &[])?;
                report.joined.push(actor.clone());
            } else {
                joins.push((actor.clone(), params));
                report.deferred.push(format!("join {actor}"));
            }
        }
        let name = orchestrator_name(&task.task_id);
        if store.is_coordinator() {
            let release: Vec<String> = store
                .names
                .iter()
                .filter(|(id, _)| **id != actor)
                .map(|(id, _)| id.clone())
                .collect();
            match store.assign_task_name(
                &actor,
                &uid,
                &name,
                &release,
                &format!("task-name:{}:{}", binding.subscription_id, binding.revision),
            ) {
                Ok(Some(_)) => report.named = true,
                Ok(None) => {}
                Err(e) => report.deferred.push(format!("name: {}", e.message)),
            }
        } else {
            report.deferred.push(format!("name {name}: coordinator only"));
        }
        report.orchestrator_name = store.names.get(&actor).map(|n| n.name.clone());
    }
    // Workers and helpers: self-join as each participant. A member who left
    // on purpose keeps the same request id, so the retry returns the prior
    // result instead of rejoining them.
    for uid in &members {
        let actor = store.participant_id(uid);
        report.members.push(actor.clone());
        if store.is_member(&binding.channel_id, &actor) {
            continue;
        }
        let params = json!({"action":"join","conversation":binding.channel_id,"request_id":format!("task-member-join:{}:{}",binding.subscription_id,uid)});
        if store.is_coordinator() {
            if *budget == 0 {
                report.deferred.push(format!("join {actor}: next pass"));
                continue;
            }
            *budget -= 1;
            match store.channel_action(&actor, &params, &[]) {
                Ok(_) => report.joined.push(actor),
                Err(e) => report.deferred.push(format!("join {actor}: {}", e.message)),
            }
        } else {
            joins.push((actor.clone(), params));
            report.deferred.push(format!("join {actor}"));
        }
    }
    Ok(report)
}

/// Refresh every task's fence, channel, name and membership synchronously.
/// The background loop also runs this so a replacement doesn't need to
/// discover chat first. Per-task failures are logged, not fatal.
pub fn refresh(state: &Arc<Mutex<DaemonState>>) -> Result<Vec<(String, Value)>, ChatError> {
    let (handle, root, graph) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging.clone(),
            s.messaging_root.clone(),
            SessionGraph::capture(&s),
        )
    };
    let mut records: BTreeMap<String, task::ContinuousTask> = task::load_all()
        .into_iter()
        .map(|t| (t.task_id.clone(), t))
        .collect();
    {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(store) = slot.as_ref() {
            for id in store.configured_task_ids() {
                if !records.contains_key(&id) {
                    if let Some(t) = task::load_one(&id) {
                        records.insert(id, t);
                    }
                }
            }
        }
    }
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    if slot.is_none() {
        *slot = Some(Store::open(&root)?);
    }
    let store = slot.as_mut().unwrap();
    if store.messaging_frozen() {
        return Ok(Vec::new());
    }
    let mut joins = Vec::new();
    let mut active = BTreeSet::new();
    let mut budget = JOINS_PER_PASS;
    for (_, task) in records {
        let id = task.task_id.clone();
        match reconcile_task(store, &graph, task, &mut active, &mut joins, &mut budget) {
            Ok(report) => {
                if report.created || report.named || !report.joined.is_empty() {
                    eprintln!(
                        "cm task messaging: {} channel={} created={} named={} joined={}",
                        id,
                        report.path.as_deref().unwrap_or("-"),
                        report.created,
                        report.named,
                        report.joined.len()
                    );
                }
            }
            Err(e) => eprintln!("cm task messaging: {id}: {e}"),
        }
    }
    store.fence_task_subscriptions(&active)?;
    Ok(joins)
}

/// Reconcile ONE task now (continuous.create / continuous.ensure_channel).
pub fn ensure_for_task(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
) -> Result<TaskReport, ChatError> {
    super::rpc::initialize(state)?;
    let (handle, graph, sync) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        (
            s.messaging.clone(),
            SessionGraph::capture(&s),
            s.messaging_sync.clone(),
        )
    };
    let task = task::load_one(task_id).ok_or_else(|| ChatError {
        code: "not_found".into(),
        message: format!("continuous task '{task_id}' not found"),
    })?;
    let mut joins = Vec::new();
    let report = {
        let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        let store = slot.as_mut().ok_or_else(|| ChatError {
            code: "internal".into(),
            message: "messaging store is not open".into(),
        })?;
        let mut active: BTreeSet<String> = store.configured_task_ids().into_iter().collect();
        // An explicit ensure is allowed to do all of its joins at once.
        let mut budget = usize::MAX;
        let report = reconcile_task(store, &graph, task, &mut active, &mut joins, &mut budget)?;
        report
    };
    if let Some(sync) = sync {
        for (actor, params) in joins {
            let _ = sync.request(&actor, "messaging.channels", &params);
        }
    }
    Ok(report)
}

/// Orientation for `chat_open`: the task channel(s) a session belongs to and
/// who its orchestrator is right now, so a worker never needs
/// `list_sessions` to address its parent and a parent never needs it to
/// address a worker.
pub fn orientation(store: &Store, graph: &SessionGraph, uid: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for task in task::load_all() {
        let Some(binding) = &task.messaging else {
            continue;
        };
        if binding.space_id != store.space_id {
            continue;
        }
        let role = if task.current_session_uid.as_deref() == Some(uid) {
            "orchestrator"
        } else if graph.members(&task).contains(uid) {
            "member"
        } else {
            continue;
        };
        let orchestrator = task.current_session_uid.as_ref().map(|o| {
            let actor = store.participant_id(o);
            json!({
                "participant_id": actor,
                "session_uid": o,
                "name": store.names.get(&actor).map(|n| n.name.clone()),
                "present": graph.live.contains_key(o),
            })
        });
        out.push(json!({
            "task_id": task.task_id,
            "planning_task_id": task.planning_task_id,
            "label": task.label,
            "paused": task.paused,
            "role": role,
            "channel": {
                "id": binding.channel_id,
                "path": binding.path.clone().or_else(|| store.channel_path_of(&binding.channel_id).map(str::to_owned)),
                "members": store.member_count(&binding.channel_id),
            },
            "orchestrator": orchestrator,
            "orchestrator_name": orchestrator_name(&task.task_id),
        }));
    }
    out
}

/// Post a system notice into a task's channel (held runs, recoveries).
/// Best-effort: a task without a channel or a frozen store is skipped.
pub fn notify_task_channel(
    state: &Arc<Mutex<DaemonState>>,
    task_id: &str,
    key: &str,
    body: &str,
) -> Result<bool, ChatError> {
    let Some(task) = task::load_one(task_id) else {
        return Ok(false);
    };
    let Some(binding) = task.messaging else {
        return Ok(false);
    };
    let handle = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging
        .clone();
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    let Some(store) = slot.as_mut() else {
        return Ok(false);
    };
    if store.messaging_frozen() || binding.space_id != store.space_id {
        return Ok(false);
    }
    store.post_system_notice(&binding.channel_id, body, key)?;
    Ok(true)
}

/// Archive a task's channel (posting pause, history readable) when the task
/// is deleted. Best-effort.
pub fn archive_task_channel(state: &Arc<Mutex<DaemonState>>, task: &task::ContinuousTask) {
    let Some(binding) = &task.messaging else {
        return;
    };
    let handle = state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .messaging
        .clone();
    let mut slot = handle.lock().unwrap_or_else(|p| p.into_inner());
    let Some(store) = slot.as_mut() else {
        return;
    };
    if !store.is_coordinator() || binding.space_id != store.space_id {
        return;
    }
    let Some(path) = store.channel_path_of(&binding.channel_id).map(str::to_owned) else {
        return;
    };
    let info = store.channel_info(&path, &binding.channel_id);
    let params = json!({
        "action": "update",
        "conversation": binding.channel_id,
        "archived": true,
        "expected_revision": info["revision"],
        "request_id": format!("task-channel-archive:{}:{}", task.task_id, info["revision"].as_str().unwrap_or("")),
    });
    if let Err(e) = store.channel_action("owner", &params, &[]) {
        eprintln!("cm task messaging: archive {} failed: {e}", task.task_id);
    }
}

pub fn spawn(state: &Arc<Mutex<DaemonState>>) {
    let weak = Arc::downgrade(state);
    std::thread::spawn(move || loop {
        let Some(state) = weak.upgrade() else {
            break;
        };
        match refresh(&state) {
            Ok(joins) => {
                let sync = state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .messaging_sync
                    .clone();
                if let Some(sync) = sync {
                    for (actor, params) in joins {
                        let _ = sync.request(&actor, "messaging.channels", &params);
                    }
                }
            }
            Err(e) => eprintln!("cm task messaging: {e}"),
        }
        drop(state);
        std::thread::sleep(Duration::from_secs(2));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_slug_and_names_are_legal_channel_segments() {
        assert_eq!(channel_slug("bug-triage"), "bug-triage");
        assert_eq!(channel_slug("Codex_Migration__Canary-20260907"), "codex-migration-canary-20260907");
        assert_eq!(channel_slug("--x--"), "x");
        assert_eq!(channel_path("health-alert-triage"), "ct/health-alert-triage");
        assert_eq!(orchestrator_name("health-alert-triage"), "health-alert-triage-orchestrator");
        let long = "a".repeat(60);
        let name = orchestrator_name(&long);
        assert!(name.ends_with("-orchestrator"));
        assert!(name.chars().count() <= 40, "{name}");
    }

    #[test]
    fn members_follow_tags_managed_by_closure_and_planning_descendants() {
        let mut graph = SessionGraph::default();
        let mut task = task::ContinuousTask::new(
            "bug-triage".into(),
            "Bug".into(),
            "ws".into(),
            "/tmp/wt".into(),
            task::Engine::Codex,
            task::RunMode::Persistent,
            task::Schedule::OnDemand,
            "p".into(),
        );
        task.planning_task_id = Some("plan-root".into());
        task.current_session_uid = Some("orch-2".into());
        task.retirement = None;
        // orch-1 was the previous instance (dead); worker-a is managed by it.
        graph.exited.insert("orch-1".into(), (None, Some("bug-triage".into())));
        graph.live.insert("worker-a".into(), (Some("child-1".into()), Some("orch-1".into()), None));
        // worker-b is managed by the live orchestrator; helper-c by worker-b.
        graph.live.insert("worker-b".into(), (Some("child-2".into()), Some("orch-2".into()), None));
        graph.live.insert("helper-c".into(), (None, Some("worker-b".into()), None));
        // worker-d is only related through the planning tree.
        graph.live.insert("worker-d".into(), (Some("grandchild".into()), None, None));
        graph.task_tree.insert("child-9".into(), Some("plan-root".into()));
        graph.task_tree.insert("grandchild".into(), Some("child-9".into()));
        // stranger shares nothing.
        graph.live.insert("stranger".into(), (Some("other".into()), Some("someone".into()), None));
        graph.live.insert("orch-2".into(), (Some("plan-root".into()), None, Some("bug-triage".into())));
        let members = graph.members(&task);
        assert_eq!(
            members.into_iter().collect::<Vec<_>>(),
            vec!["helper-c", "orch-2", "worker-a", "worker-b", "worker-d"]
        );
    }
}
