//! Grid color picker for the A-e settings forms (2026-10-20). Replaces the
//! ←/→ cycle through what is now a 28-entry palette: on a form's Color
//! field, Enter / Space / ← / → open the grid; arrows (or h/j/k/l) move and
//! preview the choice live on the form, Enter keeps it, Esc restores the
//! value the field had when the grid opened.

use super::*;
use crate::theme;

#[derive(Clone, Debug)]
pub(crate) struct ColorGrid {
    pub cell: (usize, usize),
    /// The field's value when the grid opened, restored on Esc.
    pub original: Option<String>,
}

impl App {
    /// The open settings form's color field, when the form is one that has
    /// one and that field is focused.
    fn focused_color_field(&mut self) -> Option<&mut Option<String>> {
        match &mut self.input_mode {
            InputMode::SessionSettings { color, active_field: 6, .. }
            | InputMode::WorkspaceSettings { color, active_field: 1, .. }
            | InputMode::SectionSettings { color, active_field: 1, .. }
            | InputMode::TaskSettings { color, active_field: 1, .. } => Some(color),
            _ => None,
        }
    }

    /// Route a key to the grid. Returns `Some(handled)` when the grid owns
    /// the key: it is open, or the key opens it from a focused color field.
    pub(crate) fn color_grid_event(&mut self, event: &CrosstermEvent) -> Option<bool> {
        let CrosstermEvent::Key(key) = event else {
            return self.color_grid.is_some().then_some(true);
        };
        if key.kind == crossterm::event::KeyEventKind::Release {
            return self.color_grid.is_some().then_some(true);
        }
        let Some(grid) = self.color_grid.clone() else {
            let opens = matches!(
                key.code,
                KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
            ) && !key.modifiers.contains(KeyModifiers::ALT);
            let current = self.focused_color_field().filter(|_| opens)?.clone();
            self.color_grid = Some(ColorGrid {
                cell: theme::color_grid_position(current.as_deref()),
                original: current,
            });
            self.needs_redraw = true;
            return Some(true);
        };
        let step = |dr: i32, dc: i32| theme::color_grid_step(grid.cell, dr, dc);
        let next = match key.code {
            KeyCode::Char('u' | 'd') if key.modifiers == KeyModifiers::ALT => {
                let direction = if key.code == KeyCode::Char('u') { -1 } else { 1 };
                let first = theme::color_grid_step(grid.cell, direction, 0);
                Some(theme::color_grid_step(first, direction, 0))
            }
            KeyCode::Left | KeyCode::Char('h') => Some(step(0, -1)),
            KeyCode::Right | KeyCode::Char('l') => Some(step(0, 1)),
            KeyCode::Up | KeyCode::Char('k') => Some(step(-1, 0)),
            KeyCode::Down | KeyCode::Char('j') => Some(step(1, 0)),
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.color_grid = None;
                None
            }
            KeyCode::Esc => {
                if let Some(field) = self.focused_color_field() {
                    *field = grid.original.clone();
                }
                self.color_grid = None;
                None
            }
            _ => None,
        };
        if let Some(cell) = next {
            if let Some(field) = self.focused_color_field() {
                *field = theme::color_grid_value(cell);
            }
            if let Some(open) = self.color_grid.as_mut() {
                open.cell = cell;
            }
        }
        self.needs_redraw = true;
        Some(true)
    }

    /// The grid overlay, centered over the form.
    pub(crate) fn draw_color_grid(&self, frame: &mut Frame, area: Rect) {
        let Some(grid) = &self.color_grid else { return };
        let width = 42.min(area.width);
        let height = (theme::COLOR_GRID.len() as u16 + 5).min(area.height);
        let dialog = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, dialog);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::TEXT))
            .title(" Color ");
        let inner = block.inner(dialog);
        frame.render_widget(block, dialog);
        let dim = Style::default().fg(theme::DIM);
        let mut lines: Vec<Line> = Vec::new();
        for (r, row) in theme::COLOR_GRID.iter().enumerate() {
            let mut spans = vec![Span::raw(" ")];
            for (c, cell) in row.iter().enumerate() {
                let selected = grid.cell == (r, c);
                let (glyph, style) = match cell {
                    None => ("    ".to_string(), dim),
                    Some(None) => (if selected { "[··]" } else { " ·· " }.to_string(), dim),
                    Some(Some(name)) => {
                        let color = theme::user_color(name).unwrap_or(theme::DIM);
                        (
                            if selected { "[██]" } else { " ██ " }.to_string(),
                            Style::default().fg(color),
                        )
                    }
                };
                spans.push(Span::styled(glyph, style));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        let name = theme::color_grid_value(grid.cell).unwrap_or_else(|| "none".to_string());
        lines.push(Line::from(Span::styled(format!(" {name}"), Style::default().fg(theme::TEXT))));
        lines.push(Line::from(Span::styled(" arrows move · Enter pick · Esc cancel", dim)));
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(crate::config::Config {
            api_url: String::new(),
            api_token: String::new(),
            gcp_project: String::new(),
            gcp_zone: String::new(),
            repos: HashMap::new(),
        })
    }

    fn key(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn section_form(color: Option<&str>, active_field: u8) -> InputMode {
        InputMode::SectionSettings {
            section_id: None,
            name: "s".into(),
            color: color.map(str::to_string),
            active_field,
        }
    }

    fn form_color(app: &App) -> Option<String> {
        match &app.input_mode {
            InputMode::SectionSettings { color, .. } => color.clone(),
            _ => panic!("form closed"),
        }
    }

    #[test]
    fn the_grid_opens_on_the_color_field_previews_and_commits_or_restores() {
        let mut app = test_app();
        app.input_mode = section_form(Some("blue"), 0);
        assert_eq!(app.color_grid_event(&key(KeyCode::Enter)), None, "not on the color field");

        app.input_mode = section_form(Some("blue"), 1);
        assert_eq!(app.color_grid_event(&key(KeyCode::Enter)), Some(true));
        assert_eq!(app.color_grid.as_ref().unwrap().cell, (1, 5));
        app.color_grid_event(&key(KeyCode::Up));
        assert_eq!(form_color(&app).as_deref(), Some("light-blue"), "live preview");
        app.color_grid_event(&key(KeyCode::Esc));
        assert!(app.color_grid.is_none());
        assert_eq!(form_color(&app).as_deref(), Some("blue"), "Esc restores");

        app.color_grid_event(&key(KeyCode::Right));
        app.color_grid_event(&key(KeyCode::Down));
        app.color_grid_event(&key(KeyCode::Down));
        assert_eq!(form_color(&app).as_deref(), Some("slate"), "down into the neutrals row");
        app.color_grid_event(&key(KeyCode::Enter));
        assert!(app.color_grid.is_none());
        assert_eq!(form_color(&app).as_deref(), Some("slate"), "Enter keeps it");

        // "none" clears the color.
        app.color_grid_event(&key(KeyCode::Char(' ')));
        for _ in 0..4 {
            app.color_grid_event(&key(KeyCode::Left));
        }
        app.color_grid_event(&key(KeyCode::Enter));
        assert_eq!(form_color(&app), None);
    }

    #[test]
    fn the_grid_draws_every_cell() {
        let mut app = test_app();
        app.input_mode = section_form(None, 1);
        app.color_grid_event(&key(KeyCode::Enter));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 14)).unwrap();
        terminal.draw(|f| app.draw_color_grid(f, f.area())).unwrap();
        let buf = terminal.backend().buffer().clone();
        let text: String = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text.matches('█').count(), 2 * (theme::USER_COLORS.len()), "{text}");
        assert!(text.contains("none") && text.contains("Esc cancel"), "{text}");
    }
}
