mod api;
mod app;
mod backend;
mod config;
mod input;
mod session;
mod terminal_widget;
mod worktree;

use std::io;
use std::time::Duration;

use crossterm::event::{
    self, Event as CrosstermEvent, KeyboardEnhancementFlags, poll as crossterm_poll,
    PushKeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
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
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;

    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, config: Config) -> anyhow::Result<()> {
    let mut app = App::new(config);

    loop {
        // Draw.
        terminal.draw(|frame| {
            let area = frame.area();
            let term_cols = area.width.saturating_sub(32);
            let term_rows = area.height.saturating_sub(2);
            if (term_cols, term_rows) != app.last_term_size {
                app.resize_terminals(term_cols, term_rows);
            }

            app.draw(frame);
        })?;

        if app.should_quit {
            break;
        }

        // Poll for crossterm events first so last_write_at is set
        // before idle detection runs in drain_terminal_events.
        if crossterm_poll(Duration::from_millis(16))? {
            let event = event::read()?;

            if let CrosstermEvent::Resize(_cols, _rows) = event {
                continue;
            }

            app.handle_event(&event);
        }

        // Drain events from terminal sessions and backend.
        app.drain_terminal_events();
        app.drain_backend_events();
    }

    Ok(())
}
