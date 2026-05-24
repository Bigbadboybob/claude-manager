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

impl EventListener for EventProxy {
    fn send_event(&self, event: TermEvent) {
        let _ = self.tx.send(event);
    }
}

/// A terminal session wrapping alacritty's Term + PTY + EventLoop.
pub struct Session {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub sender: EventLoopSender,
    /// Direct fd to PTY master for low-latency input writes.
    pty_writer: File,
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
}

impl Session {
    /// Spawn a new terminal session running the given shell command.
    /// When `memory_cap` is `Some`, the spawn is rewritten to run inside
    /// a `systemd-run --user --scope` transient unit with the configured
    /// `MemoryHigh`/`MemoryMax` / `MemorySwapMax=0` properties. The
    /// caller is responsible for only passing `Some` when preflight
    /// succeeded; see `App::spawn_agent_session`.
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
        let event_proxy = EventProxy { tx: event_tx };

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
        let (final_shell, final_args, cgroup_path) = wrap_with_systemd_run(shell, args, &memory_cap);

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

        // When wrapped, verify the systemd-run scope actually
        // materialized AND has a process inside — `cgroup.procs`
        // becomes non-empty as soon as systemd has set up the scope
        // and the wrapper has exec'd into the agent. `tty::new` only
        // knows that the systemd-run *binary* spawned; it can't see
        // whether systemd-run then failed to create the scope
        // (unit-name collision, cgroup-v2 quirk, user-manager refusal),
        // and bare path existence is satisfied by stale scopes left
        // over from previous TUI runs that didn't clean up. Without
        // this check, the caller would swap a dead handle in over the
        // previous live agent. See DESIGN_MEMORY_CAP.md § Failure modes.
        //
        // Bail before `EventLoop::new`/`spawn` so we don't leak the
        // event loop on a dead PTY; dropping `pty` and `pty_writer`
        // here closes both fds.
        if let Some(ref expected_cgroup) = cgroup_path {
            if !wait_for_cgroup_active(expected_cgroup, Duration::from_millis(500)) {
                return Err(anyhow::anyhow!(
                    "memory-cap scope did not materialize as active at {} within 500ms — \
                     systemd-run likely failed (unit-name collision, scope refusal, \
                     or stale lingering cgroup with no processes). Refusing to return a \
                     Session over a dead PTY child.",
                    expected_cgroup.display()
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
            pty_writer,
            event_rx,
            title: format!("{} {}", shell, args.join(" ")),
            exited: false,
            wakeup_times: Vec::new(),
            memory_cap,
            cgroup_path,
        })
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

        let total = data.len();
        let mut pos = 0;
        let start = Instant::now();
        while pos < total {
            match (&self.pty_writer).write(&data[pos..]) {
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

/// Spawn an agent session with cap + env wrapping. The single helper
/// every agent-spawning call site goes through (DESIGN_MEMORY_CAP.md
/// § Code changes). Owns:
///   1. `CM_TUI_SESSION_ID` env-population so the agent and any Bash
///      tool it spawns see the value (today `mcp_config.rs:41-58`
///      only injects it into the MCP child's env).
///   2. Cap lookup against `Config::memory_cap_for(session_type)`,
///      gated on preflight success.
///   3. Watcher thread spawn when capped.
///
/// Tests and infra (`gcloud`, `/bin/bash`, `/bin/true`) bypass this
/// helper and call `Session::new` directly with `memory_cap = None`.
pub fn spawn_agent_session(
    session_type: &str,
    session_uid: &str,
    program: &str,
    args: &[String],
    cols: u16,
    rows: u16,
    working_dir: Option<PathBuf>,
    mut env: HashMap<String, String>,
    config: &crate::config::Config,
    cap_status: &crate::memory_cap::MemoryCapAvailability,
    kill_tx: &mpsc::Sender<crate::session_watch::MemoryKillEvent>,
) -> anyhow::Result<Session> {
    // (1) Export CM_TUI_SESSION_ID into the agent's process env. The
    // agent and any Bash tool inheriting env from it will now see the
    // value, which is required for the agent to find its own
    // ~/.cm/memory_kills/$CM_TUI_SESSION_ID.jsonl on a SIGKILL.
    env.insert("CM_TUI_SESSION_ID".into(), session_uid.to_string());

    // (2) Resolve the cap. Both the user-configured limits and
    // preflight success are required.
    let memory_cap = match (cap_status, config.memory_cap_for(session_type)) {
        (
            crate::memory_cap::MemoryCapAvailability::Available { cgroup_prefix },
            Some((soft_bytes, hard_bytes)),
        ) => Some(MemoryCap {
            soft_bytes,
            hard_bytes,
            session_uid: session_uid.to_string(),
            cgroup_prefix: cgroup_prefix.clone(),
        }),
        _ => None,
    };

    // (3) Spawn the PTY (with or without the systemd-run wrapper).
    let cap_for_session = memory_cap.clone();
    let session = Session::new(
        program,
        args,
        cols,
        rows,
        working_dir,
        env,
        cap_for_session,
    )?;

    // (4) When capped, spawn the watcher. The thread terminates on
    // its own when the cgroup goes away (last process exited).
    if let (Some(cap), Some(cgroup_path)) = (memory_cap, session.cgroup_path.clone()) {
        crate::session_watch::spawn_watcher(
            cap.session_uid,
            cgroup_path,
            cap.soft_bytes,
            cap.hard_bytes,
            kill_tx.clone(),
        );
    }

    Ok(session)
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
fn wrap_with_systemd_run(
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
