//! Daemon-side session primitives. Slice 7 of doc/persistent-host-daemon.md.
//!
//! ## Where this fits
//!
//! The design doc's "Session struct split" section breaks today's
//! single `tui/src/session.rs::Session` into two structs in two
//! processes:
//!
//!   - [`DaemonSession`] — owns the OS PTY child, fan-out broadcaster,
//!     memory-cap watcher, cgroup path, wakeup-burst tracker, and
//!     `exited` flag. No `Term`, no `EventLoop` — those are client-side.
//!   - `ClientSession` (in `tui/src/session.rs`) — owns the alacritty
//!     `Term`, `EventLoop`, `EventProxy`, and a [`StreamWriter`] for
//!     keystrokes. No PTY child, no memory cap.
//!
//! Phase 1 lands the primitives incrementally. This slice ships the
//! [`PtyByteFanout`] and a skeletal `DaemonSession` carrying the uid,
//! title, and fanout. The full struct — PTY handle, memory-cap fields,
//! cgroup path — wires in slice 10 when the TUI rewires to RPC and
//! stops directly owning sessions. Until then the existing
//! `tui/src/session.rs::Session` keeps doing its job; this code is
//! reachable from tests and from the (still-being-built)
//! `attach.open` / `manifest.watch` handlers.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Instant;

/// Default per-session PTY-output ring-buffer capacity. The design
/// doc names this 1 MiB; configurable per session type in a later
/// slice. Heavy-output sessions (long compiles, fuzz runs) will blow
/// past this — that's fine; reattach replays the tail, and the live
/// stream stays running regardless of buffer state.
pub const DEFAULT_FANOUT_CAPACITY: usize = 1024 * 1024;

/// Ring buffer + subscriber list for PTY output bytes.
///
/// Producer side: the daemon's PTY-reader thread calls
/// [`push`](Self::push) with each chunk it reads from the master fd.
/// Consumer side: each attached client calls [`subscribe`](Self::subscribe)
/// to get a channel that yields (a) the current buffer tail as its
/// first item (replay for reconnect), then (b) every chunk that
/// arrives going forward.
///
/// ## Semantics
///
/// - Ring eviction is FIFO: when a push would exceed the configured
///   capacity, the oldest bytes are dropped to make room. The buffer
///   never contains more than `capacity` bytes total.
/// - Subscribers that drop their receiver are reclaimed on the next
///   push (the sender errors and is removed from the list). No
///   explicit unsubscribe needed.
/// - Push order across subscribers is preserved: every subscriber
///   sees the same chunks in the same order. Bytes within a push are
///   delivered as one `Vec<u8>` chunk — no internal splitting.
///
/// ## What's intentionally *not* here
///
/// - Memory caps, cgroup paths, exit detection, child-process state.
///   Those belong to the larger `DaemonSession` (this slice has only
///   the skeletal version). When they arrive, the cap-kill signal
///   propagates by *sending an in-band sentinel chunk* the fanout
///   recognises, or by closing all subscriber channels — design call
///   for the slice that lands cap-kill notification.
/// - Per-subscriber filtering or back-pressure. Subscribers are
///   expected to consume promptly; an unbounded mpsc queue means a
///   slow subscriber buffers in its channel rather than blocking the
///   producer. If that ever becomes a real concern, switch the
///   channel to a bounded one and drop chunks on a full subscriber.
/// Result of [`PtyByteFanout::snapshot_since`]. The byte slice
/// is the current ring's contents scoped to bytes after the
/// caller's cursor; the absolute offsets let a caller detect
/// eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanoutSnapshot {
    /// Bytes between `start_offset` and `cursor` (exclusive on
    /// the right). Empty when there's nothing new.
    pub bytes: Vec<u8>,
    /// Absolute offset of the first byte in `bytes`. For first-
    /// time callers (`since = None`), equal to the ring's
    /// `buf_start` (`bytes_written - buffer.len()` at snapshot
    /// time). For follow-up callers, equal to the requested cursor
    /// unless eviction occurred.
    pub start_offset: u64,
    /// `bytes_written` at snapshot time. Pass back as
    /// `since_cursor` on the next call to receive only newer
    /// bytes.
    pub cursor: u64,
    /// `true` iff bytes between the caller's cursor and the
    /// returned `start_offset` were evicted from the ring before
    /// the snapshot ran. Callers can warn or just resume from
    /// `cursor` going forward.
    pub evicted_since_cursor: bool,
    /// `true` iff the producer has closed (child exited /
    /// fanout closed). Companion of [`PtyByteFanout::close`].
    pub closed: bool,
}

pub struct PtyByteFanout {
    inner: Mutex<FanoutInner>,
    /// Sub-2b-1 review-r#2 #1: shared activity timestamp.
    /// `None` for fanouts constructed via the unit-test
    /// `PtyByteFanout::new` shortcut (idle isn't observable
    /// without a `DaemonSession`). `Some` for production
    /// fanouts via `PendingSession::spawn`.
    last_activity_at: Option<SharedLastActivity>,
}

/// Shared cell stamped by both the reader thread (output) and
/// `send_input` (input). `resolve_authorized_session` reads it
/// to compute the wire-level `idle` field.
pub type SharedLastActivity = Arc<Mutex<Option<Instant>>>;

struct FanoutInner {
    buffer: VecDeque<u8>,
    capacity: usize,
    subscribers: Vec<mpsc::Sender<Vec<u8>>>,
    /// Set once the producer has signalled "no more data coming"
    /// (typically: the reader thread saw EOF / read-error on the
    /// PTY master). Subscribers detect this by observing
    /// `Disconnected` on their channel — the producer side drops
    /// its senders here on close so existing receivers see the
    /// transition; future subscribers via `subscribe()` are handed
    /// a fresh receiver whose sender has already been dropped, so
    /// they also see `Disconnected` immediately.
    ///
    /// Slice-10c-c review fix: without this signal,
    /// `crate::control::stream::handle_attach_stream`'s
    /// `recv_timeout` loop kept seeing `Timeout` indefinitely after
    /// the child exited, and the connection thread leaked until the
    /// client disconnected.
    closed: bool,
    /// Total bytes pushed to the fanout over its entire lifetime
    /// (monotonically increasing). Used by `snapshot_since` (slice
    /// 10c-d's `read_session_output`) to give cursor-based callers
    /// "give me what's buffered now" semantics — bytes evicted from
    /// the ring are detectable via the gap between
    /// `buf_start = bytes_written - buffer.len()` and the caller's
    /// cursor.
    bytes_written: u64,
}

impl PtyByteFanout {
    pub fn new(capacity: usize) -> Self {
        Self::with_activity_tracker(capacity, None)
    }

    /// Sub-2b-1 review-r#2 #1: variant used by `PendingSession::spawn`
    /// that wires the per-session `last_activity_at` Arc so the
    /// reader thread can stamp activity on every `push` without an
    /// extra cross-struct hop. The shared cell sits on
    /// `DaemonSession.last_activity_at`; `send_input` and the
    /// reader-thread output path both update it through the same
    /// `SharedLastActivity` clone. Pre-fix the fanout stamped its
    /// own private cell and `send_input` didn't touch any cell at
    /// all — `wait_for_session_idle` returned early immediately
    /// after input because the daemon never saw the activity.
    pub fn with_activity_tracker(
        capacity: usize,
        last_activity_at: Option<SharedLastActivity>,
    ) -> Self {
        Self {
            inner: Mutex::new(FanoutInner {
                buffer: VecDeque::with_capacity(capacity),
                capacity,
                subscribers: Vec::new(),
                closed: false,
                bytes_written: 0,
            }),
            last_activity_at,
        }
    }

    /// Signal "no more data coming" to all subscribers. The
    /// producer (reader thread) calls this when the PTY master's
    /// `read()` returns 0 (EOF) or an unrecoverable error.
    ///
    /// Effect: every live subscriber's sender is dropped, so their
    /// next `recv` / `recv_timeout` returns
    /// `mpsc::RecvTimeoutError::Disconnected`. The buffered ring
    /// content is preserved — a future `subscribe()` still gets
    /// the replay as its first item before observing the closed
    /// state.
    ///
    /// Idempotent: subsequent calls are no-ops (subscribers list
    /// already empty; flag already set).
    pub fn close(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.closed = true;
        // Dropping the senders is what makes existing receivers
        // see Disconnected. The recv loop's next wakeup observes
        // the transition.
        inner.subscribers.clear();
    }

    /// Append `bytes` to the ring (FIFO-evicting if needed) and
    /// broadcast a copy to every live subscriber.
    pub fn push(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let cap = inner.capacity;

        if bytes.len() >= cap {
            // Incoming chunk alone exceeds the buffer — discard the
            // whole buffer and keep only the tail of the chunk.
            inner.buffer.clear();
            inner.buffer.extend(&bytes[bytes.len() - cap..]);
        } else {
            let new_len = inner.buffer.len() + bytes.len();
            if new_len > cap {
                let to_drain = new_len - cap;
                inner.buffer.drain(..to_drain);
            }
            inner.buffer.extend(bytes);
        }

        // Track total bytes pushed (NOT buffer.len() — the ring
        // evicts but `bytes_written` is monotonically increasing).
        // `snapshot_since` uses this to detect cursors that point
        // at evicted data.
        inner.bytes_written = inner.bytes_written.saturating_add(bytes.len() as u64);

        // Broadcast the *whole* incoming chunk (not the ring) to live
        // subscribers. `retain` drops senders whose receiver has been
        // closed — that's how a dropped attach connection cleans up.
        inner.subscribers.retain(|tx| tx.send(bytes.to_vec()).is_ok());

        // Sub-2b-1 review-r#2 #1: stamp shared activity AFTER the
        // fanout's own lock is released to avoid lock-order
        // questions vs `send_input` (which acquires its own
        // per-session writer mutex; we don't want
        // fanout→activity vs writer→activity to overlap). Drop
        // the inner guard explicitly for that reason.
        drop(inner);
        if let Some(ts) = self.last_activity_at.as_ref() {
            let mut slot = ts.lock().unwrap_or_else(|p| p.into_inner());
            *slot = Some(Instant::now());
        }
    }

    /// Return a "give me what's buffered right now" snapshot,
    /// scoped to bytes pushed since the caller's cursor (if any).
    /// Slice 10c-d's `read_session_output` is the consumer.
    ///
    /// Semantics:
    ///   - `since = None` (first call): returns the entire current
    ///     ring. The companion `cursor` advances to the current
    ///     `bytes_written` so the next call sees only newer bytes.
    ///   - `since = Some(N)` where N is within the ring window:
    ///     returns bytes `[N, bytes_written)`. Common case for a
    ///     caller polling at a steady pace.
    ///   - `since = Some(N)` where N is BELOW the ring's first
    ///     buffered byte: the bytes `[N, buf_start)` were evicted.
    ///     The snapshot returns the full current ring with
    ///     `evicted_since_cursor = true`. The caller knows it
    ///     missed some bytes — the toast / log surface can warn,
    ///     or the caller can just resume from the new cursor.
    ///   - `since = Some(N)` where N >= bytes_written: no new
    ///     bytes; returns empty with the same cursor.
    ///
    /// `closed` is the producer's "no more bytes coming" flag —
    /// see [`close`](Self::close). Useful for one-shot MCP callers
    /// that want to know if they should stop polling.
    pub fn snapshot_since(&self, since: Option<u64>) -> FanoutSnapshot {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let bytes_written = inner.bytes_written;
        let buf_len = inner.buffer.len() as u64;
        let buf_start = bytes_written.saturating_sub(buf_len);

        let (bytes, start_offset, evicted_since_cursor) = match since {
            None => {
                // First-time caller: return the full ring.
                let bytes: Vec<u8> = inner.buffer.iter().copied().collect();
                (bytes, buf_start, false)
            }
            Some(n) if n >= bytes_written => {
                // Caller is up-to-date or ahead (shouldn't happen
                // unless they got a stale cursor); return empty.
                (Vec::new(), bytes_written, false)
            }
            Some(n) if n < buf_start => {
                // Some bytes between the caller's cursor and the
                // ring window were evicted. Return current ring +
                // signal eviction.
                let bytes: Vec<u8> = inner.buffer.iter().copied().collect();
                (bytes, buf_start, true)
            }
            Some(n) => {
                // Caller's cursor is within the ring; return tail
                // from offset.
                let offset_in_buf = (n - buf_start) as usize;
                let bytes: Vec<u8> =
                    inner.buffer.iter().skip(offset_in_buf).copied().collect();
                (bytes, n, false)
            }
        };

        FanoutSnapshot {
            bytes,
            start_offset,
            cursor: bytes_written,
            evicted_since_cursor,
            closed: inner.closed,
        }
    }

    /// Register a new subscriber. The returned receiver yields the
    /// current buffer contents (if any) as its first item — that's
    /// the replay every reconnecting client gets. Subsequent items
    /// are the chunks delivered by future [`push`](Self::push) calls.
    ///
    /// If the fanout has already been [`close`](Self::close)d, the
    /// returned receiver yields the buffered replay (if any) and
    /// then immediately observes `Disconnected` on its next
    /// `recv` — the sender is dropped before return rather than
    /// added to the subscribers list. This way a stream that
    /// subscribes after the child exited still sees the replay
    /// AND learns it can wrap up.
    pub fn subscribe(&self) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel();
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if !inner.buffer.is_empty() {
            // Send the current buffer as one chunk. Safe to `.expect`:
            // we just created the channel, so the receiver can't have
            // been dropped yet.
            let replay: Vec<u8> = inner.buffer.iter().copied().collect();
            tx.send(replay).expect("send on fresh channel");
        }
        if inner.closed {
            // Drop tx here so rx sees Disconnected after consuming
            // any replay above. Don't add to the subscribers list
            // — there's no producer anymore.
            return rx;
        }
        inner.subscribers.push(tx);
        rx
    }

    /// Test helper: number of buffered bytes.
    #[cfg(test)]
    fn buffered_len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .buffer
            .len()
    }

    /// Test helper: raw count of `Sender` entries the fanout still
    /// tracks. Dead subscribers (whose receivers were dropped) only
    /// get reaped on the next `push`, so this is "potentially-live"
    /// rather than "guaranteed-live" — assertions should be made
    /// after a push has had a chance to sweep.
    ///
    /// mpsc::Sender doesn't expose `is_disconnected` on stable Rust,
    /// so we can't probe liveness without sending; we accept that
    /// stale entries linger until the next push and structure tests
    /// accordingly.
    #[cfg(test)]
    fn subscriber_slot_count(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .subscribers
            .len()
    }
}

/// Callback invoked by the per-session reaper thread after the
/// child exits and `waitpid` returns. Used by
/// `crate::control::methods::start_session` (slice-10c-c review
/// fix #2) to remove the session from `DaemonState.sessions` so the
/// registry reflects child liveness.
///
/// The callback is single-shot (`FnOnce`) and runs once, in the
/// reaper thread. It receives the typed `DaemonExitStatus`. It
/// MUST NOT panic (the reaper has no recovery path) and MUST NOT
/// block the daemon for long — short critical-section work only.
///
/// Race-safety with the registry insert that follows
/// `DaemonSession::spawn`: see the doc on `SpawnParams::on_exit`.
pub type OnExitCallback = Box<dyn FnOnce(&DaemonExitStatus) + Send + 'static>;

