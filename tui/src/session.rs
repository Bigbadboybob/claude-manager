use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::tty;
use alacritty_terminal::Term;

use std::sync::mpsc;

use crate::memory_cap::MemoryCap;

/// Proxy that forwards alacritty terminal events to a channel.
#[derive(Clone)]
pub struct EventProxy {
    tx: mpsc::Sender<TermEvent>,
}

impl EventProxy {
    /// Constructor used by both `Session::new` (local PTY) and
    /// `ClientSession::new` (daemon-attached PTY) so they build
    /// equivalent EventProxies without exposing the private `tx`
    /// field across modules.
    pub fn new(tx: mpsc::Sender<TermEvent>) -> Self {
        Self { tx }
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.tx.send(event);
    }
}

/// A terminal session wrapping alacritty's Term + PTY + EventLoop.
pub struct Session {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub sender: EventLoopSender,
    /// Direct fd to PTY master for low-latency input writes. `Some`
    /// for local-PTY sessions (built via [`Session::new`]); `None`
    /// for daemon-attached sessions (built via
    /// [`Session::new_attached`], slice 10c-e-3) where there is no
    /// local PTY fd — writes go through `sender` instead. The
    /// fallback path in [`Session::write`] is what makes the daemon
    /// branch a drop-in at every call site.
    pty_writer: Option<File>,
    pub event_rx: mpsc::Receiver<TermEvent>,
    pub title: String,
    pub exited: bool,
    /// Rolling window of recent Wakeup timestamps for burst detection.
    pub wakeup_times: Vec<Instant>,
    /// Set when the session is wrapped in a per-session systemd scope
    /// for memory capping. The watcher thread (if any) reads from
    /// `cgroup_path`. None for uncapped sessions (tests, infra,
    /// preflight-failed environments).
    pub memory_cap: Option<MemoryCap>,
    pub cgroup_path: Option<PathBuf>,
    /// Daemon-side session uid for daemon-attached sessions (slice
    /// 10c-e-3). `None` for local-PTY sessions. Used by the
    /// close path (A-w) to issue `kill_session` against the daemon
    /// — without this, dropping the session would just close the
    /// attach socket while the daemon's session keeps running.
    pub daemon_session_uid: Option<String>,
    /// Latched `memory_cap_kill` flag for daemon-attached
    /// sessions (slice 10c-e-3b-fix4b). The attach-stream reader
    /// half stores `true` here when the daemon's `End` frame
    /// carried `memory_cap_kill: true`. The exit handler at
    /// `app.rs::drain_pty_events` reads-and-clears it via
    /// `swap(false, SeqCst)` after observing
    /// `TermEvent::Exit`/`ChildExit` and dispatches the cap-kill
    /// toast — the same toast the local-spawn watcher fires when
    /// its `MemoryKillEvent::Killed` arrives. Single source of
    /// truth: `memory_cap_kill` (daemon path) and
    /// `MemoryKillEvent::Killed` (local path) both end at the
    /// same cap-kill rendering.
    ///
    /// `None` for local-PTY sessions — those get their cap-kill
    /// signal via the watcher's `MemoryKillEvent` channel
    /// instead.
    pub daemon_memory_cap_kill: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Latched `transport_eof` flag for daemon-attached sessions
    /// (auto-reconnect). The attach-stream reader half stores
    /// `true` here when the daemon's attach socket EOF'd WITHOUT an
    /// `End` frame — the transport died (an SSH-tunnel drop, or the
    /// local daemon re-exec'ing across a deploy —
    /// DESIGN_SEAMLESS_RESTART phase 5) while the daemon-side PTY +
    /// workflow keep running. The exit handler at
    /// `app.rs::drain_pty_events` reads-and-clears it via
    /// `swap(false, SeqCst)` after observing
    /// `TermEvent::Exit`/`ChildExit`; a `true` here means "this
    /// session's I/O stream died, but the daemon session is alive —
    /// mark it reconnecting and requeue for reattach" instead of
    /// tearing the session slot down. Host-agnostic: local rows are
    /// daemon-attached too, and a genuine child exit always arrives
    /// as a structured `End` frame (flag stays false).
    ///
    /// `None` for truly-local-PTY sessions (built via
    /// [`Session::new`], e.g. the gcloud backtest watch pane) — a
    /// local PTY has no detachable transport, so its exits are
    /// always genuine child exits.
    pub daemon_transport_eof: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// S5: a dup of the remote attach socket fd, for the main-loop HUP
    /// watchdog (`App::requeue_stale_generation_remote_sessions`). `Some` only
    /// for daemon-attached sessions; `None` for local-PTY sessions. See
    /// [`Self::attach_socket_hung_up`].
    pub attach_hup_fd: Option<std::os::fd::OwnedFd>,
}

