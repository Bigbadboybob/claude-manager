//! The brain-supervision state machine — DESIGN_HOLDER_BRAIN_SPLIT
//! § Supervision's breaker table (O1/O5), as PURE logic the binary
//! drives and unit tests pin. The binary owns the side effects
//! (fork/exec, pins as fds, sleeps); this module owns the decisions.
//!
//! | State | Exit via |
//! |---|---|
//! | RUNNING   | brain exit → BACKOFF (or an armed deploy → exec next) |
//! | BACKOFF   | respawn current pin after 0.5s→30s backoff; **3
//! |           | consecutive failures with no intervening stable run**
//! |           | (deliberately un-windowed, V1) → ROLLBACK |
//! | ROLLBACK  | exec the previous pin; the failed pin is DISCARDED,
//! |           | never demoted (O5 — no ping-pong); no previous →
//! |           | HELD_DOWN |
//! | HELD_DOWN | every retry interval: re-open `--brain` from the
//! |           | PATH (fresh pin — O1: a fixed binary on disk
//! |           | self-heals); SIGUSR2 forces an immediate retry |
//!
//! A **stable run** = the generation hello'd and lived ≥ the
//! stability horizon; it resets the consecutive-failure counter and
//! the backoff. A **deploy exit** (an armed `restart_brain` /
//! `rollback_brain`) is never a failure.

use std::time::Duration;

/// Trip threshold: consecutive failure exits with no intervening
/// stable run.
pub const TRIP_AFTER: u32 = 3;

/// What the supervisor should do after a FAILURE exit.
#[derive(Debug, PartialEq)]
pub enum BreakerDecision {
    /// Respawn the current pin after this backoff.
    Respawn { backoff: Duration },
    /// Trip: exec the previous pin, DISCARDING the current (bad) one.
    Rollback,
    /// Trip with no previous pin: hold down + path-retry.
    HoldDown,
}

#[derive(Debug)]
pub struct Breaker {
    consecutive_failures: u32,
    backoff: Duration,
    pub backoff_floor: Duration,
    pub backoff_cap: Duration,
    /// A generation that hello'd and ran at least this long resets
    /// the counter (the doc's 10 minutes; tests shrink it).
    pub stable_horizon: Duration,
}

impl Default for Breaker {
    fn default() -> Self {
        Breaker {
            consecutive_failures: 0,
            backoff: Duration::from_millis(500),
            backoff_floor: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(30),
            stable_horizon: Duration::from_secs(600),
        }
    }
}

impl Breaker {
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// An armed-deploy exit: never a failure; the fresh binary
    /// starts with a clean slate.
    pub fn note_deploy(&mut self) {
        self.consecutive_failures = 0;
        self.backoff = self.backoff_floor;
    }

    /// A successful recovery out of HELD_DOWN / a rollback that
    /// stuck: clean slate.
    pub fn reset(&mut self) {
        self.consecutive_failures = 0;
        self.backoff = self.backoff_floor;
    }

    /// A FAILURE exit (crash, wedge kill, protocol violation,
    /// hello timeout). `ran_for` + `helloed` decide whether the
    /// generation counted as stable first.
    pub fn note_failure(
        &mut self,
        ran_for: Duration,
        helloed: bool,
        has_previous_pin: bool,
    ) -> BreakerDecision {
        if helloed && ran_for >= self.stable_horizon {
            // The stable run resets BEFORE counting this failure —
            // "3 consecutive failures with no intervening stable
            // run" (V1's un-windowed rule).
            self.consecutive_failures = 0;
            self.backoff = self.backoff_floor;
        }
        self.consecutive_failures += 1;
        if self.consecutive_failures >= TRIP_AFTER {
            // Trip. The counter resets either way — the next pin
            // (previous, or a held-down path-retry) gets its own
            // three strikes.
            self.consecutive_failures = 0;
            self.backoff = self.backoff_floor;
            return if has_previous_pin {
                BreakerDecision::Rollback
            } else {
                BreakerDecision::HoldDown
            };
        }
        let backoff = self.backoff;
        self.backoff = (self.backoff * 2).min(self.backoff_cap);
        BreakerDecision::Respawn { backoff }
    }
}

/// The {current, previous} pin pair (R7: fds, never pathnames).
/// Generic so the machine is testable without real fds.
#[derive(Debug)]
pub struct PinSet<T> {
    pub current: Option<T>,
    pub previous: Option<T>,
}

// Manual impl: `derive(Default)` would wrongly require `T: Default`
// even though both fields are `Option`s.
impl<T> Default for PinSet<T> {
    fn default() -> Self {
        PinSet {
            current: None,
            previous: None,
        }
    }
}

impl<T> PinSet<T> {
    /// A DEPLOY installs the new pin; the old current becomes the
    /// rollback target.
    pub fn install_new(&mut self, pin: T) {
        self.previous = self.current.take();
        self.current = Some(pin);
    }

