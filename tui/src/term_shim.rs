//! Network-backed shim that bridges alacritty's `EventLoop` to the
//! daemon's PTY-byte stream. Slice 8 of doc/persistent-host-daemon.md.
//!
//! ## Where this fits
//!
//! Today's TUI calls `alacritty_terminal::tty::new(...)` which returns
//! a concrete PTY type. The `EventLoop` consumes that PTY, reading
//! bytes off the master fd and writing them into the `Term`. After
//! the Phase-1 split, the OS PTY lives on the daemon side; the TUI's
//! `EventLoop` instead consumes this shim, which speaks
//! length-prefixed JSON `StreamFrame`s over a dedicated attach
//! connection (Unix or TCP — same wire format either way).
//!
//! ## What this slice ships
//!
//! - [`StreamReader<R>`] — wraps any `Read` source, peels `StreamFrame`
//!   framing, and exposes the decoded PTY bytes via the standard
//!   `io::Read` interface. End-of-stream is signalled by either an
//!   `End` frame (carrying exit code + `memory_cap_kill` flag) or by
//!   the inner stream reporting EOF on a clean frame boundary. Error
//!   frames are surfaced as `io::Error::Other`. Partial-prefix EOF
//!   surfaces as `UnexpectedEof` (the doc's torn-frame distinction).
//! - [`StreamWriter<W>`] — wraps any `Write` sink. Buffers outgoing
//!   bytes as length-prefixed `StreamFrame`s and drains as far as
//!   the sink will take. Implements `io::Write`: `write()` queues a
//!   data frame and best-effort drains; `flush()` returns
//!   `WouldBlock` if the queue can't fully drain. Separate
//!   [`send_resize`](StreamWriter::send_resize) queues a
//!   `{"resize": {...}}` payload.
//! - [`ChildEvent`] — internal representation of an attach-stream exit
//!   frame. Carries both the OS exit code (alacritty's `ChildEvent`
//!   semantics) and the `memory_cap_kill` flag needed to render the
//!   "killed by memory cap" toast on detach + reattach scenarios.
//!
//! ## Nonblocking-safe state machines
//!
//! Alacritty's `EventedReadWrite` is inherently nonblocking — its
//! `EventLoop` reads on `EPOLLIN` readiness and writes on `EPOLLOUT`
//! readiness, and a `WouldBlock` partway through a frame is the
//! routine case, not the exception. Both ends keep enough state to
//! resume cleanly across `WouldBlock` boundaries:
//!
//! - `StreamReader` holds a [`ReadState`] enum: `ReadingPrefix { buf,
//!   filled }` or `ReadingBody { body, filled }`. Each call to
//!   `next_frame` reads as much as the underlying source will yield;
//!   on `WouldBlock` partway through, the state preserves `filled`
//!   so the next call resumes at the right offset. Misalignment
//!   would be silent corruption, so the FSM is essential.
//!
//! - `StreamWriter` holds a `VecDeque<u8>` outbound buffer. Every
//!   `write_frame` call appends `len_prefix || body` atomically;
//!   `flush_pending` (also called inside the `Write::write` impl)
//!   drains as much as the sink will take. A partial drain leaves
//!   the remaining bytes queued — the next call picks up mid-frame
//!   on the wire instead of emitting a fresh header.
//!
//! ## What this slice does NOT ship
//!
//! - The alacritty trait impls (`EventedReadWrite`, `EventedPty`,
//!   `event::OnResize`). Those plug this shim into the existing
//!   `EventLoop` and land in slice 10 when the TUI's
//!   `Session::new` is rewired to construct an attached `EventLoop`
//!   against a daemon connection instead of `tty::new`. Until that
//!   slice, the existing in-process PTY path keeps working.

use std::collections::VecDeque;
use std::io::{self, ErrorKind, Read, Write};

use base64::Engine;
use cm_daemon::control::protocol::{StreamFrame, StreamKind};
use serde_json::Value;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Maximum length we'll accept in a frame's length prefix before
/// refusing to allocate. 64 MiB is well above any legitimate frame
/// we generate — PTY ring buffer caps at 1 MiB by default, RPC
/// payloads (manifest snapshots, workflow state) are kilobytes — so
/// anything bigger is either a peer bug, a corrupted byte stream, or
/// a hostile sender. The trust model is local + 0600 socket so this
/// isn't a malicious-peer scenario, but a daemon crash mid-write can
/// emit garbage that decodes to a huge length; capping before the
/// allocation turns OOM-on-allocate into a clean `InvalidData` error
/// the reader's caller can recover from.
const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// Client-side mirror of the daemon's per-`Input`-frame cap (slice
/// 10c-e-3b-fix5). The daemon's `MAX_SEND_INPUT_BYTES` in
/// `daemon/src/control/methods.rs` rejects (logs + skips) any
/// inbound Input frame with payload > 64 KiB. Pre-fix5 the
/// `StreamWriter::write` impl would happily encode a single frame
/// of any size and report `Ok(buf.len())` to alacritty — the
/// daemon would then silently drop the entire oversized paste with
/// no error indication to the operator.
///
/// Fix is client-side, not daemon-side: the cap exists to bound any
/// single frame's memory footprint on the daemon side (security +
/// resource argument); if the daemon did the chunking, it'd defeat
/// the bound (a 50 MB frame would have to be received in full
/// first to chunk). Client-side chunking respects the cap and is
/// naturally lossless — large pastes split into N ≤ 64 KiB frames
/// that the daemon serializes through the per-session writer
/// mutex (slice 10c-e-3b-fix3) so they land in the PTY in order
/// without interleaving.
///
/// **Constants must agree between client and daemon.** If the
/// daemon's cap ever moves, update this constant in lockstep — the
/// `client_and_daemon_input_caps_agree` unit test below pins this
/// at build time so a drift is impossible to miss.
pub(crate) const MAX_INPUT_FRAME_BYTES: usize = 64 * 1024;

/// Reader FSM state. Persists across `next_frame` calls so a
/// `WouldBlock` partway through a frame doesn't lose bytes.
enum ReadState {
    /// Awaiting some of the 4-byte length prefix. `filled` bytes
    /// have already been read into `buf`.
    ReadingPrefix { buf: [u8; 4], filled: usize },
    /// Length parsed; awaiting body bytes. `filled` bytes into
    /// `body` are present.
    ReadingBody { body: Vec<u8>, filled: usize },
}

impl ReadState {
    /// Fresh start at a frame boundary.
    fn fresh() -> Self {
        Self::ReadingPrefix {
            buf: [0u8; 4],
            filled: 0,
        }
    }
}

/// Wraps a byte-stream `Read` source (Unix socket, TCP stream, etc.)
/// and peels `StreamFrame` framing, yielding raw PTY bytes through
/// `io::Read`. Whatever owns the `StreamReader` is responsible for
/// keeping the source alive — typically the alacritty `EventLoop`
/// holds it via the `EventedReadWrite` reader handle.
pub struct StreamReader<R: Read> {
    inner: R,
    state: ReadState,
    /// Decoded PTY bytes from the most recent data frame, not yet
    /// handed to the caller's `read` buffer.
    pending: Vec<u8>,
    /// Read cursor into `pending`.
    pending_pos: usize,
    /// Exit event surfaced once an `End` frame arrives. Drained by
    /// [`take_child_event`](Self::take_child_event); the alacritty
    /// `EventedPty` impl in slice 10 maps this to a
    /// `tty::ChildEvent::Exited(code)` and drops the
    /// `memory_cap_kill` flag into the manifest update separately.
    child_event: Option<ChildEvent>,
    /// True once any terminating frame arrives or the underlying
    /// stream signals EOF — subsequent `read` calls return `Ok(0)`.
    eof: bool,
}

