//! Path-resolution helpers shared between the TUI and the daemon.
//!
//! ## Why this module exists
//!
//! The daemon launch path runs `chdir("/")` in `pre_exec` before
//! `exec` (`tui/src/daemon_launch.rs::apply_detach_pre_exec`), so any
//! relative path observed by the parent and re-observed by the child
//! diverges:
//!
//! - **Parent (TUI)** sees a relative path resolve under its own cwd.
//! - **Child (cm-daemon)** sees the same relative path resolve under `/`.
//!
//! That divergence broke the daemon launch (parent probe couldn't
//! reach the daemon's bind path) and would break the MCP agent path
//! the same way (`tui/src/mcp_config.rs::build_env` injects
//! `CM_DAEMON_SOCKET` into spawned agents; agents would resolve
//! relative paths against their own cwd, not the daemon's).
//!
//! [`absolutize_socket_path`] is the single source of truth for
//! resolving a (possibly-relative) socket path to an absolute one
//! before it crosses any process boundary. Both `daemon_launch.rs`
//! and `mcp_config.rs::build_env` route through this — that's the
//! load-bearing "they match" property the slice-10c-b reviewer
//! flagged.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Read a socket-path env var with the same "empty means unset"
/// semantics the Python `control_client.default_socket_path` uses
/// (`.strip()` + falsy check). Returns:
///
///   - `None` when the var is unset, set to `""`, or set to a
///     whitespace-only value.
///   - `Some(trimmed)` otherwise, with leading/trailing whitespace
///     stripped.
///
/// ## Why this exists (slice-10c-b review)
///
/// Without this normalization, the Rust side accepts `""` as a
/// present-value override, then [`absolutize_socket_path`] joins
/// `""` to the cwd — daemon binds to (or polls for) the cwd
/// directory itself. That's not the failure the strict-fatal opt-in
/// is designed to surface, and it diverges from the Python
/// resolver's hygiene.
///
/// The empty-string entry that `tui/src/mcp_config.rs::build_env`
/// writes as an *authoritative override* relies on `""` meaning
/// "unset" — both sides must agree. This helper closes that gap
/// for every Rust read site (`cm_daemon::default_socket_path`,
/// `tui/src/control/server.rs::default_socket_path`).
///
/// ## What "unset" maps to
///
/// `None` returned here means callers fall through to their default
/// resolution (e.g. `$HOME/.cm/daemon.sock`). The caller decides;
/// this helper has no opinion about the fallback.
pub fn env_socket_override(var: &str) -> Option<String> {
    std::env::var(var)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Create `dir` (and any missing parents) with mode `0o700` —
/// the canonical "owner-only daemon-state subdirectory" perms.
/// Idempotent: a pre-existing directory has its mode re-asserted,
/// so accidental drift (someone `chmod 755`'d the dir manually)
/// is corrected on the next call.
///
/// ## Why this exists (slice-10c-c review fix #2)
///
/// `~/.cm/memory_kills/` holds per-session JSONL kill-log records
/// (cgroup OOM events with pids, comms, argv hashes, RSS). The
/// TUI's `tui/src/session_watch.rs::write_kill_log_to` always
/// hardened it to `0o700`; the daemon's
/// `daemon/src/reaper.rs::capture_baseline_for_spawn` regressed
/// to default umask (typically `0o755`). Both writers now route
/// through this helper so they can't drift again.
///
/// The companion file-mode contract is on the *caller*: open
/// records with `OpenOptions::new().create(true).append(true).mode(0o600)`.
/// This helper doesn't touch file modes — it only owns the
/// directory.
///
/// Linux-only (uses `mode_t` semantics from `std::os::unix`).
/// Matches the cfg-guard the rest of the crate relies on.
/// Wait up to `max_wait` for the cgroup at `path` to be both
/// present and *active* — `cgroup.procs` exists and contains at
/// least one numeric PID. A bare path-existence check would be
/// satisfied by a stale scope from a prior run that didn't clean
/// up (kill -9, crash, lingering child), and the daemon would
/// then declare the cap-protected spawn successful while actually
/// running over a dead scope.
///
/// Slice 10c-e-3b-fix4a: mirrors the TUI's local-spawn check
/// (`tui/src/session.rs::wait_for_cgroup_active`) so the daemon-
/// attached path has identical post-spawn verification semantics.
/// Pre-fix4a, the daemon just echoed the predicted cgroup_path on
/// the response without verifying anything — a partial
/// `systemd-run` failure (cgroup-namespace issue, transient
/// systemd error, scope-setup race) would silently violate the
/// memory-cap acceptance criterion.
///
/// Linux-only (cgroupfs path semantics). Returns `true` when at
/// least one numeric PID is present in `cgroup.procs` before the
/// deadline; `false` on timeout.
pub fn wait_for_cgroup_active(path: &Path, max_wait: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + max_wait;
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
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// `$HOME/.cm/memory_kills` — the canonical kill-log directory.
/// Used by both the TUI's local-spawn `session_watch` writer and
/// the daemon's spawn path (`SpawnParams::kills_dir`) so the
/// reaper can correlate cap kills against the per-spawn baseline.
///
/// Slice 10c-e-3b-fix2: introduced so the daemon's `start_session`
/// has a one-line resolver to populate `kills_dir` when the TUI
/// indicates a memory cap was applied. Daemon-spawned sessions
/// without a cap leave `kills_dir = None` (the reaper skips the
/// baseline + probe entirely).
///
/// `None` when `$HOME` isn't set (unusual; the daemon's bind path
/// has a similar fallback elsewhere).
pub fn default_kills_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cm/memory_kills"))
}

pub fn ensure_dot_cm_subdir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::metadata(dir)?;
    let mut perms = meta.permissions();
    perms.set_mode(0o700);
    std::fs::set_permissions(dir, perms)?;
    Ok(())
}

