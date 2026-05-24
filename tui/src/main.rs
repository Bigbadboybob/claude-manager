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
mod input;
mod mcp_config;
mod memory_cap;
mod planning;
mod preflight;
mod session;
mod session_watch;
mod term_shim;
mod terminal_widget;
#[cfg(test)]
mod test_support;
mod workflow;
// `worktree` relocated to the `cm-daemon` crate (slice 3 of
// doc/persistent-host-daemon.md). Callers use `cm_daemon::worktree::*`.

use std::io;
use std::io::Write;
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

    // Daemon auto-launch is gated on `CM_USE_DAEMON_SOCKET=1` (slice 11
    // + slice 13 of doc/persistent-host-daemon.md). With the opt-in
    // off (the current default), this is a no-op and the clean-home
    // path is byte-identical to today. With the opt-in on, the daemon
    // MUST come up — `mcp_config::build_env` also branches off the
    // same flag and injects `CM_DAEMON_SOCKET` into spawned MCP
    // agents, so a soft fallback would pin every agent to a dead
    // socket. Loud opt-in, loud failure: log and exit non-zero so the
    // user can see the problem and fix it instead of debugging
    // confusing MCP errors downstream.
    let daemon_socket = cm_daemon::default_socket_path();
    if let Err(e) = daemon_launch::ensure_daemon_at_startup(&daemon_socket) {
        eprintln!(
            "cm-tui: CM_USE_DAEMON_SOCKET=1 set but cm-daemon could not be launched at {}: {}",
            daemon_socket.display(),
            e,
        );
        eprintln!(
            "cm-tui: refusing to start with a broken daemon path. Either unset CM_USE_DAEMON_SOCKET or fix the daemon (build with `cargo build -p cm-daemon`, set CM_DAEMON_BINARY, check ~/.cm permissions)."
        );
        std::process::exit(1);
    }

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

    result
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

        // Render at most ~120fps, but only when something changed.
        let now = std::time::Instant::now();
        if app.needs_redraw && now.duration_since(last_draw) >= Duration::from_millis(8) {
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