/// Parameters for spawning a daemon-side session. Plain struct
/// (not a builder) because every field is meaningful at spawn time
/// and Rust's struct-update syntax handles defaults adequately.
pub struct SpawnParams {
    /// Stable session UID. Used as the key in
    /// `DaemonState.sessions` and embedded in kill-log paths,
    /// MCP env vars, etc.
    pub uid: String,
    /// Workspace the session is associated with. Recorded on
    /// `DaemonSession` so the slice 10d-mcp-surface Session-caller
    /// auth can answer "is target in caller's workspace?".
    pub workspace_id: String,
    /// Session-type discriminator surfaced by `list_sessions` to
    /// the Python MCP tool. Values mirror the TUI's existing
    /// vocabulary (`"claude-code"`, `"codex"`, `"bash"`). Phase 1
    /// daemon doesn't act on this beyond surfacing — future
    /// slices can branch on it (e.g. workflow controller
    /// engine selection).
    pub session_type: String,
    /// Parent session uid for sessions spawned via MCP
    /// `start_session` from an agent. Surfaced by `list_sessions`
    /// alongside the "managed-by" sidebar marker. `None` for
    /// operator-started sessions.
    pub managed_by_uid: Option<String>,
    /// Planning task uid this session is bound to, when launched
    /// from a tasked workspace. Surfaced by `list_sessions` and
    /// (slice 10d-mcp-surface-2) used by the Session-caller
    /// descendant-task-tree authorization. `None` for taskless
    /// sessions.
    pub task_id: Option<String>,
    /// Transcript file path for this session, when the TUI knows
    /// it at spawn time (e.g. clone/resume seed flows). Surfaced
    /// by `resolve_authorized_session` (sub-2b-1) so the Python
    /// MCP `read_session_output` tool can read transcript
    /// messages directly. `None` until either the TUI sends it
    /// at spawn or a post-detection update RPC lands (deferred
    /// — not in 2b-1's scope). When `None`,
    /// `resolve_authorized_session` returns `state: "pending"`
    /// and `transcript_path: null`; the Python tool
    /// short-circuits to empty messages + poll-again behavior.
    ///
    /// Stored as `Option<String>` (not `PathBuf`) because the
    /// daemon never opens this file — it only echoes the path
    /// back over the wire; PathBuf adds OS-encoding ceremony
    /// without benefit.
    pub transcript_path: Option<String>,
    /// Sub-2b-3 review-fix #1: memory-cap inheritance fields.
    /// Daemon-spawned children inherit the parent's cap when an
    /// agent calls `mcp_start_session` to spawn a subtask child.
    /// Pre-fix the daemon didn't carry the cap bytes so a capped
    /// agent could spawn an uncapped child via the MCP path —
    /// directly defeating the resource guard the TUI applies at
    /// spawn time (`tui/src/app.rs::try_spawn_via_daemon`).
    ///
    /// Wire shape: TUI's `start_session` sends all three so the
    /// daemon-side `DaemonSession` carries enough state to
    /// re-wrap argv for subtask spawns. `mcp_start_session` reads
    /// these off the caller and wraps via the daemon-local
    /// `wrap_with_systemd_run` helper.
    pub memory_cap_soft_bytes: Option<u64>,
    pub memory_cap_hard_bytes: Option<u64>,
    pub cgroup_prefix: Option<PathBuf>,
    /// 10d-2c-1 review round-5 (F1): workflow run id this session
    /// is a participant of, when the spawn happens with workflow
    /// context already known. `None` for non-workflow spawns.
    /// Surfaced via `lookup_session_any` so the auth check in
    /// `workflow_transition` / `workflow_done` recognizes daemon-
    /// attached workflow participants. After-the-fact tagging
    /// (workflow launched on an already-spawned daemon session)
    /// uses the `session.set_workflow_context` RPC; same field,
    /// different write path.
    pub workflow_run_id: Option<String>,
    /// Role name this session is bound to within the workflow run.
    /// See [`workflow_run_id`](Self::workflow_run_id).
    pub workflow_role: Option<String>,
    /// Continuous-task this session is a tick of, carried from
    /// `StartSessionParams.continuous_task_id` onto the final
    /// `DaemonSession`. `None` for ordinary spawns; the trigger
    /// funnel that sets it lands in Phase 2. See
    /// DESIGN_CONTINUOUS_TASKS.md §6.
    pub continuous_task_id: Option<String>,
    /// Global-permissions grant. When `true`, this session's
    /// Session-caller auth checks (`auth::check_session_caller` and
    /// `check_session_caller_for_exited`) short-circuit to `Allow`
    /// for ANY target — the session can prompt, read, kill, and
    /// spawn against every other session regardless of task tree or
    /// workspace. Granted only by the operator (TUI) or by a caller
    /// that is itself global (the `mcp_start_session` escalation
    /// guard), so a normal agent can never self-promote. `false` is
    /// the default and the safe baseline (descendant-only scope).
    pub global_perms: bool,
    /// Human-readable label for the sidebar. Not used for routing.
    pub title: String,
    /// Program to exec. Typically `claude`, `codex`, or `bash`.
    pub shell: String,
    /// Arguments to pass to `shell`.
    pub args: Vec<String>,
    /// Working directory for the child. Usually a worktree path.
    pub working_dir: Option<PathBuf>,
    /// Environment variables to inject into the child. Empty map
    /// is fine; the child inherits the daemon's env plus these
    /// additions (per `CommandBuilder::env` semantics).
    pub env: HashMap<String, String>,
    /// PTY size at spawn. Resized later via `send_resize` from the
    /// attached `term_shim::StreamWriter`.
    pub cols: u16,
    pub rows: u16,
    /// Size of the PTY-byte ring buffer. Defaults to
    /// [`DEFAULT_FANOUT_CAPACITY`] in production; tests pass a
    /// small value to exercise eviction.
    pub fanout_capacity: usize,
    /// Directory the memory-cap reaper writes JSONL kill records
    /// to. When `Some`, `PendingSession::spawn` captures the
    /// per-spawn baseline offset via
    /// [`crate::reaper::capture_baseline_for_spawn`] and the
    /// reaper thread builds a `LastExit` via
    /// [`crate::reaper::build_last_exit_since`] when the child
    /// exits — so the attach-stream End frame can carry the
    /// correct `memory_cap_kill` flag (slice-10c-e-2 review fix
    /// #2). `None` for tests / configurations without memory
    /// caps; LastExit then carries `memory_cap_kill: false`
    /// unconditionally.
    pub kills_dir: Option<PathBuf>,
}

impl SpawnParams {
    /// Convenience constructor used by tests and by the
    /// `start_session` JSON-RPC handler in 10c-b. Production
    /// callers override [`working_dir`](Self::working_dir) with
    /// the workspace's `worktree_path`; the default below exists
    /// only as a safety net for callers that don't set one.
    ///
    /// ## Why `working_dir` defaults to `std::env::temp_dir()`
    ///
    /// Slice-10c-b review caught a flake: `portable-pty`'s
    /// `spawn_command` falls back to `$HOME` as the child's cwd
    /// when `working_dir` is unset. Other daemon tests
    /// (`daemon/src/lib.rs::tests` around line 601) mutate
    /// `HOME` process-wide to drive the canonical-default-path
    /// branch in `bind_socket`. Parallel test scheduling
    /// occasionally collides the two — a session test spawns
    /// while another test has `HOME=/nonexistent-…`, and
    /// `spawn_command` fails with `ENOENT`.
    ///
    /// Defaulting `working_dir` to `std::env::temp_dir()`
    /// removes the dependence on `$HOME` entirely. The temp dir
    /// (`/tmp` or `$TMPDIR`) is always present and never the
    /// target of test mutation. This matches production
    /// (`start_session` always overrides), so the change is a
    /// pure defensive default for tests and tools.
    ///
    /// Note that this happens at construction time — a caller
    /// that explicitly sets `working_dir = None` after `::new`
    /// returns reverts to the portable-pty default behavior.
    /// The intent is "safe default, no behavior change for
    /// callers that set it explicitly."
    pub fn new(
        uid: impl Into<String>,
        title: impl Into<String>,
        shell: impl Into<String>,
    ) -> Self {
        Self {
            uid: uid.into(),
            // Tests that don't care about auth pass empty;
            // production via `start_session` always overrides.
            workspace_id: String::new(),
            // Defaults are "claude-code" for session_type and
            // None for the two optional fields. Production via
            // `start_session` overrides session_type when the
            // wire carries it; managed_by_uid + task_id stay
            // None until a future wire extension carries them.
            session_type: "claude-code".to_string(),
            managed_by_uid: None,
            task_id: None,
            transcript_path: None,
            memory_cap_soft_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            // 10d-2c-1 review round-5 (F1): non-workflow default.
            workflow_run_id: None,
            workflow_role: None,
            // Phase-1 continuous wire field: non-continuous default.
            continuous_task_id: None,
            // Global perms default to off — descendant-only scope is
            // the safe baseline. Only the operator (TUI) or a global
            // caller can flip this true at spawn time.
            global_perms: false,
            title: title.into(),
            shell: shell.into(),
            args: Vec::new(),
            working_dir: Some(std::env::temp_dir()),
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            fanout_capacity: DEFAULT_FANOUT_CAPACITY,
            kills_dir: None,
        }
    }
}

/// Daemon-side session record. Slice 10c-a wires up the PTY child:
/// `DaemonSession::spawn` opens a master/slave PTY pair, spawns the
/// requested program against the slave, and spins a background
/// reader thread that pulls bytes off the master and pushes them
/// into the shared [`PtyByteFanout`]. Attached clients (TUIs) reach
/// the fanout through the `session.attach` / `attach.open` flow
/// (slice 10c-c).
///
/// What this struct does NOT do yet (deferred sub-slices):
///   - **Memory-cap wrapping**: the TUI's existing `wrap_with_systemd_run`
///     helper hasn't moved daemon-side. Add the `memory_cap` field
///     and the wrap call in 10c-d alongside the other session-mutation
///     method relocations.
///   - **Cgroup-OOM watcher**: same — slice-12's reaper consumes the
///     existing TUI watcher today; the daemon-side equivalent lands
///     when the watcher relocates.
///   - **Wakeup-burst tracking**: used for idle detection in
///     `session_runtime_state`. Stays TUI-side until idle detection
///     itself relocates.
/// Typed exit status. Maps cleanly onto [`crate::manifest::LastExit`]:
/// `code` is `Option<i32>` because `None` already encodes "no usable
/// exit code" (signal kill, daemon-forced kill, etc.). `signal`
/// preserves which signal terminated the child, so the memory-cap
/// kill detection in `crate::reaper` can distinguish a SIGKILL
/// (the cgroup OOM-killer's signal) from a normal `exit(N)`.
///
/// `portable_pty::ExitStatus::exit_code()` collapses these into a
/// single `u32`, which would break the named acceptance criterion
/// "memory-cap kills indistinguishable from today's signal 9
/// toast" — so we bypass it and use raw `libc::waitpid` for the
/// real status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonExitStatus {
    /// Normal-exit code from `WEXITSTATUS`. `Some(0)` for clean
    /// success, `Some(N)` for `exit(N)`. `None` when the child was
    /// killed by a signal or when the status can't be decoded.
    pub code: Option<i32>,
    /// Terminating signal from `WTERMSIG`. `Some(9)` for the
    /// cap-kill SIGKILL case (the doc's named criterion), `Some(N)`
    /// for any signal that killed the child. `None` for normal
    /// exits.
    pub signal: Option<i32>,
}

/// Per-session exit probe used by the attach-stream End-frame
/// path. Two-stage shape (slice-10c-e-2 review-6 fix):
///   - Kernel-observable exit (code) is cached at `waitpid` time
///     by the reaper.
///   - `memory_cap_kill` is classified LAZILY at consume time via
///     [`crate::reaper::probe_kill_log_since`] against the kill
///     log past the per-spawn baseline.
///
/// Why lazy: the cgroup-OOM writer's kill-record write has no
/// synchronous ordering relationship with `waitpid` returning. A
/// reaper that snapshots `memory_cap_kill` at waitpid time can
/// cache `false` and then miss a record that lands a few ms
/// later. Reading at consume time (after fanout close, after the
/// attach-stream's End-frame is about to fire — i.e. well past
/// any plausible window for the OOM writer to have flushed)
/// naturally synchronizes through the filesystem.
///
/// `Arc<...>` so the attach-stream subscriber can hold a clone
/// independent of `DaemonState.sessions` membership.
pub type SharedLastExit = std::sync::Arc<LastExitProbe>;

/// Per-session writer behind an `Arc<Mutex>` so the global
/// `DaemonState` mutex isn't held across blocking PTY writes.
/// Slice 10c-e-3b-fix3 — see [`DaemonSession::writer`] for the
/// full rationale. Callers clone the Arc out of state under the
/// state lock, drop the state lock, then lock just this mutex to
/// write.
pub type SessionWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// The lazy-classification probe. Kernel exit goes into the
/// mutex slot; memory-cap config is set at construction (spawn
/// time) and read at consume time.
pub struct LastExitProbe {
    /// Set by the reaper after `waitpid` returns. `None` until
    /// then — `build_end_payload`'s spin loop polls for this to
    /// become `Some` before emitting the End frame so it doesn't
    /// surface `exit_code: null` from a wind-down that beat the
    /// reaper.
    kernel: std::sync::Mutex<Option<KernelExitStatus>>,
    /// Captured at spawn time, pre-fork.
    kills_dir: Option<PathBuf>,
    /// Per-spawn baseline (captured pre-spawn per slice-10c-e-2
    /// review-4 fix). Records past this offset belong to *this*
    /// spawn.
    kills_baseline: u64,
    /// Session uid — needed for `<uid>.jsonl` path resolution.
    uid: String,
    /// Set to `true` by the `kill_session` RPC handler BEFORE it
    /// issues the SIGKILL via pidfd (slice 10d watcher-fix #4).
    /// Joins with `kill_status` + signal in
    /// `crate::reaper::is_cap_kill` so an operator-driven kill on
    /// a session that ALSO happens to have a transient
    /// `protected`/`no_pids` record past baseline doesn't get
    /// misattributed as a cap kill.
    ///
    /// Lives on `LastExitProbe` (not on `DaemonSession`) so the
    /// attach-stream End-frame consumer can read it after the
    /// reaper has removed the session from the registry — the
    /// `SharedLastExit` Arc outlives `DaemonSession`.
    operator_kill_requested: std::sync::atomic::AtomicBool,
}