/// Stream-side representation of a session exit. Carries more than
/// alacritty's `tty::ChildEvent::Exited(Option<i32>)`: we also need
/// the `memory_cap_kill` boolean so the TUI can render the cap-kill
/// toast on reattach (per the doc's "Memory-cap kill notification in
/// Phase 1" section, this is the attached-session delivery path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildEvent {
    Exited {
        code: Option<i32>,
        memory_cap_kill: bool,
        /// True when this exit was SYNTHESIZED from a bare stream EOF
        /// — the attach socket closed at a clean frame boundary
        /// WITHOUT a structured `End` frame — rather than parsed from
        /// an explicit daemon `End` frame. For a daemon-attached
        /// REMOTE session this is the ground-truth "the transport
        /// died, but the daemon-side PTY is probably still alive"
        /// signal: a genuine child exit always arrives as an `End`
        /// frame (`transport_eof = false`), whereas an SSH-tunnel
        /// death just closes the forwarded socket
        /// (`transport_eof = true`). The TUI's exit handler
        /// (`app.rs::drain_pty_events`) uses it to decide
        /// reconnect-vs-teardown for remote sessions, plumbed up
        /// through the same `Arc<AtomicBool>` side channel as
        /// `memory_cap_kill`.
        transport_eof: bool,
    },
}

impl<R: Read> StreamReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            state: ReadState::fresh(),
            pending: Vec::new(),
            pending_pos: 0,
            child_event: None,
            eof: false,
        }
    }

    /// Drain any pending child event from a previously-consumed
    /// `End` frame. Returns `None` if no exit has been observed yet
    /// or if it's already been taken.
    pub fn take_child_event(&mut self) -> Option<ChildEvent> {
        self.child_event.take()
    }

    /// Latch a synthesized transport-death exit (`transport_eof: true`,
    /// unknown code) and mark EOF. Shared by the clean-inter-frame-EOF path
    /// AND the torn-frame / raw-socket-error path: both mean the attach
    /// socket died WITHOUT a structured `End` frame, so a REMOTE session
    /// should reconnect rather than tear down. The caller surfaces this as
    /// `Ok(0)` (EOF) — crucially NOT an `Err`, because alacritty's EventLoop
    /// `break`s on a read error without emitting an exit event or polling the
    /// child-event pipe (`alacritty_terminal` `event_loop.rs` ~280), which
    /// would freeze the session. `Ok(0)` routes through the proven clean-EOF
    /// delivery path (self-pipe → `next_child_event` → `terminal.exit()` →
    /// `Event::Exit`).
    fn synthesize_transport_death_eof(&mut self) {
        self.eof = true;
        if self.child_event.is_none() {
            self.child_event = Some(ChildEvent::Exited {
                code: None,
                memory_cap_kill: false,
                transport_eof: true,
            });
        }
    }

    /// Drive the FSM forward by reading as much as the inner source
    /// will yield right now. Three outcomes:
    ///   - `Ok(Some(frame))` — a complete frame parsed; state reset
    ///     to fresh.
    ///   - `Ok(None)` — clean EOF at a frame boundary (zero bytes
    ///     left at `ReadingPrefix { filled: 0 }`).
    ///   - `Err(e)` — propagates the inner error. `WouldBlock`
    ///     preserves state for resumption; `UnexpectedEof` with
    ///     `filled > 0` is the torn-frame signal; other errors are
    ///     forwarded as-is.
    fn next_frame(&mut self) -> io::Result<Option<StreamFrame>> {
        // Drive state out of `self` so we can mutably borrow
        // `self.inner` alongside the state without aliasing.
        let mut state = std::mem::replace(&mut self.state, ReadState::fresh());
        let outcome = drive_read_fsm(&mut state, &mut self.inner);
        self.state = state;
        outcome
    }
}

/// Pure FSM driver — separate so the borrow checker can hold
/// `state` and `inner` independently. Loops across state transitions
/// within one call so a single byte-burst can complete the prefix
/// AND start consuming body bytes without re-entering.
fn drive_read_fsm<R: Read>(
    state: &mut ReadState,
    inner: &mut R,
) -> io::Result<Option<StreamFrame>> {
    loop {
        match state {
            ReadState::ReadingPrefix { buf, filled } => {
                while *filled < buf.len() {
                    match inner.read(&mut buf[*filled..]) {
                        Ok(0) => {
                            if *filled == 0 {
                                // Clean inter-frame EOF — caller
                                // closed between frames.
                                return Ok(None);
                            }
                            return Err(io::Error::new(
                                ErrorKind::UnexpectedEof,
                                format!(
                                    "torn frame: got {} of 4 length-prefix bytes before EOF",
                                    filled
                                ),
                            ));
                        }
                        Ok(n) => *filled += n,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            // FSM state preserved in *state via the
                            // outer caller. Propagate WouldBlock so
                            // alacritty's EventLoop reschedules.
                            return Err(e);
                        }
                        Err(e) => return Err(e),
                    }
                }
                // Prefix complete; transition to body. The outer
                // loop falls through to the body branch on the next
                // iteration.
                let len = u32::from_be_bytes(*buf) as usize;
                if len > MAX_FRAME_LEN {
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "frame length {} exceeds cap of {} bytes; refusing to allocate",
                            len, MAX_FRAME_LEN,
                        ),
                    ));
                }
                *state = ReadState::ReadingBody {
                    body: vec![0u8; len],
                    filled: 0,
                };
            }
            ReadState::ReadingBody { body, filled } => {
                while *filled < body.len() {
                    match inner.read(&mut body[*filled..]) {
                        Ok(0) => {
                            // Body EOF is unambiguously a torn
                            // frame — we know there's more to read
                            // because the prefix said so.
                            return Err(io::Error::new(
                                ErrorKind::UnexpectedEof,
                                format!(
                                    "torn body: got {} of {} bytes before EOF",
                                    filled,
                                    body.len()
                                ),
                            ));
                        }
                        Ok(n) => *filled += n,
                        Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == ErrorKind::WouldBlock => return Err(e),
                        Err(e) => return Err(e),
                    }
                }
                // Body complete — parse, reset state, and return.
                let body_owned = std::mem::take(body);
                *state = ReadState::fresh();
                let frame: StreamFrame = serde_json::from_slice(&body_owned)
                    .map_err(|e| io::Error::new(ErrorKind::InvalidData, e))?;
                return Ok(Some(frame));
            }
        }
    }
}

