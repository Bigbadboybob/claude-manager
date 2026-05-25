//! `cm-daemon` library entry points and shared modules.
//!
//! ## Linux-only
//!
//! Slice 10d watcher-fix #2 (build portability): the daemon is **Linux-
//! only by construction** — it owns pidfd (`libc::SYS_pidfd_open` /
//! `pidfd_send_signal`), cgroup-v2 (`/sys/fs/cgroup` reads, `memory.events`
//! polling, `cgroup.procs` enumeration), `/proc/<pid>/cgroup` discovery,
//! and `systemd-run --user --scope` integration. The pre-fix-#2
//! per-module `#[cfg(any(target_os = "linux", test))]` gates and
//! `#[cfg(not(target_os = "linux"))]` stubs were misleading: they
//! suggested portability the daemon doesn't actually have, while
//! breaking the build on non-Linux because unconditional refs (the
//! `methods.rs` watcher-spawn arm, the `path.rs` cgroup helpers)
//! would fail to resolve.
//!
//! Declaring the whole crate Linux-only at the root matches actual
//! capability. Non-Linux developers can still build the TUI's
//! non-daemon paths against a Linux-host daemon socket (or via the
//! Python `mcp_server` shim) — they just can't run the daemon
//! binary locally.
//!
//! The binary in `src/main.rs` is a thin shim that calls [`run`]; everything
//! testable lives here so unit tests can drive the bind path against
//! temp directories without spawning a subprocess.
//!
//! ## Shared modules
//!
//! As Phase 1 of doc/persistent-host-daemon.md migrates code from the TUI
//! into this crate, modules that *both* the TUI and the daemon need
//! during the transition are re-exported here. After slice 10 (TUI ⇄
//! daemon RPC rewire), the TUI no longer depends on this crate; until
//! then, the dep is transitional.
//!
//! Currently exposed:
//!   - [`worktree`] — `git worktree add` / branch slug / path resolution
//!     helpers. The daemon's eventual spawn path owns worktree creation;
//!     the TUI still calls into these directly in `app.rs` and `backend.rs`
//!     until the RPC rewire lands.
//!   - [`control`] — JSON-RPC wire types (`Request` / `Response` /
//!     `Caller` / `StreamFrame`). The server / queue / methods that
//!     drive these stay in `tui/src/control/` until App state migrates
//!     daemon-side (slices 6–9); slice 4 finishes when those arrive.
//!   - [`attach`] — `AttachTicket` allocator. Mints and consumes
//!     single-use, identity-bound, short-TTL tokens that route fresh
//!     dedicated connections to specific session PTY fanouts. Slice 5
//!     of the design doc. The `session.attach` / `attach.open` method
//!     handlers wire these into the dispatch table when control/
//!     completes its relocation.

// Slice 10d watcher-fix #2: declare the whole crate Linux-only.
// Per-module gates inside the source files are removed so the
// portability story is honest and single-source-of-truth.
#![cfg(target_os = "linux")]

pub mod attach;
pub mod control;
pub mod manifest;
pub mod mcp_config;
pub mod path;
pub mod planning_client;
pub mod reaper;
pub mod session;
pub mod session_watch;
pub mod state;
pub mod transcript_detect;
pub mod workflow;
pub mod worktree;

#[cfg(test)]
mod test_support;

use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Resolve the daemon socket path.
///
/// Resolution order:
///   1. `$CM_DAEMON_SOCKET` if set to a non-empty value (used by
///      tests and multi-daemon setups). Empty / whitespace-only
///      values count as unset (slice-10c-b review fix — same
///      semantics as the Python `control_client.default_socket_path`
///      and as `tui/src/mcp_config.rs::build_env`'s authoritative-
///      empty entries).
///   2. `$HOME/.cm/daemon.sock`.
///   3. `/tmp/.cm/daemon.sock` as a last-ditch fallback for environments
///      with no `HOME` (CI sandboxes, container init).
///
/// Mirrors `tui/src/control/server.rs::default_socket_path` modulo the
/// env-var name and filename so the two stay structurally parallel.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = path::env_socket_override("CM_DAEMON_SOCKET") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".cm/daemon.sock")
}

