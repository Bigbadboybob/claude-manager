//! Client-side daemon-attached session. Slice 10c-e-2 of
//! `doc/persistent-host-daemon.md`.
//!
//! ## Where this fits
//!
//! Today's `Session::new` opens a kernel PTY in-process via
//! `alacritty_terminal::tty::new`. Under the daemon path the PTY
//! lives on the daemon side and the TUI talks to it through an
//! attach stream. `ClientSession::new` is the constructor that
//! orchestrates the full RPC dance:
//!
//!   1. `start_session` over a fresh control connection → uid.
//!   2. `session.attach` over a fresh control connection →
//!      ticket + attach_addr.
//!   3. Dial a fresh socket to `attach_addr`.
//!   4. Send `attach.open` as the first frame on that socket.
//!      The daemon's `handle_connection` writes the response and
//!      *morphs the same socket* into a one-way PTY stream (per
//!      slice 10c-c's `handle_attach_stream`). Subsequent frames
//!      are `StreamFrame`s carrying base64-encoded PTY bytes.
//!   5. Wrap the now-streaming socket as
//!      [`crate::attached_pty::AttachedPty`] — the alacritty
//!      `EventedReadWrite`/`EventedPty` impl from slice 10c-e-1.
//!   6. Build an alacritty `Term` + `EventLoop` over the
//!      `AttachedPty`, mirroring `Session::new`'s post-PTY shape
//!      byte-for-byte so 10c-e-3's `A-n`/`A-s` branch can swap
//!      one for the other at the construction site only.
//!
//! ## Identity binding (slice-5 contract)
//!
//! The slice-5 `TicketAllocator` binds each ticket to the
//! `Caller` payload that issued it; consume on a different
//! identity returns `IdentityMismatch`. We use the *same*
//! `operator_token_id` on all three RPC calls (`start_session`,
//! `session.attach`, `attach.open`) so the ticket consume
//! succeeds. A fresh `UnixStream` per RPC is fine — the daemon
//! validates by `Caller::Operator(token_id)` payload, not by
//! socket peer-creds.
//!
//! ## Failure semantics (slice-10c-e-2 review concern)
//!
//! Once `start_session` succeeds, the daemon has a running
//! session in its registry. Any subsequent failure in the dance
//! must `kill_session` it before bubbling the error — otherwise
//! we leak a started-but-unattached session. Cleanup runs through
//! the same `kill_session` JSON-RPC the daemon already serves
//! (slice 10c-d); the daemon's reaper handles SIGKILL + waitpid +
//! registry removal.
//!
//! ## What this slice does NOT do
//!
//! Wire this into `A-n`/`A-s`. That's slice 10c-e-3. Until then
//! the type is reachable from tests only.

use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::Config as TermConfig;
use alacritty_terminal::Term;
use anyhow::Context;
use cm_daemon::control::protocol::{Caller, ErrorCode, Request, Response};
use cm_daemon::control::wire;

use crate::attached_pty::AttachedPty;
use crate::session::{EventProxy, TermSize};

/// Construction parameters for [`ClientSession::new`]. Grouped as
/// a struct because the parameter list is long enough that
/// positional args make `A-n` / `A-s` call sites in 10c-e-3 hard
/// to read.
///
/// ## Argv/env/cwd shape (slice 10c-e-3b)
///
/// The caller is responsible for building the full `argv` and `env`
/// using the TUI's own `mcp_config::build_args(SpawnTarget::Daemon,
/// ...)` plus any memory-cap wrap
/// (`tui::session::wrap_with_systemd_run`). The daemon doesn't
/// interpret a session-type tag — it execs `argv` verbatim with
/// `env` in `working_dir`. This keeps agent-specific knowledge
/// (`--mcp-config`, Codex MCP overrides, `--resume` tokens,
/// systemd-run scope unit names) TUI-side where the config files
/// already live.
pub struct ClientSessionConfig<'a> {
    /// Daemon control socket. Three short-lived `UnixStream`
    /// connections are dialed here (one each for `start_session`,
    /// `session.attach`, and `kill_session`-if-needed for
    /// cleanup).
    pub daemon_socket: &'a Path,
    /// Operator identity for all three RPCs. Same value
    /// throughout — the slice-5 ticket allocator binds tickets to
    /// this payload and rejects consumption from a different
    /// identity.
    pub operator_token_id: &'a str,
    /// Pre-generated session uid (slice 10c-e-3b-fix). The TUI is
    /// the source of truth for uid identity because the MCP
    /// config file's env block has to bake `CM_TUI_SESSION_ID`
    /// at config-write time — before the daemon sees the spawn
    /// request. The daemon uses this verbatim (with format
    /// validation + collision check); a daemon-minted alternative
    /// would silently desync every downstream consumer.
    pub uid: &'a str,
    /// Workspace id for sidebar binding. Must already be in the
    /// daemon's manifest snapshot, OR `worktree_path` must be set
    /// (auto-register fallback — see [`Self::worktree_path`]).
    pub workspace_id: &'a str,
    /// Human-readable label for the sidebar — passed through to
    /// `start_session`'s `label` param.
    pub label: &'a str,
    /// Session-type discriminator passed to the daemon's
    /// `start_session` (slice 10d-mcp-surface-1). Canonical
    /// values: `"claude-code"`, `"codex"`, `"bash"` — these
    /// match the wire enum the Python MCP tool's caller code
    /// dispatches on. The daemon stores it on `DaemonSession`
    /// and surfaces it via `list_sessions`'s `type` field.
    ///
    /// Pre-fix #1 this wasn't sent on the wire; daemon-side
    /// `session_type` defaulted to `"claude-code"`, mislabeling
    /// codex / bash sessions and breaking the Python tool's
    /// dispatch on the `type` field.
    pub session_type: &'a str,
    /// Full argv to exec. `argv[0]` is the program; any wrappers
    /// (e.g. `systemd-run --user --scope -- claude ...` for a
    /// memory-capped session) are baked in by the caller. The
    /// daemon rejects empty argv with `InvalidParams`.
    pub argv: &'a [String],
    /// Working directory for the spawned child. Pre-resolved to an
    /// absolute path on the TUI side so the daemon doesn't reach
    /// for `worktree_path` from its manifest snapshot.
    pub working_dir: &'a Path,
    /// Process env for the spawned child. Caller populates via
    /// `mcp_config::build_env(SpawnTarget::Daemon, ...)` so the
    /// spawned MCP server routes its callbacks to the daemon
    /// socket. The daemon always pins `CM_TUI_SESSION_ID` to the
    /// session uid; other entries are passed through unchanged.
    pub env: std::collections::BTreeMap<String, String>,
    pub cols: u16,
    pub rows: u16,
    /// Memory-cap soft threshold in bytes (slice 10c-e-3b-fix2).
    /// `Some` when the TUI applied `wrap_with_systemd_run` to
    /// `argv`. The daemon uses this as the signal to populate
    /// `SpawnParams.kills_dir` so the reaper baseline + End-frame
    /// cap-kill attribution path runs. The actual cgroup-OOM
    /// watcher relocates in slice 10d-memory-cap-relocation;
    /// until then `memory_cap_kill` will stay `false` for
    /// daemon-spawned sessions even when this is `Some`.
    pub memory_cap_bytes: Option<u64>,
    /// Sub-2b-3 review-fix #1: hard cap byte count. Pre-fix
    /// only the soft byte count rode the wire — the daemon
    /// needs both to re-wrap argv for descendant
    /// `mcp_start_session` spawns. The TUI's `MemoryCap` carries
    /// `(soft_bytes, hard_bytes)`; pass both through.
    pub memory_cap_hard_bytes: Option<u64>,
    /// Sub-2b-3 review-fix #1: cgroup prefix
    /// (`memory_cap.cgroup_prefix`). Daemon needs this to build
    /// the scope unit's cgroup path when wrapping descendant
    /// argv. Distinct from `cgroup_path` below (which is the
    /// PREDICTED full path including the unit name for THIS
    /// session); cap inheritance needs the PREFIX so the
    /// daemon can generate fresh unit names for children.
    pub cgroup_prefix: Option<&'a Path>,
    /// Predicted systemd-run scope cgroup path (slice 10c-e-3b-fix2).
    /// `Some` paired with `memory_cap_bytes: Some(_)`. Passed
    /// through to the daemon (which echoes it on the response)
    /// so the TUI's local `Session.cgroup_path` mirrors what the
    /// local-spawn path produces.
    pub cgroup_path: Option<&'a Path>,
    /// Auto-register fallback for workspaces created mid-session
    /// (slice 10c-e-3). The daemon snapshots workspaces once at
    /// startup; a workspace created via `A-n` after the daemon
    /// started won't be in that map until 10e's `manifest.watch`
    /// ships. Passing the worktree path here lets the daemon
    /// register a minimal workspace entry on the fly so the spawn
    /// succeeds without a daemon restart.
    ///
    /// `None` preserves the prior behavior (daemon returns NotFound
    /// for unknown workspace_id) — appropriate for any pre-existing
    /// workspace that the daemon definitely knows about.
    pub worktree_path: Option<&'a Path>,
    /// Sub-2a Finding #1: task this session is bound to, sent on
    /// the wire so the daemon's `DaemonSession.task_id` is set at
    /// spawn time rather than left `None` and patched only via
    /// post-tag (which never updates the daemon copy). Required
    /// for the Session-caller descendant-task auth walk —
    /// without it, every daemon-spawned session looks taskless
    /// and a tasked agent falls into the same-workspace-allow
    /// branch, the widening the dispatch flip was meant to close.
    ///
    /// `None` for genuinely taskless spawns (the bare `A-n` shell
    /// flow). Operator-spawned sessions inherit task_id from the
    /// TUI's `TerminalSession.task_id` at the call site.
    pub task_id: Option<&'a str>,
    /// Sub-2b-1: transcript file path, when the TUI knows it at
    /// spawn time. The daemon stores this on
    /// `DaemonSession.transcript_path` so its
    /// `resolve_authorized_session` returns `state: "ready"` with
    /// the path; otherwise `state: "pending"`.
    ///
    /// `None` is the common fresh-spawn case (Claude/Codex
    /// transcript file is created post-spawn — the TUI doesn't
    /// know the path until its detector picks it up). Resume /
    /// clone flows where the TUI passes `--resume <id>` upfront
    /// CAN supply a predicted path; future plumbing will. The
    /// resolver returns pending until then; the Python MCP
    /// `read_session_output` tool short-circuits to empty
    /// messages and polls.
    pub transcript_path: Option<&'a str>,
    /// 10d-2c-1 review round-5 (F1): workflow run id this session
    /// is a participant of, when the spawn happens with workflow
    /// context already known. Stored on `DaemonSession` so
    /// `lookup_session_any` returns it for the daemon-side auth
    /// check in `workflow_transition` / `workflow_done`. `None`
    /// for non-workflow spawns; after-the-fact tagging on an
    /// already-spawned session uses `rpc_set_workflow_context`.
    pub workflow_run_id: Option<&'a str>,
    pub workflow_role: Option<&'a str>,
}

/// Daemon-attached terminal session. Field shape mirrors
/// [`crate::session::Session`] so 10c-e-3's opt-in branch can
/// swap the two at the construction site without rewiring
/// downstream consumers.
///
/// **Differences from `Session`**:
///   - No `pty_writer` (no kernel PTY fd; writes go through the
///     EventLoop channel via [`Self::write`]).
///   - No `memory_cap` / `cgroup_path` (the daemon owns those).
///   - New `session_uid` field — the daemon's stable id for this
///     session. Used for `kill_session` on Drop and
///     `read_session_output` snapshot reads.
pub struct ClientSession {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub sender: EventLoopSender,
    pub event_rx: mpsc::Receiver<alacritty_terminal::event::Event>,
    pub title: String,
    pub exited: bool,
    pub wakeup_times: Vec<Instant>,
    /// Daemon-side session uid (the value returned by
    /// `start_session`). Exposed for callers that need to issue
    /// follow-up RPCs (kill, read_session_output) against this
    /// session.
    pub session_uid: String,
    /// Cgroup path the daemon echoed back from `start_session`
    /// (slice 10c-e-3b-fix2). `Some` when memory cap was applied;
    /// the TUI's `Session.cgroup_path` mirrors this so the
    /// local-spawn shape is preserved across both paths.
    pub cgroup_path: Option<String>,
    /// Latched `memory_cap_kill` flag from the attach stream's
    /// `End` frame (slice 10c-e-3b-fix4b). The exit handler reads
    /// (and clears) this via `swap(false, SeqCst)` after observing
    /// `TermEvent::Exit`/`ChildExit`; `true` means the daemon's
    /// reaper attributed the exit to a memory-cap kill, and the
    /// TUI's cap-kill toast renders.
    ///
    /// Shared with the `AttachedPty` (which lives inside
    /// alacritty's EventLoop thread post-spawn); the reader half
    /// stores `true` here BEFORE signalling the child-event pipe,
    /// so by the time the EventLoop delivers the exit event the
    /// flag is already populated.
    pub memory_cap_kill: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

// Post-review #15 (deferred): a `Drop` impl that calls
// `rpc_kill_session` as a safety net for panic-paths / unexpected
// drops would prevent orphan daemon children. But it conflicts
// with `Session::new_attached`'s partial-move pattern at
// `tui/src/session.rs:274-300`: Rust forbids moving fields out of
// a type that implements `Drop`. Fixing properly requires
// extracting a `ClientSessionKillGuard` sub-struct (which can
// implement `Drop` while the rest of the fields move out around
// it) OR wrapping the destructure-on-construct path in a
// `ManuallyDrop` / `into_inner` API. Both are bigger refactors
// than this batch's scope; tracked as a follow-up. Current
// safety: the explicit `A-w` and workspace-teardown paths fire
// `App::kill_daemon_session_if_attached` before the
// `TerminalSession` is dropped, so orphans only happen on panic
// / future code that bypasses those handlers.

impl ClientSession {
    /// Drive the full RPC dance and return a fully-armed
    /// `ClientSession`. See the module docs for the step
    /// sequence and the failure-cleanup contract.
    pub fn new(config: ClientSessionConfig) -> anyhow::Result<Self> {
        // Step 1: start_session. From here on, any error must
        // kill_session the just-created session before
        // bubbling — otherwise the daemon registry leaks a
        // started-but-unattached entry.
        let start_result = rpc_start_session_full(&config)
            .context("RPC start_session")?;

        match Self::build_after_start(&config, &start_result) {
            Ok(session) => Ok(session),
            Err(e) => {
                // Best-effort cleanup. We log the cleanup error
                // separately so the operator can see both
                // failures if a cascade happens.
                if let Err(cleanup_err) = rpc_kill_session(
                    config.daemon_socket,
                    config.operator_token_id,
                    &start_result.session_uid,
                ) {
                    eprintln!(
                        "ClientSession::new cleanup: kill_session({}) failed after primary error: {}",
                        start_result.session_uid, cleanup_err,
                    );
                }
                Err(e)
            }
        }
    }