/// Kernel-observable exit. What the reaper can know
/// synchronously from `waitpid`.
///
/// Slice 10d watcher-fix #1.5 (refine consumer): `signal` was
/// added so `LastExitProbe::snapshot` can distinguish "clean
/// exit but a transient soft-limit breach was recorded" from
/// "kernel-killed". The watcher fires on `memory.events high`,
/// which can be a *recoverable* soft-limit hit — a process that
/// touched the high watermark, the kernel reclaimed pages, and
/// the process kept running to a clean exit. Pre-fix-#1.5 such
/// a record past baseline incorrectly flipped
/// `memory_cap_kill: true`, surfacing a phantom toast for an
/// exit today wouldn't have shown one. Now the consumer joins
/// `kill_status` from the record with `signal` here: only
/// `kill_status == "killed_by_us"` is unconditional;
/// `protected`/`no_pids`/`already_dead` require a signal exit
/// to flip the flag. See `crate::reaper::is_cap_kill`.
#[derive(Debug, Clone)]
pub struct KernelExitStatus {
    /// `WEXITSTATUS` — set when the child exited via `_exit(N)`.
    /// `None` when the child was signal-killed (`WIFSIGNALED`).
    pub code: Option<i32>,
    /// `WTERMSIG` — set when the child was killed by a signal.
    /// `None` for clean exits. The "kernel killed me" indicator
    /// the cap-kill consumer needs.
    pub signal: Option<i32>,
}

