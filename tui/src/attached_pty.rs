//! `AttachedPty` — a Pty-equivalent backed by the daemon's attach
//! stream instead of a kernel PTY. Slice 10c-e-1 of
//! `doc/persistent-host-daemon.md`.
//!
//! ## Where this fits
//!
//! Alacritty's `EventLoop` consumes any type implementing
//! `alacritty_terminal::tty::EventedReadWrite + EventedPty`. The
//! built-in `unix::Pty` does this against a real PTY master fd plus a
//! SIGCHLD self-pipe. After Phase 1 of the design doc, the OS PTY
//! lives daemon-side and the TUI talks to it through the attach
//! stream — `slice-10c-c`'s `attach.open` lands a per-session
//! dedicated Unix socket whose subsequent frames are PTY bytes
//! wrapped in [`cm_daemon::control::protocol::StreamFrame`]s.
//!
//! `AttachedPty` is the trait impl that plugs that stream into the
//! existing `EventLoop`:
//!   - [`term_shim::StreamReader`](crate::term_shim::StreamReader)
//!     decodes server→client data frames into raw PTY bytes; an
//!     `io::Read` impl is the trait's `Reader` associated type.
//!   - [`term_shim::StreamWriter`](crate::term_shim::StreamWriter)
//!     encodes client→server keystrokes into data frames; `io::Write`
//!     is the `Writer` associated type.
//!   - A small self-pipe lets us surface the
//!     [`term_shim::ChildEvent`](crate::term_shim::ChildEvent) decoded
//!     from an `End` frame as an `EventedPty` event the EventLoop's
//!     match arm dispatches on the same `PTY_CHILD_EVENT_TOKEN`
//!     alacritty's Unix Pty uses for SIGCHLD.
//!
//! ## What this slice ships
//!
//! Just the trait impls and a constructor that takes a pre-dialed
//! `UnixStream`. The full RPC dance (`start_session` →
//! `session.attach` → dial → `attach.open`) lives in slice 10c-e-2's
//! `ClientSession::new`; the opt-in branch in `A-n` / `A-s` lives in
//! 10c-e-3. Until those land this type is reachable only from tests.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{ChildEvent, EventedPty, EventedReadWrite};
use polling::{Event, PollMode, Poller};

use crate::term_shim::{ChildEvent as ShimChildEvent, StreamReader, StreamWriter};

/// Mirrors `alacritty_terminal::tty::unix::PTY_READ_WRITE_TOKEN`,
/// which is `pub(crate)` and not importable. Hardcoding the same
/// value is what makes alacritty's `EventLoop` match arm route
/// reader/writer events to us correctly.
///
/// If alacritty ever changes this constant, our trait impl breaks
/// at runtime (the EventLoop would silently never call `pty_read` /
/// `pty_write`). The `polling = "3.8"` pin in `tui/Cargo.toml`
/// matches alacritty 0.25 exactly; a major-version bump there would
/// be the time to revisit.
const PTY_READ_WRITE_TOKEN: usize = 0;

/// Mirrors `alacritty_terminal::tty::unix::PTY_CHILD_EVENT_TOKEN`.
/// Same rationale as [`PTY_READ_WRITE_TOKEN`] — `pub(crate)` in
/// alacritty, hardcoded here to share dispatch with alacritty's
/// EventLoop. Used for the self-pipe that signals "End frame
/// observed" so the EventLoop will call `next_child_event` and
/// surface the exit.
const PTY_CHILD_EVENT_TOKEN: usize = 1;

