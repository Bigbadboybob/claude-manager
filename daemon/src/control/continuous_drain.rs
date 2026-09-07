//! Session-scoped receipt and reconciliation for an operator's drain request.
use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::continuous::{completion, task};
use crate::state::DaemonState;

use super::methods::{capture_drain_sessions, MethodResult};
use super::protocol::{Caller, ErrorCode};

pub fn notice(task: &task::ContinuousTask) -> Option<String> {
    let drain = task.drain.as_ref()?;
    if !task.paused || drain.acknowledgment.is_some() {
        return None;
    }
    Some(format!(
        "[cm-continuous-stop {}] Stop after the current period or already-claimed batch. Finish its workers and receive their final monitors; do not start a new period or claim another batch. Acknowledge receipt with acknowledge_continuous_drain(request_id=\"{}\"). After reconciling the batch, artifacts and all background work, call checkpoint_continuous_drain for this request with your checkpoint notes, then report_done and end your turn. If these new drain tools are unavailable in this legacy session, use list_monitors to settle all current monitors, write HANDOVER_CODEX.md in your existing memory directory with this stop request ID, per-item outcomes, worker/artifact references and an explicit background-work inventory, then use the existing report_done tool and end your turn. The operator will verify that legacy handover and record the bound reconciliation. Receipt alone does not mean the work is finished. The task stays paused until the operator resumes it.",
        drain.request_id, drain.request_id,
    ))
}

static PENDING_NOTICES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

struct PendingNotice(String);
impl Drop for PendingNotice {
    fn drop(&mut self) {
        PENDING_NOTICES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

pub(super) fn notice_ready(state: &Arc<Mutex<DaemonState>>, uid: &str) -> bool {
    let (engine, idle, path, after) = {
        let state = state.lock().unwrap_or_else(|p| p.into_inner());
        if state.draining || crate::writer_gate::pause_requested() {
            return false;
        }
        let Some(session) = state.sessions.get(uid) else {
            return false;
        };
        if session.last_exit.kernel_set() {
            return false;
        }
        let last_input = *session
            .last_input_at
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let after = last_input
            .map(|at| super::methods::now_unix_f64() - at.elapsed().as_secs_f64())
            .unwrap_or(0.0);
        (
            session.session_type.clone(),
            session.semantic_idle(),
            session.transcript_path.clone(),
            after,
        )
    };
    match engine.as_str() {
        "claude-code" => idle == Some(true),
        "codex" => path.is_some_and(|p| completion::codex_turn_finished_after(&p, after)),
        _ => false,
    }
}

pub(crate) fn queue_notices(state: &Arc<Mutex<DaemonState>>, tasks: &[task::ContinuousTask]) {
    for task in tasks {
        let Some(drain) = task.drain.as_ref() else {
            continue;
        };
        let Some(uid) = drain.session_uid.as_deref() else {
            continue;
        };
        if notice(task).is_none()
            || !notice_ready(state, uid)
            || drain
                .notice_submitted_at
                .is_some_and(|at| super::methods::now_unix_f64() - at < 300.0)
        {
            continue;
        }
        let key = drain.request_id.clone();
        if !PENDING_NOTICES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone())
        {
            continue;
        }
        let pending = PendingNotice(key);
        let state = Arc::clone(state);
        let task = task.clone();
        let _ = std::thread::Builder::new()
            .name("cm-continuous-stop-notice".into())
            .spawn(move || {
                let _pending = pending;
                super::methods::deliver_drain_notice(&state, &task);
            });
    }
}

pub(super) struct NoticeBinding<'a> {
    pub state: &'a Arc<Mutex<DaemonState>>,
    pub task_id: &'a str,
    pub request_id: &'a str,
    pub session_uid: &'a str,
}

impl NoticeBinding<'_> {
    /// Held through body+Enter, so resume cannot cross the final validation
    /// and allow a queued old stop instruction to land afterward.
    pub fn acquire(&self) -> Option<task::LifecycleGuard> {
        let lock = task::lock_lifecycle(self.task_id).ok()?;
        let task = task::load_one(self.task_id)?;
        let drain = task.drain.as_ref()?;
        if !task.paused
            || drain.request_id != self.request_id
            || drain.session_uid.as_deref() != Some(self.session_uid)
            || drain.acknowledgment.is_some()
            || task.in_flight.is_some()
            || !task.last_run.as_ref().is_some_and(|run| {
                Some(run.seq) == drain.run_seq
                    && Some(&run.fire_token) == drain.fire_token.as_ref()
                    && run.session_uid.as_deref() == Some(self.session_uid)
            })
            || !notice_ready(self.state, self.session_uid)
        {
            return None;
        }
        Some(lock)
    }
}

#[derive(Deserialize)]
struct ReceiptParams {
    request_id: String,
}

#[derive(Deserialize)]
struct CheckpointParams {
    request_id: String,
    background_work_complete: bool,
    work_reconciled: bool,
    notes: String,
}