impl LastExitProbe {
    /// Construct the probe at spawn time. `kernel` starts None;
    /// the reaper sets it when waitpid returns.
    pub fn new(
        uid: String,
        kills_dir: Option<PathBuf>,
        kills_baseline: u64,
    ) -> Self {
        Self {
            kernel: std::sync::Mutex::new(None),
            kills_dir,
            kills_baseline,
            uid,
            operator_kill_requested: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Called by the reaper after `waitpid` returns.
    pub fn set_kernel(&self, status: KernelExitStatus) {
        let mut slot = self.kernel.lock().unwrap_or_else(|p| p.into_inner());
        *slot = Some(status);
    }

    /// Mark that an operator-initiated kill (the `kill_session`
    /// RPC) is about to fire, BEFORE the pidfd-SIGKILL goes out.
    /// Slice 10d watcher-fix #4: prevents the signal exit that
    /// follows from being misattributed as a cap kill when a
    /// transient `protected`/`no_pids`/`already_dead` record
    /// happens to exist past baseline.
    ///
    /// Idempotent — setting twice has no effect, and a kill
    /// initiated by the cap-watcher path (which writes a
    /// `killed_by_us` record) supersedes via the priority order
    /// inside `is_cap_kill`.
    pub fn mark_operator_kill_requested(&self) {
        self.operator_kill_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Read the operator-kill flag for the `is_cap_kill` join.
    pub fn operator_kill_requested(&self) -> bool {
        self.operator_kill_requested
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// True once the reaper has populated kernel exit. Used by
    /// `build_end_payload`'s spin loop to bound the wait.
    pub fn kernel_set(&self) -> bool {
        self.kernel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// Snapshot the End-frame payload's `(exit_code, memory_cap_kill)`.
    /// Reads kernel exit from the cached slot; scans the kill log
    /// AT THIS MOMENT for `memory_cap_kill` — that's the lazy
    /// classification that closes the slice-10c-e-2 review-6
    /// race.
    ///
    /// Slice 10d watcher-fix #1.5: `memory_cap_kill` is now the
    /// join of `kill_status` (most-decisive record past baseline)
    /// with the kernel's `WTERMSIG` via [`crate::reaper::is_cap_kill`].
    /// A clean exit with a `protected`/`no_pids`/`already_dead`
    /// record no longer fires the toast — that's the transient
    /// soft-limit breach case the watcher records but the kernel
    /// never had to escalate. Only `killed_by_us` (the watcher's
    /// own kill) is unconditionally cap-kill.
    pub fn snapshot(&self) -> (Option<i32>, bool) {
        let kernel_snapshot: Option<KernelExitStatus> = self
            .kernel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let code = kernel_snapshot.as_ref().and_then(|k| k.code);
        let signal = kernel_snapshot.as_ref().and_then(|k| k.signal);
        let operator_kill = self.operator_kill_requested();
        let memory_cap_kill = match &self.kills_dir {
            Some(dir) => {
                let probe = crate::reaper::probe_kill_log_since(
                    dir,
                    &self.uid,
                    self.kills_baseline,
                );
                crate::reaper::is_cap_kill(
                    probe.kill_status.as_deref(),
                    signal,
                    operator_kill,
                )
            }
            None => false,
        };
        (code, memory_cap_kill)
    }

    /// 10e-a: build the typed [`crate::manifest::LastExit`] record
    /// for this session at the moment of exit. Consumed by the
    /// reaper-cleanup callback (`on_exit` in
    /// `crate::control::methods::start_session`) to populate
    /// `state.workspaces[ws].sessions[*].last_exit` AND to broadcast
    /// `ManifestDiff::Exited` to live `manifest.watch` subscribers.
    ///
    /// Combines:
    /// - Kernel exit code from the cached `KernelExitStatus` (the
    ///   reaper has set this before invoking `on_exit`).
    /// - Kill-log probe at `kills_dir` past `kills_baseline` via
    ///   [`crate::reaper::build_last_exit_since`] when a kills dir
    ///   is configured (i.e. cap was requested at spawn).
    /// - `exited_at` provided by the caller (typically
    ///   `SystemTime::now()` as seconds since epoch) — kept as a
    ///   parameter so tests can pin a deterministic value.
    ///
    /// When `kills_dir` is `None` (no cap requested), returns a
    /// `LastExit` with `memory_cap_kill: false` and
    /// `kills_file_offset: None`. The kernel `code` still flows
    /// through. This is the "clean exit, no cap" shape.
    pub fn build_last_exit(&self, exited_at: f64) -> crate::manifest::LastExit {
        let kernel = self
            .kernel
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let code = kernel.as_ref().and_then(|k| k.code);
        let signal = kernel.as_ref().and_then(|k| k.signal);
        let operator = self.operator_kill_requested();
        match &self.kills_dir {
            Some(dir) => crate::reaper::build_last_exit_since(
                dir,
                &self.uid,
                self.kills_baseline,
                code,
                signal,
                operator,
                exited_at,
            ),
            None => crate::manifest::LastExit {
                code,
                memory_cap_kill: false,
                kills_file_offset: None,
                exited_at,
            },
        }
    }
}

pub struct DaemonSession {
    pub uid: String,
    pub title: String,
    /// Workspace this session was spawned into. Populated from
    /// `StartSessionParams.workspace_id` at spawn time; used by
    /// the slice 10d-mcp-surface Session-caller auth check to
    /// answer "is target session in the same workspace as the
    /// caller session?". Phase 1 auth is same-workspace + self;
    /// future slices plumb task-tree to enable descendant-task
    /// scoping.
    pub workspace_id: String,
    /// Session-type discriminator (`"claude-code"` / `"codex"` /
    /// `"bash"`) surfaced by `list_sessions` to the Python MCP
    /// tool. Mirrors the TUI's `TerminalSession.session_type`
    /// vocabulary.
    pub session_type: String,
    /// Parent session uid for sessions spawned via MCP
    /// `start_session`. `None` for operator-started sessions.
    /// Surfaced by `list_sessions` and (sub-2) used for the
    /// "managed-by" sidebar marker.
    pub managed_by_uid: Option<String>,
    /// Planning task uid this session is bound to. `None` for
    /// taskless sessions. Slice 10d-mcp-surface-2 uses this for
    /// the Session-caller descendant-task-tree auth check.
    pub task_id: Option<String>,
    /// Transcript file path for this session (sub-2b-1).
    /// Populated from `StartSessionParams.transcript_path` at
    /// spawn time when the TUI knows the path, else `None`. The
    /// daemon never reads this file — it only echoes the value
    /// back via `resolve_authorized_session` so the Python MCP
    /// `read_session_output` tool can parse the transcript
    /// without a TUI round-trip. See the field doc on
    /// `SpawnParams.transcript_path` for the pending-vs-ready
    /// state semantics.
    pub transcript_path: Option<String>,
    /// Sub-2b-3 review-fix #1: memory-cap inheritance.
    /// `mcp_start_session` reads these off the caller and wraps
    /// the subtask child's argv via daemon-local
    /// `wrap_with_systemd_run` so cap is preserved across MCP-
    /// driven spawn chains. `None` for sessions launched
    /// without a cap (the wrap is a passthrough in that case).
    pub memory_cap_soft_bytes: Option<u64>,
    pub memory_cap_hard_bytes: Option<u64>,
    pub cgroup_prefix: Option<PathBuf>,
    /// Last-known PTY window size (cols × rows). Seeded from
    /// `SpawnParams` at spawn time and updated by
    /// [`resize`](Self::resize) on every inbound attach-stream
    /// Resize frame. Read by `mcp_start_session` so an
    /// agent-spawned child PTY inherits the *caller's current*
    /// terminal width rather than falling back to the 80×24 serde
    /// default — that default was why MCP-spawned claude/codex
    /// sessions opened in a too-narrow window. See
    /// `control::methods::mcp_start_session`.
    pub last_cols: u16,
    pub last_rows: u16,
    /// 10d-2c-1 review round-5 (F1): workflow context for the
    /// auth check on `workflow_transition` / `workflow_done`.
    /// Populated at spawn from
    /// `StartSessionParams.workflow_run_id` / `.workflow_role`
    /// when the workflow is known up front, OR via the
    /// `session.set_workflow_context` RPC after-the-fact when
    /// the TUI launches a workflow on an already-spawned
    /// daemon-attached session (the Existing-slot path in the
    /// former TUI controller's launch).
    ///
    /// `None` for non-workflow sessions — the auth check then
    /// rejects them, which is correct: a daemon-attached
    /// session not bound to a workflow has no business forging
    /// transitions.
    ///
    /// **Authority** for daemon-attached sessions lives HERE,
    /// not in `tui_sessions`. The round-3 filter excluded
    /// daemon-attached sessions from the TUI's pushed snapshot
    /// to avoid duplication; this field is the canonical source
    /// for daemon-owned sessions' workflow context. See
    /// `state::lookup_session_any`.
    pub workflow_run_id: Option<String>,
    pub workflow_role: Option<String>,
    /// Phase-1 continuous wire field: the continuous-task this
    /// session is a tick of, carried through from `SpawnParams`.
    /// `None` for ordinary sessions. See DESIGN_CONTINUOUS_TASKS.md §6.
    pub continuous_task_id: Option<String>,
    /// Global-permissions grant for this session's Session-caller
    /// auth. When `true`, `auth::check_session_caller` /
    /// `check_session_caller_for_exited` short-circuit to `Allow`
    /// for any target, and `list_sessions` returns every session
    /// rather than the caller's task-tree slice. Carried from
    /// `SpawnParams` through `arm_reaper`. See the field doc on
    /// `SpawnParams.global_perms` for the grant rules.
    pub global_perms: bool,
    /// Sub-2b-1 review-r#2 #2: transcript generation counter.
    /// Initialized to 0; incremented by `session.set_transcript_path`
    /// when the incoming path differs from `transcript_path`
    /// (e.g. `/clear`, `/compact`, codex resume rebind — the TUI
    /// re-detects a new JSONL file). Surfaced by
    /// `resolve_authorized_session` so Python tool cursors
    /// (`v1:<generation>:<offset>`) reset when the underlying
    /// file rotates, avoiding applying old-file offsets to the
    /// new transcript.
    ///
    /// Same-path re-pushes (no-op semantics on the path) MUST
    /// NOT bump generation — otherwise idempotent re-discovery
    /// pings from the TUI would invalidate the agent's cursor
    /// every poll.
    pub generation: u64,
    /// Sub-2b-1 review-r#2 #1: shared "last activity" cell.
    /// Bumped by both the fanout (output side, via the reader
    /// thread) AND `methods::send_input` (input side). Read by
    /// `resolve_authorized_session` to compute `idle`.
    /// Pre-fix only output bumped the cell, so an agent calling
    /// `send_input` then immediately `wait_for_session_idle`
    /// would return early because the daemon never observed
    /// the input as activity.
    pub last_activity_at: SharedLastActivity,
    /// `Arc` so the reader thread can hold a clone independently of
    /// the `DaemonSession` instance — the thread pushes bytes into
    /// the fanout, the dispatcher / attach.open consumers subscribe
    /// to it.
    pub fanout: Arc<PtyByteFanout>,
    /// OS PID of the child at spawn time. Kept around for diagnostic
    /// correlation (kill-log paths key off `<session_uid>` not pid,
    /// but the pid still shows up in logs and `ps` output during
    /// troubleshooting). **Not used for signalling** — that's the
    /// pidfd's job (PID-reuse-safe). **Not used for wait** — that's
    /// the reaper thread's job, via the kernel's parent-child
    /// relationship (race-free regardless of PID reuse because the
    /// kernel keeps the child in the daemon's children list until
    /// `waitpid` succeeds).
    pub pid: libc::pid_t,
    /// File descriptor returned by `pidfd_open(pid, 0)` immediately
    /// after spawn. Bound to *this specific process* by the kernel;
    /// immune to PID reuse the moment it is open. Sending SIGKILL
    /// via `pidfd_send_signal` returns `ESRCH` after the child has
    /// been reaped (harmless: we treat ESRCH as success in
    /// [`kill`](Self::kill)). The slice-10c-b reviewer flagged that
    /// the previous `libc::kill(self.pid, SIGKILL)` had a TOCTOU
    /// window — between the reaper's `waitpid` returning and Drop
    /// running, the kernel can recycle the PID to another
    /// user-owned process, and the legacy code would have hit *that*
    /// process. The pidfd closes that window.
    pidfd: OwnedFd,
    /// Writer half of the master PTY. Used by `send_input`.
    ///
    /// ## Why `Arc<Mutex<...>>` (slice 10c-e-3b-fix3)
    ///
    /// Pre-fix3 this was `Box<dyn Write + Send>` accessed via
    /// `&mut self`, which forced callers to hold the
    /// `DaemonState` mutex across `write_all`. PTY writes are
    /// blocking — if the kernel buffer fills (paste larger than
    /// Linux's per-PTY buffer, typically 4-16 KiB, or a child
    /// that's stopped draining stdin), `write_all` blocks
    /// indefinitely. Every other daemon RPC (kill_session on
    /// THIS session, kill_session on UNRELATED sessions,
    /// session.attach, read_session_output, reaper cleanup) would
    /// stall waiting for the state mutex.
    ///
    /// `Arc<Mutex<>>` is the standard split for "per-session
    /// concurrency, not global concurrency." Callers clone the
    /// Arc out of state under the state lock, drop the state
    /// lock, then lock the per-writer mutex to actually write.
    /// Other RPCs touching `state.sessions` are unaffected.
    ///
    /// Multiple input sources (attach-stream Input frames + RPC
    /// `send_input`) serialize correctly through this mutex —
    /// interleaving them mid-character would mangle escape
    /// sequences and Unicode, which is exactly the wrong
    /// semantics.
    ///
    /// `pub` so callers in `control::{stream, methods}` can
    /// `Arc::clone(&session.writer)` without going through a
    /// helper. The convenience `send_input` method below remains
    /// for tests; production hot paths clone the Arc directly.
    pub writer: SessionWriter,
    /// Cached exit status. Populated lazily by `try_wait` the first
    /// time the reaper's channel yields a status; subsequent
    /// `try_wait` calls return this cached value.
    cached_exit: Option<DaemonExitStatus>,
    /// Receives the typed exit status from the reaper thread. The
    /// reaper sends exactly once when its `waitpid` returns;
    /// thereafter the channel is closed. `try_wait` drains it.
    exit_rx: mpsc::Receiver<DaemonExitStatus>,
    /// Reader-thread handle. Owned but not joined on drop — the
    /// thread terminates on its own when the PTY's reader sees EOF
    /// (which happens once the child exits + master fd is dropped).
    /// `_` because the field is never read after construction.
    _reader_handle: JoinHandle<()>,
    /// Reaper-thread handle. The reaper *owns* the
    /// `portable_pty::Child` handle (transferred at spawn time)
    /// and blocks on `libc::waitpid(pid, ..., 0)` for the typed
    /// `DaemonExitStatus`. We use raw `waitpid` rather than
    /// `Child::wait()` because portable-pty's `ExitStatus`
    /// collapses code and signal into a single `u32` — that
    /// would break the named acceptance criterion "memory-cap
    /// kills indistinguishable from today's signal 9 toast."
    /// The kernel only allows the parent (us) to reap, so the
    /// reaper is the sole waiter; no race.
    _reaper_handle: JoinHandle<()>,
    /// Hold the master PTY open until drop. Closing it earlier
    /// would SIGHUP the child unexpectedly. The reader thread holds
    /// its own clone of the reader half; this `Box` keeps the
    /// underlying device alive.
    _master: Box<dyn portable_pty::MasterPty + Send>,
    /// Most-recently-observed exit info. Written by the reaper
    /// thread after `waitpid` returns; read by the attach-stream
    /// End-frame path. See [`SharedLastExit`].
    ///
    /// Slice-10c-e-2 review fix #2: prior to this, the End frame
    /// always carried `Value::Null` and the TUI's exit decoder
    /// surfaced `exit_code: None, memory_cap_kill: false`
    /// unconditionally — breaking the "memory-cap kill on
    /// attached sessions surfaces via End frame" criterion.
    pub last_exit: SharedLastExit,
    /// Optional cgroup-OOM watcher thread handle (slice
    /// 10d-memory-cap-relocation review fix). `Some` for sessions
    /// spawned with a memory cap; `None` otherwise. The handle is
    /// detached on session removal — the watcher's main loop
    /// self-terminates when the cgroup vanishes (which happens
    /// after the session's child exits and systemd cleans up
    /// the scope), so a synchronous join would only block the
    /// reaper-cleanup path without adding safety.
    ///
    /// The field exists primarily so `start_session` can attach
    /// the handle to the registry-resident session at the same
    /// instant the session is inserted — if `Drop` of
    /// `DaemonSession` ever needs to do bounded join in the
    /// future, the handle is here. For now, drop = detach.
    pub watcher_handle: Option<JoinHandle<()>>,
}

/// Extract the transcript-file UUID (the resume key) from a stored
/// `transcript_path`. Both engines name the file `<uuid>.jsonl`
/// (Claude: `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`; Codex:
/// `~/.codex/sessions/YYYY/MM/DD/<uuid>.jsonl`), so the file stem IS
/// the id. The inverse of
/// [`crate::transcript_detect::claude_transcript_path`] /
/// [`codex_transcript_path`](crate::transcript_detect::codex_transcript_path),
/// which rebuild a path from the id. Returns `None` for a pathless or
/// extension-less value so the manifest records "no transcript yet"
/// honestly rather than guessing.
pub(crate) fn transcript_id_from_path(path: &str) -> Option<String> {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}

impl DaemonSession {
    /// Project this live session into a persistable
    /// [`ManifestEntry`](crate::manifest::ManifestEntry) for the
    /// daemon's own durable registry (P0 session durability, S1).
    ///
    /// Carries every field the daemon already tracks that a restart
    /// needs to re-spawn + resume the session: `uid`, engine
    /// (`session_type`), the resume key (`transcript_id`, derived
    /// from `transcript_path`), the task/workspace/managed-by
    /// identity, the workflow + continuous tags, and the
    /// `global_perms` grant. Worktree path is NOT here — it comes
    /// from the owning `ManifestWorkspace` (keyed by `workspace_id`)
    /// when the registry is assembled in
    /// [`DaemonState::build_daemon_manifest`](crate::state::DaemonState::build_daemon_manifest).
    ///
    /// TUI-only presentation fields the daemon doesn't model
    /// (`hidden`, idle/burst timers, `notify_on_idle`,
    /// `seeded_from_snapshot`) default to their inert values; the TUI
    /// owns those in its own `tui-sessions.json`. `last_exit` is
    /// `None` because this projection is for *live* sessions —
    /// exits are recorded by the reaper's manifest mutation, not
    /// here. `host_id` is always `local`: a daemon doesn't know its
    /// own remote name (the operator's `hosts.toml` assigns it), so
    /// the TUI overrides it at load time for remote-loaded sessions.
    ///
    /// **Mem-cap fields are intentionally NOT carried yet** —
    /// `ManifestEntry` has no slots for them, and re-applying the cap
    /// is a restore-spawn concern (S2), not a write-only-persist (S1)
    /// one. See DESIGN_SESSION_DURABILITY.md.
    pub fn to_manifest_entry(&self) -> crate::manifest::ManifestEntry {
        crate::manifest::ManifestEntry {
            uid: self.uid.clone(),
            managed_by_uid: self.managed_by_uid.clone(),
            generation: self.generation,
            label: self.title.clone(),
            session_type: self.session_type.clone(),
            transcript_id: self
                .transcript_path
                .as_deref()
                .and_then(transcript_id_from_path),
            hidden: false,
            idle_timeout_secs: 0,
            burst_threshold: 0,
            workflow_run_id: self.workflow_run_id.clone(),
            workflow_role: self.workflow_role.clone(),
            continuous_task_id: self.continuous_task_id.clone(),
            task_id: self.task_id.clone(),
            notify_on_idle: false,
            // S2: carry the cap triple so restore can re-apply the
            // argv-level systemd-run wrap (a memory cap is NOT inherited
            // across a process restart — it must be reconstructed).
            memory_cap_soft_bytes: self.memory_cap_soft_bytes,
            memory_cap_hard_bytes: self.memory_cap_hard_bytes,
            cgroup_prefix: self.cgroup_prefix.clone(),
            global_perms: self.global_perms,
            seeded_from_snapshot: None,
            last_exit: None,
            host_id: crate::host_id::HostId::local(),
        }
    }
}

/// Phase-1 result of [`DaemonSession::spawn`]. Holds a live child
/// + PTY infrastructure but **no reaper thread yet**. Returned by
/// [`PendingSession::spawn`]; consumed by
/// [`PendingSession::arm_reaper`] which spawns the reaper and
/// produces a full [`DaemonSession`].
///
/// ## Why two phases (slice-10c-c review fix)
///
/// `start_session`'s registry-insert and the reaper's on-exit
/// remove must serialize through the daemon-state mutex. If the
/// reaper is already running by the time `start_session` returns
/// `PendingSession`, a fast-exit child can fire `on_exit` before
/// the caller has even *taken* the state lock — `remove(uid)` is
/// a no-op (not yet inserted), then the caller's later insert
/// strands a dead entry forever.
///
/// The two-phase split closes the race: phase-1 spawns the child
/// without a reaper, the caller takes the state lock, then
/// [`arm_reaper`](Self::arm_reaper) spawns the reaper while the
/// lock is held. The reaper's `on_exit` callback acquires the
/// same lock; on any fast-exit case the callback blocks on the
/// mutex until the caller's insert + unlock completes, then
/// removes the (now-correctly-present) entry. See
/// `crate::control::methods::start_session` for the canonical
/// caller pattern.
pub struct PendingSession {
    /// `Option`-wrapped so [`arm_reaper`](Self::arm_reaper) can
    /// `.take()` the inner struct out, and [`Drop`] handles the
    /// "dropped without arming" cleanup path against `None`.
    inner: Option<PendingSessionInner>,
}

struct PendingSessionInner {
    uid: String,
    workspace_id: String,
    session_type: String,
    managed_by_uid: Option<String>,
    task_id: Option<String>,
    transcript_path: Option<String>,
    /// Sub-2b-3 review-fix #1: cap-inheritance fields carried
    /// from `SpawnParams` through `arm_reaper` onto the
    /// `DaemonSession`. `mcp_start_session` reads them off the
    /// caller's session and wraps the subtask child's argv with
    /// systemd-run so the cap is inherited.
    memory_cap_soft_bytes: Option<u64>,
    memory_cap_hard_bytes: Option<u64>,
    cgroup_prefix: Option<PathBuf>,
    /// Initial PTY size carried from `SpawnParams` through
    /// `arm_reaper` onto the `DaemonSession`'s `last_cols`/
    /// `last_rows` so the spawn-time width is the inheritance
    /// baseline until the first attach Resize frame updates it.
    cols: u16,
    rows: u16,
    /// 10d-2c-1 review round-5 (F1): carried through from
    /// SpawnParams onto the final DaemonSession.
    workflow_run_id: Option<String>,
    workflow_role: Option<String>,
    /// Phase-1 continuous wire field, carried SpawnParams →
    /// DaemonSession alongside the workflow tags.
    continuous_task_id: Option<String>,
    /// Global-perms grant, carried SpawnParams → DaemonSession.
    global_perms: bool,
    /// Sub-2b-1 review-r#2 #1: shared activity cell threaded
    /// from `PendingSession::spawn` → `arm_reaper` →
    /// `DaemonSession`. The fanout already holds an Arc clone
    /// for the output side; this is the same Arc, kept around
    /// so the `DaemonSession` lands with the matching cell on
    /// arm-time.
    last_activity_at: SharedLastActivity,
    title: String,
    fanout: Arc<PtyByteFanout>,
    pid: libc::pid_t,
    pidfd: OwnedFd,
    writer: SessionWriter,
    reader_handle: JoinHandle<()>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    /// Owned `Child` handle. [`arm_reaper`](PendingSession::arm_reaper)
    /// moves this into the reaper thread closure. [`Drop`] uses it
    /// (or rather the pidfd + waitpid pair) to clean up if the
    /// caller bails before arming.
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Kill-log directory (`~/.cm/memory_kills` in production)
    /// the reaper consults via
    /// [`crate::reaper::build_last_exit_since`] when the child
    /// exits. `None` skips the memory-cap probe — the LastExit
    /// then carries `memory_cap_kill: false`.
    kills_dir: Option<PathBuf>,
    /// Per-spawn baseline offset captured by
    /// [`crate::reaper::capture_baseline_for_spawn`]. Combined
    /// with `kills_dir` to scope the memory-cap probe to records
    /// landed during *this* spawn (not stale ones from a previous
    /// incarnation of the same uid).
    kills_baseline: u64,
}

impl PendingSession {
    /// Spawned child's OS PID. Used by `start_session` for
    /// /proc-based cgroup discovery (slice 10d watcher-fix #1
    /// "never trust a path that wasn't read from /proc"). The
    /// PID is valid from spawn through either `arm_reaper`
    /// (which consumes the inner state) or `Drop` (which uses
    /// it to issue the cleanup pidfd-SIGKILL via `inner`).
    ///
    /// Panics if called after `arm_reaper` has taken `inner` —
    /// a programmer error, since the PID becomes meaningless
    /// once it's owned by the live `DaemonSession`.
    pub fn pid(&self) -> libc::pid_t {
        self.inner
            .as_ref()
            .map(|i| i.pid)
            .expect("pid() called on already-armed PendingSession")
    }

    /// **Phase 1** of session spawn. Opens a PTY pair, execs the
    /// requested command against the slave, opens a pidfd bound to
    /// the child, spawns the reader thread that drains the master
    /// into the fanout. Does NOT spawn the reaper thread.
    ///
    /// The returned `PendingSession` is in an "armed but
    /// unsupervised" state: the child is alive, PTY traffic flows
    /// into the fanout, but no one is calling `waitpid` yet. The
    /// caller MUST either invoke [`arm_reaper`](Self::arm_reaper)
    /// promptly (which transfers the child into the reaper thread)
    /// or drop the `PendingSession` — `Drop` issues
    /// `SIGKILL` via pidfd and then calls `waitpid` synchronously
    /// to reap the zombie.
    ///
    /// Errors propagate from `portable-pty`'s `openpty` /
    /// `spawn_command` / `try_clone_reader` / `take_writer` and
    /// from the reader-thread spawn. Each failure path explicitly
    /// tears down the child via the still-owned Child handle
    /// (before pidfd exists) or the pidfd + manual waitpid (after).
    pub fn spawn(params: SpawnParams) -> anyhow::Result<Self> {
        use portable_pty::{native_pty_system, CommandBuilder, PtySize};

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| anyhow::anyhow!("openpty: {}", e))?;

        let mut cmd = CommandBuilder::new(&params.shell);
        cmd.args(&params.args);
        for (k, v) in &params.env {
            cmd.env(k, v);
        }
        // Defense-in-depth for daemon-scoped auth tokens. A
        // daemon-spawned agent must NOT inherit either of these:
        //   - `CM_OPERATOR_TOKEN`
        //     (`cm_daemon::control::operator::ENV_VAR`): forging
        //     `Caller::Operator` would bypass descendant-scoped
        //     Session-caller auth on the local Unix socket.
        //   - `CM_DAEMON_TOKEN`
        //     (`cm_daemon::control::tls::ENV_VAR`, slice 12h):
        //     the token authenticates ANY peer to the TLS-TCP
        //     listener. An agent that inherited it could dial
        //     the TLS port (locally, or — in a misconfigured-
        //     firewall scenario — from outside the host) and
        //     issue operator-level RPCs from a session-scoped
        //     process. Closing this gap is part of 12h landing
        //     safely; the attack surface didn't exist before
        //     the listener did.
        //
        // portable-pty's `CommandBuilder::new` is not documented
        // as env-clearing by default, so explicitly override
        // each var with an empty value. Both consumers treat
        // empty-string as "unset" — see
        // `operator::init_from_env` and the TLS listener's
        // `MissingDaemonToken` refuse-to-bind gate. A descendant
        // that re-reads the empty var hits the same path as if
        // the var were unset.
        cmd.env("CM_OPERATOR_TOKEN", "");
        cmd.env("CM_DAEMON_TOKEN", "");
        // P-1: workflow context vars are PARTICIPANT-ONLY. A genuine workflow
        // participant gets `CM_WORKFLOW_RUN_ID` / `CM_ROLE` via `params.env`
        // (set in the loop above). Every OTHER daemon-spawned session must NOT
        // carry them — and crucially must not INHERIT a stale value from the
        // daemon process's own env (e.g. a `cm-daemon` launched from inside a
        // workflow participant's shell, or a test runner that is itself a
        // workflow session). Without this strip such a non-participant child
        // would advertise a run it isn't part of. `env_remove` (not `env(k,"")`)
        // so the key is truly absent, matching the participant-only contract.
        // Mirrors the operator/daemon-token scrub directly above.
        for key in ["CM_WORKFLOW_RUN_ID", "CM_ROLE"] {
            if !params.env.contains_key(key) {
                cmd.env_remove(key);
            }
        }
        if let Some(wd) = &params.working_dir {
            cmd.cwd(wd);
        }

        // ALWAYS-ON claude folder pre-trust. A daemon-spawned interactive
        // `claude` launched in an untrusted directory wedges forever at the
        // "Do you trust the files in this folder?" dialog — state stays
        // `pending`, no transcript is written, and any headless workflow
        // stalls at iteration 1 with no human to answer. `claude` records
        // trust per-path in `~/.claude.json`; pre-seeding it before exec
        // closes the wedge. This is the SINGLE PTY-spawn choke point, so it
        // covers both `mcp_start_session` spawns and workflow-participant
        // fresh-spawns. Best-effort + gated to `claude` only (codex/bash write
        // nothing); see `crate::claude_trust`.
        crate::claude_trust::maybe_pretrust_for_spawn(
            &params.shell,
            &params.args,
            params.working_dir.as_deref(),
        );

        // Capture the memory-kill-log baseline BEFORE the child
        // starts. Order matters (slice-10c-e-2 review-4 fix): if
        // we baseline *after* spawn_command, a child that OOMs
        // immediately can land a kill record in the log between
        // spawn and the baseline read — making the baseline
        // include the record, and the reaper's later
        // `build_last_exit_since` treats it as below-the-baseline
        // (stale) → false negative on memory_cap_kill. Best-
        // effort: capture errors fall back to 0 (every record
        // counts as past-baseline), preferring false positives
        // over false negatives on the named acceptance criterion.
        let kills_baseline = match &params.kills_dir {
            Some(dir) => {
                match crate::reaper::capture_baseline_for_spawn(dir, &params.uid) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!(
                            "cm-daemon: kills_dir baseline capture failed for {}: {} (using 0 — may produce stale-record false positives)",
                            params.uid, e,
                        );
                        0
                    }
                }
            }
            None => 0,
        };

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow::anyhow!("spawn_command: {}", e))?;
        // Close the slave handle in the parent: only the child needs
        // it. Dropping releases the fd so the eventual child exit
        // closes the PTY cleanly.
        drop(pair.slave);

        // ============================================================
        // Phase-1 critical section. Each fallible step either
        // succeeds and proceeds, OR tears down the child (via the
        // still-owned Child handle before pidfd exists, via pidfd +
        // manual waitpid after) before returning. NO `?` here — all
        // recovery is explicit.
        // ============================================================

        let pid: libc::pid_t = match child.process_id() {
            Some(p) => p as libc::pid_t,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!(
                    "portable-pty did not yield a child PID"
                ));
            }
        };