/// Resolve the TUI control-socket path.
///
/// Sub-2c: the daemon-side `mcp_config::build_env` needs the TUI
/// socket path so daemon-spawned agents can reach
/// `workflow_transition` / `workflow_done` / other TUI-only methods.
/// Mirrors `tui/src/control/server.rs::default_socket_path` exactly:
///
///   1. `$CM_TUI_SOCKET` if set to a non-empty value.
///   2. `$HOME/.cm/tui.sock`.
///   3. `/tmp/.cm/tui.sock` as a last-ditch fallback.
///
/// Empty / whitespace-only env values count as "unset" — same
/// hygiene as `default_socket_path()`.
pub fn default_tui_socket_path() -> PathBuf {
    if let Some(p) = path::env_socket_override("CM_TUI_SOCKET") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    home.join(".cm/tui.sock")
}

/// The *canonical* default socket path — always `$HOME/.cm/daemon.sock`,
/// regardless of `$CM_DAEMON_SOCKET`. `bind_socket` uses this to decide
/// whether to tighten the parent directory: hardening is only applied
/// when the bound path matches what the daemon would have written
/// "by default" had the operator not opted into a custom location.
///
/// Distinct from [`default_socket_path`] because the env var changes
/// *where the daemon binds*, but it doesn't change *what counts as the
/// canonical layout we're allowed to chmod*. A user pointing
/// `CM_DAEMON_SOCKET=/tmp/x.sock` is taking responsibility for that
/// directory's permissions; the daemon must not silently chmod it.
///
/// Returns `None` only when `$HOME` is unset (unusual; means the
/// "canonical default" doesn't have a stable identity, so nothing
/// matches and hardening is skipped).
fn canonical_default_socket_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".cm/daemon.sock"))
}

/// Bind a Unix domain socket at `path`, performing a stale-socket probe
/// first.
///
/// Behavior:
///   - If `path` doesn't exist, just bind.
///   - If `path` exists, attempt a one-shot `connect` to determine whether
///     another daemon is alive:
///     - Connect succeeds → return `AddrInUse`. The doc's "running it
///       twice fails fast on the stale-socket probe" requirement.
///     - Connect refuses with `ConnectionRefused` or `NotFound` → stale
///       inode from a crashed daemon; unlink and bind.
///     - Other errors (e.g. `EACCES`) → leave the file alone and let
///       `UnixListener::bind` surface a useful error.
///
/// Mirrors the probe in `tui/src/control/server.rs:47-72`. We re-implement
/// rather than pull it in because the TUI's `control` module hasn't moved
/// to this crate yet (that's a later slice); duplication is intentional
/// and short-lived.
pub fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    // The parent chmod is scoped to the *canonical* default layout —
    // `$HOME/.cm/daemon.sock`. The decision is driven purely by the
    // `path` argument (compared against the canonical path), not by
    // ambient env vars: operators handing in a custom path (whether
    // through `$CM_DAEMON_SOCKET` resolution or any other route) are
    // taking responsibility for that directory's permissions. A
    // path-driven check also means `bind_socket` does the same thing
    // regardless of how the caller resolved the path — important for
    // tests, which can drive any branch by passing the matching path
    // and tweaking `$HOME` under `env_lock`.
    let tighten_parent = canonical_default_socket_path()
        .as_deref()
        .map(|canonical| canonical == path)
        .unwrap_or(false);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        if tighten_parent {
            // Best-effort: if the directory lives on a filesystem
            // that doesn't support chmod, or we don't own it, prefer
            // a working daemon to a hard error — the socket-level
            // 0600 below is still the load-bearing line of defense.
            let _ = std::fs::set_permissions(
                parent,
                std::fs::Permissions::from_mode(0o700),
            );
        }
    }

    if path.exists() {
        match UnixStream::connect(path) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::AddrInUse,
                    format!(
                        "another cm-daemon appears to be running at {} — \
                         set CM_DAEMON_SOCKET to a different path or stop \
                         the other instance",
                        path.display()
                    ),
                ));
            }
            Err(e)
                if e.kind() == ErrorKind::ConnectionRefused
                    || e.kind() == ErrorKind::NotFound =>
            {
                // Stale inode — safe to remove.
                let _ = std::fs::remove_file(path);
            }
            Err(_) => {
                // Other connect error (e.g. EACCES). Leave the file
                // alone; bind below will surface the real problem.
            }
        }
    }

    // Race-free socket creation: wrap the bind in a tightened umask
    // scope so the inode is born at 0o600 — not at the umask-derived
    // mode (typically 0o666 for AF_UNIX) followed by a post-bind
    // chmod. The pre-fix sequence had a microsecond TOCTOU window
    // where another local process could `connect()` to a too-
    // permissive socket. That window is harmless on the default path
    // (parent dir is 0o700, so no traversal) but real on the custom-
    // path case (`CM_DAEMON_SOCKET=/tmp/x.sock` leaves `/tmp` alone),
    // so we always go through the umask scope.
    let listener = bind_with_tight_umask(path)?;
    // Belt-and-suspenders chmod. Cheap, and defends against any
    // future codepath that constructs a listener outside this helper
    // (e.g. tests, or a future systemd-socket-activation hookup).
    // Refusing to start on chmod failure is intentional: a permissive
    // socket would silently widen the trust boundary.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Bind a `UnixListener` under a tightened umask so the socket inode