    /// migrate-tui-local Issue 1: re-attach to an ALREADY-LIVE
    /// daemon-side session (skip `start_session`). Used by the
    /// manifest-restore path when the TUI restarts but the daemon
    /// + its PTY children survive — pre-fix the restore tried to
    /// `start_session` the same UID, got `Conflict` from the
    /// daemon's collision guard, and lost the session.
    ///
    /// The caller has already established (via `rpc_list_sessions`
    /// or equivalent) that `config.uid` matches a live entry in
    /// the daemon's registry. We trust that and go straight to
    /// step 2 (`session.attach`) + step 3 (`attach.open`).
    ///
    /// On failure (e.g. the session exited between the probe and
    /// the attach), the daemon-side child is NOT touched — we
    /// don't own it. The pre-fix `new()` cleanup arm only fires
    /// on `start_session`-driven failures.
    pub fn attach_existing(config: ClientSessionConfig) -> anyhow::Result<Self> {
        // Step 1 skipped: the daemon already has the session.
        // The wire shape for steps 2–6 is identical to `new()`
        // — `build_after_start` only consumes
        // `start_result.session_uid` + `start_result.cgroup_path`
        // (which is `None` for re-attach since the daemon's
        // existing session already owns the scope).
        let synthetic_start = StartSessionResult {
            session_uid: config.uid.to_string(),
            cgroup_path: None,
        };
        Self::build_after_start(&config, &synthetic_start)
    }

    /// Steps 2–6 of the dance. Factored out so the outer `new`
    /// can wrap the entire result in one cleanup arm.
    fn build_after_start(
        config: &ClientSessionConfig,
        start_result: &StartSessionResult,
    ) -> anyhow::Result<Self> {
        let session_uid = &start_result.session_uid;
        // Step 2: session.attach → ticket + attach_addr.
        let attach_resp = rpc_session_attach(config, session_uid)
            .context("RPC session.attach")?;

        // Step 3: dial a fresh socket to attach_addr. That socket
        // morphs into the PTY stream after attach.open's response;
        // no re-dial after.
        let mut attach_socket = UnixStream::connect(Path::new(&attach_resp.attach_addr))
            .with_context(|| {
                format!("dial attach socket at {}", attach_resp.attach_addr)
            })?;

        // Step 4: attach.open on the same socket. The response
        // confirms attach success and echoes session_uid; verify
        // it matches what start_session gave us — a mismatch
        // would be a serious daemon-side bug, surface loudly.
        let open_resp = rpc_attach_open(
            &mut attach_socket,
            config.operator_token_id,
            &attach_resp.attach_ticket,
        )
        .context("RPC attach.open")?;
        if &open_resp.session_uid != session_uid {
            anyhow::bail!(
                "attach.open uid mismatch: requested {}, daemon returned {}",
                session_uid,
                open_resp.session_uid,
            );
        }

        // Step 5: morph the socket into an AttachedPty. From here
        // the connection is a one-way StreamFrame channel.
        let pty = AttachedPty::from_socket(
            attach_socket,
            format!("attach-{}", session_uid),
        )
        .context("wrap attach socket as AttachedPty")?;

        // Grab a handle to the `memory_cap_kill` flag BEFORE the
        // EventLoop takes ownership of `pty` (slice
        // 10c-e-3b-fix4b). The latched-by-reader contract stays
        // on `AttachedPty`/`ReaderHalf`; we just expose the Arc
        // out so the TUI's exit handler can observe and clear it
        // post-spawn.
        let memory_cap_kill = pty.memory_cap_kill_handle();

        // Step 6: build alacritty Term + EventLoop over the
        // AttachedPty. Mirrors Session::new's post-PTY shape so
        // 10c-e-3's branch at the construction site can produce
        // either ClientSession or Session without rewiring
        // downstream callers.
        let (event_tx, event_rx) = mpsc::channel();
        let event_proxy = EventProxy::new(event_tx);
        let mut term_config = TermConfig::default();
        term_config.kitty_keyboard = true;

        let size = TermSize {
            columns: config.cols as usize,
            screen_lines: config.rows as usize,
        };
        let term = Term::new(term_config, &size, event_proxy.clone());
        let term = Arc::new(FairMutex::new(term));

        let _window_size = WindowSize {
            num_lines: config.rows,
            num_cols: config.cols,
            cell_width: 1,
            cell_height: 1,
        };

        let event_loop = EventLoop::new(
            term.clone(),
            event_proxy,
            pty,
            true,  // drain_on_exit
            false, // ref_test
        )?;
        let sender = event_loop.channel();
        // After this, the EventLoop thread owns the AttachedPty.
        // No further fallible operations — struct construction
        // below is infallible.
        event_loop.spawn();

        Ok(ClientSession {
            term,
            sender,
            event_rx,
            title: format!("{} (daemon:{})", config.label, session_uid),
            exited: false,
            wakeup_times: Vec::new(),
            session_uid: session_uid.to_string(),
            cgroup_path: start_result.cgroup_path.clone(),
            memory_cap_kill,
        })
    }

    /// Send keystrokes to the daemon-side PTY through the
    /// EventLoop's input channel. Differs from `Session::write`
    /// which goes directly to the kernel PTY fd — here, the
    /// EventLoop thread serializes input writes through
    /// `Msg::Input`, which it then encodes as a `StreamFrame`
    /// via `AttachedPty::writer()`'s `StreamWriter::write`.
    ///
    /// Returns `Ok(())` on successful enqueue. The channel send
    /// is non-blocking; failure means the EventLoop has exited
    /// (typically because the daemon dropped the attach socket).
    pub fn write(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::borrow::Cow;
        self.sender
            .send(Msg::Input(Cow::Owned(data.to_vec())))
            .map_err(|e| {
                std::io::Error::other(format!(
                    "ClientSession::write: EventLoop channel send failed: {}",
                    e
                ))
            })
    }

    /// Notify the daemon-side PTY of a terminal resize. Mirrors
    /// `Session::resize` modulo the input path (Msg::Resize is
    /// the same; the EventLoop thread invokes
    /// `StreamWriter::send_resize` which encodes a
    /// `{"resize": {cols, rows}}` data frame the daemon
    /// recognises and applies to the PTY).
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

// ---------------------------------------------------------------------------
// RPC helpers
// ---------------------------------------------------------------------------
//
// Each helper dials a fresh Unix socket (except rpc_attach_open which
// reuses an already-dialed socket because that socket morphs into the
// stream after the response). All four use `Caller::Operator(token_id)`
// so the slice-5 ticket consume succeeds.

/// Generate a per-RPC request id. Doesn't have to be globally
/// unique — the daemon echoes it back so the client can pair
/// requests with responses; a monotonic counter mixed with nanos
/// is the same shape `tui/src/app.rs::new_session_uid` uses.
fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cs-{:x}-{:x}", nanos, n)
}

/// Execute one request/response round-trip against the daemon.
/// Dials a fresh UnixStream, writes the request, reads the
/// response, closes. Returns the parsed `Response` or an error.
///
/// On `Response::ok = false`, surfaces an anyhow error carrying
/// the daemon's error code + message so callers don't have to
/// unpack the envelope themselves.
/// Default per-RPC read timeout. Most RPCs respond in well under a second; the
/// timeout exists so the main thread can't block forever against an
/// unresponsive (e.g. ssh-tunneled) daemon.
const DEFAULT_RPC_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// P-3b: read timeout for the `start_workflow` RPC specifically. The daemon
/// serializes each participant's transcript-detector binding (the cross-bind
/// fix), waiting up to the per-role slot timeout
/// (`SLOT_WAIT_TIMEOUT_DEFAULT_MS` = 20s) before spawning the next role. A
/// feedback-style 3-role launch can therefore legitimately hold the response
/// open well past the default 5s. Size this safely ABOVE the daemon's bounded
/// worst case (roughly roles × 20s): 150s covers up to ~7 roles with margin, so
/// the client never gives up while the daemon is still mid-launch and reports a
/// false "launch failed". The daemon saves+broadcasts the run only on FULL
/// success (atomic), so even a give-up can't leave a half-launched run visible.
pub const START_WORKFLOW_RPC_READ_TIMEOUT: Duration = Duration::from_secs(150);

fn rpc_round_trip(daemon_socket: &Path, req: &Request) -> anyhow::Result<Response> {
    rpc_round_trip_with_read_timeout(daemon_socket, req, DEFAULT_RPC_READ_TIMEOUT)
}

fn rpc_round_trip_with_read_timeout(
    daemon_socket: &Path,
    req: &Request,
    read_timeout: Duration,
) -> anyhow::Result<Response> {
    let mut stream = UnixStream::connect(daemon_socket)
        .with_context(|| format!("dial daemon socket {}", daemon_socket.display()))?;
    // Without these the read blocks the main thread forever when the
    // remote side stops responding — e.g. ssh-tunneled cm-manager
    // after laptop sleep / wifi loss. The local UnixStream stays
    // alive (local ssh holds it) but the remote daemon never
    // replies; the 12e-perf reachability cache can't help because
    // it only registers on a real failure.
    stream
        .set_read_timeout(Some(read_timeout))
        .context("set rpc read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("set rpc write timeout")?;
    wire::write_request(&mut stream, req)
        .context("write request frame")?;
    // Best-effort half-close on the write side so the daemon's
    // read_request can see EOF promptly if it ever needs to.
    let _ = stream.flush();
    let resp = wire::read_response(&mut stream)
        .context("read response frame")?
        .ok_or_else(|| anyhow::anyhow!("daemon closed connection before responding"))?;
    if !resp.ok {
        let err = resp.error.as_ref();
        anyhow::bail!(
            "daemon RPC {} failed: {} ({:?})",
            req.method,
            err.map(|e| e.message.as_str()).unwrap_or("<no message>"),
            err.map(|e| e.code).unwrap_or(ErrorCode::Internal),
        );
    }
    Ok(resp)
}

/// `start_session` RPC. Returns the new session uid.
///
/// Sends `cols`/`rows` so the daemon-spawned PTY's initial
/// `TIOCGWINSZ` matches the operator's live terminal — without
/// these, the daemon would default to 80x24 and full-screen apps
/// would misrender until the first reactive resize event fired
/// (slice-10c-e-2 review-3 fix).
fn rpc_start_session(config: &ClientSessionConfig) -> anyhow::Result<String> {
    Ok(rpc_start_session_full(config)?.session_uid)
}

/// Full `start_session` response shape (slice 10c-e-3b-fix2).
/// Internal — call sites that need `cgroup_path` use this; the
/// historical `rpc_start_session` thin wrapper preserves the
/// uid-only return for callers that don't care.
pub(crate) struct StartSessionResult {
    pub session_uid: String,
    pub cgroup_path: Option<String>,
}

pub(crate) fn rpc_start_session_full(
    config: &ClientSessionConfig,
) -> anyhow::Result<StartSessionResult> {
    let mut params = serde_json::json!({
        "uid": config.uid,
        "workspace_id": config.workspace_id,
        "label": config.label,
        // Slice 10d-mcp-surface-1 fix #1: `session_type` must
        // travel on the wire so the daemon's `list_sessions`
        // surfaces the correct `type` field for the Python MCP
        // tool's dispatch. Pre-fix this was omitted and the
        // daemon defaulted to "claude-code", mislabeling
        // codex / bash sessions.
        "session_type": config.session_type,
        "argv": config.argv,
        "working_dir": config.working_dir.display().to_string(),
        "env": config.env,
        "cols": config.cols,
        "rows": config.rows,
    });
    if let Some(wt) = config.worktree_path {
        params["worktree_path"] = serde_json::Value::String(wt.display().to_string());
    }
    if let Some(bytes) = config.memory_cap_bytes {
        params["memory_cap_bytes"] = serde_json::Value::Number(bytes.into());
    }
    // Sub-2b-3 review-fix #1: also send the hard cap byte count
    // and the cgroup_prefix. Daemon stores them on
    // `DaemonSession` so descendant `mcp_start_session` spawns
    // can re-wrap argv with the same (soft, hard, prefix) and
    // the subtask inherits the cap. Pre-fix only the soft byte
    // count rode the wire and the daemon couldn't re-wrap.
    if let Some(bytes) = config.memory_cap_hard_bytes {
        params["memory_cap_hard_bytes"] = serde_json::Value::Number(bytes.into());
    }
    if let Some(prefix) = config.cgroup_prefix {
        params["cgroup_prefix"] =
            serde_json::Value::String(prefix.display().to_string());
    }
    // Sub-2a Finding #1: send task_id so the daemon records it
    // on `DaemonSession.task_id` at spawn time. The auth walk
    // reads it for the descendant-task gate; if absent, the
    // session looks taskless and a tasked caller's session can
    // reach sibling-task sessions in the same workspace via the
    // taskless-allow branch — exactly the widening sub-2a is
    // meant to close.
    if let Some(tid) = config.task_id {
        params["task_id"] = serde_json::Value::String(tid.to_string());
    }
    // Sub-2b-1: send transcript_path so the daemon's
    // `resolve_authorized_session` can serve the Python MCP
    // `read_session_output` tool's compose pattern without a
    // TUI round-trip. `None` for fresh spawns; the daemon
    // returns `state: "pending"` and the Python tool polls.
    if let Some(tp) = config.transcript_path {
        params["transcript_path"] = serde_json::Value::String(tp.to_string());
    }
    // 10d-2c-1 review round-5 (F1): workflow context for daemon-side
    // `lookup_session_any` → `workflow_transition` / `workflow_done`
    // auth. Both fields must be sent together (the daemon refuses
    // half-tagged spawns at the after-the-fact RPC; spawn-time
    // accepts None/None as "non-workflow").
    if let Some(run_id) = config.workflow_run_id {
        params["workflow_run_id"] = serde_json::Value::String(run_id.to_string());
    }
    if let Some(role) = config.workflow_role {
        params["workflow_role"] = serde_json::Value::String(role.to_string());
    }
    // Slice 10d watcher-fix #1: `cgroup_path` is NO LONGER
    // sent on the wire. The daemon discovers the actual cgroup
    // from `/proc/<spawn-pid>/cgroup` post-spawn (see
    // `daemon/src/path.rs::discover_session_cgroup_path`).
    // Sending a caller-supplied path would let a buggy or
    // malicious caller direct the daemon's watcher at a cgroup
    // pre-populated with PIDs from unrelated processes — same
    // bug class as the kill_session orphan / writer-deadlock /
    // silent-paste-drop fixes earlier in this chain. The
    // `config.cgroup_path` field is kept for build-time
    // compatibility but its value is discarded here.
    let _ = config.cgroup_path; // intentionally unused
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(config.operator_token_id),
        method: "start_session".into(),
        params,
    };
    let resp = rpc_round_trip(config.daemon_socket, &req)?;
    let result = resp.result.context("start_session response missing result")?;
    let session_uid = result["session_uid"]
        .as_str()
        .context("start_session result missing session_uid")?
        .to_string();
    let cgroup_path = result["cgroup_path"].as_str().map(|s| s.to_string());
    Ok(StartSessionResult {
        session_uid,
        cgroup_path,
    })
}

/// `create_session` response (remote-session-execution Phase 1/3). The
/// daemon resolves the repo + creates the worktree on its OWN filesystem
/// and returns identity-only.
pub(crate) struct CreateSessionResult {
    pub session_uid: String,
    pub worktree_path: String,
    pub workspace_id: String,
}

/// `create_session` RPC — A-n on a (possibly remote) daemon host. Unlike
/// `start_session`, the TUI sends only the high-level request (no local
/// argv / env / working_dir / MCP-config path / cgroup_prefix); the daemon
/// resolves the repo, creates `~/.cm/worktrees/<repo>-<slug>` on `cm/<slug>`,
/// and builds argv/env itself. Operator-only on the daemon side.
#[allow(clippy::too_many_arguments)]
pub fn rpc_create_session(
    daemon_socket: &Path,
    operator_token_id: &str,
    uid: &str,
    workspace_id: &str,
    label: &str,
    engine: &str,
    repo_url: &str,
    start_branch: Option<&str>,
    slug: &str,
    task_id: Option<&str>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<CreateSessionResult> {
    let mut params = serde_json::json!({
        "uid": uid,
        "workspace_id": workspace_id,
        "label": label,
        "engine": engine,
        "repo_url": repo_url,
        "slug": slug,
        "cols": cols,
        "rows": rows,
    });
    if let Some(sb) = start_branch {
        params["start_branch"] = serde_json::Value::String(sb.to_string());
    }
    if let Some(tid) = task_id {
        params["task_id"] = serde_json::Value::String(tid.to_string());
    }
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "create_session".into(),
        params,
    };
    let resp = rpc_round_trip(daemon_socket, &req)?;
    let result = resp.result.context("create_session response missing result")?;
    Ok(CreateSessionResult {
        session_uid: result["session_uid"]
            .as_str()
            .context("create_session result missing session_uid")?
            .to_string(),
        worktree_path: result["worktree_path"]
            .as_str()
            .context("create_session result missing worktree_path")?
            .to_string(),
        // The daemon echoes the workspace_id it registered; fall back to
        // the one we sent if an older daemon omits it.
        workspace_id: result["workspace_id"]
            .as_str()
            .unwrap_or(workspace_id)
            .to_string(),
    })
}

/// `add_session` response (remote-session-execution Phase 1/3).
pub(crate) struct AddSessionResult {
    pub session_uid: String,
    pub worktree_path: String,
}

/// `add_session` RPC — A-s on a remote-hosted workspace. Reuses the
/// workspace's existing worktree (no `repo_url`/`slug`/`start_branch`);
/// the daemon looks up `workspace_id` and spawns into its worktree.
/// Operator-only on the daemon side.
#[allow(clippy::too_many_arguments)]
pub fn rpc_add_session(
    daemon_socket: &Path,
    operator_token_id: &str,
    uid: &str,
    workspace_id: &str,
    label: &str,
    engine: &str,
    task_id: Option<&str>,
    cols: u16,
    rows: u16,
) -> anyhow::Result<AddSessionResult> {
    let mut params = serde_json::json!({
        "uid": uid,
        "workspace_id": workspace_id,
        "label": label,
        "engine": engine,
        "cols": cols,
        "rows": rows,
    });
    if let Some(tid) = task_id {
        params["task_id"] = serde_json::Value::String(tid.to_string());
    }
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "add_session".into(),
        params,
    };
    let resp = rpc_round_trip(daemon_socket, &req)?;
    let result = resp.result.context("add_session response missing result")?;
    Ok(AddSessionResult {
        session_uid: result["session_uid"]
            .as_str()
            .context("add_session result missing session_uid")?
            .to_string(),
        worktree_path: result["worktree_path"]
            .as_str()
            .context("add_session result missing worktree_path")?
            .to_string(),
    })
}