impl Session {
    /// Spawn a new terminal session running the given shell command.
    /// When `memory_cap` is `Some`, the spawn is rewritten to run inside
    /// a `systemd-run --user --scope` transient unit with the configured
    /// `MemoryHigh`/`MemoryMax` / `MemorySwapMax=0` properties. The
    /// caller is responsible for only passing `Some` when preflight
    /// succeeded.
    pub fn new(
        shell: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        working_dir: Option<PathBuf>,
        env: HashMap<String, String>,
        memory_cap: Option<MemoryCap>,
    ) -> anyhow::Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        let event_proxy = EventProxy::new(event_tx);

        // Enable Kitty keyboard protocol tracking so alacritty actually
        // records `\x1b[>Nu` push/pop sequences from agents like codex —
        // otherwise `term.mode()` never reflects DISAMBIGUATE_ESC_CODES
        // and our Enter encoding logic in app.rs has nothing to react to.
        let mut config = TermConfig::default();
        config.kitty_keyboard = true;
        // Alacritty defaults to 10_000 lines of scrollback per Term. With
        // many sessions open this dominates RAM in practice (each line
        // holds `num_cols` cells, ~28 bytes each → ~56 MB per session at
        // 200 cols when fully populated). We keep transcripts on disk for
        // long history, so a much smaller in-memory window is fine here.
        config.scrolling_history = 1500;

