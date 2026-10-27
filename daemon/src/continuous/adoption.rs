//! Operator-only, host-local adoption. No executor or prompt-delivery calls.
//!
//! A durable intent precedes the remote CAS. Every intermediate task is paused;
//! identical retries finish it. The final task write is the activation commit.
use super::{migration, task};
use crate::{agent_state, control::protocol::ErrorCode, items::api::Api, state::DaemonState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

type Result<T> = std::result::Result<T, (ErrorCode, String)>;
fn conflict(s: impl Into<String>) -> (ErrorCode, String) {
    (ErrorCode::Conflict, s.into())
}
fn internal(e: impl std::fmt::Display) -> (ErrorCode, String) {
    (ErrorCode::Internal, e.to_string())
}
fn invalid(e: impl std::fmt::Display) -> (ErrorCode, String) {
    (ErrorCode::InvalidParams, e.to_string())
}

// Serializes adoption, workflow binding, operator edits and task messaging.
// Acquire before lifecycle, DaemonState, messaging-store or task-file locks;
// never wait for this gate from their callbacks. Messaging RPC refresh uses
// try_binding_lock so nested calls/contended adoption cannot reverse that order.
static BINDING: Mutex<()> = Mutex::new(());
pub(crate) struct BindingGuard {
    _guard: MutexGuard<'static, ()>,
}
pub(crate) fn binding_lock() -> BindingGuard {
    BindingGuard { _guard: BINDING.lock().unwrap_or_else(|p| p.into_inner()) }
}
pub(crate) fn try_binding_lock() -> Option<BindingGuard> {
    match BINDING.try_lock() {
        Ok(guard) => Some(BindingGuard { _guard: guard }),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(BindingGuard { _guard: p.into_inner() }),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    #[serde(default)]
    pub daemon_id: String,
    #[serde(default)]
    pub host: String,
    pub planning_task_id: String,
    pub session_uid: String,
    pub workspace_id: String,
    pub worktree_path: String,
    pub engine: task::Engine,
    /// Centrally guarded workspaces may only retain their explicit binding.
    /// Preview reports the epoch; commit must echo it, including on retry.
    #[serde(default)]
    pub workspace_binding_epoch: Option<u64>,
    // Omit process fields for a preview; the commit must echo its full identity.
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub child_start_time: Option<u64>,
    #[serde(default)]
    pub holder_incarnation: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCron {
    /// `none` or `removed_by_owner`; CM cannot execute engine CronDelete.
    status: String,
    owner_session_uid: Option<String>,
    #[serde(default)]
    cron_ids: Vec<String>,
    evidence: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    request_id: String,
    task_id: String,
    expected: Identity,
    expected_state_hash: Option<String>,
    expected_registration: Option<Value>,
    schedule: task::Schedule,
    first_fire_at: u64,
    label: String,
    default_prompt: String,
    native_cron: NativeCron,
    #[serde(default)]
    dry_run: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Intent {
    request: Value,
    registration: Value,
    original_hash: Option<String>,
    proposed: task::ContinuousTask,
    receipt: Option<Value>,
}
fn root() -> PathBuf {
    crate::path::dot_cm_dir().join("continuous-adoptions")
}
fn intent_path(id: &str) -> PathBuf {
    root().join(format!("{id}.json"))
}
fn read_intent(id: &str) -> Result<Option<Intent>> {
    match fs::read(intent_path(id)) {
        Ok(b) => serde_json::from_slice(&b).map(Some).map_err(internal),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(internal(e)),
    }
}
/// fsync both content and rename. Also sync the parent of a newly made directory.
fn durable_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let dir = path.parent().unwrap();
    fs::create_dir_all(dir).map_err(internal)?;
    if let Some(parent) = dir.parent() {
        fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(internal)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(internal)?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(internal)?)
        .map_err(internal)?;
    file.sync_all().map_err(internal)?;
    fs::rename(tmp, path).map_err(internal)?;
    fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .map_err(internal)
}
fn intents() -> Result<Vec<Intent>> {
    let entries = match fs::read_dir(root()) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(internal(e)),
    };
    entries
        .filter_map(|e| match e {
            Ok(e) if e.path().extension().is_some_and(|s| s == "json") => Some(
                fs::read(e.path())
                    .map_err(internal)
                    .and_then(|b| serde_json::from_slice(&b).map_err(internal)),
            ),
            Ok(_) => None,
            Err(e) => Some(Err(internal(e))),
        })
        .collect()
}
/// Protect even the crash window between the intent and paused-task writes.
pub(crate) fn ensure_available(task_id: &str) -> Result<()> {
    if intents()?
        .iter()
        .any(|i| i.proposed.task_id == task_id && i.receipt.is_none())
    {
        return Err(conflict(
            "adoption_pending: retry the original continuous.adopt_existing request",
        ));
    }
    Ok(())
}
pub(crate) fn session_available(uid: &str) -> Result<()> {
    if intents()?
        .iter()
        .any(|i| i.proposed.current_session_uid.as_deref() == Some(uid) && i.receipt.is_none())
    {
        return Err(conflict(
            "adoption_pending: session reserved by continuous adoption",
        ));
    }
    Ok(())
}
fn read_tasks() -> Result<Vec<task::ContinuousTask>> {
    task::load_all_strict().map_err(internal)
}
fn readback(t: &task::ContinuousTask) -> Value {
    json!({"task_id":t.task_id,"planning_task_id":t.planning_task_id,"current_session_uid":t.current_session_uid,
        "workspace_id":t.workspace_id,"worktree_path":t.worktree_path,"engine":t.engine,"host_id":t.host_id,
        "schedule":t.schedule,"next_fire_at":t.next_fire_at,"paused":t.paused,"enabled":t.enabled,
        "adoption_pending":t.adoption_pending,"messaging":t.messaging,"run_count":t.run_count})
}
fn check_native(p: &Params) -> Result<()> {
    let n = &p.native_cron;
    if !matches!(n.status.as_str(), "none" | "removed_by_owner") {
        return Err(conflict("unsupported_owner: CM cannot retire an engine-native cron; its owner must remove it and verify removal first"));
    }
    if n.evidence.trim().is_empty()
        || (n.status == "removed_by_owner"
            && (n.owner_session_uid.as_ref().is_none_or(|s| s.is_empty()) || n.cron_ids.is_empty()))
    {
        return Err(invalid("native_cron requires operator evidence; removed_by_owner also requires owner_session_uid and cron_ids"));
    }
    if n.status == "none" && (!n.cron_ids.is_empty() || n.owner_session_uid.is_some()) {
        return Err(invalid("none must not identify a retired cron"));
    }
    Ok(())
}
fn observe(state: &DaemonState, p: &Params, daemon_id: &str) -> Result<Identity> {
    let e = &p.expected;
    let s = state
        .sessions
        .get(&e.session_uid)
        .ok_or_else(|| conflict("session is not live on this daemon"))?;
    let ws = state
        .workspaces
        .get(&s.workspace_id)
        .ok_or_else(|| conflict("workspace missing"))?;
    let path = ws
        .worktree_path
        .as_ref()
        .ok_or_else(|| conflict("workspace has no local worktree"))?;
    let engine = match s.session_type.as_str() {
        "claude-code" => task::Engine::Claude,
        "codex" => task::Engine::Codex,
        _ => {
            return Err(conflict(
                "only local Claude/Codex agents can be adopted (no shell/workflow engine)",
            ))
        }
    };
    if s.last_exit.kernel_set()
        || s.workflow_run_id.is_some()
        || s.workflow_role.is_some()
        || ws.is_closed
    {
        return Err(conflict(
            "session is exited, workflow-bound, or workspace closed",
        ));
    }
    if s.continuous_task_id
        .as_deref()
        .is_some_and(|id| id != p.task_id)
    {
        return Err(conflict(
            "session already belongs to another continuous owner",
        ));
    }
    for (source, bindings) in [("task_workspaces", &state.task_workspaces), ("bindings", &state.bindings)] {
        if bindings.get(&e.planning_task_id).is_some_and(|id| id != &s.workspace_id) {
            return Err(conflict(format!("planning task workspace mapping conflicts in {source}")));
        }
    }
    if crate::state::binding_guarded(ws)
        && (state.bindings.get(&e.planning_task_id) != Some(&s.workspace_id)
            || state.inferred_bindings.iter().any(|(task_id, ws_id)| task_id == &e.planning_task_id && ws_id == &s.workspace_id))
    {
        return Err(conflict("guarded workspace requires an explicit matching durable planning binding; session task identity is not binding authority"));
    }
    if crate::workflow::run::load_all_strict()
        .map_err(internal)?
        .iter()
        .any(|r| {
            r.is_active()
                && r.role_sessions
                    .values()
                    .any(|b| b.daemon_session_uid.as_deref() == Some(&e.session_uid))
        })
    {
        return Err(conflict("session is reserved by an active workflow record"));
    }
    let a = agent_state::read_only(s);
    if a.state != agent_state::State::Idle
        || !a.source.engine_reported()
        || a.detail.input_pending == Some(true)
        || !a.background.complete
        || !a.background.jobs.is_empty()
    {
        return Err(conflict(format!(
            "agent must be semantically idle without background jobs, observed {}",
            a.state.as_str()
        )));
    }
    // Do not turn an operator attestation into a claim of engine cancellation.
    for other in state.sessions.values().filter(|s| {
        s.task_id == Some(e.planning_task_id.clone())
            || s.uid == e.session_uid
            || p.native_cron.owner_session_uid.as_ref() == Some(&s.uid)
    }) {
        if !other.last_exit.kernel_set()
            && !agent_state::read_only(other).background.crons.is_empty()
        {
            return Err(conflict(format!("unsupported_owner: native cron still observed on {}; require its owner's verified removal", other.uid)));
        }
    }
    let cell = s.agent_state.lock().unwrap_or_else(|p| p.into_inner());
    let kernel_start = crate::adopt::proc_starttime(s.pid)
        .map_err(|_| conflict("session process is no longer live"))?;
    if cell.child_start_time != Some(kernel_start)
        || !s.process_alive_read_only().map_err(internal)?
    {
        return Err(conflict(
            "identity_changed: process exited or kernel identity differs",
        ));
    }
    let got = Identity {
        daemon_id: daemon_id.into(),
        host: crate::items::heartbeat::host_label()
            .ok_or_else(|| conflict("host identity unavailable"))?,
        planning_task_id: s.task_id.clone().unwrap_or_default(),
        session_uid: s.uid.clone(),
        workspace_id: s.workspace_id.clone(),
        worktree_path: path.to_string_lossy().into_owned(),
        engine,
        workspace_binding_epoch: cm_holder_proto::org::binding_epoch(ws.org.as_ref()),
        pid: s.pid,
        child_start_time: cell.child_start_time,
        holder_incarnation: s.holder_incarnation,
    };
    let mut expected = e.clone();
    if p.dry_run {
        if expected.workspace_binding_epoch.is_none() {
            expected.workspace_binding_epoch = got.workspace_binding_epoch;
        }
        if expected.daemon_id.is_empty() {
            expected.daemon_id = got.daemon_id.clone();
        }
        if expected.host.is_empty() {
            expected.host = got.host.clone();
        }
    }
    if p.dry_run && expected.pid == 0 {
        expected.pid = got.pid;
        expected.child_start_time = got.child_start_time;
        expected.holder_incarnation = got.holder_incarnation;
    }
    if expected != got || got.child_start_time.is_none() {
        return Err(conflict(format!("identity_changed: expected task/session/workspace/engine/host/process must match; observed {}", serde_json::to_value(&got).unwrap())));
    }
    if !path.is_dir() {
        return Err(conflict("worktree directory missing"));
    }
    Ok(got)
}
fn eligible_existing(t: &task::ContinuousTask, p: &Params, state: &DaemonState) -> Result<()> {
    if !matches!(t.host_id.as_str(), "local") && t.host_id != p.expected.host {
        return Err(conflict("continuous record belongs to another host"));
    }
    if !t.paused
        || t.in_flight.is_some()
        || t.last_run.as_ref().is_some_and(|r| {
            matches!(
                r.status,
                task::RunStatus::Running | task::RunStatus::Pending | task::RunStatus::Stuck
            )
        })
        || t.investigator_uid.is_some()
        || t.reconciliation.is_some()
        || t.drain.is_some()
        || t.retirement.is_some()
        || t.recovery_hold.is_some()
        || t.recovery.is_some()
        || t.account_blocked.is_some()
    {
        return Err(conflict("existing continuous task must be paused, settled and free of drain/recovery/retirement holds"));
    }
    if t.planning_task_id.as_deref() != Some(&p.expected.planning_task_id)
        || t.workspace_id != p.expected.workspace_id
        || t.worktree_path != p.expected.worktree_path
        || t.engine != p.expected.engine
    {
        return Err(conflict("existing continuous task identity differs"));
    }
    if t.current_session_uid
        .iter()
        .chain(t.messaging.as_ref().and_then(|m| m.session_uid.as_ref()))
        .any(|uid| {
            uid != &p.expected.session_uid
                && state
                    .sessions
                    .get(uid)
                    .is_some_and(|s| !s.last_exit.kernel_set())
        })
    {
        return Err(conflict(
            "another live orchestrator owns this continuous task",
        ));
    }
    Ok(())
}

fn validate_legacy_owners(
    current: &Value,
    host: &str,
    task_id: &str,
    workspace: &str,
    path: &str,
    engine: task::Engine,
) -> Result<()> {
    let owners = current["api_host_owners"]
        .as_array()
        .ok_or_else(|| conflict("API-host ownership inventory missing"))?;
    for old in owners {
        if old["host"] != host
            || old["task_id"] != task_id
            || old["workspace_id"] != workspace
            || old["worktree_path"] != path
            || old["engine"] != json!(engine)
        {
            return Err(conflict(
                "legacy_owner_conflict: API-host continuous record already owns this planning task",
            ));
        }
    }
    Ok(())
}

fn validate_registration(r: &Value, p: &Params, has_local_record: bool) -> Result<()> {
    validate_legacy_owners(
        r,
        &p.expected.host,
        &p.task_id,
        &p.expected.workspace_id,
        &p.expected.worktree_path,
        p.expected.engine,
    )?;
    if r["task_id"] != p.expected.planning_task_id
        || !matches!(r["kind"].as_str(), Some("oneshot" | "continuous"))
    {
        return Err(conflict("planning task identity/kind does not match"));
    }
    if matches!(
        r["status"].as_str(),
        Some("done" | "archived" | "cancelled")
    ) {
        return Err(conflict("terminal planning task cannot be adopted"));
    }
    if r["kind"] == "continuous" && !has_local_record {
        return Err(conflict(
            "continuous planning task has no local record; locate its owning scheduler first",
        ));
    }
    let b = &r["binding"];
    if !b.is_null()
        && (b["daemon_id"] != p.expected.daemon_id || b["continuous_task_id"] != p.task_id)
    {
        return Err(conflict(
            "planning task belongs to another daemon/scheduler",
        ));
    }
    let c = &r["continuous"];
    if !c.is_null()
        && (!c.is_object()
            || c.get("host")
                .is_some_and(|v| !v.is_null() && v != &json!(p.expected.host))
            || c.get("task_id")
                .is_some_and(|v| !v.is_null() && v != &json!(p.task_id)))
    {
        return Err(conflict(
            "planning continuous metadata belongs to another scheduler",
        ));
    }
    Ok(())
}

// Replayed API receipts report historical success, not permission to activate
// today. Validate their current task and mirror, and re-read just before commit.
fn validate_claim(current: &Value, binding: &Value, planning_task_id: &str) -> Result<()> {
    validate_legacy_owners(
        current,
        binding["host"].as_str().unwrap_or_default(),
        binding["continuous_task_id"].as_str().unwrap_or_default(),
        binding["workspace_id"].as_str().unwrap_or_default(),
        binding["worktree_path"].as_str().unwrap_or_default(),
        serde_json::from_value(binding["engine"].clone()).map_err(internal)?,
    )?;
    if current["task_id"] != planning_task_id
        || current["kind"] != "continuous"
        || current["status"].as_str().is_none()
        || matches!(
            current["status"].as_str(),
            Some("done" | "archived" | "cancelled")
        )
        || current["binding"] != *binding
    {
        return Err(conflict("planning_claim_changed: task identity/kind/status/ownership changed; pending adoption remains fenced"));
    }
    let c = &current["continuous"];
    if c["task_id"] != binding["continuous_task_id"]
        || c["host"] != binding["host"]
        || c["engine"] != binding["engine"]
        || c["run_mode"] != "persistent"
    {
        return Err(conflict(
            "planning_claim_changed: continuous metadata no longer matches this scheduler/engine",
        ));
    }
    Ok(())
}

pub fn handle(state: &Arc<Mutex<DaemonState>>, params: &Value) -> Result<Value> {
    let p: Params = serde_json::from_value(params.clone()).map_err(invalid)?;
    task::validate_task_id(&p.task_id).map_err(invalid)?;
    task::validate_task_id(&p.request_id).map_err(invalid)?;
    uuid::Uuid::parse_str(&p.expected.planning_task_id).map_err(invalid)?;
    let every = match p.schedule { task::Schedule::Periodic{every_secs} if every_secs > 0 => every_secs,
        _ => return Err(invalid("unsupported_schedule: adoption supports only Periodic with every_secs > 0; Cron is not implemented")) };
    if p.label.trim().is_empty() || p.default_prompt.trim().is_empty() {
        return Err(invalid("label/default_prompt required"));
    }
    check_native(&p)?;
    let _binding = binding_lock();
    let mut canonical = serde_json::to_value(&p).map_err(internal)?;
    canonical["dry_run"] = json!(false);
    let old = read_intent(&p.request_id)?;
    if let Some(i) = &old {
        if i.request != canonical {
            return Err(conflict(
                "request_id already describes a different adoption",
            ));
        }
        if let Some(receipt) = &i.receipt {
            let current = read_tasks()?
                .iter()
                .find(|t| t.task_id == p.task_id)
                .map(readback);
            return Ok(json!({"adopted":true,"replayed":true,"receipt":receipt,"current":current}));
        }
    }
    let tasks = read_tasks()?;
    let existing = tasks.iter().find(|t| t.task_id == p.task_id);
    // Final task commit is authoritative even if the reply/journal update was lost.
    if let (Some(i), Some(t)) = (&old, existing) {
        if t.adoption_pending.is_none()
            && t.adoption_receipt
                .as_ref()
                .is_some_and(|r| r["request_id"] == p.request_id)
        {
            let mut done = i.clone();
            done.receipt = t.adoption_receipt.clone();
            if !p.dry_run {
                durable_json(&intent_path(&p.request_id), &done)?;
            }
            return Ok(
                json!({"adopted":true,"replayed":true,"receipt":done.receipt,"current":readback(t)}),
            );
        }
    }
    let (api, handle) = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        if s.draining {
            return Err(conflict("daemon is draining"));
        }
        if !s.config.scheduler.enabled {
            return Err(conflict(
                "local scheduler disabled; adoption does not relocate the session to manager",
            ));
        }
        if s.daemon_sessions_path.is_none() {
            return Err(conflict("durable session manifest is not configured"));
        }
        (
            Api::from_config(&s.config.api_url, &s.config.api_token)?,
            s.messaging.clone(),
        )
    };
    let daemon_id = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref()
            .ok_or_else(|| conflict("messaging not ready; wait for startup"))?
            .daemon_id
            .clone()
    };
    for i in intents()? {
        if i.receipt.is_none()
            && i.request["request_id"] != p.request_id
            && (i.proposed.task_id == p.task_id
                || i.proposed.planning_task_id.as_deref() == Some(&p.expected.planning_task_id)
                || i.proposed.current_session_uid.as_deref() == Some(&p.expected.session_uid))
        {
            return Err(conflict(
                "another pending adoption reserves this task/session",
            ));
        }
    }
    for t in &tasks {
        if t.task_id != p.task_id
            && (t.planning_task_id.as_deref() == Some(&p.expected.planning_task_id)
                || t.current_session_uid.as_deref() == Some(&p.expected.session_uid))
        {
            return Err(conflict(
                "another local continuous record owns this planning task/session",
            ));
        }
    }
    let observed = {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        let observed = observe(&s, &p, &daemon_id)?;
        if old.is_none() {
            if let Some(t) = existing {
                let mut checked = p.clone();
                checked.expected = observed.clone();
                eligible_existing(t, &checked, &s)?;
            }
        }
        observed
    };
    let path = format!(
        "/tasks/{}/continuous-registration",
        p.expected.planning_task_id
    );
    let registration = if let Some(i) = &old {
        i.registration.clone()
    } else {
        api.get(&path, &[])?
    };
    if old.is_none() {
        let mut checked = p.clone();
        checked.expected = observed.clone();
        validate_registration(&registration, &checked, existing.is_some())?;
    }
    // The planning read can block while the local process exits, is removed,
    // or changes identity. Reject before even previewing/staging a stale agent.
    {
        let s = state.lock().unwrap_or_else(|p| p.into_inner());
        if observe(&s, &p, &daemon_id)? != observed {
            return Err(conflict("identity_changed during planning registration read"));
        }
    }
    let hash = existing.map(migration::state_hash);
    if p.dry_run {
        return Ok(
            json!({"dry_run":true,"expected":observed,"expected_state_hash":hash,"expected_registration":registration,
            "schedule":p.schedule,"first_fire_at":p.first_fire_at,"task_id":p.task_id,"channel_path":crate::messaging::tasks::channel_path(&p.task_id),
            "native_cron_verification":"operator evidence; no engine cancellation performed","pending":old.is_some()}),
        );
    }
    let mut intent = if let Some(i) = old {
        i
    } else {
        if p.first_fire_at <= task::now_unix() {
            return Err(invalid("first_fire_at must be in the future"));
        }
        if p.expected_state_hash != hash || p.expected_registration.as_ref() != Some(&registration)
        {
            return Err(conflict("snapshot changed: repeat dry-run and echo expected_state_hash/expected_registration"));
        }
        let mut t = existing.cloned().unwrap_or_else(|| {
            task::ContinuousTask::new(
                p.task_id.clone(),
                p.label.clone(),
                p.expected.workspace_id.clone(),
                p.expected.worktree_path.clone(),
                p.expected.engine,
                task::RunMode::Persistent,
                p.schedule.clone(),
                p.default_prompt.clone(),
            )
        });
        if existing.is_none() {
            let s = state.lock().unwrap_or_else(|p| p.into_inner());
            observe(&s, &p, &daemon_id)?;
            let session = s.sessions.get(&p.expected.session_uid)
                .ok_or_else(|| conflict("session disappeared before staging"))?;
            t.model = session.model.clone();
            t.effort = session.effort.clone();
            t.host_id = observed.host.clone();
        }
        t.planning_task_id = Some(p.expected.planning_task_id.clone());
        t.current_session_uid = Some(p.expected.session_uid.clone());
        t.run_mode = task::RunMode::Persistent;
        t.schedule = p.schedule.clone();
        t.next_fire_at = p.first_fire_at;
        // Keep background channel reconciliation off until the planning claim
        // and session manifest are durable. Pending also fences the scheduler.
        t.enabled = false;
        t.paused = true;
        t.supervise = false;
        t.admission_revision = t.admission_revision.saturating_add(1);
        t.adoption_pending = Some(p.request_id.clone());
        t.adoption_receipt = None;
        if let Some(m) = &mut t.messaging {
            // Every adoption gets a new task-subscription generation, including
            // same-UID re-adoption. Its quiet checkpoint must not reuse an old
            // live monitor with historical hits.
            m.revision = m
                .revision
                .checked_add(1)
                .ok_or_else(|| conflict("channel revision overflow"))?;
            m.session_uid = Some(p.expected.session_uid.clone());
        }
        if t.task_channel == task::TaskChannelPolicy::Off {
            return Err(conflict("adoption requires a task channel"));
        }
        Intent {
            request: canonical,
            registration,
            original_hash: hash,
            proposed: t,
            receipt: None,
        }
    };
    // This is the only new local directory before the intent. Dry-run never
    // calls lock_lifecycle/load_one/initialize (all can create files).
    fs::create_dir_all(task::task_dir(&p.task_id)).map_err(internal)?;
    let _lifecycle = task::lock_lifecycle(&p.task_id).map_err(internal)?;
    let current = read_tasks()?.into_iter().find(|t| t.task_id == p.task_id);
    if current
        .as_ref()
        .is_none_or(|t| t.adoption_pending.as_deref() != Some(&p.request_id))
    {
        if current.as_ref().map(migration::state_hash) != intent.original_hash {
            return Err(conflict("continuous record changed before staging"));
        }
        durable_json(&intent_path(&p.request_id), &intent)?;
        fault("intent")?;
        task::save(&intent.proposed).map_err(internal)?;
    }
    fault("staged")?;
    let binding = json!({"daemon_id":daemon_id,"host":observed.host,"continuous_task_id":p.task_id,
        "session_uid":observed.session_uid,"workspace_id":observed.workspace_id,"worktree_path":observed.worktree_path,"engine":observed.engine});
    let claimed=api.put(&path,&json!({"actor":"owner","request_id":p.request_id,"expected_kind":intent.registration["kind"],
        "expected_continuous":intent.registration["continuous"],"expected_binding":intent.registration["binding"],"binding":binding}))?;
    validate_claim(&claimed["current"], &binding, &p.expected.planning_task_id)?;
    fault("registered")?;
    {
        let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
        observe(&s, &p, &daemon_id)?;
        s.sessions
            .get_mut(&p.expected.session_uid)
            .unwrap()
            .continuous_task_id = Some(p.task_id.clone());
        s.workspaces
            .get_mut(&p.expected.workspace_id)
            .unwrap()
            .pinned = true;
        s.task_workspaces.insert(
            p.expected.planning_task_id.clone(),
            p.expected.workspace_id.clone(),
        );
        // task_workspaces is a TUI snapshot and is not persisted. Record the
        // explicit durable resolver too, just as task.register_agent_subtask
        // does, after observe validated both maps and the workspace epoch.
        s.bindings.insert(p.expected.planning_task_id.clone(), p.expected.workspace_id.clone());
        s.inferred_bindings.retain(|(task_id, _)| task_id != &p.expected.planning_task_id);
        s.save_daemon_sessions_checked(s.daemon_sessions_path.as_ref().unwrap())
            .map_err(internal)?;
        let entry = s.sessions[&p.expected.session_uid].to_manifest_entry();
        s.manifest_watcher
            .broadcast(crate::manifest::ManifestDiff::Updated {
                uid: p.expected.session_uid.clone(),
                entry: serde_json::to_value(entry).map_err(internal)?,
            });
    }
    fault("session")?;
    task::modify(&p.task_id, |t| t.enabled = true).map_err(internal)?;
    let channel = crate::messaging::tasks::ensure_for_task_locked(
        state, &p.task_id, &_binding, Some(&p.request_id),
    ).map_err(internal)?;
    let channel_ready = {
        let slot = handle.lock().unwrap_or_else(|p| p.into_inner());
        slot.as_ref().is_some_and(|store| {
            channel.bound
                && channel.channel_id.as_ref().is_some_and(|id| {
                    store.is_member(id, &store.participant_id(&p.expected.session_uid))
                })
        })
    };
    if !channel_ready {
        return Ok(
            json!({"adopted":false,"pending":true,"request_id":p.request_id,"stage":"channel","channel":channel,"retry":"identical request after channel exists/replicates and orchestrator joins"}),
        );
    }
    fault("channel")?;
    validate_claim(
        &api.get(&path, &[])?,
        &binding,
        &p.expected.planning_task_id,
    )?;
    // A delayed retry skips missed slots rather than firing immediately.
    let mut activation_guard = None;
    let mut receipt = json!({"request_id":p.request_id,"identity":observed,"schedule":p.schedule,"next_fire_at":null,
        "native_cron":p.native_cron,"planning_binding":binding,"initial_fire":false,"spawned":false});
    let finalized = match task::try_modify(&p.task_id, |t| {
        // Never WAIT for DaemonState while holding the task-file lock: other
        // paths may hold DaemonState first. A busy lock aborts for an identical
        // retry. Once acquired, retain it through the durable activation write
        // so the registry identity cannot change between the final check and commit.
        let s = match state.try_lock() {
            Ok(s) => s,
            Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(conflict(
                    "session_state_busy: retry identical adoption request",
                ))
            }
        };
        if s.draining || !s.config.scheduler.enabled {
            return Err(conflict(
                "local scheduler is disabled or daemon is draining",
            ));
        }
        observe(&s, &p, &daemon_id)?;
        activation_guard = Some(s);
        if t.adoption_pending.as_deref() != Some(&p.request_id) {
            return Err(conflict("adoption intent changed"));
        }
        let next = next_slot(p.first_fire_at, every, task::now_unix())?;
        receipt["next_fire_at"] = json!(next);
        t.next_fire_at = next;
        t.enabled = true;
        t.paused = false;
        t.adoption_pending = None;
        t.adoption_receipt = Some(receipt.clone());
        Ok(())
    }) {
        task::TryModifyOutcome::Ok(t) => t,
        task::TryModifyOutcome::Aborted(e) => return Err(e),
        task::TryModifyOutcome::Persist(e) => return Err(internal(e)),
    };
    drop(activation_guard);
    fault("committed")?;
    intent.receipt = Some(receipt.clone());
    durable_json(&intent_path(&p.request_id), &intent)?;
    Ok(
        json!({"adopted":true,"replayed":false,"receipt":receipt,"current":readback(&finalized),"channel":channel}),
    )
}
fn next_slot(first: u64, every: u64, now: u64) -> Result<u64> {
    if first > now {
        return Ok(first);
    }
    first
        .checked_add(
            ((now - first) / every)
                .checked_add(1)
                .and_then(|n| n.checked_mul(every))
                .ok_or_else(|| invalid("schedule overflow"))?,
        )
        .ok_or_else(|| invalid("schedule overflow"))
}
#[cfg(not(test))]
fn fault(_: &str) -> Result<()> {
    Ok(())
}
#[cfg(test)]
thread_local! { static FAULT: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
fn fault(stage: &str) -> Result<()> {
    FAULT.with(|f| {
        if f.borrow().as_deref() == Some(stage) {
            Err(internal(format!("test crash at {stage}")))
        } else {
            Ok(())
        }
    })
}
#[cfg(test)]
pub(crate) fn inject_fault(stage: Option<&str>) {
    FAULT.with(|f| *f.borrow_mut() = stage.map(str::to_owned));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adoption_delayed_retry_skips_missed_slots() {
        assert_eq!(next_slot(1000, 100, 999).unwrap(), 1000);
        assert_eq!(next_slot(1000, 100, 1000).unwrap(), 1100);
        assert_eq!(next_slot(1000, 100, 1250).unwrap(), 1300);
        assert!(next_slot(u64::MAX - 10, 100, u64::MAX).is_err());
    }
}