/// PTY-equivalent backed by a daemon attach stream.
///
/// Owns:
///   - The dedicated `UnixStream` returned by `attach.open` (slice
///     10c-c); polling registers this fd under
///     [`PTY_READ_WRITE_TOKEN`].
///   - A `StreamReader<UnixStream>` over a `try_clone()`d socket
///     (server→client PTY bytes).
///   - A `StreamWriter<UnixStream>` over a second `try_clone()`d
///     socket (client→server keystrokes / resizes).
///   - A self-pipe (`OwnedFd` read+write halves); polling registers
///     the read half under [`PTY_CHILD_EVENT_TOKEN`]. The reader-
///     side wrapper writes a byte to the pipe the first time it
///     observes a `term_shim::ChildEvent` so the EventLoop will
///     call `next_child_event` and drain the cached exit.
pub struct AttachedPty {
    /// Data socket. The original; the reader/writer wrappers each
    /// hold a `try_clone()`d copy. Kept here so polling
    /// register/reregister/deregister have a single source to
    /// reference for the read/write token.
    socket: UnixStream,
    /// Server→client side: decodes data frames into PTY bytes,
    /// signals child-exit through `child_pipe_write`.
    reader: ReaderHalf,
    /// Client→server side: encodes keystrokes into data frames.
    writer: StreamWriter<UnixStream>,
    /// Read end of the child-event self-pipe. Registered under
    /// `PTY_CHILD_EVENT_TOKEN` in `register`. `next_child_event`
    /// drains a byte from this and returns the cached
    /// `ChildEvent`.
    child_pipe_read: OwnedFd,
    /// Cached exit observed from `StreamReader::take_child_event`.
    /// Drained by [`Self::next_child_event`].
    pending_exit: Option<ShimChildEvent>,
    /// Side channel for `memory_cap_kill` (slice-10c-e-2 review-5
    /// fix #2b). Alacritty's `ChildEvent::Exited(Option<i32>)`
    /// can't carry the flag, but the design doc names the attach
    /// stream as the path that surfaces it for attached sessions
    /// (manifest.watch is the detached path). The reader half
    /// writes this flag BEFORE signalling the child-event pipe,
    /// so by the time alacritty's EventLoop processes the
    /// child-event token and the TUI's exit-handler reads back,
    /// the flag is already populated.
    ///
    /// `Arc<AtomicBool>` so reader half and `next_child_event`
    /// share access without coupling on a non-atomic field.
    memory_cap_kill: Arc<AtomicBool>,
}

/// Wrapper around [`StreamReader<UnixStream>`] that signals
/// child-exit through a self-pipe so alacritty's EventLoop will
/// dispatch [`PTY_CHILD_EVENT_TOKEN`]. The signal fires exactly
/// once per stream lifetime; subsequent observations of the same
/// `child_event` are coalesced.
///
/// `pending_exit_stash` is the side channel that lets the
/// `Read::read` impl hand the moved-out
/// [`term_shim::ChildEvent`](crate::term_shim::ChildEvent) up to
/// [`AttachedPty::next_child_event`]: putting the field on
/// `ReaderHalf` avoids needing an `Arc<Mutex<…>>` for shared mutable
/// access from the EventLoop's reader path.
pub struct ReaderHalf {
    inner: StreamReader<UnixStream>,
    /// Write end of the child-event self-pipe.
    pipe_write: OwnedFd,
    /// True once we've signalled the pipe for the current stream's
    /// exit. Prevents repeated writes if the EventLoop calls read
    /// again after EOF.
    signalled: bool,
    /// Stash slot for the moved-out `ChildEvent::Exited{…}` the
    /// reader observes via `take_child_event`. Drained by
    /// `AttachedPty::next_child_event` through
    /// [`Self::take_pending_exit`].
    pending_exit_stash: Option<ShimChildEvent>,
    /// Shared with [`AttachedPty::memory_cap_kill`]. Set BEFORE
    /// signalling the self-pipe so a TUI exit-handler reading
    /// the flag on `ChildEvent::Exited` always sees a populated
    /// value (no read-vs-write ordering hazard).
    memory_cap_kill: Arc<AtomicBool>,
}

impl ReaderHalf {
    /// Drain the pending-exit stash. Called by
    /// [`AttachedPty::next_child_event`] after the self-pipe
    /// readability has been consumed.
    fn take_pending_exit(&mut self) -> Option<ShimChildEvent> {
        self.pending_exit_stash.take()
    }
}

impl Read for ReaderHalf {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.inner.read(buf);
        // After every read, drain any cached child event. The
        // signal is one-shot per stream: once we've written to
        // the pipe, repeat reads don't write again. The EventLoop
        // sees the readability, calls `next_child_event`, which
        // drains both the byte AND the stashed event.
        if !self.signalled {
            if let Some(event) = self.inner.take_child_event() {
                self.signalled = true;
                // Set the `memory_cap_kill` side-channel BEFORE
                // signalling the pipe (slice-10c-e-2 review-5
                // fix #2b). Ordering: TUI's exit-handler reads
                // the flag when it sees `ChildEvent::Exited` —
                // which is delivered via the pipe → token →
                // next_child_event path. Setting the flag first
                // and using SeqCst means by the time the reader
                // observes the token, the flag is already
                // populated.
                let ShimChildEvent::Exited { memory_cap_kill, .. } = &event;
                if *memory_cap_kill {
                    self.memory_cap_kill.store(true, Ordering::SeqCst);
                }
                // SAFETY: pipe_write is a valid pipe fd owned by
                // ReaderHalf. write(2) on a non-full pipe with a
                // 1-byte payload is non-blocking when the fd is
                // O_NONBLOCK (which we set in
                // `AttachedPty::from_socket`). A failure here
                // means the EventLoop won't see the
                // child-event token fire — recoverable: the
                // socket EOF (Ok(0) from this very read on
                // the next call) will eventually cause alacritty
                // to wind down. Best-effort.
                let signal_byte = [0u8];
                let _ = unsafe {
                    libc::write(
                        self.pipe_write.as_raw_fd(),
                        signal_byte.as_ptr() as *const _,
                        1,
                    )
                };
                self.pending_exit_stash = Some(event);
            }
        }
        result
    }
}