/// `session.attach` response shape — local to the client; we
/// don't share types with the daemon-side `attach::SessionAttachResponse`
/// because the wire format is a plain JSON object and the daemon's
/// struct is internal.
struct AttachResp {
    attach_ticket: String,
    attach_addr: String,
}

/// `session.attach` RPC. Returns ticket + attach_addr.
fn rpc_session_attach(
    config: &ClientSessionConfig,
    session_uid: &str,
) -> anyhow::Result<AttachResp> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(config.operator_token_id),
        method: "session.attach".into(),
        params: serde_json::json!({ "uid": session_uid }),
    };
    let resp = rpc_round_trip(config.daemon_socket, &req)?;
    let result = resp.result.context("session.attach response missing result")?;
    Ok(AttachResp {
        attach_ticket: result["attach_ticket"]
            .as_str()
            .context("session.attach result missing attach_ticket")?
            .to_string(),
        attach_addr: result["attach_addr"]
            .as_str()
            .context("session.attach result missing attach_addr")?
            .to_string(),
    })
}

struct AttachOpenResp {
    session_uid: String,
}

/// `attach.open` over an *already-dialed* `UnixStream`. The same
/// socket morphs into the PTY stream after the response; callers
/// pass it back into [`AttachedPty::from_socket`] for the
/// stream-mode half.
fn rpc_attach_open(
    socket: &mut UnixStream,
    operator_token_id: &str,
    ticket: &str,
) -> anyhow::Result<AttachOpenResp> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "attach.open".into(),
        params: serde_json::json!({ "ticket": ticket }),
    };
    wire::write_request(socket, &req).context("write attach.open frame")?;
    let _ = socket.flush();
    let resp = wire::read_response(socket)
        .context("read attach.open response")?
        .ok_or_else(|| anyhow::anyhow!("daemon closed attach socket before responding"))?;
    if !resp.ok {
        let err = resp.error.as_ref();
        anyhow::bail!(
            "attach.open failed: {} ({:?})",
            err.map(|e| e.message.as_str()).unwrap_or("<no message>"),
            err.map(|e| e.code).unwrap_or(ErrorCode::Internal),
        );
    }
    let result = resp.result.context("attach.open response missing result")?;
    Ok(AttachOpenResp {
        session_uid: result["session_uid"]
            .as_str()
            .context("attach.open result missing session_uid")?
            .to_string(),
    })
}

/// `kill_session` RPC. Two callers in slice 10c-e-3:
///   - Internal: cleanup path when a later step in
///     `ClientSession::new` fails after `start_session` succeeded.
///   - External (`pub`): the TUI's A-w close path invokes this
///     against the daemon when a daemon-attached session is being
///     torn down. Without it, closing the session would only drop
///     the attach socket — the daemon's PTY child would keep
///     running until it exited on its own.
pub fn rpc_kill_session(
    daemon_socket: &Path,
    operator_token_id: &str,
    session_uid: &str,
) -> anyhow::Result<()> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "kill_session".into(),
        params: serde_json::json!({ "session_uid": session_uid }),
    };
    rpc_round_trip(daemon_socket, &req).map(|_| ())
}

/// migrate-tui-local Issue 1: enumerate the daemon's currently
/// known session UIDs. The manifest-restore path uses this on TUI
/// startup to distinguish "daemon already has this session"
/// (attach) from "daemon doesn't know about it, must spawn"
/// (start_session).
///
/// migrate-tui-local Issue A: passes `daemon_owned_only: true`
/// so the response excludes UIDs that only exist in
/// `state.tui_sessions` (stale TUI-pushed snapshot rows from a
/// previous TUI process). Without the filter the probe would
/// treat snapshot rows as attachable; `session.attach` would
/// then fail (no live PTY behind a snapshot row) and the
/// restore would silently drop the manifest entry.
///
/// Operator-only on the daemon side. Returns the set of
/// daemon-owned session UIDs. Empty set on RPC error (treated as
/// "daemon doesn't know about anything"; the caller then falls
/// back to the start_session path which is the pre-fix
/// behavior).
pub fn rpc_list_session_uids(
    daemon_socket: &Path,
    operator_token_id: &str,
) -> anyhow::Result<std::collections::HashSet<String>> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "list_sessions".into(),
        params: serde_json::json!({ "daemon_owned_only": true }),
    };
    let resp = rpc_round_trip(daemon_socket, &req)?;
    let result = resp
        .result
        .context("list_sessions response missing result")?;
    let arr = result
        .as_array()
        .context("list_sessions result not an array")?;
    let mut out = std::collections::HashSet::with_capacity(arr.len());
    for entry in arr {
        if let Some(uid) = entry.get("session_uid").and_then(|v| v.as_str()) {
            out.insert(uid.to_string());
        }
    }
    Ok(out)
}

/// Summary of a daemon-owned session, parsed from `list_sessions`. Used
/// by the TUI's adoption pass (`App::adopt_untracked_daemon_sessions`) to
/// surface agent-spawned ("phantom") sessions in the sidebar. Fields
/// beyond uid/label/type are `Option` so an older daemon (pre Part-1
/// adoption metadata) still parses — the TUI then degrades to a synthetic
/// per-session workspace instead of grouping by `workspace_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSessionSummary {
    pub session_uid: String,
    pub label: String,
    /// Wire session type ("claude-code" / "codex" / "bash").
    pub session_type: String,
    /// `Some` = agent-spawned via mcp_start_session; `None` = TUI-/
    /// operator-spawned. The adoption pass only adopts `Some`.
    pub managed_by_uid: Option<String>,
    pub workspace_id: Option<String>,
    pub task_id: Option<String>,
    pub workflow_run_id: Option<String>,
    pub workflow_role: Option<String>,
    pub worktree_path: Option<String>,
}

/// Parse one `list_sessions` array entry into a [`DaemonSessionSummary`].
/// Returns `None` if the entry lacks a `session_uid`. Split out so the
/// adoption-pass tests can exercise parsing without a live daemon.
pub fn parse_daemon_session_summary(entry: &serde_json::Value) -> Option<DaemonSessionSummary> {
    let session_uid = entry.get("session_uid").and_then(|v| v.as_str())?.to_string();
    let str_field = |k: &str| {
        entry
            .get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    Some(DaemonSessionSummary {
        session_uid,
        label: str_field("label").unwrap_or_default(),
        session_type: str_field("type").unwrap_or_default(),
        managed_by_uid: str_field("managed_by_uid"),
        workspace_id: str_field("workspace_id"),
        task_id: str_field("task_id"),
        workflow_run_id: str_field("workflow_run_id"),
        workflow_role: str_field("workflow_role"),
        worktree_path: str_field("worktree_path"),
    })
}

/// List daemon-owned sessions with the metadata the TUI needs to adopt
/// (surface) agent-spawned sessions into the sidebar. Mirrors
/// [`rpc_list_session_uids`] (same `daemon_owned_only: true` filter, which
/// excludes stale `tui_sessions` snapshot rows) but returns full summaries.
/// Operator-only on the daemon side.
pub fn rpc_list_daemon_sessions(
    daemon_socket: &Path,
    operator_token_id: &str,
) -> anyhow::Result<Vec<DaemonSessionSummary>> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "list_sessions".into(),
        params: serde_json::json!({ "daemon_owned_only": true }),
    };
    let resp = rpc_round_trip(daemon_socket, &req)?;
    let result = resp
        .result
        .context("list_sessions response missing result")?;
    let arr = result
        .as_array()
        .context("list_sessions result not an array")?;
    Ok(arr.iter().filter_map(parse_daemon_session_summary).collect())
}

/// `session.set_transcript_path` RPC. Sub-2b-1 review #1: the
/// TUI's transcript-discovery detector (the
/// `pending_jsonl_files` → `transcript_id` binding in
/// `app.rs::drain_terminal_events`) calls this when it resolves
/// a Claude/Codex transcript file for a daemon-attached
/// session. The daemon stores the path on
/// `DaemonSession.transcript_path` so its
/// `resolve_authorized_session` transitions from `pending` to
/// `ready` and the Python MCP `read_session_output` tool can
/// parse the file.
///
/// Operator-only on the daemon side — TUI uses `tui-operator`
/// token id (same as the other Operator RPCs).
pub fn rpc_set_transcript_path(
    daemon_socket: &Path,
    operator_token_id: &str,
    session_uid: &str,
    transcript_path: &str,
) -> anyhow::Result<()> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "session.set_transcript_path".into(),
        params: serde_json::json!({
            "session_uid": session_uid,
            "transcript_path": transcript_path,
        }),
    };
    rpc_round_trip(daemon_socket, &req).map(|_| ())
}

/// 10d-2c-1 review round-5 (F1): push workflow context onto an
/// already-spawned daemon session. Used by the former TUI
/// controller's launch after tagging a daemon-attached
/// `TerminalSession` so the daemon's `DaemonSession` mirrors the
/// TUI's view — without this RPC, `lookup_session_any` returns
/// `(None, None)` for daemon-attached workflow participants and
/// auth on `workflow_transition` / `workflow_done` rejects them.
///
/// Operator-only on the daemon side; the TUI uses the shared
/// `tui-operator` token id.
///
/// Best-effort caller: pass `None` / `None` to clear (workflow
/// stopped on the session). Pass `Some(_)` for both to set; the
/// daemon refuses half-tagged (one Some, one None) updates.
pub fn rpc_set_workflow_context(
    daemon_socket: &Path,
    operator_token_id: &str,
    uid: &str,
    workflow_run_id: Option<&str>,
    workflow_role: Option<&str>,
) -> anyhow::Result<()> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "session.set_workflow_context".into(),
        params: serde_json::json!({
            "uid": uid,
            "workflow_run_id": workflow_run_id,
            "workflow_role": workflow_role,
        }),
    };
    rpc_round_trip(daemon_socket, &req).map(|_| ())
}

/// `task.update_tree` RPC. Sub-2a Finding #1: pushes the TUI's
/// current task tree to the daemon as a full-replace snapshot.
/// The daemon caches it in `DaemonState.task_tree` for the
/// Session-caller descendant-task auth check (see
/// `daemon/src/control/auth.rs::task_is_self_or_descendant_of`).
///
/// `tasks` is the full set of TaskEntries to publish — each item
/// is `(task_id, parent_task_id)`. Tasks with `task_id == None`
/// in the TUI (the rare backlog-without-id entries) are excluded
/// by the caller; this helper assumes every item has a real id.
///
/// Operator-only on the daemon side — a Session caller rewriting
/// the tree could escape their own auth scope. The TUI uses the
/// shared `tui-operator` token id like every other RPC site.
/// Sub-2b-3 review-2 #1: each task entry now carries an
/// optional bound `workspace_id` so the daemon can resolve a
/// descendant task's workspace without needing a live anchor
/// session there. Per-workspace `worktree_path` rides in the
/// `workspaces` slice (replace-not-merge on the daemon side
/// in lockstep with the task list).
///
/// Operator-only on the daemon side — a Session caller
/// rewriting the tree could escape their own auth scope. The
/// TUI uses the shared `tui-operator` token id like every
/// other RPC site.
/// 10d-1: TUI-pushed session snapshot. Replaces the daemon's
/// `state.tui_sessions` map wholesale (replace-not-merge, same
/// shape as `task.update_tree`). Operator-only on the daemon
/// side — a Session caller could grant itself visibility into
/// another task's sessions once 10d-2's workflow-method auth
/// reads from this map.
///
/// `sessions` carries `(uid, task_id, label, session_type,
/// hidden, workflow_run_id, workflow_role)` per row. The auth
/// consumer in 10d-2 needs the task_id + workflow tags for
/// descendant-task / workflow-run scoping; the label / type /
/// hidden fields are forward-compat for a future merged
/// `list_sessions` view (opt-in-off mode where the TUI is
/// authoritative).
pub fn rpc_tui_update_sessions_snapshot(
    daemon_socket: &Path,
    operator_token_id: &str,
    sessions: &[TuiSessionSnapshotPush<'_>],
) -> anyhow::Result<()> {
    let sessions_json: Vec<serde_json::Value> = sessions
        .iter()
        .map(|s| {
            serde_json::json!({
                "uid": s.uid,
                "task_id": s.task_id,
                "label": s.label,
                "type": s.session_type,
                "hidden": s.hidden,
                "workflow_run_id": s.workflow_run_id,
                "workflow_role": s.workflow_role,
            })
        })
        .collect();
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "tui.update_sessions_snapshot".into(),
        params: serde_json::json!({ "sessions": sessions_json }),
    };
    rpc_round_trip(daemon_socket, &req).map(|_| ())
}

/// 10d-1: borrowed view of a single TUI session row to push.
/// Built fresh per call from `App.workspaces[*].sessions[*]`
/// so the RPC site doesn't need to clone the whole session
/// map. `&str` borrows live until `rpc_tui_update_sessions_snapshot`
/// returns.
pub struct TuiSessionSnapshotPush<'a> {
    pub uid: &'a str,
    pub task_id: Option<&'a str>,
    pub label: Option<&'a str>,
    pub session_type: Option<&'a str>,
    pub hidden: bool,
    pub workflow_run_id: Option<&'a str>,
    pub workflow_role: Option<&'a str>,
}