impl<R: Read> Read for StreamReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // `Read` contract: a zero-length read must not consume any
        // input. Without this guard, `next_frame` could decode a
        // frame and stash payload bytes in `pending` that the caller
        // wasn't asking for — meaningful when an EventLoop probes
        // readiness via a sentinel call.
        if buf.is_empty() {
            return Ok(0);
        }
        if self.eof {
            return Ok(0);
        }

        // Serve from pending first.
        if self.pending_pos < self.pending.len() {
            let avail = self.pending.len() - self.pending_pos;
            let to_copy = avail.min(buf.len());
            buf[..to_copy].copy_from_slice(
                &self.pending[self.pending_pos..self.pending_pos + to_copy],
            );
            self.pending_pos += to_copy;
            return Ok(to_copy);
        }

        // Pending exhausted — pull frames until we get something to
        // return, hit a terminator, or run out of immediately-
        // available bytes (WouldBlock).
        loop {
            let frame = match self.next_frame() {
                Ok(Some(f)) => f,
                Ok(None) => {
                    // Clean inter-frame EOF — the daemon vanished /
                    // restarted / closed the attach socket at a frame
                    // boundary without writing a structured exit. We
                    // don't know exit_code or memory_cap_kill; the
                    // "daemon-vanished" exit looks identical to a
                    // SIGKILL from outside. Synthesize the transport-
                    // death exit (slice-10c-e-2 review-5 fix #2a +
                    // remote-reconnect).
                    self.synthesize_transport_death_eof();
                    return Ok(0);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // Transient: no data available right now. `next_frame`
                    // preserved the partial-frame FSM state, so surface
                    // WouldBlock and let alacritty's EventLoop reschedule.
                    // NOT a transport death.
                    return Err(e);
                }
                Err(e) if e.kind() == ErrorKind::InvalidData => {
                    // Protocol error (oversized frame length / malformed
                    // JSON body) — a genuine error, not a transport death.
                    // Surface it unchanged.
                    return Err(e);
                }
                Err(_e) => {
                    // Mid-frame EOF (a TORN frame — the socket closed
                    // partway through a length prefix or body) or a raw
                    // socket error (ECONNRESET / EPIPE / …). Semantically
                    // identical to the clean-EOF case above: the transport
                    // died WITHOUT a structured `End` frame; only the close
                    // landed mid-frame instead of at a boundary. This is the
                    // ACTIVELY-STREAMING disconnect — a session producing
                    // output when connectivity drops is almost always
                    // mid-frame, not idle-at-boundary — so it's the COMMON
                    // case, not an edge case. Synthesize the SAME
                    // transport-death exit and return `Ok(0)` (NOT the
                    // `Err`) so the reconnect flag latches here too. (A
                    // genuine daemon-side child exit always arrives as a
                    // structured `End` frame, so it's still never
                    // misclassified as transport death.)
                    self.synthesize_transport_death_eof();
                    return Ok(0);
                }
            };
            match frame.kind {
                StreamKind::Data => {
                    if let Some(b64) = frame.payload.get("bytes").and_then(Value::as_str) {
                        let decoded = BASE64.decode(b64).map_err(|e| {
                            io::Error::new(ErrorKind::InvalidData, e)
                        })?;
                        if decoded.is_empty() {
                            // Empty data frame — keep reading.
                            continue;
                        }
                        self.pending = decoded;
                        self.pending_pos = 0;
                        let avail = self.pending.len();
                        let to_copy = avail.min(buf.len());
                        buf[..to_copy].copy_from_slice(&self.pending[..to_copy]);
                        self.pending_pos = to_copy;
                        return Ok(to_copy);
                    } else {
                        // Reader-direction data frames are
                        // server→client; an unexpected non-bytes
                        // payload is a wire-protocol bug rather
                        // than a runtime condition. Treat as
                        // malformed.
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "server data frame missing `bytes` payload",
                        ));
                    }
                }
                StreamKind::End => {
                    let code = frame
                        .payload
                        .get("exit_code")
                        .and_then(Value::as_i64)
                        .map(|i| i as i32);
                    let memory_cap_kill = frame
                        .payload
                        .get("memory_cap_kill")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    self.child_event = Some(ChildEvent::Exited {
                        code,
                        memory_cap_kill,
                        // Structured `End` frame → a genuine
                        // daemon-side child exit, NOT a transport
                        // death. The exit handler tears the session
                        // down normally (no reconnect).
                        transport_eof: false,
                    });
                    self.eof = true;
                    return Ok(0);
                }
                StreamKind::Error => {
                    let msg = frame
                        .payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("attach stream error")
                        .to_string();
                    self.eof = true;
                    return Err(io::Error::other(msg));
                }
                // `Input` and `Resize` are CLIENT→SERVER kinds
                // (slice 10c-e-2 review fix). The client should
                // never see them on the inbound side; treat as a
                // wire-protocol bug. Defensive: log via
                // io::Error and close.
                StreamKind::Input | StreamKind::Resize => {
                    self.eof = true;
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "client received server-direction kind {:?} on attach stream",
                            frame.kind
                        ),
                    ));
                }
                // 10e-b: `ManifestSnapshot` and `ManifestDiff`
                // belong to the `manifest.watch` stream, NOT the
                // PTY-attach stream this `term_shim` decodes.
                // Reaching this arm means the wrong stream is
                // wired to this reader — surface as malformed.
                // 10e-b r1: `Heartbeat` likewise — manifest.watch
                // sends them on idle, but PTY attach never does.
                // 11b: `WorkflowEventStateSnapshot` /
                // `WorkflowEvent` belong to `events.subscribe`,
                // same rationale.
                StreamKind::ManifestSnapshot
                | StreamKind::ManifestDiff
                | StreamKind::Heartbeat
                | StreamKind::WorkflowEventStateSnapshot
                | StreamKind::WorkflowEvent => {
                    self.eof = true;
                    return Err(io::Error::new(
                        ErrorKind::InvalidData,
                        format!(
                            "client received manifest-watch kind {:?} on PTY-attach stream",
                            frame.kind
                        ),
                    ));
                }
            }
        }
    }
}

/// Wraps a byte-stream `Write` sink and buffers outgoing
/// `StreamFrame`s so partial writes don't corrupt the wire.
///
/// `write_frame` appends `len_prefix || body` to an internal
/// `VecDeque<u8>`. `flush_pending` (also invoked from inside the
/// `Write::write` impl) drains as much as the sink will take. The
/// frame is only logically "on the wire" once the queue drains past
/// its tail — and crucially, no partial header is ever emitted
/// followed by a fresh one, because all bytes go through the same
/// queue in append order.
pub struct StreamWriter<W: Write> {
    inner: W,
    stream_id: String,
    outbound: VecDeque<u8>,
}

impl<W: Write> StreamWriter<W> {
    pub fn new(inner: W, stream_id: impl Into<String>) -> Self {
        Self {
            inner,
            stream_id: stream_id.into(),
            outbound: VecDeque::new(),
        }
    }

    /// Append a frame to the outbound queue. Does NOT drain — call
    /// `flush_pending` or `write` afterwards to push it toward the
    /// inner sink. Separated so multiple frames can be queued
    /// atomically before a drain attempt.
    fn queue_frame(&mut self, kind: StreamKind, payload: Value) -> io::Result<()> {
        let frame = StreamFrame {
            id: self.stream_id.clone(),
            kind,
            payload,
        };
        let body = serde_json::to_vec(&frame).map_err(io::Error::other)?;
        let len = (body.len() as u32).to_be_bytes();
        self.outbound.extend(&len);
        self.outbound.extend(&body);
        Ok(())
    }