        let size = TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        };
        let term = Term::new(config, &size, event_proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        // When capped, rewrite (shell, args) to run inside a
        // `systemd-run --user --scope` transient unit. The caller has
        // already verified preflight; `Some(MemoryCap)` here is an
        // unconditional "wrap me".
        //
        // Slice 10d watcher-fix #1: the predicted path returned by
        // `wrap_with_systemd_run` is no longer authoritative —
        // post-spawn we discover the actual cgroup from
        // `/proc/<pid>/cgroup`. The prediction stays useful as
        // the `--unit=` flag and for unit-testing the wrapper, but
        // the path that gets stashed on `Session.cgroup_path` (and
        // handed to the watcher) is read from /proc. "Never trust
        // a path that wasn't read from /proc" applies to local-spawn
        // and daemon-spawn equally.
        let (final_shell, final_args, _predicted_cgroup_path) =
            wrap_with_systemd_run(shell, args, &memory_cap);

        let pty_config = tty::Options {
            shell: Some(tty::Shell::new(final_shell, final_args)),
            working_directory: working_dir,
            drain_on_exit: true,
            env,
        };

        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };

        // Setup terminal environment (TERM, COLORTERM).
        tty::setup_env();

        let pty = tty::new(&pty_config, window_size, 0)?;

        // Dup the PTY master fd so we can write input directly,
        // bypassing the event loop channel for lower latency.
        let pty_writer = pty.file().try_clone()?;

        // Slice 10d watcher-fix #1: discover the cgroup path by
        // reading `/proc/<pid>/cgroup` rather than trusting the
        // path `wrap_with_systemd_run` predicted. The kernel writes
        // the cgroup of the live child PID; systemd-run will have
        // moved the child into a `cm-sess-*.scope` cgroup if the
        // scope setup completed, and the discovery helper verifies
        // the basename matches that pattern. If it doesn't
        // (systemd-run failed mid-setup, scope ended up elsewhere),
        // bail before constructing the session — same recovery
        // shape as the slice 10c-e-3b-fix4a verification arm.
        //
        // Dropping `pty` here closes the master fd, which SIGHUPs
        // the child; alacritty's Pty Drop also `wait()`s the child.
        // No process leak.
        let cgroup_path: Option<PathBuf> = if memory_cap.is_some() {
            let child_pid = pty.child().id();
            match cm_daemon::path::discover_session_cgroup_path(
                child_pid,
                Duration::from_millis(500),
            ) {
                Ok(p) => Some(p),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "cgroup discovery from /proc/{}/cgroup failed: {} \
                         (systemd-run did not produce the expected \
                         cm-sess-*.scope hierarchy — likely a transient \
                         systemd error or unit-name collision)",
                        child_pid,
                        e
                    ));
                }
            }
        } else {
            None
        };

        // Belt-and-suspenders verification: cgroup is active
        // (cgroup.procs non-empty). discover already verified the
        // pattern and path existence; the active check confirms
        // the kernel has actually moved the child into this cgroup.
        // Same semantics as the daemon side for parity.
        if let Some(ref discovered) = cgroup_path {
            if !wait_for_cgroup_active(discovered, Duration::from_millis(500)) {
                return Err(anyhow::anyhow!(
                    "discovered cgroup {} has no procs within 500ms — \
                     systemd-run scope is half-created. Refusing to return \
                     a Session over a dead PTY child.",
                    discovered.display()
                ));
            }
        }

        let event_loop = EventLoop::new(
            term.clone(),
            event_proxy,
            pty,
            true,  // drain_on_exit
            false, // ref_test
        )?;

        let sender = event_loop.channel();

        // Spawn the PTY I/O thread.
        event_loop.spawn();

        Ok(Session {
            term,
            sender,
            pty_writer: Some(pty_writer),
            event_rx,
            title: format!("{} {}", shell, args.join(" ")),
            exited: false,
            wakeup_times: Vec::new(),
            memory_cap,
            cgroup_path,
            daemon_session_uid: None,
            // Local-spawn sessions get cap-kill signals via the
            // watcher's `MemoryKillEvent` channel, not this Arc.
            daemon_memory_cap_kill: None,
            // A local PTY has no detachable transport, so its exits
            // are always genuine child exits — never a reconnect.
            daemon_transport_eof: None,
            // Local PTY session — no attach socket to watch for HUP.
            attach_hup_fd: None,
        })
    }

    /// Build a `Session` backed by a daemon-attached PTY (slice
    /// 10c-e-3 opt-in path, gated on `CM_USE_DAEMON_SOCKET=1`).
    /// Runs the RPC dance via [`crate::client_session::ClientSession::new`]
    /// and wraps the result in a `Session` whose `pty_writer` is
    /// `None` (writes route through `sender`) and whose
    /// `daemon_session_uid` carries the daemon-minted uid so the
    /// close path can issue `kill_session` on teardown.
    ///
    /// `memory_cap` / `cgroup_path` are always `None` — the daemon
    /// owns memory-cap enforcement for daemon-attached sessions (a
    /// 10c-d concern); the TUI's local cap-watcher path is not
    /// exercised here.
    /// migrate-tui-local Issue 1: re-attach to an ALREADY-LIVE
    /// daemon-side session WITHOUT calling `start_session`.
    /// Manifest restore on TUI startup uses this when the daemon
    /// survived but the TUI is fresh — pre-fix the restore called
    /// `start_session` against an existing UID and the daemon's
    /// collision guard returned Conflict, dropping the session
    /// from `ws.sessions`. The caller has already established
    /// (e.g. via `rpc_list_session_uids`) that the UID is live.
    pub fn new_attached_existing(
        config: crate::client_session::ClientSessionConfig,
    ) -> anyhow::Result<Self> {
        let title_label = config.label.to_string();
        let cs = crate::client_session::ClientSession::attach_existing(config)?;
        let cgroup_path = cs.cgroup_path.as_deref().map(PathBuf::from);
        Ok(Session {
            term: cs.term,
            sender: cs.sender,
            pty_writer: None,
            event_rx: cs.event_rx,
            title: format!("{} (daemon:{})", title_label, cs.session_uid),
            exited: false,
            wakeup_times: Vec::new(),
            memory_cap: None,
            cgroup_path,
            daemon_session_uid: Some(cs.session_uid),
            daemon_memory_cap_kill: Some(cs.memory_cap_kill),
            // Route the latched transport-EOF flag up to the exit
            // handler so a tunnel death on this re-attached remote
            // session triggers reconnect, not teardown.
            daemon_transport_eof: Some(cs.transport_eof),
            attach_hup_fd: cs.hup_fd,
        })
    }

    pub fn new_attached(
        config: crate::client_session::ClientSessionConfig,
    ) -> anyhow::Result<Self> {
        let title_label = config.label.to_string();
        let cs = crate::client_session::ClientSession::new(config)?;
        // Mirror the daemon-echoed cgroup_path so the local
        // Session has parity with what `Session::new` would
        // produce for a memory-capped local spawn (slice
        // 10c-e-3b-fix2). The cgroup-OOM watcher relocation in
        // slice 10d-memory-cap-relocation will pick this up.
        // The daemon-echoed cgroup_path mirrors what `Session::new`
        // would produce for a memory-capped local spawn (slice
        // 10c-e-3b-fix2). Slice 10d-memory-cap-relocation moved
        // the cgroup-OOM watcher to the daemon side, so the TUI
        // doesn't spawn its own watcher for this path — the
        // daemon writes the kill-log records and the End frame
        // surfaces them via `daemon_memory_cap_kill`.
        let cgroup_path = cs.cgroup_path.as_deref().map(PathBuf::from);
        Ok(Session {
            term: cs.term,
            sender: cs.sender,
            pty_writer: None,
            event_rx: cs.event_rx,
            title: format!("{} (daemon:{})", title_label, cs.session_uid),
            exited: false,
            wakeup_times: Vec::new(),
            // Daemon-spawned sessions: the memory-cap watcher
            // runs on the daemon side (slice 10d-memory-cap-
            // relocation), writing into the SAME kill-log
            // (`~/.cm/memory_kills/<uid>.jsonl`) that local-spawn
            // sessions use. The End frame's `memory_cap_kill`
            // flag carries the daemon's attribution decision up
            // through `daemon_memory_cap_kill` to the exit
            // handler (fix4b consumer chain). The TUI's
            // `MemoryCap` struct is the local-spawn shape and
            // stays `None` here — there's no local watcher to
            // describe.
            memory_cap: None,
            cgroup_path,
            daemon_session_uid: Some(cs.session_uid),
            // Slice 10c-e-3b-fix4b: route the latched cap-kill
            // flag from the AttachedPty's reader half up to the
            // exit handler.
            daemon_memory_cap_kill: Some(cs.memory_cap_kill),
            // Remote auto-reconnect: route the latched transport-EOF
            // flag up too, so a dead attach stream on a remote
            // session reconnects instead of tearing down.
            daemon_transport_eof: Some(cs.transport_eof),
            attach_hup_fd: cs.hup_fd,
        })
    }

    /// S5: true if the attach socket has hung up (`POLLHUP`) or errored
    /// (`POLLERR`) — the tunnel died half-open. Non-blocking `poll(timeout=0)`
    /// on the dup'd fd; `false` for a local session (no `attach_hup_fd`) or a
    /// still-connected socket. This is the signal alacritty's EventLoop can't
    /// act on (it skips reading an interrupt event and spins), so the main-loop
    /// watchdog polls it here and tears the spinning loop down.
    pub fn attach_socket_hung_up(&self) -> bool {
        use std::os::fd::AsRawFd;
        let Some(fd) = self.attach_hup_fd.as_ref() else {
            return false;
        };
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            // POLLHUP/POLLERR are always reported in revents regardless of
            // events; request POLLIN so a readable-EOF also flags.
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd, timeout 0 (non-blocking). EINTR just
        // returns <=0 and we report "not hung up" this tick.
        let rc = unsafe { libc::poll(&mut pfd, 1, 0) };
        rc > 0 && (pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL)) != 0
    }

    /// S5: ask this session's alacritty EventLoop to shut down (breaks a loop
    /// that's spinning on a half-open socket's `POLLHUP`). Unlike dropping the
    /// `Session`, this leaves the slot intact so the reconnect can rebind it.
    /// Idempotent + best-effort (a gone receiver just fails the send).
    pub fn request_shutdown(&self) {
        let _ = self.sender.send(alacritty_terminal::event_loop::Msg::Shutdown);
    }

    /// Send raw bytes to the PTY (keyboard input).
    /// Writes directly to the PTY fd for minimal latency.
    ///
    /// The PTY fd is non-blocking (set by alacritty), so we loop on
    /// WouldBlock to avoid dropping data on large writes (e.g. pastes).
    /// The loop is bounded by `WRITE_DEADLINE` so a stalled PTY (child
    /// process not draining, wedged terminal, etc.) cannot freeze the TUI
    /// main loop indefinitely. On timeout, returns `TimedOut` with the
    /// number of bytes successfully delivered embedded in the message so
    /// callers can surface a useful status note.
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        const WRITE_DEADLINE: Duration = Duration::from_millis(200);

        // Daemon-attached path (slice 10c-e-3): no local PTY fd.
        // Route through the EventLoop's input channel, which
        // serializes writes onto `StreamWriter` as `StreamKind::Input`
        // frames. The EventLoop's writable-readiness drain (Shape B
        // hooks in `AttachedPty`) handles backpressure.
        if self.pty_writer.is_none() {
            use std::borrow::Cow;
            return self
                .sender
                .send(Msg::Input(Cow::Owned(data.to_vec())))
                .map_err(|e| {
                    std::io::Error::other(format!(
                        "daemon-attached Session::write: channel send failed: {}",
                        e
                    ))
                });
        }
        let pty_writer = self.pty_writer.as_ref().expect("checked is_some above");

        let total = data.len();
        let mut pos = 0;
        let start = Instant::now();
        while pos < total {
            // &File implements Write, so write through the shared ref —
            // matches the prior `&self.pty_writer` shape.
            match (&*pty_writer).write(&data[pos..]) {
                // Ok(0) on a non-empty slice means the writer accepted no
                // bytes but didn't signal WouldBlock. Looping on it is a
                // busy-spin that bypasses the deadline check below, so bail
                // immediately with the same N/M shape as the timeout error.
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        format!(
                            "PTY write made no progress: {}/{} bytes delivered",
                            pos, total
                        ),
                    ));
                }
                Ok(n) => pos += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() >= WRITE_DEADLINE {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "PTY write timed out: {}/{} bytes delivered",
                                pos, total
                            ),
                        ));
                    }
                    std::thread::sleep(Duration::from_micros(100));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Notify the PTY of a terminal resize.
    pub fn resize(&self, cols: u16, rows: u16) {
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let _ = self.sender.send(Msg::Resize(window_size));
        self.term.lock().resize(TermSize {
            columns: cols as usize,
            screen_lines: rows as usize,
        });
    }
}