    /// A ROLLBACK (breaker trip or operator `rollback_brain`): the
    /// bad current is DISCARDED — never demoted to previous, so the
    /// pair cannot ping-pong (O5). Returns false when no previous
    /// exists (→ HELD_DOWN).
    pub fn rollback(&mut self) -> bool {
        match self.previous.take() {
            Some(prev) => {
                self.current = Some(prev);
                true
            }
            None => {
                self.current = None;
                false
            }
        }
    }

    /// A HELD_DOWN path-retry re-pin: replaces current outright
    /// (there is nothing worth keeping).
    pub fn replace_current(&mut self, pin: T) {
        self.current = Some(pin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fail(b: &mut Breaker, has_prev: bool) -> BreakerDecision {
        b.note_failure(Duration::from_secs(1), true, has_prev)
    }

    #[test]
    fn trips_to_rollback_on_third_consecutive_failure() {
        let mut b = Breaker::default();
        assert!(matches!(fail(&mut b, true), BreakerDecision::Respawn { .. }));
        assert!(matches!(fail(&mut b, true), BreakerDecision::Respawn { .. }));
        assert_eq!(fail(&mut b, true), BreakerDecision::Rollback);
        // Clean slate for the rolled-back pin.
        assert_eq!(b.consecutive_failures(), 0);
    }

    #[test]
    fn trips_to_hold_down_without_a_previous_pin() {
        let mut b = Breaker::default();
        fail(&mut b, false);
        fail(&mut b, false);
        assert_eq!(fail(&mut b, false), BreakerDecision::HoldDown);
    }

    #[test]
    fn a_stable_run_resets_the_counter_unwindowed() {
        // V1: slow crash loops (one crash a minute) must still trip;
        // only a genuinely STABLE run resets. Two failures, then a
        // stable generation, then two more: no trip until the third
        // consecutive.
        let mut b = Breaker {
            stable_horizon: Duration::from_secs(10),
            ..Breaker::default()
        };
        fail(&mut b, true);
        fail(&mut b, true);
        // Stable generation (helloed, ran past the horizon), then
        // its eventual failure counts as ONE.
        assert!(matches!(
            b.note_failure(Duration::from_secs(11), true, true),
            BreakerDecision::Respawn { .. }
        ));
        assert_eq!(b.consecutive_failures(), 1);
        fail(&mut b, true);
        assert_eq!(fail(&mut b, true), BreakerDecision::Rollback);
    }

    #[test]
    fn a_long_run_without_hello_is_not_stable() {
        // A brain that never hello'd (proto_mismatch loop, hello
        // timeout) can run "long" in wall time between kills; that
        // must not reset the counter.
        let mut b = Breaker {
            stable_horizon: Duration::from_secs(1),
            ..Breaker::default()
        };
        b.note_failure(Duration::from_secs(60), false, true);
        b.note_failure(Duration::from_secs(60), false, true);
        assert_eq!(
            b.note_failure(Duration::from_secs(60), false, true),
            BreakerDecision::Rollback
        );
    }

    #[test]
    fn deploy_exits_never_count() {
        let mut b = Breaker::default();
        fail(&mut b, true);
        fail(&mut b, true);
        b.note_deploy();
        assert!(matches!(fail(&mut b, true), BreakerDecision::Respawn { .. }));
    }

    #[test]
    fn backoff_doubles_to_the_cap_and_resets_on_stability() {
        let mut b = Breaker {
            backoff_floor: Duration::from_millis(500),
            backoff_cap: Duration::from_secs(2),
            stable_horizon: Duration::from_secs(10),
            ..Breaker::default()
        };
        let d1 = match fail(&mut b, true) {
            BreakerDecision::Respawn { backoff } => backoff,
            other => panic!("{other:?}"),
        };
        let d2 = match fail(&mut b, true) {
            BreakerDecision::Respawn { backoff } => backoff,
            other => panic!("{other:?}"),
        };
        assert_eq!(d1, Duration::from_millis(500));
        assert_eq!(d2, Duration::from_secs(1));
        // Stable run → floor again.
        let d3 = match b.note_failure(Duration::from_secs(11), true, true) {
            BreakerDecision::Respawn { backoff } => backoff,
            other => panic!("{other:?}"),
        };
        assert_eq!(d3, Duration::from_millis(500));
    }

    #[test]
    fn pins_discard_not_demote() {
        // O5: after a rollback the bad pin is GONE — a later trip of
        // the good pin holds down instead of ping-ponging back.
        let mut pins: PinSet<&'static str> = PinSet::default();
        pins.install_new("v1");
        pins.install_new("v2-bad");
        assert_eq!(pins.previous, Some("v1"));
        assert!(pins.rollback());
        assert_eq!(pins.current, Some("v1"));
        assert_eq!(pins.previous, None, "the bad pin must be discarded");
        assert!(!pins.rollback(), "no previous → hold down");
    }
}