    /// Send a winsize update. Uses `StreamKind::Resize` (slice
    /// 10c-e-2 review: the per-direction kind set replaced the
    /// previous Data-with-payload-discriminator scheme). Payload:
    /// `{"cols": N, "rows": N}`. Best-effort drains after
    /// queueing; if the sink returns `WouldBlock`, the frame stays
    /// in the queue and a later `flush_pending` (or another
    /// `write`) finishes it.
    pub fn send_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.queue_frame(
            StreamKind::Resize,
            serde_json::json!({ "cols": cols, "rows": rows }),
        )?;
        match self.flush_pending() {
            Ok(_) | Err(_) if false => unreachable!(),
            Ok(_) => Ok(()),
            // WouldBlock just means the queue isn't fully drained
            // yet; future calls will finish it. Errors other than
            // WouldBlock surface to the caller — the sink is dead
            // or misbehaving.
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Drain the outbound queue as far as `self.inner` will take.
    ///
    /// Returns:
    ///   - `Ok(true)` — queue is now empty; the last frame is fully
    ///     on the wire.
    ///   - `Ok(false)` — queue still has bytes (sink returned
    ///     `WouldBlock` or accepted 0 bytes on a partial write).
    ///   - `Err(e)` — sink error other than `WouldBlock` /
    ///     `Interrupted`. Queued bytes remain in the buffer; the
    ///     caller usually drops the shim.
    pub fn flush_pending(&mut self) -> io::Result<bool> {
        while !self.outbound.is_empty() {
            // VecDeque's contiguous front; if empty, fall through
            // to the back.
            let (front, back) = self.outbound.as_slices();
            let chunk = if !front.is_empty() { front } else { back };
            match self.inner.write(chunk) {
                Ok(0) => {
                    // Sink accepted 0 bytes despite being asked.
                    // Typical cause: peer closed. Surface as
                    // WriteZero so callers know to give up on this
                    // shim rather than retrying forever.
                    return Err(io::Error::new(
                        ErrorKind::WriteZero,
                        "stream writer accepted 0 bytes",
                    ));
                }
                Ok(n) => {
                    self.outbound.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    return Ok(false);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Number of bytes still queued for the inner sink. Useful for
    /// tests and for callers that want to detect backpressure (a
    /// growing queue means the sink isn't keeping up).
    pub fn pending_bytes(&self) -> usize {
        self.outbound.len()
    }
}

// STATIC-ANALYSIS-FYI (Shape B vs Shape A): the
// `Ok(buf.len())` return at the bottom of `write` below
// completes without guaranteeing that all queued bytes
// reached the wire. The opportunistic drain that handles
// the tail lives on the INBOUND side — `AttachedPty`'s
// `drain_pending` fires on `reader()` / `register` /
// `reregister` / `next_child_event`. See `attached_pty.rs::
// drain_pending`'s long doc for the full Shape B vs Shape A
// debate and the standing rejection (rounds 26, 30, 31, 33).
// This is intentional, not a missing piece. Quiescent-session
// caveat is the documented tradeoff — escalate to Shape A
// (per-attach writer thread) only on smoke evidence of stuck
// input in practice. Do not "fix" preemptively. If you're a
// future reviewer pattern-matching on the `Ok(buf.len())`
// shape, read `drain_pending`'s doc and the NOTES.md
// "Rejected findings" subsection before flagging.
impl<W: Write> Write for StreamWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Empty writes are a no-op rather than triggering an empty
        // data frame — the latter would waste bandwidth and the
        // reader-side already skips empty frames.
        if buf.is_empty() {
            return Ok(0);
        }

        // Drain any pending bytes from a previous partial write
        // BEFORE accepting new bytes. Slice-10c-e-2 review-5 fix
        // #1: the prior implementation returned `Ok(buf.len())`
        // regardless of whether the bytes actually drained,
        // making alacritty's EventLoop think the write was done
        // when there were still bytes queued — keystrokes
        // silently stalled under socket backpressure.
        //
        // If the queue has pending bytes and the sink is fully
        // blocked, surface backpressure to the caller via
        // `WouldBlock`. The caller (alacritty's EventLoop) holds
        // onto the bytes in its own buffer and re-polls for
        // writable readiness. Our `AttachedPty::reregister`
        // override adds writable interest while
        // `pending_bytes() > 0` so the kernel keeps delivering
        // writable events even after alacritty's own state.write
        // empties.
        match self.flush_pending() {
            Ok(true) => {
                // Queue empty; safe to accept new bytes.
            }
            Ok(false) => {
                // Queue still has pending bytes from a prior
                // partial drain. Don't accept new bytes;
                // alacritty must retry.
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "outbound queue has pending bytes; retry on writable readiness",
                ));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                // Same backpressure shape — flush_pending returns
                // Err(WouldBlock) only when the sink itself
                // signalled WouldBlock mid-drain. Bytes remain
                // queued; surface to caller.
                return Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "outbound queue has pending bytes; retry on writable readiness",
                ));
            }
            Err(e) => return Err(e),
        }