        let pidfd = match open_pidfd(pid) {
            Ok(fd) => fd,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("pidfd_open: {}", e));
            }
        };

        let reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("try_clone_reader: {}", e));
            }
        };
        let writer: SessionWriter = match pair.master.take_writer() {
            Ok(w) => Arc::new(Mutex::new(w)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("take_writer: {}", e));
            }
        };

        // Sub-2b-1 review-r#2 #1: shared activity-timestamp cell.
        // Cloned three ways: into the fanout (stamped on every
        // output push by the reader thread), onto the `DaemonSession`
        // (stamped by `send_input` and read by
        // `resolve_authorized_session`), and into the `PendingSessionInner`
        // which carries it from spawn through `arm_reaper`.
        //
        // Sub-2b-1 review-r#4 #1: stamped to `Some(spawn_time)`
        // here — NOT left as `None`. Pre-r#4 a fresh session
        // (no I/O yet) had `last_activity_at: None`, which the
        // idle predicate mapped to "infinitely long ago" →
        // `idle: true` immediately. Agents polling
        // `wait_for_session_idle` would observe idle=true on a
        // session that hadn't even attached its transcript yet
        // and return prematurely. Stamping spawn-time means
        // the session needs `IDLE_THRESHOLD` of post-spawn
        // quiet before idle flips — matching the TUI's behavior
        // where `SessionStatus::Running` is the spawn default
        // and only the next event-drain tick flips to Idle if
        // `wakeup_times` is empty.
        let last_activity_at: SharedLastActivity =
            Arc::new(Mutex::new(Some(Instant::now())));
        let fanout = Arc::new(PtyByteFanout::with_activity_tracker(
            params.fanout_capacity,
            Some(Arc::clone(&last_activity_at)),
        ));
        let reader_fanout = Arc::clone(&fanout);
        let reader_handle = match std::thread::Builder::new()
            .name(format!("cm-session-{}-reader", params.uid))
            .spawn(move || {
                let mut reader = reader;
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            // EOF on the PTY master. Signal
                            // subscribers via close() so their
                            // `recv_timeout` returns Disconnected
                            // (slice-10c-c review fix).
                            reader_fanout.close();
                            return;
                        }
                        Ok(n) => reader_fanout.push(&buf[..n]),
                        Err(e)
                            if e.kind() == std::io::ErrorKind::Interrupted =>
                        {
                            continue;
                        }
                        Err(_) => {
                            reader_fanout.close();
                            return;
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("spawn reader thread: {}", e));
            }
        };

        // ============================================================
        // End phase-1. PendingSession now owns the live child
        // (Box<dyn Child + Send + Sync>) along with pidfd + PTY
        // master + reader thread. The caller MUST arm the reaper
        // (Phase 2) or drop — Drop here does kill+waitpid synchronously.
        // ============================================================

        Ok(PendingSession {
            inner: Some(PendingSessionInner {
                uid: params.uid,
                workspace_id: params.workspace_id,
                session_type: params.session_type,
                managed_by_uid: params.managed_by_uid,
                task_id: params.task_id,
                transcript_path: params.transcript_path,
                memory_cap_soft_bytes: params.memory_cap_soft_bytes,
                memory_cap_hard_bytes: params.memory_cap_hard_bytes,
                cgroup_prefix: params.cgroup_prefix,
                cols: params.cols,
                rows: params.rows,
                // 10d-2c-1 review round-5 (F1): workflow context
                // carried through to the final DaemonSession.
                workflow_run_id: params.workflow_run_id,
                workflow_role: params.workflow_role,
                continuous_task_id: params.continuous_task_id,
                global_perms: params.global_perms,
                last_activity_at,
                title: params.title,
                fanout,
                pid,
                pidfd,
                writer,
                reader_handle,
                master: pair.master,
                child,
                kills_dir: params.kills_dir,
                kills_baseline,
            }),
        })
    }

    /// Test helper / convenience accessor: the session uid this
    /// pending session was spawned with. Useful between phase-1 and
    /// arm_reaper for callers that want to log without unwrapping
    /// the inner.
    pub fn uid(&self) -> &str {
        &self.inner.as_ref().expect("PendingSession in valid state").uid
    }

    /// **Phase 2** of session spawn: spawn the per-session reaper
    /// thread and return a fully-armed [`DaemonSession`].
    ///
    /// `on_exit` is invoked by the reaper thread after `waitpid`
    /// returns. `start_session` passes a closure that removes the
    /// session from the daemon-state registry. See the
    /// [type docs](PendingSession) for the race-safety
    /// argument — the caller MUST hold the relevant state lock
    /// from before calling `arm_reaper` through the post-arm
    /// `insert` so a fast-exit child can't fire `on_exit` before
    /// the registry sees the session.
    pub fn arm_reaper(
        mut self,
        on_exit: Option<OnExitCallback>,
    ) -> anyhow::Result<DaemonSession> {
        let inner = self
            .inner
            .take()
            .expect("arm_reaper called on already-taken PendingSession");
        let PendingSessionInner {
            uid,
            workspace_id,
            session_type,
            managed_by_uid,
            task_id,
            transcript_path,
            memory_cap_soft_bytes,
            memory_cap_hard_bytes,
            cgroup_prefix,
            cols,
            rows,
            workflow_run_id,
            workflow_role,
            continuous_task_id,
            global_perms,
            last_activity_at,
            title,
            fanout,
            pid,
            pidfd,
            writer,
            reader_handle,
            master,
            child,
            kills_dir,
            kills_baseline,
        } = inner;

        let last_exit: SharedLastExit = std::sync::Arc::new(LastExitProbe::new(
            uid.clone(),
            kills_dir,
            kills_baseline,
        ));
        let last_exit_for_reaper = last_exit.clone();

        let (exit_tx, exit_rx) = mpsc::channel::<DaemonExitStatus>();
        let reaper_handle = match std::thread::Builder::new()
            .name(format!("cm-session-{}-reaper", uid))
            .spawn(move || {
                // Keep `child` in scope until after the waitpid
                // returns; explicit drop at end documents the
                // lifetime even though it's a no-op.
                let _child = child;
                let status = wait_for_child(pid);
                let _ = exit_tx.send(status.clone());

                // Cache ONLY the kernel-observable exit at
                // waitpid time. memory_cap_kill gets classified
                // lazily by `LastExitProbe::snapshot()` at
                // End-frame emission time — the slice-10c-e-2
                // review-6 fix that closes the race between the
                // cgroup-OOM writer and `waitpid`.
                last_exit_for_reaper.set_kernel(KernelExitStatus {
                    code: status.code,
                    // Slice 10d watcher-fix #1.5: signal is what
                    // distinguishes a kernel kill (signal-exit) from
                    // a clean exit-with-transient-soft-breach. The
                    // lazy `LastExitProbe::snapshot` reads both and
                    // joins with kill_status via `is_cap_kill`.
                    signal: status.signal,
                });

                if let Some(cb) = on_exit {
                    cb(&status);
                }
                drop(_child);
            }) {
            Ok(h) => h,
            Err(e) => {
                // The closure has consumed `child` (move into
                // closure happens before spawn's internal
                // allocation can fail). pidfd survives in this
                // scope; use it to kill, then waitpid to reap.
                let _ = send_sigkill_via_pidfd(&pidfd);
                let _ = wait_for_child(pid);
                return Err(anyhow::anyhow!("arm_reaper: spawn reaper thread: {}", e));
            }
        };

        Ok(DaemonSession {
            uid,
            title,
            workspace_id,
            session_type,
            managed_by_uid,
            task_id,
            transcript_path,
            memory_cap_soft_bytes,
            memory_cap_hard_bytes,
            cgroup_prefix,
            last_cols: cols,
            last_rows: rows,
            // 10d-2c-1 review round-5 (F1): workflow context lands
            // on the final DaemonSession; auth via
            // `lookup_session_any` reads it from here.
            workflow_run_id,
            workflow_role,
            continuous_task_id,
            global_perms,
            // Sub-2b-1 review-r#2 #2: generation starts at 0;
            // `session.set_transcript_path` increments on
            // path-change. Subscribed-from-spawn callers
            // observe generation=0 until a rebind happens.
            generation: 0,
            last_activity_at,
            fanout,
            pid,
            pidfd,
            writer,
            cached_exit: None,
            exit_rx,
            _reader_handle: reader_handle,
            _reaper_handle: reaper_handle,
            _master: master,
            last_exit,
            // Filled in by `start_session` after arm_reaper returns
            // when a memory cap was applied (slice 10d-memory-cap-
            // relocation review fix). The two-step assignment is
            // structurally important: arm_reaper is on the spawn
            // hot path and shouldn't care about the watcher's
            // existence, while `start_session` is where the
            // watcher's lifetime decisions are made.
            watcher_handle: None,
        })
    }
}

impl Drop for PendingSession {
    /// Cleanup for the "spawn but never arm" path. SIGKILLs the
    /// child via pidfd and reaps the zombie via `waitpid` so the
    /// dropped pending session leaves no kernel-side residue. The
    /// reader thread terminates naturally when `master` drops
    /// (closes the master fd, reader sees EOF, calls
    /// `fanout.close()`, returns).
    ///
    /// If `inner` is `None`, `arm_reaper` has already consumed it
    /// — drop is a no-op in that case.
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            // pidfd may have already been moved if arm_reaper's
            // error path took it, but in that case inner is None;
            // we only get here when arm_reaper wasn't called.
            let _ = send_sigkill_via_pidfd(&inner.pidfd);
            let _ = wait_for_child(inner.pid);
        }
    }
}

/// Sub-2b-1 review-r#3 #2: cloned-Arc pair used by every input
/// path that writes to a daemon-owned PTY. Centralizes the
/// "write bytes + stamp activity" invariant in one place so
/// future input paths (Resize-with-input, paste throttling,
/// etc.) cannot accidentally skip the stamp the way
/// `stream::handle_input_frame` did pre-fix.
///
/// **Why cloned Arcs, not `&DaemonSession`**: a method on
/// `&DaemonSession` would force callers to hold the
/// `DaemonState` mutex across `write_all`. The slice
/// 10c-e-3b-fix3 deadlock fix ruled that out — a backpressured
/// PTY would freeze every other daemon RPC. The handle pattern
/// keeps the lock-then-clone-then-drop-then-write shape
/// load-bearing.
pub struct InputHandle {
    writer: SessionWriter,
    last_activity_at: SharedLastActivity,
}

impl InputHandle {
    /// Write `bytes` to the PTY then stamp activity. Stamp is
    /// AFTER the write so a failed write doesn't lie about
    /// "session was active." Lock order: writer mutex first
    /// (blocking I/O), then activity mutex (fast cell swap).
    /// Both are released by the time this returns.
    pub fn write_and_stamp(&self, bytes: &[u8]) -> std::io::Result<()> {
        {
            let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
            w.write_all(bytes)?;
            w.flush()?;
        }
        {
            let mut slot = self
                .last_activity_at
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *slot = Some(Instant::now());
        }
        Ok(())
    }
}

impl DaemonSession {
    /// Sub-2b-1 review-r#3 #2: clone the writer + activity
    /// Arcs into an `InputHandle`. Caller pattern: lock state
    /// → call this on the `&DaemonSession` → drop state lock
    /// → call `handle.write_and_stamp(bytes)`. The handle
    /// outlives any state-mutex critical section so blocking
    /// PTY writes don't stall other daemon RPCs.
    pub fn input_handle(&self) -> InputHandle {
        InputHandle {
            writer: Arc::clone(&self.writer),
            last_activity_at: Arc::clone(&self.last_activity_at),
        }
    }

    /// Convenience: one-shot spawn that combines [`PendingSession::spawn`]
    /// + [`PendingSession::arm_reaper(None)`]. Used by tests and by
    /// other callers that don't need a registry-cleanup callback.
    ///
    /// **Race warning**: callers that insert the returned session
    /// into a shared registry must NOT use this form if they need
    /// the reaper to clean up the registry on exit. Use the explicit
    /// two-phase form for that — see
    /// `crate::control::methods::start_session` for the canonical
    /// pattern.
    pub fn spawn(params: SpawnParams) -> anyhow::Result<Self> {
        let pending = PendingSession::spawn(params)?;
        pending.arm_reaper(None)
    }

    /// Write `bytes` to the child's PTY master (i.e. the child's
    /// stdin).
    ///
    /// ## Lock contract (slice 10c-e-3b-fix3)
    ///
    /// Pre-fix3 this took `&mut self`, which forced callers to
    /// hold the global `DaemonState` mutex across the blocking
    /// PTY write. With the writer split into `Arc<Mutex<>>` the
    /// signature is now `&self` — but production hot paths
    /// **must still** clone the writer Arc OUT of state and
    /// drop the state lock before calling this (or just call
    /// `lock` on the cloned Arc directly). The convenience form
    /// here calls `self.writer.lock()` internally; if a caller
    /// has the state lock held while calling this method, the
    /// state lock is technically held across the PTY write
    /// because the caller's `&self` reference comes from a
    /// `state.sessions.get()` lookup.
    ///
    /// **Test code is fine** — tests don't have RPC contention.
    /// **Production code routes through the explicit clone-out
    /// pattern** in `control/stream.rs` and `control/methods.rs`.
    pub fn send_input(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut w = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        w.write_all(bytes)?;
        w.flush()
    }