impl AttachedPty {
    /// Construct an `AttachedPty` over a freshly-dialed `UnixStream`
    /// — typically the dedicated attach connection returned by
    /// slice 10c-c's `attach.open` flow. `stream_id` is the
    /// `request_id` field of the `attach.open` request (every
    /// outgoing `StreamFrame` echoes it so the daemon can demux
    /// multiplexed control streams later).
    ///
    /// Errors propagate from `try_clone` (rare — typical cause is
    /// fd exhaustion) and from `pipe2` (Linux self-pipe creation).
    /// All temporaries are dropped on the error path so no fd
    /// leaks across a partial construction.
    pub fn from_socket(
        socket: UnixStream,
        stream_id: impl Into<String>,
    ) -> io::Result<Self> {
        let reader_socket = socket.try_clone()?;
        let writer_socket = socket.try_clone()?;
        // Non-blocking so polling-driven reads/writes don't park
        // the EventLoop thread. Alacritty's existing PTY does the
        // same.
        socket.set_nonblocking(true)?;
        reader_socket.set_nonblocking(true)?;
        writer_socket.set_nonblocking(true)?;

        // Self-pipe for child-event signalling. O_CLOEXEC so a
        // future fork from the TUI process doesn't leak the fds
        // to the child; O_NONBLOCK so the reader-half's
        // best-effort write never blocks the EventLoop.
        let (child_pipe_read, child_pipe_write) = open_child_event_pipe()?;

        let memory_cap_kill = Arc::new(AtomicBool::new(false));
        let reader = ReaderHalf {
            inner: StreamReader::new(reader_socket),
            pipe_write: child_pipe_write,
            signalled: false,
            pending_exit_stash: None,
            memory_cap_kill: memory_cap_kill.clone(),
        };
        let writer = StreamWriter::new(writer_socket, stream_id);

        Ok(Self {
            socket,
            reader,
            writer,
            child_pipe_read,
            pending_exit: None,
            memory_cap_kill,
        })
    }

    /// Take the latched `memory_cap_kill` flag. Returns `true`
    /// once if the attach stream's `End` frame carried
    /// `memory_cap_kill: true`; subsequent calls return `false`
    /// (the flag is consumed). The TUI's exit-handler reads this
    /// when it observes `ChildEvent::Exited` from
    /// `next_child_event` and decides whether to render the
    /// "killed by memory cap" toast — slice-10c-e-2 review-5 fix
    /// #2b. See [`AttachedPty::memory_cap_kill`] for the
    /// ordering contract with the child-event self-pipe.
    pub fn took_memory_cap_kill(&self) -> bool {
        // swap-with-false: atomic read-and-clear.
        self.memory_cap_kill.swap(false, Ordering::SeqCst)
    }

