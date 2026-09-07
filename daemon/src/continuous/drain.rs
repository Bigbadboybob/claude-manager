//! Durable admission barrier and conservative, advisory drain status.
//!
//! A run report alone does not prove that its final turn, descendants and
//! completion deliveries have settled. Read durable MCP producer journals and
//! a request-bound reconciliation checkpoint; absent evidence blocks cutover.

use serde::{Deserialize, Serialize};

use super::task::{ContinuousTask, RunStatus};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DrainRecord {
    pub request_id: String,
    pub requested_at: u64,
    pub fire_token: Option<String>,
    pub run_seq: Option<u64>,
    pub session_uid: Option<String>,
    /// Diagnostic only: never authorizes closing/replaying the held run.
    #[serde(default)]
    pub diagnostic: Option<String>,
    #[serde(default)]
    pub acknowledgment: Option<super::completion::Acknowledgment>,
    #[serde(default)]
    pub checkpoint: Option<super::completion::Checkpoint>,
    #[serde(default)]
    pub notice_submitted_at: Option<f64>,
}

/// Called under the same task lock that admits a fire. Capture the spawn
/// window before last_run: its UID/token belong to the newly admitted work.
pub fn request(task: &mut ContinuousTask, now: u64) {
    task.paused = true;
    if task.drain.is_some() {
        return;
    }
    task.admission_revision = task.admission_revision.saturating_add(1);
    let (fire_token, run_seq, session_uid) = match &task.in_flight {
        Some(fire) => (
            Some(fire.fire_token.clone()),
            // The executor may have committed last_run before its guard clears.
            Some(
                task.last_run
                    .as_ref()
                    .filter(|run| run.fire_token == fire.fire_token)
                    .map_or(task.run_count as u64 + 1, |run| run.seq),
            ),
            Some(fire.session_uid.clone()),
        ),
        None => (
            task.last_run.as_ref().map(|run| run.fire_token.clone()),
            task.last_run.as_ref().map(|run| run.seq),
            task.current_session_uid.clone().or_else(|| {
                task.last_run
                    .as_ref()
                    .and_then(|run| run.session_uid.clone())
            }),
        ),
    };
    task.drain = Some(DrainRecord {
        request_id: uuid::Uuid::new_v4().to_string(),
        requested_at: now,
        fire_token,
        run_seq,
        session_uid,
        diagnostic: None,
        acknowledgment: None,
        checkpoint: None,
        notice_submitted_at: None,
    });
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DrainState {
    Draining,
    Drained,
    Blocked,
}

#[derive(Clone, Debug, Serialize)]
pub struct Obligation {
    pub kind: &'static str,
    pub session_uid: Option<String>,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct DrainStatus {
    pub request_id: String,
    pub state: DrainState,
    pub admission_closed: bool,
    /// This is a readout, never a permit to retire a session. Migration must
    /// revalidate under its own lifecycle guard.
    pub outstanding: Vec<Obligation>,
}

/// Facts captured from the daemon session registry, including descendants
/// created after the drain request. `final_turn_ended` must be positive
/// evidence after report_done; a quiet PTY is insufficient.
pub struct SessionObservation {
    pub session_uid: String,
    pub orchestrator: bool,
    pub exited: bool,
    pub reported_done: bool,
    pub final_turn_ended: bool,
    pub reported_at: Option<f64>,
    pub last_input_at: Option<f64>,
    pub failed: bool,
}

#[cfg(test)]
pub fn status(task: &ContinuousTask, sessions: &[SessionObservation]) -> Option<DrainStatus> {
    status_with_evidence(
        task,
        sessions,
        &super::completion::MonitorEvidence::default(),
    )
}

pub fn status_with_evidence(
    task: &ContinuousTask,
    sessions: &[SessionObservation],
    monitors: &super::completion::MonitorEvidence,
) -> Option<DrainStatus> {
    let drain = task.drain.as_ref()?;
    let mut outstanding = Vec::new();
    let mut active = false;
    let mut failed = false;
    let mut add = |kind, uid: Option<&str>, detail: &str| {
        outstanding.push(Obligation {
            kind,
            session_uid: uid.map(str::to_owned),
            detail: detail.to_string(),
        });
    };
    if !task.paused {
        failed = true;
        add(
            "admission_open",
            None,
            "Drain record exists but admission is open.",
        );
    }
    if let Some(fire) = &task.in_flight {
        active = true;
        add(
            "fire_in_flight",
            Some(&fire.session_uid),
            "An admitted fire is still claiming or delivering its work.",
        );
    }
    if let Some(run) = &task.last_run {
        let matches =
            drain.fire_token.as_ref() == Some(&run.fire_token) && drain.run_seq == Some(run.seq);
        // During spawn, last_run can still describe the previous run.
        if !matches && task.in_flight.is_none() {
            failed = true;
            add(
                "run_identity_changed",
                run.session_uid.as_deref(),
                "The last run differs from the work captured by this drain request.",
            );
        } else if matches {
            match run.status {
                RunStatus::Pending | RunStatus::Running => {
                    active = true;
                    add(
                        "run_active",
                        run.session_uid.as_deref(),
                        "The admitted run has not reported completion.",
                    );
                }
                RunStatus::Failed | RunStatus::Stuck | RunStatus::Orphaned => {
                    failed = true;
                    add(
                        "run_needs_reconciliation",
                        run.session_uid.as_deref(),
                        "The run ended unsuccessfully; reconcile its work and any staged batch.",
                    );
                }
                RunStatus::Done | RunStatus::Idle => {}
            }
        }
    } else if drain.run_seq.is_some() && task.in_flight.is_none() {
        failed = true;
        add(
            "admitted_run_missing",
            drain.session_uid.as_deref(),
            "The admitted fire has no durable run record.",
        );
    }
    if let Some(hold) = &task.recovery_hold {
        failed = true;
        add(
            "batch_reconciliation_required",
            Some(&hold.session_uid),
            &hold.detail,
        );
    }
    if task.account_blocked.is_some() {
        failed = true;
        add(
            "account_blocked",
            drain.session_uid.as_deref(),
            "An account failure holds this run; drain does not restart it or replay its batch.",
        );
    }
    if let Some(detail) = drain.diagnostic.as_deref() {
        failed = true;
        add("completion_missing", drain.session_uid.as_deref(), detail);
    }
    for session in sessions {
        if session.failed || (session.exited && !session.reported_done) {
            failed = true;
            add("session_needs_reconciliation", Some(&session.session_uid), "Session exited without a current completion report, or was killed. Its unfinished work needs reconciliation.");
        }
        if !session.exited && !(session.reported_done && session.final_turn_ended) {
            active = true;
            add(
                if session.orchestrator { "orchestrator_turn" } else { "worker" },
                Some(&session.session_uid),
                "Waiting for completion and the final turn to end; an interim idle turn is insufficient.",
            );
        }
    }
    if let Some(uid) = drain.session_uid.as_deref() {
        if !sessions.iter().any(|s| s.session_uid == uid) {
            add(
                "session_evidence_unavailable",
                Some(uid),
                "The captured session is absent from the live registry and retained exit records.",
            );
        }
    }
    if task.run_count > 0
        || drain.run_seq.is_some()
        || drain.session_uid.is_some()
        || !sessions.is_empty()
    {
        if drain.acknowledgment.is_none() {
            add(
                "drain_notice_unacknowledged",
                drain.session_uid.as_deref(),
                "The orchestrator has not acknowledged this stop request.",
            );
        }
        if monitors.fingerprint.is_none() && monitors.outstanding.is_empty() {
            add(
                "completion_evidence_unavailable",
                drain.session_uid.as_deref(),
                "Durable monitor evidence is unavailable.",
            );
        }
        if let Some(checkpoint) = &drain.checkpoint {
            if monitors.fingerprint.as_deref() != Some(&checkpoint.monitor_fingerprint)
                || super::completion::worker_fingerprint(sessions) != checkpoint.worker_fingerprint
                || sessions.iter().any(|s| {
                    s.orchestrator
                        && s.last_input_at
                            .is_some_and(|at| at > checkpoint.recorded_at)
                })
            {
                add("checkpoint_stale", drain.session_uid.as_deref(), "Monitor or worker obligations changed after the checkpoint; reconcile them and checkpoint again.");
            }
            if !sessions.iter().any(|s| {
                s.orchestrator && s.reported_at.is_some_and(|at| at >= checkpoint.recorded_at)
            }) {
                active = true;
                add(
                    "checkpoint_completion_report",
                    drain.session_uid.as_deref(),
                    "Report completion after checkpointing, then finish the final turn.",
                );
            }
        } else {
            add("checkpoint_missing", drain.session_uid.as_deref(), "After reconciling the admitted work and background jobs, record a checkpoint for this stop request.");
        }
    }
    outstanding.extend(monitors.outstanding.iter().cloned());
    let state = if failed {
        DrainState::Blocked
    } else if active {
        DrainState::Draining
    } else if outstanding.is_empty() {
        DrainState::Drained
    } else {
        DrainState::Blocked
    };
    Some(DrainStatus {
        request_id: drain.request_id.clone(),
        state,
        admission_closed: task.paused,
        outstanding,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::continuous::task::{Engine, InFlight, RunMode, RunRecord, Schedule};

    fn task() -> ContinuousTask {
        ContinuousTask::new(
            "test".into(),
            "Test".into(),
            "ws".into(),
            "/unused".into(),
            Engine::Codex,
            RunMode::Persistent,
            Schedule::OnDemand,
            "go".into(),
        )
    }

    fn with_run(status: RunStatus) -> ContinuousTask {
        let mut t = task();
        t.run_count = 4;
        t.current_session_uid = Some("orchestrator".into());
        t.last_run = Some(RunRecord {
            seq: 4,
            fire_token: "fire-4".into(),
            started_at: 100,
            finished_at: None,
            session_uid: Some("orchestrator".into()),
            status,
            trigger_source: "operator".into(),
        });
        t
    }

    #[test]
    fn drain_is_durable_idempotent_and_empty_task_is_drained() {
        let mut t = task();
        request(&mut t, 200);
        let id = t.drain.as_ref().unwrap().request_id.clone();
        let mut restored: ContinuousTask =
            serde_json::from_value(serde_json::to_value(&t).unwrap()).unwrap();
        request(&mut restored, 300);
        assert_eq!(restored.drain.as_ref().unwrap().request_id, id);
        assert_eq!(restored.drain.as_ref().unwrap().requested_at, 200);
        assert_eq!(restored.admission_revision, 1);
        assert!(restored.paused);
        assert_eq!(status(&restored, &[]).unwrap().state, DrainState::Drained);
    }

    #[test]
    fn legacy_state_has_no_drain_and_engine_remains_required() {
        let mut value = serde_json::to_value(task()).unwrap();
        value.as_object_mut().unwrap().remove("drain");
        value.as_object_mut().unwrap().remove("admission_revision");
        let t: ContinuousTask = serde_json::from_value(value.clone()).unwrap();
        assert!(t.drain.is_none());
        assert_eq!(t.admission_revision, 0);
        assert_eq!(t.engine, Engine::Codex);
        value.as_object_mut().unwrap().remove("engine");
        assert!(serde_json::from_value::<ContinuousTask>(value).is_err());
    }

    #[test]
    fn drain_captures_fire_before_run_commit_without_touching_it() {
        let mut t = with_run(RunStatus::Done);
        t.in_flight = Some(InFlight {
            fire_token: "fire-5".into(),
            session_uid: "new-uid".into(),
            started_at: 200,
        });
        request(&mut t, 201);
        let drain = t.drain.as_ref().unwrap();
        assert_eq!(drain.fire_token.as_deref(), Some("fire-5"));
        assert_eq!(drain.run_seq, Some(5));
        assert_eq!(drain.session_uid.as_deref(), Some("new-uid"));
        assert_eq!(t.last_run.as_ref().unwrap().seq, 4);
        assert!(t.in_flight.is_some());
        assert_eq!(status(&t, &[]).unwrap().state, DrainState::Draining);
        // The admitted fire completes normally after the request.
        t.last_run.as_mut().unwrap().seq = 5;
        t.last_run.as_mut().unwrap().fire_token = "fire-5".into();
        t.last_run.as_mut().unwrap().session_uid = Some("new-uid".into());
        t.in_flight = None;
        assert!(!status(&t, &[])
            .unwrap()
            .outstanding
            .iter()
            .any(|o| o.kind == "run_identity_changed"));
    }

    #[test]
    fn drain_after_run_commit_before_guard_clear_keeps_committed_sequence() {
        let mut t = with_run(RunStatus::Running);
        t.in_flight = Some(InFlight {
            fire_token: "fire-4".into(),
            session_uid: "orchestrator".into(),
            started_at: 100,
        });
        request(&mut t, 201);
        assert_eq!(t.drain.as_ref().unwrap().run_seq, Some(4));
    }

    #[test]
    fn report_done_does_not_hide_trailing_turn_or_late_worker() {
        let mut t = with_run(RunStatus::Done);
        request(&mut t, 201);
        let mut observations = vec![SessionObservation {
            session_uid: "orchestrator".into(),
            orchestrator: true,
            exited: false,
            reported_done: true,
            final_turn_ended: false,
            reported_at: Some(200.0),
            last_input_at: None,
            failed: false,
        }];
        assert_eq!(
            status(&t, &observations).unwrap().state,
            DrainState::Draining
        );
        observations[0].final_turn_ended = true;
        observations.push(SessionObservation {
            session_uid: "late-worker".into(),
            orchestrator: false,
            exited: false,
            reported_done: false,
            final_turn_ended: true,
            reported_at: Some(200.0),
            last_input_at: None,
            failed: false,
        });
        let s = status(&t, &observations).unwrap();
        assert_eq!(s.state, DrainState::Draining);
        assert!(s.outstanding.iter().any(|o| o.kind == "worker"));
        observations[1].reported_done = true;
        let s = status(&t, &observations).unwrap();
        assert_eq!(s.state, DrainState::Blocked);
        assert!(s
            .outstanding
            .iter()
            .any(|o| o.kind == "completion_evidence_unavailable"));
    }

    #[test]
    fn failed_or_missing_run_and_evicted_session_never_prove_drain() {
        for run_status in [RunStatus::Failed, RunStatus::Stuck, RunStatus::Orphaned] {
            let mut t = with_run(run_status);
            request(&mut t, 201);
            assert_eq!(status(&t, &[]).unwrap().state, DrainState::Blocked);
        }
        let mut t = with_run(RunStatus::Done);
        request(&mut t, 201);
        let s = status(&t, &[]).unwrap();
        assert!(s
            .outstanding
            .iter()
            .any(|o| o.kind == "session_evidence_unavailable"));
        t.last_run = None;
        assert!(status(&t, &[])
            .unwrap()
            .outstanding
            .iter()
            .any(|o| o.kind == "admitted_run_missing"));
    }

    #[test]
    fn superseded_run_and_open_admission_are_blockers() {
        let mut t = with_run(RunStatus::Done);
        request(&mut t, 201);
        t.last_run.as_mut().unwrap().fire_token = "unexpected".into();
        assert!(status(&t, &[])
            .unwrap()
            .outstanding
            .iter()
            .any(|o| o.kind == "run_identity_changed"));
        t.paused = false;
        assert!(status(&t, &[])
            .unwrap()
            .outstanding
            .iter()
            .any(|o| o.kind == "admission_open"));
    }
}
