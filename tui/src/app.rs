use std::time::Instant;

use alacritty_terminal::event::Event as TermEvent;
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::input;
use crate::session::Session;
use crate::terminal_widget::TerminalWidget;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPINNER_INTERVAL_MS: u128 = 80;

/// Task status for display in the side panel.
#[derive(Clone, Debug)]
pub enum TaskStatus {
    Running,
    Blocked,
    Backlog,
}

/// An entry in the session list �� may or may not have a live terminal.
pub struct SessionEntry {
    pub name: String,
    pub status: TaskStatus,
    pub session: Option<Session>,
}

pub struct App {
    pub entries: Vec<SessionEntry>,
    pub active: usize,
    pub should_quit: bool,
    pub last_term_size: (u16, u16),
    start_time: Instant,
}

impl App {
    pub fn new() -> Self {
        App {
            entries: Vec::new(),
            active: 0,
            should_quit: false,
            last_term_size: (80, 24),
            start_time: Instant::now(),
        }
    }

    fn spinner_frame(&self) -> &'static str {
        let elapsed = self.start_time.elapsed().as_millis();
        let idx = (elapsed / SPINNER_INTERVAL_MS) as usize % SPINNER_FRAMES.len();
        SPINNER_FRAMES[idx]
    }

    /// Add a demo session running a local shell.
    pub fn add_shell_session(&mut self, name: &str, shell: &str, args: &[String]) {
        let (cols, rows) = self.last_term_size;
        let session = Session::new(shell, args, cols, rows).ok();
        self.entries.push(SessionEntry {
            name: name.to_string(),
            status: TaskStatus::Running,
            session,
        });
    }

    /// Process all pending terminal events (non-blocking).
    pub fn drain_terminal_events(&mut self) {
        for entry in &mut self.entries {
            if let Some(ref mut session) = entry.session {
                while let Ok(event) = session.event_rx.try_recv() {
                    match event {
                        TermEvent::Exit | TermEvent::ChildExit(_) => {
                            session.exited = true;
                        }
                        TermEvent::Title(title) => {
                            session.title = title;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// Handle a crossterm event. Returns true if consumed by the app.
    pub fn handle_event(&mut self, event: &CrosstermEvent) -> bool {
        if let CrosstermEvent::Key(key) = event {
            if key.modifiers.contains(KeyModifiers::ALT) {
                match key.code {
                    KeyCode::Char('q') => {
                        self.should_quit = true;
                        return true;
                    }
                    KeyCode::Char('j') => {
                        if !self.entries.is_empty() {
                            self.active = (self.active + 1) % self.entries.len();
                        }
                        return true;
                    }
                    KeyCode::Char('k') => {
                        if !self.entries.is_empty() {
                            self.active =
                                (self.active + self.entries.len() - 1) % self.entries.len();
                        }
                        return true;
                    }
                    KeyCode::Enter => {
                        if let Some(entry) = self.entries.get_mut(self.active) {
                            if entry.session.is_none() {
                                let (cols, rows) = self.last_term_size;
                                entry.session =
                                    Session::new("/bin/bash", &[], cols, rows).ok();
                                entry.status = TaskStatus::Running;
                            }
                        }
                        return true;
                    }
                    _ => {}
                }
            }
        }

        // Forward to active terminal.
        if let Some(entry) = self.entries.get(self.active) {
            if let Some(ref session) = entry.session {
                if !session.exited {
                    if let Some(bytes) = input::event_to_bytes(event) {
                        session.write(bytes);
                    }
                    return true;
                }
            }
        }

        false
    }

    /// Handle terminal resize.
    pub fn resize_terminals(&mut self, cols: u16, rows: u16) {
        self.last_term_size = (cols, rows);
        for entry in &mut self.entries {
            if let Some(ref session) = entry.session {
                session.resize(cols, rows);
            }
        }
    }

    /// Draw the UI.
    pub fn draw(&self, frame: &mut Frame) {
        let area = frame.area();

        let rows = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);

        let content_area = rows[0];
        let bar_area = rows[1];

        let cols = Layout::horizontal([
            Constraint::Min(40),
            Constraint::Length(30),
        ])
        .split(content_area);

        self.draw_terminal(frame, cols[0]);
        self.draw_session_list(frame, cols[1]);
        self.draw_status_bar(frame, bar_area);
    }

    fn draw_terminal(&self, frame: &mut Frame, area: Rect) {
        let has_session = self.entries.get(self.active).and_then(|e| e.session.as_ref()).is_some();

        let title_style = if has_session {
            Style::default().fg(Color::White)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(self.active_title(), title_style));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if let Some(entry) = self.entries.get(self.active) {
            if let Some(ref session) = entry.session {
                let widget = TerminalWidget::new(&session.term, true);
                frame.render_widget(widget, inner);
            } else {
                let msg = Paragraph::new("Press Alt+Enter to attach")
                    .style(Style::default().fg(Color::DarkGray));
                frame.render_widget(msg, inner);
            }
        } else {
            let msg = Paragraph::new("No sessions")
                .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, inner);
        }
    }

    fn draw_session_list(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(" Sessions ", Style::default().fg(Color::White)));

        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.height < 2 || inner.width < 4 {
            return;
        }

        let spinner = self.spinner_frame();

        let list_height = inner.height.saturating_sub(3);
        let items: Vec<ListItem> = self
            .entries
            .iter()
            .enumerate()
            .take(list_height as usize)
            .map(|(i, entry)| {
                let is_active = i == self.active;

                let (indicator, indicator_style) = match entry.status {
                    TaskStatus::Running => (spinner, Style::default().fg(Color::Green)),
                    TaskStatus::Blocked => ("●", Style::default().fg(Color::Yellow)),
                    TaskStatus::Backlog => ("○", Style::default().fg(Color::DarkGray)),
                };

                let max_name = (inner.width as usize).saturating_sub(4);
                let name = if entry.name.len() > max_name {
                    format!("{}...", &entry.name[..max_name.saturating_sub(3)])
                } else {
                    entry.name.clone()
                };

                let line = Line::from(vec![
                    Span::styled(format!(" {} ", indicator), indicator_style),
                    Span::raw(name),
                ]);

                let base_style = if is_active {
                    Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };

                ListItem::new(line).style(base_style)
            })
            .collect();

        let list = List::new(items);
        frame.render_widget(list, Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: list_height,
        });

        // Help text at bottom.
        let help_y = inner.y + inner.height.saturating_sub(2);
        let help_area = Rect {
            x: inner.x,
            y: help_y,
            width: inner.width,
            height: 2,
        };
        let help = Paragraph::new(vec![
            Line::from(Span::styled(
                "Alt-j/k nav  Alt-q quit",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(help, help_area);
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        let running = self.entries.iter().filter(|e| matches!(e.status, TaskStatus::Running)).count();
        let blocked = self.entries.iter().filter(|e| matches!(e.status, TaskStatus::Blocked)).count();
        let backlog = self.entries.iter().filter(|e| matches!(e.status, TaskStatus::Backlog)).count();

        let left = " claude-manager ";
        let right = format!(" {}r {}b {}q ", running, blocked, backlog);

        let left_width = left.len() as u16;
        let right_width = right.len() as u16;
        let mid_width = area.width.saturating_sub(left_width + right_width);

        let line = Line::from(vec![
            Span::styled(left, Style::default().fg(Color::DarkGray)),
            Span::styled(" ".repeat(mid_width as usize), Style::default()),
            Span::styled(right, Style::default().fg(Color::DarkGray)),
        ]);

        frame.render_widget(Paragraph::new(line), area);
    }

    fn active_title(&self) -> String {
        if let Some(entry) = self.entries.get(self.active) {
            format!(" {} ", entry.name)
        } else {
            " Terminal ".to_string()
        }
    }
}