    /// Update the PTY's window size. Slice 10c-e-2 review-fix
    /// addition for the inbound side of `handle_attach_stream`:
    /// `Resize` frames from the client carry new dimensions which
    /// the daemon forwards to the kernel PTY via portable-pty's
    /// `resize` (TIOCSWINSZ under the hood).
    ///
    /// Returns the underlying portable-pty error wrapped as
    /// `std::io::Error`. Failures are rare — typically only if
    /// the master fd has been closed under us (child gone +
    /// reader-thread already shut down). Best-effort at the
    /// caller; a missed resize is a cosmetic glitch, not a
    /// correctness bug.
    pub fn resize(&mut self, cols: u16, rows: u16) -> std::io::Result<()> {
        use portable_pty::PtySize;
        // Track the latest requested size so `mcp_start_session`
        // can hand a child PTY the caller's *current* width. Stamp
        // the intent even if the TIOCSWINSZ below errors (rare;
        // only when the master fd is gone) — a child inheriting
        // the last commanded size is still closer than 80×24.
        self.last_cols = cols;
        self.last_rows = rows;
        self._master
            .resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| std::io::Error::other(format!("PTY resize: {}", e)))
    }

    /// Terminate the child via `SIGKILL` through the pidfd.
    ///
    /// Routing the signal through the pidfd (rather than
    /// `libc::kill(self.pid, ...)`) is PID-reuse-safe: the kernel
    /// resolves the signal target through the pidfd's bound task
    /// identity, not by name-lookup on `pid`. The slice-10c-b
    /// reviewer flagged the prior `libc::kill(self.pid, ...)` as
    /// a foot-cannon — if the reaper's `waitpid` had already
    /// completed by the time Drop ran, the kernel could have
    /// recycled `pid` to an unrelated user-owned process and the
    /// SIGKILL would have landed there.
    ///
    /// Returns `Ok(())` whether the signal was delivered or the
    /// task has already exited (`ESRCH` on the pidfd path; same
    /// outcome — "the child is no longer running" — is achieved
    /// either way).
    pub fn kill(&mut self) -> std::io::Result<()> {
        send_sigkill_via_pidfd(&self.pidfd)
    }

    /// Non-blocking exit check. Returns `Some(DaemonExitStatus)`
    /// once the child has exited (the result is cached, so
    /// subsequent calls keep returning the same value), `None`
    /// while it's still running.
    ///
    /// Reads from the reaper's channel: if a status is queued,
    /// take it and cache. The reaper sends exactly once and then
    /// the channel becomes disconnected; both states are
    /// indistinguishable from the consumer's perspective once the
    /// status has been cached.
    pub fn try_wait(&mut self) -> Option<DaemonExitStatus> {
        if self.cached_exit.is_none() {
            if let Ok(status) = self.exit_rx.try_recv() {
                self.cached_exit = Some(status);
            }
        }
        self.cached_exit.clone()
    }
}

/// Open a `pidfd` bound to `pid` via `pidfd_open(2)`.
///
/// The returned [`OwnedFd`] is bound to the *specific task* `pid`
/// referred to at the moment the syscall returned — once the kernel
/// reaps that task, the fd refers to a zombie/exited identity, and
/// signal delivery via [`send_sigkill_via_pidfd`] returns `ESRCH`
/// instead of being misrouted to a recycled PID. This is the
/// PID-reuse-safe replacement for the legacy `libc::kill(pid, ...)`
/// call site (slice-10c-b review).
///
/// Linux 5.3+; this whole crate is Linux-only per
/// `doc/persistent-host-daemon.md`. `flags = 0` is the only
/// well-defined value today.
fn open_pidfd(pid: libc::pid_t) -> std::io::Result<OwnedFd> {
    let ret = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a non-negative fd; we own it.
    // Linux fds always fit in i32; the syscall return type is
    // c_long, so the cast narrows safely on this platform.
    let raw = ret as RawFd;
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Send `SIGKILL` to the task identified by `pidfd` via
/// `pidfd_send_signal(2)`. Returns `Ok(())` on successful delivery
/// AND on `ESRCH` (the underlying task has already exited — same
/// outcome from the caller's perspective).
///
/// Per the slice-10c-b review, this replaces `libc::kill(pid, ...)`
/// at every call site that would otherwise have run after the reaper
/// reaped the child. The kernel binds the signal target through the
/// pidfd's task identity, not via the now-possibly-recycled PID, so
/// no foot-cannon: an unrelated process that happens to own the
/// recycled PID slot won't receive our SIGKILL.
fn send_sigkill_via_pidfd(pidfd: &OwnedFd) -> std::io::Result<()> {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if ret == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        // Task already gone; same outcome as success.
        return Ok(());
    }
    Err(err)
}

