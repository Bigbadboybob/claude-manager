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
    turn_seq: u64,
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
            presence_stamp: input.presence.as_ref().map(|p| p.status_updated_at),
        }
    }
    pub fn confirmed_by(&self, input: &Inputs) -> Option<&'static str> {
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
                && p.status_updated_at >= self.at - 0.001
                && Some(p.status_updated_at) != self.presence_stamp
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
    fn fresh_presence_and_hook_edges_confirm_but_background_busy_does_not() {
        let mut cell = StateCell::new(1.0, None);
        let baseline = Baseline::capture(&cell.inputs, 100.0);
        cell.note_input(101.0);
        cell.inputs.presence = Some(crate::agent_state::PresenceObs {
            valid: true,
            status: PresenceStatus::Busy,
            observed_at: 102.0,
            status_updated_at: 101.0,
            main_turn_open: Some(false),
            engine_version: None,
            waiting_for: None,
            transcript_error: None,
        });
        assert_eq!(baseline.confirmed_by(&cell.inputs), None);
        cell.inputs.presence.as_mut().unwrap().main_turn_open = Some(true);
        assert_eq!(baseline.confirmed_by(&cell.inputs), Some("presence"));
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