pub fn handle(
    state: &Arc<Mutex<DaemonState>>,
    caller: &Caller,
    method: &str,
    params: &Value,
) -> MethodResult {
    let uid = match caller {
        Caller::Session(session) => &session.session_uid,
        _ => {
            return Err((
                ErrorCode::Unauthorized,
                "Drain receipt/checkpoint is session-scoped; operators read continuous.list."
                    .into(),
            ))
        }
    };
    let task_id = {
        let state = state.lock().unwrap_or_else(|p| p.into_inner());
        let session = state
            .sessions
            .get(uid)
            .ok_or((ErrorCode::NotFound, "Caller session is not live.".into()))?;
        session.continuous_task_id.clone()
    };
    let Some(task_id) = task_id else {
        return if method == "continuous.context" {
            Ok(json!({"task_id": null, "drain": null}))
        } else {
            Err((
                ErrorCode::Unauthorized,
                "Caller is not a continuous orchestrator.".into(),
            ))
        };
    };
    if method == "continuous.context" {
        let task = task::load_one(&task_id)
            .ok_or((ErrorCode::NotFound, "Continuous task is missing.".into()))?;
        return Ok(
            json!({"task_id": task_id, "paused": task.paused, "drain": task.drain, "notice": notice(&task)}),
        );
    }
    let receipt: ReceiptParams = serde_json::from_value(params.clone()).map_err(|e| {
        (
            ErrorCode::InvalidParams,
            format!("drain receipt params: {e}"),
        )
    })?;
    let checkpoint: Option<CheckpointParams> = if method == "continuous.checkpoint_drain" {
        let p: CheckpointParams = serde_json::from_value(params.clone()).map_err(|e| {
            (
                ErrorCode::InvalidParams,
                format!("drain checkpoint params: {e}"),
            )
        })?;
        if !p.background_work_complete
            || !p.work_reconciled
            || p.notes.trim().is_empty()
            || p.notes.len() > 8192
        {
            return Err((ErrorCode::InvalidParams, "Checkpoint requires reconciled work, no remaining background work, and 1–8192 bytes of notes.".into()));
        }
        Some(p)
    } else {
        None
    };
    let _lifecycle = task::lock_lifecycle(&task_id)
        .map_err(|e| (ErrorCode::Internal, format!("drain lifecycle lock: {e}")))?;
    let current = task::load_one(&task_id)
        .ok_or((ErrorCode::NotFound, "Continuous task is missing.".into()))?;
    let valid_binding = |task: &task::ContinuousTask| {
        task.paused
            && task.drain.as_ref().is_some_and(|drain| {
                drain.request_id == receipt.request_id
                    && drain.session_uid.as_deref() == Some(uid)
                    && task.last_run.as_ref().is_some_and(|run| {
                        Some(run.seq) == drain.run_seq
                            && Some(&run.fire_token) == drain.fire_token.as_ref()
                            && run.session_uid.as_deref() == Some(uid)
                    })
            })
    };
    if !valid_binding(&current) {
        return Err((
            ErrorCode::Conflict,
            "Stop request or admitted run/session has changed; read get_continuous_context again."
                .into(),
        ));
    }
    let proof = if let Some(p) = checkpoint {
        debug_assert_eq!(p.request_id, receipt.request_id);
        if current.in_flight.is_some() || current.drain.as_ref().unwrap().acknowledgment.is_none() {
            return Err((
                ErrorCode::Conflict,
                "Wait for admitted delivery and acknowledge the stop request before checkpointing."
                    .into(),
            ));
        }
        let sessions = capture_drain_sessions(state, &current);
        if sessions.iter().any(|s| {
            !s.orchestrator && (s.failed || !s.reported_done || (!s.exited && !s.final_turn_ended))
        }) {
            return Err((
                ErrorCode::Conflict,
                "Workers still need completion, their final turn, or reconciliation.".into(),
            ));
        }
        let monitors = completion::load_session_monitors(Some(uid), &sessions);
        if !monitors.outstanding.is_empty() || monitors.fingerprint.is_none() {
            return Err((ErrorCode::Conflict, "Monitor evidence is missing or deliveries remain unsettled; inspect list_monitors, including retained producers.".into()));
        }
        Some(completion::Checkpoint {
            recorded_at: super::methods::now_unix_f64(),
            monitor_fingerprint: monitors.fingerprint.unwrap(),
            worker_fingerprint: completion::worker_fingerprint(&sessions),
            notes: p.notes,
        })
    } else {
        None
    };
    let result = task::try_modify::<_, ()>(&task_id, |task| {
        if !valid_binding(task) {
            return Err(());
        }
        let drain = task.drain.as_mut().unwrap();
        if method == "continuous.ack_drain" {
            drain
                .acknowledgment
                .get_or_insert_with(|| completion::Acknowledgment {
                    received_at: super::methods::now_unix_f64(),
                    session_uid: uid.clone(),
                    run_seq: drain.run_seq,
                    fire_token: drain.fire_token.clone(),
                });
        } else {
            drain.checkpoint = proof;
        }
        Ok(())
    });
    match result {
        task::TryModifyOutcome::Ok(task) => {
            Ok(json!({"ok": true, "task_id": task_id, "drain": task.drain}))
        }
        task::TryModifyOutcome::Aborted(()) => Err((
            ErrorCode::Conflict,
            "Stop request changed before commit.".into(),
        )),
        task::TryModifyOutcome::Persist(e) => Err((
            ErrorCode::Internal,
            format!("persist drain receipt/checkpoint: {e}"),
        )),
    }
}
