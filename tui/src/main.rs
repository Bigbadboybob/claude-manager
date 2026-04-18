mod api;
mod app;
mod backend;
mod config;
mod input;
mod planning;
mod session;
mod terminal_widget;
mod worktree;

use std::io;
use std::time::Duration;

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

        if app.should_quit {
            break;
        }

        // Drain events from terminal sessions, backend, and planning editor.
        app.drain_terminal_events();
        app.drain_backend_events();
        app.drain_planning_events();

        // Render at most ~120fps, but only when something changed.
        let now = std::time::Instant::now();
        if app.needs_redraw && now.duration_since(last_draw) >= Duration::from_millis(8) {
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
            app.needs_redraw = false;
            last_draw = now;
        } else {
            // Yield CPU briefly when idle.
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    Ok(())
}
