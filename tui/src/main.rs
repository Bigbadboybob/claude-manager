mod agent;
mod agent_memory;
mod api;
mod app;
mod attached_pty;
mod backend;
mod client_session;
mod config;
mod control;
mod daemon_launch;
mod host_pool;
mod hosts;
mod input;
mod manifest_watch;
mod mcp_config;
mod memory_cap;
mod planning;
mod preflight;
mod push_worker;
mod session;
mod session_watch;
mod term_shim;
mod terminal_widget;
#[cfg(test)]
mod test_support;
mod tls_dialer;
mod workflow;
mod workflow_watch;
// `worktree` relocated to the `cm-daemon` crate (slice 3 of
// doc/persistent-host-daemon.md). Callers use `cm_daemon::worktree::*`.

use std::io;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, Event as CrosstermEvent, KeyboardEnhancementFlags, poll as crossterm_poll,
    PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    EnableBracketedPaste, DisableBracketedPaste,
    EnableMouseCapture, DisableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, SIDEBAR_WIDTH};
use config::Config;

fn main() -> anyhow::Result<()> {
    let config = Config::load();

    // 10f default-flip: daemon mode is now mandatory. The TUI cannot
    // operate without the daemon — `mcp_config::build_env` injects
    // `CM_DAEMON_SOCKET` into every spawned MCP agent, and session
    // state lives on the daemon. A soft fallback would pin agents
    // to a dead socket and lose session ownership. Loud failure on
    // startup is the only safe shape.
    let daemon_socket = cm_daemon::default_socket_path();
    // Load (or generate-and-persist) the operator token BEFORE
    // spawning the daemon. The daemon reads the same value from
    // `CM_OPERATOR_TOKEN` in its spawn env and uses it to authenticate
    // every `Caller::Operator` RPC. See
    // `tui/src/daemon_launch.rs::load_or_create_operator_token`.
    let operator_token = match daemon_launch::load_or_create_operator_token() {
        Ok(t) => t,
        Err(e) => {
            eprintln!(
                "cm-tui: failed to load/create ~/.cm/operator-token: {}",
                e,
            );
            eprintln!(
                "cm-tui: the operator token is the shared secret the TUI presents to its own \
                 daemon. Without it, every daemon RPC will be unauthorized. Fix: ensure \
                 ~/.cm is owned by you and writable."
            );
            std::process::exit(1);
        }
    };
    if let Err(e) = daemon_launch::ensure_daemon_at_startup(&daemon_socket, &operator_token) {
        eprintln!(
            "cm-tui: failed to launch cm-daemon at {}: {}",
            daemon_socket.display(),
            e,
        );
        eprintln!(
            "cm-tui: cannot start without the daemon. Fixes:"
        );
        eprintln!(
            "  - Build the daemon: `cargo build -p cm-daemon`"
        );
        eprintln!(
            "  - Override the binary path: `CM_DAEMON_BINARY=/path/to/cm-daemon cm-tui`"
        );
        eprintln!(
            "  - Check permissions on ~/.cm/ (the daemon binds the socket there)"
        );
        std::process::exit(1);
    }

    // Warn (don't fail) if the cm-daemon binary on disk is newer
    // than the running daemon's socket — symptom of "I ran
    // `cargo build` but forgot to restart the daemon, so my code
    // changes aren't live." Best-effort; silent if metadata
    // lookups fail. See `scripts/cm-redeploy` for the clean
    // rebuild+restart workflow.
    daemon_launch::warn_if_daemon_is_stale(&daemon_socket);

    // Sub-2a Finding (round 3) #1: do NOT push an empty
    // `task.update_tree` at startup. The persistent-host
    // daemon may already hold a non-empty tree from a previous
    // TUI session — an unconditional empty push would wipe it
    // (see `methods::task_update_tree`: clear-then-extend
    // semantics; `client_session::tests::rpc_task_update_tree_replaces_on_second_push`
    // pins this). Before reconcile fires, the TUI has nothing
    // authoritative to publish; the empty `task.update_tree`
    // would lose information rather than add any. The first
    // `reconcile_tasks` call (which fires on
    // `BackendEvent::TasksUpdated` and calls
    // `push_task_tree_to_daemon` at the tail) is the
    // authoritative populator — until then, the daemon's
    // existing tree stands.
    //
    // Pre-fix this slot held an unconditional empty push that
    // wiped the daemon's tree on every TUI startup. If the API
    // was slow or unreachable, the daemon stayed wiped until
    // reconcile eventually fired — opening a window where
    // Session-caller descendant-task auth lost all parent
    // edges. Deleting the empty push closes that window.

    // Setup terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        )
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Physically wipe the screen before the first draw. EnterAlternateScreen
    // normally clears the alt buffer, but if the previous process exited
    // without running the LeaveAlternateScreen cleanup (panic/SIGKILL), the
    // terminal is already in the alt buffer and re-entering is a no-op on many
    // terminals — leaving the old frame on screen. ratatui's diff renderer
    // can't fix that: its previous buffer starts empty, so blank cells in the
    // first frame are never emitted and pre-existing content bleeds through
    // (ghosted sidebar, shell scrollback, desktop). terminal.clear() emits a
    // real ESC[2J and resets the previous buffer to force a full first repaint.
    // (The Clear *widget* in App::draw can't do this — it only wipes cells
    // ratatui itself drew in a prior frame, not pre-existing screen content.)
    terminal.clear()?;

    // Redirect stderr to a log file for the lifetime of the alt screen.
    // ratatui owns the terminal and assumes it is the only writer — but
    // ~60 `eprintln!` calls fire from background reconnect/watch threads
    // (push_worker, manifest_watch, workflow_watch) at arbitrary times.
    // Each writes raw text straight onto the alt screen, which both shows
    // garbage AND desyncs ratatui's previous-buffer model (a stray line can
    // scroll the screen, after which every incremental diff paints at the
    // wrong offset — the scattered-character + bleed-through artifacts that
    // never self-heal because the diff only repaints cells it thinks
    // changed). Pointing fd 2 at a file removes the whole class at the
    // source, including stderr from dependencies we don't control.
    // Diagnostics (and panic backtraces) are preserved in ~/.cm/cm-tui.log.
    let saved_stderr = redirect_stderr_to_log();

    let result = run(&mut terminal, config);

    // Restore terminal.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    // Restore the real stderr BEFORE returning so a fatal error from
    // `run()` (printed by main's `Termination` impl) reaches the user's
    // terminal instead of vanishing into the log.
    if let Some(saved) = saved_stderr {
        restore_stderr(saved);
    }

    result
}