    /// Hand out a clone of the latched `memory_cap_kill` Arc so
    /// callers (specifically `Session::new_attached`) can
    /// observe the flag AFTER alacritty's EventLoop has taken
    /// ownership of the `AttachedPty`. Slice 10c-e-3b-fix4b:
    /// without this, the flag is latched but no consumer reads
    /// it — the cap-kill toast was silently broken for daemon-
    /// attached sessions.
    ///
    /// The returned Arc shares the same `AtomicBool` as
    /// [`AttachedPty::took_memory_cap_kill`]; reading via either
    /// path consumes the flag (the read-and-clear semantics
    /// hold). In practice the exit handler reads via the cloned
    /// Arc since `AttachedPty` itself is unreachable post-spawn
    /// (owned by the EventLoop thread).
    pub fn memory_cap_kill_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.memory_cap_kill)
    }

    /// Opportunistically drain pending writer bytes. Slice-10c-e-2
    /// review-7 (Shape B).
    ///
    /// The bug being fixed: `StreamWriter::write` may report
    /// `Ok(buf.len())` after a partial drain — its outbound
    /// queue keeps the remainder, but alacritty's EventLoop
    /// sees "all bytes written" and clears its own
    /// `state.write_buffer`. The writable-interest override on
    /// `register`/`reregister` keeps kernel EPOLLOUT firing, but
    /// alacritty's writable handler short-circuits when its own
    /// buffer is empty — so those queued bytes wait until the
    /// next user keystroke fortuitously triggers another
    /// `write()`.
    ///
    /// Architectural truth: alacritty's `EventLoop` has no hook
    /// that lets us say "I have pending bytes, please call my
    /// flush method." Whatever fix we land has to either (a) drive
    /// the drain ourselves on writable readiness via a separate
    /// writer thread, or (b) piggyback on the natural TUI rhythm
    /// of inbound EventLoop calls.
    ///
    /// We picked (b) — Shape B in the review's terminology —
    /// because the steady traffic during any active session is
    /// daemon→client bytes, so any inbound EventLoop call
    /// (`reader()`, `register`/`reregister`, `next_child_event`)
    /// implies a drain cycle. The cost is one no-op call when
    /// the queue is empty (a `VecDeque::is_empty()` check).
    ///
    /// Quiescent-session caveat: if the daemon is silent AND the
    /// user is not typing AND polling reregister doesn't fire,
    /// this hook doesn't drain. But a quiescent session has no
    /// input pressure either — the queue only grows when the user
    /// types, and typing guarantees another inbound event will
    /// follow soon. We document the pathological case here but
    /// don't engineer around it. If the smoke test surfaces a
    /// case Shape B doesn't cover, escalate to Shape A (writer
    /// thread that owns the socket write-half) in a follow-up.
    ///
    /// ## Do NOT "fix" this preemptively
    ///
    /// **Standing rejection from review round 26.** This Shape B
    /// vs Shape A tradeoff was debated explicitly at the time
    /// the hook landed; the decision was to ship Shape B with the
    /// quiescent-session caveat above and escalate to Shape A
    /// (per-attach writer thread) only on smoke evidence of stuck
    /// input in practice. The realistic paths keep the reader hot:
    ///
    /// - Claude/Codex sessions stream tokens continuously →
    ///   reader hot → drain fires.
    /// - Bash sessions echo input characters → reader hot →
    ///   drain fires.
    /// - Raw-mode + no-echo (password prompts, paste into
    ///   `vim` insert mode) is the edge case. Hasn't surfaced
    ///   as a real smoke failure.
    ///
    /// If a future review surfaces "the queue tail might not
    /// drain in quiescent sessions" again: reject in place,
    /// point at this comment. The decision is made on smoke
    /// evidence (visible stuck-paste with no follow-up event),
    /// not static analysis. Don't re-litigate it round by round.
    fn drain_pending(&mut self) {
        // Errors are swallowed: a failed drain leaves bytes
        // queued, which the next inbound hook will retry. A
        // hard socket error will surface through the reader's
        // next `read()` and tear down the session cleanly.
        let _ = self.writer.flush_pending();
    }
}

impl EventedReadWrite for AttachedPty {
    type Reader = ReaderHalf;
    type Writer = StreamWriter<UnixStream>;

    /// Register the data socket under `PTY_READ_WRITE_TOKEN` and
    /// the child-event self-pipe read end under
    /// `PTY_CHILD_EVENT_TOKEN`. Mirrors alacritty's Unix Pty
    /// shape exactly so the EventLoop's match-on-event.key
    /// dispatch routes both kinds of readiness correctly.
    ///
    /// # Safety
    ///
    /// The contract is alacritty's: the underlying sources
    /// (`self.socket`, `self.child_pipe_read`) must outlive their
    /// registration. `AttachedPty` owns both for the duration of
    /// its lifetime; `deregister` clears them. So as long as the
    /// caller pairs `register` with `deregister` before drop, the
    /// invariant holds.
    unsafe fn register(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        // Slice-10c-e-2 review-7 (Shape B): opportunistic drain
        // before any other work. See `drain_pending` for the
        // full why.
        self.drain_pending();
        // Slice-10c-e-2 review-5 fix #1: keep writable interest
        // set while OUR outbound queue has pending bytes, even
        // when alacritty's `state.write` thinks the write
        // completed. Without this the kernel stops delivering
        // writable events once alacritty's buffer empties, and
        // our partially-queued bytes sit until the next user
        // keystroke triggers another write() call.
        if self.writer.pending_bytes() > 0 {
            interest.writable = true;
        }
        unsafe {
            poll.add_with_mode(&self.socket, interest, mode)?;
        }
        unsafe {
            poll.add_with_mode(
                &self.child_pipe_read,
                Event::readable(PTY_CHILD_EVENT_TOKEN),
                PollMode::Level,
            )
        }
    }

    fn reregister(
        &mut self,
        poll: &Arc<Poller>,
        mut interest: Event,
        mode: PollMode,
    ) -> io::Result<()> {
        interest.key = PTY_READ_WRITE_TOKEN;
        // Slice-10c-e-2 review-7 (Shape B): opportunistic drain.
        self.drain_pending();
        // Same writable-interest override as in `register` —
        // override alacritty's "I think writes are done" view
        // when our queue still has pending bytes.
        if self.writer.pending_bytes() > 0 {
            interest.writable = true;
        }
        poll.modify_with_mode(&self.socket, interest, mode)?;
        poll.modify_with_mode(
            &self.child_pipe_read,
            Event::readable(PTY_CHILD_EVENT_TOKEN),
            PollMode::Level,
        )
    }

