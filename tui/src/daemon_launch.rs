//! Auto-launch the `cm-daemon` binary when the socket is absent.
//! Originally slice 11 of doc/persistent-host-daemon.md.
//!
//! ## Phase 1 default flip (slice 10f)
//!
//! Daemon mode is now **mandatory**. TUI startup always tries to
//! ensure a daemon is reachable; if the binary is missing or the
//! handshake fails, `main()` exits non-zero with a clear error.
//! The historical `CM_USE_DAEMON_SOCKET=1` opt-in env var is now a
//! silent no-op — its presence or absence has no effect.
//!
//! On startup the TUI probes `~/.cm/daemon.sock`; if no daemon is
//! listening it spawns the `cm-daemon` binary detached
//! (stdin/stdout/stderr redirected to /dev/null) and polls for the
//! socket to appear. Spawn failure or timeout is a fatal error
//! surfaced by the caller — the daemon now hosts session state
//! that the TUI can't synthesize.

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Env var the daemon child reads at startup to learn the operator
/// token. See `cm_daemon::control::operator` for the validation
/// path. The TUI sets this on the daemon's spawn env (see
/// [`prepare_daemon_command`]) from the value persisted at
/// `~/.cm/operator-token`.
pub const OPERATOR_TOKEN_ENV: &str = "CM_OPERATOR_TOKEN";

/// Persisted across TUI restarts so the same token works against an
/// already-running daemon. Path: `~/.cm/operator-token`, 0o600.
/// Generated on first TUI launch.
pub(crate) const OPERATOR_TOKEN_FILENAME: &str = "operator-token";

/// Global accessor to the operator token. Initialized by the first
/// successful call to [`load_or_create_operator_token`] (which `main`
/// invokes at startup); subsequent calls in the TUI's RPC paths use
/// this accessor instead of plumbing the token through every method
/// signature.
static OPERATOR_TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Returns the operator token loaded at startup, panicking if
/// [`load_or_create_operator_token`] hasn't been called yet. In
/// production main.rs initializes the token before App::new, so
/// every call site is downstream of the initializer. In tests,
/// `test_support::init_dummy_operator_token` provides a fallback.
pub fn operator_token() -> &'static str {
    OPERATOR_TOKEN.get().map(String::as_str).unwrap_or_else(|| {
        // Fallback for tests that don't go through the main.rs
        // initialization path. Using `set` here is racy across
        // concurrent first-call tests but the value chosen is
        // deterministic, so the OnceLock either contains the
        // chosen literal OR another thread's identical literal.
        let _ = OPERATOR_TOKEN.set("test-fallback-operator-token".to_string());
        OPERATOR_TOKEN.get().map(String::as_str).unwrap()
    })
}

/// Read or generate the operator token used to authenticate the
/// TUI as `Caller::Operator` against its own daemon.
///
/// First TUI launch on a host: generates a fresh UUID v4, writes it
/// to `~/.cm/operator-token` (0o600), and returns it. Subsequent
/// launches read the same value back — important because the
/// daemon may already be running with the previously-generated
/// token in its env.
///
/// If reading the file fails for a reason other than "doesn't
/// exist" (e.g. permission denied), returns that error rather than
/// regenerating — silently overwriting could lock the user out of a
/// running daemon. The token file lives under `~/.cm/`, which has
/// 0o700 perms already (see `cm_daemon::path::dot_cm_dir`), so
/// only same-UID processes can read it.
pub fn load_or_create_operator_token() -> std::io::Result<String> {
    let path = operator_token_path();
    let token = match std::fs::read_to_string(&path) {
        Ok(s) => {
            let trimmed = s.trim().to_string();
            if trimmed.is_empty() {
                // Treat an empty file as missing — fall through to
                // generate fresh. Lets the operator clear the file
                // to force regeneration on next TUI launch (after
                // killing the daemon).
                generate_and_persist_operator_token(&path)?
            } else {
                trimmed
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            generate_and_persist_operator_token(&path)?
        }
        Err(e) => return Err(e),
    };
    // Cache for downstream `operator_token()` accesses. First call
    // wins (OnceLock semantics); subsequent calls in tests with a
    // different value are silently ignored — which is fine because
    // the token is immutable for the process lifetime.
    let _ = OPERATOR_TOKEN.set(token.clone());
    Ok(token)
}

fn generate_and_persist_operator_token(path: &Path) -> std::io::Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let token = uuid::Uuid::new_v4().simple().to_string();
    // Write with 0o600 so only the owner can read. Use create_new=true
    // so a concurrent TUI launch losing the race observes EEXIST
    // and falls through to read-existing — both end up with the
    // same on-disk value, just whichever one wrote first wins.
    let write_res = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
    match write_res {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(token.as_bytes())?;
            f.write_all(b"\n")?;
            Ok(token)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Race with another TUI process — read the value it wrote.
            std::fs::read_to_string(path).map(|s| s.trim().to_string())
        }
        Err(e) => Err(e),
    }
}

fn operator_token_path() -> PathBuf {
    cm_daemon::path::dot_cm_dir().join(OPERATOR_TOKEN_FILENAME)
}

/// Default timeout for the auto-launch handshake. The daemon's bind
/// + stale-probe + listener registration is sub-second on a healthy
/// host; 2 seconds gives generous headroom and matches the pattern
/// the design doc names (`tmux`-style auto-launch with up-to-2s wait).
pub const AUTO_LAUNCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Sleep granularity between socket-probe attempts inside the
/// auto-launch wait loop. Small enough to land sub-100ms after the
/// daemon binds; large enough not to busy-spin.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(50);

/// Strict wrapper used by `main()`. Daemon launch failure is
/// fatal — the daemon owns session state and `mcp_config::build_env`
/// always injects `CM_DAEMON_SOCKET` for agents, so a "log and
/// continue" fallback would pin every agent to a dead socket.
///
/// Use [`ensure_daemon_at_startup_with_timeout`] in tests that
/// want to pin the timeout; the public alias here uses the default
/// `AUTO_LAUNCH_TIMEOUT` for production.
pub fn ensure_daemon_at_startup(
    socket_path: &Path,
    operator_token: &str,
) -> std::io::Result<()> {
    ensure_daemon_at_startup_with_timeout(socket_path, operator_token, AUTO_LAUNCH_TIMEOUT)
}