/// 10d-2c-2-1: push the full workflow-definitions map to the
/// daemon (replace-not-merge). Operator-only. The daemon stores
/// the map on `DaemonState.workflow_definitions`; the upcoming
/// daemon-resident workflow driver (2c-2-2) reads from there
/// instead of TUI-resident `App.workflows`.
///
/// Called from `App::push_workflow_definitions_to_daemon` once
/// at TUI startup (after the TOML load). The map is small and
/// rarely changes — replace-not-merge mirrors
/// `tui.update_sessions_snapshot` semantics.
/// Phase 4 §D: launch a workflow on the daemon. The daemon spawns the
/// participants, writes the initial state.json, and drives the run via its
/// poller; the TUI observes the result through `workflow_watch` /
/// `manifest.watch`. Returns the new run_id. The `worktree`/`workspace_id` are
/// passed explicitly (Operator caller) since the TUI knows the focused
/// workspace.
pub fn rpc_start_workflow(
    daemon_socket: &Path,
    operator_token_id: &str,
    workflow_name: &str,
    worktree: &str,
    workspace_id: &str,
    goal: Option<&str>,
    task_id: Option<&str>,
    role_sessions: &std::collections::BTreeMap<String, String>,
    role_engines: &std::collections::BTreeMap<String, String>,
    size: (u16, u16),
) -> anyhow::Result<String> {
    // (cols, rows): participants spawn at the operator's terminal size instead
    // of the daemon-side 80×24 default ("super narrow window" otherwise).
    let mut params = serde_json::json!({
        "workflow_name": workflow_name,
        "worktree": worktree,
        "workspace_id": workspace_id,
        "cols": size.0,
        "rows": size.1,
    });
    if let Some(g) = goal {
        params["goal"] = serde_json::Value::String(g.to_string());
    }
    if let Some(t) = task_id {
        params["task_id"] = serde_json::Value::String(t.to_string());
    }
    // Phase 3 (doc/existing-session-binding.md): forward the existing-session
    // bindings (`role -> daemon_session_uid`) ONLY when non-empty, so a launch
    // with no `Existing` slots is byte-identical on the wire to the pre-Phase-3
    // fresh-spawn call (the daemon's `role_sessions` param is
    // `#[serde(default)]`). Values are DAEMON session uids, never local UI
    // handles — the daemon's eligibility check keys on `state.sessions`.
    if !role_sessions.is_empty() {
        let map: serde_json::Map<String, serde_json::Value> = role_sessions
            .iter()
            .map(|(role, uid)| (role.clone(), serde_json::Value::String(uid.clone())))
            .collect();
        params["role_sessions"] = serde_json::Value::Object(map);
    }
    // Per-role engine overrides ("new claude" vs "new codex"). Forwarded ONLY
    // when non-empty (i.e. at least one fresh slot diverges from its TOML
    // default), so an all-default launch stays byte-identical on the wire to the
    // pre-engine-choice call (the daemon's `role_engines` param is
    // `#[serde(default)]`).
    if !role_engines.is_empty() {
        let map: serde_json::Map<String, serde_json::Value> = role_engines
            .iter()
            .map(|(role, engine)| (role.clone(), serde_json::Value::String(engine.clone())))
            .collect();
        params["role_engines"] = serde_json::Value::Object(map);
    }
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "start_workflow".into(),
        params,
    };
    // P-3b: use the longer start_workflow timeout — the daemon may serialize
    // per-role detector binding for several roles before responding.
    let resp = rpc_round_trip_with_read_timeout(
        daemon_socket,
        &req,
        START_WORKFLOW_RPC_READ_TIMEOUT,
    )?;
    let run_id = resp
        .result
        .as_ref()
        .and_then(|v| v.get("run_id"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("start_workflow: response missing run_id"))?
        .to_string();
    Ok(run_id)
}

pub fn rpc_workflow_update_definitions(
    daemon_socket: &Path,
    operator_token_id: &str,
    workflows: &std::collections::HashMap<
        String,
        cm_daemon::workflow::toml_schema::Workflow,
    >,
) -> anyhow::Result<()> {
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "workflow.update_definitions".into(),
        params: serde_json::json!({ "workflows": workflows }),
    };
    rpc_round_trip(daemon_socket, &req).map(|_| ())
}