    fn deregister(&mut self, poll: &Arc<Poller>) -> io::Result<()> {
        poll.delete(&self.socket)?;
        poll.delete(&self.child_pipe_read)
    }

    fn reader(&mut self) -> &mut Self::Reader {
        // Slice-10c-e-2 review-7 (Shape B): drain pending writer
        // bytes opportunistically. This is the hot inbound hook —
        // alacritty's EventLoop calls `pty.reader()` on every
        // EPOLLIN dispatch to fetch the reader half, so as long
        // as the daemon is sending bytes (the steady traffic for
        // any active session), our writer queue gets drained in
        // lockstep with reads.
        self.drain_pending();
        &mut self.reader
    }

    fn writer(&mut self) -> &mut Self::Writer {
        &mut self.writer
    }
}

/// `OnResize` plumbs alacritty's resize hook through the
/// `StreamWriter`'s `send_resize` — the daemon sees a
/// `{"resize":{cols, rows}}` data frame and applies it to the
/// kernel PTY's `TIOCSWINSZ` on its side.
///
/// Best-effort: if the writer's queue can't drain right now
/// (network slow, daemon paused), the resize stays queued for the
/// next `EPOLLOUT`. A failure here logs to stderr but isn't
/// fatal — the terminal will just be off by a few cells until
/// the next legitimate resize event.
impl OnResize for AttachedPty {
    fn on_resize(&mut self, window_size: WindowSize) {
        if let Err(e) = self
            .writer
            .send_resize(window_size.num_cols, window_size.num_lines)
        {
            eprintln!(
                "cm-tui: AttachedPty resize {}x{} failed: {} (daemon will see the next resize event)",
                window_size.num_cols, window_size.num_lines, e,
            );
        }
    }
}

impl EventedPty for AttachedPty {
    /// Drain a pending child-event observed by the reader half.
    /// Two-step:
    ///   1. Drain the self-pipe (one byte) so its readability
    ///      transitions back to "no event pending" — otherwise
    ///      level-triggered polling re-fires forever.
    ///   2. Return the cached `ShimChildEvent::Exited{code,
    ///      memory_cap_kill}` mapped to alacritty's
    ///      `ChildEvent::Exited(code)`. The `memory_cap_kill`
    ///      flag is dropped here — it lands in the manifest update
    ///      separately (slice 10e's manifest-watch path), since
    ///      alacritty's ChildEvent doesn't carry it.
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        // Slice-10c-e-2 review-7 (Shape B): last-chance drain on
        // the child-exit path. If the user typed something right
        // before the child exited and the bytes are still queued,
        // surface them before the session winds down.
        self.drain_pending();
        // Drain the pipe regardless of whether we have an exit to
        // surface — being conservative about leftover bytes from
        // any retry path.
        let mut buf = [0u8; 1];
        let _ = unsafe {
            libc::read(
                self.child_pipe_read.as_raw_fd(),
                buf.as_mut_ptr() as *mut _,
                1,
            )
        };

        // Coalesce: pull any newly-stashed event from the reader
        // half into our slot, then drain.
        if self.pending_exit.is_none() {
            self.pending_exit = self.reader.take_pending_exit();
        }
        match self.pending_exit.take() {
            Some(ShimChildEvent::Exited { code, .. }) => Some(ChildEvent::Exited(code)),
            None => None,
        }
    }
}