/// Inner form of [`ensure_daemon_at_startup`] that accepts an
/// explicit timeout — tests use a small one so they don't wait the
/// full 2-second production budget.
///
/// On failure (binary not found, timeout, spawn error) the caller
/// is expected to log + `std::process::exit(1)`. Returning an
/// `Err` from `main()` works equally well; the test below asserts
/// only that an error is produced — the exit policy is `main()`'s.
///
/// ## Path absolutization (slice-10c-b review)
///
/// `socket_path` is absolutized **once at the top of this function**
/// and the result is the value passed to both:
///   - the `try_connect` probe loop in `ensure_daemon_running`, AND
///   - the child's `CM_DAEMON_SOCKET` env in `spawn_daemon_binary`.
///
/// Without this, a relative `CM_DAEMON_SOCKET` would resolve under
/// the TUI's cwd in the parent but under `/` in the child (because
/// the detach `pre_exec` does `chdir("/")` before `exec`), and the
/// probe loop would never connect. The "they match" property below
/// is load-bearing.
pub fn ensure_daemon_at_startup_with_timeout(
    socket_path: &Path,
    operator_token: &str,
    timeout: Duration,
) -> std::io::Result<()> {
    let abs_socket = absolutize_socket_path(socket_path)?;
    let abs_socket_for_spawn = abs_socket.clone();
    let token = operator_token.to_string();
    ensure_daemon_running(
        &abs_socket,
        move || {
            let (bin, args) = locate_launch_spec()?;
            spawn_daemon_binary(&bin, &args, &abs_socket_for_spawn, &token)
        },
        timeout,
    )
}

/// What to launch (DESIGN_HOLDER_BRAIN_SPLIT phase 6, O12): the
/// holder/brain split is entered ONLY by explicit configuration —
/// `CM_HOLDER_BINARY` names the cm-holder binary, and the TUI then
/// launches `cm-holder --brain <cm-daemon>`. NEVER inferred from
/// sibling-binary presence (merely building the workspace must not
/// flip a host into the split before its soak). Unset → today's
/// monolith launch.
pub(crate) fn locate_launch_spec() -> std::io::Result<(PathBuf, Vec<std::ffi::OsString>)> {
    if let Some(h) = std::env::var_os("CM_HOLDER_BINARY") {
        let holder = canonicalize_binary_path(&PathBuf::from(h))?;
        let daemon = locate_daemon_binary()?;
        eprintln!(
            "cm-tui: CM_HOLDER_BINARY set — launching the holder/brain split \
             ({} --brain {})",
            holder.display(),
            daemon.display()
        );
        return Ok((
            holder,
            vec!["--brain".into(), daemon.into_os_string()],
        ));
    }
    Ok((locate_daemon_binary()?, Vec::new()))
}

/// Core auto-launch logic, parameterized over the spawner so tests
/// can inject a cooperating (or non-cooperating) one. Production
/// callers go through [`maybe_ensure_daemon_running`] with the
/// real binary-spawn impl.
///
/// Behavior:
/// 1. If `socket_path` accepts a probe `connect()`, return Ok — a
///    daemon is already listening.
/// 2. Otherwise call `spawn_daemon`. If it errors, propagate.
/// 3. Poll `connect()` at `PROBE_INTERVAL` until `timeout` elapses
///    or the daemon comes up. Return `TimedOut` if neither.
pub fn ensure_daemon_running<F>(
    socket_path: &Path,
    spawn_daemon: F,
    timeout: Duration,
) -> std::io::Result<()>
where
    F: FnOnce() -> std::io::Result<()>,
{
    if try_connect(socket_path) {
        // Daemon already running. Sanity-check it isn't a stale
        // build — common after `cargo build` produces a new
        // binary on disk while the previously-started daemon
        // process keeps the old in-memory image. The stale
        // process is missing any dispatch arms / RPC handlers
        // added after its build time; agents calling those
        // methods see `unknown_method` from the daemon.
        warn_if_daemon_binary_stale(socket_path);
        return Ok(());
    }
    spawn_daemon()?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if try_connect(socket_path) {
            return Ok(());
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!(
            "cm-daemon did not become reachable at {} within {:?}",
            socket_path.display(),
            timeout
        ),
    ))
}

/// One-shot connect probe. Doesn't send or receive bytes — just
/// asks the OS whether something accepted the connection.
fn try_connect(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Detect "daemon is running but its binary on disk was
/// replaced after the process started" and warn the operator.
/// Linux annotates `/proc/<pid>/exe` with a literal ` (deleted)`
/// suffix when the executable file has been unlinked or replaced
/// (the typical outcome of `cargo build` producing a fresh
/// binary). When that suffix is present, the running daemon is
/// executing in-memory code that may be missing dispatch arms or
/// RPC handlers added in the on-disk version — symptom: agents
/// see `unknown_method` from methods that exist in source.
///
/// Best-effort: any failure to read PID or symlink → silent
/// pass. False negatives are tolerable (worst case the operator
/// hits the original unknown_method again); false positives
/// would be noisy and the connect-time peer-cred / procfs reads
/// can fail for benign reasons.
fn warn_if_daemon_binary_stale(socket_path: &Path) {
    let Some(pid) = peer_pid_of_socket(socket_path) else {
        return;
    };
    let exe_link = std::path::PathBuf::from(format!("/proc/{}/exe", pid));
    let Ok(target) = std::fs::read_link(&exe_link) else {
        return;
    };
    if !target.to_string_lossy().ends_with(" (deleted)") {
        return;
    }
    eprintln!(
        "cm-tui: WARNING: running cm-daemon (pid {}) is executing a stale \
         binary — the on-disk binary has been replaced since the daemon \
         started. Symptom: agent RPCs may return `unknown_method` for \
         methods that exist in the current source.",
        pid,
    );
    // Holder-split (phase 6, O12): the split changes the ADVICE —
    // `pkill cm-daemon` would SIGKILL the BRAIN (the holder counts a
    // crash and respawns the OLD pin: a silent revert). Detect the
    // split by the peer's parent being cm-holder (SO_PEERCRED names
    // the listener's binder, whose parent is the holder).
    let split = std::fs::read_to_string(format!("/proc/{}/stat", pid))
        .ok()
        .and_then(|stat| {
            let rest = &stat[stat.rfind(')')? + 1..];
            let ppid: i32 = rest.split_whitespace().nth(1)?.parse().ok()?;
            std::fs::read_to_string(format!("/proc/{}/comm", ppid)).ok()
        })
        .is_some_and(|comm| comm.trim() == "cm-holder");
    if split {
        eprintln!(
            "cm-tui:   This daemon runs in holder/brain SPLIT mode: deploy the \
             new binary with `scripts/cm-redeploy` (daemon.restart → the \
             holder's restart_brain path) — sessions survive. NEVER `pkill \
             cm-daemon` here: that kills the brain and the holder reverts to \
             the OLD pinned binary."
        );
    } else {
        eprintln!(
            "cm-tui:   To pick up the new binary: stop the TUI, run \
             `pkill cm-daemon`, then relaunch the TUI (it will respawn the \
             daemon from the on-disk binary). All running sessions will be \
             killed."
        );
    }
}

/// Read the peer's PID off a UnixStream connected to
/// `socket_path` via `SO_PEERCRED`. Linux-only API; the project
/// targets Linux exclusively. Returns `None` on any failure
/// (connect refused, getsockopt error, etc.) — the caller treats
/// the absence of a PID as "skip the staleness check."
fn peer_pid_of_socket(socket_path: &Path) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    let stream = UnixStream::connect(socket_path).ok()?;
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(cred.pid as u32)
}

