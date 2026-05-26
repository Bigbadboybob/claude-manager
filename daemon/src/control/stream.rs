//! Bidirectional attach-stream transition. Slice 10c-c shipped the
//! one-way (server→client) version; the slice-10c-e-2 review fix
//! makes the connection full duplex so client→server input and
//! resize frames reach the daemon's PTY.
//!
//! After [`crate::control::dispatch::dispatch_request`] returns a
//! successful response for `attach.open`, the connection's role
//! changes from "one RPC round-trip, then close" to "bidirectional
//! PTY stream until the client disconnects."
//!
//! [`handle_attach_stream`] runs the inbound loop on the calling
//! thread and spawns one outbound thread:
//!
//! - **Outbound (spawned thread)**: subscribes to
//!   [`DaemonSession.fanout`](crate::session::PtyByteFanout). Each
//!   chunk becomes a [`StreamKind::Data`] frame written to a
//!   `try_clone()`d half of the socket. Closes with a
//!   [`StreamKind::End`] frame when the fanout is closed (child
//!   exited / session removed) or with a write error if the client
//!   disconnected.
//!
//! - **Inbound (calling thread)**: reads length-prefixed
//!   [`StreamFrame`]s from the socket. Dispatches by kind:
//!     - [`StreamKind::Input`] → decode base64 bytes →
//!       `DaemonSession::send_input`.
//!     - [`StreamKind::Resize`] → extract `cols`/`rows` →
//!       `DaemonSession::resize`.
//!     - Anything else: log and ignore (defensive against a
//!       confused client; the schema doc names which kinds are
//!       valid in this direction).
//!   Returns on EOF or unrecoverable read error.
//!
//! ## Exit coordination
//!
//! When the client disconnects, the inbound read returns EOF; we
//! then `shutdown(Both)` on the socket which makes the outbound
//! thread's next `write_stream_frame` fail and exit. Joining the
//! writer handle is the cleanup hand-off. Conversely, if the
//! outbound side fails first (e.g. fanout closes → write `End` →
//! return), the inbound loop will see EOF on the next read and
//! also exit. No explicit signal channel needed — the socket
//! itself is the coordination primitive.

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;

use crate::control::dispatch::AttachStreamHandle;
use crate::control::protocol::{StreamFrame, StreamKind};
use crate::control::wire;
use crate::session::SharedLastExit;
use crate::state::DaemonState;

/// Standard-padded base64 — matches `tui/src/term_shim.rs::BASE64`
/// byte-for-byte. The decoder there reads
/// `frame.payload["bytes"].as_str()` and calls
/// `BASE64.decode(...)`; daemon must encode with the same engine
/// for the round-trip to work.
const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// How long an attach-stream subscriber waits on the fanout
/// receiver before checking liveness. 250ms is short enough that
/// disconnect is observed promptly, long enough not to busy-spin.
const FANOUT_RECV_TICK: Duration = Duration::from_millis(250);

/// Run the bidirectional attach stream on `stream`.
///
/// Preconditions:
///   - The `attach.open` OK response has already been written.
///   - `handle.fanout_rx` is the pre-built subscriber from the
///     dispatcher's locked section (slice-10c-e-2 review-2 fix).
///   - `handle.last_exit` is the shared exit slot the reaper
///     writes to; this function reads it to populate the End
///     frame's `{exit_code, memory_cap_kill}` payload.
///
/// `state` is still passed in for the inbound side's
/// `send_input` / `resize` dispatch — those need to look the
/// session up at handle time, not at attach time, because
/// `send_input` is meant to fail with Conflict if the session
/// exits mid-stream.
///
/// On exit, the socket is shut down `Both`-ways (idempotent if
/// the client beat us to it), and the writer thread is joined.
pub fn handle_attach_stream(
    stream: &mut UnixStream,
    state: Arc<Mutex<DaemonState>>,
    handle: AttachStreamHandle,
) {
    let AttachStreamHandle {
        session_uid,
        fanout_rx,
        last_exit,
        request_id,
    } = handle;

    // Attach streams are long-lived. Clear the read timeout the
    // RPC path set (30s by default) — an idle client typing
    // shouldn't drop the stream. A write timeout is fine: it
    // catches stuck clients without false-positive disconnects.
    let _ = stream.set_read_timeout(None);

    // Clone the socket for the outbound thread to write through.
    // try_clone returns a separate handle on the same underlying
    // file descriptor — kernel-level r/w sync handles concurrent
    // read on the original + write on the clone.
    let write_socket = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "cm-daemon: attach stream {} try_clone failed: {} (cannot start writer thread)",
                session_uid, e,
            );
            return;
        }
    };

    // Shared stop flag so the inbound side can wake the writer
    // thread out of its `recv_timeout` loop on client disconnect.
    let stop = Arc::new(AtomicBool::new(false));

    let writer_request_id = request_id.clone();
    let writer_session_uid = session_uid.clone();
    let writer_stop = stop.clone();
    let writer_last_exit = last_exit.clone();
    let writer_handle = std::thread::Builder::new()
        .name(format!("cm-attach-out-{}", session_uid))
        .spawn(move || {
            run_outbound(
                write_socket,
                fanout_rx,
                &writer_request_id,
                &writer_session_uid,
                writer_stop,
                writer_last_exit,
            );
        });
    let writer_handle = match writer_handle {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!(
                "cm-daemon: attach stream {} writer-thread spawn failed: {}",
                session_uid, e,
            );
            None
        }
    };

    // Run the inbound loop on this thread. Returns on EOF or
    // unrecoverable read error.
    run_inbound(stream, state, &session_uid);

    // Inbound has returned — client disconnected or read errored.
    // Signal the writer thread to exit on its next tick, and
    // shut down both halves of the socket so any in-flight write
    // returns immediately. shutdown(Both) is idempotent if the
    // client already closed; if the writer thread already exited
    // (e.g. child exited and End frame went out), it's a no-op.
    stop.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(std::net::Shutdown::Both);

    if let Some(h) = writer_handle {
        let _ = h.join();
    }
}

/// 10e-b r1: production heartbeat interval for `manifest.watch`
/// streams. The handler issues a `StreamKind::Heartbeat` frame on
/// every `recv_timeout` boundary; the write attempt's success or
/// failure is the liveness probe — a disconnected client surfaces
/// as `BrokenPipe` on the next heartbeat and the handler exits,
/// freeing the subscriber slot.
///
/// 15s bounds the dead-handler-accumulation window to roughly
/// (interval + RTT) per disconnected client. Smaller values
/// detect faster at the cost of constant socket traffic for idle
/// streams; 15s is the same order as cgroup-OOM polling and feels
/// like a reasonable default.
///
/// 10e-b r3 (test-isolation fix): the interval is now a
/// PER-HANDLE field on `ManifestWatchHandle::heartbeat_interval`,
/// not a process-global atomic. Tests construct the handle
/// directly with a short value to exercise the idle-disconnect
/// detection path quickly; production uses this constant via the
/// dispatcher. Avoids the test-flake surface from concurrent
/// override/restore of a shared atomic.
pub const DEFAULT_MANIFEST_WATCH_HEARTBEAT_MICROS: u64 = 15_000_000;

/// 11b: stream handler for `events.subscribe`. One-way (daemon →
/// client). Mirror of [`handle_manifest_watch_stream`].
///
/// Loop:
///   - Write one `WorkflowEventStateSnapshot` frame per active
///     run captured under the dispatch lock.
///   - `recv_timeout(heartbeat_interval)` on `event_rx`:
///     - `Ok(event)` → write `WorkflowEvent` frame; on write
///       error (BrokenPipe), exit.
///     - `Err(Timeout)` → write a `Heartbeat` frame; on write
///       error, exit. Same 10e-b r1 idle-disconnect path.
///     - `Err(Disconnected)` → broadcaster reaped our sender
///       (daemon shutdown or slow-subscriber retain). Exit.
///
/// On exit, the receiver drops at end-of-scope; the RAII guard
/// reaps the subscriber slot immediately.
pub fn handle_events_subscribe_stream(
    stream: &mut UnixStream,
    handle: crate::control::dispatch::EventsSubscribeHandle,
) {
    let crate::control::dispatch::EventsSubscribeHandle {
        initial_snapshots,
        event_rx,
        guard: _guard,
        heartbeat_interval,
        request_id,
    } = handle;

    // Events stream is long-lived. Clear any read timeout the
    // RPC path may have set.
    let _ = stream.set_read_timeout(None);

    // Snapshot frames: one per active run. Sent before any live
    // diff frames so the consumer has a baseline.
    for snapshot in &initial_snapshots {
        let frame = StreamFrame::workflow_event_state_snapshot(
            request_id.clone(),
            snapshot.clone(),
        );
        if let Err(e) = wire::write_stream_frame(stream, &frame) {
            eprintln!(
                "cm-daemon: events.subscribe snapshot write failed: {} \
                 (client likely disconnected before we got started)",
                e,
            );
            return;
        }
    }

    // Live loop with heartbeat-on-timeout.
    loop {
        match event_rx.recv_timeout(heartbeat_interval) {
            Ok(event) => {
                let payload = match serde_json::to_value(&event) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "cm-daemon: events.subscribe event serialize \
                             failed: {} (event id {:?}) — dropping \
                             subscriber",
                            e, event.id,
                        );
                        return;
                    }
                };
                let frame = StreamFrame::workflow_event(
                    request_id.clone(),
                    payload,
                );
                if let Err(e) = wire::write_stream_frame(stream, &frame) {
                    eprintln!(
                        "cm-daemon: events.subscribe event write \
                         error: {} (client disconnected)",
                        e,
                    );
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let frame = StreamFrame::heartbeat(request_id.clone());
                if let Err(e) = wire::write_stream_frame(stream, &frame) {
                    eprintln!(
                        "cm-daemon: events.subscribe heartbeat write \
                         error: {} (idle client disconnected)",
                        e,
                    );
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return;
            }
        }
    }
}

