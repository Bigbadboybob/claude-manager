//! Guarded operator cutover and explicit reconciliation of legacy evidence.
use super::{completion, drain, retirement, task};
use crate::control::{methods, protocol::ErrorCode};
use crate::state::DaemonState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

type Result<T> = std::result::Result<T, (ErrorCode, String)>;
fn conflict(message: impl Into<String>) -> (ErrorCode, String) {
    (ErrorCode::Conflict, message.into())
}
fn internal(e: impl std::fmt::Display) -> (ErrorCode, String) {
    (ErrorCode::Internal, e.to_string())
}
pub fn digest(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("JSON value"))
    )
}
pub fn state_hash(t: &task::ContinuousTask) -> String {
    digest(&serde_json::to_value(t).expect("task serialization"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Completed,
    Unfinished,
    Ambiguous,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ItemOutcome {
    pub status: ItemStatus,
    pub evidence: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reconciliation {
    pub request_id: String,
    pub evidence_hash: String,
    pub recorded_at: u64,
    pub handover: Artifact,
    pub notes: String,
    pub items: BTreeMap<String, ItemOutcome>,
    pub batch_hash: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Retirement {
    pub request_id: String,
    pub source_engine: task::Engine,
    pub run_count: u32,
    pub source_session_uid: Option<String>,
    pub session_uids: Vec<String>,
    pub proof: String,
    pub recorded_at: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EngineChange {
    pub operation_id: String,
    pub source: task::Engine,
    pub target: task::Engine,
    pub source_state_hash: String,
    pub old_session_uid: Option<String>,
    pub run_count: u32,
    pub committed_at: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    task_id: String,
    expected_state_hash: Option<String>,
    expected_evidence_hash: Option<String>,
    operation_id: Option<String>,
    target_engine: Option<task::Engine>,
    #[serde(default)]
    dry_run: bool,
    handover: Option<Artifact>,
    notes: Option<String>,
    #[serde(default)]
    background_work_complete: bool,
    #[serde(default)]
    deliveries_reconciled: bool,
    #[serde(default)]
    items: BTreeMap<String, ItemOutcome>,
}

fn file_hash(path: &Path, limit: u64) -> Result<String> {
    let file = std::fs::File::open(path).map_err(internal)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(internal)?;
    if bytes.len() as u64 > limit {
        return Err(conflict(format!(
            "Evidence exceeds size limit: {}",
            path.display()
        )));
    }
    Ok(format!("{:x}", Sha256::digest(bytes)))
}
fn artifact_valid(t: &task::ContinuousTask, artifact: &Artifact) -> Result<()> {
    let path = std::fs::canonicalize(&artifact.path).map_err(internal)?;
    let worktree = std::fs::canonicalize(&t.worktree_path).map_err(internal)?;
    let backup = crate::path::dot_cm_dir().join("migrations");
    if !path.starts_with(&worktree)
        && !std::fs::canonicalize(backup)
            .ok()
            .is_some_and(|b| path.starts_with(b))
    {
        return Err(conflict(
            "Handover must be in this worktree or the private migration backup.",
        ));
    }
    if file_hash(&path, 4 * 1024 * 1024)? != artifact.sha256 {
        return Err(conflict("Handover hash changed."));
    }
    if std::fs::metadata(path).map_err(internal)?.len() == 0 {
        return Err(conflict("Handover is empty."));
    }
    Ok(())
}

pub fn batch(t: &task::ContinuousTask) -> Result<Vec<super::queue::QueueItem>> {
    let task::Schedule::Consumer { queue, .. } = &t.schedule else {
        return Ok(Vec::new());
    };
    let Some(run) = &t.last_run else {
        return Ok(Vec::new());
    };
    let path = Path::new(&t.worktree_path)
        .join(".queue")
        .join(format!("batch-{}.json", run.seq));
    // Idle/maintenance periods have no admitted batch.
    if !path.exists() && run.status == task::RunStatus::Idle {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(&path).map_err(internal)?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(internal)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(conflict("Batch exceeds evidence size limit."));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(internal)?;
    if value["task_id"] != t.task_id || value["queue"] != *queue || value["seq"] != run.seq {
        return Err(conflict("Staged batch identity does not match this run."));
    }
    let items: Vec<super::queue::QueueItem> =
        serde_json::from_value(value["items"].clone()).map_err(internal)?;
    if items.iter().map(|i| &i.id).collect::<BTreeSet<_>>().len() != items.len() {
        return Err(conflict("Staged batch has duplicate IDs."));
    }
    Ok(items)
}

struct Evidence {
    hash: String,
    value: Value,
    blockers: Vec<String>,
    sessions: Vec<drain::SessionObservation>,
}
fn evidence(state: &Arc<Mutex<DaemonState>>, t: &task::ContinuousTask) -> Evidence {
    // Held runs need the same descendant walk even outside an operator drain.
    let mut observed = t.clone();
    if observed.drain.is_none() {
        drain::request(&mut observed, task::now_unix());
    }
    let sessions = methods::capture_drain_sessions(state, &observed);
    let mut blockers = Vec::new();
    if t.last_run.as_ref().is_some_and(|r| {
        matches!(
            r.status,
            task::RunStatus::Running | task::RunStatus::Pending
        )
    }) && t.account_blocked.is_none()
    {
        blockers.push("The admitted run is still active.".into());
    }
    if t.in_flight.is_some() {
        blockers.push("An admitted fire is still in flight.".into());
    }
    if !t.paused && t.account_blocked.is_none() && t.recovery_hold.is_none() {
        blockers.push("Close admission with continuous.drain before reconciliation.".into());
    }
    let mut rows = Vec::new();
    for s in &sessions {
        let (engine, transcript, input, semantic_idle) = {
            let state = state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(live) = state.sessions.get(&s.session_uid) {
                (
                    Some(live.session_type.clone()),
                    live.transcript_path.clone(),
                    format!(
                        "{:?}",
                        *live.last_input_at.lock().unwrap_or_else(|p| p.into_inner())
                    ),
                    live.semantic_idle(),
                )
            } else {
                (
                    None,
                    state
                        .exited_tombstone(&s.session_uid)
                        .and_then(|x| x.transcript_path.clone()),
                    String::new(),
                    None,
                )
            }
        };
        let account_terminal = s.orchestrator
            && (t.account_blocked.is_some() || t.recovery_hold.is_some())
            && engine.as_deref() == Some("codex")
            && transcript
                .as_deref()
                .and_then(|p| {
                    super::codex_probe::probe(Path::new(p), s.last_input_at.unwrap_or(0.0))
                })
                .is_some_and(|p| p.shape == super::probe::TailShape::TurnComplete);
        if !s.exited && !(s.reported_done && s.final_turn_ended) && !account_terminal {
            blockers.push(format!(
                "{} still needs its completion report and final turn.",
                s.session_uid
            ));
        }
        let journal = crate::path::dot_cm_dir()
            .join("monitor-state")
            .join(format!("{}.json", s.session_uid));
        let journal_hash = match std::fs::File::open(&journal).and_then(|f| {
            let mut bytes = Vec::new();
            f.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 4 * 1024 * 1024 {
                return Err(std::io::Error::other("journal exceeds limit"));
            }
            Ok(bytes)
        }) {
            Ok(bytes) => {
                match serde_json::from_slice::<Value>(&bytes) {
                    Ok(doc)
                        if doc["session_uid"] == s.session_uid && doc["schema_version"] == 1 =>
                    {
                        if let Some(producers) = doc["producers"].as_object() {
                            for producer in producers.values() {
                                if let Some(records) = producer["records"].as_object() {
                                    for (id, record) in records {
                                        if !matches!(
                                            record["state"].as_str(),
                                            Some(
                                                "delivered"
                                                    | "cancelled"
                                                    | "replaced"
                                                    | "undelivered"
                                                    | "interrupted"
                                            )
                                        ) {
                                            blockers.push(format!(
                                                "{} monitor {id} is still active or unknown.",
                                                s.session_uid
                                            ));
                                        }
                                    }
                                } else {
                                    blockers.push(format!(
                                        "{} has malformed monitor records.",
                                        s.session_uid
                                    ));
                                }
                            }
                        } else {
                            blockers.push(format!(
                                "{} has malformed monitor producers.",
                                s.session_uid
                            ));
                        }
                    }
                    _ => {
                        blockers.push(format!("{} has an invalid monitor journal.", s.session_uid))
                    }
                }
                Some(format!("{:x}", Sha256::digest(bytes)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None, // explicit legacy evidence required below
            Err(e) => {
                blockers.push(format!(
                    "Cannot inspect {} monitor journal: {e}",
                    s.session_uid
                ));
                None
            }
        };
        let inbox = crate::path::dot_cm_dir().join("inbox").join(&s.session_uid);
        match std::fs::read_dir(&inbox) {
            Ok(entries) => {
                if entries.count() > 0 {
                    blockers.push(format!("{} has pending inbox deliveries.", s.session_uid));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => blockers.push(format!("Cannot inspect inbox: {e}")),
        }
        let transcript_stamp = transcript.as_deref().map(|p| {
            match std::fs::metadata(p) {
                Ok(m) => json!({"path": p, "bytes": m.len(), "modified": m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos().to_string())}),
                Err(_) => json!({"path": p, "unreadable": true}),
            }
        });
        rows.push(json!({"uid":s.session_uid,"orchestrator":s.orchestrator,"exited":s.exited,"reported_at":s.reported_at,
            "reported":s.reported_done,"final_turn":s.final_turn_ended,"failed":s.failed,"input":input,
            "semantic_idle":semantic_idle,"transcript":transcript_stamp,"monitor_hash":journal_hash}));
    }
    let items = match batch(t) {
        Ok(items) => json!(items),
        Err(e) => {
            blockers.push(e.1);
            Value::Null
        }
    };
    let mut binding = serde_json::to_value(t).unwrap();
    // Recording a reconciliation must not invalidate its own evidence.
    for key in ["reconciliation", "retirement", "engine_changes", "recovery"] {
        binding.as_object_mut().unwrap().remove(key);
    }
    let value = json!({"task":binding,"sessions":rows,"batch":items});
    Evidence {
        hash: digest(&value),
        value,
        blockers,
        sessions,
    }
}

fn reconciled(t: &task::ContinuousTask, e: &Evidence) -> bool {
    t.reconciliation
        .as_ref()
        .is_some_and(|r| r.evidence_hash == e.hash && artifact_valid(t, &r.handover).is_ok())
        && e.blockers.is_empty()
}

pub fn reconciled_drain_status(
    state: &Arc<Mutex<DaemonState>>,
    t: &task::ContinuousTask,
) -> Option<drain::DrainStatus> {
    let request = t.drain.as_ref()?;
    if t.reconciliation.is_none()
        || t.account_blocked.is_some()
        || t.recovery_hold.is_some()
        || t.recovery.is_some()
    {
        return None;
    }
    let e = evidence(state, t);
    if !reconciled(t, &e)
        || t.reconciliation
            .as_ref()
            .is_some_and(|r| r.items.values().any(|i| i.status != ItemStatus::Completed))
    {
        return None;
    }
    Some(drain::DrainStatus {
        request_id: request.request_id.clone(),
        state: drain::DrainState::Drained,
        admission_closed: t.paused,
        outstanding: Vec::new(),
    })
}

pub fn handle(
    state: &Arc<Mutex<DaemonState>>,
    method: &str,
    params: &Value,
) -> methods::MethodResult {
    let p: Params = serde_json::from_value(params.clone())
        .map_err(|e| (ErrorCode::InvalidParams, format!("{method}: {e}")))?;
    task::validate_task_id(&p.task_id).map_err(|e| (ErrorCode::InvalidParams, e.to_string()))?;
    if task::load_one(&p.task_id).is_none() {
        return Err((ErrorCode::NotFound, "Continuous task is missing.".into()));
    }
    let _lifecycle = task::lock_lifecycle(&p.task_id).map_err(internal)?;
    let t = task::load_one(&p.task_id).ok_or_else(|| conflict("Task disappeared."))?;
    if method == "continuous.migrate_engine" {
        if let Some(previous) = t
            .engine_changes
            .iter()
            .find(|c| Some(&c.operation_id) == p.operation_id.as_ref())
        {
            if Some(previous.target) != p.target_engine
                || Some(&previous.source_state_hash) != p.expected_state_hash.as_ref()
            {
                return Err(conflict(
                    "Operation ID already identifies a different migration.",
                ));
            }
            return Ok(
                json!({"task_id":t.task_id,"committed":true,"already_committed":true,"change":previous,"current_engine":t.engine}),
            );
        }
    }
    let e = evidence(state, &t);
    if method == "continuous.migration_preview" {
        return Ok(
            json!({"task_id":t.task_id,"state_hash":state_hash(&t),"evidence_hash":e.hash,"evidence":e.value,"blockers":e.blockers,"reconciled":reconciled(&t,&e),"retirement":t.retirement}),
        );
    }
    if p.expected_state_hash.as_deref() != Some(&state_hash(&t)) {
        return Err(conflict(
            "Task state changed; refresh continuous.migration_preview.",
        ));
    }
    match method {
        "continuous.reconcile" => {
            if p.expected_evidence_hash.as_deref() != Some(&e.hash) {
                return Err(conflict(
                    "Execution, delivery or batch evidence changed; refresh the preview.",
                ));
            }
            if !e.blockers.is_empty() {
                return Err(conflict(e.blockers.join(" ")));
            }
            if !p.background_work_complete || !p.deliveries_reconciled {
                return Err(conflict(
                    "Explicit background-work and delivery reconciliation is required.",
                ));
            }
            let handover = p
                .handover
                .ok_or_else(|| conflict("A verified handover artifact is required."))?;
            artifact_valid(&t, &handover)?;
            let notes = p.notes.filter(|s| !s.trim().is_empty() && s.len() <= 8192).ok_or_else(|| conflict("Supply 1–8192 bytes describing the inspected transcripts, legacy monitors, artifacts and background work."))?;
            let expected_ids: BTreeSet<_> = batch(&t)?.into_iter().map(|i| i.id).collect();
            if p.items.keys().cloned().collect::<BTreeSet<_>>() != expected_ids
                || p.items.values().any(|v| {
                    v.status == ItemStatus::Ambiguous
                        || v.evidence.trim().is_empty()
                        || v.evidence.len() > 8192
                })
            {
                return Err(conflict("Every staged item needs a completed or unfinished outcome with evidence; missing, extra or ambiguous IDs remain held."));
            }
            if p.dry_run {
                return Ok(json!({"valid":true,"committed":false}));
            }
            let record = Reconciliation {
                request_id: t
                    .drain
                    .as_ref()
                    .map(|d| d.request_id.clone())
                    .unwrap_or_else(|| format!("recovery-{}", t.run_count)),
                evidence_hash: e.hash,
                recorded_at: task::now_unix(),
                handover,
                notes,
                items: p.items,
                batch_hash: digest(&json!(batch(&t)?)),
            };
            task::modify(&t.task_id, |t| t.reconciliation = Some(record)).map_err(internal)?;
            Ok(json!({"task_id":t.task_id,"reconciled":true,"paused":t.paused}))
        }
        "continuous.retire" => retire(state, &t, &e, p.dry_run),
        "continuous.migrate_engine" => migrate(state, &t, &p),
        _ => Err((ErrorCode::UnknownMethod, method.into())),
    }
}

fn retire(
    state: &Arc<Mutex<DaemonState>>,
    t: &task::ContinuousTask,
    initial: &Evidence,
    dry_run: bool,
) -> methods::MethodResult {
    if !t.paused
        || t.drain.is_none()
        || t.in_flight.is_some()
        || t.account_blocked.is_some()
        || t.recovery_hold.is_some()
        || t.recovery.is_some()
        || t.last_run.as_ref().is_some_and(|r| {
            matches!(
                r.status,
                task::RunStatus::Running | task::RunStatus::Pending
            )
        })
    {
        return Err(conflict("Retirement requires a completed paused drain with no active fire, run, or recovery hold."));
    }
    let mut uids: Vec<String> = initial
        .sessions
        .iter()
        .map(|s| s.session_uid.clone())
        .collect();
    uids.extend(t.drain.as_ref().and_then(|d| d.session_uid.clone()));
    if let Some(prior) = &t.retirement {
        uids.extend(prior.session_uids.iter().cloned());
    }
    uids.sort();
    uids.dedup();
    let gates: Vec<_> = uids.iter().map(|u| retirement::gate(u)).collect();
    let _permits: Vec<_> = gates
        .iter()
        .map(|g| g.write().unwrap_or_else(|p| p.into_inner()))
        .collect();
    let current = task::load_one(&t.task_id).ok_or_else(|| conflict("Task disappeared."))?;
    if state_hash(&current) != state_hash(t) {
        return Err(conflict("Task changed while acquiring retirement guards."));
    }
    let e = evidence(state, t);
    let record = if let Some(prior) = &t.retirement {
        if prior.source_engine != t.engine
            || prior.run_count != t.run_count
            || prior.source_session_uid != t.current_session_uid
        {
            return Err(conflict(
                "Retirement belongs to a different task/run/session.",
            ));
        }
        for uid in &prior.session_uids {
            if retirement::ensure_open(uid).is_ok() {
                return Err(conflict("Retirement intent is incomplete."));
            }
        }
        prior.clone()
    } else {
        if e.hash != initial.hash {
            return Err(conflict(
                "Execution changed while acquiring retirement guards; refresh the preview.",
            ));
        }
        let monitors = completion::load_session_monitors(
            t.drain.as_ref().and_then(|d| d.session_uid.as_deref()),
            &e.sessions,
        );
        let naturally_drained = drain::status_with_evidence(t, &e.sessions, &monitors)
            .is_some_and(|d| d.state == drain::DrainState::Drained);
        if !naturally_drained && !reconciled(t, &e) {
            return Err(conflict(format!(
                "Verified drain or legacy reconciliation is required. {}",
                e.blockers.join(" ")
            )));
        }
        Retirement {
            request_id: t.drain.as_ref().unwrap().request_id.clone(),
            source_engine: t.engine,
            run_count: t.run_count,
            source_session_uid: t.current_session_uid.clone(),
            session_uids: uids.clone(),
            proof: e.hash,
            recorded_at: task::now_unix(),
        }
    };
    if dry_run {
        return Ok(json!({"valid":true,"committed":false,"retirement":record}));
    }
    for uid in &record.session_uids {
        let path = retirement::path(uid);
        if path.exists() {
            let previous: Value = serde_json::from_slice(&std::fs::read(&path).map_err(internal)?)
                .map_err(internal)?;
            if previous["task_id"] != t.task_id
                || previous["retirement"]["request_id"] != record.request_id
            {
                return Err(conflict("UID has a different retirement intent."));
            }
        } else {
            retirement::write_json(
                &path,
                &json!({"task_id":t.task_id,"session_uid":uid,"retirement":record}),
            )
            .map_err(internal)?;
        }
    }
    task::modify(&t.task_id, |t| t.retirement = Some(record.clone())).map_err(internal)?;
    // Intents and all input barriers are durable before signaling. No global
    // state lock is held while waiting; callers poll until the reaper finishes.
    for uid in &record.session_uids {
        if state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .contains_key(uid)
        {
            methods::kill_session(state, &json!({"session_uid":uid}), None)?;
        }
    }
    Ok(json!({"task_id":t.task_id,"retirement":record,"exit_pending":true,"paused":true}))
}

fn migrate(
    state: &Arc<Mutex<DaemonState>>,
    t: &task::ContinuousTask,
    p: &Params,
) -> methods::MethodResult {
    let target = p
        .target_engine
        .ok_or_else(|| conflict("target_engine is required."))?;
    if target == t.engine || target == task::Engine::Bash || t.engine == task::Engine::Bash {
        return Err(conflict("Migration switches between Claude and Codex."));
    }
    let operation_id = p
        .operation_id
        .as_ref()
        .filter(|s| uuid::Uuid::parse_str(s).is_ok())
        .ok_or_else(|| conflict("Supply a stable UUID operation_id for retries."))?;
    if !t.paused
        || t.drain.is_none()
        || t.in_flight.is_some()
        || t.account_blocked.is_some()
        || t.recovery_hold.is_some()
        || t.last_run.as_ref().is_some_and(|r| {
            matches!(
                r.status,
                task::RunStatus::Running | task::RunStatus::Pending
            )
        })
    {
        return Err(conflict("Migration requires a paused, completed drain with all run/account/queue holds reconciled."));
    }
    let record = t
        .retirement
        .as_ref()
        .ok_or_else(|| conflict("Record and finish continuous.retire first."))?;
    let unused_target = t.current_session_uid.is_none()
        && t.engine_changes
            .last()
            .is_some_and(|c| c.target == t.engine && c.run_count == t.run_count);
    if !unused_target
        && (record.source_engine != t.engine
            || record.run_count != t.run_count
            || record.source_session_uid != t.current_session_uid)
    {
        return Err(conflict("Retirement identity changed."));
    }
    {
        let state = state.lock().unwrap_or_else(|p| p.into_inner());
        if state.sessions.values().any(|s| {
            s.continuous_task_id.as_deref() == Some(&t.task_id)
                || record.session_uids.contains(&s.uid)
        }) {
            return Err(conflict(
                "Old sessions are still in the live registry; wait for their exit.",
            ));
        }
    }
    if let Some(holder) = crate::holder_mode::global() {
        let status = holder.status().map_err(internal)?;
        let registered = state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .len();
        if status.sessions != registered || status.pending_exit_events != 0 {
            return Err(conflict(
                "Holder/brain registry or exit acknowledgments have not settled.",
            ));
        }
    }
    for uid in &record.session_uids {
        if retirement::ensure_open(uid).is_ok() {
            return Err(conflict("Old UID has no durable retirement marker."));
        }
    }
    if !Path::new(&t.worktree_path).is_dir() || t.default_prompt.trim().is_empty() {
        return Err(conflict("Worktree and nonempty prompt are required."));
    }
    if t.run_count > 0 {
        let reconciliation = t.reconciliation.as_ref().ok_or_else(|| conflict("A preserved handover and per-item reconciliation are required before engine switching."))?;
        artifact_valid(t, &reconciliation.handover)?;
        if reconciliation.batch_hash != digest(&json!(batch(t)?)) {
            return Err(conflict("Staged batch changed since reconciliation."));
        }
        if reconciliation
            .items
            .values()
            .any(|i| i.status != ItemStatus::Completed)
        {
            return Err(conflict(
                "Unfinished batch items need recovery before migration.",
            ));
        }
    }
    let change = EngineChange {
        operation_id: operation_id.clone(),
        source: t.engine,
        target,
        source_state_hash: state_hash(t),
        old_session_uid: t.current_session_uid.clone(),
        run_count: t.run_count,
        committed_at: task::now_unix(),
    };
    if p.dry_run {
        return Ok(json!({"valid":true,"committed":false,"change":change}));
    }
    let updated = match task::try_modify(&t.task_id, |current| {
        if state_hash(current) != state_hash(t) {
            return Err(());
        }
        current.engine = target;
        current.current_session_uid = None;
        current.admission_revision = current.admission_revision.saturating_add(1);
        current.engine_changes.push(change.clone());
        Ok(())
    }) {
        task::TryModifyOutcome::Ok(t) => t,
        task::TryModifyOutcome::Aborted(()) => {
            return Err(conflict("Task changed before migration commit."))
        }
        task::TryModifyOutcome::Persist(e) => return Err(internal(e)),
    };
    let audit = super::runlog::ContinuousRunLog::append(&super::runlog::RunLogLine {
        seq: t.run_count as u64,
        ts: change.committed_at as f64,
        task_id: t.task_id.clone(),
        event: "engine_changed".into(),
        fire_token: t.last_run.as_ref().map(|r| r.fire_token.clone()),
        session_uid: t.current_session_uid.clone(),
        run_mode: Some(
            match t.run_mode {
                task::RunMode::Fresh => "fresh",
                task::RunMode::Persistent => "persistent",
            }
            .into(),
        ),
        trigger_source: Some("operator".into()),
        status: None,
        detail: Some(json!(change)),
    });
    Ok(
        json!({"task_id":t.task_id,"committed":true,"change":change,"paused":updated.paused,"audit_pending":audit.is_err()}),
    )
}

/// A crash-resumable recovery transaction. Queue progress is persisted after
/// each accepted/deduplicated item; an uncertain HTTP response can safely retry
/// the same deduplication key. Completed items are never replay candidates.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryProgress {
    pub run_seq: u64,
    pub fire_token: String,
    pub session_uid: String,
    pub binding: String,
    pub reconciliation: Reconciliation,
    pub retired_uids: Vec<String>,
    pub replayed: BTreeSet<String>,
}
fn recovery_binding(t: &task::ContinuousTask) -> String {
    digest(
        &json!({"engine":t.engine,"run":t.last_run,"current":t.current_session_uid,
        "count":t.run_count,"account":t.account_blocked,"hold":t.recovery_hold,
        "worktree":t.worktree_path,"schedule":t.schedule}),
    )
}

pub fn recover_codex(state: &Arc<Mutex<DaemonState>>, task_id: &str, now: u64) -> Result<bool> {
    let _lifecycle = task::lock_lifecycle(task_id).map_err(internal)?;
    let Some(t) = task::load_one(task_id) else {
        return Ok(false);
    };
    if t.engine != task::Engine::Codex
        || !t.enabled
        || t.paused
        || t.drain.is_some()
        || t.in_flight.is_some()
        || (t.account_blocked.is_none() && t.recovery_hold.is_none() && t.recovery.is_none())
    {
        return Ok(false);
    }
    let root = crate::path::dot_cm_dir();
    let detected = t
        .account_blocked
        .as_ref()
        .map(|b| b.detected_at)
        .or_else(|| t.recovery_hold.as_ref().map(|h| h.detected_at))
        .ok_or_else(|| conflict("Recovery lost its hold identity."))?;
    if !super::codex_account::ok_after(
        &root.join("codex-probe-config.json"),
        &root.join("codex-probe-state.json"),
        detected,
        now as f64,
    ) {
        return Ok(false);
    }
    let Some(reconciliation) = t.reconciliation.as_ref() else {
        return Ok(false);
    };
    artifact_valid(&t, &reconciliation.handover)?;
    let items = batch(&t)?;
    if reconciliation.batch_hash != digest(&json!(items)) {
        return Err(conflict("Recovery batch changed since reconciliation."));
    }
    let progress = if let Some(progress) = &t.recovery {
        if progress.binding != recovery_binding(&t)
            || digest(&json!(progress.reconciliation)) != digest(&json!(reconciliation))
        {
            return Err(conflict("Recovery task/run/account identity changed."));
        }
        progress.clone()
    } else {
        let initial = evidence(state, &t);
        if !reconciled(&t, &initial) {
            return Ok(false);
        }
        let mut uids: Vec<_> = initial
            .sessions
            .iter()
            .map(|s| s.session_uid.clone())
            .collect();
        uids.extend(t.current_session_uid.iter().cloned());
        uids.sort();
        uids.dedup();
        let gates: Vec<_> = uids.iter().map(|uid| retirement::gate(uid)).collect();
        let _permits: Vec<_> = gates
            .iter()
            .map(|g| g.write().unwrap_or_else(|p| p.into_inner()))
            .collect();
        let current =
            task::load_one(task_id).ok_or_else(|| conflict("Recovery task disappeared."))?;
        if state_hash(&current) != state_hash(&t) || !reconciled(&t, &evidence(state, &t)) {
            return Ok(false);
        }
        let run = t
            .last_run
            .as_ref()
            .ok_or_else(|| conflict("Recovery run is missing."))?;
        let uid = run
            .session_uid
            .clone()
            .ok_or_else(|| conflict("Recovery run UID is missing."))?;
        let progress = RecoveryProgress {
            run_seq: run.seq,
            fire_token: run.fire_token.clone(),
            session_uid: uid,
            binding: recovery_binding(&t),
            reconciliation: reconciliation.clone(),
            retired_uids: uids,
            replayed: BTreeSet::new(),
        };
        for uid in &progress.retired_uids {
            let path = retirement::path(uid);
            let intent = json!({"task_id":t.task_id,"session_uid":uid,"recovery":{"run_seq":run.seq,"fire_token":run.fire_token}});
            if path.exists() {
                let existing: Value =
                    serde_json::from_slice(&std::fs::read(&path).map_err(internal)?)
                        .map_err(internal)?;
                if existing != intent {
                    return Err(conflict("Session has a different retirement intent."));
                }
            } else {
                retirement::write_json(&path, &intent).map_err(internal)?;
            }
        }
        task::modify(task_id, |t| t.recovery = Some(progress.clone())).map_err(internal)?;
        progress
    };
    for uid in &progress.retired_uids {
        if retirement::ensure_open(uid).is_ok() {
            return Err(conflict("Recovery retirement marker is missing."));
        }
        if state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sessions
            .contains_key(uid)
        {
            // Only sessions sealed after verified final turns can reach here.
            methods::kill_session(state, &json!({"session_uid":uid}), None)?;
        }
    }
    if state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .sessions
        .keys()
        .any(|u| progress.retired_uids.contains(u))
    {
        return Ok(false);
    }
    if let Some(holder) = crate::holder_mode::global() {
        let status = holder.status().map_err(internal)?;
        if status.sessions
            != state
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .sessions
                .len()
            || status.pending_exit_events != 0
        {
            return Ok(false);
        }
    }
    let mut progress = progress;
    if let task::Schedule::Consumer { queue, .. } = &t.schedule {
        let (url, token) = {
            let state = state.lock().unwrap_or_else(|p| p.into_inner());
            (state.config.api_url.clone(), state.config.api_token.clone())
        };
        let client = super::queue::QueueClient::from_overrides(Some(&url), Some(&token))
            .map_err(|e| e.to_method_err())?;
        for item in &items {
            let outcome = progress
                .reconciliation
                .items
                .get(&item.id)
                .ok_or_else(|| conflict("Missing item outcome during recovery."))?;
            if outcome.status == ItemStatus::Completed || progress.replayed.contains(&item.id) {
                continue;
            }
            if outcome.status != ItemStatus::Unfinished {
                return Err(conflict("Ambiguous item remains held."));
            }
            let key = digest(&json!([t.task_id, progress.fire_token, item.id]));
            let response = client
                .recover_item(
                    queue,
                    &item.id,
                    &format!("{}#{}", t.task_id, progress.run_seq),
                    &key,
                )
                .map_err(|e| e.to_method_err())?;
            if response["recovered"] != true || response["id"] != item.id {
                return Err(conflict("Queue did not acknowledge recovery item."));
            }
            progress.replayed.insert(item.id.clone());
            task::modify(task_id, |t| t.recovery = Some(progress.clone())).map_err(internal)?;
        }
    }
    // Save a durable per-item receipt outside the mutable last-run record
    // BEFORE reopening admission; retries after an uncertain write retain it.
    let receipt_path = task::task_dir(task_id)
        .join("recoveries")
        .join(format!("{}.json", progress.run_seq));
    retirement::write_json(
        &receipt_path,
        &json!({"completed_at":now,"progress":progress}),
    )
    .map_err(internal)?;
    match task::try_modify(task_id, |current| {
        if current.paused
            || current.drain.is_some()
            || current.in_flight.is_some()
            || recovery_binding(current) != progress.binding
        {
            return Err(());
        }
        if let Some(run) = current.last_run.as_mut() {
            run.status = task::RunStatus::Failed;
            run.finished_at = Some(now);
        }
        current.current_session_uid = None;
        current.account_blocked = None;
        current.recovery_hold = None;
        current.recovery = None;
        current.reconciliation = None;
        current.consecutive_wedge_closes = 0;
        current.next_fire_at = current.next_fire_at.max(now.saturating_add(5));
        current.admission_revision = current.admission_revision.saturating_add(1);
        Ok(())
    }) {
        task::TryModifyOutcome::Ok(_) => Ok(true),
        task::TryModifyOutcome::Aborted(()) => Err(conflict(
            "Recovery identity changed before release; receipt retained.",
        )),
        task::TryModifyOutcome::Persist(e) => Err(internal(e)),
    }
}