/// is created with mode `0o600` directly — no bind-then-chmod race
/// window. The previous umask is restored before returning, so the
/// scope is invisible to callers and to any other fs operations they
/// do later.
///
/// ## Why `0o177` and not the usual `0o077`
///
/// The Linux AF_UNIX bind path creates the socket inode with default
/// mode `0o777`, not the `0o666` used for regular files. The
/// security-canonical umask `0o077` would strip group + other bits
/// but leave owner-execute, giving the socket mode `0o700`. That's
/// still safe from a trust-boundary standpoint (only owner can
/// `connect()`), but the slice-4 review asked specifically for
/// `0o600` to be observable from the bind step alone — which
/// requires `0o177` so the owner-execute bit is also masked off.
///
/// This is the load-bearing primitive for `bind_socket`'s atomic
/// security guarantee; the belt-and-suspenders `chmod` in `bind_socket`
/// runs after this and defends against any future codepath that
/// constructs a listener some other way.
fn bind_with_tight_umask(path: &Path) -> std::io::Result<UnixListener> {
    // SAFETY: `umask(2)` mutates a process-global field. There is no
    // safer Rust API for this — `nix::sys::stat::umask` is also unsafe
    // to compose against and pulls in another dep. We restore the
    // previous value unconditionally below, before returning, even on
    // error paths. If the test harness runs multiple bind tests in
    // parallel the `env_lock` test mutex serializes them; in
    // production there's only ever the one daemon process.
    let prev_umask = unsafe { libc::umask(0o177) };
    let result = UnixListener::bind(path);
    unsafe {
        libc::umask(prev_umask);
    }
    result
}