        // Queue is empty. Encode buf as one or more Input
        // frames — each ≤ MAX_INPUT_FRAME_BYTES so the daemon
        // doesn't silently reject any of them (slice
        // 10c-e-3b-fix5).
        //
        // The chunks are queued atomically (all frames append to
        // the same outbound VecDeque before any drain), so a
        // partial socket drain doesn't intersperse another
        // session's frames between chunks of this paste. The
        // daemon's per-session writer mutex (slice 10c-e-3b-fix3)
        // serializes the inbound chunks too — they hit the PTY in
        // the same order, with no interleaving from concurrent
        // send_input RPCs. End-to-end: a 200 KiB paste from
        // alacritty arrives at the child process as 200 KiB,
        // bytewise identical, in order.
        //
        // Client→server bytes use the `Input` kind (slice 10c-e-2
        // review). The daemon's bidirectional attach-stream reader
        // dispatches `Input` frames to `DaemonSession::send_input`.
        for chunk in buf.chunks(MAX_INPUT_FRAME_BYTES) {
            let encoded = BASE64.encode(chunk);
            self.queue_frame(
                StreamKind::Input,
                serde_json::json!({ "bytes": encoded }),
            )?;
        }
        // Best-effort drain. If the sink returns WouldBlock
        // partway through, bytes stay queued — the NEXT write()
        // call will hit the queue-has-pending branch above and
        // surface backpressure. Either way `buf` has been
        // accepted into our system, so report Ok(buf.len()).
        match self.flush_pending() {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        // STATIC-ANALYSIS-FYI: yes, this returns Ok(buf.len())
        // without guaranteeing the queue fully drained. The tail
        // is drained opportunistically on inbound EventLoop calls
        // — see `attached_pty.rs::drain_pending` (Shape B) and
        // its standing rejection comment (rounds 26, 30, 31, 33).
        // Rejected five times; sixth pass should reject in place.
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.flush_pending() {
            Ok(true) => self.inner.flush(),
            Ok(false) => Err(io::Error::new(
                ErrorKind::WouldBlock,
                "outbound buffer not fully drained; caller should retry on EPOLLOUT",
            )),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};

    // -------------------------------------------------------------
    // Fakes for byte-at-a-time / WouldBlock / partial-write tests.
    // -------------------------------------------------------------

    /// `Read` source whose delivery is dictated by a pre-loaded
    /// script. Lets tests rehearse exactly how an nonblocking socket
    /// would behave: bytes arrive in chunks of any size, interleaved
    /// with `WouldBlock` / `Interrupted` errors, eventually followed
    /// by EOF.
    ///
    /// Reads work like a real socket: every call drains up to
    /// `buf.len()` from the currently-available bytes, and when the
    /// available buffer empties the next scripted outcome is
    /// consumed. A `push_bytes(chunk)` outcome doesn't atomically
    /// deliver `chunk` in one call — the FSM might consume it across
    /// many `read()` calls if `buf` is smaller — which matches real
    /// sockets where the kernel keeps bytes buffered until consumed.
    struct ScriptedReader {
        available: VecDeque<u8>,
        upcoming: VecDeque<ReadOutcome>,
    }
    enum ReadOutcome {
        Bytes(Vec<u8>),
        Err(io::Error),
    }
    impl ScriptedReader {
        fn new() -> Self {
            Self {
                available: VecDeque::new(),
                upcoming: VecDeque::new(),
            }
        }
        /// Enqueue bytes to be delivered. Multiple reads may consume
        /// these (size-dependent on the caller's `buf`). The next
        /// outcome is only popped once these are fully drained.
        fn push_bytes(&mut self, bytes: Vec<u8>) {
            self.upcoming.push_back(ReadOutcome::Bytes(bytes));
        }
        /// Enqueue one read() outcome that returns WouldBlock. Pops
        /// only when previously-buffered bytes are exhausted.
        fn push_block(&mut self) {
            self.upcoming.push_back(ReadOutcome::Err(io::Error::new(
                ErrorKind::WouldBlock,
                "scripted",
            )));
        }
        /// Enqueue one read() outcome that returns Interrupted —
        /// must be transparently retried by the FSM.
        fn push_interrupted(&mut self) {
            self.upcoming.push_back(ReadOutcome::Err(io::Error::new(
                ErrorKind::Interrupted,
                "scripted",
            )));
        }
    }
    impl Read for ScriptedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Refill from upcoming when available is empty. We only
            // consume one outcome per call so a WouldBlock between
            // two byte-chunks surfaces before the next chunk gets
            // mingled in.
            if self.available.is_empty() {
                match self.upcoming.pop_front() {
                    Some(ReadOutcome::Bytes(bytes)) => self.available.extend(bytes),
                    Some(ReadOutcome::Err(e)) => return Err(e),
                    None => return Ok(0), // EOF
                }
            }
            let n = self.available.len().min(buf.len());
            for slot in buf.iter_mut().take(n) {
                *slot = self.available.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    /// `Write` sink whose every call accepts up to N bytes per the
    /// scripted outcome. Lets tests rehearse partial drains
    /// interleaved with WouldBlock.
    struct ScriptedWriter {
        /// Bytes actually accepted, in order.
        accepted: Vec<u8>,
        /// Per-call quotas. `Ok(n)` = accept up to n bytes from the
        /// next write call. `Err` = return that error.
        outcomes: VecDeque<io::Result<usize>>,
    }
    impl ScriptedWriter {
        fn new() -> Self {
            Self {
                accepted: Vec::new(),
                outcomes: VecDeque::new(),
            }
        }
        fn push_quota(&mut self, max: usize) {
            self.outcomes.push_back(Ok(max));
        }
        fn push_block(&mut self) {
            self.outcomes
                .push_back(Err(io::Error::new(ErrorKind::WouldBlock, "scripted")));
        }
    }
    impl Write for ScriptedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self.outcomes.pop_front() {
                Some(Ok(max)) => {
                    let n = buf.len().min(max);
                    self.accepted.extend_from_slice(&buf[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                None => Err(io::Error::new(
                    ErrorKind::WouldBlock,
                    "no scripted outcome",
                )),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Compose a single length-prefixed StreamFrame on the wire.
    fn frame_bytes(kind: StreamKind, payload: Value) -> Vec<u8> {
        let frame = StreamFrame {
            id: "stream-1".into(),
            kind,
            payload,
        };
        let body = serde_json::to_vec(&frame).unwrap();
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    /// Compose a data frame from raw PTY bytes (base64-encoded).
    fn data_frame(payload_bytes: &[u8]) -> Vec<u8> {
        frame_bytes(
            StreamKind::Data,
            serde_json::json!({ "bytes": BASE64.encode(payload_bytes) }),
        )
    }

    // --- StreamReader: blocking-style happy path -------------------

    #[test]
    fn reader_zero_length_buf_returns_zero_without_consuming() {
        // Per the `Read` contract, a 0-length buf must yield Ok(0)
        // and not advance the source. The frame must still be
        // available on the next non-empty read.
        let mut scripted = ScriptedReader::new();
        scripted.push_bytes(data_frame(b"intact"));
        let mut r = StreamReader::new(scripted);
        assert_eq!(r.read(&mut []).unwrap(), 0);
        let mut buf = [0u8; 32];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"intact");
    }

    #[test]
    fn reader_empty_inner_yields_eof_and_synthesized_child_event() {
        // Post slice-10c-e-2 review-5 fix #2a: a clean EOF
        // before any frame arrives synthesizes a
        // `ChildEvent::Exited { code: None, memory_cap_kill: false }`
        // so the TUI's EventLoop notices the daemon-vanished
        // case. Without this, the session pane stays alive
        // indefinitely after the daemon closes the socket
        // without writing an End frame.
        let mut r = StreamReader::new(Cursor::new(Vec::<u8>::new()));
        let mut buf = [0u8; 16];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        match r.take_child_event() {
            Some(ChildEvent::Exited { code, memory_cap_kill, transport_eof }) => {
                assert_eq!(code, None, "EOF-without-End surfaces unknown exit_code");
                assert!(!memory_cap_kill, "EOF-without-End cannot attribute memory_cap_kill");
                assert!(
                    transport_eof,
                    "a bare EOF with no End frame is a transport death — \
                     this is the signal the remote-reconnect path keys off",
                );
            }
            None => panic!("clean EOF must synthesize a ChildEvent so the TUI observes exit"),
        }
        // Subsequent calls return None (drained).
        assert!(r.take_child_event().is_none());
    }

    #[test]
    fn reader_single_data_frame_yields_decoded_bytes() {
        let wire = data_frame(b"hello");
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 16];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn reader_multiple_data_frames_decode_in_sequence() {
        let mut wire = data_frame(b"part-A ");
        wire.extend(data_frame(b"part-B"));
        let mut r = StreamReader::new(Cursor::new(wire));

        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, b"part-A part-B");
    }

    #[test]
    fn reader_buf_smaller_than_payload_uses_multiple_reads() {
        let wire = data_frame(b"abcdefghij"); // 10 bytes
        let mut r = StreamReader::new(Cursor::new(wire));

        let mut buf = [0u8; 4];
        let n1 = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n1], b"abcd");
        let n2 = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n2], b"efgh");
        let n3 = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n3], b"ij");
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn reader_end_frame_surfaces_exit_event() {
        let mut wire = data_frame(b"output\n");
        wire.extend(frame_bytes(
            StreamKind::End,
            serde_json::json!({ "exit_code": 0, "memory_cap_kill": false }),
        ));
        let mut r = StreamReader::new(Cursor::new(wire));

        let mut buf = [0u8; 32];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"output\n");
        assert_eq!(r.read(&mut buf).unwrap(), 0);

