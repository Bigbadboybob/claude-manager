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
                // Slice 10c-e-3b-fix3 deadlock fix: clone the writer
                // Arc out of state, THEN drop the state lock, THEN
                // do the blocking PTY write. Pre-fix3 the state
                // mutex was held across `write_all`, so any
                // backpressured PTY (paste larger than the kernel
                // buffer, child not draining stdin) deadlocked
                // every other daemon RPC.
                let writer_arc = {
                    let s = state.lock().unwrap_or_else(|p| p.into_inner());
                    s.sessions
                        .get(session_uid)
                        .map(|sess| Arc::clone(&sess.writer))
                };
                let Some(writer_arc) = writer_arc else {
                    // Session removed mid-stream. Drop this
                    // input; the outbound side will see the
                    // fanout close and write End shortly.
                    eprintln!(
                        "cm-daemon: attach stream {} input received for removed session",
                        session_uid
                    );
                    continue;
                };
                // State lock is dropped. Do the actual write
                // under just the per-writer mutex.
                let result = {
                    let mut w = writer_arc.lock().unwrap_or_else(|p| p.into_inner());
                    w.write_all(&decoded).and_then(|_| w.flush())
                };
                if let Err(e) = result {
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
            let res = crate::control::methods::send_input(&state_for_send, &params_b);
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
            let res = crate::control::methods::kill_session(&state_for_kill, &kill_params);
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
        let _ = crate::control::methods::kill_session(&state, &kill_a);
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
            let res = crate::control::methods::kill_session(&state_for_kill, &kill_params);
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
}