/// Linux-specific helper to open a O_CLOEXEC + O_NONBLOCK self-pipe
/// via `pipe2(2)`. Returns `(read_end, write_end)` as `OwnedFd`s so
/// the caller doesn't have to deal with raw fd leaks. The TUI is
/// Linux-only per the project's `doc/persistent-host-daemon.md`.
fn open_child_event_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [libc::c_int; 2] = [-1, -1];
    // SAFETY: pipe2 fills the array on success; we check the
    // return value before treating the values as fds.
    let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pipe2 succeeded; both fds are now valid and owned
    // by us.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((read, write))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cm_daemon::control::protocol::{StreamFrame, StreamKind};
    use std::io::Write as _;

    /// Encode one length-prefixed `StreamFrame` into a Vec for
    /// wire-shape tests.
    fn encode_frame(frame: &StreamFrame) -> Vec<u8> {
        let body = serde_json::to_vec(frame).expect("encode");
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend(body);
        out
    }

    /// Pair of connected `UnixStream`s. The "client" side is
    /// what `AttachedPty` wraps; the "server" side is the test's
    /// stand-in for the daemon writing PTY bytes back.
    fn socket_pair() -> (UnixStream, UnixStream) {
        UnixStream::pair().expect("UnixStream::pair")
    }

    #[test]
    fn from_socket_clones_and_constructs_without_error() {
        // Smoke: the constructor doesn't panic, doesn't leak the
        // socket, and the wrapper holds reader+writer with the
        // right stream id stamped on outgoing frames.
        let (client, _server) = socket_pair();
        let pty = AttachedPty::from_socket(client, "attach-1")
            .expect("from_socket ok");
        // Sanity check that the structure is observable.
        let _ = pty.writer;
    }

    #[test]
    fn reader_decodes_data_frame_to_pty_bytes() {
        // Server writes a data frame; reader yields the decoded
        // bytes through io::Read.
        let (client, mut server) = socket_pair();
        let mut pty = AttachedPty::from_socket(client, "attach-data")
            .expect("from_socket");

        // Write a data frame carrying "hi" (base64 "aGk=").
        let frame = StreamFrame {
            id: "attach-data".into(),
            kind: StreamKind::Data,
            payload: serde_json::json!({ "bytes": "aGk=" }),
        };
        server.write_all(&encode_frame(&frame)).expect("write frame");

        // Bounded poll for the bytes; non-blocking + EventLoop-
        // style scheduling means a tight retry loop is the right
        // shape for unit tests.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = [0u8; 16];
        loop {
            match pty.reader.read(&mut buf) {
                Ok(0) => {
                    if std::time::Instant::now() >= deadline {
                        panic!("reader yielded 0 bytes / EOF before decoded frame");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Ok(n) => {
                    assert_eq!(&buf[..n], b"hi");
                    return;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        panic!("reader stuck on WouldBlock for 2s");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("reader error: {}", e),
            }
        }
    }

    #[test]
    fn writer_encodes_pty_bytes_to_data_frame() {
        // Reader-side (our wrapper) writes bytes; server side
        // observes the corresponding length-prefixed StreamFrame.
        let (client, mut server) = socket_pair();
        let mut pty = AttachedPty::from_socket(client, "attach-out")
            .expect("from_socket");
        pty.writer.write_all(b"keystroke").expect("write");

        // Drain server side and decode the frame.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        server.set_nonblocking(true).expect("nonblock");
        let mut accumulated = Vec::new();
        loop {
            let mut buf = [0u8; 256];
            match server.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    accumulated.extend_from_slice(&buf[..n]);
                    if accumulated.len() >= 4 {
                        let len = u32::from_be_bytes([
                            accumulated[0], accumulated[1], accumulated[2], accumulated[3],
                        ]) as usize;
                        if accumulated.len() >= 4 + len {
                            let body = &accumulated[4..4 + len];
                            let frame: StreamFrame =
                                serde_json::from_slice(body).expect("decode");
                            // Client→server bytes carry the `Input`
                            // kind (slice-10c-e-2 review fix).
                            assert_eq!(frame.kind, StreamKind::Input);
                            assert_eq!(frame.id, "attach-out");
                            // bytes field is base64 of b"keystroke"
                            let b64 = frame.payload["bytes"].as_str().unwrap();
                            use base64::Engine;
                            let decoded = base64::engine::general_purpose::STANDARD
                                .decode(b64)
                                .expect("base64");
                            assert_eq!(decoded, b"keystroke");
                            return;
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        panic!("server didn't see the frame within 2s");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("server read error: {}", e),
            }
        }
        panic!("server stream closed before frame arrived");
    }

    #[test]
    fn end_frame_surfaces_via_next_child_event_after_pipe_signal() {
        // Server writes an End frame. Reader detects it and
        // signals the self-pipe; next_child_event drains and
        // returns Some(Exited(code)).
        let (client, mut server) = socket_pair();
        let mut pty = AttachedPty::from_socket(client, "attach-end")
            .expect("from_socket");

        let frame = StreamFrame {
            id: "attach-end".into(),
            kind: StreamKind::End,
            payload: serde_json::json!({ "exit_code": 137, "memory_cap_kill": true }),
        };
        server.write_all(&encode_frame(&frame)).expect("write end");

        // Drive the reader until EOF — that's what triggers the
        // child-event stash.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = [0u8; 64];
        loop {
            match pty.reader.read(&mut buf) {
                Ok(0) => break, // EOF after End frame
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        panic!("reader stuck on WouldBlock for 2s");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("reader error: {}", e),
            }
        }

        // Now next_child_event must surface Exited(Some(137)).
        // alacritty's `ChildEvent::Exited(Option<i32>)` carries
        // exit_code only; `memory_cap_kill` rides the side
        // channel — see the dedicated test below.
        match pty.next_child_event() {
            Some(ChildEvent::Exited(code)) => assert_eq!(code, Some(137)),
            other => panic!("expected Exited(Some(137)), got {:?}", other),
        }
        // Subsequent calls yield None.
        assert!(pty.next_child_event().is_none());
    }

    // --- slice-10c-e-3b-fix4b acceptance --------------------------------

    #[test]
    fn memory_cap_kill_handle_observes_same_flag_as_took_memory_cap_kill() {
        // Slice 10c-e-3b-fix4b: `Session::new_attached` clones the
        // memory_cap_kill Arc out of AttachedPty BEFORE handing
        // the pty to alacritty's EventLoop. The exit handler in
        // app.rs reads via the cloned Arc since the EventLoop
        // owns the AttachedPty post-spawn. Pin the structural
        // contract: the handle and the AttachedPty's internal
        // field share the SAME AtomicBool — a `true` written by
        // the reader half is visible via both consumers.
        let (client, mut server) = socket_pair();
        let pty = AttachedPty::from_socket(client, "attach-handle")
            .expect("from_socket");

        // Grab the handle BEFORE the End frame arrives.
        let handle = pty.memory_cap_kill_handle();
        use std::sync::atomic::Ordering;
        assert!(
            !handle.load(Ordering::SeqCst),
            "handle must start false (no End frame yet)",
        );

        // Now make the reader observe an End frame with the
        // flag set. We need a fresh `pty` mut binding to call
        // `.reader.read()`.
        let mut pty = pty;
        let frame = StreamFrame {
            id: "attach-handle".into(),
            kind: StreamKind::End,
            payload: serde_json::json!({
                "exit_code": serde_json::Value::Null,
                "memory_cap_kill": true,
            }),
        };
        server.write_all(&encode_frame(&frame)).expect("write end");

        // Drive the reader to consume the End frame and latch
        // the flag.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = [0u8; 64];
        loop {
            match pty.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        panic!("reader stuck on WouldBlock");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("reader error: {}", e),
            }
        }

        // The handle (shared Arc) now sees the flag.
        assert!(
            handle.load(Ordering::SeqCst),
            "handle MUST observe the latched flag the reader stored \
             — this is the property `Session.daemon_memory_cap_kill` \
             depends on for the exit-handler toast dispatch",
        );

        // Either consumer can read-and-clear; the other sees
        // the cleared state on next read. We test the
        // handle's swap (the exit handler's call shape).
        assert!(
            handle.swap(false, Ordering::SeqCst),
            "swap(false) returns the prior latched value (true)",
        );
        assert!(
            !pty.took_memory_cap_kill(),
            "AttachedPty's read-and-clear sees the now-cleared state",
        );
    }

    // --- slice-10c-e-2 review-5 fixes -----------------------------------

    #[test]
    fn end_frame_memory_cap_kill_propagates_to_side_channel() {
        // Named acceptance for fix #2b: server sends
        // `End { exit_code: null, memory_cap_kill: true }`;
        // client observes `ChildEvent::Exited(None)` AND
        // `pty.took_memory_cap_kill() == true`. The side channel
        // is read by the TUI's exit-handler when it surfaces the
        // "killed by memory cap" toast for attached sessions.
        let (client, mut server) = socket_pair();
        let mut pty = AttachedPty::from_socket(client, "attach-mck")
            .expect("from_socket");

        let frame = StreamFrame {
            id: "attach-mck".into(),
            kind: StreamKind::End,
            payload: serde_json::json!({
                "exit_code": serde_json::Value::Null,
                "memory_cap_kill": true,
            }),
        };
        server.write_all(&encode_frame(&frame)).expect("write end");

        // Drive reader to observe End.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut buf = [0u8; 64];
        loop {
            match pty.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        panic!("reader stuck on WouldBlock");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("reader error: {}", e),
            }
        }

        // The named pair of assertions:
        match pty.next_child_event() {
            Some(ChildEvent::Exited(code)) => assert_eq!(
                code, None,
                "End{{exit_code: null}} must surface Exited(None)"
            ),
            other => panic!("expected Exited(None), got {:?}", other),
        }
        assert!(
            pty.took_memory_cap_kill(),
            "memory_cap_kill=true on End frame must latch the side-channel flag",
        );
        // Side channel is one-shot: subsequent reads return false.
        assert!(
            !pty.took_memory_cap_kill(),
            "took_memory_cap_kill must clear on first read",
        );
    }

    #[test]
    fn clean_eof_without_end_frame_synthesizes_exited_none() {
        // Named acceptance for fix #2a: when the daemon vanishes
        // / restarts / closes the attach socket WITHOUT sending
        // an End frame, the client must still observe
        // ChildEvent::Exited(None) within bound — otherwise the
        // session pane stays alive indefinitely.
        let (client, server) = socket_pair();
        let mut pty = AttachedPty::from_socket(client, "attach-vanished")
            .expect("from_socket");

        // Drop server side WITHOUT writing any frames. The
        // client's reader sees clean EOF — must synthesize the
        // exit event.
        drop(server);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut buf = [0u8; 64];
        // Drive reader until it reports EOF (Ok(0)).
        loop {
            match pty.reader.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        panic!("reader didn't reach EOF within 3s");
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("reader error: {}", e),
            }
        }

        // The contract: synthesized Exited(None), memory_cap_kill
        // remains false (we can't attribute it without an
        // explicit End frame).
        match pty.next_child_event() {
            Some(ChildEvent::Exited(code)) => assert_eq!(code, None),
            other => panic!(
                "EOF without End must synthesize Exited(None), got {:?}",
                other
            ),
        }
        assert!(
            !pty.took_memory_cap_kill(),
            "EOF without End must not attribute memory_cap_kill"
        );
    }

    // --- slice-10c-e-2 review-7 (Shape B) -------------------------------

    #[test]
    fn inbound_reader_hook_fully_drains_writer_queue_after_partial_flush() {
        // Named acceptance: AttachedPty::reader() (the hot inbound
        // EventLoop hook) opportunistically drains the writer's
        // outbound queue. Pins the contract: after a partial drain
        // leaves bytes queued, an inbound call (no further write()
        // from alacritty) finishes the drain.
        //
        // The bug being pinned: StreamWriter::write can return
        // Ok(buf.len()) after the post-queue flush_pending only
        // partial-drained. Alacritty clears its state.write_buffer,
        // the writable-interest override keeps EPOLLOUT firing,
        // but alacritty's writable handler short-circuits because
        // its own buffer is empty. The hook in reader() is what
        // closes that gap (Shape B).
        //
        // Test shape (per the review brief):
        //   1. Write enough bytes (> SO_SNDBUF for the AF_UNIX
        //      socket) to force a partial drain and leave bytes
        //      queued locally.
        //   2. Drain server-side reads to free kernel buffer space.
        //   3. Call pty.reader() — the only inbound EventLoop hook,
        //      no further write() call.
        //   4. Assert the queue is fully drained (pending_bytes ==
        //      0).
        let (client, mut server) = socket_pair();
        let mut pty = AttachedPty::from_socket(client, "attach-bp")
            .expect("from_socket");

        // Fill the kernel send buffer. Linux default SO_SNDBUF for
        // AF_UNIX is ~208KB; 100 × 16KB raw → ~133KB×1.33 base64
        // wrapped ≈ ~1.7MB on the wire — far more than the kernel
        // will accept without blocking. Writes return Ok(16KB)
        // until the queue picks up remainder, then return
        // WouldBlock (the slice-10c-e-2 review-5 backpressure
        // semantics).
        let payload = vec![b'X'; 16 * 1024];
        for _ in 0..100 {
            let _ = pty.writer().write(&payload);
        }
        let pending_before = pty.writer().pending_bytes();
        assert!(
            pending_before > 0,
            "test setup: writer queue must have pending bytes after fill \
             (got pending_before={}); SO_SNDBUF may be larger than expected, \
             increase write count.",
            pending_before
        );

        // Drain server side to free kernel buffer. Read at least
        // `pending_before + 64KB` so the kernel has plenty of
        // space for the full drain. Use a bounded deadline so a
        // wedged test fails fast.
        server.set_nonblocking(true).expect("server nonblock");
        let target_drain = pending_before + 64 * 1024;
        let mut sink = [0u8; 64 * 1024];
        let mut total_read = 0usize;
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(2);
        while total_read < target_drain && std::time::Instant::now() < deadline {
            match server.read(&mut sink) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("server read: {}", e),
            }
        }
        assert!(
            total_read >= target_drain,
            "test setup: server must drain at least {} bytes to free kernel \
             buffer (got {}); maybe the kernel buffer is larger than expected.",
            target_drain, total_read,
        );

        // The crux of the test: an inbound EventLoop hook fires
        // (we use reader() since it's the hot path), and that
        // alone — with no further write() call from alacritty —
        // must drain the queue to zero. The kernel now has space
        // (we drained `target_drain` bytes), and the flush_pending
        // hook on reader() runs `inner.write(queue)` until the
        // queue is empty or the kernel blocks again.
        let _ = pty.reader();

        let pending_after = pty.writer().pending_bytes();
        assert_eq!(
            pending_after, 0,
            "AttachedPty::reader() must fully drain writer queue when \
             kernel buffer has space; pending_before={}, pending_after={} \
             (a non-zero remainder means the inbound hook is not closing \
             the Shape-B backpressure gap)",
            pending_before, pending_after,
        );
    }
}