        let ev = r.take_child_event().expect("child event after End");
        assert_eq!(
            ev,
            ChildEvent::Exited {
                code: Some(0),
                memory_cap_kill: false,
                // A structured End frame is a genuine exit, never a
                // transport death.
                transport_eof: false,
            }
        );
        assert!(r.take_child_event().is_none());
    }

    #[test]
    fn reader_end_frame_with_memory_cap_kill_flag_propagates() {
        let wire = frame_bytes(
            StreamKind::End,
            serde_json::json!({ "exit_code": 137, "memory_cap_kill": true }),
        );
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 16];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        assert_eq!(
            r.take_child_event().unwrap(),
            ChildEvent::Exited {
                code: Some(137),
                memory_cap_kill: true,
                transport_eof: false,
            }
        );
    }

    #[test]
    fn reader_end_frame_without_exit_code_defaults_to_none() {
        let wire = frame_bytes(StreamKind::End, serde_json::json!({}));
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 4];
        assert_eq!(r.read(&mut buf).unwrap(), 0);
        assert_eq!(
            r.take_child_event().unwrap(),
            ChildEvent::Exited {
                code: None,
                memory_cap_kill: false,
                // Explicit End frame (with an absent exit_code) is
                // still a genuine exit, not a transport death.
                transport_eof: false,
            }
        );
    }

    #[test]
    fn reader_error_frame_returns_io_error_other() {
        let wire = frame_bytes(
            StreamKind::Error,
            serde_json::json!({ "message": "daemon closed the stream" }),
        );
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 16];
        let err = r.read(&mut buf).expect_err("error frame must surface");
        assert!(err.to_string().contains("daemon closed the stream"));
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn reader_malformed_json_payload_is_invalid_data() {
        let body = b"not json {";
        let mut wire = (body.len() as u32).to_be_bytes().to_vec();
        wire.extend(body);
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 16];
        let err = r.read(&mut buf).expect_err("malformed frame must error");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn reader_data_frame_missing_bytes_field_is_invalid_data() {
        let wire = frame_bytes(
            StreamKind::Data,
            serde_json::json!({ "resize": { "cols": 80, "rows": 24 } }),
        );
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 8];
        let err = r.read(&mut buf).expect_err("missing bytes must error");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn reader_partial_frame_truncated_at_body() {
        // A TORN frame (socket closed mid-body) is a TRANSPORT DEATH, not a
        // surfaced error. Returning Err would freeze the session — alacritty
        // `break`s its EventLoop on a read error without emitting an exit or
        // polling the child-event pipe. So the reader yields Ok(0) and
        // latches a transport-death child event → the TUI reconnects.
        let prefix = (50u32).to_be_bytes();
        let truncated_body = b"short";
        let mut wire = prefix.to_vec();
        wire.extend(truncated_body);
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 8];
        assert_eq!(
            r.read(&mut buf).unwrap(),
            0,
            "a torn body surfaces as EOF, not an error",
        );
        match r.take_child_event() {
            Some(ChildEvent::Exited { transport_eof, code, .. }) => {
                assert!(
                    transport_eof,
                    "a torn frame is a transport death → must flag reconnect",
                );
                assert_eq!(code, None, "torn frame has no structured exit code");
            }
            other => panic!(
                "torn frame must synthesize a transport-death exit, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn reader_clean_close_at_frame_boundary_returns_eof_not_error() {
        let wire = data_frame(b"complete");
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 16];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"complete");
        assert_eq!(r.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn reader_partial_length_prefix_is_transport_death() {
        // Socket closed partway through the 4-byte length prefix → torn
        // frame → transport death (Ok(0) + transport_eof), NOT a surfaced
        // error (which would freeze an actively-streaming session).
        let mut wire = (42u32).to_be_bytes().to_vec();
        wire.truncate(2);
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 8];
        assert_eq!(
            r.read(&mut buf).unwrap(),
            0,
            "a partial length prefix surfaces as EOF, not an error",
        );
        match r.take_child_event() {
            Some(ChildEvent::Exited { transport_eof, .. }) => assert!(
                transport_eof,
                "a torn length prefix is a transport death → must flag reconnect",
            ),
            other => panic!("expected transport-death exit, got {:?}", other),
        }
    }

    #[test]
    fn reader_oversized_frame_length_errors_before_allocating() {
        let len = (MAX_FRAME_LEN as u32) + 1;
        let wire = len.to_be_bytes().to_vec();
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 8];
        let err = r
            .read(&mut buf)
            .expect_err("oversized length must error before allocating");
        assert_eq!(err.kind(), ErrorKind::InvalidData);
        assert!(err.to_string().contains("exceeds cap"));
    }

    #[test]
    fn reader_frame_length_at_cap_is_accepted() {
        // A length exactly AT the cap is ACCEPTED (not rejected like an
        // oversized one): the reader proceeds to read the body, which is
        // absent here, so it surfaces as a torn-frame transport death
        // (Ok(0) + transport_eof) rather than an "exceeds cap" InvalidData
        // error — distinguishing it from
        // `reader_oversized_frame_length_errors_before_allocating`.
        let len = MAX_FRAME_LEN as u32;
        let wire = len.to_be_bytes().to_vec();
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 8];
        assert_eq!(
            r.read(&mut buf).unwrap(),
            0,
            "at-cap length is accepted → reads body → absent → torn → EOF",
        );
        match r.take_child_event() {
            Some(ChildEvent::Exited { transport_eof, .. }) => assert!(
                transport_eof,
                "the at-cap length was accepted and the missing body torn",
            ),
            other => panic!("expected transport-death exit, got {:?}", other),
        }
    }

    #[test]
    fn reader_empty_data_frame_is_skipped() {
        let mut wire = data_frame(b"");
        wire.extend(data_frame(b"after-empty"));
        let mut r = StreamReader::new(Cursor::new(wire));
        let mut buf = [0u8; 32];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"after-empty");
    }

    // --- StreamReader: nonblocking FSM behavior --------------------

    #[test]
    fn reader_would_block_mid_prefix_preserves_state() {
        // Deliver 2 prefix bytes, then WouldBlock, then the rest of
        // the frame as one big chunk. The reader must resume the
        // prefix where it left off — without the FSM, the next call
        // would mistake bytes 3–4 as a fresh prefix.
        let wire = data_frame(b"hello");
        let mut scripted = ScriptedReader::new();
        scripted.push_bytes(wire[0..2].to_vec()); // prefix bytes 0..2
        scripted.push_block();
        scripted.push_bytes(wire[2..].to_vec()); // prefix bytes 2..4 + whole body
        let mut r = StreamReader::new(scripted);
        let mut buf = [0u8; 32];

        // First call: 2 prefix bytes arrive, then WouldBlock.
        let err = r.read(&mut buf).expect_err("must surface WouldBlock");
        assert_eq!(err.kind(), ErrorKind::WouldBlock);

        // Second call: remaining prefix + body arrive; we get the
        // decoded payload.
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
    }

    #[test]
    fn reader_would_block_mid_body_preserves_state() {
        let wire = data_frame(b"hello world this is a longer payload");
        // Send the prefix in one chunk, half the body, WouldBlock,
        // rest.
        let split = wire.len() / 2;
        let mut scripted = ScriptedReader::new();
        scripted.push_bytes(wire[..4].to_vec()); // full prefix
        scripted.push_bytes(wire[4..split].to_vec()); // partial body
        scripted.push_block();
        scripted.push_bytes(wire[split..].to_vec()); // rest of body
        let mut r = StreamReader::new(scripted);

        let mut buf = [0u8; 64];
        // First call: prefix completes, body starts but blocks
        // partway. WouldBlock propagates.
        let err = r.read(&mut buf).expect_err("WouldBlock");
        assert_eq!(err.kind(), ErrorKind::WouldBlock);
        // Second call: body completes; we get the full payload.
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello world this is a longer payload");
    }

    #[test]
    fn reader_byte_at_a_time_multi_frame_stream() {
        // Worst case from the reviewer's spec: deliver three data
        // frames one byte at a time, interleaving an occasional
        // WouldBlock. The reader must reassemble every payload
        // perfectly with no misalignment.
        let mut wire = Vec::new();
        wire.extend(data_frame(b"alpha"));
        wire.extend(data_frame(b"beta"));
        wire.extend(data_frame(b"gamma-payload"));

        let mut scripted = ScriptedReader::new();
        for (i, b) in wire.iter().enumerate() {
            scripted.push_bytes(vec![*b]);
            // Sprinkle WouldBlocks and an Interrupted to exercise
            // both transient cases.
            if i % 7 == 3 {
                scripted.push_block();
            }
            if i % 11 == 5 {
                scripted.push_interrupted();
            }
        }

        let mut r = StreamReader::new(scripted);
        let mut out = Vec::new();
        let mut buf = [0u8; 32];
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(e) => panic!("unexpected error: {}", e),
            }
        }
        assert_eq!(out, b"alphabetagamma-payload");
    }

    #[test]
    fn reader_interrupted_during_prefix_is_transparent() {
        // ErrorKind::Interrupted is a transient OS hiccup, not a
        // partial-read. The FSM must retry inline rather than
        // bubbling it up.
        let wire = data_frame(b"survives");
        let mut scripted = ScriptedReader::new();
        scripted.push_bytes(wire[..1].to_vec());
        scripted.push_interrupted();
        scripted.push_bytes(wire[1..].to_vec());
        let mut r = StreamReader::new(scripted);
        let mut buf = [0u8; 32];
        let n = r.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"survives");
    }

    // --- StreamWriter: queued framing & partial drains -------------

    /// Decode all length-prefixed StreamFrames from a byte
    /// buffer. Used to assert against `StreamWriter` output
    /// directly — `StreamReader` can't loopback-decode writer
    /// output anymore because writer emits `Input` kind
    /// (client→server) while reader only accepts `Data`/`End`/
    /// `Error` kinds (server→client).
    fn decode_all_frames(sink: &[u8]) -> Vec<StreamFrame> {
        let mut out = Vec::new();
        let mut offset = 0;
        while offset + 4 <= sink.len() {
            let len = u32::from_be_bytes([
                sink[offset], sink[offset + 1], sink[offset + 2], sink[offset + 3],
            ]) as usize;
            offset += 4;
            if offset + len > sink.len() {
                break;
            }
            let frame: StreamFrame =
                serde_json::from_slice(&sink[offset..offset + len]).unwrap();
            out.push(frame);
            offset += len;
        }
        out
    }

    #[test]
    fn writer_frames_a_single_write_as_input_kind() {
        // Post slice-10c-e-2 review: client→server bytes use the
        // `Input` kind (vs. server→client `Data`). Decode the
        // wire frame directly instead of round-tripping through
        // StreamReader (which only accepts server-direction
        // kinds).
        let mut sink = Vec::new();
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        let n = w.write(b"hi").unwrap();
        assert_eq!(n, 2);
        drop(w);
        let frames = decode_all_frames(&sink);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, StreamKind::Input);
        let b64 = frames[0].payload["bytes"].as_str().unwrap();
        let decoded = BASE64.decode(b64).unwrap();
        assert_eq!(decoded, b"hi");
    }

    #[test]
    fn writer_round_trips_multiple_writes_as_input_frames() {
        let mut sink = Vec::new();
        {
            let mut w = StreamWriter::new(&mut sink, "stream-1");
            assert_eq!(w.write(b"one ").unwrap(), 4);
            assert_eq!(w.write(b"two ").unwrap(), 4);
            assert_eq!(w.write(b"three").unwrap(), 5);
        }
        let frames = decode_all_frames(&sink);
        assert_eq!(frames.len(), 3);
        let mut concatenated = Vec::new();
        for f in &frames {
            assert_eq!(f.kind, StreamKind::Input);
            let b64 = f.payload["bytes"].as_str().unwrap();
            concatenated.extend(BASE64.decode(b64).unwrap());
        }
        assert_eq!(concatenated, b"one two three");
    }

    #[test]
    fn writer_empty_write_is_noop() {
        let mut sink = Vec::new();
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        assert_eq!(w.write(b"").unwrap(), 0);
        assert!(sink.is_empty(), "empty write must not emit a frame");
    }

    #[test]
    fn writer_send_resize_frames_resize_payload_with_resize_kind() {
        // Post slice-10c-e-2 review: resize uses its own
        // `StreamKind::Resize` variant, payload `{cols, rows}`
        // (not the previous `Data` with `{resize: {…}}` shape).
        let mut sink = Vec::new();
        {
            let mut w = StreamWriter::new(&mut sink, "stream-1");
            w.send_resize(120, 40).unwrap();
        }
        let frames = decode_all_frames(&sink);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, StreamKind::Resize);
        assert_eq!(frames[0].payload["cols"], 120);
        assert_eq!(frames[0].payload["rows"], 40);
        assert!(frames[0].payload.get("bytes").is_none());
    }

    #[test]
    fn writer_uses_provided_stream_id_on_every_frame() {
        let mut sink = Vec::new();
        {
            let mut w = StreamWriter::new(&mut sink, "attach-uuid-xyz");
            w.write(b"x").unwrap();
            w.send_resize(80, 24).unwrap();
        }
        let mut cursor = Cursor::new(sink.as_slice());
        for _ in 0..2 {
            let mut len_buf = [0u8; 4];
            cursor.read_exact(&mut len_buf).unwrap();
            let len = u32::from_be_bytes(len_buf) as usize;
            let mut body = vec![0u8; len];
            cursor.read_exact(&mut body).unwrap();
            let frame: StreamFrame = serde_json::from_slice(&body).unwrap();
            assert_eq!(frame.id, "attach-uuid-xyz");
        }
    }

    #[test]
    fn writer_partial_drain_keeps_remainder_queued() {
        // Sink accepts only the first 3 bytes per write; writer must
        // keep the rest queued for a later drain.
        let mut sink = ScriptedWriter::new();
        sink.push_quota(3);
        sink.push_block();
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        // queue_frame + flush_pending; flush_pending sees the
        // 3-byte quota then WouldBlock.
        let n = w.write(b"hi").unwrap();
        assert_eq!(n, 2);
        assert!(
            w.pending_bytes() > 0,
            "remainder of frame must still be queued"
        );
    }

    #[test]
    fn writer_drains_across_multiple_partial_writes() {
        // Scripted sink accepts 1 byte at a time, with an occasional
        // WouldBlock. The writer must reconstruct the full frame
        // on the wire across many flush_pending calls.
        let mut sink = ScriptedWriter::new();
        // Enough byte-quotas to drain a small frame fully.
        for i in 0..120 {
            sink.push_quota(1);
            if i % 5 == 3 {
                sink.push_block();
            }
        }
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        // Queue one frame.
        w.write(b"payload").unwrap();

        // Drain in a loop, ignoring WouldBlock until the queue is
        // empty (or we run out of scripted quotas).
        loop {
            match w.flush_pending() {
                Ok(true) => break,
                Ok(false) => continue,
                Err(e) => panic!("unexpected error during drain: {}", e),
            }
        }
        assert_eq!(w.pending_bytes(), 0);

        // The bytes the sink accumulated must be one valid
        // `Input` frame on the wire. Decode directly — StreamReader
        // rejects `Input` (slice 10c-e-2 review fix; reader is
        // server-direction only).
        let frames = decode_all_frames(&sink.accepted);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, StreamKind::Input);
        let b64 = frames[0].payload["bytes"].as_str().unwrap();
        let decoded = BASE64.decode(b64).unwrap();
        assert_eq!(decoded, b"payload");
    }

    #[test]
    fn writer_does_not_emit_fresh_header_on_top_of_partial_frame() {
        // Reviewer's exact concern: write_frame must not emit a new
        // length prefix while a prior frame is still partially on
        // the wire. We rehearse by queueing TWO frames and draining
        // 1 byte at a time, then confirm the sink's bytes parse as
        // two clean frames in order.
        let mut sink = ScriptedWriter::new();
        for _ in 0..256 {
            sink.push_quota(1);
        }
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        w.write(b"first").unwrap();
        w.write(b"second").unwrap();
        loop {
            match w.flush_pending() {
                Ok(true) => break,
                Ok(false) => continue,
                Err(e) => panic!("error: {}", e),
            }
        }
        // Decode the wire frames directly. Two `Input` frames,
        // in order — the contract is "no fresh header on top of
        // a partial frame", which we verify by parsing two clean
        // frames from the accumulated bytes.
        let frames = decode_all_frames(&sink.accepted);
        assert_eq!(frames.len(), 2);
        for f in &frames {
            assert_eq!(f.kind, StreamKind::Input);
        }
        let first = BASE64.decode(frames[0].payload["bytes"].as_str().unwrap()).unwrap();
        let second = BASE64.decode(frames[1].payload["bytes"].as_str().unwrap()).unwrap();
        assert_eq!(first, b"first");
        assert_eq!(second, b"second");
    }

    #[test]
    fn writer_write_returns_buflen_even_when_inner_blocks_immediately() {
        // Inner accepts nothing; writer queues and reports it took
        // all the caller's bytes (they're in the outbound queue).
        let mut sink = ScriptedWriter::new();
        sink.push_block();
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        let n = w.write(b"hello").unwrap();
        assert_eq!(n, 5, "writer must report it accepted all caller bytes");
        assert!(
            w.pending_bytes() > 0,
            "frame must be queued waiting for drain"
        );
    }

    #[test]
    fn writer_returns_would_block_when_queue_has_pending_from_prior_partial() {
        // Slice-10c-e-2 review-5 fix #1 contract: a second
        // write() with the queue still holding pending bytes
        // must return WouldBlock — without this, alacritty's
        // EventLoop thought writes succeeded and stopped
        // polling for writable, leaving keystrokes silently
        // queued indefinitely.
        let mut sink = ScriptedWriter::new();
        sink.push_block(); // post-queue drain attempt of write 1
        sink.push_block(); // pre-queue drain attempt of write 2
        let mut w = StreamWriter::new(&mut sink, "stream-1");

        // First write: queue empty initially, accepts buf, but
        // post-queue drain blocks → bytes stay queued.
        let n1 = w.write(b"first").expect("first write ok");
        assert_eq!(n1, 5);
        let pending_after_first = w.pending_bytes();
        assert!(pending_after_first > 0, "first write must queue bytes");

        // Second write: queue still has pending, sink still
        // blocks → must return WouldBlock, must NOT consume the
        // caller's buf into the queue.
        let err = w
            .write(b"second")
            .expect_err("second write with pending queue must return WouldBlock");
        assert_eq!(err.kind(), ErrorKind::WouldBlock);
        // Verify no bytes added to queue — the queue is the same
        // size as after write 1.
        assert_eq!(
            w.pending_bytes(),
            pending_after_first,
            "WouldBlock write must NOT add bytes to outbound queue",
        );
    }

    #[test]
    fn writer_drains_when_socket_becomes_writable_after_backpressure() {
        // Companion to the above: after a backpressured write
        // returns WouldBlock, calling flush_pending against a
        // now-writable sink drains the queue. Mimics the
        // EventLoop's writable-readiness branch.
        let mut sink = ScriptedWriter::new();
        sink.push_block(); // initial drain attempt fails
        // Now plenty of quota for the next drain.
        for _ in 0..256 {
            sink.push_quota(1);
        }
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        let _ = w.write(b"hello").expect("first write ok");
        assert!(w.pending_bytes() > 0);

        // Sink is "now writable" (quotas available). flush_pending
        // drains.
        loop {
            match w.flush_pending() {
                Ok(true) => break,
                Ok(false) => continue,
                Err(e) => panic!("unexpected drain error: {}", e),
            }
        }
        assert_eq!(w.pending_bytes(), 0, "drain must clear the queue");

        // After drain, a new write() succeeds normally.
        let n = w.write(b"after").expect("post-drain write ok");
        assert_eq!(n, 5);
    }

    #[test]
    fn writer_flush_surfaces_would_block_when_queue_not_empty() {
        let mut sink = ScriptedWriter::new();
        sink.push_block(); // for the initial drain inside write()
        sink.push_block(); // for the explicit flush() below
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        w.write(b"x").unwrap();
        let err = w
            .flush()
            .expect_err("flush with pending bytes returns WouldBlock");
        assert_eq!(err.kind(), ErrorKind::WouldBlock);
    }

    #[test]
    fn writer_zero_byte_accept_is_write_zero_error() {
        // A misbehaving sink (or peer-close-on-Unix) returns Ok(0).
        // The writer surfaces WriteZero so callers know to give up.
        let mut sink = ScriptedWriter::new();
        sink.push_quota(0); // returns Ok(0)
        let mut w = StreamWriter::new(&mut sink, "stream-1");
        let err = w.write(b"x").expect_err("write_zero must surface");
        assert_eq!(err.kind(), ErrorKind::WriteZero);
    }

    // ===== Slice 10c-e-3b-fix5: input chunking =====

    #[test]
    fn client_and_daemon_input_caps_agree() {
        // Constants-drift guard. The daemon's
        // `MAX_SEND_INPUT_BYTES` (in `daemon/src/control/methods.rs`)
        // is the per-Input-frame cap the daemon enforces; the
        // client's `MAX_INPUT_FRAME_BYTES` (above) is the per-
        // chunk size the writer respects when slicing large
        // pastes. If either changes without the other matching,
        // the client either oversends (silent daemon drop) or
        // wastes frames (sub-optimal but harmless). This test
        // pins them at build time.
        assert_eq!(
            MAX_INPUT_FRAME_BYTES,
            cm_daemon::control::methods::MAX_SEND_INPUT_BYTES,
            "client-side chunking cap must match daemon's per-frame cap; \
             one moved without the other — fix in lockstep",
        );
    }

    #[test]
    fn writer_chunks_large_input_into_multiple_frames() {
        // Reviewer's named acceptance for fix5: a 200 KiB write
        // produces ceil(200K / 64K) = 4 frames, each ≤ 64 KiB
        // (decoded payload), and the concatenated payloads
        // reconstruct the input byte-for-byte. No silent
        // truncation, no oversized frame.
        let mut sink = Vec::new();
        {
            let mut w = StreamWriter::new(&mut sink, "stream-1");
            let payload = vec![b'A'; 200 * 1024];
            let n = w.write(&payload).expect("write 200 KiB ok");
            assert_eq!(n, 200 * 1024, "writer must report full buf consumed");
        }
        let frames = decode_all_frames(&sink);
        // 200 KiB / 64 KiB = 3.125 → 4 chunks.
        assert_eq!(
            frames.len(),
            4,
            "200 KiB must split into ceil(200/64) = 4 frames; got {}",
            frames.len()
        );
        let mut reconstructed = Vec::with_capacity(200 * 1024);
        for f in &frames {
            assert_eq!(f.kind, StreamKind::Input);
            let b64 = f.payload["bytes"].as_str().unwrap();
            let chunk = BASE64.decode(b64).unwrap();
            assert!(
                chunk.len() <= MAX_INPUT_FRAME_BYTES,
                "chunk size {} exceeds cap {}",
                chunk.len(),
                MAX_INPUT_FRAME_BYTES
            );
            reconstructed.extend_from_slice(&chunk);
        }
        assert_eq!(reconstructed.len(), 200 * 1024);
        assert!(
            reconstructed.iter().all(|&b| b == b'A'),
            "reconstructed payload must be byte-identical to input"
        );
    }

    #[test]
    fn writer_chunks_at_exact_cap_boundary() {
        // Boundary: exactly MAX bytes → one frame, full size.
        let mut sink = Vec::new();
        {
            let mut w = StreamWriter::new(&mut sink, "stream-1");
            let payload = vec![b'B'; MAX_INPUT_FRAME_BYTES];
            let n = w.write(&payload).expect("write at exact cap ok");
            assert_eq!(n, MAX_INPUT_FRAME_BYTES);
        }
        let frames = decode_all_frames(&sink);
        assert_eq!(
            frames.len(),
            1,
            "exactly-cap payload must be one frame, not two empty-tail frames",
        );
        let decoded = BASE64
            .decode(frames[0].payload["bytes"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded.len(), MAX_INPUT_FRAME_BYTES);
    }

    #[test]
    fn writer_chunks_one_over_boundary() {
        // Boundary: cap + 1 byte → two frames, the second carrying
        // a single byte. Verifies the chunk loop's termination is
        // inclusive of the trailing remainder.
        let mut sink = Vec::new();
        {
            let mut w = StreamWriter::new(&mut sink, "stream-1");
            let payload = vec![b'C'; MAX_INPUT_FRAME_BYTES + 1];
            let n = w.write(&payload).expect("write at cap+1 ok");
            assert_eq!(n, MAX_INPUT_FRAME_BYTES + 1);
        }
        let frames = decode_all_frames(&sink);
        assert_eq!(frames.len(), 2);
        let first = BASE64
            .decode(frames[0].payload["bytes"].as_str().unwrap())
            .unwrap();
        let second = BASE64
            .decode(frames[1].payload["bytes"].as_str().unwrap())
            .unwrap();
        assert_eq!(first.len(), MAX_INPUT_FRAME_BYTES);
        assert_eq!(second.len(), 1);
    }

    #[test]
    fn writer_small_input_remains_single_frame() {
        // Regression guard: the chunking path doesn't break the
        // common-case "one keystroke → one frame" shape.
        let mut sink = Vec::new();
        {
            let mut w = StreamWriter::new(&mut sink, "stream-1");
            w.write(b"hello").expect("small write ok");
        }
        let frames = decode_all_frames(&sink);
        assert_eq!(frames.len(), 1);
        let decoded = BASE64
            .decode(frames[0].payload["bytes"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"hello");
    }
}