/// Resolution order for the `cm-daemon` binary:
///   1. `$CM_DAEMON_BINARY` env var (explicit override, useful for
///      multi-installation setups and tests).
///   2. Sibling of the currently-running TUI binary — the installed
///      layout puts `cm-daemon` next to the TUI under `bin/`.
///   3. `<workspace_root>/target/debug/cm-daemon` — the dev layout
///      when running `cargo run -p claude-manager-tui`.
///
/// Returns `NotFound` with a clear message if none of these resolve;
/// the caller surfaces this so an operator who flipped the opt-in
/// without building the daemon sees a useful error rather than a
/// silent timeout.
///
/// ## Canonicalization (slice-10c-b review)
///
/// Every resolved path is canonicalized via
/// [`canonicalize_binary_path`] before return. This matters for the
/// env-override branch: an operator setting
/// `CM_DAEMON_BINARY=target/debug/cm-daemon` (relative) would
/// validate in the parent's cwd, but the child's `pre_exec` runs
/// `chdir("/")` before `exec` — the relative path would re-resolve
/// under `/` and `exec` would fail (or, worse, exec the wrong file
/// if `/target/debug/cm-daemon` happened to exist). Canonicalizing
/// here makes the path absolute and symlink-resolved at the moment
/// it's about to be passed to `Command::new`.
fn locate_daemon_binary() -> std::io::Result<PathBuf> {
    if let Some(p) = std::env::var_os("CM_DAEMON_BINARY") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return canonicalize_binary_path(&pb);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("cm-daemon");
            if candidate.is_file() {
                return canonicalize_binary_path(&candidate);
            }
        }
    }
    // Dev fallback: walk up from the current exe to find a workspace
    // `target/` directory and look for cm-daemon there. Cargo puts
    // tui's binary at <workspace>/target/<profile>/claude-manager-tui,
    // so cm-daemon is a sibling.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let candidate = parent.join("cm-daemon");
            if candidate.is_file() {
                return canonicalize_binary_path(&candidate);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "cm-daemon binary not found; set CM_DAEMON_BINARY or build it via `cargo build -p cm-daemon`",
    ))
}

/// Canonicalize `path` (resolve symlinks, expand to absolute) for
/// use as a binary argv[0] across the daemon `pre_exec` boundary.
/// The slice-10c-b reviewer flagged that the spawn's `chdir("/")`
/// invalidates relative paths between parent validation and child
/// exec; `fs::canonicalize` errors when the target doesn't exist,
/// so an invalid override fails fast in the parent rather than as
/// a confusing post-detach exec failure.
pub(crate) fn canonicalize_binary_path(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "canonicalize CM_DAEMON_BINARY={}: {} (the daemon detach runs chdir(\"/\"), so the binary path must be absolute and resolvable at the parent — set CM_DAEMON_BINARY to an absolute path that exists)",
                path.display(),
                e,
            ),
        )
    })
}

/// Re-export of [`cm_daemon::path::absolutize_socket_path`] for
/// callers that previously imported it under
/// `daemon_launch::absolutize_socket_path` (pre-second-consumer
/// extraction). The shared definition lives in `cm_daemon::path`
/// so `tui/src/mcp_config.rs::build_env` can share the exact same
/// code path — that's the "they match" load-bearing property
/// generalized across the daemon-launch and MCP-injection consumers.
pub(crate) use cm_daemon::path::absolutize_socket_path;

/// Spawn `cm-daemon` detached:
///
///   - stdio → /dev/null so the daemon doesn't write into the TUI's
///     terminal.
///   - `setsid` in `pre_exec` so the child becomes the leader of a
///     new session and process group, with no controlling terminal.
///     Without this, terminal `SIGHUP` (close the terminal, Ctrl-\\,
///     parent shell job-control) propagates to the daemon and kills
///     it — the opposite of "persistent host daemon" and a guaranteed
///     fail on the doc's reconnect / ring-buffer acceptance test
///     (which kills the TUI and expects the daemon to survive).
///   - `chdir("/")` in the same `pre_exec` so the daemon doesn't pin
///     the TUI's cwd (often a worktree the user wants to move or
///     remove).
///
/// We use a single fork + setsid rather than the classic double
/// fork. The single-fork variant is sufficient here because:
///   1. `setsid` already drops the controlling terminal — the child
///      can't accidentally re-acquire one (would require an explicit
///      `open(O_TTY)` against a tty device, which the daemon doesn't
///      do).
///   2. When the TUI exits, init (PID 1) inherits the daemon and
///      reaps it on exit; no zombie risk.
///   3. The doc's stale-socket probe (`daemon/src/lib.rs::bind_socket`)
///      already handles "daemon crashed, socket left over" cleanly,
///      so any edge case in the parent->child handoff is recoverable
///      on the next start.
///
/// The daemon's own `bind_socket` stale-probe handles any
/// stale-inode case left by an earlier daemon that crashed.
///
/// `socket_path` must be absolute — the caller is responsible for
/// absolutization via [`absolutize_socket_path`] (slice-10c-b
/// review). `bin` should be canonical (`locate_daemon_binary` is
/// the production source and does that).
fn spawn_daemon_binary(
    bin: &Path,
    args: &[std::ffi::OsString],
    socket_path: &Path,
    operator_token: &str,
) -> std::io::Result<()> {
    let mut cmd = prepare_daemon_command(bin, socket_path, operator_token);
    // Holder-split launch (phase 6): `--brain <cm-daemon>` when
    // CM_HOLDER_BINARY selected the split; empty otherwise.
    cmd.args(args);
    apply_detach_pre_exec(&mut cmd);
    let child = cmd.spawn()?;
    // Reap the child if it exits before us. `setsid()` in pre_exec
    // detaches the session (so terminal SIGHUP can't kill the
    // daemon) but the kernel PPID is still the TUI's PID — the TUI
    // remains responsible for `wait()`ing on the daemon if it exits
    // first. Without an explicit waiter the daemon's `task_struct`
    // would linger as a zombie pinned to the TUI's PID table until
    // the TUI itself exits.
    //
    // Choice: spawn a one-shot waiter thread instead of a classic
    // double-fork. Simpler (no intermediate process to manage), and
    // the thread blocks on `child.wait()` for the daemon's entire
    // lifetime — when the TUI exits, the thread dies with it and
    // init inherits any still-running daemon. Either approach
    // works; the waiter is the one we own.
    std::thread::Builder::new()
        .name("cm-daemon-waiter".into())
        .spawn(move || {
            let mut child = child;
            let _ = child.wait();
        })
        .ok();
    Ok(())
}

