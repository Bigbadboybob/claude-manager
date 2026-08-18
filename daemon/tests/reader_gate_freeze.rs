//! Integration test for the poll-then-read PTY reader + reader
//! gate. Restart hardening / DESIGN_SEAMLESS_RESTART phase 2b
//! (review finding R3).
//!
//! Proves, against a real child through the published library API,
//! the two halves of the reader-gate contract:
//!
//! 1. **The permit actually gates** — with `reader_gate::freeze()`
//!    held, a mid-stream session's fanout stops growing for
//!    multiple reader poll periods: the reader is parked at
//!    `read_permit()` (or in its poll wait) with ZERO bytes on its
//!    stack, and the child's continuing output waits in the
//!    kernel's PTY buffer.
//! 2. **Deferred, never lost** — after the freeze drops, the
//!    complete known byte sequence arrives in order with no gaps:
//!    everything the child wrote is either pushed to the fanout or
//!    was waiting in the kernel buffer, exactly the invariant the
//!    future re-exec handoff relies on (the phase-1 OS proof
//!    assumed it by construction; this test makes it true with a
//!    live reader draining).
//!
//! Lives in `tests/` (own process) deliberately: the reader gate is
//! process-global, so holding a freeze for ~1s inside the unit test
//! binary would stall every concurrently-running session test's
//! reader there. Deadlines are generous throughout — this suite
//! runs under machine load, per repo test convention.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cm_daemon::reader_gate;
use cm_daemon::session::{DaemonSession, SpawnParams, READER_POLL_INTERVAL_MS};

/// Number of `LINE-$i` lines the child emits. At ~20ms pacing per
/// line the stream lasts ≥4s of wall time (sleep is a lower bound
/// even under load), so the freeze below reliably lands mid-stream.
/// Total output is ~2.5 KiB — far inside the default 1 MiB fanout
/// ring, so nothing is ever evicted and `snapshot_since(None)` at
/// the end is the complete byte stream.
const LINE_COUNT: usize = 200;

#[test]
fn freeze_pauses_fanout_growth_and_loses_no_bytes() {
    // Known bounded sequence, paced so it spans several seconds.
    let script = format!(
        "i=1; while [ $i -le {} ]; do echo LINE-$i; sleep 0.02; i=$((i+1)); done",
        LINE_COUNT
    );
    let mut params =
        SpawnParams::new("rdrg-freeze", "rdrg-freeze-title", "/bin/sh");
    params.args = vec!["-c".into(), script.clone()];
    let mut session = DaemonSession::spawn(params)
        .unwrap_or_else(|e| panic!("spawn /bin/sh -c {:?}: {}", script, e));
    let fanout = Arc::clone(&session.fanout);

    // Wait for the stream to start (generous: load).
    let deadline = Instant::now() + Duration::from_secs(30);
    while fanout.snapshot_since(None).cursor == 0 {
        assert!(
            Instant::now() < deadline,
            "no PTY output reached the fanout within 30s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Freeze mid-stream. `freeze()` blocks until any in-flight
    // read→push unit completes, so once it returns the cursor
    // (`bytes_written`, monotonic) is guaranteed stable.
    let frozen = reader_gate::freeze();
    let frozen_cursor = fanout.snapshot_since(None).cursor;

    // Hold the freeze across several reader poll periods and
    // assert the fanout does not grow. The reader definitely had
    // multiple chances to run its loop in this window; only the
    // gate can be keeping bytes out. This direction cannot flake
    // under load: a slow machine only makes pushes *less* likely.
    let window =
        Duration::from_millis(6 * READER_POLL_INTERVAL_MS as u64 + 200);
    let start = Instant::now();
    while start.elapsed() < window {
        assert_eq!(
            fanout.snapshot_since(None).cursor,
            frozen_cursor,
            "fanout grew while the reader gate was frozen — the \
             freeze must exclude every read→push unit",
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Thaw. The parked reader resumes draining: first whatever
    // accumulated in the kernel PTY buffer during the freeze, then
    // the live remainder of the stream.
    drop(frozen);

    // Child exit (generous: ≥4s of pacing + load).
    let deadline = Instant::now() + Duration::from_secs(60);
    while session.try_wait().is_none() {
        assert!(
            Instant::now() < deadline,
            "child exit never surfaced within 60s of the thaw"
        );
        std::thread::sleep(Duration::from_millis(25));
    }

    // Wait for the reader's EOF close so every byte the child wrote
    // has been pushed (child exit precedes the reader draining the
    // final chunks; `closed` is the reader's own "done" signal).
    let deadline = Instant::now() + Duration::from_secs(30);
    while !fanout.snapshot_since(None).closed {
        assert!(
            Instant::now() < deadline,
            "fanout never closed within 30s of child exit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // The complete sequence, in order, no gaps. The whole stream is
    // still in the ring (see LINE_COUNT doc), so snapshot(None) is
    // byte-complete from offset 0.
    let snap = fanout.snapshot_since(None);
    assert_eq!(
        snap.start_offset, 0,
        "ring evicted bytes — output must be sized to fit the ring \
         for the completeness assertion to be meaningful"
    );
    let text = String::from_utf8_lossy(&snap.bytes);
    let lines: Vec<&str> = text
        .split(['\r', '\n'])
        .filter(|l| l.starts_with("LINE-"))
        .collect();
    assert_eq!(
        lines.len(),
        LINE_COUNT,
        "expected exactly {} LINE-* lines, got {} — bytes were lost \
         or duplicated across the freeze; raw output:\n{}",
        LINE_COUNT,
        lines.len(),
        text
    );
    for (i, line) in lines.iter().enumerate() {
        let expected = format!("LINE-{}", i + 1);
        assert_eq!(
            *line, expected,
            "sequence gap/misorder at position {}: got {:?}, raw \
             output:\n{}",
            i, line, text
        );
    }

    // Meaningfulness guard: the freeze really was mid-stream — some
    // of the sequence arrived only after the thaw. With ≥4s of
    // pacing and the freeze taken within ~10ms of first output,
    // this can only fail if the test thread was descheduled for
    // seconds between those two steps (i.e. an extreme-load flake,
    // not a gate regression).
    assert!(
        frozen_cursor < snap.cursor,
        "freeze was not mid-stream (cursor {} at freeze == {} at \
         EOF) — the growth assertion proved nothing",
        frozen_cursor,
        snap.cursor
    );
}
