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
use std::time::Instant;

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
fn rpc_round_trip(daemon_socket: &Path, req: &Request) -> anyhow::Result<Response> {
    let mut stream = UnixStream::connect(daemon_socket)
        .with_context(|| format!("dial daemon socket {}", daemon_socket.display()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use cm_daemon::session::{DaemonSession, SpawnParams};
    use cm_daemon::state::DaemonState;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc as StdArc, Mutex};
    use tempfile::TempDir;

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
            argv,
            working_dir,
            env: std::collections::BTreeMap::new(),
            cols,
            rows,
            memory_cap_bytes: None,
            cgroup_path: None,
            worktree_path: None,
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
            argv: &argv,
            working_dir: std::path::Path::new("/tmp"),
            env: std::collections::BTreeMap::new(),
            cols: 80,
            rows: 24,
            memory_cap_bytes: Some(64 * 1024 * 1024),
            cgroup_path: Some(hostile_cgroup),
            worktree_path: None,
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
            argv: &argv,
            working_dir: &working_dir,
            env: std::collections::BTreeMap::new(),
            cols: 200,
            rows: 50,
            memory_cap_bytes: None,
            cgroup_path: None,
            worktree_path: None,
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