// Drop semantics: send `Msg::Shutdown` so alacritty's EventLoop
// thread exits. Without this the thread runs forever holding the
// PTY master fd, the child never sees EOF/SIGHUP, and every A-w
// leaks a claude process plus a "PTY" thread inside the TUI.
//
// The EventLoop owns the only path to the master fd: `pty: T` is
// `move`d into the spawned thread at `event_loop.rs:205-322`, and
// the thread only exits on `Msg::Shutdown` (`event_loop.rs:96`).
// `EventLoopSender` has no `Drop`, and the EventLoop holds its
// own internal `tx` clone, so simply dropping our sender does NOT
// close the channel — the thread keeps polling. `pty_writer` is
// a `try_clone()` dup of the master fd, so closing it alone is
// not sufficient either; both fds must drop before the child sees
// SIGHUP.
//
// Per-flavor behavior with Shutdown sent:
//   - **Local-PTY sessions** (`pty_writer = Some(File)`): EventLoop
//     drops its `pty` → master fd closes → child sees EOF on slave
//     reads → kernel sends SIGHUP → child exits. `pty_writer`
//     dropping in the same Drop closes the dup'd fd, so the SIGHUP
//     actually fires.
//   - **Daemon-attached sessions** (`daemon_session_uid = Some(...)`,
//     `pty_writer = None`): EventLoop exits, closing the attach
//     socket. The daemon's reader sees EOF and detaches; the
//     daemon's PTY child KEEPS RUNNING (the daemon owns its own
//     master fd separately). This preserves the reconnect property.
//
// Operator-driven teardown (A-w) still issues `kill_session` via
// `App::kill_daemon_session_if_attached` BEFORE Drop runs, so
// daemon-attached sessions go fully away on A-w. Drop's role here
// is the cleanup floor that runs no matter how the Session is
// removed.
//
// Best-effort: a send error means the EventLoop has already
// exited (e.g., child died naturally and the thread broke out of
// the poll loop at `event_loop.rs:269`); nothing to do.
//
// TODO(reconnect-slice): on TUI restart, `spawn_restored_session`
// should probe the daemon for a matching uid and attach instead
// of respawning.
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown);
    }
}

