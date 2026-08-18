//! Integration tests for the pidfd-poll reaper + reap gate.
//! Restart hardening / DESIGN_SEAMLESS_RESTART phase 2a (review
//! finding R4).
//!
//! Two properties of the converted reaper are proven here, against
//! real children through the published library API:
//!
//! 1. **Exit fidelity** — the detect-then-consume reaper
//!    (`poll(2)` on a dup'd pidfd, then `waitid(P_PIDFD)` under
//!    `reap_gate::read_permit()`) reports exactly the same typed
//!    `DaemonExitStatus` the old blocking-`waitpid` reaper did, for
//!    clean exits (0 and N) and signal deaths alike.
//! 2. **The permit actually gates** — with `reap_gate::freeze()`
//!    held, a child that exits promptly is *detected* but not
//!    *consumed*: no exit status reaches the session's exit channel
//!    for multiple reaper poll periods (the child parks as a
//!    kernel zombie). Releasing the freeze delivers the exit with
//!    full, correct status — deferred, never lost. This is the
//!    exact guarantee the future restart coordinator relies on for
//!    the handoff window.
//!
//! Lives in `tests/` (own process) deliberately: the reap gate is
//! process-global, so holding a freeze for seconds inside the unit
//! test binary would stall every concurrently-running session
//! test's reaper there. In this binary the freeze test coexists
//! only with the fidelity tests below, whose await deadlines are
//! sized to tolerate it (this suite also runs under machine load —
//! generous bounds throughout, per repo test convention).

use std::time::{Duration, Instant};

use cm_daemon::reap_gate;
use cm_daemon::session::{
    DaemonExitStatus, DaemonSession, SpawnParams, REAPER_POLL_INTERVAL_MS,
};

/// Spin-wait for `session.try_wait()` to surface the exit. The
/// deadline is deliberately generous (way past the unit-test
/// binary's 3s idiom): the freeze test in this same binary holds
/// the process-global gate for ~2s while cargo runs these tests in
/// parallel, and the whole suite runs under load.
fn await_exit_generous(
    session: &mut DaemonSession,
    label: &str,
) -> DaemonExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(status) = session.try_wait() {
            return status;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{}: child exit never surfaced within 20s", label);
}

fn spawn_sh(uid: &str, script: &str) -> DaemonSession {
    let mut params = SpawnParams::new(uid, format!("{}-title", uid), "/bin/sh");
    params.args = vec!["-c".into(), script.into()];
    DaemonSession::spawn(params).unwrap_or_else(|e| {
        panic!("{}: spawn /bin/sh -c {:?}: {}", uid, script, e)
    })
}

/// Fidelity: clean exit 0 → `{ code: Some(0), signal: None }`.
#[test]
fn gated_reaper_reports_exit_zero() {
    let mut session = spawn_sh("rg-exit0", "exit 0");
    let status = await_exit_generous(&mut session, "sh -c 'exit 0'");
    assert_eq!(
        status,
        DaemonExitStatus {
            code: Some(0),
            signal: None,
        },
        "clean exit must surface code=Some(0), signal=None",
    );
}

/// Fidelity: nonzero exit → `{ code: Some(3), signal: None }`.
#[test]
fn gated_reaper_reports_exit_code() {
    let mut session = spawn_sh("rg-exit3", "exit 3");
    let status = await_exit_generous(&mut session, "sh -c 'exit 3'");
    assert_eq!(
        status,
        DaemonExitStatus {
            code: Some(3),
            signal: None,
        },
        "exit(3) must surface code=Some(3), signal=None",
    );
}

/// Fidelity: SIGKILL death → `{ code: None, signal: Some(9) }` —
/// the discriminator the memory-cap-kill classification keys off.
#[test]
fn gated_reaper_reports_sigkill_signal() {
    let mut session = spawn_sh("rg-sigkill", "kill -9 $$");
    let status = await_exit_generous(&mut session, "sh -c 'kill -9 $$'");
    assert_eq!(
        status,
        DaemonExitStatus {
            code: None,
            signal: Some(9),
        },
        "SIGKILL must surface code=None, signal=Some(9)",
    );
}

/// The gate gates: under `freeze()`, an exited child's status is
/// not consumed (nothing on the exit channel for well over two
/// reaper poll periods); on release, the status arrives intact.
#[test]
fn freeze_defers_exit_delivery_until_released() {
    // Take the freeze BEFORE spawning so there is no window in
    // which the reaper could consume the fast-exiting child's
    // status ungated. Spawning under a freeze is legal: spawn and
    // readiness *detection* need no permit, only consumption does.
    let frozen = reap_gate::freeze();

    let mut session = spawn_sh("rg-freeze", "exit 7");

    // The child exits almost immediately; the reaper's pidfd poll
    // detects that within one poll wakeup and then parks acquiring
    // the read permit. Assert that across a window comfortably
    // longer than two poll periods (the spec's bound for "the loop
    // definitely had multiple chances to consume"), nothing lands
    // on the exit channel. This direction cannot flake under load:
    // a slow machine only makes delivery *less* likely during the
    // window.
    let window = Duration::from_millis(2 * REAPER_POLL_INTERVAL_MS as u64 + 1000);
    let start = Instant::now();
    while start.elapsed() < window {
        assert_eq!(
            session.try_wait(),
            None,
            "exit status was consumed and delivered while the reap \
             gate was frozen — the freeze must block consumption",
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Thaw. The parked reaper acquires its permit, consumes via
    // waitid(P_PIDFD), and the status arrives with full fidelity —
    // deferred by the freeze, not degraded by it.
    drop(frozen);
    let status = await_exit_generous(&mut session, "sh -c 'exit 7' post-thaw");
    assert_eq!(
        status,
        DaemonExitStatus {
            code: Some(7),
            signal: None,
        },
        "exit consumed after thaw must carry the original status",
    );
}