/// Point the process's stderr (fd 2) at `~/.cm/cm-tui.log`, returning the
/// saved original fd so [`restore_stderr`] can put it back on exit.
/// Returns `None` (leaving stderr untouched) if the redirect can't be set
/// up — a missing `$HOME` or an unwritable `~/.cm` is non-fatal; the only
/// cost is that stray `eprintln!`s keep corrupting the screen, the exact
/// pre-existing behavior.
fn redirect_stderr_to_log() -> Option<OwnedFd> {
    let home = std::env::var_os("HOME")?;
    let dir = std::path::PathBuf::from(home).join(".cm");
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("cm-tui.log"))
        .ok()?;
    // SAFETY: dup/dup2 on STDERR_FILENO and a live file fd. `saved` is an
    // independent dup of the original stderr; `file`'s fd is dup2'd onto
    // fd 2, so fd 2 keeps its own reference to the open file description
    // and `file` is free to drop afterwards.
    let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
    if saved < 0 {
        return None;
    }
    let rc = unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
    if rc < 0 {
        unsafe { libc::close(saved) };
        return None;
    }
    Some(unsafe { OwnedFd::from_raw_fd(saved) })
}

/// Restore the original stderr saved by [`redirect_stderr_to_log`].
fn restore_stderr(saved: OwnedFd) {
    let fd = saved.into_raw_fd();
    // SAFETY: `fd` is the saved original stderr; dup2 it back onto fd 2 and
    // drop the now-redundant copy.
    unsafe {
        libc::dup2(fd, libc::STDERR_FILENO);
        libc::close(fd);
    }
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, config: Config) -> anyhow::Result<()> {
    let mut app = App::new(config);
    let mut last_draw = std::time::Instant::now();

    loop {
        // Drain ALL queued crossterm events.
        let phase_start = Instant::now();
        while crossterm_poll(Duration::ZERO)? {
            let event = event::read()?;

            if let CrosstermEvent::Resize(_cols, _rows) = event {
                app.needs_redraw = true;
                // Force a full repaint after a resize. ratatui clears on a
                // dimension change, but a same-size SIGWINCH (font/zoom
                // change, some multiplexers) leaves its buffers untouched
                // and the screen can desync — wipe unconditionally.
                app.force_clear = true;
                break;
            }

            app.handle_event(&event);

            if app.should_quit {
                break;
            }
        }
        log_slow_phase("input", phase_start.elapsed());

        if app.should_quit {
            break;
        }

        // Drain events from terminal sessions, backend, and planning editor.
        let t = Instant::now();
        app.drain_terminal_events();
        log_slow_phase("drain_terminal_events", t.elapsed());

        let t = Instant::now();
        app.drain_backend_events();
        log_slow_phase("drain_backend_events", t.elapsed());

        let t = Instant::now();
        app.drain_planning_events();
        log_slow_phase("drain_planning_events", t.elapsed());

        let t = Instant::now();
        app.drain_control_events();
        log_slow_phase("drain_control_events", t.elapsed());

        let t = Instant::now();
        app.drain_memory_kill_events();
        log_slow_phase("drain_memory_kill_events", t.elapsed());

        // 10e-c: drain ManifestDiff frames from the
        // manifest.watch consumer (daemon-mode opt-in only;
        // no-op in legacy single-process mode).
        let t = Instant::now();
        app.drain_manifest_watch_events();
        log_slow_phase("drain_manifest_watch_events", t.elapsed());

        // 11d: drain WorkflowWatchEvent frames from the
        // events.subscribe consumer. Same shape as the
        // manifest_watch drain — single-thread apply under the
        // main loop's invariant.
        let t = Instant::now();
        app.drain_workflow_watch_events();
        log_slow_phase("drain_workflow_watch_events", t.elapsed());

        // Render at most ~120fps, but only when something changed.
        let now = std::time::Instant::now();
        if app.needs_redraw && now.duration_since(last_draw) >= Duration::from_millis(8) {
            // Consume a pending full-repaint request (resize / A-r refresh).
            // terminal.clear() emits ESC[2J and resets ratatui's previous
            // buffer, so the draw below repaints every cell — curing any
            // diff-model desync. Cheap because it only fires on explicit
            // request, never in steady state.
            if app.force_clear {
                terminal.clear()?;
                app.force_clear = false;
            }
            let t = Instant::now();
            terminal.draw(|frame| {
                let area = frame.area();
                // Sidebar + the terminal panel's left/right border = SIDEBAR_WIDTH + 2.
                let term_cols = area.width.saturating_sub(SIDEBAR_WIDTH + 2);
                let term_rows = area.height.saturating_sub(3);
                if (term_cols, term_rows) != app.last_term_size {
                    app.resize_terminals(term_cols, term_rows);
                }

                // Update planning layout before draw.
                app.planning.update_layout(area.width, area.height);

                app.draw(frame);
            })?;
            log_slow_phase("draw", t.elapsed());
            app.needs_redraw = false;
            last_draw = now;
        } else {
            // Yield CPU briefly when idle.
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    Ok(())
}

/// If a single phase of the main loop took longer than the threshold, append
/// a line to `~/.cm/slow-ticks.log`. Used to attribute UI freezes to a specific
/// phase so we can see whether they come from event drain, backend poll,
/// rendering, or something else.
///
/// Threshold: 200ms — anything visibly janky to the user should land in here,
/// nothing routine should.
fn log_slow_phase(phase: &str, elapsed: Duration) {
    const THRESHOLD: Duration = Duration::from_millis(200);
    if elapsed < THRESHOLD {
        return;
    }
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = std::path::PathBuf::from(home).join(".cm");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("slow-ticks.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{} phase={} elapsed_ms={}", now, phase, elapsed.as_millis());
    }
}