/// Simple dimensions struct implementing alacritty's Dimensions trait.
pub struct TermSize {
    pub columns: usize,
    pub screen_lines: usize,
}

impl alacritty_terminal::grid::Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Process-wide monotonic counter for systemd scope unit names.
/// Workflow `fresh`-context respawns reuse the same `session_uid`
/// while the old scope's cgroup may not have been GC'd yet by
/// systemd; deriving the unit name from `<uid>` alone collides on
/// the second spawn. Appending a generation suffix gives every
/// spawn a fresh unit name. The kill-log filename and CLAUDE.md
/// correlation key continue to use the stable `session_uid`, so
/// agents can still find their `~/.cm/memory_kills/$CM_TUI_SESSION_ID.jsonl`.
static SCOPE_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Per-TUI-run nonce mixed into every scope unit name. The
/// generation counter alone resets to 0 on each TUI start; a
/// previous run that died without cleanup (kill -9, crash, child
/// agent keeping the scope alive) can leave a stale
/// `cm-sess-<uid>-0.scope` registered with systemd. On the next TUI
/// start a restored session with the same `session_uid` would pick
/// `<uid>-0` again and collide. A run-nonce makes the namespace
/// distinct across processes:
///   `cm-sess-<uid>-<run-nonce>-<gen>`
/// PID alone is reusable across reboots; combining with sub-second
/// start time defangs even rapid same-PID restarts.
fn run_nonce() -> &'static str {
    static NONCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NONCE.get_or_init(|| {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{:x}{:x}", pid, nanos)
    })
}