/// Block on `waitpid(pid, 0)` and decode the C-style wait status
/// into a typed [`DaemonExitStatus`]. Used by the per-session reaper
/// thread.
fn wait_for_child(pid: libc::pid_t) -> DaemonExitStatus {
    let mut status: libc::c_int = 0;
    let ret = unsafe { libc::waitpid(pid, &mut status, 0) };
    if ret < 0 {
        // ECHILD here means the child was reaped by someone else
        // (very rare — would require a SIGCHLD-handler somewhere
        // we don't install). Surface "unknown" cleanly.
        return DaemonExitStatus { code: None, signal: None };
    }
    if libc::WIFEXITED(status) {
        DaemonExitStatus {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else if libc::WIFSIGNALED(status) {
        DaemonExitStatus {
            code: None,
            signal: Some(libc::WTERMSIG(status)),
        }
    } else {
        // Stopped / continued — we asked for 0 flags so this
        // shouldn't happen, but defend against unexpected kernel
        // behavior.
        DaemonExitStatus { code: None, signal: None }
    }
}

impl Drop for DaemonSession {
    /// Best-effort cleanup: SIGKILL the child if it's still
    /// running, via the pidfd. Two cases:
    ///
    /// **Case 1: child still alive.** The reaper thread is blocked
    /// on `waitpid`; our SIGKILL via the pidfd reaches the actual
    /// child (PID-reuse-safe), the kernel delivers `SIGCHLD`, the
    /// wait returns, the reaper sends the typed status (best-effort
    /// — we may have already dropped the receiver), and the reaper
    /// thread exits. No zombie.
    ///
    /// **Case 2: child already reaped.** The reaper has already
    /// returned from `waitpid`. The pidfd is bound to a now-gone
    /// task identity, so `pidfd_send_signal` returns `ESRCH` — we
    /// treat as success. **This is exactly the case the
    /// slice-10c-b reviewer flagged**: under the legacy
    /// `libc::kill(self.pid, ...)` code path, the PID could have
    /// been recycled by the kernel between the reaper's `waitpid`
    /// and Drop's signal, and the SIGKILL would have landed on an
    /// unrelated user-owned process. The pidfd closes that window.
    ///
    /// We don't join either the reader or reaper thread here.
    /// Joining could block Drop indefinitely if `read` / `waitpid`
    /// is slow to return. They terminate naturally:
    ///   - reader: when the PTY's read returns 0 / errors.
    ///   - reaper: when its `waitpid` returns.
    /// Both happen shortly after the kill — within milliseconds
    /// for the kernel-level paths.
    fn drop(&mut self) {
        let _ = self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// Wait briefly for an mpsc receiver to surface a value; helps
    /// the few multi-thread tests not flake.
    fn recv_with_timeout<T>(rx: &mpsc::Receiver<T>, dur: Duration) -> Option<T> {
        rx.recv_timeout(dur).ok()
    }

    #[test]
    fn subscribe_to_empty_fanout_yields_no_replay() {
        let fanout = PtyByteFanout::new(64);
        let rx = fanout.subscribe();
        // Channel is open but no item should be queued (no buffer
        // contents means no replay).
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn push_then_subscribe_replays_buffer() {
        let fanout = PtyByteFanout::new(64);
        fanout.push(b"hello ");
        fanout.push(b"world");
        let rx = fanout.subscribe();
        let replay = rx.try_recv().expect("replay chunk");
        assert_eq!(replay, b"hello world");
        // No further items queued.
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn subscribe_then_push_broadcasts() {
        let fanout = PtyByteFanout::new(64);
        let rx = fanout.subscribe();
        fanout.push(b"after sub");
        let chunk = rx.try_recv().expect("broadcast chunk");
        assert_eq!(chunk, b"after sub");
    }

    #[test]
    fn multiple_subscribers_all_see_pushed_chunks() {
        let fanout = PtyByteFanout::new(64);
        let rx1 = fanout.subscribe();
        let rx2 = fanout.subscribe();
        fanout.push(b"shared");
        assert_eq!(rx1.try_recv().unwrap(), b"shared");
        assert_eq!(rx2.try_recv().unwrap(), b"shared");
    }

    #[test]
    fn subscribers_after_push_get_replay_not_future_only() {
        let fanout = PtyByteFanout::new(64);
        let rx1 = fanout.subscribe();
        fanout.push(b"chunk-a");
        // rx1 sees the live push.
        assert_eq!(rx1.try_recv().unwrap(), b"chunk-a");

        // A new subscriber should see the replay (chunk-a is in the
        // buffer) AND any subsequent pushes.
        let rx2 = fanout.subscribe();
        let replay = rx2.try_recv().expect("replay");
        assert_eq!(replay, b"chunk-a");

        fanout.push(b"chunk-b");
        assert_eq!(rx1.try_recv().unwrap(), b"chunk-b");
        assert_eq!(rx2.try_recv().unwrap(), b"chunk-b");
    }

    #[test]
    fn ring_buffer_fifo_evicts_oldest_at_capacity() {
        let fanout = PtyByteFanout::new(5);
        fanout.push(b"abc");
        fanout.push(b"de");
        // Buffer full at 5 bytes.
        assert_eq!(fanout.buffered_len(), 5);

        // Push 2 more — drops "ab" from the front, keeps "cde" + "fg".
        fanout.push(b"fg");
        assert_eq!(fanout.buffered_len(), 5);

        let rx = fanout.subscribe();
        let replay = rx.try_recv().expect("replay");
        assert_eq!(replay, b"cdefg");
    }

    #[test]
    fn push_larger_than_capacity_keeps_only_tail() {
        let fanout = PtyByteFanout::new(4);
        fanout.push(b"abcdefghij"); // 10 bytes, cap 4
        assert_eq!(fanout.buffered_len(), 4);
        let rx = fanout.subscribe();
        let replay = rx.try_recv().expect("replay");
        assert_eq!(replay, b"ghij");
    }

    #[test]
    fn dropped_subscriber_does_not_break_push() {
        let fanout = PtyByteFanout::new(64);
        let rx_alive = fanout.subscribe();
        let rx_dropping = fanout.subscribe();
        drop(rx_dropping);

        // Before the push: both subscriber slots are still tracked
        // (mpsc::Sender has no `is_disconnected` on stable, so the
        // fanout can't proactively prune; the dead one is reaped on
        // the next push attempt).
        assert_eq!(fanout.subscriber_slot_count(), 2);

        // Push must not panic and must reap the dead subscriber via
        // the `retain` in `push`.
        fanout.push(b"survives");
        assert_eq!(rx_alive.try_recv().unwrap(), b"survives");
        assert_eq!(
            fanout.subscriber_slot_count(),
            1,
            "dead subscriber should have been reaped during push"
        );
    }

    #[test]
    fn empty_push_is_noop() {
        let fanout = PtyByteFanout::new(8);
        let rx = fanout.subscribe();
        fanout.push(b"");
        // No replay, no broadcast.
        assert!(rx.try_recv().is_err());
        assert_eq!(fanout.buffered_len(), 0);
    }

    #[test]
    fn concurrent_pushes_do_not_deadlock_or_corrupt_buffer() {
        // Eight threads pushing 100 chunks each. Total bytes well
        // under capacity so nothing evicts; we just verify the
        // primitive survives contention. Order across threads is
        // unspecified, but every chunk gets serialized into the
        // buffer atomically.
        let fanout = Arc::new(PtyByteFanout::new(1024 * 16));
        let mut handles = Vec::new();
        for tid in 0..8u8 {
            let f = fanout.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    f.push(&[tid]);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(fanout.buffered_len(), 8 * 100);
    }

    #[test]
    fn broadcast_order_matches_push_order() {
        let fanout = PtyByteFanout::new(64);
        let rx = fanout.subscribe();
        fanout.push(b"one");
        fanout.push(b"two");
        fanout.push(b"three");
        // No replay for an "already-subscribed-before-pushes" rx,
        // so the three chunks arrive in order.
        let t = Duration::from_secs(1);
        assert_eq!(recv_with_timeout(&rx, t).unwrap(), b"one");
        assert_eq!(recv_with_timeout(&rx, t).unwrap(), b"two");
        assert_eq!(recv_with_timeout(&rx, t).unwrap(), b"three");
    }

    // ===============================================================
    // PtyByteFanout::close (slice 10c-c review fix #1)
    //
    // The reader thread calls `close()` on EOF / unrecoverable
    // error. Subscribers must observe `Disconnected` on their next
    // recv so the stream loop's `End`-frame path fires.
    // ===============================================================

    #[test]
    fn close_makes_existing_subscribers_see_disconnected_on_next_recv() {
        // Named contract: an existing subscriber sees Disconnected
        // (not Timeout) once the fanout closes — that's what makes
        // the stream loop send `End` and return.
        let fanout = PtyByteFanout::new(64);
        let rx = fanout.subscribe();
        // Push something so the subscriber sees regular flow first.
        fanout.push(b"alive");
        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), b"alive");

        // Close: producer signals "no more data."
        fanout.close();

        // Next recv must yield Disconnected. With a small timeout
        // because Disconnected is detectable immediately — there's
        // no race with a pending value.
        let err = rx
            .recv_timeout(Duration::from_secs(1))
            .expect_err("must yield Disconnected after close");
        assert_eq!(err, mpsc::RecvTimeoutError::Disconnected);
    }

    #[test]
    fn close_is_idempotent() {
        // Calling close twice is a no-op the second time. Defends
        // against reader-thread error paths that might call it
        // both on EOF AND on a follow-up error.
        let fanout = PtyByteFanout::new(64);
        fanout.close();
        fanout.close();
        // Sanity: subscribers added after close still see
        // Disconnected.
        let rx = fanout.subscribe();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Disconnected),
        );
    }

    #[test]
    fn subscribe_after_close_yields_replay_then_disconnected() {
        // Late subscriber (e.g. attach.open arrives after the
        // child exited) gets the buffered tail AND learns it can
        // wrap up. The replay-first-then-end semantics is what
        // makes reconnect-and-replay work for the
        // ring-buffer-replay acceptance criterion.
        let fanout = PtyByteFanout::new(64);
        fanout.push(b"goodbye");
        fanout.close();

        let rx = fanout.subscribe();
        // First item: the buffered replay.
        let replay = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("replay must arrive");
        assert_eq!(replay, b"goodbye");

        // Then: Disconnected. No further items.
        let err = rx
            .recv_timeout(Duration::from_secs(1))
            .expect_err("after replay, must be Disconnected");
        assert_eq!(err, mpsc::RecvTimeoutError::Disconnected);
    }

    #[test]
    fn subscribe_after_close_with_empty_buffer_immediately_disconnected() {
        // No replay, no senders. The receiver is born ready to
        // surface Disconnected.
        let fanout = PtyByteFanout::new(64);
        fanout.close();
        let rx = fanout.subscribe();
        let err = rx
            .recv_timeout(Duration::from_millis(50))
            .expect_err("empty-buffer-after-close must be Disconnected");
        assert_eq!(err, mpsc::RecvTimeoutError::Disconnected);
    }

    // ===============================================================
    // PtyByteFanout::snapshot_since (slice 10c-d / read_session_output)
    // ===============================================================

    #[test]
    fn snapshot_since_none_returns_full_ring_with_correct_cursor() {
        let fanout = PtyByteFanout::new(64);
        fanout.push(b"hello ");
        fanout.push(b"world");
        let snap = fanout.snapshot_since(None);
        assert_eq!(snap.bytes, b"hello world");
        assert_eq!(snap.start_offset, 0, "no eviction yet");
        assert_eq!(
            snap.cursor, 11,
            "cursor advances to bytes_written (6 + 5 = 11)",
        );
        assert!(!snap.evicted_since_cursor);
        assert!(!snap.closed);
    }

    #[test]
    fn snapshot_since_up_to_date_cursor_returns_empty() {
        let fanout = PtyByteFanout::new(64);
        fanout.push(b"first");
        let first = fanout.snapshot_since(None);
        // Re-snap with the cursor from the first call — no new bytes
        // pushed since.
        let second = fanout.snapshot_since(Some(first.cursor));
        assert_eq!(second.bytes, b"");
        assert_eq!(second.cursor, first.cursor);
        assert!(!second.evicted_since_cursor);
    }

    #[test]
    fn snapshot_since_within_ring_returns_only_new_bytes() {
        let fanout = PtyByteFanout::new(64);
        fanout.push(b"older");
        let snap1 = fanout.snapshot_since(None);
        assert_eq!(snap1.cursor, 5);

        fanout.push(b"newer-bytes");
        let snap2 = fanout.snapshot_since(Some(snap1.cursor));
        assert_eq!(snap2.bytes, b"newer-bytes");
        assert_eq!(snap2.start_offset, 5);
        assert_eq!(snap2.cursor, 16);
        assert!(!snap2.evicted_since_cursor);
    }

    #[test]
    fn snapshot_since_evicted_cursor_returns_ring_with_eviction_flag() {
        // Named contract: caller's cursor refers to bytes that have
        // been evicted from the ring. Snapshot returns the current
        // ring (the best we can offer) AND signals
        // `evicted_since_cursor = true` so the caller can warn.
        let fanout = PtyByteFanout::new(4);
        fanout.push(b"ab"); // bytes_written = 2
        let snap1 = fanout.snapshot_since(None);
        assert_eq!(snap1.cursor, 2);

        // Push enough to evict "ab".
        fanout.push(b"cdefg"); // bytes_written = 7; ring holds "defg"
        // Caller still has cursor=2 from snap1. "cd" were evicted
        // (offsets 2 and 3 are no longer in the ring; ring start
        // is offset 3 because bytes_written - buf_len = 7 - 4 = 3).
        let snap2 = fanout.snapshot_since(Some(snap1.cursor));
        // Ring contains "defg" (last 4 bytes of "abcdefg").
        assert_eq!(snap2.bytes, b"defg");
        assert_eq!(snap2.start_offset, 3, "ring starts at offset 3 (=7-4)");
        assert_eq!(snap2.cursor, 7);
        assert!(
            snap2.evicted_since_cursor,
            "must signal eviction: cursor 2 is below ring start 3",
        );
    }

    #[test]
    fn snapshot_since_reports_closed_state() {
        // Closed flag plumbs through. Useful for MCP callers that
        // need to know when to stop polling.
        let fanout = PtyByteFanout::new(64);
        fanout.push(b"final");
        fanout.close();
        let snap = fanout.snapshot_since(None);
        assert_eq!(snap.bytes, b"final");
        assert!(snap.closed);
    }

    #[test]
    fn snapshot_since_cursor_at_buf_start_returns_full_ring() {
        // Boundary case: cursor exactly equals buf_start (the
        // earliest non-evicted byte). Should return the full ring,
        // no eviction flag.
        let fanout = PtyByteFanout::new(4);
        fanout.push(b"abcdefg"); // ring = "defg", buf_start = 3
        let snap = fanout.snapshot_since(Some(3));
        assert_eq!(snap.bytes, b"defg");
        assert_eq!(snap.start_offset, 3);
        assert!(
            !snap.evicted_since_cursor,
            "cursor exactly at buf_start is NOT eviction",
        );
    }

    #[test]
    fn snapshot_since_cursor_ahead_of_writer_returns_empty() {
        // Pathological / stale-cursor case: caller's cursor is
        // somehow ahead of bytes_written (impossible without
        // memory corruption, but defend the contract). Return
        // empty + current cursor.
        let fanout = PtyByteFanout::new(64);
        fanout.push(b"only-5"); // bytes_written = 6
        let snap = fanout.snapshot_since(Some(999));
        assert_eq!(snap.bytes, b"");
        assert_eq!(snap.cursor, 6);
        assert!(!snap.evicted_since_cursor);
    }

    #[test]
    fn close_after_child_exit_fires_via_reader_thread() {
        // Named regression: previously the reader thread would
        // just return on EOF without signalling the fanout, so
        // attach streams would hang. The fix has the reader call
        // `close()` on EOF — verifiable by subscribing to a real
        // session's fanout and observing Disconnected after the
        // child exits.
        //
        // `/bin/true` exits immediately, so we can:
        //   1. Spawn the session.
        //   2. Subscribe.
        //   3. Wait for try_wait to surface the exit (reaper has
        //      observed exit; reader thread is racing to see EOF).
        //   4. Within a short bound, the subscriber sees
        //      Disconnected.
        let params = SpawnParams::new("ts-eof", "eof-test", "/bin/true");
        let mut session = DaemonSession::spawn(params).expect("spawn /bin/true");
        let rx = session.fanout.subscribe();
        let _ = await_exit(&mut session, "/bin/true close-on-eof");

        // The reader thread might still be racing to observe EOF
        // after the child exits; give it a bounded window. The
        // typical close path takes a few ms once the kernel
        // closes the master fd.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(_chunk) => {
                    // Late replay / final bytes are fine; keep
                    // draining until we see Disconnected.
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() >= deadline {
                        panic!(
                            "reader thread did not close fanout within 3s of child exit"
                        );
                    }
                }
            }
        }
    }

    // ===============================================================
    // DaemonSession::spawn (slice 10c-a)
    // ===============================================================
    //
    // Each test spawns a real child process through the daemon's PTY
    // primitive. Tests use `/bin/echo`, `/bin/cat`, `/bin/sleep` — all
    // POSIX standards present on every Linux box.
    //
    // Note on PTY behavior: lines written by the child go through the
    // PTY line discipline, which translates LF → CR LF on output. Our
    // assertions read the fanout and look for the *content* substring
    // (e.g. b"hello") rather than the exact terminator, so the line-
    // discipline rewrite doesn't make the tests fragile.

    fn read_until<F: Fn(&[u8]) -> bool>(
        rx: &mpsc::Receiver<Vec<u8>>,
        deadline: std::time::Instant,
        done: F,
    ) -> Vec<u8> {
        let mut accumulated = Vec::new();
        while std::time::Instant::now() < deadline {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(chunk) => {
                    accumulated.extend_from_slice(&chunk);
                    if done(&accumulated) {
                        return accumulated;
                    }
                }
                Err(_) => break,
            }
        }
        accumulated
    }

    /// Spin-wait for `session.try_wait()` to return `Some` within
    /// the deadline. Returns the exit status, or panics with a
    /// descriptive message.
    fn await_exit(session: &mut DaemonSession, label: &str) -> DaemonExitStatus {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Some(status) = session.try_wait() {
                return status;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("{}: child never exited within 3s", label);
    }

    #[test]
    fn spawn_echoes_argument_into_fanout() {
        // The simplest end-to-end: `/bin/echo hello` writes "hello\r\n"
        // to its stdout (PTY-translated), the reader thread pushes it
        // to the fanout, our subscriber sees it.
        let mut params = SpawnParams::new("ts-echo", "echo-test", "/bin/echo");
        params.args = vec!["hello".into()];
        let mut session = DaemonSession::spawn(params).expect("spawn echo");
        let rx = session.fanout.subscribe();

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        let bytes = read_until(&rx, deadline, |acc| acc.windows(5).any(|w| w == b"hello"));
        assert!(
            bytes.windows(5).any(|w| w == b"hello"),
            "expected 'hello' substring in fanout output, got {:?}",
            String::from_utf8_lossy(&bytes),
        );

        let _status = await_exit(&mut session, "echo");
    }

    #[test]
    fn try_wait_reports_code_for_normal_exit() {
        // The named acceptance criterion's "exit code preserved" half:
        // /bin/sh -c 'exit 5' exits normally with code 5. The typed
        // DaemonExitStatus must carry { code: Some(5), signal: None }.
        let mut params = SpawnParams::new("ts-exit", "exit-test", "/bin/sh");
        params.args = vec!["-c".into(), "exit 5".into()];
        let mut session = DaemonSession::spawn(params).expect("spawn sh");
        let status = await_exit(&mut session, "sh -c 'exit 5'");
        assert_eq!(
            status,
            DaemonExitStatus {
                code: Some(5),
                signal: None,
            },
            "normal exit must surface code=Some(N), signal=None",
        );
    }

    #[test]
    fn try_wait_reports_signal_for_kill() {
        // The named acceptance criterion's "signal preserved" half:
        // /bin/sh -c 'kill -9 $$' self-sends SIGKILL. The typed
        // DaemonExitStatus must carry { code: None, signal: Some(9) }
        // — that's the discriminator the cap-kill detection relies
        // on (a cgroup OOM also delivers SIGKILL).
        let mut params = SpawnParams::new("ts-signal", "signal-test", "/bin/sh");
        params.args = vec!["-c".into(), "kill -9 $$".into()];
        let mut session = DaemonSession::spawn(params).expect("spawn sh");
        let status = await_exit(&mut session, "sh -c 'kill -9 $$'");
        assert_eq!(
            status,
            DaemonExitStatus {
                code: None,
                signal: Some(9),
            },
            "signal kill must surface code=None, signal=Some(9)",
        );
    }

    #[test]
    fn try_wait_is_idempotent_after_first_observation() {
        // Subsequent try_wait calls return the cached status, not
        // None. The reaper sends exactly once; without caching,
        // callers that re-poll after observing exit would flip
        // back to "still running" — a contract bug.
        let params = SpawnParams::new("ts-idem", "idem-test", "/bin/true");
        let mut session = DaemonSession::spawn(params).expect("spawn /bin/true");
        let first = await_exit(&mut session, "/bin/true (first)");
        // Drain a few times to confirm the cache holds.
        for _ in 0..5 {
            assert_eq!(session.try_wait(), Some(first.clone()));
        }
    }

    #[test]
    fn send_input_feeds_child_stdin_and_appears_in_fanout() {
        // `/bin/cat` echoes its stdin to its stdout. We write "hi\n"
        // via `send_input` and expect "hi" to come back through the
        // fanout. Then kill cat to terminate the session.
        let mut session = DaemonSession::spawn(SpawnParams::new(
            "ts-cat",
            "cat-test",
            "/bin/cat",
        ))
        .expect("spawn cat");
        let rx = session.fanout.subscribe();

        session.send_input(b"hi\n").expect("send_input");

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        let bytes =
            read_until(&rx, deadline, |acc| acc.windows(2).any(|w| w == b"hi"));
        assert!(
            bytes.windows(2).any(|w| w == b"hi"),
            "expected 'hi' echo in fanout output, got {:?}",
            String::from_utf8_lossy(&bytes),
        );

        // cat keeps running; kill cleanly.
        session.kill().expect("kill cat");
    }

    #[test]
    fn kill_terminates_child_and_try_wait_reports_exit() {
        // `/bin/sleep 30` would otherwise hold the PTY for half a
        // minute. kill it immediately; try_wait should report the
        // SIGKILL exit within a short window.
        let mut params = SpawnParams::new("ts-sleep", "sleep-test", "/bin/sleep");
        params.args = vec!["30".into()];
        let mut session = DaemonSession::spawn(params).expect("spawn sleep");

        // Before kill: still running.
        assert_eq!(
            session.try_wait(),
            None,
            "try_wait must report None for a running child",
        );

        session.kill().expect("kill sleep");
        let status = await_exit(&mut session, "sleep");
        // SIGKILL from kill(2) → signal=Some(9), code=None.
        assert_eq!(
            status,
            DaemonExitStatus {
                code: None,
                signal: Some(9),
            },
            "kill() must surface signal=Some(9), code=None",
        );
    }

    #[test]
    fn spawn_uses_custom_fanout_capacity_for_eviction() {
        // The fanout-capacity parameter on SpawnParams plumbs through
        // to the underlying PtyByteFanout. We verify by spawning a
        // command that emits more bytes than the cap and confirming
        // the buffered_len observed by a late subscriber doesn't
        // exceed the cap.
        let mut params = SpawnParams::new("ts-cap", "cap-test", "/bin/echo");
        // ~5 bytes of output ("a x b\r\n" or similar); cap at 4 forces
        // eviction down to the last 4 bytes of buffered content.
        params.args = vec!["a x b".into()];
        params.fanout_capacity = 4;
        let session = DaemonSession::spawn(params).expect("spawn echo for cap");

        // Wait for the child to write + exit before subscribing so
        // the replay carries the full (truncated) content.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            // Hammer the fanout's buffered_len through subscribe →
            // we can't peek without a subscribe, but a transient
            // subscribe doesn't disturb others.
            let probe = session.fanout.subscribe();
            // Drain whatever replayed.
            while probe.try_recv().is_ok() {}
            std::thread::sleep(std::time::Duration::from_millis(20));
            if session.fanout.buffered_len() == 4 {
                return;
            }
        }
        // Allow up to 4 (the cap) — anything ≤ 4 means eviction
        // worked, even if the child wrote slightly fewer bytes than
        // expected on the particular PTY's line discipline.
        assert!(
            session.fanout.buffered_len() <= 4,
            "fanout buffered_len {} exceeds custom capacity 4",
            session.fanout.buffered_len(),
        );
    }

    #[test]
    fn drop_kills_and_reaps_child_no_zombie() {
        // The named regression (slice-10c-a review #2): a dropped
        // DaemonSession must not leave a zombie in the process
        // table. The reaper thread blocks on waitpid; Drop sends
        // SIGKILL; the reaper wakes up, consumes the status, and
        // exits — leaving no zombie.
        //
        // Test verifies by capturing the PID, dropping the
        // session, then polling libc::waitpid(pid, &status, WNOHANG)
        // until it returns -1 with ECHILD ("no child with that
        // pid to wait for" — i.e. already reaped).
        let mut params = SpawnParams::new("ts-drop", "drop-test", "/bin/sleep");
        params.args = vec!["30".into()];
        let session = DaemonSession::spawn(params).expect("spawn sleep");
        let pid = session.pid;
        drop(session);

        // Poll for the PID to be fully reaped.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let mut status: libc::c_int = 0;
            let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if ret == -1 {
                let errno = std::io::Error::last_os_error();
                if errno.raw_os_error() == Some(libc::ECHILD) {
                    // The reaper got there first — the canonical
                    // post-Drop state.
                    return;
                }
                panic!("unexpected waitpid error: {}", errno);
            }
            if ret == pid {
                // We just reaped it (the reaper hadn't gotten
                // there yet). Also fine — the child is no longer
                // a zombie in the process table.
                return;
            }
            // ret == 0 means the child is still around as a
            // zombie waiting to be reaped. Sleep briefly and retry.
            if std::time::Instant::now() >= deadline {
                panic!(
                    "child pid {} not reaped within 3s after Drop — zombie leak",
                    pid
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn drop_does_not_panic_and_pty_pool_does_not_exhaust() {
        // Smoke check on the Drop path: spawn, drop, then spawn
        // again immediately to confirm the daemon's PTY pool
        // hasn't been exhausted by leaked fds.
        let mut params = SpawnParams::new("ts-drop-smoke", "drop-smoke", "/bin/sleep");
        params.args = vec!["30".into()];
        let session = DaemonSession::spawn(params).expect("spawn sleep");
        drop(session);

        // Spawn again to confirm the daemon's PTY pool isn't
        // exhausted (kernels have generous ptmx limits, but a
        // leak across many tests would be visible).
        let params2 = SpawnParams::new("ts-drop-2", "drop-test-2", "/bin/true");
        let mut session2 = DaemonSession::spawn(params2).expect("spawn /bin/true");
        let _status = await_exit(&mut session2, "post-drop /bin/true");
    }

    #[test]
    fn drop_after_reaper_observed_is_safe_no_zombie() {
        // The named regression from the slice-10c-b review: prior
        // implementation called `libc::kill(self.pid, SIGKILL)` in
        // Drop, which would misroute the signal if the kernel had
        // recycled the PID between the reaper's `waitpid` and
        // Drop's signal. The pidfd-based kill closes that window:
        // signalling via the pidfd checks the bound task identity,
        // returns ESRCH for a reaped task, and we treat ESRCH as
        // success.
        //
        // Test: spawn `/bin/true` (exits in ms), wait for the
        // reaper to surface the exit status (proving `waitpid` has
        // completed — the kernel slot is freed and the PID is
        // eligible for reuse), then drop the session and verify:
        //   1. Drop doesn't panic.
        //   2. `libc::waitpid(pid, WNOHANG)` returns -1/ECHILD —
        //      the child has been reaped (which the reaper
        //      already did) and Drop didn't accidentally
        //      resurrect a zombie.
        let params = SpawnParams::new("ts-post-reap", "post-reap-test", "/bin/true");
        let mut session = DaemonSession::spawn(params).expect("spawn /bin/true");
        let pid = session.pid;
        // Wait for the reaper to consume the exit status.
        let status = await_exit(&mut session, "/bin/true post-reap setup");
        assert_eq!(
            status,
            DaemonExitStatus {
                code: Some(0),
                signal: None,
            },
            "sanity: /bin/true must exit clean before we exercise the post-reap Drop",
        );

        // From here, the reaper has waitpid'd; the kernel may
        // recycle pid at any moment. Drop's pidfd kill must NOT
        // misroute to a recycled process — and ECHILD is the
        // proof, since after waitpid the kernel records "no
        // child with that pid" for this parent.
        drop(session);

        // Verify Drop didn't somehow leave a zombie. Since the
        // reaper already reaped, waitpid must return ECHILD
        // immediately.
        let mut status: libc::c_int = 0;
        let ret = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        assert_eq!(
            ret, -1,
            "waitpid after reaper-reaped Drop must return -1 (ECHILD), got {}",
            ret
        );
        let errno = std::io::Error::last_os_error();
        assert_eq!(
            errno.raw_os_error(),
            Some(libc::ECHILD),
            "expected ECHILD post-reaper-Drop, got {}",
            errno,
        );
    }

    #[test]
    fn kill_signals_running_child_via_pidfd() {
        // The named acceptance criterion from the reviewer:
        // "Spawn /bin/sleep 30, kill via the public kill() method,
        // observe try_wait returns Some within bound — proves
        // pidfd-based signalling works on a live child."
        //
        // Complementary to kill_terminates_child_and_try_wait_
        // reports_exit (which also tests the kill path); this
        // test is explicitly framed around the pidfd contract.
        let mut params = SpawnParams::new("ts-pidfd-kill", "pidfd-test", "/bin/sleep");
        params.args = vec!["30".into()];
        let mut session = DaemonSession::spawn(params).expect("spawn sleep");
        // Pre-condition: child running, try_wait sees None.
        assert_eq!(session.try_wait(), None);
        // Public kill() routes through pidfd_send_signal under
        // the hood — see DaemonSession::kill / send_sigkill_via_pidfd.
        session.kill().expect("kill via pidfd");
        // Within a short window, the reaper observes SIGKILL and
        // try_wait surfaces { code: None, signal: Some(9) }.
        let status = await_exit(&mut session, "post-pidfd-kill");
        assert_eq!(
            status,
            DaemonExitStatus {
                code: None,
                signal: Some(9),
            },
            "pidfd SIGKILL must produce signal=Some(9), code=None",
        );
    }

    #[test]
    fn kill_on_already_exited_child_returns_ok_via_pidfd_esrch() {
        // The named contract from the kill() doc: ESRCH path
        // returns Ok(()) ("the child is no longer running" is
        // achieved either way). Spawn /bin/true, wait for exit,
        // then call kill() — pidfd_send_signal must return ESRCH
        // which we map to Ok(()).
        let params = SpawnParams::new("ts-esrch", "esrch-test", "/bin/true");
        let mut session = DaemonSession::spawn(params).expect("spawn /bin/true");
        let _status = await_exit(&mut session, "/bin/true esrch setup");
        // Reaper has waitpid'd; the kernel-bound task identity is
        // gone. Calling kill() must surface as Ok, not Err.
        session
            .kill()
            .expect("kill on post-reaped session must be Ok (ESRCH mapped to success)");
        // And again — idempotent.
        session
            .kill()
            .expect("kill is idempotent on post-reaped session");
    }

    // ============================================================
    // Per-spawn baseline ordering (slice-10c-e-2 review-4 fix).
    //
    // The named race: child OOMs immediately, kill record lands in
    // the log BEFORE we capture the baseline → record is below
    // baseline → `build_last_exit_since` treats as historical →
    // false negative on memory_cap_kill.
    //
    // Fix: capture baseline BEFORE `child.spawn()`. This test
    // simulates the race by manually injecting a kill record
    // AFTER `PendingSession::spawn` returns and BEFORE
    // `arm_reaper` runs the reaper. With the pre-spawn baseline,
    // the injected record is past-baseline and surfaces as
    // memory_cap_kill: true.
    // ============================================================

    #[test]
    fn pre_spawn_baseline_catches_record_landed_during_spawn() {
        // The race-simulation test: with the fix, a record
        // written between `PendingSession::spawn` and
        // `arm_reaper` is past-baseline. Without the fix
        // (post-spawn baseline), the same record would be
        // below-baseline.
        let tmp = tempfile::TempDir::new().unwrap();
        let kills_dir = tmp.path().to_path_buf();
        let uid = "ts-race-fix";

        // Use /bin/sh self-SIGKILL so the child exits via signal
        // (the reaper records exit_code=None) once the reaper
        // waitpids. The test is about the kill-log probe path —
        // we manually inject the OOM record and use a fast-exit
        // child as the trigger.
        let mut params = SpawnParams::new(uid, "race-sh", "/bin/sh");
        params.args = vec!["-c".into(), "kill -9 $$".into()];
        params.kills_dir = Some(kills_dir.clone());

        // PendingSession::spawn captures the baseline (now BEFORE
        // child.spawn). Without the fix, this happened AFTER
        // spawn — letting a fast-exit child's kill record sneak in.
        let pending = PendingSession::spawn(params).expect("spawn");

        // Simulate the race: write a kill record to the log
        // BETWEEN spawn and arm_reaper. Conceptually this is the
        // cgroup OOM watcher firing on the just-spawned process,
        // which under the prior bug would land below baseline.
        let log_path = kills_dir.join(format!("{}.jsonl", uid));
        let record = format!(
            r#"{{"ts":1700000000,"session_uid":"{}","pid":12345,"comm":"sh","argc":3,"argv_sha256_prefix":"deadbeef","rss_kb":1024,"soft_cap_bytes":104857600,"hard_cap_bytes":209715200}}
"#,
            uid
        );
        use std::io::Write as IoWrite;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("open kill log");
        f.write_all(record.as_bytes()).expect("inject record");
        drop(f);

        // Arm the reaper and let the child exit. Slice-10c-e-2
        // review-6 refactor: `memory_cap_kill` is classified
        // lazily at `snapshot()` time — the reaper only caches
        // kernel exit. We wait for the reaper to populate kernel
        // exit, then call snapshot() which scans the kill log
        // NOW (post-spawn record IS past-baseline → true).
        let mut session = pending.arm_reaper(None).expect("arm");
        let _status = await_exit(&mut session, "race-fix");

        // Wait for the reaper to populate the kernel exit slot.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !session.last_exit.kernel_set() {
            if std::time::Instant::now() >= deadline {
                panic!("reaper did not populate kernel exit within 3s");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let (_code, memory_cap_kill) = session.last_exit.snapshot();
        assert!(
            memory_cap_kill,
            "record injected between spawn and arm_reaper must be \
             past-baseline → memory_cap_kill: true via lazy classification."
        );
    }

    #[test]
    fn pre_existing_records_remain_below_baseline_after_spawn() {
        // Companion to the race test: a record written BEFORE
        // PendingSession::spawn must be below the baseline (the
        // baseline captures the post-stale file size). The
        // child exits clean — memory_cap_kill must stay false.
        // This is the stale-isolation contract from slice 10c-b.
        let tmp = tempfile::TempDir::new().unwrap();
        let kills_dir = tmp.path().to_path_buf();
        let uid = "ts-stale-pre";

        // Pre-populate the log with a stale record.
        crate::path::ensure_dot_cm_subdir(&kills_dir).expect("mkdir");
        let log_path = kills_dir.join(format!("{}.jsonl", uid));
        let stale = format!(
            r#"{{"ts":1500000000,"session_uid":"{}","pid":99999,"comm":"old","argc":1,"argv_sha256_prefix":"feedface","rss_kb":2048,"soft_cap_bytes":104857600,"hard_cap_bytes":209715200}}
"#,
            uid
        );
        std::fs::write(&log_path, &stale).expect("seed stale");

        // Spawn — baseline captures the post-stale file size.
        // Child exits cleanly (no fresh record).
        let mut params = SpawnParams::new(uid, "stale-true", "/bin/true");
        params.kills_dir = Some(kills_dir.clone());
        let pending = PendingSession::spawn(params).expect("spawn");
        let mut session = pending.arm_reaper(None).expect("arm");
        let _status = await_exit(&mut session, "stale-pre");

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !session.last_exit.kernel_set() {
            if std::time::Instant::now() >= deadline {
                panic!("reaper did not populate kernel exit within 3s");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (code, memory_cap_kill) = session.last_exit.snapshot();
        assert_eq!(code, Some(0), "/bin/true must exit clean (code = Some(0))");
        assert!(
            !memory_cap_kill,
            "stale pre-baseline record must NOT flag memory_cap_kill on a clean exit"
        );
    }

    /// 12h reviewer round 3: PTY env scrub must cover BOTH
    /// `CM_OPERATOR_TOKEN` (pre-12h) and `CM_DAEMON_TOKEN`
    /// (new in 12h). Pre-fix the latter was inherited from the
    /// daemon's systemd-injected env into every spawned agent,
    /// letting a session-scoped agent dial the TLS port and
    /// issue operator-level RPCs.
    ///
    /// Test sets both vars in the test binary's process env,
    /// spawns a `bash -c 'env > <file>; exit 0'` child via
    /// `DaemonSession::spawn`, and asserts the dumped env
    /// either lacks the keys or carries them with empty values
    /// (`cmd.env(KEY, "")` semantics — both consumers
    /// `init_from_env` / `MissingDaemonToken` treat empty as
    /// "unset").
    #[test]
    fn spawn_scrubs_daemon_and_operator_tokens_from_child_env() {
        let _g = crate::test_support::env_lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let env_dump = dir.path().join("env-dump.txt");

        // Set BOTH vars on the test process so they'd otherwise
        // be inherited by `CommandBuilder::new`.
        std::env::set_var("CM_OPERATOR_TOKEN", "operator-leak-canary");
        std::env::set_var("CM_DAEMON_TOKEN", "daemon-leak-canary");

        let mut params = SpawnParams::new(
            "ts-scrub",
            "scrub-test",
            "/bin/bash",
        );
        params.args = vec![
            "-c".into(),
            format!(
                "env > {}; exit 0",
                env_dump.display(),
            ),
        ];
        let mut session =
            DaemonSession::spawn(params).expect("spawn bash");
        let _ = await_exit(&mut session, "bash env-dump");

        // Restore env BEFORE asserting so a panic doesn't leak
        // canary tokens into adjacent tests sharing env_lock.
        std::env::remove_var("CM_OPERATOR_TOKEN");
        std::env::remove_var("CM_DAEMON_TOKEN");

        let dumped =
            std::fs::read_to_string(&env_dump).expect("read env dump");
        let env: std::collections::HashMap<&str, &str> = dumped
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();

        // Critical: neither canary value must reach the child.
        // The scrub uses `cmd.env(KEY, "")` so the key MAY be
        // present with empty value; what matters is that the
        // INHERITED canary value does not propagate.
        assert_ne!(
            env.get("CM_OPERATOR_TOKEN").copied(),
            Some("operator-leak-canary"),
            "CM_OPERATOR_TOKEN canary value MUST NOT leak into \
             the child (would let the agent forge \
             Caller::Operator on the Unix socket); got dump:\n{}",
            dumped,
        );
        assert_ne!(
            env.get("CM_DAEMON_TOKEN").copied(),
            Some("daemon-leak-canary"),
            "CM_DAEMON_TOKEN canary value MUST NOT leak into \
             the child (would let the agent authenticate to the \
             TLS-TCP listener and issue operator-level RPCs); \
             got dump:\n{}",
            dumped,
        );
        // Defensive: if the key IS present, it MUST be empty.
        // Tightens the assertion above against a future change
        // that introduces a different non-canary value.
        if let Some(v) = env.get("CM_OPERATOR_TOKEN").copied() {
            assert!(
                v.is_empty(),
                "if CM_OPERATOR_TOKEN is present it must be \
                 empty; got {:?}",
                v,
            );
        }
        if let Some(v) = env.get("CM_DAEMON_TOKEN").copied() {
            assert!(
                v.is_empty(),
                "if CM_DAEMON_TOKEN is present it must be \
                 empty; got {:?}",
                v,
            );
        }
    }
}