/// Build the `Command` that `spawn_daemon_binary` will launch,
/// **without** applying `pre_exec` or spawning. Extracted so unit
/// tests can inspect the prepared env (slice-10c-b review: the
/// "they match" assertion between the probe path and the child env
/// path).
///
/// Production callers should use [`spawn_daemon_binary`]; calling
/// this directly skips the detach setup and would tether the daemon
/// to the TUI's session.
pub(crate) fn prepare_daemon_command(
    bin: &Path,
    abs_socket: &Path,
    operator_token: &str,
) -> std::process::Command {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(bin);
    cmd.env("CM_DAEMON_SOCKET", abs_socket)
        .env(OPERATOR_TOKEN_ENV, operator_token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Install the `setsid` + `chdir("/")` pre_exec hook on `cmd`.
/// Extracted so the unit test can exercise the same detach setup
/// against any binary (we test against `/bin/sleep`, which is
/// available everywhere `cm-daemon` would also be).
///
/// `pre_exec` runs between `fork(2)` and `exec(2)` in the child, so
/// the closure must use only async-signal-safe syscalls — `setsid`
/// and `chdir` both qualify per POSIX. The `unsafe` block is
/// unavoidable because `pre_exec` can be a footgun (running in the
/// half-initialized child); the documented constraint above plus the
/// minimal closure body keep that risk bounded.
fn apply_detach_pre_exec(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs in the child between fork and exec
    // and uses only async-signal-safe POSIX calls. No allocation,
    // no Rust I/O, no shared state. Both `setsid` and `chdir` are
    // explicitly listed by POSIX as async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            // "/\\0" is a NUL-terminated byte literal; the cast
            // matches `libc::c_char` (i8 on Linux, can be u8 on
            // some platforms — the cast handles both).
            let root: &[u8] = b"/\0";
            if libc::chdir(root.as_ptr() as *const libc::c_char) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Warn at TUI startup if the `cm-daemon` binary on disk is newer than
/// the connected daemon's socket file mtime — i.e., the binary was
/// rebuilt but the running daemon process is still using older
/// in-memory code. Code changes that touch the daemon (mcp_start_session
/// semantics, dispatcher arms, session lifecycle, auth) will NOT be
/// live until the daemon is killed and restarted with the new binary.
///
/// The socket file's mtime is set when the daemon binds it at startup,
/// so it's a reliable proxy for daemon start time without having to
/// shell out to `ps` for the running PID.
///
/// Best-effort: if any of the metadata lookups fail (binary not found,
/// socket missing, mtime not available on this filesystem), this
/// returns silently. The warning is a usability nudge, not a
/// correctness contract.
///
/// See `scripts/cm-redeploy` for the clean rebuild+restart workflow.
pub fn warn_if_daemon_is_stale(socket_path: &Path) {
    let bin_path = match locate_daemon_binary() {
        Ok(p) => p,
        Err(_) => return,
    };
    let bin_mtime = match std::fs::metadata(&bin_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return,
    };
    let sock_mtime = match std::fs::metadata(socket_path).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return,
    };
    // 60s tolerance: a clean redeploy legitimately writes both the
    // binary and (later) the new socket within a tight window — the
    // binary's mtime can be a few seconds newer than the socket without
    // the daemon being stale. Without slop we'd false-positive on every
    // restart-immediately-after-build cycle.
    const TOLERANCE: Duration = Duration::from_secs(60);
    if let Ok(age) = bin_mtime.duration_since(sock_mtime + TOLERANCE) {
        eprintln!(
            "WARNING: cm-daemon binary at {} was rebuilt ~{}s after the running daemon started.\n\
             The running daemon is using OLD in-memory code. Code changes that touch the daemon\n\
             (mcp_start_session, dispatcher, session lifecycle, auth) are NOT live until the\n\
             daemon is restarted. Run scripts/cm-redeploy to rebuild and load the new binary.",
            bin_path.display(),
            age.as_secs(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use tempfile::TempDir;

    /// Crate-wide env-lock alias for this module's HOME/env-mutating
    /// tests. Matches the daemon's `env_lock()` discipline — see
    /// `daemon/src/test_support.rs` for the rationale.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::home_lock()
    }

    /// Test-only spawner that creates a Unix listener at the given
    /// path on a background thread. The listener accepts one
    /// connection then drops it — enough to make `try_connect`
    /// succeed on the polling loop.
    fn spawn_cooperating_listener(path: &Path) -> std::io::Result<()> {
        let p = path.to_path_buf();
        thread::spawn(move || {
            // Small delay so the auto-launch wait loop has a chance
            // to fail at least one probe — exercises the polling
            // path rather than a happy first-try connect.
            thread::sleep(Duration::from_millis(20));
            let listener = match UnixListener::bind(&p) {
                Ok(l) => l,
                Err(_) => return,
            };
            // Accept one connection and drop. The probe just needs
            // the connect() syscall to succeed; we don't read or
            // write anything.
            let _ = listener.accept();
            // Hold the listener so the socket stays bound until
            // ensure_daemon_running returns.
            thread::sleep(Duration::from_millis(200));
            drop(listener);
        });
        Ok(())
    }

    #[test]
    fn ensure_short_circuits_when_already_listening() {
        let _g = env_lock();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.sock");
        // Bind a listener up-front so the first try_connect succeeds.
        let _listener = UnixListener::bind(&path).unwrap();

        let spawn_called = Arc::new(AtomicBool::new(false));
        let flag = spawn_called.clone();
        let result = ensure_daemon_running(
            &path,
            || {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            },
            Duration::from_secs(2),
        );
        assert!(result.is_ok(), "already-listening path must succeed: {:?}", result);
        assert!(
            !spawn_called.load(Ordering::SeqCst),
            "spawn must not be called when daemon is already up"
        );
    }

    #[test]
    fn ensure_calls_spawner_and_succeeds_when_socket_appears() {
        let _g = env_lock();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.sock");

        let result = ensure_daemon_running(
            &path,
            || spawn_cooperating_listener(&path),
            Duration::from_secs(2),
        );
        assert!(result.is_ok(), "cooperating spawn must succeed: {:?}", result);
    }

    #[test]
    fn ensure_times_out_when_spawner_does_nothing() {
        let _g = env_lock();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.sock");

        // Spawner reports success but does nothing — the socket
        // never appears. Should TimeOut, NOT hang.
        let start = Instant::now();
        let result = ensure_daemon_running(
            &path,
            || Ok(()),
            Duration::from_millis(200),
        );
        let err = result.expect_err("absent daemon must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "timeout must honor the configured duration; took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn ensure_propagates_spawn_failure() {
        let _g = env_lock();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.sock");

        let result = ensure_daemon_running(
            &path,
            || {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "scripted permission denied",
                ))
            },
            Duration::from_secs(2),
        );
        let err = result.expect_err("spawn-side error must propagate");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
    }

    // 10f: deleted `maybe_ensure_is_noop_when_opt_in_unset` — its
    // premise (opt-in off → no-op Ok(true)) is contradictory under
    // the daemon-mandatory model. Replaced by T30 immediately
    // below + the always-on probe semantics exercised in
    // maybe_ensure_returns_false_on_timeout +
    // startup_strict_fatal_when_binary_missing (T31).

    /// T30 (10f) — daemon auto-launch is now UNCONDITIONAL with
    /// respect to `CM_USE_DAEMON_SOCKET`. With the env var
    /// scrubbed, the startup-strict wrapper still attempts the
    /// connect/spawn dance and returns Err (because we point at
    /// a binary that doesn't bind) rather than the pre-flip
    /// silent Ok(()) no-op.
    #[test]
    fn daemon_auto_launch_unconditional_post_flip() {
        let _g = env_lock();
        struct EnvGuard {
            prev_opt: Option<std::ffi::OsString>,
            prev_bin: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.prev_opt.take() {
                        Some(v) => std::env::set_var("CM_USE_DAEMON_SOCKET", v),
                        None => std::env::remove_var("CM_USE_DAEMON_SOCKET"),
                    }
                    match self.prev_bin.take() {
                        Some(v) => std::env::set_var("CM_DAEMON_BINARY", v),
                        None => std::env::remove_var("CM_DAEMON_BINARY"),
                    }
                }
            }
        }
        let _restore = EnvGuard {
            prev_opt: std::env::var_os("CM_USE_DAEMON_SOCKET"),
            prev_bin: std::env::var_os("CM_DAEMON_BINARY"),
        };
        unsafe {
            // Scrub the opt-in env var to prove the gate is gone.
            std::env::remove_var("CM_USE_DAEMON_SOCKET");
            // Point at /bin/true so locate_daemon_binary succeeds
            // and we get into the probe loop. /bin/true exits
            // immediately without binding → TimedOut.
            std::env::set_var("CM_DAEMON_BINARY", "/bin/true");
        }

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t30.sock");
        let result = ensure_daemon_at_startup_with_timeout(
            &path,
            "test-operator-token",
            Duration::from_millis(150),
        );

        let err = result.expect_err(
            "post-10f the wrapper must attempt the dance even with the \
             opt-in env scrubbed; got Ok which proves the gate regression",
        );
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::TimedOut,
            "expected TimedOut (proves we reached the probe loop), \
             got {:?}: {}",
            err.kind(),
            err,
        );
    }

    #[test]
    fn maybe_ensure_returns_false_on_timeout() {
        // Binary located but daemon never binds. The wrapper folds
        // TimedOut into Ok(false) so the caller can decide whether
        // to fatal-exit or retry. We bypass `locate_daemon_binary`
        // by pointing CM_DAEMON_BINARY at /bin/true — exists,
        // spawns, exits immediately, never creates the socket.
        let _g = env_lock();

        struct EnvGuard {
            prev_bin: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.prev_bin.take() {
                        Some(v) => std::env::set_var("CM_DAEMON_BINARY", v),
                        None => std::env::remove_var("CM_DAEMON_BINARY"),
                    }
                }
            }
        }
        let _g2 = EnvGuard {
            prev_bin: std::env::var_os("CM_DAEMON_BINARY"),
        };
        unsafe {
            std::env::set_var("CM_DAEMON_BINARY", "/bin/true");
        }

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("never-appears.sock");

        // Use a small explicit timeout so the test doesn't actually
        // wait 2s. Call ensure_daemon_running directly with the
        // same /bin/true spawner.
        let result = ensure_daemon_running(
            &path,
            || spawn_daemon_binary(Path::new("/bin/true"), &[], &path, "test-operator-token"),
            Duration::from_millis(150),
        );
        let err = result.expect_err("daemon binary that doesn't bind must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    // --- ensure_daemon_at_startup (strict opt-in) -------------------

    /// Reviewer-named regression fix (slice 10a review #1): when
    /// the daemon is already running, `ensure_daemon_at_startup`
    /// must short-circuit on `try_connect` without ever touching
    /// `locate_daemon_binary`. Without this, the common
    /// "daemon already up, restart TUI" workflow fails on any host
    /// where the binary isn't on the resolved path.
    ///
    /// Setup: bind a real listener on a tempdir socket so
    /// `try_connect` succeeds. Set `CM_DAEMON_BINARY` to a path
    /// that definitely doesn't exist. If the resolver is invoked,
    /// the function would return `Err(NotFound)` — instead it must
    /// return `Ok` because the existing daemon is reachable.
    #[test]
    fn startup_strict_skips_binary_lookup_when_daemon_already_running() {
        let _g = env_lock();

        struct EnvGuard {
            prev_opt: Option<std::ffi::OsString>,
            prev_bin: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.prev_opt.take() {
                        Some(v) => std::env::set_var("CM_USE_DAEMON_SOCKET", v),
                        None => std::env::remove_var("CM_USE_DAEMON_SOCKET"),
                    }
                    match self.prev_bin.take() {
                        Some(v) => std::env::set_var("CM_DAEMON_BINARY", v),
                        None => std::env::remove_var("CM_DAEMON_BINARY"),
                    }
                }
            }
        }
        let _restore = EnvGuard {
            prev_opt: std::env::var_os("CM_USE_DAEMON_SOCKET"),
            prev_bin: std::env::var_os("CM_DAEMON_BINARY"),
        };
        unsafe {
            std::env::set_var("CM_USE_DAEMON_SOCKET", "1");
            // If the resolver gets invoked, this path is what it
            // would try — and it doesn't exist. So a passing test
            // proves the resolver never ran.
            std::env::set_var(
                "CM_DAEMON_BINARY",
                "/nonexistent/cm-daemon-must-not-be-resolved",
            );
        }

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("already-up.sock");
        // Bind a listener — simulates an already-running daemon.
        let _listener = std::os::unix::net::UnixListener::bind(&path)
            .expect("bind sentinel listener");

        // With the daemon "already up", startup must succeed
        // without resolving the binary.
        let result = ensure_daemon_at_startup_with_timeout(
            &path,
            "test-operator-token",
            Duration::from_millis(150),
        );
        assert!(
            result.is_ok(),
            "already-running daemon must short-circuit without binary lookup: {:?}",
            result,
        );
    }

    // 10f: deleted `startup_strict_is_noop_when_opt_in_unset` —
    // contradictory under daemon-mandatory model. Replaced by
    // T31 (`startup_strict_fatal_when_binary_missing`) below
    // pinning the always-on probe semantics.

    /// 10f: with a daemon binary that spawns but never binds, the
    /// strict wrapper MUST return Err so `main()` exits before
    /// mcp_config injects `CM_DAEMON_SOCKET` into agents.
    /// /bin/true is a real binary that exits Ok immediately,
    /// reproducing "spawn succeeded but socket never appeared."
    #[test]
    fn startup_strict_fatal_when_daemon_never_binds() {
        let _g = env_lock();

        struct EnvGuard {
            prev_bin: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.prev_bin.take() {
                        Some(v) => std::env::set_var("CM_DAEMON_BINARY", v),
                        None => std::env::remove_var("CM_DAEMON_BINARY"),
                    }
                }
            }
        }
        let _restore = EnvGuard {
            prev_bin: std::env::var_os("CM_DAEMON_BINARY"),
        };
        unsafe {
            std::env::set_var("CM_DAEMON_BINARY", "/bin/true");
        }

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("never-binds.sock");

        let result = ensure_daemon_at_startup_with_timeout(
            &path,
            "test-operator-token",
            Duration::from_millis(150),
        );
        let err = result.expect_err("daemon doesn't bind must be fatal");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::TimedOut,
            "expected TimedOut, got {:?}: {}",
            err.kind(),
            err
        );
    }

    /// Companion to the strict-fatal test: opt-in ON with a
    /// nonexistent binary surfaces NotFound. Important to test
    /// because the binary-not-found path lives in
    /// `locate_daemon_binary`, not in the polling loop — without
    /// this test, a regression there would let the strict wrapper
    /// silently succeed.
    /// T31 (10f) — daemon binary missing → strict startup fails
    /// with a clear error. Renamed from
    /// `startup_strict_fatal_when_opt_in_on_and_binary_missing`
    /// and simplified to drop the opt-in env-var manipulation now
    /// that daemon mode is mandatory.
    #[test]
    fn startup_strict_fatal_when_binary_missing() {
        let _g = env_lock();
        struct EnvGuard {
            prev_bin: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.prev_bin.take() {
                        Some(v) => std::env::set_var("CM_DAEMON_BINARY", v),
                        None => std::env::remove_var("CM_DAEMON_BINARY"),
                    }
                }
            }
        }
        let _restore = EnvGuard {
            prev_bin: std::env::var_os("CM_DAEMON_BINARY"),
        };
        unsafe {
            // A path that definitely doesn't exist.
            // locate_daemon_binary rejects the env override and
            // falls through to the sibling-of-exe / dev-target
            // lookup, which in the test binary's location won't
            // find a cm-daemon either, so we get NotFound.
            std::env::set_var(
                "CM_DAEMON_BINARY",
                "/nonexistent/cm-daemon-strict-test",
            );
            // Make sure the sibling-of-exe path also can't find a
            // cm-daemon by accident. We can't actually move
            // current_exe, but the test binary's exe lives under
            // `target/debug/deps/` which has no cm-daemon sibling
            // — the workspace's cm-daemon is two directories up.
            // So the sibling-of-exe lookup naturally fails, which
            // is exactly the no-binary state this test wants.
        }

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nb.sock");

        let result = ensure_daemon_at_startup_with_timeout(
            &path,
            "test-operator-token",
            Duration::from_millis(150),
        );
        // Either NotFound (locate failed) is the expected behavior.
        // If the test binary happens to sit next to a real
        // cm-daemon (e.g. someone runs this from `target/debug/`),
        // we get TimedOut instead. Both indicate the strict
        // wrapper refused to succeed quietly, which is what
        // matters.
        let err = result.expect_err("missing binary must be fatal");
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::TimedOut
            ),
            "expected NotFound or TimedOut, got {:?}: {}",
            err.kind(),
            err,
        );
    }

    #[test]
    fn locate_daemon_binary_honors_env_override() {
        let _g = env_lock();
        let prev = std::env::var_os("CM_DAEMON_BINARY");
        // Use /bin/true which exists on every Linux system; tests
        // run on Linux per the repo target. Post slice-10c-b review
        // the result is canonicalized (symlinks resolved) — on
        // distros where /bin is a symlink to /usr/bin, the canonical
        // form is /usr/bin/true. Compare against fs::canonicalize
        // rather than the literal input so the test stays portable.
        unsafe { std::env::set_var("CM_DAEMON_BINARY", "/bin/true") };
        let result = locate_daemon_binary();
        match prev {
            Some(v) => unsafe { std::env::set_var("CM_DAEMON_BINARY", v) },
            None => unsafe { std::env::remove_var("CM_DAEMON_BINARY") },
        }
        let resolved = result.expect("env override must resolve");
        let expected =
            std::fs::canonicalize("/bin/true").expect("canonicalize /bin/true");
        assert_eq!(
            resolved, expected,
            "locate_daemon_binary must return the canonical form",
        );
    }

    /// Black-box check that `apply_detach_pre_exec` actually
    /// detaches: spawn `/bin/sleep` through it, read the child's
    /// session id via `getsid`, confirm it differs from the parent's
    /// and equals the child's own PID (the `setsid` postcondition —
    /// the caller becomes the leader of a new session whose ID is
    /// its PID).
    ///
    /// This is the most direct unit-level evidence that terminal
    /// SIGHUP can't propagate from the TUI to the daemon. The
    /// reviewer's named acceptance criterion (reconnect test kills
    /// the TUI mid-session, daemon survives) hinges on this.
    #[test]
    fn detached_command_runs_in_new_session() {
        use std::process::{Command, Stdio};
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        apply_detach_pre_exec(&mut cmd);

        let mut child = cmd.spawn().expect("spawn detached sleep");
        let child_pid = child.id() as libc::pid_t;

        // The child is alive here (sleep 5 takes 5s, we read its
        // SID well within that window). getsid(child_pid) returns
        // -1 only if the PID is invalid or the process is in a
        // different session we can't see, neither of which applies
        // to a child we just spawned.
        let child_sid = unsafe { libc::getsid(child_pid) };
        let our_sid = unsafe { libc::getsid(0) };

        // Always clean up the sleep before asserting — a panic
        // before kill() would leave a 5s zombie attached to the
        // test binary, which is rude.
        let _ = child.kill();
        let _ = child.wait();

        assert!(
            child_sid > 0,
            "getsid({}) returned {} (errno {})",
            child_pid,
            child_sid,
            std::io::Error::last_os_error(),
        );
        assert_ne!(
            child_sid, our_sid,
            "child SID {} equals parent SID {} — setsid did not run",
            child_sid, our_sid,
        );
        assert_eq!(
            child_sid, child_pid,
            "child SID {} != child PID {} — child is not its own session leader",
            child_sid, child_pid,
        );
    }

    #[test]
    fn locate_daemon_binary_rejects_nonexistent_override() {
        let _g = env_lock();
        let prev = std::env::var_os("CM_DAEMON_BINARY");
        unsafe {
            std::env::set_var("CM_DAEMON_BINARY", "/nonexistent/cm-daemon-xyz");
        }
        let result = locate_daemon_binary();
        match prev {
            Some(v) => unsafe { std::env::set_var("CM_DAEMON_BINARY", v) },
            None => unsafe { std::env::remove_var("CM_DAEMON_BINARY") },
        }
        // The env override didn't resolve — falls through to other
        // resolution branches. In the test environment the TUI's
        // current_exe sibling may or may not have cm-daemon, so we
        // accept either Ok or Err NotFound; what we're proving is
        // that the env override doesn't blindly succeed when the
        // file is missing.
        if let Ok(p) = result {
            assert_ne!(
                p, PathBuf::from("/nonexistent/cm-daemon-xyz"),
                "missing env-override path must not be returned",
            );
        }
    }

    // ============================================================
    // Slice-10c-b review fix: relative override paths must not
    // break across the daemon detach (pre_exec runs chdir("/")
    // before exec).
    // ============================================================

    /// Run `f` with the process cwd temporarily set to `dir`,
    /// restoring the original cwd on exit. Used in the
    /// absolutize tests below where we need a known cwd to assert
    /// against the joined output.
    fn with_cwd<R>(dir: &Path, f: impl FnOnce() -> R) -> R {
        let original = std::env::current_dir().expect("get original cwd");
        std::env::set_current_dir(dir).expect("set test cwd");
        let result = f();
        std::env::set_current_dir(&original).expect("restore cwd");
        result
    }

    #[test]
    fn absolutize_socket_path_passes_through_absolute() {
        // Already absolute: no work to do, no IO, identical PathBuf
        // (or at least equal-by-comparison — the contract).
        let abs = PathBuf::from("/run/user/1000/cm/daemon.sock");
        let out = absolutize_socket_path(&abs).expect("absolute passthrough");
        assert_eq!(out, abs, "absolute path must pass through unchanged");
        assert!(out.is_absolute());
    }

    #[test]
    fn absolutize_socket_path_joins_relative_to_cwd() {
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        // Canonicalize the tempdir path so we can compare against
        // the absolutized output without macOS-style /private prefix
        // mismatches (Linux-only here, but be defensive).
        let cwd_canonical = dir.path().canonicalize().expect("canonicalize tempdir");
        let result = with_cwd(&cwd_canonical, || {
            absolutize_socket_path(Path::new("daemon.sock"))
        });
        let resolved = result.expect("absolutize ok");
        assert!(
            resolved.is_absolute(),
            "relative path must become absolute: {}",
            resolved.display()
        );
        assert_eq!(
            resolved,
            cwd_canonical.join("daemon.sock"),
            "relative absolutization must join cwd",
        );
    }

    #[test]
    fn canonicalize_binary_path_returns_canonical_for_existing_relative_path() {
        let _g = env_lock();
        // Cwd at /tmp, then canonicalize "bin/true". We can't
        // assume /tmp contains "bin/true" — use the well-known
        // location instead.
        let bin_dir = Path::new("/bin");
        let result = with_cwd(bin_dir, || {
            canonicalize_binary_path(Path::new("true"))
        });
        let canonical = result.expect("canonical /bin/true");
        assert!(canonical.is_absolute(), "result must be absolute");
        // /bin/true exists everywhere we run; canonical form is
        // either /bin/true or /usr/bin/true depending on
        // distribution-specific symlinking. Both are valid.
        assert!(
            canonical.ends_with("true"),
            "canonical path must end with 'true': {}",
            canonical.display()
        );
    }

    #[test]
    fn canonicalize_binary_path_errors_for_nonexistent_file() {
        // The contract: canonicalize errors when target doesn't
        // exist, so an invalid override fails fast in the parent
        // rather than as a post-detach exec failure. The error
        // message must mention CM_DAEMON_BINARY so the operator
        // knows what to fix.
        let err = canonicalize_binary_path(Path::new("/nonexistent/no-such-binary"))
            .expect_err("nonexistent must surface as Err");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let msg = err.to_string();
        assert!(
            msg.contains("CM_DAEMON_BINARY"),
            "error must reference CM_DAEMON_BINARY so operator knows what to fix: {}",
            msg
        );
        assert!(
            msg.contains("chdir"),
            "error should explain WHY canonicalization matters: {}",
            msg
        );
    }

    #[test]
    fn locate_daemon_binary_canonicalizes_relative_env_override() {
        // The headline regression from the slice-10c-b review:
        // `CM_DAEMON_BINARY=relative/path` validates in the
        // parent's cwd, but the child's chdir("/") would invalidate
        // it. After this fix, locate_daemon_binary returns the
        // canonical absolute path.
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let dir_canonical = dir.path().canonicalize().expect("canonicalize tempdir");
        // Materialize a "binary" — any executable will do. We use
        // /bin/true symlinked, which is portable.
        let target = dir_canonical.join("daemon-stub");
        std::os::unix::fs::symlink("/bin/true", &target)
            .expect("symlink /bin/true into tempdir");

        let prev = std::env::var_os("CM_DAEMON_BINARY");
        let result = with_cwd(&dir_canonical, || {
            unsafe {
                // Relative override — pointing at the symlink we
                // just made, by basename only.
                std::env::set_var("CM_DAEMON_BINARY", "daemon-stub");
            }
            locate_daemon_binary()
        });
        unsafe {
            match prev {
                Some(v) => std::env::set_var("CM_DAEMON_BINARY", v),
                None => std::env::remove_var("CM_DAEMON_BINARY"),
            }
        }
        let resolved = result.expect("relative override must resolve");
        assert!(
            resolved.is_absolute(),
            "locate_daemon_binary must return absolute path: {}",
            resolved.display(),
        );
        // The symlink target is /bin/true (or /usr/bin/true on
        // some distros); fs::canonicalize follows symlinks.
        assert!(
            resolved.ends_with("true"),
            "canonical resolved path follows symlink: {}",
            resolved.display(),
        );
    }

    #[test]
    fn prepare_daemon_command_carries_socket_path_in_env() {
        // Sanity check on the command builder before the bigger
        // "they match" test below: prepare_daemon_command must
        // set CM_DAEMON_SOCKET to whatever the caller hands in.
        let cmd = prepare_daemon_command(
            Path::new("/bin/true"),
            Path::new("/run/user/1000/cm/daemon.sock"),
            "test-operator-token",
        );
        let env_pair = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new("CM_DAEMON_SOCKET"))
            .expect("CM_DAEMON_SOCKET must be in the prepared env");
        let env_val = env_pair.1.expect("env value present");
        assert_eq!(
            Path::new(env_val),
            Path::new("/run/user/1000/cm/daemon.sock"),
        );
    }

    #[test]
    fn relative_socket_path_resolves_identically_for_probe_and_child_env() {
        // The "they match" property the slice-10c-b reviewer
        // named as load-bearing: a relative CM_DAEMON_SOCKET must
        // produce a single absolute PathBuf that's consumed by:
        //   1. the probe path (try_connect / ensure_daemon_running),
        //   2. the child env (Command's CM_DAEMON_SOCKET).
        //
        // If the two are absolutized independently from
        // different cwds (or one is absolutized and the other
        // isn't), they diverge and the daemon and TUI bind/poll
        // on different inodes.
        //
        // This test runs `ensure_daemon_at_startup_with_timeout`
        // with the opt-in on, a relative socket path, and a
        // /bin/true binary stub (which spawns and exits without
        // binding). It captures both:
        //   - the absolutized path computed at the entry
        //     (absolutize_socket_path on the relative input),
        //   - the env CM_DAEMON_SOCKET passed to the child by
        //     prepare_daemon_command on the same value.
        // and asserts equality.
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let dir_canonical = dir.path().canonicalize().expect("canonicalize tempdir");

        // Two consumers must see the same absolute path.
        let expected_abs = with_cwd(&dir_canonical, || {
            absolutize_socket_path(Path::new("daemon.sock")).expect("absolutize ok")
        });
        let cmd_env_path = {
            let cmd = prepare_daemon_command(
                Path::new("/bin/true"),
                &expected_abs,
                "test-operator-token",
            );
            cmd.get_envs()
                .find(|(k, _)| *k == std::ffi::OsStr::new("CM_DAEMON_SOCKET"))
                .and_then(|(_, v)| v)
                .map(|v| PathBuf::from(v))
                .expect("CM_DAEMON_SOCKET set on Command")
        };

        // The load-bearing assertion: both consumers refer to
        // the SAME absolute path.
        assert_eq!(
            cmd_env_path, expected_abs,
            "Command env CM_DAEMON_SOCKET must equal the absolutized probe path",
        );
        assert!(
            expected_abs.is_absolute(),
            "absolutized path must be absolute (for cross-detach correctness): {}",
            expected_abs.display(),
        );
        assert_eq!(
            expected_abs,
            dir_canonical.join("daemon.sock"),
            "relative absolutization must join cwd",
        );
    }

    #[test]
    fn ensure_daemon_at_startup_canonicalizes_relative_socket_and_binary() {
        // End-to-end integration of both fixes through the public
        // entry. Setup:
        //   - CM_DAEMON_BINARY = symlink to /bin/true in tempdir
        //     (relative to chdir'd tempdir, so the canonicalization
        //     path is exercised)
        //   - relative socket path
        //   - chdir into tempdir (so the relative paths have a
        //     known root)
        // /bin/true spawns and exits without binding the socket,
        // so the probe loop times out — we want the TimedOut
        // result (proves we reached the spawn) plus structural
        // confirmation that absolutization happened.
        let _g = env_lock();
        let dir = TempDir::new().expect("tempdir");
        let dir_canonical = dir.path().canonicalize().expect("canonicalize tempdir");
        let target = dir_canonical.join("cm-daemon-stub");
        std::os::unix::fs::symlink("/bin/true", &target)
            .expect("symlink /bin/true into tempdir");

        struct EnvGuard {
            prev_bin: Option<std::ffi::OsString>,
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                unsafe {
                    match self.prev_bin.take() {
                        Some(v) => std::env::set_var("CM_DAEMON_BINARY", v),
                        None => std::env::remove_var("CM_DAEMON_BINARY"),
                    }
                }
            }
        }
        let _restore = EnvGuard {
            prev_bin: std::env::var_os("CM_DAEMON_BINARY"),
        };

        let result = with_cwd(&dir_canonical, || {
            unsafe {
                // Relative — basename only. Without canonicalization
                // this would resolve under "/" in the child and exec
                // would fail.
                std::env::set_var("CM_DAEMON_BINARY", "cm-daemon-stub");
            }
            ensure_daemon_at_startup_with_timeout(
                Path::new("daemon.sock"),
                "test-operator-token",
                Duration::from_millis(150),
            )
        });
        let err = result.expect_err(
            "/bin/true spawns + exits without binding, so probe must time out",
        );
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::TimedOut,
            "expected TimedOut (proves we reached spawn + probe with absolutized paths), got {:?}: {}",
            err.kind(),
            err,
        );
    }
}