/// Resolve `path` to an absolute path. Passes through when already
/// absolute; joins with `std::env::current_dir()` when relative.
/// **Does not canonicalize** — sockets don't exist on disk at
/// resolution time and `fs::canonicalize` would error.
///
/// Why both consumers (TUI launch path and MCP env injection) need
/// this: see the [module docs](self).
///
/// Returns an `io::Error` when the path is relative AND
/// `current_dir()` fails (rare; typically means the cwd was unlinked
/// out from under the process). The error message names the source
/// var so an operator can correlate.
pub fn absolutize_socket_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!(
                "current_dir() failed while absolutizing socket path '{}': {} (consider setting an absolute path in CM_DAEMON_SOCKET / CM_TUI_SOCKET)",
                path.display(),
                e,
            ),
        )
    })?;
    Ok(cwd.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_path_passes_through_unchanged() {
        let abs = PathBuf::from("/run/user/1000/cm/daemon.sock");
        let out = absolutize_socket_path(&abs).expect("absolute passthrough");
        assert_eq!(out, abs);
        assert!(out.is_absolute());
    }

    #[test]
    fn relative_path_joins_with_current_dir() {
        let out = absolutize_socket_path(Path::new("rel/some.sock"))
            .expect("relative absolutization");
        assert!(out.is_absolute(), "result must be absolute: {}", out.display());
        let expected = std::env::current_dir()
            .expect("cwd")
            .join("rel/some.sock");
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_path_joins_with_current_dir() {
        // An empty PathBuf is treated as relative by is_absolute().
        // Joining it with cwd yields cwd — surprising but well-defined
        // (and the only meaningful behavior given the input). The
        // `env_socket_override` helper below is what prevents an
        // empty env var from reaching this fn in the first place.
        let out =
            absolutize_socket_path(Path::new("")).expect("empty path absolutization");
        let expected = std::env::current_dir().expect("cwd");
        assert_eq!(out, expected);
    }

    // --- env_socket_override -------------------------------------------------
    //
    // The "empty means unset" contract matches Python's
    // `control_client.default_socket_path` (which does `.strip()` and
    // a falsy check). Every Rust read site of `CM_DAEMON_SOCKET` /
    // `CM_TUI_SOCKET` routes through this helper so both sides agree.
    //
    // Tests are serialized through `crate::test_support::env_lock`
    // because they mutate process-global env state.

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env_lock()
    }

    /// Save+restore guard for a single env var. Set on construction,
    /// restored on Drop — so a panicking assert can't leak the test's
    /// pollution into adjacent tests under the same `env_lock`.
    struct EnvGuard {
        key: &'static str,
        prev: Option<std::ffi::OsString>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var_os(key);
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prev: prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match self.prev.take() {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    #[test]
    fn env_socket_override_returns_none_when_unset() {
        let _g = env_lock();
        let _unset = EnvGuard::unset("CM_PATH_TEST_VAR_UNSET");
        assert_eq!(env_socket_override("CM_PATH_TEST_VAR_UNSET"), None);
    }

    #[test]
    fn env_socket_override_returns_none_for_empty_string() {
        // The named regression contract: `export CM_DAEMON_SOCKET=`
        // (literal empty) is treated as "unset", matching the
        // Python resolver. Without this, the Rust side absolutizes
        // "" to the cwd and the daemon binds the worktree directory.
        let _g = env_lock();
        let _set = EnvGuard::set("CM_PATH_TEST_VAR_EMPTY", "");
        assert_eq!(env_socket_override("CM_PATH_TEST_VAR_EMPTY"), None);
    }

    #[test]
    fn env_socket_override_returns_none_for_whitespace_only() {
        // Tabs, spaces, newlines all qualify as "effectively unset".
        // Shell expansions can produce these by accident (e.g.
        // `export CM_DAEMON_SOCKET="$missing"` where $missing is
        // unset and a wrapping shell adds whitespace).
        let _g = env_lock();
        let _set = EnvGuard::set("CM_PATH_TEST_VAR_WS", "  \t\n  ");
        assert_eq!(env_socket_override("CM_PATH_TEST_VAR_WS"), None);
    }

    #[test]
    fn env_socket_override_returns_trimmed_value_for_padded_input() {
        // Surrounding whitespace is stripped. The path itself is
        // preserved verbatim. (No canonicalization, no
        // absolutization — those happen at the call site via
        // `absolutize_socket_path`.)
        let _g = env_lock();
        let _set = EnvGuard::set("CM_PATH_TEST_VAR_PAD", "  /tmp/foo.sock  ");
        assert_eq!(
            env_socket_override("CM_PATH_TEST_VAR_PAD"),
            Some("/tmp/foo.sock".into()),
        );
    }

    #[test]
    fn env_socket_override_returns_value_unchanged_when_no_whitespace() {
        let _g = env_lock();
        let _set = EnvGuard::set("CM_PATH_TEST_VAR_OK", "/run/cm.sock");
        assert_eq!(
            env_socket_override("CM_PATH_TEST_VAR_OK"),
            Some("/run/cm.sock".into()),
        );
    }

    #[test]
    fn env_socket_override_preserves_internal_whitespace_in_paths() {
        // POSIX paths can contain spaces. Only leading/trailing
        // whitespace is stripped; internal whitespace is part of
        // the path identity.
        let _g = env_lock();
        let _set =
            EnvGuard::set("CM_PATH_TEST_VAR_SPACES", "/tmp/path with spaces.sock");
        assert_eq!(
            env_socket_override("CM_PATH_TEST_VAR_SPACES"),
            Some("/tmp/path with spaces.sock".into()),
        );
    }

    // --- ensure_dot_cm_subdir ------------------------------------------------
    //
    // The named regression contract from slice-10c-c review #2:
    // memory-kills directory must NOT widen perms to 0o755 under a
    // permissive process umask. Tests serialize via `env_lock`
    // because `libc::umask` is process-global.

    /// RAII guard that flips the process umask to `mask` for the
    /// scope's lifetime and restores it on drop.
    struct UmaskGuard {
        previous: libc::mode_t,
    }
    impl UmaskGuard {
        fn set(mask: libc::mode_t) -> Self {
            // SAFETY: umask(2) mutates a process-global field.
            // The crate's env_lock serializes umask/HOME-mutating
            // tests; restored on Drop unconditionally.
            let previous = unsafe { libc::umask(mask) };
            Self { previous }
        }
    }
    impl Drop for UmaskGuard {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.previous);
            }
        }
    }

    #[test]
    fn ensure_dot_cm_subdir_creates_with_0700_under_permissive_umask() {
        // Named acceptance contract: under a permissive umask
        // (0o000), `create_dir_all` alone would land at 0o777.
        // The helper must post-create chmod to 0o700.
        let _lock = env_lock();
        let _umask = UmaskGuard::set(0o000);
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("memory_kills");

        ensure_dot_cm_subdir(&target).expect("create ok");
        let meta = std::fs::metadata(&target).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o700,
            "directory must be 0o700 regardless of umask (got 0o{:o})",
            mode,
        );
    }

    #[test]
    fn ensure_dot_cm_subdir_corrects_existing_drift() {
        // Idempotency + drift correction: a pre-existing directory
        // at 0o755 must be tightened to 0o700 on next call.
        let _lock = env_lock();
        let _umask = UmaskGuard::set(0o000);
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("drift");

        // Create at 0o755 directly to simulate drift.
        std::fs::create_dir(&target).unwrap();
        let mut perms = std::fs::metadata(&target).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target, perms).unwrap();

        ensure_dot_cm_subdir(&target).expect("re-call ok");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "drifted directory must be re-tightened (got 0o{:o})",
            mode,
        );
    }

    #[test]
    fn ensure_dot_cm_subdir_creates_parents() {
        // create_dir_all behavior: missing parents are created.
        // The mode contract applies to the leaf, not the parents
        // (matching how `tui/src/session_watch.rs::write_kill_log_to`
        // operated pre-refactor — the leaf is the security
        // boundary).
        let _lock = env_lock();
        let _umask = UmaskGuard::set(0o000);
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("a/b/c/memory_kills");

        ensure_dot_cm_subdir(&nested).expect("nested create ok");
        let leaf_mode = std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        assert_eq!(leaf_mode, 0o700, "leaf must be 0o700");
        // Parents exist; their modes follow umask.
        assert!(tmp.path().join("a/b/c").is_dir());
    }
}
