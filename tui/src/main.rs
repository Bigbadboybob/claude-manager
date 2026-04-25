mod api;
mod app;
mod backend;
mod config;
mod input;
mod planning;
mod session;
mod terminal_widget;
mod workflow;
mod worktree;

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

use app::App;
use config::Config;

fn main() -> anyhow::Result<()> {
    let config = Config::load();

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

        // Render at most ~120fps, but only when something changed.
        let now = std::time::Instant::now();
        if app.needs_redraw && now.duration_since(last_draw) >= Duration::from_millis(8) {
            let t = Instant::now();
            terminal.draw(|frame| {
                let area = frame.area();
                let term_cols = area.width.saturating_sub(32);
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