/// Run the daemon: bind the socket, log the address, and drive the
/// accept loop until the process is signalled.
///
/// Slice 10a-shell wires the accept loop to a real
/// read-dispatch-write cycle:
///   1. Accept a connection.
///   2. Spawn a per-connection thread so a slow request doesn't
///      head-of-line block accept.
///   3. Read one length-prefixed `Request` (`wire::read_request`).
///   4. Lock the shared `DaemonState`, call `dispatch_request`.
///   5. Write the `Response` back (`wire::write_response`).
///   6. Close.
///
/// The dispatcher itself is a placeholder until slice 10b — every
/// method returns `UnknownMethod`. The wire path is what's being
/// proved by this commit: a client can dial the daemon, send a
/// request, and receive a structured response. End-to-end test in
/// `daemon/tests/accept_loop.rs` confirms that.
pub fn run() -> anyhow::Result<()> {
    let path = default_socket_path();
    let listener = bind_socket(&path).map_err(|e| {
        anyhow::anyhow!("failed to bind cm-daemon socket at {}: {}", path.display(), e)
    })?;
    eprintln!("cm-daemon: listening on {}", path.display());

    // Shared mutable daemon state. Per-connection threads lock for
    // the duration of one dispatch. Slice 10c onward fills in the
    // fields (sessions, etc.) as App-state migrates; 10a-types
    // already wires the read-only manifest snapshot below.
    let mut initial_state = state::DaemonState::new();
    // The path the TUI dials for a dedicated attach connection.
    // For Phase 1 (Unix socket only), the attach connection lands
    // on the same listener as the control connection — the
    // `attach.open` first frame routes it. When the TCP transport
    // arrives in Phase 3 this becomes `host:port`.
    initial_state.attach_addr = path.to_string_lossy().into_owned();
    let manifest_path = state::default_manifest_path();
    match initial_state.load_manifest_from_disk(&manifest_path) {
        Ok(true) => {
            eprintln!(
                "cm-daemon: loaded {} workspace(s) from {}",
                initial_state.workspaces.len(),
                manifest_path.display(),
            );
        }
        Ok(false) => {
            eprintln!(
                "cm-daemon: no manifest at {}; starting with empty state",
                manifest_path.display(),
            );
        }
        Err(e) => {
            // Log + continue with empty state. The TUI's load path
            // does the corrupt-file backup + recovery; duplicating
            // it here would risk two writers fighting on the
            // backup filename (slice 10a is read-only on the daemon
            // side, so deferral is correct).
            eprintln!(
                "cm-daemon: manifest at {} failed to load ({}); starting with empty state. The TUI will back it up on its next read.",
                manifest_path.display(),
                e,
            );
        }
    }
    let state = std::sync::Arc::new(std::sync::Mutex::new(initial_state));

    // 10d-2c-2-2-a: spawn the workflow on_idle poller. 2c-2-2-a's
    // gate is hard-coded to "TUI owns" so this is a no-op behavior
    // change — the loop runs and produces decisions but every
    // decision is `Skip{TuiOwns}`. 2c-2-2-b flips the gate.
    //
    // Failure to spawn the poller thread is logged-and-continued,
    // not fatal: the daemon's core dispatch path doesn't depend on
    // polling, and TUI-driven static `on_idle` still works for
    // opt-in-off sessions. Matches the `spawn_watcher` pattern
    // (slice 10d-memory-cap-relocation): transient resource
    // pressure during startup shouldn't crash the daemon.
    let poller = std::sync::Arc::new(
        workflow::poller::WorkflowPoller::new(std::sync::Arc::clone(&state)),
    );
    if let Err(e) = poller.start() {
        eprintln!(
            "cm-daemon: workflow poller thread spawn failed: {}; \
             daemon continuing without workflow polling (TUI-driven \
             static on_idle still works for opt-in-off sessions)",
            e,
        );
    }

    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let state = state.clone();
                let _ = std::thread::Builder::new()
                    .name("cm-daemon-conn".into())
                    .spawn(move || handle_connection(stream, state));
            }
            Err(e) => {
                // EBADF on shutdown — listener dropped. Anything else is
                // worth surfacing so an operator can see why the loop
                // exited.
                if e.kind() != ErrorKind::Other {
                    eprintln!("cm-daemon: accept error: {}", e);
                }
                break;
            }
        }
    }

    // Best-effort cleanup. If the daemon was killed hard, the inode
    // stays and the next start's stale-socket probe handles it.
    let _ = std::fs::remove_file(&path);
    // 10d-2c-2-2-a: signal the poller to exit. The accept loop has
    // already broken (listener dropped or EBADF on shutdown), so
    // dispatch-side threads will wind down on their own; the
    // poller is the one long-running auxiliary that needs an
    // explicit signal.
    poller.shutdown();
    Ok(())
}

/// Maximum time `handle_connection` waits for the request side of
/// one round trip. A client that connects and sends nothing (or
/// sends a partial length prefix and stalls) would otherwise park a
/// per-connection thread forever. 30 seconds is comfortably above
/// any legitimate latency we expect from local-host MCP traffic;
/// when the daemon goes remote (Phase 3) this gets revisited.
pub const CONNECTION_READ_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);

