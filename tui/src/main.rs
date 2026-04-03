mod app;
mod input;
mod session;
mod terminal_widget;

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, poll as crossterm_poll};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;

fn main() -> anyhow::Result<()> {
    // Setup terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal);

    // Restore terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> anyhow::Result<()> {
    let mut app = App::new();

    // Demo sessions showing all statuses.
    app.add_shell_session("Fix calibration test", "/bin/bash", &[]);
    app.add_shell_session("Refactor parser", "/bin/bash", &[]);
    app.entries.push(app::SessionEntry {
        name: "Add retry logic".to_string(),
        status: app::TaskStatus::Blocked,
        session: None,
    });
    app.entries.push(app::SessionEntry {
        name: "Update scraper".to_string(),
        status: app::TaskStatus::Backlog,
        session: None,
    });
    app.entries.push(app::SessionEntry {
        name: "Clean up types".to_string(),
        status: app::TaskStatus::Backlog,
        session: None,
    });

    loop {
        // Draw.
        terminal.draw(|frame| {
            // Update terminal size for new sessions.
            let area = frame.area();
            // Account for borders: the terminal inner area is smaller.
            let term_cols = area.width.saturating_sub(32); // sidebar + borders
            let term_rows = area.height.saturating_sub(2); // top + bottom border
            if (term_cols, term_rows) != app.last_term_size {
                app.resize_terminals(term_cols, term_rows);
            }

            app.draw(frame);
        })?;

        if app.should_quit {
            break;
        }

        // Drain terminal events (new content, exits).
        app.drain_terminal_events();

        // Poll for crossterm events with a short timeout for responsive rendering.
        if crossterm_poll(Duration::from_millis(16))? {
            let event = event::read()?;

            if let CrosstermEvent::Resize(_cols, _rows) = event {
                // The terminal will handle the resize on next draw.
                continue;
            }

            app.handle_event(&event);
        }
    }

    Ok(())
}