/// 10e-b: stream handler for `manifest.watch`. One-way (daemon →
/// client), so simpler than `handle_attach_stream` (no inbound
/// thread, no Input/Resize dispatch).
///
/// Loop:
///   - Write initial `ManifestSnapshot` frame.
///   - `recv_timeout(MANIFEST_WATCH_HEARTBEAT_MICROS)` on `diff_rx`:
///     - `Ok(diff)` → write `ManifestDiff` frame; on write error
///       (BrokenPipe), exit.
///     - `Err(Timeout)` → write a `Heartbeat` frame; on write
///       error, exit. This is the 10e-b r1 idle-disconnect fix —
///       quiet streams previously left a parked handler thread
///       per disconnected client.
///     - `Err(Disconnected)` → broadcaster reaped our sender
///       (daemon shutdown or slow-subscriber retain). Exit.
///
/// On exit, the receiver drops at end-of-scope and the
/// broadcaster's next `try_send` returns `Disconnected` →
/// `retain` reaps the SyncSender. No explicit unsubscribe API.
///
/// Heartbeat detection bound: ~`MANIFEST_WATCH_HEARTBEAT_MICROS`
/// after client disconnect, the handler exits AND the next
/// broadcast or heartbeat reap removes its slot.
pub fn handle_manifest_watch_stream(
    stream: &mut UnixStream,
    handle: crate::control::dispatch::ManifestWatchHandle,
) {
    let crate::control::dispatch::ManifestWatchHandle {
        initial_snapshot,
        diff_rx,
        // 10e-b r2: hold the guard until end-of-scope. Its Drop
        // reaps the subscriber slot from the broadcaster's map.
        // `_guard` binds so the value isn't dropped early (a
        // bare `_` discard would Drop immediately).
        guard: _guard,
        // 10e-b r3: per-handle interval (was process-global
        // atomic). The handler reads it once and uses for every
        // `recv_timeout` iteration.
        heartbeat_interval,
        request_id,
    } = handle;

    // Manifest stream is long-lived. Clear the read timeout the
    // RPC path may have set so an idle subscriber doesn't drop.
    let _ = stream.set_read_timeout(None);

    // Frame 1: initial snapshot. Sent before any diff frames so
    // the consumer has a baseline to apply diffs onto.
    let snapshot_frame = StreamFrame::manifest_snapshot(
        request_id.clone(),
        initial_snapshot,
    );
    if let Err(e) = wire::write_stream_frame(stream, &snapshot_frame) {
        eprintln!(
            "cm-daemon: manifest.watch initial snapshot write failed: {} \
             (client likely disconnected before we got started)",
            e,
        );
        return;
    }

    // Frame loop with heartbeat-on-timeout. Each iteration either
    // delivers a real diff OR a heartbeat; both probe the socket
    // for client liveness. Quiet streams get a heartbeat every
    // `heartbeat_interval`, so a disconnected client surfaces
    // within roughly one interval.
    loop {
        match diff_rx.recv_timeout(heartbeat_interval) {
            Ok(diff) => {
                // Variant-tagged JSON serialization
                // (Serialize/Deserialize on ManifestDiff lands the
                // enum on the wire as e.g. `{"Exited": {...}}`).
                let payload = match serde_json::to_value(&diff) {
                    Ok(v) => v,
                    Err(e) => {
                        // Should be impossible — ManifestDiff is
                        // a closed enum of serde-friendly types.
                        // Log loudly so a future variant addition
                        // that breaks serialize surfaces here.
                        eprintln!(
                            "cm-daemon: manifest.watch diff serialize \
                             failed: {} (variant {:?}) — dropping \
                             subscriber",
                            e, diff,
                        );
                        return;
                    }
                };
                let frame =
                    StreamFrame::manifest_diff(request_id.clone(), payload);
                if let Err(e) = wire::write_stream_frame(stream, &frame) {
                    // Typical case: BrokenPipe from client
                    // disconnect. Exit cleanly; our receiver drops
                    // at end-of-scope and the broadcaster will
                    // reap our SyncSender on the next broadcast
                    // via `try_send → Err(Disconnected)`.
                    eprintln!(
                        "cm-daemon: manifest.watch diff write \
                         error: {} (client disconnected)",
                        e,
                    );
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // 10e-b r1 idle-disconnect fix: probe the socket
                // via a heartbeat frame on every quiet interval.
                // If the client has gone away, the write fails
                // BrokenPipe → exit → receiver dropped → next
                // broadcast (or heartbeat scan) reaps the slot.
                //
                // Heartbeat carries no payload; the client
                // interprets it as a no-op. Production interval
                // is `DEFAULT_MANIFEST_WATCH_HEARTBEAT_MICROS`
                // (15s); tests pass their own interval into the
                // `ManifestWatchHandle::heartbeat_interval`
                // field (10e-b r3 — per-handle, not global).
                let frame = StreamFrame::heartbeat(request_id.clone());
                if let Err(e) = wire::write_stream_frame(stream, &frame) {
                    eprintln!(
                        "cm-daemon: manifest.watch heartbeat write \
                         error: {} (idle client disconnected)",
                        e,
                    );
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Broadcaster reaped our sender (daemon shutdown,
                // or `retain` removed us because a prior try_send
                // hit our channel's capacity — slow-subscriber
                // drop per 10e-b §5 R2). No more diffs incoming.
                return;
            }
        }
    }
}

/// Outbound thread body: drain the fanout, encode each chunk as a
/// `Data` frame, write to the socket. Exit on:
///   - `Disconnected` from the fanout (child exited / session
///     removed) → write a final `End` frame and return.
///   - Write error (typically BrokenPipe from client disconnect)
///     → return without writing End; the client has already
///     dropped the stream.
fn run_outbound(
    mut write_socket: UnixStream,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    request_id: &str,
    session_uid: &str,
    stop: Arc<AtomicBool>,
    last_exit: SharedLastExit,
) {
    loop {
        // Check the stop flag every iteration so the writer
        // thread can exit promptly when the inbound side signals
        // client-disconnect (slice-10c-e-2 review fix: without
        // this the writer polled forever on pure-client-close).
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match rx.recv_timeout(FANOUT_RECV_TICK) {
            Ok(chunk) => {
                let frame = StreamFrame::data(
                    request_id,
                    serde_json::json!({
                        "bytes": BASE64.encode(&chunk),
                    }),
                );
                if let Err(e) =
                    wire::write_stream_frame(&mut write_socket, &frame)
                {
                    eprintln!(
                        "cm-daemon: attach stream {} outbound write error: {} (client disconnected)",
                        session_uid, e,
                    );
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Liveness tick — loop, recheck stop flag, re-poll.
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Fanout closed cleanly. Build End-frame payload
                // from the reaper-recorded kernel exit + a
                // consume-time kill-log scan (slice-10c-e-2
                // review-6 fix: lazy `memory_cap_kill`
                // classification). The fanout closing means the
                // reader thread saw EOF on the PTY master, which
                // means the child exited — `build_end_payload`
                // spins briefly waiting for `waitpid` to return
                // and populate the kernel-exit slot.
                let payload = build_end_payload(&last_exit);
                let _ = wire::write_stream_frame(
                    &mut write_socket,
                    &StreamFrame::end(request_id, payload),
                );
                // Slice-10c-e-2 review-6 fix #2: wake the inbound
                // thread immediately. Without this, the inbound's
                // `read_stream_frame` keeps blocking on the
                // socket until the CLIENT closes it — which may
                // be much later than the daemon writing End.
                // Setting `stop` + shutting down the socket
                // forces the inbound side to exit on its next
                // read attempt.
                stop.store(true, Ordering::SeqCst);
                let _ = write_socket.shutdown(std::net::Shutdown::Both);
                return;
            }
        }
    }
}

/// Build the End-frame payload (`{exit_code, memory_cap_kill}`).
/// Slice-10c-e-2 review-6 fix: `memory_cap_kill` is classified
/// HERE, at consume time, by scanning the kill log since the
/// per-spawn baseline — not cached by the reaper at waitpid time.
/// That closes the race where the cgroup-OOM writer's record
/// landed after the reaper's snapshot.
///
/// Spins briefly (≤1s) waiting for the reaper to populate the
/// kernel-exit slot if it isn't ready yet — the typical race
/// window is "reader EOF observed → fanout.close() → outbound
/// sees Disconnected" all before the reaper's waitpid syscall
/// returns. After the timeout we emit a degraded payload with
/// `exit_code: null, memory_cap_kill: false` rather than holding
/// the stream open indefinitely.
fn build_end_payload(last_exit: &SharedLastExit) -> serde_json::Value {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(1);
    while std::time::Instant::now() < deadline {
        if last_exit.kernel_set() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Snapshot reads kernel exit AND scans the kill log NOW.
    // Both halves are computed at this moment, so a kill record
    // that landed any time between spawn and now is observable.
    let (code, memory_cap_kill) = last_exit.snapshot();
    serde_json::json!({
        "exit_code": code,
        "memory_cap_kill": memory_cap_kill,
    })
}

/// Inbound loop body: read frames from the socket, dispatch by
/// kind. Runs on the calling thread (no thread spawn for the
/// inbound side). Returns on EOF or unrecoverable read error.
fn run_inbound(
    stream: &mut UnixStream,
    state: Arc<Mutex<DaemonState>>,
    session_uid: &str,
) {
    loop {
        let frame = match wire::read_stream_frame(stream) {
            Ok(Some(f)) => f,
            Ok(None) => return, // clean EOF — client disconnected
            Err(e) => {
                // BrokenPipe / ConnectionReset are normal
                // disconnects; log everything else louder.
                if !matches!(
                    e.kind(),
                    std::io::ErrorKind::UnexpectedEof
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::ConnectionReset
                ) {
                    eprintln!(
                        "cm-daemon: attach stream {} inbound read error: {}",
                        session_uid, e,
                    );
                }
                return;
            }
        };

        match frame.kind {
            StreamKind::Input => {
                let Some(b64) = frame.payload.get("bytes").and_then(|v| v.as_str()) else {
                    eprintln!(
                        "cm-daemon: attach stream {} inbound Input frame missing 'bytes' field",
                        session_uid
                    );
                    continue;
                };
                let decoded = match BASE64.decode(b64) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!(
                            "cm-daemon: attach stream {} inbound Input frame bad base64: {}",
                            session_uid, e
                        );
                        continue;
                    }
                };
                // Per-frame cap to match the JSON-RPC `send_input`
                // path (slice 10c-e-3b-fix3). A single oversized
                // frame is logged + skipped — DON'T tear down the
                // attach stream; a malformed/oversized paste
                // shouldn't kill the operator's session. Clients
                // that need to send >64 KiB chunk it (alacritty
                // already calls `write()` in chunks naturally).
                if decoded.len() > crate::control::methods::MAX_SEND_INPUT_BYTES {
                    eprintln!(
                        "cm-daemon: attach stream {} dropped {}-byte Input frame (cap is {} bytes); \
                         clients must chunk larger pastes",
                        session_uid,
                        decoded.len(),
                        crate::control::methods::MAX_SEND_INPUT_BYTES,
                    );
                    continue;
                }
                // Slice 10c-e-3b-fix3 deadlock fix + sub-2b-1
                // review-r#3 #2: clone the centralized
                // `InputHandle` (writer + activity Arcs) out of
                // state under the state lock, THEN drop the
                // state lock, THEN do the blocking write +
                // activity stamp through the handle. Pre-fix
                // (r#3) this path cloned only the writer Arc
                // and skipped the activity stamp — so an
                // operator typing through the attach stream
                // (the primary input path for daemon-attached
                // sessions) didn't bump idle, and
                // `wait_for_session_idle` returned immediately
                // after typing.
                let handle = {
                    let s = state.lock().unwrap_or_else(|p| p.into_inner());
                    s.sessions
                        .get(session_uid)
                        .map(|sess| sess.input_handle())
                };
                let Some(handle) = handle else {
                    // Session removed mid-stream. Drop this
                    // input; the outbound side will see the
                    // fanout close and write End shortly.
                    eprintln!(
                        "cm-daemon: attach stream {} input received for removed session",
                        session_uid
                    );
                    continue;
                };
                // State lock is dropped. Do the actual write +
                // stamp via the handle's cloned Arcs.
                if let Err(e) = handle.write_and_stamp(&decoded) {
                    eprintln!(
                        "cm-daemon: attach stream {} send_input failed: {} (session may have exited)",
                        session_uid, e,
                    );
                    // Don't return — the outbound side will
                    // observe fanout close and surface End.
                    // We just stop forwarding more input until
                    // the client gives up.
                }
            }
            StreamKind::Resize => {
                let cols = frame
                    .payload
                    .get("cols")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u16);
                let rows = frame
                    .payload
                    .get("rows")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u16);
                let (Some(c), Some(r)) = (cols, rows) else {
                    eprintln!(
                        "cm-daemon: attach stream {} inbound Resize frame missing cols/rows",
                        session_uid
                    );
                    continue;
                };
                let mut s = state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(session) = s.sessions.get_mut(session_uid) {
                    if let Err(e) = session.resize(c, r) {
                        eprintln!(
                            "cm-daemon: attach stream {} resize {}x{} failed: {}",
                            session_uid, c, r, e
                        );
                    }
                }
            }
            other => {
                // Server-only kinds (Data / End / Error) from the
                // client are wire-protocol bugs but not fatal.
                eprintln!(
                    "cm-daemon: attach stream {} unexpected inbound kind {:?}",
                    session_uid, other
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{DaemonSession, SpawnParams};
    use std::io::Read as _;
    use std::io::Write as _;

    /// Encode one length-prefixed StreamFrame into bytes for a
    /// test's synthetic "client send."
    fn encode_frame(frame: &StreamFrame) -> Vec<u8> {
        let body = serde_json::to_vec(frame).unwrap();
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    /// Build the [`AttachStreamHandle`] that the dispatcher arm
    /// produces, by subscribing to a session that's already been
    /// inserted into `state.sessions`. Mirrors the locked-section
    /// shape `dispatch_attach_open` uses in production.
    fn build_handle(
        state: &Arc<Mutex<DaemonState>>,
        session_uid: &str,
        request_id: &str,
    ) -> AttachStreamHandle {
        let s = state.lock().unwrap();
        let session = s
            .sessions
            .get(session_uid)
            .expect("session must be in registry for handle build");
        AttachStreamHandle {
            session_uid: session_uid.to_string(),
            fanout_rx: session.fanout.subscribe(),
            last_exit: session.last_exit.clone(),
            request_id: request_id.to_string(),
        }
    }

    #[test]
    fn inbound_input_frame_invokes_send_input_on_daemon_session() {
        // The named acceptance for the bidirectional fix: the
        // client sends a `StreamKind::Input` frame with base64
        // bytes; the daemon's read thread decodes it and calls
        // `DaemonSession::send_input` against the bound session.
        //
        // We verify by spawning `/bin/cat` (echoes its stdin),
        // subscribing to the fanout, sending an Input frame with
        // "hello-input\n", and observing the bytes echoed back
        // through the fanout.
        let params = SpawnParams::new("ts-input", "cat-test", "/bin/cat");
        let session = DaemonSession::spawn(params).expect("spawn cat");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-input".into(), session);
        }
        // Subscribe BEFORE attach so we don't race on missing
        // early bytes.
        let rx = state
            .lock()
            .unwrap()
            .sessions
            .get("ts-input")
            .unwrap()
            .fanout
            .subscribe();

        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            {
                let handle = build_handle(&state_clone, "ts-input", "req-input");
                handle_attach_stream(&mut server, state_clone, handle);
            };
        });

        // Client sends an Input frame.
        let input_frame = StreamFrame {
            id: "req-input".into(),
            kind: StreamKind::Input,
            payload: serde_json::json!({
                "bytes": BASE64.encode(b"hello-input\n"),
            }),
        };
        client
            .write_all(&encode_frame(&input_frame))
            .expect("client write input");

        // Within bounded time, the cat session echoes back via
        // the fanout we subscribed to.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut accumulated = Vec::new();
        loop {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    if accumulated.windows(11).any(|w| w == b"hello-input") {
                        // Cleanup: drop client (inbound EOF →
                        // shutdown → writer exits → handle joins).
                        drop(client);
                        let _ = handle.join();
                        state.lock().unwrap().sessions.remove("ts-input");
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        // Cleanup before failing.
        drop(client);
        let _ = handle.join();
        state.lock().unwrap().sessions.remove("ts-input");
        panic!(
            "did not observe Input bytes echoed back through PTY within 3s; got:\n{}",
            String::from_utf8_lossy(&accumulated)
        );
    }

    #[test]
    fn inbound_resize_frame_invokes_resize_on_daemon_session() {
        // Resize frames must reach DaemonSession::resize. We
        // verify by sending a resize and observing the call
        // doesn't panic — the daemon-side resize is best-effort
        // (ioctl TIOCSWINSZ); a deeper test would round-trip
        // through `stty size` but that's flaky on test runners
        // without a real tty.
        let params = SpawnParams::new("ts-resize", "cat", "/bin/cat");
        let session = DaemonSession::spawn(params).expect("spawn cat");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-resize".into(), session);
        }

        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            {
                let handle = build_handle(&state_clone, "ts-resize", "req-resize");
                handle_attach_stream(&mut server, state_clone, handle);
            };
        });

        let resize_frame = StreamFrame {
            id: "req-resize".into(),
            kind: StreamKind::Resize,
            payload: serde_json::json!({ "cols": 120u16, "rows": 40u16 }),
        };
        client
            .write_all(&encode_frame(&resize_frame))
            .expect("client write resize");

        // Allow the daemon a moment to process the resize, then
        // shut down cleanly.
        std::thread::sleep(Duration::from_millis(100));
        drop(client);
        let _ = handle.join();
        state.lock().unwrap().sessions.remove("ts-resize");
        // No panic = pass. The ioctl actually firing is verified
        // indirectly: a missing or broken resize would surface in
        // the manual smoke test in slice 10c-e-3.
    }

    // ===== Slice 10c-e-3b-fix3: per-session writer mutex + cap =====

    #[test]
    fn inbound_input_frame_over_cap_is_skipped_stream_survives() {
        // Reviewer's named test: send a 100 KiB Input frame to
        // exercise the 64 KiB cap. The oversized frame is
        // logged + skipped — DO NOT tear down the stream (a
        // single oversized paste shouldn't kill the operator's
        // session). Then send a 16 KiB frame and confirm it
        // goes through.
        let params = SpawnParams::new("ts-cap", "cat-cap", "/bin/cat");
        let session = DaemonSession::spawn(params).expect("spawn cat");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-cap".into(), session);
        }
        let rx = state
            .lock()
            .unwrap()
            .sessions
            .get("ts-cap")
            .unwrap()
            .fanout
            .subscribe();

        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            let h = build_handle(&state_clone, "ts-cap", "req-cap");
            handle_attach_stream(&mut server, state_clone, h);
        });

        // Oversized frame: 100 KiB > 64 KiB cap. The cap
        // rejection logs + skips; the X's MUST NOT echo via cat.
        let oversized = vec![b'X'; 100 * 1024];
        let big_frame = StreamFrame {
            id: "req-cap".into(),
            kind: StreamKind::Input,
            payload: serde_json::json!({ "bytes": BASE64.encode(&oversized) }),
        };
        client
            .write_all(&encode_frame(&big_frame))
            .expect("write big frame");

        // Small marker frame, well under the cap. Must go
        // through because the stream survived the cap
        // rejection. We send `\n` but the PTY echoes back
        // `\r\n` (line-ending conversion is the kernel's
        // default), so the assertion below matches the bare
        // string without the line ending.
        let sent_marker = b"after-cap-marker\n";
        let small_frame = StreamFrame {
            id: "req-cap".into(),
            kind: StreamKind::Input,
            payload: serde_json::json!({ "bytes": BASE64.encode(sent_marker) }),
        };
        let small_marker = b"after-cap-marker";
        client
            .write_all(&encode_frame(&small_frame))
            .expect("write small frame");

        // Read fanout output. Within bound:
        //   - The small frame's marker MUST appear (proving
        //     stream survived the cap rejection AND small input
        //     flows through the writer mutex correctly).
        //   - The oversized frame's X bytes MUST NOT appear in
        //     ANY substantial quantity (the cap rejection
        //     dropped them before write_all).
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut accumulated = Vec::new();
        let mut saw_marker = false;
        while std::time::Instant::now() < deadline {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
                Ok(chunk) => accumulated.extend_from_slice(&chunk),
                Err(_) => {} // poll; keep looping until deadline OR marker seen
            }
            if accumulated
                .windows(small_marker.len())
                .any(|w| w == small_marker)
            {
                saw_marker = true;
                break;
            }
        }

        // Cleanup BEFORE assertions so a failure doesn't leak
        // the handler thread.
        drop(client);
        let _ = handle.join();
        state.lock().unwrap().sessions.remove("ts-cap");

        assert!(
            saw_marker,
            "small frame's marker did not echo back — cap rejection probably tore down the stream. Got {} bytes:\n{:?}",
            accumulated.len(),
            String::from_utf8_lossy(&accumulated[..accumulated.len().min(200)]),
        );
        // The 100 KiB of X's MUST have been cap-rejected, not
        // written to the PTY. We tolerate a tiny smattering of
        // legitimate X's (vanishingly unlikely from cat's
        // output, but be robust). Anything over 1 KiB means the
        // cap broke and the oversized bytes leaked through.
        let x_count = accumulated.iter().filter(|&&b| b == b'X').count();
        assert!(
            x_count < 1024,
            "oversized frame's X bytes leaked through cap rejection ({} bytes of X in fanout output)",
            x_count,
        );
    }

    #[test]
    fn parallel_sessions_do_not_block_each_other_on_writer_contention() {
        // Reviewer's named acceptance for the deadlock fix:
        // session A's writer is held by a slow drain (we simulate
        // by holding A's writer mutex directly from the test
        // thread). Session B's send_input + kill_session via
        // their respective dispatcher paths must still complete
        // because they touch B's writer Arc, not A's. The state
        // mutex is only held briefly during the Arc-clone-out
        // step.
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let p_a = SpawnParams::new("ts-a", "cat-a", "/bin/cat");
        let p_b = SpawnParams::new("ts-b", "cat-b", "/bin/cat");
        let session_a = DaemonSession::spawn(p_a).expect("spawn a");
        let session_b = DaemonSession::spawn(p_b).expect("spawn b");
        let writer_a_clone = Arc::clone(&session_a.writer);
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-a".into(), session_a);
            s.sessions.insert("ts-b".into(), session_b);
        }

        // "Stall" session A by holding its writer mutex on this
        // thread. Any send_input on A would block waiting for
        // it; send_input on B must NOT.
        let _a_guard = writer_a_clone.lock().unwrap();

        // Send input to B via the RPC method handler — uses the
        // same lock-discipline as the production path.
        let params_b = serde_json::json!({
            "session_uid": "ts-b",
            "text": "hello-b",
            "submit": true,
        });
        let bounded = std::time::Duration::from_secs(2);
        let (tx, rx) = std::sync::mpsc::channel();
        let state_for_send = state.clone();
        std::thread::spawn(move || {
            let res = crate::control::methods::send_input(&state_for_send, &params_b, None);
            let _ = tx.send(res);
        });
        let result = rx
            .recv_timeout(bounded)
            .expect("send_input on session B must complete even while A's writer is held");
        assert!(
            result.is_ok(),
            "send_input on session B returned error: {:?}",
            result
        );

        // Kill session B via the RPC. Same bound: must NOT wait
        // for A's writer.
        let kill_params = serde_json::json!({ "session_uid": "ts-b" });
        let (tx, rx) = std::sync::mpsc::channel();
        let state_for_kill = state.clone();
        std::thread::spawn(move || {
            let res = crate::control::methods::kill_session(&state_for_kill, &kill_params, None);
            let _ = tx.send(res);
        });
        let result = rx
            .recv_timeout(bounded)
            .expect("kill_session on session B must complete even while A's writer is held");
        assert!(
            result.is_ok(),
            "kill_session on B returned error: {:?}",
            result
        );

        // Release A's writer mutex.
        drop(_a_guard);

        // Cleanup A.
        let kill_a = serde_json::json!({ "session_uid": "ts-a" });
        let _ = crate::control::methods::kill_session(&state, &kill_a, None);
        // Drain reaper-cleanup callbacks.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if state.lock().unwrap().sessions.is_empty() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn kill_session_completes_even_when_target_writer_is_held() {
        // kill_session goes through pidfd (SIGKILL), not the
        // writer. So even when the SAME session's writer mutex
        // is held by an in-flight write, kill_session completes
        // within bound. This is the second deadlock-class
        // property: a stuck PTY's own send_input shouldn't
        // prevent the operator from killing it.
        let state = Arc::new(Mutex::new(DaemonState::new()));
        let p = SpawnParams::new("ts-self-kill", "cat-self", "/bin/cat");
        let session = DaemonSession::spawn(p).expect("spawn");
        let writer_clone = Arc::clone(&session.writer);
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-self-kill".into(), session);
        }

        // Hold the writer mutex (simulating a stuck PTY write
        // in progress).
        let _guard = writer_clone.lock().unwrap();

        // kill_session must complete within a tight bound
        // regardless of the held writer mutex — the pidfd
        // SIGKILL path doesn't touch the writer.
        let kill_params = serde_json::json!({ "session_uid": "ts-self-kill" });
        let (tx, rx) = std::sync::mpsc::channel();
        let state_for_kill = state.clone();
        std::thread::spawn(move || {
            let res = crate::control::methods::kill_session(&state_for_kill, &kill_params, None);
            let _ = tx.send(res);
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("kill_session must complete even when target's writer mutex is held");
        assert!(result.is_ok(), "kill_session returned error: {:?}", result);
        drop(_guard);
    }

    #[test]
    fn inbound_unexpected_kind_is_logged_but_does_not_terminate_stream() {
        // Defensive case: client sends a kind that's only valid
        // in the server→client direction (e.g. End). The daemon
        // logs and continues — doesn't tear down the stream over
        // a single malformed frame.
        let params = SpawnParams::new("ts-bad", "cat", "/bin/cat");
        let session = DaemonSession::spawn(params).expect("spawn cat");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-bad".into(), session);
        }

        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            {
                let handle = build_handle(&state_clone, "ts-bad", "req-bad");
                handle_attach_stream(&mut server, state_clone, handle);
            };
        });

        // Send a stray End frame from the client (server-only kind).
        let bad_frame = StreamFrame {
            id: "req-bad".into(),
            kind: StreamKind::End,
            payload: serde_json::json!({}),
        };
        client.write_all(&encode_frame(&bad_frame)).expect("write");

        // Now send a legitimate Input frame; the daemon should
        // process it (the stream is still alive).
        let rx = state
            .lock()
            .unwrap()
            .sessions
            .get("ts-bad")
            .unwrap()
            .fanout
            .subscribe();
        let input_frame = StreamFrame {
            id: "req-bad".into(),
            kind: StreamKind::Input,
            payload: serde_json::json!({
                "bytes": BASE64.encode(b"after-bad\n"),
            }),
        };
        client
            .write_all(&encode_frame(&input_frame))
            .expect("write after bad frame");

        // Within bound, cat echoes "after-bad" — proving the
        // stream survived the malformed kind.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut accumulated = Vec::new();
        loop {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    if accumulated.windows(9).any(|w| w == b"after-bad") {
                        drop(client);
                        let _ = handle.join();
                        state.lock().unwrap().sessions.remove("ts-bad");
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        drop(client);
        let _ = handle.join();
        state.lock().unwrap().sessions.remove("ts-bad");
        panic!(
            "stream did NOT survive stray End frame; got:\n{}",
            String::from_utf8_lossy(&accumulated)
        );
    }

    #[test]
    fn outbound_data_frames_arrive_when_session_writes_to_fanout() {
        // Reasserts slice-10c-c's outbound contract under the new
        // bidirectional shape: spawn echo, server writes Data
        // frames carrying the bytes, then End on fanout close.
        let mut params = SpawnParams::new("ts-out", "echo-test", "/bin/echo");
        params.args = vec!["bidir-echo".into()];
        let session = DaemonSession::spawn(params).expect("spawn");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-out".into(), session);
        }

        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            {
                let handle = build_handle(&state_clone, "ts-out", "req-out");
                handle_attach_stream(&mut server, state_clone, handle);
            };
        });

        // Read frames from client side until we see bidir-echo or
        // the stream closes.
        client.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut accumulated_bytes = Vec::new();
        let mut saw_end = false;
        loop {
            let mut prefix = [0u8; 4];
            match client.read_exact(&mut prefix) {
                Ok(()) => {}
                Err(_) => break,
            }
            let len = u32::from_be_bytes(prefix) as usize;
            let mut body = vec![0u8; len];
            if client.read_exact(&mut body).is_err() {
                break;
            }
            let frame: StreamFrame = match serde_json::from_slice(&body) {
                Ok(f) => f,
                Err(_) => continue,
            };
            match frame.kind {
                StreamKind::Data => {
                    let b64 = frame.payload["bytes"].as_str().unwrap();
                    let decoded = BASE64.decode(b64).unwrap();
                    accumulated_bytes.extend(decoded);
                }
                StreamKind::End => {
                    saw_end = true;
                    break;
                }
                _ => {}
            }
            if accumulated_bytes.windows(10).any(|w| w == b"bidir-echo") && saw_end {
                break;
            }
        }
        // Wait for echo to exit + fanout to close so the writer
        // thread sees Disconnected and sends End.
        std::thread::sleep(Duration::from_millis(200));
        // Trigger session removal so the fanout's last sender drops.
        state.lock().unwrap().sessions.remove("ts-out");
        // Continue reading until End.
        if !saw_end {
            loop {
                let mut prefix = [0u8; 4];
                if client.read_exact(&mut prefix).is_err() {
                    break;
                }
                let len = u32::from_be_bytes(prefix) as usize;
                let mut body = vec![0u8; len];
                if client.read_exact(&mut body).is_err() {
                    break;
                }
                let frame: StreamFrame = match serde_json::from_slice(&body) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                if frame.kind == StreamKind::End {
                    saw_end = true;
                    break;
                }
                if let Some(b64) = frame.payload.get("bytes").and_then(|v| v.as_str()) {
                    if let Ok(decoded) = BASE64.decode(b64) {
                        accumulated_bytes.extend(decoded);
                    }
                }
            }
        }
        drop(client);
        let _ = handle.join();
        assert!(
            accumulated_bytes.windows(10).any(|w| w == b"bidir-echo"),
            "expected bidir-echo bytes in outbound data; got:\n{}",
            String::from_utf8_lossy(&accumulated_bytes),
        );
        assert!(saw_end, "stream must end with an End frame after fanout closes");
    }

    #[test]
    fn client_disconnect_winds_down_both_threads_cleanly() {
        // The named coordination contract: when the client
        // disconnects, both inbound and outbound threads exit
        // promptly. handle_attach_stream returns within a bound
        // (no hang).
        let params = SpawnParams::new("ts-dc", "cat", "/bin/cat");
        let session = DaemonSession::spawn(params).expect("spawn");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-dc".into(), session);
        }

        let (client, mut server) = UnixStream::pair().expect("socket pair");
        let state_clone = state.clone();
        let start = std::time::Instant::now();
        let handle = std::thread::spawn(move || {
            {
                let handle = build_handle(&state_clone, "ts-dc", "req-dc");
                handle_attach_stream(&mut server, state_clone, handle);
            };
        });

        // Brief moment so the writer thread starts up + subscribes.
        std::thread::sleep(Duration::from_millis(50));
        // Client disconnects.
        drop(client);
        // handle_attach_stream must return within a bound.
        let res = handle.join();
        assert!(res.is_ok(), "stream thread panicked: {:?}", res);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "handle_attach_stream took {:?} after client disconnect (expected < 3s)",
            start.elapsed()
        );
        state.lock().unwrap().sessions.remove("ts-dc");
    }

    #[test]
    fn session_exit_winds_down_handler_thread_without_waiting_for_client_close() {
        // Slice-10c-e-2 review-6 fix #2: when the session exits
        // and the outbound thread writes End, the inbound thread
        // must also exit promptly — without this, the handler
        // thread blocks on the inbound socket read until the
        // CLIENT closes, leaking the connection thread.
        //
        // Drive a session that exits while attached. Client does
        // NOT close. handle_attach_stream must still return
        // within bound.
        let params = SpawnParams::new("ts-server-end", "true", "/bin/true");
        let session = DaemonSession::spawn(params).expect("spawn");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-server-end".into(), session);
        }

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let state_clone = state.clone();
        let start = std::time::Instant::now();
        let handle = std::thread::spawn(move || {
            let h = build_handle(&state_clone, "ts-server-end", "req-server-end");
            handle_attach_stream(&mut server, state_clone, h);
        });

        // Wait briefly so the daemon-side threads are running,
        // then trigger session removal (fanout close → outbound
        // writes End → outbound signals stop + shutdown).
        std::thread::sleep(Duration::from_millis(150));
        state.lock().unwrap().sessions.remove("ts-server-end");

        // The handler thread must return within a bound EVEN
        // THOUGH `client` is still held open. This is the fix:
        // the outbound thread's End-send branch now shuts down
        // the socket itself, freeing the inbound thread.
        let res = handle.join();
        assert!(res.is_ok(), "handler thread panicked: {:?}", res);
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "handle_attach_stream took {:?} after session exit (expected < 3s; \
             client never closed — the fix is the outbound thread shutting \
             down the socket after End)",
            start.elapsed()
        );
        // Sanity: client should have observed the End frame on
        // the wire (server's last write before shutdown).
        client.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let mut probe = [0u8; 4];
        let _ = client.read_exact(&mut probe); // best-effort; we already proved the thread exited
    }

    // ==========================================================
    // End-frame payload tests (slice-10c-e-2 review-2 fix #2)
    //
    // Verify that the End frame the daemon emits when the
    // fanout closes carries the right `{exit_code, memory_cap_kill}`
    // shape — the contract the TUI's `term_shim::StreamReader`
    // decodes into a `ChildEvent::Exited`. Three cases per the
    // reviewer's spec:
    //   - clean `/bin/true` exit → `exit_code: Some(0), memory_cap_kill: false`
    //   - SIGKILL via /bin/sleep + kill → `exit_code: None, memory_cap_kill: false`
    //     (no kills_dir configured for this test — daemon falls back to false)
    //   - SIGKILL + kills_dir with a baseline-relative record →
    //     `exit_code: None, memory_cap_kill: true`
    //   - SIGKILL + kills_dir with a STALE (pre-baseline) record →
    //     `exit_code: None, memory_cap_kill: false` (baseline-isolated)
    // ==========================================================

    /// Drain wire frames from `client` until an End frame appears
    /// (or the deadline elapses). Returns the End frame's payload
    /// as serde_json::Value for the test to assert on.
    fn drain_to_end_frame(
        client: &mut UnixStream,
    ) -> serde_json::Value {
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        loop {
            let mut prefix = [0u8; 4];
            client.read_exact(&mut prefix).expect("read prefix");
            let len = u32::from_be_bytes(prefix) as usize;
            let mut body = vec![0u8; len];
            client.read_exact(&mut body).expect("read body");
            let frame: StreamFrame =
                serde_json::from_slice(&body).expect("decode");
            if frame.kind == StreamKind::End {
                return frame.payload;
            }
            // Skip Data frames silently.
        }
    }

    #[test]
    fn end_frame_for_clean_exit_carries_exit_code_zero_and_no_cap_kill() {
        // Spawn /bin/true (exits 0 immediately), attach a client,
        // remove the session from the registry so the fanout's
        // last sender drops, observe End frame:
        // `exit_code: Some(0), memory_cap_kill: false`.
        let params = SpawnParams::new("ts-clean-exit", "true-test", "/bin/true");
        let session = DaemonSession::spawn(params).expect("spawn /bin/true");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-clean-exit".into(), session);
        }

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            let h = build_handle(&state_clone, "ts-clean-exit", "req-clean");
            handle_attach_stream(&mut server, state_clone, h);
        });

        // Wait for /bin/true to exit + reader to see EOF + close
        // the fanout. The reaper records the exit ~immediately.
        std::thread::sleep(Duration::from_millis(200));
        // Remove session from registry so the writer's
        // recv_timeout sees Disconnected promptly.
        state.lock().unwrap().sessions.remove("ts-clean-exit");

        let payload = drain_to_end_frame(&mut client);
        assert_eq!(payload["exit_code"], 0, "clean exit must surface exit_code=0");
        assert_eq!(
            payload["memory_cap_kill"], false,
            "clean exit must NOT flag memory_cap_kill"
        );
        drop(client);
        let _ = handle.join();
    }

    #[test]
    fn end_frame_for_signal_kill_with_no_kills_dir_carries_none_and_no_cap_kill() {
        // Spawn /bin/sh -c 'kill -9 $$' (self-SIGKILL). No
        // kills_dir set, so memory_cap_kill defaults to false.
        // exit_code is None (signal-kill).
        let mut params = SpawnParams::new("ts-sigkill", "sh-test", "/bin/sh");
        params.args = vec!["-c".into(), "kill -9 $$".into()];
        let session = DaemonSession::spawn(params).expect("spawn sh");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-sigkill".into(), session);
        }

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            let h = build_handle(&state_clone, "ts-sigkill", "req-sigkill");
            handle_attach_stream(&mut server, state_clone, h);
        });

        std::thread::sleep(Duration::from_millis(200));
        state.lock().unwrap().sessions.remove("ts-sigkill");

        let payload = drain_to_end_frame(&mut client);
        assert!(
            payload["exit_code"].is_null(),
            "signal kill must surface exit_code: null (got {:?})",
            payload["exit_code"]
        );
        assert_eq!(payload["memory_cap_kill"], false);
        drop(client);
        let _ = handle.join();
    }

    #[test]
    fn end_frame_for_signal_kill_with_baseline_relative_record_carries_cap_kill_true() {
        // Named acceptance: a SIGKILL with a fresh memory-kill
        // record (past the per-spawn baseline) must surface
        // memory_cap_kill: true via the End frame.
        //
        // Test ordering (slice-10c-e-2 review-6 lessons learned):
        //   1. Spawn a LONG-RUNNING child (/bin/sleep 30) — keeps
        //      the fanout open while we set up the kill record.
        //      A fast-exit child (sh -c 'kill -9 $$') would have
        //      the outbound thread fire build_end_payload before
        //      the test's write_all lands — a race the prior
        //      version of this test hit on reviewer hardware.
        //   2. Subscribe + start handle_attach_stream (outbound
        //      thread sits on the fanout recv_timeout loop).
        //   3. Write the kill record. File now has the record;
        //      the lazy probe at consume time will see it.
        //   4. Remove the session — triggers Drop → SIGKILL via
        //      pidfd → reader EOF → fanout.close() → outbound
        //      sees Disconnected → build_end_payload scans the
        //      kill log NOW (record is there) → memory_cap_kill:
        //      true.
        let tmp = tempfile::TempDir::new().unwrap();
        let kills_dir = tmp.path().to_path_buf();
        let uid = "ts-cap-kill";

        let mut params = SpawnParams::new(uid, "sleep-cap-test", "/bin/sleep");
        params.args = vec!["30".into()];
        params.kills_dir = Some(kills_dir.clone());
        let session = DaemonSession::spawn(params).expect("spawn sleep");

        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert(uid.into(), session);
        }

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let state_clone = state.clone();
        let uid_owned = uid.to_string();
        let handle = std::thread::spawn(move || {
            let h = build_handle(&state_clone, &uid_owned, "req-cap-kill");
            handle_attach_stream(&mut server, state_clone, h);
        });

        // Brief moment so the handler thread + outbound thread
        // are running and subscribed to the (still-open) fanout.
        std::thread::sleep(Duration::from_millis(50));

        // Inject the kill record while the child is still alive.
        // The lazy probe at End-frame emission time will scan
        // the kill log AND find this record past-baseline.
        let record = format!(
            r#"{{"ts":1700000000,"session_uid":"{}","pid":12345,"comm":"sleep","argc":2,"argv_sha256_prefix":"deadbeef","rss_kb":1024,"soft_cap_bytes":104857600,"hard_cap_bytes":209715200}}
"#,
            uid
        );
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(kills_dir.join(format!("{}.jsonl", uid)))
            .expect("open kill log for append");
        f.write_all(record.as_bytes()).expect("append");
        drop(f);

        // Trigger session exit (Drop → SIGKILL → reader EOF →
        // fanout close → outbound → build_end_payload scans kill
        // log NOW → memory_cap_kill: true). The lazy classification
        // is the load-bearing piece: the kill log scan happens
        // INSIDE build_end_payload, not in the reaper closure.
        state.lock().unwrap().sessions.remove(uid);

        let payload = drain_to_end_frame(&mut client);
        assert!(
            payload["exit_code"].is_null(),
            "SIGKILL must surface exit_code: null"
        );
        assert_eq!(
            payload["memory_cap_kill"], true,
            "kill record past baseline must flag memory_cap_kill: true \
             via lazy classification. Got payload: {}",
            payload
        );
        drop(client);
        let _ = handle.join();
    }

    #[test]
    fn lazy_classification_picks_up_records_landing_between_waitpid_and_end_frame() {
        // Direct unit test of the lazy-classification contract.
        // Drives `LastExitProbe::snapshot` against the kill log
        // at two different moments to prove the scan happens at
        // read time, not at `set_kernel` time.
        use crate::session::{KernelExitStatus, LastExitProbe};
        use std::io::Write as _;

        let tmp = tempfile::TempDir::new().unwrap();
        let kills_dir = tmp.path().to_path_buf();
        let uid = "ts-lazy";

        // Touch the file so the baseline can read the size.
        crate::path::ensure_dot_cm_subdir(&kills_dir).unwrap();
        let log_path = kills_dir.join(format!("{}.jsonl", uid));
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        // Baseline is 0 (empty file). Probe acts as if anything
        // present is past-baseline.

        let probe = std::sync::Arc::new(LastExitProbe::new(
            uid.into(),
            Some(kills_dir.clone()),
            0,
        ));

        // Step 1: set kernel exit. This is what the reaper does.
        // Signal-killed exit (no WEXITSTATUS); we use signal=9
        // to model a SIGKILL — what slice 10d watcher-fix #1.5
        // requires for the kill-record + signal join to flag
        // memory_cap_kill once a fresh kill record exists.
        probe.set_kernel(KernelExitStatus {
            code: None,
            signal: Some(9),
        });

        // Step 2: snapshot now — no kill record present, so
        // memory_cap_kill must be false.
        let (code1, mck1) = probe.snapshot();
        assert_eq!(code1, None);
        assert!(!mck1, "no kill record → memory_cap_kill: false");

        // Step 3: write a kill record AFTER set_kernel. Under
        // the OLD code (cache at waitpid time) this would never
        // reach the End frame. Under lazy classification, the
        // NEXT snapshot picks it up.
        let record = format!(
            r#"{{"ts":1700000000,"session_uid":"{}","pid":1,"comm":"x","argc":1,"argv_sha256_prefix":"a","rss_kb":1,"soft_cap_bytes":1,"hard_cap_bytes":1}}
"#,
            uid
        );
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap();
        f.write_all(record.as_bytes()).unwrap();
        drop(f);

        // Step 4: snapshot again. Lazy classification reads the
        // kill log NOW → memory_cap_kill: true.
        let (code2, mck2) = probe.snapshot();
        assert_eq!(code2, None);
        assert!(
            mck2,
            "kill record written after set_kernel must be visible \
             to the next snapshot() — that's the lazy-classification contract"
        );
    }

    #[test]
    fn end_frame_with_stale_pre_baseline_record_keeps_cap_kill_false() {
        // Baseline-isolation contract (slice 10c-b cleanup): a
        // pre-existing kill record from a *previous* incarnation
        // of the same uid must NOT contaminate the current run's
        // exit. We:
        //   1. Pre-populate the kill log with a stale record.
        //   2. Spawn (captures baseline = stale.len()).
        //   3. SIGKILL via /bin/sh.
        //   4. Observe End frame: memory_cap_kill: false.
        let tmp = tempfile::TempDir::new().unwrap();
        let kills_dir = tmp.path().to_path_buf();
        let uid = "ts-stale-only";
        let log_path = kills_dir.join(format!("{}.jsonl", uid));

        let stale = format!(
            r#"{{"ts":1500000000,"session_uid":"{}","pid":99999,"comm":"old","argc":1,"argv_sha256_prefix":"feedface","rss_kb":2048,"soft_cap_bytes":104857600,"hard_cap_bytes":209715200}}
"#,
            uid
        );
        std::fs::write(&log_path, &stale).expect("seed stale record");

        // Spawn now — `capture_baseline_for_spawn` reads the
        // post-stale file size as baseline.
        let mut params = SpawnParams::new(uid, "sh-stale-test", "/bin/sh");
        params.args = vec!["-c".into(), "kill -9 $$".into()];
        params.kills_dir = Some(kills_dir.clone());
        let session = DaemonSession::spawn(params).expect("spawn sh");

        // Do NOT append any post-baseline record. The exit is
        // signal-9 but no fresh kill — memory_cap_kill must be
        // false.
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert(uid.into(), session);
        }

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let state_clone = state.clone();
        let uid_owned = uid.to_string();
        let handle = std::thread::spawn(move || {
            let h = build_handle(&state_clone, &uid_owned, "req-stale");
            handle_attach_stream(&mut server, state_clone, h);
        });

        std::thread::sleep(Duration::from_millis(300));
        state.lock().unwrap().sessions.remove(uid);

        let payload = drain_to_end_frame(&mut client);
        assert!(payload["exit_code"].is_null());
        assert_eq!(
            payload["memory_cap_kill"], false,
            "stale pre-baseline record must NOT flag memory_cap_kill on current exit (got payload {})",
            payload
        );
        drop(client);
        let _ = handle.join();
    }

    // ==========================================================
    // TOCTOU contract test (slice-10c-e-2 review-2 fix #1)
    //
    // Verifies that a session exiting between the dispatcher's
    // ticket consume and the client's stream read produces a
    // clean End frame with the real exit info — not a generic
    // stream error against a dead handle.
    // ==========================================================

    #[test]
    fn session_exiting_between_attach_open_and_stream_start_yields_end_frame() {
        // Spawn a session, build the handle (mirroring what the
        // dispatcher does inside the lock), then BEFORE invoking
        // handle_attach_stream remove the session from the
        // registry — simulating "child exited between attach.open
        // OK and handle_attach_stream start." The pre-built
        // subscription is held; on producer close (which fires as
        // the session's Drop closes the fanout via the reader
        // thread) the subscriber sees Disconnected and emits End.
        let params = SpawnParams::new("ts-toctou", "true-test", "/bin/true");
        let session = DaemonSession::spawn(params).expect("spawn");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        {
            let mut s = state.lock().unwrap();
            s.sessions.insert("ts-toctou".into(), session);
        }

        // Build the handle while session is live (dispatcher
        // semantics).
        let attach_handle = build_handle(&state, "ts-toctou", "req-toctou");

        // Now remove the session from the registry. Its Drop
        // sends SIGKILL via pidfd; reader thread sees EOF; fanout
        // closes; our held subscription sees Disconnected.
        state.lock().unwrap().sessions.remove("ts-toctou");

        let (mut client, mut server) = UnixStream::pair().unwrap();
        let state_clone = state.clone();
        let handle = std::thread::spawn(move || {
            handle_attach_stream(&mut server, state_clone, attach_handle);
        });

        // Read frames until we see End. The contract: a clean End
        // frame fires, NOT a stream-error frame against the dead
        // handle.
        let payload = drain_to_end_frame(&mut client);
        // exit_code is None (signal kill from Drop's SIGKILL).
        assert!(
            payload.get("exit_code").is_some(),
            "End frame payload must carry exit_code field (got {})",
            payload
        );
        // memory_cap_kill is false (no kills_dir was set).
        assert_eq!(payload["memory_cap_kill"], false);
        drop(client);
        let _ = handle.join();
    }

    // =================================================================
    // 10e-b: manifest.watch streaming tests (T3-T9 from the plan)
    // =================================================================
    //
    // These tests exercise `handle_manifest_watch_stream` end-to-end
    // through `UnixStream::pair`. The handler is driven on one end,
    // the test reads frames from the other. No real cgroup-OOM needed
    // — we drive `state.manifest_watcher.broadcast(...)` directly to
    // simulate what `handle_session_exit` does in production.

    /// Helper: read one StreamFrame from a UnixStream with a bounded
    /// timeout. Panics if the read times out or fails.
    fn read_one_frame(stream: &mut UnixStream) -> StreamFrame {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        match wire::read_stream_frame(stream) {
            Ok(Some(f)) => f,
            Ok(None) => panic!("EOF before frame received"),
            Err(e) => panic!("read_stream_frame failed: {}", e),
        }
    }

    /// Build a `ManifestWatchHandle` synthetically — subscribes to
    /// `state.manifest_watcher` and captures the current snapshot.
    /// Mirrors what `dispatch_manifest_watch` does in production
    /// (under the state lock).
    ///
    /// 10e-b r3: takes an explicit `heartbeat_interval` so tests
    /// drive the handler's recv_timeout cadence directly. Pre-r3
    /// this lived in a process-global atomic overridden via a
    /// test helper — that pattern flaked under parallel test
    /// execution.
    fn build_manifest_watch_handle(
        state: &Arc<Mutex<DaemonState>>,
        request_id: &str,
        heartbeat_interval: Duration,
    ) -> crate::control::dispatch::ManifestWatchHandle {
        let s = state.lock().unwrap();
        // 10e-b r2: subscribe returns (rx, guard). Guard is
        // packaged into the handle so the test exercises the
        // same RAII lifecycle production uses.
        let (diff_rx, guard) = s.manifest_watcher.subscribe();
        let initial_snapshot = serde_json::json!({
            "workspaces": &s.workspaces,
            "bindings": &s.bindings,
        });
        crate::control::dispatch::ManifestWatchHandle {
            initial_snapshot,
            diff_rx,
            guard,
            heartbeat_interval,
            request_id: request_id.to_string(),
        }
    }

    /// T3 — subscribe via `handle_manifest_watch_stream` →
    /// `ManifestSnapshot` frame arrives first → broadcaster fires
    /// a `ManifestDiff::Exited` → next frame matches that diff.
    #[test]
    fn manifest_watch_sends_snapshot_then_streams_diffs() {
        // 10e-b r3: per-handle heartbeat interval — short value
        // so the post-drop cleanup wakes the handler quickly via
        // a heartbeat write that hits BrokenPipe. (No
        // process-global mutation; concurrent tests don't race.)
        let test_heartbeat = Duration::from_micros(50_000);

        let state = Arc::new(Mutex::new(DaemonState::new()));
        // Seed a workspace so the snapshot has identifiable
        // content (something to differentiate from an empty
        // initial state).
        {
            let mut s = state.lock().unwrap();
            s.workspaces.insert(
                "ws-t3".into(),
                crate::manifest::ManifestWorkspace {
                    id: "ws-t3".into(),
                    name: "test".into(),
                    sessions: Vec::new(),
                    ..Default::default()
                },
            );
        }

        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let handle =
            build_manifest_watch_handle(&state, "req-t3", test_heartbeat);
        let state_clone = state.clone();
        let join = std::thread::spawn(move || {
            handle_manifest_watch_stream(&mut server, handle);
            // Hold state_clone for the thread's lifetime so the
            // broadcaster stays alive.
            drop(state_clone);
        });

        // Frame 1: snapshot.
        let frame = read_one_frame(&mut client);
        assert_eq!(frame.kind, StreamKind::ManifestSnapshot);
        assert_eq!(frame.id, "req-t3");
        let workspaces = &frame.payload["workspaces"];
        assert!(
            workspaces.get("ws-t3").is_some(),
            "snapshot frame must include seeded workspace; got payload {}",
            frame.payload,
        );

        // Fire a broadcast — simulates `handle_session_exit`'s call.
        state
            .lock()
            .unwrap()
            .manifest_watcher
            .broadcast(crate::manifest::ManifestDiff::Exited {
                uid: "ts-t3-victim".into(),
                last_exit: crate::manifest::LastExit {
                    code: Some(1),
                    memory_cap_kill: false,
                    kills_file_offset: None,
                    exited_at: 1.0,
                },
            });

        // Frame 2: the diff.
        let frame = read_one_frame(&mut client);
        assert_eq!(frame.kind, StreamKind::ManifestDiff);
        assert_eq!(frame.id, "req-t3");
        // ManifestDiff is variant-tagged. The Exited variant should
        // be present in the payload.
        assert!(
            frame.payload["Exited"]["uid"] == "ts-t3-victim",
            "diff frame must carry the broadcast payload; got {}",
            frame.payload,
        );
        assert_eq!(frame.payload["Exited"]["last_exit"]["code"], 1);

        // Cleanup: drop the client. With the 10e-b r1 heartbeat
        // fix + r3 per-handle interval, the handler's
        // `recv_timeout` hits its (test-shortened) interval
        // shortly, writes a heartbeat, sees BrokenPipe, and
        // exits. No global restore needed — the interval lived
        // only in this handle.
        drop(client);
        let _ = join.join();
    }

    /// T4 — a slow subscriber whose channel fills gets dropped by
    /// the broadcaster's `try_send → retain`. Drives MANIFEST_WATCH_BUFFER+1
    /// broadcasts without the subscriber draining, then verifies the
    /// subscriber was reaped from the slot list AND the broadcaster
    /// didn't hang.
    ///
    /// Tests at the broadcaster level (not via the streaming RPC)
    /// because the RPC's read-side would drain the channel; we want
    /// to exercise the bounded-channel drop directly.
    #[test]
    fn manifest_watch_drops_slow_subscriber_on_full_channel() {
        let watcher = crate::manifest::ManifestWatcher::new();
        // 10e-b r2: hold both rx (so it doesn't drop and reap via
        // guard's Disconnected detection in unsubscribe) AND
        // guard. The slow-subscriber drop we test here is the
        // broadcaster's try_send→Full path, NOT the guard-drop
        // path (which T11 pins separately).
        let (_rx, _guard) = watcher.subscribe();

        // Broadcast MANIFEST_WATCH_BUFFER + 1 diffs without reading.
        // The first 32 fill the channel; the 33rd causes
        // `try_send → Err(Full)` → broadcast removes the subscriber.
        for i in 0..(crate::manifest::MANIFEST_WATCH_BUFFER + 1) {
            watcher.broadcast(crate::manifest::ManifestDiff::Tombstoned {
                uid: format!("ts-slow-{}", i),
                exited_at: i as f64,
            });
        }

        // After overrun, subscriber list is empty (slow one dropped).
        assert_eq!(
            watcher.subscriber_slot_count(),
            0,
            "slow subscriber MUST be dropped after broadcaster's \
             try_send returns Full",
        );
    }

    /// T5 — concurrent broadcasts serialize through the broadcaster's
    /// internal mutex. Two threads broadcasting interleave; the
    /// subscriber sees all diffs (no loss, bounded order at the
    /// per-broadcast level — `Mutex<Vec<SyncSender>>` is FIFO per
    /// acquisition).
    #[test]
    fn manifest_watch_concurrent_broadcasts_all_delivered() {
        let watcher = Arc::new(crate::manifest::ManifestWatcher::new());
        // 10e-b r2: hold guard so the subscription persists
        // through the threaded broadcasts.
        let (rx, _guard) = watcher.subscribe();

        let w1 = Arc::clone(&watcher);
        let w2 = Arc::clone(&watcher);
        let t1 = std::thread::spawn(move || {
            for i in 0..5 {
                w1.broadcast(crate::manifest::ManifestDiff::Tombstoned {
                    uid: format!("t1-{}", i),
                    exited_at: i as f64,
                });
            }
        });
        let t2 = std::thread::spawn(move || {
            for i in 0..5 {
                w2.broadcast(crate::manifest::ManifestDiff::Tombstoned {
                    uid: format!("t2-{}", i),
                    exited_at: 100.0 + i as f64,
                });
            }
        });
        t1.join().unwrap();
        t2.join().unwrap();

        // Drain. Both threads' 5 diffs each should arrive.
        let mut count_t1 = 0;
        let mut count_t2 = 0;
        for _ in 0..10 {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(crate::manifest::ManifestDiff::Tombstoned {
                    uid, ..
                }) => {
                    if uid.starts_with("t1-") {
                        count_t1 += 1;
                    } else if uid.starts_with("t2-") {
                        count_t2 += 1;
                    }
                }
                Ok(_) => panic!("unexpected diff variant"),
                Err(e) => panic!("missing diff after {} t1 + {} t2: {:?}",
                    count_t1, count_t2, e),
            }
        }
        assert_eq!(count_t1, 5);
        assert_eq!(count_t2, 5);
    }

    /// T6 — subscribe → broadcast diff-A → drop subscriber →
    /// broadcast diff-B (no live subscribers) → subscribe fresh →
    /// new subscriber MUST NOT see diff-A or diff-B (no replay).
    /// The snapshot read separately by the dispatcher is canonical;
    /// resubscribe doesn't surface historical diffs.
    #[test]
    fn manifest_watch_reconnect_does_not_replay_historical_diffs() {
        let watcher = crate::manifest::ManifestWatcher::new();

        // First subscription. Hold the guard alongside the
        // receiver — both drop together below.
        let (rx1, guard1) = watcher.subscribe();
        watcher.broadcast(crate::manifest::ManifestDiff::Tombstoned {
            uid: "ts-A".into(),
            exited_at: 1.0,
        });
        // First subscriber receives diff-A.
        let diff_a = rx1.recv_timeout(Duration::from_millis(500)).unwrap();
        assert!(matches!(
            diff_a,
            crate::manifest::ManifestDiff::Tombstoned { ref uid, .. } if uid == "ts-A",
        ));

        // Drop first subscriber AND its guard. 10e-b r2: guard's
        // Drop reaps the slot immediately — no need to wait for
        // a follow-up broadcast.
        drop(rx1);
        drop(guard1);
        assert_eq!(
            watcher.subscriber_slot_count(),
            0,
            "10e-b r2: guard Drop reaps slot immediately, no \
             follow-up broadcast required",
        );

        // Diff-B fires with no live subscribers.
        watcher.broadcast(crate::manifest::ManifestDiff::Tombstoned {
            uid: "ts-B".into(),
            exited_at: 2.0,
        });

        // Fresh subscribe. Must NOT see diff-A or diff-B (no
        // replay buffer — that's the §3 design choice; consumer
        // resyncs via dispatcher-side snapshot reads, not via
        // broadcaster replay).
        let (rx2, _guard2) = watcher.subscribe();
        match rx2.recv_timeout(Duration::from_millis(200)) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            other => panic!(
                "fresh subscriber MUST NOT replay historical diffs; got {:?}",
                other,
            ),
        }
    }

    /// T7 — Operator-only auth: a Session caller is rejected with
    /// `Unauthorized` AND no subscription is created on the
    /// broadcaster.
    #[test]
    fn manifest_watch_session_caller_rejected_no_subscription_leak() {
        use crate::control::protocol::{Caller, CallerSession, Request};

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let initial_slots = state
            .lock()
            .unwrap()
            .manifest_watcher
            .subscriber_slot_count();

        // Build a manifest.watch request with a Session caller.
        let req = Request {
            id: "req-t7".into(),
            caller: Caller::Session(CallerSession {
                session_uid: "ts-some-agent".into(),
            }),
            method: "manifest.watch".into(),
            params: serde_json::json!({}),
        };
        let outcome = crate::control::dispatch::dispatch_request(&state, &req);
        let response = outcome.into_response();
        assert!(
            !response.ok,
            "Session-caller manifest.watch MUST be rejected",
        );
        assert_eq!(
            response.error.as_ref().expect("error body").code,
            crate::control::protocol::ErrorCode::Unauthorized,
        );

        // No subscription leaked.
        let post_slots = state
            .lock()
            .unwrap()
            .manifest_watcher
            .subscriber_slot_count();
        assert_eq!(
            post_slots, initial_slots,
            "rejected Session caller MUST NOT leak a subscriber slot",
        );
    }

    /// T8 — no-gap guarantee for the dispatcher's
    /// subscribe-and-snapshot critical section. The dispatcher
    /// holds `DaemonState` lock across BOTH operations
    /// (subscribe + snapshot read), so no broadcast can interleave
    /// — broadcasts are themselves gated by the state lock
    /// (called from `handle_session_exit` which holds it). The
    /// receiver returned in the handle is wired to the
    /// broadcaster, so every broadcast that fires AFTER the
    /// dispatcher releases the lock arrives in `diff_rx`.
    ///
    /// On the subscribe-then-snapshot ordering specifically:
    /// because both ops happen under a single lock acquisition,
    /// the in-source order is observationally equivalent under
    /// current implementation. The order is documented as a
    /// structural defense for any future refactor that splits the
    /// lock acquisition — subscribing FIRST would still preserve
    /// the no-gap invariant in that case (a snapshot-read-only
    /// gap can drop a diff; a subscribe-only gap can't, because
    /// subscribers see only diffs fired AFTER subscribe).
    ///
    /// What this test pins runtime: the receiver returned by the
    /// dispatcher actually receives broadcasts. A future refactor
    /// that subscribed-then-shadowed-the-receiver, or returned
    /// a stale receiver, would fail this assertion.
    #[test]
    fn manifest_watch_handle_receives_broadcasts_after_dispatcher_lock_release() {
        use crate::control::protocol::{Caller, CallerOperator, Request};

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let req = Request {
            id: "req-t8".into(),
            caller: Caller::Operator(CallerOperator {
                token_id: "t".into(),
            }),
            method: "manifest.watch".into(),
            params: serde_json::json!({}),
        };
        let outcome = crate::control::dispatch::dispatch_request(&state, &req);
        let handle = match outcome {
            crate::control::dispatch::DispatchOutcome::ManifestWatchStream {
                handle,
                ..
            } => handle,
            other => panic!(
                "expected ManifestWatchStream, got {:?}",
                std::mem::discriminant(&other),
            ),
        };

        // Now the state lock is released. Broadcast a diff.
        // Our receiver MUST observe it because we subscribed
        // (line `state.manifest_watcher.subscribe()` inside
        // `dispatch_manifest_watch`) before the lock release.
        state
            .lock()
            .unwrap()
            .manifest_watcher
            .broadcast(crate::manifest::ManifestDiff::Tombstoned {
                uid: "ts-t8-post-release".into(),
                exited_at: 42.0,
            });

        let diff = handle
            .diff_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("subscribe-before-snapshot MUST deliver \
                     post-release broadcasts to the receiver");
        match diff {
            crate::manifest::ManifestDiff::Tombstoned { uid, .. } => {
                assert_eq!(uid, "ts-t8-post-release");
            }
            other => panic!("unexpected diff variant: {:?}", other),
        }
    }

    /// T11 (10e-b r2) — dispatcher-level RAII guard reap: when
    /// `dispatch_manifest_watch` returns a handle and that
    /// handle is dropped (handler exit, panic, or
    /// abandoned-before-spawn), the broadcaster's subscriber
    /// slot is reaped IMMEDIATELY via the guard's Drop. No
    /// follow-up broadcast required.
    ///
    /// Pre-r2 a TUI that disconnected during a quiet period
    /// would leave the SyncSender in the broadcaster's vec until
    /// the next real broadcast triggered `try_send → Err →
    /// retain`. Repeated cycles accumulated dead slots.
    #[test]
    fn manifest_watch_handle_drop_immediately_reaps_subscriber_slot() {
        use crate::control::protocol::{Caller, CallerOperator, Request};

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let req = Request {
            id: "req-t11".into(),
            caller: Caller::Operator(CallerOperator {
                token_id: "t".into(),
            }),
            method: "manifest.watch".into(),
            params: serde_json::json!({}),
        };

        // Pre-subscribe slot count.
        assert_eq!(
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .subscriber_slot_count(),
            0,
        );

        let outcome = crate::control::dispatch::dispatch_request(&state, &req);
        let handle = match outcome {
            crate::control::dispatch::DispatchOutcome::ManifestWatchStream {
                handle,
                ..
            } => handle,
            _ => panic!("expected ManifestWatchStream"),
        };
        // Mid-handle: slot is present.
        assert_eq!(
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .subscriber_slot_count(),
            1,
            "subscribe inserted a slot",
        );

        // Drop the handle WITHOUT spawning a handler — simulates
        // the failure mode where dispatch returned but the
        // stream consumer dropped the handle (e.g., write of
        // initial response failed in handle_connection).
        drop(handle);

        // Post-drop: slot MUST be reaped immediately via guard.
        // Pre-r2 the slot would persist until the next broadcast.
        assert_eq!(
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .subscriber_slot_count(),
            0,
            "10e-b r2: ManifestWatchHandle drop MUST immediately \
             reap the subscriber slot via the held guard",
        );
    }

    /// T12 (10e-b r2) — repeated connect/disconnect cycles via
    /// the dispatcher path don't accumulate slots. Bounds the
    /// idle-daemon slot count regardless of broadcast rate.
    #[test]
    fn manifest_watch_repeated_dispatch_cycles_do_not_accumulate_slots() {
        use crate::control::protocol::{Caller, CallerOperator, Request};

        let state = Arc::new(Mutex::new(DaemonState::new()));
        for i in 0..100 {
            let req = Request {
                id: format!("req-t12-{}", i),
                caller: Caller::Operator(CallerOperator {
                    token_id: "t".into(),
                }),
                method: "manifest.watch".into(),
                params: serde_json::json!({}),
            };
            let outcome =
                crate::control::dispatch::dispatch_request(&state, &req);
            // Take the handle and drop it — same lifecycle as a
            // TUI that subscribed then immediately disconnected.
            if let crate::control::dispatch::DispatchOutcome::ManifestWatchStream {
                handle,
                ..
            } = outcome
            {
                drop(handle);
            } else {
                panic!("iter {}: expected ManifestWatchStream", i);
            }
        }
        assert_eq!(
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .subscriber_slot_count(),
            0,
            "100 dispatch+drop cycles MUST leave zero slots; \
             pre-r2 this would have left 100 orphan SyncSenders \
             in the broadcaster's vec until the next real \
             broadcast triggered retain",
        );
    }

    /// T10 (10e-b r1) — idle-disconnect detection via heartbeat.
    /// Pre-r1 a client that disconnected during a quiet period
    /// (no diff broadcasts firing) left the handler parked on
    /// `recv()` indefinitely; the dead subscriber + parked thread
    /// would accumulate until the next real broadcast.
    /// Post-r1 the handler uses `recv_timeout` with a heartbeat
    /// write on each timeout — the write attempt itself probes
    /// liveness. A disconnected client surfaces as `BrokenPipe`
    /// on the next heartbeat, within roughly one
    /// `MANIFEST_WATCH_HEARTBEAT_MICROS` interval.
    ///
    /// Test uses a 50ms heartbeat passed per-handle so the
    /// assertion runs in <1s rather than the 15s production
    /// interval. 10e-b r3 (per-handle field) — no process-global
    /// state, no concurrent-test isolation hazard.
    #[test]
    fn manifest_watch_idle_disconnect_detected_within_heartbeat_interval() {
        let test_heartbeat = Duration::from_micros(50_000); // 50ms

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let (client, mut server) =
            UnixStream::pair().expect("socket pair");
        let handle =
            build_manifest_watch_handle(&state, "req-t10", test_heartbeat);
        let join = std::thread::spawn(move || {
            handle_manifest_watch_stream(&mut server, handle);
        });

        // Wait for the handler to start up and write the initial
        // snapshot — confirms the subscription is wired before we
        // drop the client.
        let mut client = client;
        let _snapshot = read_one_frame(&mut client);

        // Drop the client. With NO broadcasts firing, the handler
        // sits on `recv_timeout(50ms)`. On the next timeout it
        // writes a heartbeat → BrokenPipe → exits. 10e-b r2's
        // RAII guard reaps the subscriber slot at the same
        // moment (handle drop on handler return).
        drop(client);

        // Wait for the handler to detect disconnect + exit. Bound
        // generously (heartbeat interval × 10 + 200ms slack for
        // scheduling jitter) — typical detection is one
        // heartbeat interval after disconnect.
        let deadline = std::time::Instant::now()
            + test_heartbeat * 10
            + Duration::from_millis(200);
        loop {
            if join.is_finished() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "handler did NOT exit within {} ms (interval {} µs × 10 + slack); \
                     pre-r1 it would have stayed parked forever — heartbeat \
                     fix didn't fire",
                    deadline
                        .saturating_duration_since(std::time::Instant::now())
                        .as_millis(),
                    test_heartbeat.as_micros(),
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = join.join();

        // 10e-b r2: subscriber slot is reaped immediately via
        // the guard's Drop on handler exit. No broadcast needed
        // to trigger reaping (T11 pins this directly; here we
        // assert it's the case for the disconnect path too).
        assert_eq!(
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .subscriber_slot_count(),
            0,
            "post-heartbeat-disconnect, broadcaster's slot list \
             must be empty — guard Drop reaped on handler exit",
        );
    }

    /// T9 — disconnect cleanup: when the client drops the socket,
    /// the daemon's next broadcast attempt either fails with
    /// BrokenPipe (handler exits, receiver dropped) OR the
    /// broadcaster's next `try_send` returns `Disconnected` and
    /// `retain` reaps the slot. Either path leaves `subscriber_slot_count`
    /// at zero.
    #[test]
    fn manifest_watch_disconnect_drops_subscriber_on_next_broadcast() {
        let state = Arc::new(Mutex::new(DaemonState::new()));

        let (client, mut server) =
            UnixStream::pair().expect("socket pair");
        // 10e-b r3: short heartbeat so the handler wakes
        // quickly. T9's path is broadcast-driven cleanup (the
        // broadcasts in the loop below wake the handler before
        // any heartbeat would), but a short interval keeps the
        // test fast even if scheduling delays the broadcasts.
        let handle = build_manifest_watch_handle(
            &state,
            "req-t9",
            Duration::from_micros(50_000),
        );
        let join = std::thread::spawn(move || {
            handle_manifest_watch_stream(&mut server, handle);
        });

        // Read the initial snapshot frame so we know the handler
        // has started up before we drop the client.
        let mut client = client;
        let _snapshot = read_one_frame(&mut client);

        // Drop the client. The next broadcast will fail to deliver.
        drop(client);

        // Broadcast — write error inside the handler → return →
        // receiver dropped. We can't observe ordering precisely
        // (the broadcast happens, the handler MAY have already
        // exited, the SyncSender lives until our broadcast
        // attempt). Drive a second broadcast to definitively
        // reap any lingering slot.
        for i in 0..3 {
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .broadcast(crate::manifest::ManifestDiff::Tombstoned {
                    uid: format!("ts-t9-{}", i),
                    exited_at: i as f64,
                });
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = join.join();
        assert_eq!(
            state
                .lock()
                .unwrap()
                .manifest_watcher
                .subscriber_slot_count(),
            0,
            "after client disconnect + broadcasts, no subscribers \
             must remain in the broadcaster slot list",
        );
    }

    // ---- 10g: reconnect ring-buffer replay (Phase 1 named --------
    //          acceptance: TUI restart preserves session output).

    /// Drive an attach-stream over a `UnixStream::pair()` for an
    /// already-registered session. Returns `(client_half,
    /// handle_attach_stream_thread)`. The caller drops the client
    /// half to terminate the attach; the thread handle is joined to
    /// confirm clean shutdown.
    ///
    /// Shared by both the single-reconnect (T-reconnect) and
    /// multi-reconnect (T-reconnect-multi) tests so the wire-level
    /// setup is identical to production.
    fn spawn_attach_stream(
        state: &Arc<Mutex<DaemonState>>,
        session_uid: &str,
        request_id: &str,
    ) -> (UnixStream, std::thread::JoinHandle<()>) {
        let (client, mut server) =
            UnixStream::pair().expect("socket pair");
        let state_clone = state.clone();
        let session_uid = session_uid.to_string();
        let request_id = request_id.to_string();
        let handle = std::thread::spawn(move || {
            let attach_handle =
                build_handle(&state_clone, &session_uid, &request_id);
            handle_attach_stream(&mut server, state_clone, attach_handle);
        });
        (client, handle)
    }

    /// Read the FIRST `StreamKind::Data` frame on the wire and
    /// return its base64-decoded payload bytes. Panics on
    /// deadline or unexpected stream errors — both modes indicate
    /// a contract violation the test must surface.
    ///
    /// Non-Data frames (heartbeats, control) are skipped silently
    /// so the helper pins the "first PTY-byte frame" not "first
    /// frame of any kind". The named acceptance contract is:
    /// **the first Data frame after `attach.open` IS the
    /// `PtyByteFanout::subscribe()` replay chunk**. Anything
    /// looser (accumulating across frames, allowing the test to
    /// pass on a late live duplicate) lets a broken replay
    /// implementation slip through if `/bin/cat`'s PTY echo
    /// happens to emit a matching byte sequence after the
    /// reconnect (the reviewer's r1 finding).
    fn read_first_data_frame_bytes(
        client: &mut UnixStream,
        deadline: std::time::Instant,
    ) -> Vec<u8> {
        loop {
            let remaining = deadline
                .saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!(
                    "did not receive first Data frame within deadline; \
                     pre-r1 the test would have accepted a later live \
                     frame, masking a broken replay impl",
                );
            }
            let _ = client.set_read_timeout(Some(remaining));
            let frame = match wire::read_stream_frame(client) {
                Ok(Some(f)) => f,
                Ok(None) => {
                    panic!(
                        "EOF before first Data frame — the daemon \
                         closed the attach stream without delivering \
                         the replay (contract violation)",
                    );
                }
                Err(e) => {
                    panic!("read_stream_frame failed: {}", e);
                }
            };
            match frame.kind {
                StreamKind::Data => {
                    let Some(b64) = frame
                        .payload
                        .get("bytes")
                        .and_then(|v| v.as_str())
                    else {
                        panic!(
                            "first Data frame missing `bytes` payload: \
                             {:?}",
                            frame.payload,
                        );
                    };
                    return BASE64.decode(b64).expect(
                        "first Data frame's bytes must be valid base64",
                    );
                }
                // Heartbeat and other future control frames don't
                // count — skip and keep reading for the first
                // payload-carrying frame.
                _ => continue,
            }
        }
    }

    /// T-reconnect (10g, Phase 1 named acceptance gate) — daemon
    /// retains the session's PTY output across TUI restart.
    /// Production sequence: TUI spawns session → bytes accumulate
    /// in the daemon's ring buffer → TUI dies → fresh TUI dials
    /// `attach.open` → the SECOND attach.open's first Data frame
    /// carries the pre-disconnect bytes as the ring-buffer replay.
    ///
    /// Test maps this 1:1 onto the in-process surface: a real
    /// `/bin/echo` PTY behind `DaemonSession::spawn`, both
    /// attaches driven through `handle_attach_stream` over
    /// `UnixStream::pair()`. The second client reads the first
    /// frame off the wire and asserts the replay payload contains
    /// the pre-disconnect substring.
    ///
    /// **r2 seed choice — `/bin/echo` over `/bin/cat` + Input
    /// frame**: a PTY-backed `/bin/cat` emits the canary TWICE per
    /// input (kernel line-discipline echo + cat's own stdout). The
    /// reviewer's r1 finding was that even with a tightened
    /// first-frame assertion, a broken-replay impl could still
    /// pass if the SECOND chunk arrives as a live broadcast on
    /// the reconnected attach. `/bin/echo` exits after one write
    /// and never reads stdin, so the kernel's line-discipline
    /// echo path never fires — the fanout receives exactly one
    /// canary chunk, eliminating the dual-emit race at the
    /// source.
    #[test]
    fn attach_stream_replay_survives_disconnect_reconnect() {
        let mut params = SpawnParams::new(
            "ts-reconnect",
            "echo-reconnect",
            "/bin/echo",
        );
        params.args = vec!["hello-pre-disconnect".to_string()];
        let session = DaemonSession::spawn(params).expect("spawn echo");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        state
            .lock()
            .unwrap()
            .sessions
            .insert("ts-reconnect".into(), session);

        // First attach: confirm bytes are in the ring buffer by
        // observing them on attach1's wire BEFORE we drop.
        // `/bin/echo` writes the canary once and exits; the
        // daemon's reader thread pushes a single chunk to the
        // fanout. attach1 might subscribe before OR after the
        // push lands — either way its first Data frame contains
        // the canary (live broadcast or replay-of-already-filled-
        // buffer).
        let (mut client1, handle1) =
            spawn_attach_stream(&state, "ts-reconnect", "req-attach-1");

        let deadline1 =
            std::time::Instant::now() + Duration::from_secs(3);
        let acc1 =
            read_first_data_frame_bytes(&mut client1, deadline1);
        let acc1_text = String::from_utf8_lossy(&acc1);
        assert!(
            acc1_text.contains("hello-pre-disconnect"),
            "first attach's first Data frame must contain the \
             canary; got {:?}",
            acc1_text,
        );

        // Drop the first attach. The daemon's handle_attach_stream
        // observes the client EOF and exits; the DaemonSession
        // stays alive in state.sessions — that's the property
        // we're testing.
        drop(client1);
        let _ = handle1.join();

        // The session must still be registered (no stale-reap on
        // attach disconnect — only fanout-close / child-exit reap,
        // and even the latter only triggers the registry-cleanup
        // callback when one was wired, which the convenience-form
        // spawn used by this test does not).
        assert!(
            state
                .lock()
                .unwrap()
                .sessions
                .contains_key("ts-reconnect"),
            "session must survive attach disconnect; daemon's \
             whole reason to exist is this property",
        );

        // Second attach: drive through the production wire path.
        // The FIRST Data frame on the new pair MUST be the
        // ring-buffer replay chunk — `PtyByteFanout::subscribe`'s
        // contract sends the buffer as one chunk before any
        // subsequent push. After `handle_attach_stream` wraps
        // that chunk, the very first Data frame on the wire IS
        // the replay (r1 finding: pre-fix the test accepted a
        // later live duplicate; r2 fix: seed source emits exactly
        // once so no live duplicate exists to mask the contract).
        let (mut client2, handle2) =
            spawn_attach_stream(&state, "ts-reconnect", "req-attach-2");

        let deadline2 =
            std::time::Instant::now() + Duration::from_secs(3);
        let replay_bytes =
            read_first_data_frame_bytes(&mut client2, deadline2);
        let replay_text = String::from_utf8_lossy(&replay_bytes);
        assert!(
            replay_text.contains("hello-pre-disconnect"),
            "FIRST Data frame on the reconnected attach MUST be \
             the ring-buffer replay containing the pre-disconnect \
             bytes — this is the named acceptance criterion for \
             Phase 1. Got {:?}",
            replay_text,
        );

        // Cleanup. echo exited long ago; removing the registry
        // entry drops the DaemonSession.
        drop(client2);
        let _ = handle2.join();
        state.lock().unwrap().sessions.remove("ts-reconnect");
    }

    /// T-reconnect-multi (10g) — replay survives multiple
    /// sequential reconnects, not just the first one. Pins the
    /// invariant that the ring buffer isn't "consume-once": each
    /// fresh subscriber sees the same replay independently.
    /// Three cycles in this test; the property holds for arbitrary
    /// N at the data-structure level (per the existing
    /// `subscribers_after_push_get_replay_not_future_only` unit
    /// test), here we cover the wire-level path through
    /// `handle_attach_stream` for confidence the cumulative wire
    /// state doesn't drift across cycles.
    #[test]
    fn attach_stream_replay_survives_multiple_disconnect_reconnect() {
        // r2: see T-reconnect's seed-choice doc — `/bin/echo` is
        // single-emit (no PTY echo dual-chunk).
        let mut params = SpawnParams::new(
            "ts-reconnect-multi",
            "echo-reconnect-multi",
            "/bin/echo",
        );
        params.args = vec!["canary-multi-reconnect".to_string()];
        let session = DaemonSession::spawn(params).expect("spawn echo");
        let state = Arc::new(Mutex::new(DaemonState::new()));
        state
            .lock()
            .unwrap()
            .sessions
            .insert("ts-reconnect-multi".into(), session);

        // Seed sync: confirm the canary landed in the buffer
        // before we start reconnect cycles.
        let (mut seed_client, seed_handle) =
            spawn_attach_stream(&state, "ts-reconnect-multi", "req-seed");
        let seed_deadline =
            std::time::Instant::now() + Duration::from_secs(3);
        let seed_bytes =
            read_first_data_frame_bytes(&mut seed_client, seed_deadline);
        let seed_text = String::from_utf8_lossy(&seed_bytes);
        assert!(
            seed_text.contains("canary-multi-reconnect"),
            "seed attach's first Data frame must contain the \
             canary; got {:?}",
            seed_text,
        );
        drop(seed_client);
        let _ = seed_handle.join();

        // Three sequential reconnects via the production wire.
        // Each one's FIRST Data frame MUST be the replay chunk
        // containing the canary. Pins ring-buffer-isn't-
        // consume-once at the wire level across N cycles.
        for cycle in 0..3 {
            let req_id = format!("req-cycle-{}", cycle);
            let (mut client, handle) = spawn_attach_stream(
                &state,
                "ts-reconnect-multi",
                &req_id,
            );
            let deadline =
                std::time::Instant::now() + Duration::from_secs(3);
            let replay_bytes =
                read_first_data_frame_bytes(&mut client, deadline);
            let replay_text = String::from_utf8_lossy(&replay_bytes);
            assert!(
                replay_text.contains("canary-multi-reconnect"),
                "cycle {}: FIRST Data frame MUST be the ring-buffer \
                 replay containing the canary; got {:?}",
                cycle,
                replay_text,
            );
            drop(client);
            let _ = handle.join();
        }

        // Cleanup.
        state
            .lock()
            .unwrap()
            .sessions
            .remove("ts-reconnect-multi");
    }

    // =================================================================
    // 11b: events.subscribe streaming tests
    // =================================================================
    //
    // Tests exercise `handle_events_subscribe_stream` end-to-end through
    // `UnixStream::pair`. The handler is driven on one end, the test
    // reads frames from the other. Mirror of the manifest.watch
    // T3-T11 suite above, plus T_snapshot_disk for the
    // disk-authoritative snapshot invariant.

    /// Build an `EventsSubscribeHandle` synthetically. Mirrors what
    /// `dispatch_events_subscribe` does in production. The
    /// `initial_snapshots` arg lets a test inject whatever payload
    /// it wants (or empty) without forcing a disk write — the
    /// `T_snapshot_disk` test exercises the real disk path
    /// separately.
    fn build_events_subscribe_handle(
        state: &Arc<Mutex<DaemonState>>,
        request_id: &str,
        heartbeat_interval: Duration,
        initial_snapshots: Vec<serde_json::Value>,
    ) -> crate::control::dispatch::EventsSubscribeHandle {
        let s = state.lock().unwrap();
        let (event_rx, guard) = s.workflow_event_watcher.subscribe();
        crate::control::dispatch::EventsSubscribeHandle {
            initial_snapshots,
            event_rx,
            guard,
            heartbeat_interval,
            request_id: request_id.to_string(),
        }
    }

    fn make_test_event(id: &str, run_id: &str) -> crate::workflow::events::Event {
        crate::workflow::events::Event {
            id: id.into(),
            ts: 0.0,
            run_id: run_id.into(),
            role: "worker".into(),
            tool: "workflow_transition".into(),
            args: serde_json::json!({"to": "reviewer", "prompt": ""}),
            source: String::new(),
            from_role: None,
            iteration: 0,
        }
    }

    /// T5 — subscribe via `handle_events_subscribe_stream` → one
    /// `WorkflowEventStateSnapshot` per active run arrives first →
    /// broadcaster fires a `WorkflowEvent` → next frame matches.
    #[test]
    fn events_subscribe_sends_snapshot_then_streams_events() {
        let test_heartbeat = Duration::from_micros(50_000);
        let state = Arc::new(Mutex::new(DaemonState::new()));

        // Inject two synthetic snapshot payloads (the handler
        // doesn't care how they were obtained; the dispatcher's
        // disk-read path is exercised by T_snapshot_disk).
        let snapshots = vec![
            serde_json::json!({"run_id": "wf-1"}),
            serde_json::json!({"run_id": "wf-2"}),
        ];

        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let handle = build_events_subscribe_handle(
            &state,
            "req-t5",
            test_heartbeat,
            snapshots,
        );
        let state_clone = state.clone();
        let join = std::thread::spawn(move || {
            handle_events_subscribe_stream(&mut server, handle);
            drop(state_clone);
        });

        // Frame 1: first snapshot.
        let frame = read_one_frame(&mut client);
        assert_eq!(frame.kind, StreamKind::WorkflowEventStateSnapshot);
        assert_eq!(frame.id, "req-t5");
        assert_eq!(frame.payload["run_id"], "wf-1");

        // Frame 2: second snapshot.
        let frame = read_one_frame(&mut client);
        assert_eq!(frame.kind, StreamKind::WorkflowEventStateSnapshot);
        assert_eq!(frame.payload["run_id"], "wf-2");

        // Fire a broadcast — simulates what
        // `append_event_with_retry` does post-write.
        state
            .lock()
            .unwrap()
            .workflow_event_watcher
            .broadcast(make_test_event("e-1", "wf-1"));

        // Frame 3: the WorkflowEvent.
        let frame = read_one_frame(&mut client);
        assert_eq!(frame.kind, StreamKind::WorkflowEvent);
        assert_eq!(frame.id, "req-t5");
        assert_eq!(frame.payload["id"], "e-1");
        assert_eq!(frame.payload["run_id"], "wf-1");

        drop(client);
        let _ = join.join();
    }

    /// T6 — Operator-only auth: a Session caller is rejected with
    /// `Unauthorized` AND no subscription is created on the
    /// broadcaster.
    #[test]
    fn events_subscribe_session_caller_rejected_no_subscription_leak() {
        use crate::control::protocol::{Caller, CallerSession, Request};

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let initial_slots = state
            .lock()
            .unwrap()
            .workflow_event_watcher
            .subscriber_slot_count();

        let req = Request {
            id: "req-t6".into(),
            caller: Caller::Session(CallerSession {
                session_uid: "ts-some-agent".into(),
            }),
            method: "events.subscribe".into(),
            params: serde_json::json!({}),
        };
        let outcome = crate::control::dispatch::dispatch_request(&state, &req);
        let response = outcome.into_response();
        assert!(
            !response.ok,
            "Session-caller events.subscribe MUST be rejected",
        );
        assert_eq!(
            response.error.as_ref().expect("error body").code,
            crate::control::protocol::ErrorCode::Unauthorized,
        );

        let post_slots = state
            .lock()
            .unwrap()
            .workflow_event_watcher
            .subscriber_slot_count();
        assert_eq!(
            post_slots, initial_slots,
            "rejected Session caller MUST NOT leak a subscriber slot",
        );
    }

    /// T7 — handle receives broadcasts after the dispatcher
    /// releases the state lock. Mirror of manifest.watch T8: the
    /// subscribe happens inside the dispatch arm under the state
    /// lock; once `dispatch_request` returns, broadcasts MUST
    /// land in our receiver.
    #[test]
    fn events_subscribe_handle_receives_broadcasts_after_lock_release() {
        use crate::control::protocol::{Caller, CallerOperator, Request};
        // env_lock + tempdir for HOME so dispatch_events_subscribe's
        // load_all() read doesn't see real user state.
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let req = Request {
            id: "req-t7".into(),
            caller: Caller::Operator(CallerOperator {
                token_id: "t".into(),
            }),
            method: "events.subscribe".into(),
            params: serde_json::json!({}),
        };
        let outcome = crate::control::dispatch::dispatch_request(&state, &req);
        let handle = match outcome {
            crate::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                handle,
                ..
            } => handle,
            _ => panic!("expected EventsSubscribeStream outcome"),
        };

        // Lock is released. Broadcast an event; our receiver
        // MUST observe it because subscribe ran inside the dispatch
        // arm's locked critical section.
        state
            .lock()
            .unwrap()
            .workflow_event_watcher
            .broadcast(make_test_event("e-post-release", "wf-x"));

        let event = handle
            .event_rx
            .recv_timeout(Duration::from_secs(1))
            .expect(
                "subscribe-before-snapshot MUST deliver \
                 post-release broadcasts to the receiver",
            );
        assert_eq!(event.id, "e-post-release");

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// T8 — handle Drop reaps the subscriber slot immediately
    /// via the guard's Drop. Mirror of manifest.watch T11.
    #[test]
    fn events_subscribe_handle_drop_immediately_reaps_subscriber_slot() {
        use crate::control::protocol::{Caller, CallerOperator, Request};
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let state = Arc::new(Mutex::new(DaemonState::new()));
        let req = Request {
            id: "req-t8".into(),
            caller: Caller::Operator(CallerOperator {
                token_id: "t".into(),
            }),
            method: "events.subscribe".into(),
            params: serde_json::json!({}),
        };

        let outcome = crate::control::dispatch::dispatch_request(&state, &req);
        let handle = match outcome {
            crate::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                handle,
                ..
            } => handle,
            _ => panic!("expected EventsSubscribeStream outcome"),
        };
        // Subscribe should have taken effect.
        assert_eq!(
            state
                .lock()
                .unwrap()
                .workflow_event_watcher
                .subscriber_slot_count(),
            1,
        );

        drop(handle);
        // Guard Drop must reap immediately.
        assert_eq!(
            state
                .lock()
                .unwrap()
                .workflow_event_watcher
                .subscriber_slot_count(),
            0,
            "events.subscribe handle Drop MUST reap slot immediately",
        );

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// T9 — idle-disconnect detection via the heartbeat path.
    /// A client that closes during a quiet period (no events) must
    /// be detected within roughly one heartbeat interval — the
    /// handler's `recv_timeout` boundary writes a heartbeat,
    /// hits BrokenPipe, and exits.
    #[test]
    fn events_subscribe_idle_disconnect_detected_within_heartbeat_interval() {
        let test_heartbeat = Duration::from_micros(50_000);
        let state = Arc::new(Mutex::new(DaemonState::new()));

        let (client, mut server) = UnixStream::pair().expect("socket pair");
        let handle = build_events_subscribe_handle(
            &state,
            "req-t9",
            test_heartbeat,
            Vec::new(),
        );
        let join = std::thread::spawn(move || {
            handle_events_subscribe_stream(&mut server, handle);
        });

        // Close the client. The next heartbeat write will hit
        // BrokenPipe and the handler exits.
        drop(client);

        let start = std::time::Instant::now();
        let _ = join.join();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "handler must exit within heartbeat interval of client \
             close, elapsed={:?}",
            elapsed,
        );

        // No slot leak after handler exit.
        assert_eq!(
            state
                .lock()
                .unwrap()
                .workflow_event_watcher
                .subscriber_slot_count(),
            0,
        );
    }

    /// T_snapshot_disk — the critical disk-authoritative
    /// snapshot invariant. Write a WorkflowRun's state.json (the
    /// post-write durability point), broadcast an event AFTER
    /// the write (the broadcast happens after disk-persist),
    /// then have a fresh subscriber dial `events.subscribe`.
    /// The subscriber's first frame MUST be a snapshot
    /// reflecting the post-event state — because state.json on
    /// disk has already advanced.
    ///
    /// Pins the invariant from NOTES.md slice 11b: snapshots
    /// MUST come from disk via `load_all()`, not from the
    /// `state.workflow_runs` in-memory cache (whose update
    /// trails the broadcast). Mutation-verify by swapping the
    /// dispatch arm to read `state.workflow_runs` and watching
    /// this test fail.
    #[test]
    fn events_subscribe_snapshot_reflects_disk_state_not_cache() {
        use crate::control::protocol::{Caller, CallerOperator, Request};
        use crate::workflow::run::{
            save, RoleBinding, RunStatus, WorkflowRun,
        };

        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        // Build + save the WorkflowRun on disk. Active status so
        // the dispatch arm picks it up.
        let mut roles = std::collections::BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "claude".into(),
                current_session_id: Some("sid-w".into()),
                daemon_session_uid: None,
            },
        );
        let run = WorkflowRun::new(
            "wf_snapshot_disk_t".into(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
            std::collections::BTreeMap::new(),
            None,
            std::collections::BTreeMap::new(),
            0,
        );
        // Assert active so the dispatch arm includes it.
        assert!(matches!(run.status, RunStatus::Running));
        save(&run).expect("save state.json");

        let state = Arc::new(Mutex::new(DaemonState::new()));
        // NOTE: do NOT populate `state.workflow_runs`. The cache
        // is intentionally empty — proving that the dispatch
        // arm reads from DISK is the contract this test pins.

        // Broadcast an event BEFORE subscribing — mirrors the
        // ordering in `append_event_with_retry`: state.json
        // written first, broadcast fired, cache update LAGS.
        // A new subscriber landing now must still see the run
        // in its snapshot frame.
        state
            .lock()
            .unwrap()
            .workflow_event_watcher
            .broadcast(make_test_event("e-pre-subscribe", "wf_snapshot_disk_t"));

        let req = Request {
            id: "req-snap-disk".into(),
            caller: Caller::Operator(CallerOperator {
                token_id: "t".into(),
            }),
            method: "events.subscribe".into(),
            params: serde_json::json!({}),
        };
        let outcome = crate::control::dispatch::dispatch_request(&state, &req);
        let handle = match outcome {
            crate::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                handle,
                ..
            } => handle,
            _ => panic!("expected EventsSubscribeStream outcome"),
        };

        // The handle's initial_snapshots MUST contain our saved
        // run — proving the snapshot came from disk, not from
        // the empty cache.
        assert_eq!(
            handle.initial_snapshots.len(),
            1,
            "snapshot MUST be built from disk via load_all(); \
             cache was intentionally empty so a cache-read \
             dispatch arm would produce zero snapshots",
        );
        assert_eq!(
            handle.initial_snapshots[0]["run_id"],
            "wf_snapshot_disk_t",
        );

        drop(handle);

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// T21 (slice 11f) — Phase 2 named acceptance gate: Killing
    /// the TUI mid-workflow and reattaching shows the current
    /// active role and recent transitions via `events.subscribe`'s
    /// snapshot frame. Pins the disk-authoritative snapshot
    /// invariant end-to-end:
    ///
    ///   1. Seed a workflow run on disk with multiple history
    ///      entries (simulating prior transitions).
    ///   2. attach1 subscribes → observes snapshot frame +
    ///      receives a fresh event broadcast → drops.
    ///   3. attach2 subscribes fresh → assert FIRST frame is
    ///      `WorkflowEventStateSnapshot` containing the
    ///      post-transition WorkflowRun (history.len() > 1).
    ///
    /// Mutation-verify the snapshot-send path by removing the
    /// snapshot frame emission in `handle_events_subscribe_stream`
    /// (the for-loop over `initial_snapshots`) and confirming this
    /// test fails because attach2's first frame is a heartbeat
    /// (no snapshot to read).
    #[test]
    fn t21_reconnect_first_frame_is_snapshot_with_post_transition_history() {
        use crate::control::protocol::{Caller, CallerOperator, Request};
        use crate::workflow::run::{
            save, HistoryEntry, RoleBinding, TriggerKind, WorkflowRun,
        };

        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }

        // Seed a workflow run with TWO history entries (initial
        // worker + an activation to reviewer) — simulates the
        // post-transition state at TUI-disconnect time.
        let mut roles = std::collections::BTreeMap::new();
        for r in ["worker", "reviewer", "manager"] {
            roles.insert(
                r.to_string(),
                RoleBinding {
                    session_label: r.into(),
                    current_session_id: None,
                    daemon_session_uid: None,
                },
            );
        }
        let mut run = WorkflowRun::new(
            "wf_t21_reconnect".into(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
            std::collections::BTreeMap::new(),
            None,
            std::collections::BTreeMap::new(),
            0,
        );
        // Append a second history entry to simulate a transition.
        run.history.push(HistoryEntry {
            iteration: 2,
            role: "reviewer".into(),
            session_id: None,
            last_message: Some("worker said diff lgtm?".into()),
            activated_at: 1,
            deactivated_at: None,
            trigger: TriggerKind::McpTransition {
                from_role: "worker".into(),
                prompt: "diff lgtm?".into(),
                event_id: "ev-pre-disconnect".into(),
            },
            assistant_count_at_start: 3,
            text_messages_at_start: 3,
        });
        run.active_role = Some("reviewer".into());
        run.iteration = 2;
        save(&run).expect("save wf state.json");

        let state = Arc::new(Mutex::new(DaemonState::new()));

        // --- attach1: subscribe + drop ---
        let req1 = Request {
            id: "req-attach1".into(),
            caller: Caller::Operator(CallerOperator { token_id: "t".into() }),
            method: "events.subscribe".into(),
            params: serde_json::json!({}),
        };
        let outcome1 = crate::control::dispatch::dispatch_request(&state, &req1);
        let handle1 = match outcome1 {
            crate::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                handle,
                ..
            } => handle,
            _ => panic!("attach1: expected EventsSubscribeStream"),
        };
        assert_eq!(handle1.initial_snapshots.len(), 1);
        assert_eq!(
            handle1.initial_snapshots[0]["history"]
                .as_array()
                .map(|a| a.len()),
            Some(2),
            "attach1 snapshot must reflect post-transition history",
        );
        // Drop attach1 (simulates TUI exit).
        drop(handle1);
        assert_eq!(
            state
                .lock()
                .unwrap()
                .workflow_event_watcher
                .subscriber_slot_count(),
            0,
            "attach1 drop must reap subscriber slot",
        );

        // --- attach2: fresh subscribe; first frame MUST be the
        //     snapshot reflecting the same post-transition state ---
        let req2 = Request {
            id: "req-attach2".into(),
            caller: Caller::Operator(CallerOperator { token_id: "t".into() }),
            method: "events.subscribe".into(),
            params: serde_json::json!({}),
        };
        let outcome2 = crate::control::dispatch::dispatch_request(&state, &req2);
        let handle2 = match outcome2 {
            crate::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                handle,
                ..
            } => handle,
            _ => panic!("attach2: expected EventsSubscribeStream"),
        };
        assert_eq!(
            handle2.initial_snapshots.len(),
            1,
            "attach2 fresh subscribe must surface the active run",
        );
        let snap = &handle2.initial_snapshots[0];
        assert_eq!(snap["run_id"], "wf_t21_reconnect");
        assert_eq!(snap["active_role"], "reviewer");
        assert_eq!(snap["iteration"], 2);
        assert_eq!(
            snap["history"].as_array().map(|a| a.len()),
            Some(2),
            "attach2 snapshot's history MUST contain the pre-disconnect \
             transition — this is the slice 11f acceptance gate",
        );

        // Drive a wire-level acceptance loop too — sends the snapshot
        // through the actual stream handler and reads the frame back.
        // Pins that the in-process snapshot bundle survives the
        // serialize→write→deserialize round-trip.
        let (mut client, mut server) =
            UnixStream::pair().expect("socket pair");
        let join = std::thread::spawn(move || {
            handle_events_subscribe_stream(&mut server, handle2);
        });
        let frame = read_one_frame(&mut client);
        assert_eq!(frame.kind, StreamKind::WorkflowEventStateSnapshot);
        assert_eq!(frame.payload["run_id"], "wf_t21_reconnect");
        assert_eq!(frame.payload["active_role"], "reviewer");
        drop(client);
        let _ = join.join();

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// T22 (slice 11f) — multi-reconnect: 3 cycles of
    /// subscribe-then-drop, each cycle's FIRST handle frame is
    /// the snapshot. Pins that the per-subscribe disk-read
    /// reliably surfaces the run across many reconnects (the
    /// "killing the TUI repeatedly" stress case).
    #[test]
    fn t22_multi_reconnect_each_first_frame_is_snapshot() {
        use crate::control::protocol::{Caller, CallerOperator, Request};
        use crate::workflow::run::{save, RoleBinding, WorkflowRun};

        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", tmp.path()); }

        let mut roles = std::collections::BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            RoleBinding {
                session_label: "claude".into(),
                current_session_id: None,
                daemon_session_uid: None,
            },
        );
        let run = WorkflowRun::new(
            "wf_t22_multi".into(),
            "feedback".into(),
            "/tmp/repo".into(),
            roles,
            "worker".into(),
            std::collections::BTreeMap::new(),
            None,
            std::collections::BTreeMap::new(),
            0,
        );
        save(&run).expect("save");

        let state = Arc::new(Mutex::new(DaemonState::new()));
        for cycle in 0..3 {
            let req = Request {
                id: format!("req-multi-{}", cycle),
                caller: Caller::Operator(CallerOperator { token_id: "t".into() }),
                method: "events.subscribe".into(),
                params: serde_json::json!({}),
            };
            let outcome = crate::control::dispatch::dispatch_request(&state, &req);
            let handle = match outcome {
                crate::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                    handle,
                    ..
                } => handle,
                _ => panic!("cycle {}: expected EventsSubscribeStream", cycle),
            };
            assert_eq!(
                handle.initial_snapshots.len(),
                1,
                "cycle {}: snapshot must surface the run",
                cycle,
            );
            assert_eq!(
                handle.initial_snapshots[0]["run_id"],
                "wf_t22_multi",
                "cycle {}: snapshot must be the seeded run",
                cycle,
            );
            drop(handle);
            assert_eq!(
                state
                    .lock()
                    .unwrap()
                    .workflow_event_watcher
                    .subscriber_slot_count(),
                0,
                "cycle {}: handle drop must reap slot",
                cycle,
            );
        }

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    /// T_reconnect_no_accumulation — repeated dispatch + drop
    /// cycles do NOT accumulate subscriber slots. Mirror of
    /// manifest.watch's T12-equivalent (subscription leak
    /// guard). Drives 5 dispatch/drop cycles and asserts the
    /// slot count returns to zero after each.
    #[test]
    fn events_subscribe_repeated_dispatch_cycles_do_not_accumulate_slots() {
        use crate::control::protocol::{Caller, CallerOperator, Request};
        let _guard = crate::test_support::env_lock();
        let tmp = tempfile::tempdir().expect("tempdir");
        let orig_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let state = Arc::new(Mutex::new(DaemonState::new()));
        for i in 0..5 {
            let req = Request {
                id: format!("req-reconnect-{}", i),
                caller: Caller::Operator(CallerOperator {
                    token_id: "t".into(),
                }),
                method: "events.subscribe".into(),
                params: serde_json::json!({}),
            };
            let outcome = crate::control::dispatch::dispatch_request(&state, &req);
            let handle = match outcome {
                crate::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                    handle,
                    ..
                } => handle,
                _ => panic!("iter {}: expected EventsSubscribeStream", i),
            };
            assert_eq!(
                state
                    .lock()
                    .unwrap()
                    .workflow_event_watcher
                    .subscriber_slot_count(),
                1,
                "iter {}: subscribe should have produced one slot",
                i,
            );
            drop(handle);
            assert_eq!(
                state
                    .lock()
                    .unwrap()
                    .workflow_event_watcher
                    .subscriber_slot_count(),
                0,
                "iter {}: handle drop must reap slot",
                i,
            );
        }

        match orig_home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}
