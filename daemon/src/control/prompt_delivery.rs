//! Confirmation of an initial launch prompt, separate from PTY write success.
use crate::agent_state::{Inputs, PresenceStatus};
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) const CONFIRM_MAX: Duration = Duration::from_secs(90);
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Serialize)]
pub struct Receipt {
    pub id: String,
    pub status: &'static str,
    pub submitted: bool,
    pub attempts: u8,
    pub confirmed_by: Option<&'static str>,
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_seq_before: Option<u64>,
}
/// Content-free evidence captured from the existing bounded PTY ring before
/// registry removal. Markers are observations, not a claim about root cause.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct StartupFailure {
    pub session_uid: String,
    pub pid: i32,
    pub process_start: Option<u64>,
    pub generation: u64,
    pub receipt_id: Option<String>,
    pub exited_at: f64,
    pub exit_code: i32,
    pub pty_bytes_inspected: usize,
    pub pty_tail_truncated: bool,
    pub pty_closed: bool,
    pub markers: Vec<String>,
    pub hint: Option<String>,
}

pub(crate) fn startup_failure(
    session: &crate::session::DaemonSession,
    code: Option<i32>,
    at: f64,
    killed: bool,
) -> Option<StartupFailure> {
    let code = code.filter(|c| *c != 0)?;
    if session.session_type != "codex" || killed || session.reported_done().is_some() {
        return None;
    }
    let cell = session.agent_state.lock().unwrap_or_else(|p| p.into_inner());
    // A known engine turn excludes startup failure, even if a confirmation
    // worker has not published its result yet. Never inspect a running turn.
    if at - cell.inputs.spawned_at > 120.0 || at < cell.inputs.spawned_at
        || cell.inputs.hooks.prompt_at.is_some_and(|t| t >= cell.inputs.spawned_at)
        || cell.inputs.relay.as_ref().is_some_and(|r| r.turn_started_at.is_some_and(|t| t >= cell.inputs.spawned_at)) {
        return None;
    }
    let process_start = cell.child_start_time;
    drop(cell);
    let receipt = session.prompt_delivery.as_ref().map(|t| t.lock().unwrap_or_else(|p| p.into_inner()).clone());
    if receipt.as_ref().is_some_and(|r| r.submitted) { return None; }
    let tail = session.fanout.startup_tail();
    let bytes: Vec<u8> = tail.bytes.iter().map(u8::to_ascii_lowercase).collect();
    let markers: Vec<String> = [
        ("update available", "update_notice"), ("update now", "update_choice"),
        ("eacces", "permission_denied"),
        ("startupsuppressionerror", "update_suppression_failed"),
        ("codex app-server did not start", "backend_start_failed"),
        ("codex app-server startup timed out", "backend_start_timeout"),
    ].into_iter().filter(|(needle,_)| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .map(|(_,label)| label.to_owned()).collect();
    let hint = markers.iter().any(|m| m == "update_suppression_failed" || m == "update_notice")
        .then(|| "Inspect effective CODEX_HOME/version.json; repair the cache and set dismissed_version to latest_version. CM never installs updates.".to_string());
    Some(StartupFailure {session_uid:session.uid.clone(), pid:session.pid,
        process_start, generation:session.generation, receipt_id:receipt.map(|r|r.id),
        exited_at:at, exit_code:code, pty_bytes_inspected:bytes.len(),
        pty_tail_truncated:tail.evicted_since_cursor, pty_closed:tail.closed, markers, hint})
}

pub type Ticket = Arc<Mutex<Receipt>>;
impl Receipt {
    pub fn pending() -> Ticket {
        Arc::new(Mutex::new(Self {
            id: uuid::Uuid::new_v4().to_string(),
            status: "pending",
            submitted: false,
            attempts: 0,
            confirmed_by: None,
            reason: None,
            turn_seq_before: None,
        }))
    }
}
pub(crate) fn finish(ticket: &Ticket, result: Result<&'static str, &'static str>) {
    let mut receipt = ticket.lock().unwrap_or_else(|p| p.into_inner());
    receipt.status = if result.is_ok() {
        "confirmed"
    } else {
        "unconfirmed"
    };
    receipt.submitted = result.is_ok();
    receipt.confirmed_by = result.as_ref().ok().copied();
    receipt.reason = result.err();
}

pub(crate) fn delivered_command(ticket: &Ticket) {
    finish(ticket, Ok("write"));
    ticket.lock().unwrap_or_else(|p| p.into_inner()).status = "delivered";
}

pub(super) enum Check {
    Confirmed(&'static str),
    Pending,
    Deferred,
    Stop(&'static str),
}
pub(super) fn confirm(
    ticket: &Ticket,
    mut check: impl FnMut() -> Check,
    mut retry: impl FnMut() -> Check,
    retry_after: Duration,
    max: Duration,
    interval: Duration,
) {
    let started = Instant::now();
    let mut retried = false;
    loop {
        match check() {
            Check::Confirmed(source) => {
                finish(ticket, Ok(source));
                return;
            }
            Check::Stop(reason) => {
                finish(ticket, Err(reason));
                return;
            }
            Check::Pending | Check::Deferred => {}
        }
        if started.elapsed() >= max {
            finish(ticket, Err("no_engine_turn"));
            return;
        }
        if !retried && started.elapsed() >= retry_after {
            // The callback rechecks identity/operator activity at the write.
            match retry() {
                Check::Pending => {
                    ticket.lock().unwrap_or_else(|p| p.into_inner()).attempts = 2;
                    retried = true;
                }
                Check::Deferred => {}
                Check::Confirmed(source) => {
                    finish(ticket, Ok(source));
                    return;
                }
                Check::Stop(reason) => {
                    finish(ticket, Err(reason));
                    return;
                }
            }
        }
        std::thread::sleep(interval);
    }
}

pub(super) struct LaunchWrite {
    pub written_at: Option<Instant>,
    pub enter: Vec<u8>,
}

pub(super) struct Baseline {
    pub at: f64,
    pub(super) turn_seq: u64,
    relay_seq: Option<u64>,
    relay_start: Option<f64>,
    hook_prompt: Option<f64>,
    presence_stamp: Option<f64>,
}
impl Baseline {
    pub fn capture(input: &Inputs, at: f64) -> Self {
        Self {
            at,
            turn_seq: input.turn_seq,
            relay_seq: input.relay.as_ref().map(|r| r.turn_seq),
            relay_start: input.relay.as_ref().and_then(|r| r.turn_started_at),
            hook_prompt: input.hooks.prompt_at,
            presence_stamp: input.presence.as_ref().and_then(|p| p.status_updated_at),
        }
    }
    pub fn confirmed_for(&self, input: &mut Inputs, transcript: Option<&str>) -> Option<&'static str> {
        crate::agent_state::check_presence_binding(input, transcript);
        self.confirmed_by(input)
    }
    fn confirmed_by(&self, input: &Inputs) -> Option<&'static str> {
        // CM stamps this counter itself. It is necessary, but alone cannot
        // prove that an engine consumed the prompt (nor can PTY repaint).
        if input.turn_seq <= self.turn_seq {
            return None;
        }
        if let Some(relay) = &input.relay {
            if relay.backend_connected
                && relay.observed_at >= self.at
                && relay
                    .turn_started_at
                    .is_some_and(|at| at >= self.at && Some(at) != self.relay_start)
                && self.relay_seq.is_none_or(|seq| relay.turn_seq != seq)
            {
                return Some("relay");
            }
        }
        if input
            .hooks
            .prompt_at
            .is_some_and(|at| at >= self.at && Some(at) != self.hook_prompt)
        {
            return Some("hooks");
        }
        if let Some(p) = &input.presence {
            if p.valid
                && p.status == PresenceStatus::Busy
                && p.main_turn_open == Some(true)
                && p.observed_at >= self.at
                && p.status_updated_at.is_some_and(|at| at >= self.at - 0.001)
                && p.status_updated_at != self.presence_stamp
            {
                return Some("presence");
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_state::{RelaySnapshot, RelayStatus, StateCell};

    #[test]
    fn cm_input_or_old_busy_state_is_not_submission_evidence() {
        let mut cell = StateCell::new(1.0, None);
        cell.inputs.relay = Some(RelaySnapshot {
            backend_connected: true,
            foreground: RelayStatus::Active,
            turn_seq: 1,
            turn_started_at: Some(90.0),
            observed_at: 99.0,
            ..Default::default()
        });
        let baseline = Baseline::capture(&cell.inputs, 100.0);
        cell.note_input(101.0);
        assert_eq!(baseline.confirmed_by(&cell.inputs), None);
        let relay = cell.inputs.relay.as_mut().unwrap();
        relay.turn_seq = 2;
        relay.turn_started_at = Some(102.0);
        relay.observed_at = 103.0;
        assert_eq!(baseline.confirmed_by(&cell.inputs), Some("relay"));
        cell.inputs.relay.as_mut().unwrap().backend_connected = false;
        assert_eq!(baseline.confirmed_by(&cell.inputs), None);
    }

    #[test]
    fn failure_receipt_never_claims_submission() {
        let ticket = Receipt::pending();
        finish(&ticket, Err("no_engine_turn"));
        let receipt = ticket.lock().unwrap();
        assert!(!receipt.submitted);
        assert_eq!(receipt.status, "unconfirmed");
    }

    #[test]
    fn confirmed_receipt_retains_the_counter_before_the_launch_write() {
        let ticket = Receipt::pending();
        ticket.lock().unwrap().turn_seq_before = Some(4);
        finish(&ticket, Ok("relay"));
        let receipt = serde_json::to_value(ticket.lock().unwrap().clone()).unwrap();
        assert_eq!(receipt["turn_seq_before"], 4);
        assert_eq!(receipt["submitted"], true);
    }

    #[test]
    fn deferred_retry_does_not_end_confirmation_or_consume_an_attempt() {
        use std::cell::Cell;
        let ticket = Receipt::pending();
        ticket.lock().unwrap().attempts = 1;
        let polls = Cell::new(0);
        confirm(
            &ticket,
            || {
                polls.set(polls.get() + 1);
                if polls.get() >= 4 {
                    Check::Confirmed("hooks")
                } else {
                    Check::Pending
                }
            },
            || Check::Deferred,
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_millis(1),
        );
        let receipt = ticket.lock().unwrap();
        assert!(receipt.submitted);
        assert_eq!(receipt.attempts, 1);
    }

    #[test]
    fn launch_confirmation_revalidates_cached_busy_at_effect_time() {
        let mut cell = StateCell::new(1.0, None);
        let baseline = Baseline::capture(&cell.inputs, 100.0);
        cell.note_input(101.0);
        cell.inputs.presence = Some(crate::agent_state::PresenceObs {
            valid: true, status: PresenceStatus::Busy, observed_at: 102.0,
            status_updated_at: Some(101.0), native_session_id: Some("original".into()),
            main_turn_open: Some(true), engine_version: None, waiting_for: None,
            transcript_error: None, transcript_end: None, transcript_evidence_path: None, wait_resolution: None,
        });
        assert_eq!(baseline.confirmed_for(&mut cell.inputs, Some("/fixture/original.jsonl")), Some("presence"));
        assert_eq!(baseline.confirmed_for(&mut cell.inputs, Some("/fixture/rebound.jsonl")), None,
            "after generation deferral, an old busy snapshot still cannot confirm a new conversation");
        let p = cell.inputs.presence.as_mut().unwrap();
        p.native_session_id = Some("rebound".into());
        p.valid = true;
        assert_eq!(baseline.confirmed_for(&mut cell.inputs, Some("/fixture/rebound.jsonl")), Some("presence"));
    }

    #[test]
    fn fresh_presence_and_hook_edges_confirm_but_background_busy_does_not() {
        let mut cell = StateCell::new(1.0, None);
        let baseline = Baseline::capture(&cell.inputs, 100.0);
        cell.note_input(101.0);
        cell.inputs.presence = Some(crate::agent_state::PresenceObs {
            valid: true,
            status: PresenceStatus::Busy,
            observed_at: 102.0,
            status_updated_at: Some(101.0), native_session_id: None,
            main_turn_open: Some(false),
            engine_version: None,
            waiting_for: None,
            transcript_error: None, transcript_end: None, transcript_evidence_path: None, wait_resolution: None,
        });
        assert_eq!(baseline.confirmed_by(&cell.inputs), None);
        cell.inputs.presence.as_mut().unwrap().main_turn_open = Some(true);
        assert_eq!(baseline.confirmed_by(&cell.inputs), Some("presence"));
        cell.inputs.presence.as_mut().unwrap().status_updated_at = None;
        assert_eq!(baseline.confirmed_by(&cell.inputs), None,
            "a parse/poll without an engine timestamp cannot confirm delivery");
        cell.inputs.presence.as_mut().unwrap().valid = false;
        assert_eq!(baseline.confirmed_by(&cell.inputs), None);
        cell.inputs.hooks.prompt_at = Some(99.0);
        assert_eq!(baseline.confirmed_by(&cell.inputs), None);
        cell.inputs.hooks.prompt_at = Some(101.0);
        assert_eq!(baseline.confirmed_by(&cell.inputs), Some("hooks"));
    }

    #[test]
    fn confirmation_retries_once_and_reports_the_observed_outcome() {
        use std::cell::Cell;
        for succeeds in [false, true] {
            let ticket = Receipt::pending();
            ticket.lock().unwrap().attempts = 1;
            let retries = Cell::new(0);
            confirm(
                &ticket,
                || {
                    if succeeds && retries.get() == 1 {
                        Check::Confirmed("presence")
                    } else {
                        Check::Pending
                    }
                },
                || {
                    retries.set(retries.get() + 1);
                    Check::Pending
                },
                Duration::ZERO,
                Duration::from_millis(8),
                Duration::from_millis(1),
            );
            assert_eq!(retries.get(), 1);
            let receipt = ticket.lock().unwrap();
            assert_eq!(receipt.submitted, succeeds);
            assert_eq!(receipt.attempts, 2);
            assert_eq!(
                receipt.reason,
                if succeeds {
                    None
                } else {
                    Some("no_engine_turn")
                }
            );
        }
    }

    #[test]
    fn unrelated_activity_or_operator_input_stops_without_retry() {
        let ticket = Receipt::pending();
        confirm(
            &ticket,
            || Check::Stop("operator_input"),
            || panic!("must not retry"),
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::ZERO,
        );
        assert!(!ticket.lock().unwrap().submitted);
    }
}