/// Rewrite `(shell, args)` into `systemd-run --user --scope ...
/// -- shell args...` when a `MemoryCap` is provided, and compute the
/// predicted cgroup path. When `memory_cap` is `None`, returns inputs
/// unchanged with `cgroup_path = None`.
///
/// `pub(crate)` so slice 10c-e-3b can reuse this for the daemon
/// spawn path: the TUI side pre-wraps argv before sending it across
/// the wire so the daemon-spawned child runs under the same
/// systemd-run scope the local Session::new path produces. Element-
/// wise parity is the contract.
pub(crate) fn wrap_with_systemd_run(
    shell: &str,
    args: &[String],
    memory_cap: &Option<MemoryCap>,
) -> (String, Vec<String>, Option<PathBuf>) {
    let cap = match memory_cap {
        Some(c) => c,
        None => return (shell.to_string(), args.to_vec(), None),
    };

    let gen = SCOPE_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unit_name = format!("cm-sess-{}-{}-{}", cap.session_uid, run_nonce(), gen);
    let cgroup_path = cap.cgroup_prefix.join(format!("{}.scope", unit_name));

    let mut wrapped: Vec<String> = vec![
        "--user".into(),
        "--scope".into(),
        "--quiet".into(),
        format!("--unit={}", unit_name),
        "-p".into(),
        format!("MemoryHigh={}", cap.soft_bytes),
        "-p".into(),
        format!("MemoryMax={}", cap.hard_bytes),
        "-p".into(),
        "MemorySwapMax=0".into(),
        "--".into(),
        shell.to_string(),
    ];
    wrapped.extend(args.iter().cloned());

    ("systemd-run".to_string(), wrapped, Some(cgroup_path))
}