/// Maximum time the response side of one round trip can take.
/// Shorter than read because the response body is bounded
/// (`wire::MAX_REQUEST_BYTES`) and any legitimate consumer drains
/// it in milliseconds.
pub const CONNECTION_WRITE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(10);

/// Per-connection handler. Reads one request, dispatches it under
/// the state lock, writes the response, and closes. Errors and
/// timeouts are logged but never crash the accept loop — a
/// malformed or stalled client shouldn't take down the daemon.
fn handle_connection(
    mut stream: std::os::unix::net::UnixStream,
    state: std::sync::Arc<std::sync::Mutex<state::DaemonState>>,
) {
    handle_connection_with_timeouts(
        &mut stream,
        state,
        CONNECTION_READ_TIMEOUT,
        CONNECTION_WRITE_TIMEOUT,
    );
}

/// Inner form with explicit timeout parameters — tests use short
/// timeouts (a few hundred ms) so the silent-client case can be
/// exercised without making the suite slow.
fn handle_connection_with_timeouts(
    stream: &mut std::os::unix::net::UnixStream,
    state: std::sync::Arc<std::sync::Mutex<state::DaemonState>>,
    read_timeout: std::time::Duration,
    write_timeout: std::time::Duration,
) {
    // Best-effort: if setting the timeout fails (very unusual on
    // Unix sockets), proceed with the default-blocking behavior.
    // The lower-bound is that on any sane Linux we will get the
    // timeout we asked for.
    let _ = stream.set_read_timeout(Some(read_timeout));
    let _ = stream.set_write_timeout(Some(write_timeout));

    let req = match control::wire::read_request(stream) {
        Ok(Some(req)) => req,
        Ok(None) => {
            // Client closed without sending — clean disconnect.
            return;
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            // Read timeout: the client connected and stalled. Log
            // briefly and let the thread terminate — the connection
            // is closed when `stream` drops.
            eprintln!(
                "cm-daemon: connection idle past {:?}, dropping",
                read_timeout
            );
            return;
        }
        Err(e) => {
            eprintln!("cm-daemon: read_request error: {}", e);
            return;
        }
    };
    // `dispatch_request` returns a DispatchOutcome — most
    // methods produce `Done(response)`; `attach.open` produces
    // `AttachStream { response, handle }` on success so this
    // function can write the OK and then run the bidirectional
    // stream with a pre-built subscription (slice-10c-e-2
    // review-2 TOCTOU fix: subscription happens inside the same
    // lock as the ticket consume, not later in handle_attach_stream).
    let outcome = control::dispatch::dispatch_request(&state, &req);
    match outcome {
        control::dispatch::DispatchOutcome::Done(response) => {
            if let Err(e) = control::wire::write_response(stream, &response) {
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    eprintln!(
                        "cm-daemon: write stalled past {:?}, dropping",
                        write_timeout
                    );
                } else {
                    eprintln!("cm-daemon: write_response error: {}", e);
                }
            }
        }
        control::dispatch::DispatchOutcome::AttachStream { response, handle } => {
            if let Err(e) = control::wire::write_response(stream, &response) {
                eprintln!(
                    "cm-daemon: attach.open OK write failed: {} (dropping subscription)",
                    e
                );
                return;
            }
            control::stream::handle_attach_stream(stream, state, handle);
        }
        // 10e-b: manifest.watch streaming. Same shape as
        // AttachStream — write the OK response, then transition
        // the connection to the manifest-stream loop.
        control::dispatch::DispatchOutcome::ManifestWatchStream { response, handle } => {
            if let Err(e) = control::wire::write_response(stream, &response) {
                eprintln!(
                    "cm-daemon: manifest.watch OK write failed: {} (dropping subscription)",
                    e
                );
                return;
            }
            control::stream::handle_manifest_watch_stream(stream, handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::env_lock;
    use std::io::ErrorKind;
    use tempfile::TempDir;

    /// The doc's acceptance criterion: "running it twice fails fast on
    /// the stale-socket probe."
    ///
    /// Acquires `env_lock` because `bind_socket` transiently mutates
    /// the process umask through `bind_with_tight_umask`; concurrent
    /// tests creating tempdirs would otherwise see the tightened
    /// umask during the race window and end up with directories that
    /// can't be traversed.
    #[test]
    fn second_bind_against_running_daemon_returns_addr_in_use() {
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("daemon.sock");

        let _listener = bind_socket(&path).expect("first bind");

        let err = bind_socket(&path).expect_err("second bind must fail");
        assert_eq!(
            err.kind(),
            ErrorKind::AddrInUse,
            "expected AddrInUse, got {:?}",
            err.kind()
        );
    }

    /// A stale socket file with no live listener should be unlinked
    /// and the bind should succeed — that's the recovery path after
    /// a crashed daemon.
    ///
    /// `env_lock` rationale: same as above — `bind_socket` flips
    /// umask during the bind, and a `std::fs::write` racing that
    /// can fail mysteriously with `EACCES`.
    #[test]
    fn second_bind_replaces_stale_socket_file() {
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("daemon.sock");

        // Create a regular file at the path — `UnixStream::connect`
        // will refuse, simulating the "stale inode" case.
        std::fs::write(&path, b"stale").expect("write stale file");
        assert!(path.exists());

        let _listener = bind_socket(&path).expect("stale-cleanup bind should succeed");
    }

    /// Proves the atomic guarantee: the `bind(2)` syscall itself lands
    /// the inode at 0o600, *without* relying on the belt-and-suspenders
    /// chmod that `bind_socket` runs afterwards. If the umask scope
    /// regresses (e.g. someone removes it or moves the umask call
    /// outside `bind_with_tight_umask`), this test catches it because
    /// the chmod step isn't on this codepath.
    ///
    /// Closes the bind/chmod TOCTOU window the slice-4 reviewer flagged
    /// on the custom-path case (`CM_DAEMON_SOCKET=/tmp/x.sock`, parent
    /// not chmod'd to 0o700, so other local users could potentially
    /// race a connect between bind and chmod).
    #[test]
    fn umask_scoped_bind_lands_at_0600_before_any_chmod() {
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("daemon.sock");

        // SAFETY: process-global umask, serialized via env_lock.
        // Setting 0o000 means a naive AF_UNIX bind would land at 0o666.
        // The umask scope inside `bind_with_tight_umask` must tighten
        // it to 0o600 *during* the bind syscall.
        let prev_umask = unsafe { libc::umask(0o000) };
        let listener_result = bind_with_tight_umask(&path);
        unsafe {
            libc::umask(prev_umask);
        }
        let _listener = listener_result.expect("bind under permissive umask");

        let mode = std::fs::metadata(&path)
            .expect("stat socket")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "bind step alone yielded {:o}; umask scope did not apply",
            mode & 0o777,
        );
    }

    /// End-to-end check on the full `bind_socket` pipeline. With the
    /// umask scope this is now redundant with
    /// `umask_scoped_bind_lands_at_0600_before_any_chmod`, but it
    /// stays as the belt-and-suspenders test: any future regression
    /// that removes BOTH the umask scope and the post-bind chmod
    /// trips this.
    #[test]
    fn socket_is_0600_regardless_of_umask() {
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("daemon.sock");

        // SAFETY: `umask(2)` is process-global. The `env_lock`
        // mutex above serializes us against any other test that touches
        // process-global state. We restore the previous umask before
        // returning. We do NOT use a Drop-based guard because a panic
        // between the two calls would leave the test-binary's umask
        // permanently at 0o000 — bad for any subsequent test. A
        // straight-line restore is the right shape here; if the bind
        // panics, the test fails anyway and the test binary exits.
        let prev_umask = unsafe { libc::umask(0o000) };
        let bind_result = bind_socket(&path);
        unsafe { libc::umask(prev_umask) };

        let _listener = bind_result.expect("bind under permissive umask");

        let mode = std::fs::metadata(&path)
            .expect("stat socket")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "socket mode is {:o}, expected 0o600",
            mode & 0o777,
        );
    }

    /// Parent dir is tightened to 0o700 by `bind_socket` *only* when
    /// the path being bound matches the canonical default
    /// `$HOME/.cm/daemon.sock`. To drive that branch, the test points
    /// HOME at a tempdir and binds the canonical path under it.
    ///
    /// Belt-and-suspenders: also clears `$CM_DAEMON_SOCKET` for the
    /// test scope so a shell that exports it doesn't shift `default_socket_path`
    /// without affecting the canonical check — the policy is now
    /// path-driven, but a clean env makes the test's intent obvious.
    #[test]
    fn parent_dir_is_0700_after_bind() {
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");

        // Snapshot and clear the two env vars that affect path resolution.
        let prev_home = std::env::var_os("HOME");
        let prev_sock = std::env::var_os("CM_DAEMON_SOCKET");
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("CM_DAEMON_SOCKET");

        // Now the canonical default points inside the tempdir; binding
        // it triggers the parent-hardening branch.
        let parent = dir.path().join(".cm");
        std::fs::create_dir_all(&parent).expect("mkparent");
        // Set permissive perms on purpose so the bind has work to do.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
            .expect("loose parent");

        let path = parent.join("daemon.sock");
        let listener_result = bind_socket(&path);

        // Restore env before assertions so a panic doesn't leak state
        // into adjacent tests sharing `env_lock`.
        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        if let Some(s) = prev_sock {
            std::env::set_var("CM_DAEMON_SOCKET", s);
        }

        let _listener = listener_result.expect("bind");
        let parent_mode = std::fs::metadata(&parent)
            .expect("stat parent")
            .permissions()
            .mode();
        assert_eq!(
            parent_mode & 0o777,
            0o700,
            "parent mode is {:o}, expected 0o700",
            parent_mode & 0o777,
        );
    }

    /// Counterpart to `parent_dir_is_0700_after_bind`: binding a path
    /// that *isn't* the canonical default must leave its parent alone,
    /// no matter what `$CM_DAEMON_SOCKET` is set to. This is the
    /// behavior change in the slice-6 review fix — pre-fix, an env var
    /// pointing at `/tmp/x.sock` would have caused `bind_socket(&tempdir_path)`
    /// to leave the tempdir parent alone, but `bind_socket(&/tmp/x.sock)`
    /// would have hardened `/tmp`. The new path-driven policy makes
    /// the answer "are we binding the canonical default?" the only
    /// question that matters.
    #[test]
    fn parent_dir_left_alone_on_non_canonical_path() {
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let prev_home = std::env::var_os("HOME");
        let prev_sock = std::env::var_os("CM_DAEMON_SOCKET");
        // Set HOME to somewhere that *isn't* the tempdir so that
        // the canonical default and the bound path are guaranteed
        // not to match.
        std::env::set_var("HOME", "/nonexistent-home-for-test");
        // Setting CM_DAEMON_SOCKET to the path we're about to bind
        // would have triggered hardening under the OLD env-driven
        // logic; under the new policy it has no effect on hardening.
        let path = dir.path().join("ambient.sock");
        std::env::set_var("CM_DAEMON_SOCKET", &path);

        // Capture the parent's mode before bind so we can prove it
        // didn't change.
        let parent = dir.path().to_path_buf();
        let before = std::fs::metadata(&parent)
            .expect("stat parent before")
            .permissions()
            .mode()
            & 0o777;

        let listener_result = bind_socket(&path);

        match prev_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match prev_sock {
            Some(s) => std::env::set_var("CM_DAEMON_SOCKET", s),
            None => std::env::remove_var("CM_DAEMON_SOCKET"),
        }

        let _listener = listener_result.expect("bind");
        let after = std::fs::metadata(&parent)
            .expect("stat parent after")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            before, after,
            "non-canonical parent must not be modified: was {:o}, now {:o}",
            before, after,
        );
    }

    /// The default path honors `$CM_DAEMON_SOCKET`. Setting the env
    /// var in a test is process-wide; we don't actually bind here,
    /// just resolve. Serialized via `env_lock`.
    #[test]
    fn default_path_respects_env_override() {
        let _g = env_lock();

        struct EnvGuard {
            prev: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.prev.take() {
                    Some(v) => std::env::set_var("CM_DAEMON_SOCKET", v),
                    None => std::env::remove_var("CM_DAEMON_SOCKET"),
                }
            }
        }
        let _restore = EnvGuard {
            prev: std::env::var_os("CM_DAEMON_SOCKET"),
        };

        std::env::set_var("CM_DAEMON_SOCKET", "/tmp/cm-daemon-test-override.sock");
        let p = default_socket_path();
        assert_eq!(p, PathBuf::from("/tmp/cm-daemon-test-override.sock"));
    }

    /// Without `$CM_DAEMON_SOCKET`, the default lands under `$HOME/.cm`.
    /// Serialized via `env_lock`.
    #[test]
    fn default_path_uses_home_dot_cm() {
        let _g = env_lock();

        struct EnvGuard {
            prev_sock: Option<std::ffi::OsString>,
            prev_home: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.prev_sock.take() {
                    Some(v) => std::env::set_var("CM_DAEMON_SOCKET", v),
                    None => std::env::remove_var("CM_DAEMON_SOCKET"),
                }
                match self.prev_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
        let _restore = EnvGuard {
            prev_sock: std::env::var_os("CM_DAEMON_SOCKET"),
            prev_home: std::env::var_os("HOME"),
        };

        std::env::remove_var("CM_DAEMON_SOCKET");
        std::env::set_var("HOME", "/home/test-user");
        let p = default_socket_path();
        assert_eq!(p, PathBuf::from("/home/test-user/.cm/daemon.sock"));
    }

    /// The named slice-10c-b review regression: `CM_DAEMON_SOCKET=""`
    /// (literal empty) must NOT be treated as an override. Without
    /// the `env_socket_override` hygiene, `default_socket_path`
    /// would return `PathBuf::from("")` and downstream
    /// `absolutize_socket_path("")` would join cwd — the daemon
    /// would bind to (or poll for) the worktree directory itself.
    ///
    /// With the fix, the fn falls back to `$HOME/.cm/daemon.sock`.
    /// We also check the whitespace-only variant since that's the
    /// other common "accidentally unset" shape (a shell expansion
    /// that produced spaces).
    #[test]
    fn default_path_treats_empty_env_as_unset() {
        let _g = env_lock();

        struct EnvGuard {
            prev_sock: Option<std::ffi::OsString>,
            prev_home: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.prev_sock.take() {
                    Some(v) => std::env::set_var("CM_DAEMON_SOCKET", v),
                    None => std::env::remove_var("CM_DAEMON_SOCKET"),
                }
                match self.prev_home.take() {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
        let _restore = EnvGuard {
            prev_sock: std::env::var_os("CM_DAEMON_SOCKET"),
            prev_home: std::env::var_os("HOME"),
        };

        std::env::set_var("HOME", "/home/empty-env-test");

        // Case 1: literal empty.
        std::env::set_var("CM_DAEMON_SOCKET", "");
        let p = default_socket_path();
        assert_eq!(
            p,
            PathBuf::from("/home/empty-env-test/.cm/daemon.sock"),
            "literal empty CM_DAEMON_SOCKET must fall back to default — NOT be treated as an override (got {})",
            p.display(),
        );

        // Case 2: whitespace-only.
        std::env::set_var("CM_DAEMON_SOCKET", "   \t ");
        let p = default_socket_path();
        assert_eq!(
            p,
            PathBuf::from("/home/empty-env-test/.cm/daemon.sock"),
            "whitespace-only CM_DAEMON_SOCKET must fall back to default (got {})",
            p.display(),
        );

        // Sanity: padded valid override still resolves (trimmed).
        std::env::set_var("CM_DAEMON_SOCKET", "  /tmp/explicit.sock  ");
        let p = default_socket_path();
        assert_eq!(
            p,
            PathBuf::from("/tmp/explicit.sock"),
            "padded valid override must be trimmed and used",
        );
    }
}