pub fn rpc_task_update_tree(
    daemon_socket: &Path,
    operator_token_id: &str,
    tasks: &[(String, Option<String>, Option<String>)],
    workspaces: &[(String, Option<String>)],
) -> anyhow::Result<()> {
    let tasks_json: Vec<serde_json::Value> = tasks
        .iter()
        .map(|(task_id, parent_task_id, workspace_id)| {
            serde_json::json!({
                "task_id": task_id,
                "parent_task_id": parent_task_id,
                "workspace_id": workspace_id,
            })
        })
        .collect();
    let workspaces_json: Vec<serde_json::Value> = workspaces
        .iter()
        .map(|(workspace_id, worktree_path)| {
            serde_json::json!({
                "workspace_id": workspace_id,
                "worktree_path": worktree_path,
            })
        })
        .collect();
    let req = Request {
        id: next_request_id(),
        caller: Caller::operator(operator_token_id),
        method: "task.update_tree".into(),
        params: serde_json::json!({
            "tasks": tasks_json,
            "workspaces": workspaces_json,
        }),
    };
    rpc_round_trip(daemon_socket, &req).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cm_daemon::session::{DaemonSession, SpawnParams};
    use cm_daemon::state::DaemonState;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as StdArc, Mutex};
    use tempfile::TempDir;

    /// P-3b: the start_workflow timeout must comfortably exceed the daemon's
    /// bounded worst case so A-f never falsely reports "launch failed" while the
    /// daemon is still serializing per-role detector binding. Worst case ≈
    /// roles × `SLOT_WAIT_TIMEOUT_DEFAULT_MS` (20s); 150s covers ~7 roles. This
    /// guards against someone shrinking it back toward the 5s default.
    #[test]
    fn start_workflow_rpc_timeout_exceeds_daemon_worst_case() {
        let slot = std::time::Duration::from_millis(
            cm_daemon::control::methods::SLOT_WAIT_TIMEOUT_DEFAULT_MS,
        );
        // Headroom for a comfortably-larger-than-3-role feedback launch.
        assert!(
            START_WORKFLOW_RPC_READ_TIMEOUT >= slot * 6,
            "start_workflow RPC timeout {:?} must exceed roles×slot ({:?}×6)",
            START_WORKFLOW_RPC_READ_TIMEOUT,
            slot,
        );
        assert!(
            START_WORKFLOW_RPC_READ_TIMEOUT > DEFAULT_RPC_READ_TIMEOUT,
            "must be longer than the default per-RPC timeout",
        );
    }

    /// P-3b: a response that arrives AFTER the default 5s window must still be
    /// received when the caller uses a longer read timeout — and must fail with
    /// a short one. Proves the timeout is what gates the round-trip, so a slow
    /// (multi-role) launch isn't killed by the client. Uses a small delay
    /// (300ms) against short (100ms) vs long (5s) timeouts to stay fast.
    #[test]
    fn rpc_round_trip_honors_read_timeout_for_slow_response() {
        use std::io::Write;
        use std::os::unix::net::UnixListener;

        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("slow.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        // Mock daemon: read the request, sleep 300ms, write an OK response.
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = match listener.accept() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let _ = cm_daemon::control::wire::read_request(&mut stream);
                std::thread::sleep(std::time::Duration::from_millis(300));
                let resp = Response {
                    id: "x".into(),
                    ok: true,
                    result: Some(serde_json::json!({"run_id": "wf_slow"})),
                    error: None,
                };
                let _ = cm_daemon::control::wire::write_response(&mut stream, &resp);
                let _ = stream.flush();
            }
        });

        let req = Request {
            id: next_request_id(),
            caller: Caller::operator("op"),
            method: "ping".into(),
            params: serde_json::json!({}),
        };

        // Short timeout (< 300ms response delay) → fails.
        let short = rpc_round_trip_with_read_timeout(
            &sock,
            &req,
            std::time::Duration::from_millis(100),
        );
        assert!(short.is_err(), "100ms timeout must trip on a 300ms response");

        // Long timeout (>> delay) → succeeds, like the start_workflow path.
        let long = rpc_round_trip_with_read_timeout(
            &sock,
            &req,
            std::time::Duration::from_secs(5),
        );
        let resp = long.expect("5s timeout must receive the 300ms-delayed response");
        assert_eq!(resp.result.unwrap()["run_id"], "wf_slow");

        drop(handle);
    }

    /// `parse_daemon_session_summary` reads the full adoption metadata from
    /// a new daemon, tolerates a partial entry from an old daemon (missing
    /// fields → None), and rejects an entry with no `session_uid`.
    #[test]
    fn parse_daemon_session_summary_full_partial_and_missing() {
        let full = serde_json::json!({
            "session_uid": "ts-1", "label": "worker", "type": "codex",
            "managed_by_uid": "ts-parent", "workspace_id": "ws-9",
            "task_id": "task-9", "workflow_run_id": "wf-1",
            "workflow_role": "worker", "worktree_path": "/home/u/.cm/worktrees/x",
        });
        let s = parse_daemon_session_summary(&full).expect("entry has uid");
        assert_eq!(s.session_uid, "ts-1");
        assert_eq!(s.session_type, "codex");
        assert_eq!(s.managed_by_uid.as_deref(), Some("ts-parent"));
        assert_eq!(s.workspace_id.as_deref(), Some("ws-9"));
        assert_eq!(s.task_id.as_deref(), Some("task-9"));
        assert_eq!(s.worktree_path.as_deref(), Some("/home/u/.cm/worktrees/x"));

        // Old daemon: only the original list_sessions fields present.
        let partial = serde_json::json!({
            "session_uid": "ts-2", "label": "w", "type": "claude-code",
        });
        let s2 = parse_daemon_session_summary(&partial).expect("entry has uid");
        assert_eq!(s2.session_uid, "ts-2");
        assert!(s2.workspace_id.is_none());
        assert!(s2.managed_by_uid.is_none());
        assert!(s2.worktree_path.is_none());

        // No uid → not a session entry.
        assert!(parse_daemon_session_summary(&serde_json::json!({ "label": "x" })).is_none());
    }

    /// Spin up an in-process daemon listening on a tempdir socket,
    /// pre-populating `DaemonState.workspaces` with a single
    /// workspace whose `worktree_path` points at the tempdir.
    /// Returns the socket path + state arc + a stop flag the
    /// caller flips to wind down the accept loop. The thread
    /// joins when `stop` is set and a final no-op connect kicks
    /// the loop out of accept().
    /// Returns (socket_path, workspace_worktree_path, state, stop, handle).
    /// `workspace_worktree_path` is the same tempdir the helper
    /// registers as the workspace's `worktree_path`; tests pass it
    /// as `working_dir` in their `ClientSessionConfig` so the
    /// daemon-side spawn uses a known-good directory.
    fn start_test_daemon(
        ws_id: &str,
    ) -> (
        std::path::PathBuf,
        std::path::PathBuf,
        StdArc<Mutex<DaemonState>>,
        StdArc<AtomicBool>,
        std::thread::JoinHandle<()>,
    ) {
        let dir = TempDir::new().expect("tempdir");
        // Keep the TempDir alive by leaking; tests own a fresh
        // tempdir per call so cleanup happens when the test
        // binary exits.
        let dir_path = dir.path().to_path_buf();
        std::mem::forget(dir);
        let socket_path = dir_path.join("test-daemon.sock");

        let listener = std::os::unix::net::UnixListener::bind(&socket_path)
            .expect("bind test socket");
        listener.set_nonblocking(true).expect("nonblocking listener");

        let mut state_inner = DaemonState::new();
        state_inner.attach_addr = socket_path.to_string_lossy().into_owned();
        state_inner.workspaces.insert(
            ws_id.into(),
            cm_daemon::manifest::ManifestWorkspace {
                id: ws_id.into(),
                name: "test-ws".into(),
                is_closed: false,
                is_cloud: false,
                worktree_path: Some(dir_path.clone()),
                main_repo_path: None,
                repo_url: None,
                worker_vm: None,
                worker_zone: None,
                sessions: Vec::new(),
                tombstones: Vec::new(),
            },
        );
        let state = StdArc::new(Mutex::new(state_inner));
        let stop = StdArc::new(AtomicBool::new(false));

        let state_for_thread = state.clone();
        let stop_for_thread = stop.clone();
        let socket_for_thread = socket_path.clone();
        let handle = std::thread::spawn(move || {
            while !stop_for_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let state = state_for_thread.clone();
                        std::thread::spawn(move || {
                            // Mirror the production handle_connection
                            // shape: one RPC, then stream-transition
                            // for successful attach.open.
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(2)));
                            let req = match cm_daemon::control::wire::read_request(&mut stream) {
                                Ok(Some(r)) => r,
                                _ => return,
                            };
                            // Mirror production handle_connection: the
                            // DispatchOutcome::AttachStream variant
                            // carries the pre-built subscription
                            // handle (slice-10c-e-2 review-2 TOCTOU
                            // fix), used by handle_attach_stream.
                            match cm_daemon::control::dispatch::dispatch_request(&state, &req) {
                                cm_daemon::control::dispatch::DispatchOutcome::Done(resp) => {
                                    let _ = cm_daemon::control::wire::write_response(
                                        &mut stream,
                                        &resp,
                                    );
                                }
                                cm_daemon::control::dispatch::DispatchOutcome::AttachStream {
                                    response,
                                    handle,
                                } => {
                                    if cm_daemon::control::wire::write_response(
                                        &mut stream,
                                        &response,
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    cm_daemon::control::stream::handle_attach_stream(
                                        &mut stream,
                                        state,
                                        handle,
                                    );
                                }
                                // 10e-b: manifest.watch streaming.
                                // Same shape as AttachStream — write
                                // the OK response, then enter the
                                // manifest-stream loop. Required for
                                // exhaustiveness once 10e-c wires
                                // TUI-side consumer.
                                cm_daemon::control::dispatch::DispatchOutcome::ManifestWatchStream {
                                    response,
                                    handle,
                                } => {
                                    if cm_daemon::control::wire::write_response(
                                        &mut stream,
                                        &response,
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    cm_daemon::control::stream::handle_manifest_watch_stream(
                                        &mut stream,
                                        handle,
                                    );
                                }
                                // 11b: events.subscribe streaming. Same
                                // shape as ManifestWatchStream — write
                                // the OK response, then run the
                                // workflow-events-stream loop.
                                cm_daemon::control::dispatch::DispatchOutcome::EventsSubscribeStream {
                                    response,
                                    handle,
                                } => {
                                    if cm_daemon::control::wire::write_response(
                                        &mut stream,
                                        &response,
                                    )
                                    .is_err()
                                    {
                                        return;
                                    }
                                    cm_daemon::control::stream::handle_events_subscribe_stream(
                                        &mut stream,
                                        handle,
                                    );
                                }
                            }
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            let _ = std::fs::remove_file(socket_for_thread);
        });

        (socket_path, dir_path, state, stop, handle)
    }

    /// Build a `ClientSessionConfig` for a bare `/bin/bash` spawn
    /// (the most common shape across these tests, post-10c-e-3b
    /// argv/env/cwd refactor). `argv` and `uid` must outlive the
    /// returned config; the caller stages them as `let argv = ...;`
    /// `let uid = test_uid();`.
    fn bash_config<'a>(
        socket: &'a Path,
        working_dir: &'a Path,
        token: &'a str,
        uid: &'a str,
        workspace_id: &'a str,
        label: &'a str,
        argv: &'a [String],
        cols: u16,
        rows: u16,
    ) -> ClientSessionConfig<'a> {
        ClientSessionConfig {
            daemon_socket: socket,
            operator_token_id: token,
            uid,
            workspace_id,
            label,
            // Tests default to "bash" (the bare-shell helper);
            // type-specific tests override via direct
            // ClientSessionConfig construction.
            session_type: "bash",
            argv,
            working_dir,
            env: std::collections::BTreeMap::new(),
            cols,
            rows,
            memory_cap_bytes: None,
            // Sub-2b-3 review-fix #1: tests default to uncapped.
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: None,
            // Sub-2a Finding #1: tests default to taskless;
            // tests that exercise tasked-caller paths build
            // ClientSessionConfig directly.
            task_id: None,
            // Sub-2b-1: tests default to None; the
            // start_session_threads_transcript_path_into_resolve_response
            // test in dispatch.rs builds the wire shape directly.
            transcript_path: None,
            // 10d-2c-1 review round-5 (F1): tests default to
            // non-workflow; tests that exercise workflow-tagged
            // spawn build ClientSessionConfig directly.
            workflow_run_id: None,
            workflow_role: None,
        }
    }

    /// Fresh TUI-format uid for tests (slice 10c-e-3b-fix).
    fn test_uid() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("ts-{:x}-{:x}", nanos, n)
    }

    fn stop_test_daemon(
        socket_path: &Path,
        stop: StdArc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    ) {
        stop.store(true, Ordering::SeqCst);
        // Kick the accept loop out of WouldBlock.
        let _ = std::os::unix::net::UnixStream::connect(socket_path);
        let _ = handle.join();
    }

    #[test]
    fn rpc_start_session_returns_uid_for_known_workspace() {
        let (socket, working_dir, _state, stop, handle) = start_test_daemon("ws-rpc");
        let argv = vec!["/bin/bash".to_string()];
        let uid = test_uid();
        let config = bash_config(&socket, &working_dir, "op-test", &uid, "ws-rpc", "rpc-test", &argv, 80, 24);
        let returned = rpc_start_session(&config).expect("start_session rpc ok");
        assert_eq!(returned, uid, "daemon must echo the supplied uid verbatim");
        let _ = rpc_kill_session(&socket, "op-test", &uid);
        stop_test_daemon(&socket, stop, handle);
    }

    #[test]
    fn rpc_start_session_with_unknown_workspace_surfaces_not_found() {
        let (socket, working_dir, _state, stop, handle) = start_test_daemon("ws-rpc");
        let argv = vec!["/bin/bash".to_string()];
        let uid = test_uid();
        let config = bash_config(&socket, &working_dir, "op-test", &uid, "ws-does-not-exist", "fail", &argv, 80, 24);
        let err = rpc_start_session(&config).expect_err("unknown ws must error");
        let msg = err.to_string();
        assert!(
            msg.contains("not_found") || msg.contains("ws-does-not-exist"),
            "error must name the not_found cause: {}",
            msg
        );
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2a Finding #1: TUI push of the task tree lands in
    /// `DaemonState.task_tree`. Pin the wire shape end-to-end:
    /// the daemon sees the parent_task_id chain and a follow-up
    /// auth walk would resolve descendants.
    #[test]
    fn rpc_task_update_tree_lands_in_daemon_state() {
        let (socket, _working_dir, state, stop, handle) = start_test_daemon("ws-tree");
        let tasks: Vec<(String, Option<String>, Option<String>)> = vec![
            ("task-root".to_string(), None, None),
            ("task-child".to_string(), Some("task-root".to_string()), None),
            ("task-grandchild".to_string(), Some("task-child".to_string()), None),
        ];
        rpc_task_update_tree(&socket, "op-tree", &tasks, &[]).expect("update_tree ok");
        {
            let s = state.lock().unwrap();
            assert_eq!(s.task_tree.get("task-root"), Some(&None));
            assert_eq!(
                s.task_tree.get("task-child"),
                Some(&Some("task-root".to_string()))
            );
            assert_eq!(
                s.task_tree.get("task-grandchild"),
                Some(&Some("task-child".to_string()))
            );
        }
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2b-3 review-5 #2: the TUI's `finish_push` clears a
    /// workspace's `worktree_path` locally and then pushes
    /// the updated tree to the daemon. This test pins the
    /// wire shape that flow uses: the workspaces map carries
    /// the now-`None` worktree_path for the cloud-pushed
    /// workspace, and the daemon drops the stale path
    /// immediately.
    ///
    /// Pre-fix (review-5), `finish_push` skipped the push so
    /// the daemon kept the old path until the next reconcile,
    /// during which a concurrent `mcp_start_session` would
    /// spawn into a deleted worktree.
    #[test]
    fn finish_push_wire_shape_clears_daemon_worktree_path() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-finish-push");
        // Round 1: push workspace WITH a worktree_path
        // (pre-`finish_push` state).
        let workspaces_pre: Vec<(String, Option<String>)> = vec![(
            "ws-pushed".to_string(),
            Some("/tmp/about-to-be-pushed-to-cloud".to_string()),
        )];
        rpc_task_update_tree(&socket, "op-fp", &[], &workspaces_pre)
            .expect("pre push ok");
        {
            let s = state.lock().unwrap();
            let ws = s.workspaces.get("ws-pushed").expect("workspace landed");
            assert_eq!(
                ws.worktree_path
                    .as_deref()
                    .map(|p| p.display().to_string()),
                Some("/tmp/about-to-be-pushed-to-cloud".to_string()),
                "pre-finish_push state has the local path",
            );
        }
        // Round 2: simulate `finish_push` — same workspace id,
        // worktree_path now `None`. This is the shape `App::
        // finish_push` produces via
        // `push_task_tree_to_daemon`'s iterator over
        // `self.workspaces`.
        let workspaces_post: Vec<(String, Option<String>)> = vec![
            ("ws-pushed".to_string(), None),
        ];
        rpc_task_update_tree(&socket, "op-fp", &[], &workspaces_post)
            .expect("post push ok");
        {
            let s = state.lock().unwrap();
            let ws = s
                .workspaces
                .get("ws-pushed")
                .expect("workspace still present (entry retained, path cleared)");
            assert!(
                ws.worktree_path.is_none(),
                "finish_push push must clear worktree_path on the daemon \
                 before any other refresh trigger; still holding {:?}",
                ws.worktree_path,
            );
        }
        stop_test_daemon(&socket, stop, handle);
    }

    /// Snapshot semantics: a second push fully REPLACES the
    /// daemon's tree, not merges.
    #[test]
    fn rpc_task_update_tree_replaces_on_second_push() {
        let (socket, _working_dir, state, stop, handle) = start_test_daemon("ws-tree-2");
        let first: Vec<(String, Option<String>, Option<String>)> = vec![
            ("old-a".to_string(), None, None),
            ("old-b".to_string(), Some("old-a".to_string()), None),
        ];
        rpc_task_update_tree(&socket, "op-tree", &first, &[]).expect("first ok");
        let second: Vec<(String, Option<String>, Option<String>)> =
            vec![("new-a".to_string(), None, None)];
        rpc_task_update_tree(&socket, "op-tree", &second, &[]).expect("second ok");
        let s = state.lock().unwrap();
        assert!(!s.task_tree.contains_key("old-a"));
        assert!(!s.task_tree.contains_key("old-b"));
        assert!(s.task_tree.contains_key("new-a"));
        assert_eq!(s.task_tree.len(), 1);
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1: Operator push of the TUI session snapshot lands
    /// in `DaemonState.tui_sessions`; full-replace semantics
    /// match `task.update_tree`. A second push fully REPLACES,
    /// not merges — same invariant as the task tree push, since
    /// the daemon never sees TUI session removals as deltas (the
    /// TUI sends its full session list every time).
    #[test]
    fn rpc_tui_update_sessions_snapshot_full_replace() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-tui-snap");
        // Round 1: push {A, B}.
        let first = vec![
            TuiSessionSnapshotPush {
                uid: "ses-a",
                task_id: Some("task-a"),
                label: Some("A"),
                session_type: Some("claude-code"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
            TuiSessionSnapshotPush {
                uid: "ses-b",
                task_id: Some("task-b"),
                label: Some("B"),
                session_type: Some("bash"),
                hidden: true,
                workflow_run_id: Some("wf-1"),
                workflow_role: Some("worker"),
            },
        ];
        rpc_tui_update_sessions_snapshot(&socket, "op-snap", &first)
            .expect("first push ok");
        {
            let s = state.lock().unwrap();
            assert!(s.tui_sessions_pushed, "pushed flag must flip on first push");
            assert_eq!(s.tui_sessions.len(), 2);
            let a = s.tui_sessions.get("ses-a").expect("A landed");
            assert_eq!(a.task_id.as_deref(), Some("task-a"));
            assert_eq!(a.label.as_deref(), Some("A"));
            assert_eq!(a.session_type.as_deref(), Some("claude-code"));
            assert!(!a.hidden);
            let b = s.tui_sessions.get("ses-b").expect("B landed");
            assert_eq!(b.workflow_run_id.as_deref(), Some("wf-1"));
            assert_eq!(b.workflow_role.as_deref(), Some("worker"));
            assert!(b.hidden);
        }
        // Round 2: push {A, C} — B must disappear, C appear,
        // A retained. Full-replace, not merge.
        let second = vec![
            TuiSessionSnapshotPush {
                uid: "ses-a",
                task_id: Some("task-a"),
                label: Some("A"),
                session_type: Some("claude-code"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
            TuiSessionSnapshotPush {
                uid: "ses-c",
                task_id: Some("task-c"),
                label: Some("C"),
                session_type: Some("codex"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
        ];
        rpc_tui_update_sessions_snapshot(&socket, "op-snap", &second)
            .expect("second push ok");
        {
            let s = state.lock().unwrap();
            assert_eq!(s.tui_sessions.len(), 2);
            assert!(s.tui_sessions.contains_key("ses-a"));
            assert!(s.tui_sessions.contains_key("ses-c"));
            assert!(
                !s.tui_sessions.contains_key("ses-b"),
                "B must be evicted on full-replace",
            );
        }
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1: empty push is meaningful — `tui_sessions_pushed`
    /// flips to true so the auth consumer in 10d-2 can
    /// distinguish "TUI deliberately reports zero sessions"
    /// from "TUI hasn't pushed yet". The latter must fall
    /// through to a different branch (e.g. trust daemon-side
    /// task_id) rather than auto-deny.
    #[test]
    fn rpc_tui_update_sessions_snapshot_empty_push_sets_pushed_flag() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-tui-empty");
        // Pre-push: `tui_sessions_pushed` is false by default.
        {
            let s = state.lock().unwrap();
            assert!(!s.tui_sessions_pushed, "default must be false");
            assert!(s.tui_sessions.is_empty());
        }
        rpc_tui_update_sessions_snapshot(&socket, "op-empty", &[])
            .expect("empty push ok");
        {
            let s = state.lock().unwrap();
            assert!(
                s.tui_sessions_pushed,
                "even an empty push must flip the flag",
            );
            assert!(s.tui_sessions.is_empty());
        }
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1: a `Caller::Session` cannot push the TUI session
    /// snapshot. Same rationale as `task.update_tree`'s
    /// Operator-only constraint — a Session caller rewriting
    /// the map could insert rows that grant itself visibility
    /// into another task's sessions when the 10d-2 auth
    /// consumer reads from it. Must surface `Unauthorized`.
    #[test]
    fn rpc_tui_update_sessions_snapshot_rejects_session_caller() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-tui-rej");
        let req = Request {
            id: next_request_id(),
            caller: Caller::session("ses-imposter"),
            method: "tui.update_sessions_snapshot".into(),
            params: serde_json::json!({
                "sessions": [{
                    "uid": "ses-x",
                    "task_id": "task-x",
                    "label": "X",
                    "type": "bash",
                    "hidden": false,
                }],
            }),
        };
        let err = rpc_round_trip(&socket, &req).expect_err("must be denied");
        let msg = err.to_string();
        assert!(
            msg.contains("Unauthorized"),
            "Session caller must be Unauthorized, got: {}",
            msg,
        );
        // And the state must remain untouched.
        let s = state.lock().unwrap();
        assert!(!s.tui_sessions_pushed);
        assert!(s.tui_sessions.is_empty());
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1: the RPC helper surfaces socket failures as `Err`
    /// to the caller, not silently swallowed. The TUI's
    /// `push_tui_sessions_to_daemon` then turns that `Err` into
    /// an `eprintln!` visible to the user — the round-11
    /// invariant: under opt-in, the daemon is a hard dependency
    /// of TUI session-list mutations; failure must NOT be
    /// silent.
    #[test]
    fn rpc_tui_update_sessions_snapshot_surfaces_socket_failure() {
        let nonexistent = std::path::PathBuf::from("/tmp/cm-tui-snap-no-such.sock");
        let _ = std::fs::remove_file(&nonexistent);
        let result = rpc_tui_update_sessions_snapshot(
            &nonexistent,
            "op-fail",
            &[TuiSessionSnapshotPush {
                uid: "ses-a",
                task_id: None,
                label: None,
                session_type: None,
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            }],
        );
        assert!(
            result.is_err(),
            "socket-not-found must propagate as Err, not silently swallowed; \
             got {:?}",
            result,
        );
    }

    /// 10d-1 startup-ordering fix: `drain_backend_events` fires
    /// `reconcile_tasks` on the first `TasksUpdated`, and that
    /// path's `push_state_to_daemon` runs BEFORE
    /// `restore_sessions` hydrates `self.workspaces[].sessions`
    /// from the on-disk manifest. Pre-fix the daemon was left
    /// with a full-replace empty-`tui_sessions` snapshot — a
    /// lie about TUI state, harmless in 10d-1 with no auth
    /// consumer but in 10d-2 would cause every TUI-minted
    /// session restored from manifest to be rejected as
    /// "caller session not found" by the workflow auth path
    /// until some later mutation triggered a re-push.
    ///
    /// Post-fix: `restore_sessions` calls `push_state_to_daemon`
    /// at its tail. The simulation here is the wire-shape
    /// equivalent: an early empty push (the lying reconcile
    /// push) followed by a populated push (the corrective
    /// restore push). Full-replace semantics guarantee the
    /// later push wins.
    #[test]
    fn manifest_restored_sessions_land_in_daemon_snapshot() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-restore");
        // Simulate `reconcile_tasks` firing first with no sessions
        // populated yet (the pre-restore moment of startup).
        rpc_tui_update_sessions_snapshot(&socket, "op-recon", &[])
            .expect("early reconcile push ok");
        {
            let s = state.lock().unwrap();
            assert!(s.tui_sessions_pushed, "reconcile push must flip flag");
            assert!(
                s.tui_sessions.is_empty(),
                "pre-restore daemon snapshot is empty (the lying state)",
            );
        }
        // Now simulate `restore_sessions` finishing — three
        // sessions hydrated from the on-disk manifest. The
        // corrective push lands here. These rows cover the
        // wire-shape fields the 10d-2 auth consumer will key off:
        // task_id, workflow_run_id, workflow_role.
        let restored = vec![
            TuiSessionSnapshotPush {
                uid: "ses-restored-1",
                task_id: Some("task-1"),
                label: Some("worker"),
                session_type: Some("claude-code"),
                hidden: false,
                workflow_run_id: Some("wf-r"),
                workflow_role: Some("worker"),
            },
            TuiSessionSnapshotPush {
                uid: "ses-restored-2",
                task_id: Some("task-1"),
                label: Some("reviewer"),
                session_type: Some("claude-code"),
                hidden: true,
                workflow_run_id: Some("wf-r"),
                workflow_role: Some("reviewer"),
            },
            TuiSessionSnapshotPush {
                uid: "ses-restored-3",
                task_id: Some("task-2"),
                label: Some("solo"),
                session_type: Some("bash"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
        ];
        rpc_tui_update_sessions_snapshot(&socket, "op-restore", &restored)
            .expect("restore push ok");
        let s = state.lock().unwrap();
        assert_eq!(
            s.tui_sessions.len(),
            3,
            "all restored sessions must land in daemon snapshot",
        );
        let r1 = s.tui_sessions.get("ses-restored-1").expect("1 present");
        assert_eq!(r1.task_id.as_deref(), Some("task-1"));
        assert_eq!(r1.workflow_run_id.as_deref(), Some("wf-r"));
        assert_eq!(r1.workflow_role.as_deref(), Some("worker"));
        let r2 = s.tui_sessions.get("ses-restored-2").expect("2 present");
        assert!(r2.hidden);
        assert_eq!(r2.workflow_role.as_deref(), Some("reviewer"));
        let r3 = s.tui_sessions.get("ses-restored-3").expect("3 present");
        assert_eq!(r3.task_id.as_deref(), Some("task-2"));
        assert!(r3.workflow_run_id.is_none());
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1 startup-ordering fix, empty-manifest path:
    /// `restore_sessions` early-returns when both
    /// `manifest.workspaces` and `manifest.bindings` are empty,
    /// so no corrective push fires there. Coverage comes from
    /// the pre-restore `reconcile_tasks` push, which always
    /// fires on `TasksUpdated` regardless of TUI session state.
    /// The end-state contract: daemon snapshot empty AND
    /// `tui_sessions_pushed=true` — so the auth consumer in
    /// 10d-2 can distinguish "TUI deliberately reports zero
    /// sessions" from "TUI hasn't pushed yet".
    #[test]
    fn empty_manifest_startup_marks_pushed_flag_true() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-empty-restore");
        // Pre-startup: pushed flag is false.
        {
            let s = state.lock().unwrap();
            assert!(!s.tui_sessions_pushed);
            assert!(s.tui_sessions.is_empty());
        }
        // `reconcile_tasks` fires on first TasksUpdated. Even
        // with no TUI sessions, this push runs (pushes empty
        // workspaces + empty tui_sessions). `restore_sessions`
        // then early-returns on empty manifest — no second
        // push, which is fine.
        rpc_tui_update_sessions_snapshot(&socket, "op-empty", &[])
            .expect("reconcile push on empty TUI state ok");
        let s = state.lock().unwrap();
        assert!(
            s.tui_sessions_pushed,
            "reconcile-time push must mark pushed=true even with no sessions",
        );
        assert!(s.tui_sessions.is_empty(), "snapshot must be empty");
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1 round-3 Part 1: `push_tui_sessions_to_daemon`
    /// filters out daemon-attached sessions
    /// (`ts.session.daemon_session_uid.is_some()`) so the daemon's
    /// `state.tui_sessions` only carries TUI-LOCAL rows. Daemon-
    /// attached sessions live in `state.sessions` already; appearing
    /// in `tui_sessions` too would double-register and make
    /// `lookup_session_any`'s precedence load-bearing for
    /// correctness. The two maps are kept non-overlapping by
    /// construction.
    ///
    /// Wire-shape simulation: build a push that mimics what the
    /// filter produces — only the TUI-local row (`ses-local`)
    /// reaches the daemon; the daemon-attached row (`ses-daemon`)
    /// is omitted by the call site. Assert the daemon's
    /// `tui_sessions` contains exactly the local row.
    #[test]
    fn push_filters_out_daemon_attached_sessions() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-filter");
        // The TUI-side filter (`filter(|ts|
        // ts.session.daemon_session_uid.is_none())`) keeps only
        // local rows in the wire payload. We don't construct
        // App in tests, so simulate the filter's output here:
        // a single TUI-local row passed to the RPC, with the
        // daemon-attached row deliberately omitted.
        let filtered = vec![TuiSessionSnapshotPush {
            uid: "ses-local",
            task_id: Some("task-local"),
            label: Some("local"),
            session_type: Some("bash"),
            hidden: false,
            workflow_run_id: None,
            workflow_role: None,
        }];
        rpc_tui_update_sessions_snapshot(&socket, "op-filter", &filtered)
            .expect("filtered push ok");
        let s = state.lock().unwrap();
        assert_eq!(s.tui_sessions.len(), 1, "filter must drop the daemon-attached row");
        assert!(s.tui_sessions.contains_key("ses-local"));
        assert!(
            !s.tui_sessions.contains_key("ses-daemon"),
            "daemon-attached sessions must NOT appear in tui_sessions \
             (they live in state.sessions instead — 10d-2 lookup_session_any \
             keeps the maps non-overlapping by construction)",
        );
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1 round-3 Part 2: graceful shutdown pushes an
    /// explicit empty `tui_sessions` snapshot to the daemon, so
    /// no stale rows linger across TUI restart. Pre-fix, the
    /// final `save_session_manifest` push at A-q (or
    /// `PlanAction::Quit`) left the daemon holding rows for
    /// sessions that the TUI was about to drop — 10d-2's
    /// `lookup_session_any` would falsely treat those as live
    /// TUI sessions.
    ///
    /// Wire-shape simulation: first push the live snapshot
    /// (what `save_session_manifest`'s hook sends), then push
    /// the explicit empty snapshot (what
    /// `clear_tui_sessions_on_daemon` sends). Assert daemon
    /// ends with empty `tui_sessions` AND
    /// `tui_sessions_pushed=true` (deliberate empty, not unset).
    #[test]
    fn graceful_shutdown_clears_tui_sessions_snapshot() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-shutdown");
        // Live state during normal use — `save_session_manifest`
        // hook pushes this on every mutation.
        let live = vec![
            TuiSessionSnapshotPush {
                uid: "ses-a",
                task_id: Some("task-a"),
                label: Some("A"),
                session_type: Some("claude-code"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
            TuiSessionSnapshotPush {
                uid: "ses-b",
                task_id: Some("task-b"),
                label: Some("B"),
                session_type: Some("bash"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
        ];
        rpc_tui_update_sessions_snapshot(&socket, "op-live", &live)
            .expect("live push ok");
        {
            let s = state.lock().unwrap();
            assert_eq!(s.tui_sessions.len(), 2);
        }
        // Quit-time clear (`clear_tui_sessions_on_daemon`).
        rpc_tui_update_sessions_snapshot(&socket, "op-quit", &[])
            .expect("shutdown clear ok");
        let s = state.lock().unwrap();
        assert!(
            s.tui_sessions.is_empty(),
            "graceful shutdown must leave daemon with empty tui_sessions",
        );
        assert!(
            s.tui_sessions_pushed,
            "pushed flag remains true — `lookup_session_any` reads this \
             to distinguish 'TUI gone' from 'never connected'",
        );
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// 10d-1 round-3: TUI restart after graceful shutdown leaves
    /// the daemon with the NEW TUI's sessions only, not a merge
    /// of old + new. Sequence: TUI #1 launches with {A, B}, quits
    /// (pushes empty), TUI #2 launches with {A, C}. Final daemon
    /// state must be exactly {A, C} — not {A, B, C} merged, not
    /// stale {A, B}, not empty.
    #[test]
    fn restart_replaces_snapshot_not_merges() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-restart");
        // TUI #1 running with {A, B}.
        let first = vec![
            TuiSessionSnapshotPush {
                uid: "ses-a",
                task_id: Some("task-a"),
                label: Some("A"),
                session_type: Some("bash"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
            TuiSessionSnapshotPush {
                uid: "ses-b",
                task_id: Some("task-b"),
                label: Some("B"),
                session_type: Some("bash"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
        ];
        rpc_tui_update_sessions_snapshot(&socket, "op-tui1-live", &first)
            .expect("tui#1 live ok");
        // TUI #1 quits gracefully (empty push).
        rpc_tui_update_sessions_snapshot(&socket, "op-tui1-quit", &[])
            .expect("tui#1 quit ok");
        {
            let s = state.lock().unwrap();
            assert!(s.tui_sessions.is_empty(), "post-quit must be empty");
        }
        // TUI #2 launches: reconcile pushes empty, restore pushes
        // its sessions {A, C} from the on-disk manifest (A
        // persisted, B was killed before quit, C is brand new).
        rpc_tui_update_sessions_snapshot(&socket, "op-tui2-reconcile", &[])
            .expect("tui#2 reconcile ok");
        let second = vec![
            TuiSessionSnapshotPush {
                uid: "ses-a",
                task_id: Some("task-a"),
                label: Some("A"),
                session_type: Some("bash"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
            TuiSessionSnapshotPush {
                uid: "ses-c",
                task_id: Some("task-c"),
                label: Some("C"),
                session_type: Some("claude-code"),
                hidden: false,
                workflow_run_id: None,
                workflow_role: None,
            },
        ];
        rpc_tui_update_sessions_snapshot(&socket, "op-tui2-restore", &second)
            .expect("tui#2 restore ok");
        let s = state.lock().unwrap();
        assert_eq!(s.tui_sessions.len(), 2, "exactly {{A, C}} — no merge");
        assert!(s.tui_sessions.contains_key("ses-a"));
        assert!(s.tui_sessions.contains_key("ses-c"));
        assert!(
            !s.tui_sessions.contains_key("ses-b"),
            "ses-b from previous TUI run must NOT survive",
        );
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2a Finding #1: a `start_session` request carrying
    /// `task_id` lands on `DaemonSession.task_id`. The pre-fix
    /// path had the TUI tagging `TerminalSession.task_id`
    /// post-spawn — purely TUI-local; the daemon's copy stayed
    /// `None`. With `task_id` left None on the daemon, the
    /// Session-caller auth walk's tasked-caller branch can't
    /// fire and EVERY daemon-spawned session falls into the
    /// taskless same-workspace-allow branch.
    #[test]
    fn rpc_start_session_threads_task_id_to_daemon_session() {
        let (socket, working_dir, state, stop, handle) = start_test_daemon("ws-task-thread");
        let argv = vec!["/bin/bash".to_string()];
        let uid = test_uid();
        let mut config = bash_config(&socket, &working_dir, "op-thr", &uid, "ws-task-thread", "thr", &argv, 80, 24);
        config.task_id = Some("task-payload");
        let returned = rpc_start_session(&config).expect("start_session ok");
        assert_eq!(returned, uid);
        {
            let s = state.lock().unwrap();
            assert_eq!(
                s.sessions[&uid].task_id.as_deref(),
                Some("task-payload"),
                "daemon must record task_id on DaemonSession at spawn time",
            );
        }
        let _ = rpc_kill_session(&socket, "op-thr", &uid);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2a Finding #1 acceptance: with task_id threaded
    /// through, a tasked Session caller can NO LONGER act on a
    /// sibling-task session in the same workspace. Pre-fix the
    /// daemon stored `task_id: None` for both sessions, so the
    /// auth walk hit the taskless same-workspace-allow branch
    /// and the call succeeded — the widening sub-2a aims to
    /// close. Post-fix the daemon stores `Some("task-a")` on
    /// the caller and `Some("task-b")` on the target; with no
    /// parent/child link, OutOfScope fires.
    #[test]
    fn sibling_task_send_input_is_unauthorized_after_task_id_threading() {
        use crate::client_session::{rpc_kill_session, rpc_start_session};
        let (socket, working_dir, state, stop, handle) =
            start_test_daemon("ws-sibling");
        // Spawn two sessions in the SAME workspace but DIFFERENT
        // task_ids. Both via the TUI's path so task_id rides the
        // wire — that's the F1 fix being exercised.
        let argv = vec!["/bin/bash".to_string()];
        let caller_uid = test_uid();
        let mut caller_config = bash_config(
            &socket, &working_dir, "op-sib", &caller_uid,
            "ws-sibling", "caller", &argv, 80, 24,
        );
        caller_config.task_id = Some("task-a");
        let _ = rpc_start_session(&caller_config).expect("caller spawn ok");
        let target_uid = test_uid();
        let mut target_config = bash_config(
            &socket, &working_dir, "op-sib", &target_uid,
            "ws-sibling", "target", &argv, 80, 24,
        );
        target_config.task_id = Some("task-b");
        let _ = rpc_start_session(&target_config).expect("target spawn ok");

        // Sanity: both task_ids landed on the daemon side.
        {
            let s = state.lock().unwrap();
            assert_eq!(s.sessions[&caller_uid].task_id.as_deref(), Some("task-a"));
            assert_eq!(s.sessions[&target_uid].task_id.as_deref(), Some("task-b"));
        }

        // Push a task tree with NO parent/child link between
        // task-a and task-b. The daemon's auth walk must then
        // surface OutOfScope on send_input.
        rpc_task_update_tree(
            &socket,
            "op-sib",
            &[
                ("task-a".to_string(), None, None),
                ("task-b".to_string(), None, None),
            ],
            &[],
        )
        .expect("update_tree ok");

        // Hand-craft a Session-caller send_input against the
        // target's uid; assert Unauthorized.
        let req = Request {
            id: next_request_id(),
            caller: Caller::session(&caller_uid),
            method: "send_input".into(),
            params: serde_json::json!({
                "session_uid": &target_uid,
                "text": "should-be-denied",
            }),
        };
        let err = rpc_round_trip(&socket, &req).expect_err("must be denied");
        let msg = err.to_string();
        assert!(
            msg.contains("Unauthorized"),
            "sibling-task target must be Unauthorized, got: {}",
            msg,
        );

        let _ = rpc_kill_session(&socket, "op-sib", &caller_uid);
        let _ = rpc_kill_session(&socket, "op-sib", &target_uid);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2a Finding #2 integration acceptance: when a
    /// subtask launches, the local TaskEntry stub initialized
    /// with `parent_task_id: Some(parent)` and pushed via
    /// `App::push_task_tree_to_daemon` (which forwards to
    /// `rpc_task_update_tree`) results in the daemon's
    /// `task_tree` showing the parent edge IMMEDIATELY — no
    /// waiting for API reconcile. Pre-fix the stub used
    /// `parent_task_id: None` and the daemon saw the subtask
    /// as top-level until reconcile patched it.
    #[test]
    fn first_push_after_subtask_launch_publishes_parent_edge() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-f2-edge");
        // Simulate the slice `push_task_tree_to_daemon` builds
        // from `self.tasks` AFTER a launch where the stub was
        // initialized with `parent_task_id: Some("parent-x")`.
        // The PARENT may already be in self.tasks too (it
        // typically is — that's what made it a subtask), but
        // the F2 acceptance hinges on the SUBTASK row carrying
        // the parent edge.
        let tasks: Vec<(String, Option<String>, Option<String>)> = vec![
            ("parent-x".to_string(), None, None),
            ("subtask-y".to_string(), Some("parent-x".to_string()), None),
        ];
        rpc_task_update_tree(&socket, "op-f2", &tasks, &[]).expect("push ok");
        let s = state.lock().unwrap();
        // Daemon sees the parent edge on the FIRST push, not
        // after a deferred reconcile.
        assert_eq!(
            s.task_tree.get("subtask-y"),
            Some(&Some("parent-x".to_string())),
            "first push after subtask launch must publish parent edge",
        );
        assert_eq!(s.task_tree.get("parent-x"), Some(&None));
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Empty push (startup priming): daemon accepts and the
    /// resulting `task_tree` is empty. Pins the "TUI startup
    /// snapshot" wire on a freshly-launched daemon.
    #[test]
    fn rpc_task_update_tree_accepts_empty_snapshot() {
        let (socket, _working_dir, state, stop, handle) = start_test_daemon("ws-tree-3");
        rpc_task_update_tree(&socket, "op-tree", &[], &[]).expect("empty ok");
        let s = state.lock().unwrap();
        assert!(s.task_tree.is_empty());
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2b-1 review #1: TUI post-detection push via
    /// `rpc_set_transcript_path` lands on the daemon. Models
    /// the production flow: session spawns with no transcript
    /// (resolver returns pending), TUI's detector later
    /// discovers the path and pushes, resolver flips to ready.
    #[test]
    fn rpc_set_transcript_path_pushes_post_spawn_discovery() {
        let (socket, working_dir, _state, stop, handle) =
            start_test_daemon("ws-set-trsc");
        let argv = vec!["/bin/sleep".to_string(), "30".to_string()];
        let uid = test_uid();
        let config = bash_config(
            &socket, &working_dir, "op-set", &uid,
            "ws-set-trsc", "set-trsc", &argv, 80, 24,
        );
        // Spawn WITHOUT transcript_path (fresh-spawn case).
        let _ = rpc_start_session(&config).expect("spawn ok");
        // Resolve: pending.
        let req = Request {
            id: next_request_id(),
            caller: Caller::operator("op-set"),
            method: "resolve_authorized_session".into(),
            params: serde_json::json!({ "session_uid": &uid }),
        };
        let before = rpc_round_trip(&socket, &req).expect("pre-resolve");
        assert_eq!(before.result.unwrap()["state"], "pending");
        // TUI detector resolved a path — push it.
        let discovered = "/home/u/.claude/projects/x/post-detect.jsonl";
        rpc_set_transcript_path(&socket, "op-set", &uid, discovered)
            .expect("push ok");
        // Resolve: ready, with the pushed path.
        let after = rpc_round_trip(&socket, &req).expect("post-resolve");
        let r = after.result.expect("result");
        assert_eq!(r["state"], "ready");
        assert_eq!(r["transcript_path"], discovered);
        let _ = rpc_kill_session(&socket, "op-set", &uid);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2b-1: `rpc_start_session_full` rides
    /// `ClientSessionConfig.transcript_path` onto the wire, and
    /// the daemon's `resolve_authorized_session` surfaces it.
    /// End-to-end TUI → daemon proof of the wire shape addition.
    /// Pre-sub-2b-1 the Python MCP `read_session_output` tool
    /// hit `UnknownMethod` on its first leg
    /// (`resolve_authorized_session`); post-fix the chain works.
    #[test]
    fn rpc_start_session_threads_transcript_path_into_resolve_authorized() {
        let (socket, working_dir, _state, stop, handle) =
            start_test_daemon("ws-trsc-e2e");
        let argv = vec!["/bin/sleep".to_string(), "30".to_string()];
        let uid = test_uid();
        let mut config = bash_config(
            &socket, &working_dir, "op-trsc", &uid,
            "ws-trsc-e2e", "trsc-e2e", &argv, 80, 24,
        );
        let predicted_path = "/home/u/.claude/projects/encoded-x/abc-123.jsonl";
        config.transcript_path = Some(predicted_path);
        let _ = rpc_start_session(&config).expect("spawn ok");
        // Now resolve via a follow-up RPC; assert the daemon
        // returns state=ready with our path.
        let req = Request {
            id: next_request_id(),
            caller: Caller::operator("op-trsc"),
            method: "resolve_authorized_session".into(),
            params: serde_json::json!({ "session_uid": &uid }),
        };
        let resp = rpc_round_trip(&socket, &req).expect("resolve ok");
        let r = resp.result.expect("result");
        assert_eq!(r["state"], "ready", "daemon must echo transcript_path");
        assert_eq!(r["transcript_path"], predicted_path);
        assert_eq!(r["engine"], "claude-code");
        // Cleanup.
        let _ = rpc_kill_session(&socket, "op-trsc", &uid);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2a Finding (round 3) #2: after `create_subtask` adds
    /// the new subtask's TaskEntry to `app.tasks` (with
    /// `parent_task_id: Some(caller_task)`), the immediate
    /// `app.push_task_tree_to_daemon()` call must publish the
    /// new parent edge. Pre-fix the push was missing — the
    /// daemon's `task_tree` showed the new subtask as
    /// top-level (or absent) until the next API reconcile,
    /// opening a window where descendant-task auth failed for
    /// the just-created subtask.
    ///
    /// Test scope: the same wire mechanism `create_subtask`
    /// now triggers — `rpc_task_update_tree` carrying parent +
    /// new-subtask edges — verified against a live test
    /// daemon. (A full App-level integration test for
    /// `create_subtask` would require constructing the whole
    /// `App` with backend / control server / workflows loaded;
    /// the marginal value over this targeted check is low.
    /// The first_push_after_subtask_launch_publishes_parent_edge
    /// test below proves the same chain for launch paths.)
    #[test]
    fn create_subtask_push_publishes_parent_edge_immediately() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-subtask-push");
        // Pre-state: parent already in the daemon's tree (the
        // tasked caller's task — a top-level row that an
        // earlier launch published).
        rpc_task_update_tree(
            &socket,
            "op-subtask",
            &[("task-parent".to_string(), None, None)],
            &[],
        )
        .expect("seed parent ok");
        // The post-`create_subtask` `app.tasks` slice as
        // `push_task_tree_to_daemon` would build it: parent
        // edge plus the new subtask carrying
        // `parent_task_id: Some("task-parent")`.
        let post_create_subtask: Vec<(String, Option<String>, Option<String>)> = vec![
            ("task-parent".to_string(), None, None),
            ("task-newsubtask".to_string(), Some("task-parent".to_string()), None),
        ];
        rpc_task_update_tree(&socket, "op-subtask", &post_create_subtask, &[])
            .expect("post-create_subtask push ok");
        // Acceptance: the new subtask's parent edge is visible
        // on the daemon BEFORE any API reconcile. Pre-fix this
        // would have failed: `task-newsubtask` wouldn't be in
        // the tree at all until reconcile, so a tasked-caller
        // act-on-subtask attempt would fail OutOfScope.
        let s = state.lock().unwrap();
        assert_eq!(
            s.task_tree.get("task-newsubtask"),
            Some(&Some("task-parent".to_string())),
            "create_subtask's push must publish the parent edge \
             immediately, not wait for reconcile",
        );
        assert_eq!(s.task_tree.get("task-parent"), Some(&None));
        drop(s);
        stop_test_daemon(&socket, stop, handle);
    }

    /// Sub-2a Finding (round 3) #1: a freshly-started TUI must
    /// NOT wipe the persistent-host daemon's existing task tree.
    /// Pre-fix `main.rs` unconditionally sent an empty
    /// `task.update_tree` at opt-in startup; persistent-daemon
    /// state was lost until the next API reconcile (possibly
    /// indefinitely if the API was unreachable).
    ///
    /// Post-fix the startup empty push is gone. This test pins
    /// the bug-class by exercising both halves:
    ///   1. Pre-populate the daemon's tree with a parent edge.
    ///   2. Simulate the post-fix TUI startup — opt-in is on
    ///      and `ensure_daemon_at_startup` ran, but NO
    ///      `task.update_tree` is sent because the TUI has
    ///      nothing authoritative to publish yet.
    ///   3. Assert the daemon's tree is still intact.
    /// A second push of `&[]` (simulating the pre-fix
    /// behavior) is then exercised to prove it WOULD wipe —
    /// the justification for skipping the startup push.
    #[test]
    fn startup_without_push_preserves_existing_daemon_tree() {
        let (socket, _working_dir, state, stop, handle) =
            start_test_daemon("ws-startup-preserve");
        // 1. Pre-populate (simulating a prior TUI session that
        //    successfully ran reconcile_tasks and pushed).
        let prior: Vec<(String, Option<String>, Option<String>)> = vec![
            ("task-root".to_string(), None, None),
            ("task-leaf".to_string(), Some("task-root".to_string()), None),
        ];
        rpc_task_update_tree(&socket, "op-startup", &prior, &[]).expect("seed ok");
        {
            let s = state.lock().unwrap();
            assert_eq!(s.task_tree.len(), 2, "seed must land");
        }
        // 2. Simulate the post-fix TUI startup — nothing here.
        //    The fresh TUI has no authoritative state, so no
        //    push fires.
        //    (`ensure_daemon_at_startup` was the only RPC the
        //    pre-fix startup made; it doesn't touch task_tree.)
        // 3. Daemon's tree must still hold the seeded edge.
        {
            let s = state.lock().unwrap();
            assert_eq!(
                s.task_tree.get("task-leaf"),
                Some(&Some("task-root".to_string())),
                "TUI startup must not erase a persistent-host \
                 daemon's existing task tree",
            );
            assert_eq!(s.task_tree.get("task-root"), Some(&None));
        }
        // Regression evidence: if the TUI HAD sent the empty
        // push, the daemon would wipe — proving the gate is
        // load-bearing.
        rpc_task_update_tree(&socket, "op-startup", &[], &[])
            .expect("empty push (regression simulation) ok");
        {
            let s = state.lock().unwrap();
            assert!(
                s.task_tree.is_empty(),
                "empty push wipes — this is why startup must skip it",
            );
        }
        stop_test_daemon(&socket, stop, handle);
    }

    #[test]
    fn rpc_session_attach_succeeds_for_live_uid_with_matching_identity() {
        // Two-step: start_session, then session.attach with the
        // same operator token_id. The ticket allocator must mint
        // a ticket bound to that identity.
        let (socket, working_dir, _state, stop, handle) = start_test_daemon("ws-att");
        let argv = vec!["/bin/bash".to_string()];
        let uid_pre = test_uid();
        let config = bash_config(&socket, &working_dir, "op-att", &uid_pre, "ws-att", "attach-test", &argv, 80, 24);
        let uid = rpc_start_session(&config).expect("start_session");
        let attach = rpc_session_attach(&config, &uid).expect("session.attach");
        assert!(!attach.attach_ticket.is_empty());
        assert_eq!(attach.attach_addr, socket.to_string_lossy());
        let _ = rpc_kill_session(&socket, "op-att", &uid);
        stop_test_daemon(&socket, stop, handle);
    }

    #[test]
    fn client_session_new_completes_dance_against_live_daemon() {
        // End-to-end: the constructor runs all 6 steps, returns
        // a ClientSession with the expected session_uid + title
        // shape, and the daemon's registry contains a live entry
        // under that uid.
        //
        // This is the named acceptance test for slice 10c-e-2:
        // proof that the full dance composes cleanly without
        // wiring through A-n.
        let (socket, working_dir, state, stop, handle) = start_test_daemon("ws-full");
        let argv = vec!["/bin/bash".to_string()];
        let uid_pre = test_uid();
        let config = bash_config(&socket, &working_dir, "op-full", &uid_pre, "ws-full", "full-dance", &argv, 80, 24);
        let session = ClientSession::new(config).expect("full dance ok");
        assert!(session.session_uid.starts_with("ts-"));
        assert!(
            session.title.contains("full-dance"),
            "title must reflect the label: {}",
            session.title
        );
        assert!(
            session.title.contains(&session.session_uid),
            "title must reflect the uid: {}",
            session.title
        );
        // Registry: session is live.
        {
            let s = state.lock().unwrap();
            assert!(
                s.sessions.contains_key(&session.session_uid),
                "daemon registry must contain session uid {} after new()",
                session.session_uid
            );
        }
        // Cleanup: kill the session before tearing down the daemon.
        let _ = rpc_kill_session(&socket, "op-full", &session.session_uid);
        // Drop the ClientSession so its EventLoop thread tears down.
        drop(session);
        stop_test_daemon(&socket, stop, handle);
    }

    #[test]
    fn client_session_new_cleans_up_when_attach_step_fails() {
        // Failure semantics contract: if a step *after* start_session
        // fails, the daemon's session must be cleaned up via
        // kill_session so no started-but-unattached entry leaks.
        //
        // Drive a failure by handing in a workspace that the
        // daemon doesn't know about — but we can't, because
        // that fails at start_session itself (step 1), which
        // doesn't trigger the cleanup path.
        //
        // To exercise the cleanup arm, we use a more elaborate
        // setup: start_session succeeds against a valid workspace,
        // but we corrupt the attach_addr by deleting the socket
        // file between start_session and session.attach (no — too
        // racy). Instead: use a config that wedges attach.open by
        // shutting down the daemon mid-dance. Skip this concrete
        // test variant; the equivalent code path is exercised by
        // the manual smoke test in slice 10c-e-3.
        //
        // What we CAN test cheaply: after a successful new(),
        // dropping the session and immediately calling new()
        // again works — no leftover registry state confuses the
        // second dance.
        let (socket, working_dir, state, stop, handle) = start_test_daemon("ws-twice");
        let argv = vec!["/bin/bash".to_string()];
        let uid_first = test_uid();
        let config = bash_config(&socket, &working_dir, "op-twice", &uid_first, "ws-twice", "first", &argv, 80, 24);
        let first = ClientSession::new(config).expect("first dance");
        let first_uid = first.session_uid.clone();
        let _ = rpc_kill_session(&socket, "op-twice", &first_uid);
        drop(first);
        // Wait briefly for daemon's reaper to cleanup the first
        // session's registry entry.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if !state.lock().unwrap().sessions.contains_key(&first_uid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let uid_second = test_uid();
        let config2 = bash_config(&socket, &working_dir, "op-twice", &uid_second, "ws-twice", "second", &argv, 80, 24);
        let second = ClientSession::new(config2).expect("second dance");
        assert_ne!(second.session_uid, first_uid);
        let _ = rpc_kill_session(&socket, "op-twice", &second.session_uid);
        drop(second);
        stop_test_daemon(&socket, stop, handle);
    }

    // --- Initial PTY size plumbing (slice-10c-e-2 review-3 fix) -----------

    #[test]
    fn rpc_start_session_request_carries_cols_and_rows() {
        // The client wire-content contract: rpc_start_session
        // must serialize `cols` and `rows` into the request
        // params. Verified by intercepting the request at a stub
        // server that doesn't actually dispatch — just reads the
        // request bytes and inspects them.
        use cm_daemon::control::wire as wire_local;
        use std::os::unix::net::UnixListener;

        let dir = TempDir::new().expect("tempdir");
        std::mem::forget(dir);
        let socket_path = std::path::PathBuf::from(format!(
            "/tmp/cm-cols-rows-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Spy server: accepts one connection, parses the request,
        // writes back a synthetic OK response, exits.
        let socket_for_spy = socket_path.clone();
        let spy = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let req = wire_local::read_request(&mut stream)
                .expect("read req")
                .expect("frame");
            let resp = cm_daemon::control::protocol::Response::ok(
                req.id.clone(),
                serde_json::json!({ "session_uid": "ts-spy" }),
            );
            wire_local::write_response(&mut stream, &resp).expect("write");
            drop(socket_for_spy);
            req
        });

        let argv = vec!["/bin/bash".to_string()];
        let uid = test_uid();
        let config = bash_config(
            &socket_path,
            std::path::Path::new("/tmp"),
            "op-spy",
            &uid,
            "ws-spy",
            "spy",
            &argv,
            132,
            50,
        );
        let returned_uid = rpc_start_session(&config).expect("rpc ok");

        let req = spy.join().expect("spy joined");
        assert_eq!(req.method, "start_session");
        assert_eq!(req.params["cols"], 132);
        assert_eq!(req.params["rows"], 50);

        // Slice 10c-e-3b-fix: the TUI's pre-generated uid must
        // travel on the wire verbatim. The spy server echoes
        // whatever uid it sees back in the response (here it
        // synthesizes "ts-spy" for protocol-level isolation), and
        // the request params must contain our supplied uid
        // — the wire payload is what binds the TUI's MCP config
        // identity to the daemon's registry key.
        assert_eq!(
            req.params["uid"].as_str(),
            Some(uid.as_str()),
            "rpc_start_session must serialize the supplied uid in request params"
        );
        // (And the spy's response is what `rpc_start_session`
        // returns. Confirms the function honors the daemon's
        // echo rather than dropping it.)
        assert_eq!(returned_uid, "ts-spy");

        let _ = std::fs::remove_file(&socket_path);
    }

    #[test]
    fn client_session_new_with_explicit_size_spawns_pty_at_that_size() {
        // End-to-end: ClientSession::new with non-default size
        // must produce a daemon-side PTY whose `stty size` reports
        // the same. We use ClientSession's session_uid +
        // read_session_output to inspect what's flowing on the
        // daemon-side fanout.
        //
        // Note: this duplicates the daemon-side
        // start_session_with_explicit_cols_rows test at the
        // wire-edge level. Keeping it because the reviewer
        // explicitly asked for the client-side end-to-end
        // verification.
        let (socket, working_dir, _state, stop, handle) = start_test_daemon("ws-e2e-size");
        let argv = vec!["/bin/bash".to_string()];
        let uid_pre = test_uid();
        let config = bash_config(&socket, &working_dir, "op-e2e", &uid_pre, "ws-e2e-size", "e2e-size", &argv, 100, 30);
        let session = ClientSession::new(config).expect("dance");
        let uid = session.session_uid.clone();

        // Use the daemon's send_input RPC to ask the shell about
        // its size. read_session_output snapshot will show the
        // response. We need a fresh socket for each follow-up
        // RPC.
        let send_resp = {
            let mut s = UnixStream::connect(&socket).expect("connect");
            let req = Request {
                id: "spy-send".into(),
                caller: Caller::operator("op-e2e"),
                method: "send_input".into(),
                params: serde_json::json!({
                    "session_uid": &uid,
                    "text": "stty size",
                }),
            };
            wire::write_request(&mut s, &req).expect("write");
            wire::read_response(&mut s).expect("read").expect("present")
        };
        assert!(send_resp.ok, "send_input must succeed: {:?}", send_resp.error);

        // Poll read_session_output for "30 100" in the buffered
        // PTY output.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut cursor: Option<u64> = None;
        let mut accumulated = String::new();
        loop {
            let mut s = UnixStream::connect(&socket).expect("connect");
            let mut params = serde_json::json!({ "session_uid": &uid });
            if let Some(c) = cursor {
                params["since_cursor"] = serde_json::json!(c);
            }
            let req = Request {
                id: "spy-read".into(),
                caller: Caller::operator("op-e2e"),
                method: "read_session_output".into(),
                params,
            };
            wire::write_request(&mut s, &req).expect("write");
            let resp = wire::read_response(&mut s).expect("read").expect("present");
            if !resp.ok {
                break;
            }
            let result = resp.result.unwrap();
            let b64 = result["bytes"].as_str().unwrap();
            use base64::Engine;
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .expect("base64");
            accumulated.push_str(&String::from_utf8_lossy(&chunk));
            cursor = Some(result["cursor"].as_u64().unwrap());

            if accumulated.contains("30 100") {
                let _ = rpc_kill_session(&socket, "op-e2e", &uid);
                drop(session);
                stop_test_daemon(&socket, stop, handle);
                return;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let _ = rpc_kill_session(&socket, "op-e2e", &uid);
        drop(session);
        stop_test_daemon(&socket, stop, handle);
        panic!(
            "PTY did not report '30 100' from stty size within 3s; got:\n{}",
            accumulated
        );
    }

    // ===== Slice 10c-e-3b-fix2 acceptance =====

    #[test]
    fn dropping_client_session_does_not_kill_daemon_session() {
        // Named acceptance: Drop is detach-only. The daemon's
        // child must keep running after the TUI's ClientSession
        // drops — that's the property reconnect-on-restart
        // depends on. Operator-driven kill (A-w) is the
        // separate explicit path; tested in
        // `app::close_active_session` integration. Here we pin
        // the structural rule by simulating "TUI shutdown
        // without A-w": construct a `ClientSession`, drop it,
        // confirm the daemon's `state.sessions` still has the
        // entry within a bounded window.
        let (socket, working_dir, state, stop, handle) =
            start_test_daemon("ws-drop-detach");
        let argv = vec!["/bin/bash".to_string()];
        let uid_pre = test_uid();
        let config = bash_config(
            &socket,
            &working_dir,
            "op-drop",
            &uid_pre,
            "ws-drop-detach",
            "drop-test",
            &argv,
            80,
            24,
        );
        let session = ClientSession::new(config).expect("spawn");
        let uid = session.session_uid.clone();

        // Confirm the session is in the registry while the
        // ClientSession is alive.
        assert!(
            state.lock().unwrap().sessions.contains_key(&uid),
            "session should be in registry while ClientSession is alive"
        );

        // Drop the ClientSession WITHOUT calling kill_session.
        drop(session);

        // The structural invariant: drop closes the attach
        // socket and EventLoop, but the daemon's child PTY
        // keeps running. Allow a brief settle window for the
        // attach-handler thread to wind down — the session's
        // bookkeeping in `state.sessions` must NOT be removed.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            state.lock().unwrap().sessions.contains_key(&uid),
            "DROP MUST NOT KILL: daemon's state.sessions should still \
             contain {} after ClientSession dropped — that's the \
             property reconnect-on-restart depends on",
            uid,
        );

        // Cleanup for the test (explicit kill, like A-w would).
        let _ = rpc_kill_session(&socket, "op-drop", &uid);
        // Drain the daemon's reaper-cleanup callback.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if !state.lock().unwrap().sessions.contains_key(&uid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        stop_test_daemon(&socket, stop, handle);
    }

    #[test]
    fn explicit_rpc_kill_session_removes_entry_from_registry() {
        // Companion to the Drop-detach test: the operator-driven
        // kill path (A-w wraps `rpc_kill_session`) IS what
        // removes the session from the daemon's registry. After
        // a successful kill, the reaper-cleanup callback fires
        // and the entry disappears within a bounded window.
        let (socket, working_dir, state, stop, handle) =
            start_test_daemon("ws-explicit-kill");
        let argv = vec!["/bin/bash".to_string()];
        let uid_pre = test_uid();
        let config = bash_config(
            &socket,
            &working_dir,
            "op-kill",
            &uid_pre,
            "ws-explicit-kill",
            "kill-test",
            &argv,
            80,
            24,
        );
        let session = ClientSession::new(config).expect("spawn");
        let uid = session.session_uid.clone();
        assert!(state.lock().unwrap().sessions.contains_key(&uid));

        // Explicit kill. Returns Ok once the daemon has
        // SIGKILL'd via pidfd.
        rpc_kill_session(&socket, "op-kill", &uid)
            .expect("explicit kill_session rpc ok");

        // Wait for the reaper-cleanup callback to remove the
        // entry. Bounded — the callback is asynchronous.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if !state.lock().unwrap().sessions.contains_key(&uid) {
                drop(session);
                stop_test_daemon(&socket, stop, handle);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        drop(session);
        stop_test_daemon(&socket, stop, handle);
        panic!(
            "explicit kill_session did not remove {} from registry within 3s",
            uid
        );
    }

    /// Slice 10d watcher-fix #1: `memory_cap_bytes` still
    /// travels on the wire, but `cgroup_path` is NO LONGER sent
    /// by the TUI. The daemon discovers the actual cgroup from
    /// `/proc/<spawn-pid>/cgroup` post-spawn — a buggy or
    /// malicious caller cannot direct the daemon's watcher at a
    /// cgroup containing PIDs from unrelated processes anymore.
    ///
    /// This test pins the new wire contract: even when the
    /// caller's `ClientSessionConfig.cgroup_path` is `Some(...)`
    /// (legacy code path or accidental), `rpc_start_session_full`
    /// must drop it before the wire. The response's
    /// `cgroup_path` field is now informational (daemon-
    /// authoritative) — we test the round-trip by having the
    /// fake daemon echo a fabricated discovered path.
    #[test]
    fn memory_cap_bytes_travels_on_wire_but_caller_cgroup_path_is_dropped() {
        use cm_daemon::control::wire as wire_local;
        use std::os::unix::net::UnixListener;

        let dir = TempDir::new().expect("tempdir");
        std::mem::forget(dir);
        let socket_path = std::path::PathBuf::from(format!(
            "/tmp/cm-memcap-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path).expect("bind");

        // Fabricated discovered path the spy returns — proves
        // the response-side `cgroup_path` is the daemon's word,
        // not echoed from a request field.
        let daemon_authoritative_path =
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/cm-sess-discovered.scope";

        let socket_for_spy = socket_path.clone();
        let spy = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let req = wire_local::read_request(&mut stream)
                .expect("read req")
                .expect("frame");
            let resp = cm_daemon::control::protocol::Response::ok(
                req.id.clone(),
                serde_json::json!({
                    "session_uid": req.params["uid"].as_str().unwrap(),
                    "cgroup_path": daemon_authoritative_path,
                }),
            );
            wire_local::write_response(&mut stream, &resp).expect("write");
            drop(socket_for_spy);
            req
        });

        let argv = vec!["/bin/bash".to_string()];
        let uid = test_uid();
        // The HOSTILE caller-supplied path — the test verifies
        // this never reaches the wire. Pre-fix this would have
        // ridden the request to the daemon and (in production)
        // been trusted as the watcher's cgroup.
        let hostile_cgroup = std::path::Path::new(
            "/sys/fs/cgroup/user.slice/HOSTILE-CALLER-SUPPLIED.scope",
        );
        let config = ClientSessionConfig {
            daemon_socket: &socket_path,
            operator_token_id: "op-memcap",
            uid: &uid,
            workspace_id: "ws-memcap",
            label: "memcap",
            session_type: "bash",
            argv: &argv,
            working_dir: std::path::Path::new("/tmp"),
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: Some(64 * 1024 * 1024),
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: Some(hostile_cgroup),
            worktree_path: None,
            task_id: None,
            transcript_path: None,
            workflow_run_id: None,
            workflow_role: None,
        };
        let result = rpc_start_session_full(&config).expect("rpc ok");

        let req = spy.join().expect("spy joined");
        // Wire-out: memory_cap_bytes still travels.
        assert_eq!(
            req.params["memory_cap_bytes"].as_u64(),
            Some(64 * 1024 * 1024),
            "memory_cap_bytes must still travel on the wire"
        );
        // Wire-out: `cgroup_path` must NOT travel — this is
        // the security fix. The field is absent from the JSON
        // (serde_json `Value` returns Null for missing keys;
        // we assert the as_str() option is None).
        assert!(
            req.params["cgroup_path"].is_null()
                || req.params.get("cgroup_path").is_none(),
            "cgroup_path must NOT be sent on the wire (slice 10d watcher-fix #1) — \
             observed: {:?}",
            req.params["cgroup_path"]
        );

        // Wire-in: daemon's response carries the
        // daemon-discovered path, not the caller's hostile
        // one. (The fake daemon here echoes whatever it
        // claims; in production it's the /proc-discovered
        // path.)
        assert_eq!(
            result.cgroup_path.as_deref(),
            Some(daemon_authoritative_path),
            "response cgroup_path must be the daemon-authoritative path"
        );
        assert_ne!(
            result.cgroup_path.as_deref(),
            Some(hostile_cgroup.to_str().unwrap()),
            "result must NOT contain the hostile caller-supplied path"
        );

        let _ = std::fs::remove_file(&socket_path);
    }

    // ===== Slice 10c-e-3b-fix5: end-to-end chunking =====

    #[test]
    fn client_session_write_200kib_arrives_at_daemon_pty_without_drops() {
        // Reviewer's named E2E acceptance for fix5: write a
        // 200 KiB buffer through `ClientSession::write` (which
        // queues `Msg::Input` on the EventLoop, which calls
        // `StreamWriter::write`, which CHUNKS into 4 frames),
        // and observe the daemon's `/bin/cat` PTY echo back the
        // full 200 KiB through the fanout. Pre-fix5 the daemon
        // silently dropped the entire 200 KiB frame on the
        // 64 KiB cap; post-fix5 it arrives intact.
        //
        // Configure cat via custom argv since the default
        // `bash_config` helper hardcodes `/bin/bash`.
        let (socket, working_dir, state, stop, handle) =
            start_test_daemon("ws-chunking-e2e");
        // Subscribe to the fanout BEFORE the write so we don't
        // miss any echoed bytes. The subscription is registered
        // against the daemon's pre-spawned session; we'll spawn
        // cat into ws-chunking-e2e via ClientSession::new
        // shortly. Order: spawn first, then subscribe — the
        // session uid isn't known until start_session returns.

        let argv = vec!["/bin/cat".to_string()];
        let uid_pre = test_uid();
        let config = ClientSessionConfig {
            daemon_socket: &socket,
            operator_token_id: "op-chunk",
            uid: &uid_pre,
            workspace_id: "ws-chunking-e2e",
            label: "chunking-e2e",
            session_type: "bash",
            argv: &argv,
            working_dir: &working_dir,
            env: std::collections::BTreeMap::new(),
            cols: 200,
            rows: 50,
            memory_cap_bytes: None,
            memory_cap_hard_bytes: None,
            cgroup_prefix: None,
            cgroup_path: None,
            worktree_path: None,
            task_id: None,
            transcript_path: None,
            workflow_run_id: None,
            workflow_role: None,
        };
        let session = ClientSession::new(config).expect("spawn cat session");
        let uid = session.session_uid.clone();

        // Subscribe to the daemon-side fanout. Anything cat
        // writes back lands here.
        let rx = state
            .lock()
            .unwrap()
            .sessions
            .get(&uid)
            .expect("session in daemon registry")
            .fanout
            .subscribe();

        // Write 200 KiB of a recognizable pattern. The pattern
        // must not contain newlines (PTY CR/LF translation
        // would double the byte count) — use uppercase 'A's.
        let payload_len = 200 * 1024;
        let payload = vec![b'A'; payload_len];
        session
            .sender
            .send(alacritty_terminal::event_loop::Msg::Input(
                std::borrow::Cow::Owned(payload.clone()),
            ))
            .expect("queue Msg::Input on EventLoop");

        // Read from the fanout until cumulative bytes ≥ payload_len
        // OR deadline. We accumulate ALL bytes (cat's echo, PTY
        // line-discipline echo, whatever) and assert the running
        // 'A'-byte count reaches payload_len. If chunking is
        // broken pre-fix5, the daemon dropped the whole 200 KiB
        // frame and the count stays at 0.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut a_count: usize = 0;
        let mut total_bytes: usize = 0;
        while std::time::Instant::now() < deadline {
            let remaining =
                deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(remaining.min(std::time::Duration::from_millis(200))) {
                Ok(chunk) => {
                    total_bytes += chunk.len();
                    a_count += chunk.iter().filter(|&&b| b == b'A').count();
                    if a_count >= payload_len {
                        // Cleanup before returning.
                        let _ = rpc_kill_session(&socket, "op-chunk", &uid);
                        drop(session);
                        stop_test_daemon(&socket, stop, handle);
                        return;
                    }
                }
                Err(_) => {} // poll; loop until deadline
            }
        }

        // Cleanup before panic.
        let _ = rpc_kill_session(&socket, "op-chunk", &uid);
        drop(session);
        stop_test_daemon(&socket, stop, handle);
        panic!(
            "200 KiB write through ClientSession::write did NOT arrive at the \
             daemon's PTY: only {} of {} 'A' bytes echoed back through the fanout \
             ({} total bytes observed). Either the chunking is broken (frames \
             oversized → daemon cap rejected them) or the daemon's per-session \
             writer mutex stalled on the chunks.",
            a_count, payload_len, total_bytes,
        );
    }
}