/// Wait up to `max_wait` for the cgroup at `path` to be both present
/// and *active* — `cgroup.procs` exists and contains at least one
/// numeric PID. A bare path-existence check would be satisfied by a
/// stale scope from a prior TUI run that didn't clean up (kill -9,
/// crash, lingering agent), and the watcher would then aim at a dead
/// scope. Requiring at least one PID inside proves *our* spawn made
/// it through systemd-run's exec into the cgroup. Combined with the
/// per-run nonce in the unit name, this makes cross-process scope
/// collisions both unlikely (different unit names) and detectable
/// (empty `cgroup.procs` ⇒ Err) if one ever slips through.
fn wait_for_cgroup_active(path: &Path, max_wait: Duration) -> bool {
    let deadline = Instant::now() + max_wait;
    let procs = path.join("cgroup.procs");
    loop {
        if let Ok(content) = std::fs::read_to_string(&procs) {
            if content
                .lines()
                .any(|l| !l.trim().is_empty() && l.trim().parse::<u32>().is_ok())
            {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_none_passes_through() {
        let (shell, args, path) = wrap_with_systemd_run("/bin/bash", &[], &None);
        assert_eq!(shell, "/bin/bash");
        assert!(args.is_empty());
        assert!(path.is_none());
    }

    #[test]
    fn wrap_some_rewrites() {
        let cap = MemoryCap {
            soft_bytes: 6 * 1024 * 1024 * 1024,
            hard_bytes: 10 * 1024 * 1024 * 1024,
            session_uid: "abc123".into(),
            cgroup_prefix: PathBuf::from("/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice"),
        };
        let (shell, args, path) =
            wrap_with_systemd_run("claude", &["--foo".into()], &Some(cap));
        assert_eq!(shell, "systemd-run");
        assert!(args.contains(&"--user".to_string()));
        assert!(args.contains(&"--scope".to_string()));
        // Unit name is `cm-sess-<uid>-<gen>`; we know the prefix.
        let unit_arg = args
            .iter()
            .find(|a| a.starts_with("--unit=cm-sess-abc123-"))
            .expect("unit arg with session_uid prefix");
        assert!(args.contains(&format!("MemoryHigh={}", 6u64 * 1024 * 1024 * 1024)));
        assert!(args.contains(&format!("MemoryMax={}", 10u64 * 1024 * 1024 * 1024)));
        assert!(args.contains(&"MemorySwapMax=0".to_string()));
        // Original program + args follow the `--`.
        let dash_dash = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash_dash + 1], "claude");
        assert_eq!(args[dash_dash + 2], "--foo");
        // cgroup_path matches the unit name in the args.
        let cgroup = path.expect("Some cgroup path when capped");
        let unit = unit_arg.strip_prefix("--unit=").unwrap();
        assert!(
            cgroup.ends_with(format!("{}.scope", unit)),
            "cgroup path {} should end with {}.scope",
            cgroup.display(),
            unit
        );
    }

    /// The bug this guards against: workflow `fresh`-context respawns
    /// reuse the same `session_uid` for back-to-back spawns. Before
    /// the generation suffix, both spawns produced the identical unit
    /// name `cm-sess-<uid>` and the second one collided in systemd
    /// with "Unit was already loaded" — silently. Each spawn must
    /// produce a distinct unit name (and matching cgroup path) even
    /// when the `MemoryCap.session_uid` is identical.
    #[test]
    fn wrap_consecutive_spawns_use_distinct_unit_names() {
        let cap = MemoryCap {
            soft_bytes: 1 << 30,
            hard_bytes: 2 << 30,
            session_uid: "stable-uid-xyz".into(),
            cgroup_prefix: PathBuf::from("/sys/fs/cgroup/x"),
        };
        let cap2 = cap.clone();
        let (_shell_a, args_a, path_a) = wrap_with_systemd_run("claude", &[], &Some(cap));
        let (_shell_b, args_b, path_b) = wrap_with_systemd_run("claude", &[], &Some(cap2));

        let unit_a = args_a
            .iter()
            .find(|a| a.starts_with("--unit="))
            .expect("unit arg")
            .clone();
        let unit_b = args_b
            .iter()
            .find(|a| a.starts_with("--unit="))
            .expect("unit arg")
            .clone();
        assert_ne!(
            unit_a, unit_b,
            "consecutive spawns must use distinct unit names — would collide in systemd"
        );
        assert_ne!(
            path_a, path_b,
            "cgroup paths must differ in lockstep with unit names"
        );
        // Both must still encode the stable session_uid (so agents can
        // find their kill log via $CM_TUI_SESSION_ID — that's keyed on
        // session_uid, not the unit name).
        assert!(unit_a.contains("stable-uid-xyz"));
        assert!(unit_b.contains("stable-uid-xyz"));
    }

    #[test]
    fn wait_for_cgroup_active_times_out_when_path_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("never-created");
        let t0 = Instant::now();
        let ok = wait_for_cgroup_active(&missing, Duration::from_millis(60));
        let elapsed = t0.elapsed();
        assert!(!ok, "non-existent cgroup must report not-active");
        assert!(
            elapsed >= Duration::from_millis(50),
            "should wait the full deadline; waited {:?}",
            elapsed
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "shouldn't massively overshoot deadline; waited {:?}",
            elapsed
        );
    }

    /// Models the exact stale-scope failure mode the readback exists
    /// to catch: a previous TUI exited without cleanup and left a
    /// `cm-sess-...scope` directory behind whose `cgroup.procs` is
    /// readable but empty (no process is keeping the scope alive).
    /// Bare path existence would have returned true and handed back
    /// a Session pointed at a dead scope; checking that
    /// `cgroup.procs` has at least one PID rejects this.
    #[test]
    fn wait_for_cgroup_active_rejects_empty_procs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope_dir = dir.path().join("stale.scope");
        std::fs::create_dir(&scope_dir).unwrap();
        std::fs::write(scope_dir.join("cgroup.procs"), b"").unwrap();
        let t0 = Instant::now();
        let ok = wait_for_cgroup_active(&scope_dir, Duration::from_millis(60));
        assert!(!ok, "empty cgroup.procs must report not-active");
        assert!(
            t0.elapsed() >= Duration::from_millis(50),
            "should wait the full deadline before giving up"
        );
    }

    #[test]
    fn wait_for_cgroup_active_returns_true_when_proc_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope_dir = dir.path().join("our.scope");
        std::fs::create_dir(&scope_dir).unwrap();
        std::fs::write(scope_dir.join("cgroup.procs"), b"12345\n").unwrap();
        let t0 = Instant::now();
        let ok = wait_for_cgroup_active(&scope_dir, Duration::from_millis(500));
        assert!(ok);
        // Fast path: returns on first probe.
        assert!(t0.elapsed() < Duration::from_millis(50));
    }

    /// Whitespace-only or non-numeric `cgroup.procs` content must
    /// not satisfy the active check. `\n` alone or a trailing newline
    /// after a real PID is fine; pure whitespace is not.
    #[test]
    fn wait_for_cgroup_active_rejects_whitespace_procs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope_dir = dir.path().join("whitespace.scope");
        std::fs::create_dir(&scope_dir).unwrap();
        std::fs::write(scope_dir.join("cgroup.procs"), b"\n\n  \n").unwrap();
        let ok = wait_for_cgroup_active(&scope_dir, Duration::from_millis(50));
        assert!(!ok);
    }

    /// Unit names must be unique across TUI runs as well as within
    /// one — the per-run nonce makes the namespace distinct between
    /// processes so a stale `cm-sess-<uid>-0.scope` left behind by a
    /// crashed previous run can't collide with the new run's
    /// generation 0.
    #[test]
    fn run_nonce_is_stable_within_run() {
        let a = run_nonce().to_string();
        let b = run_nonce().to_string();
        assert_eq!(a, b, "run_nonce() must return the same value within one process");
        assert!(!a.is_empty(), "run_nonce() must not be empty");
    }

    #[test]
    fn wrap_unit_name_includes_run_nonce() {
        let cap = MemoryCap {
            soft_bytes: 1 << 30,
            hard_bytes: 2 << 30,
            session_uid: "stable-uid-xyz".into(),
            cgroup_prefix: PathBuf::from("/sys/fs/cgroup/x"),
        };
        let (_shell, args, _path) = wrap_with_systemd_run("claude", &[], &Some(cap));
        let unit = args
            .iter()
            .find(|a| a.starts_with("--unit="))
            .expect("unit arg")
            .strip_prefix("--unit=")
            .unwrap()
            .to_string();
        // Format: cm-sess-<uid>-<run-nonce>-<gen>
        assert!(unit.starts_with("cm-sess-stable-uid-xyz-"));
        let suffix = unit.strip_prefix("cm-sess-stable-uid-xyz-").unwrap();
        // Suffix is `<run-nonce>-<gen>` — both segments non-empty.
        let parts: Vec<&str> = suffix.rsplitn(2, '-').collect();
        assert_eq!(parts.len(), 2, "expected `<run-nonce>-<gen>` in {}", suffix);
        let (gen_str, nonce) = (parts[0], parts[1]);
        assert!(gen_str.parse::<u64>().is_ok(), "gen must be numeric: {}", gen_str);
        assert!(!nonce.is_empty(), "nonce must be non-empty");
        assert_eq!(nonce, run_nonce(), "nonce in unit name must match run_nonce()");
    }
}
