use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use alacritty_terminal::Term;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use std::sync::Arc;

use crate::session::EventProxy;

/// Widget that renders an alacritty terminal grid into a ratatui buffer.
pub struct TerminalWidget<'a> {
    term: &'a Arc<FairMutex<Term<EventProxy>>>,
    focused: bool,
}

impl<'a> TerminalWidget<'a> {
    pub fn new(term: &'a Arc<FairMutex<Term<EventProxy>>>, focused: bool) -> Self {
        Self { term, focused }
    }
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cursor = content.cursor;

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = &indexed.cell;

            let x = area.left() + point.column.0 as u16;
            let y = area.top() + point.line.0 as u16;

            if x >= area.right() || y >= area.bottom() {
                continue;
            }

            // Skip wide char spacers — the wide char itself covers both columns.
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            let fg = convert_color(cell.fg);
            let bg = convert_color(cell.bg);
            let modifier = convert_flags(cell.flags);

            let (fg, bg) = if cell.flags.contains(Flags::INVERSE) {
                (bg, fg)
            } else {
                (fg, bg)
            };

            if let Some(ratatui_cell) = buf.cell_mut((x, y)) {
                ratatui_cell.set_char(cell.c);
                ratatui_cell.set_fg(fg);
                ratatui_cell.set_bg(bg);
                ratatui_cell.set_style(Style::default().add_modifier(modifier));
            }
        }

        // Render cursor. Always show when focused — inner apps (like Claude Code)
        // may hide the hardware cursor but we have no real hardware cursor to show,
        // so we always draw one at the reported position.
        if self.focused {
            let cx = area.left() + cursor.point.column.0 as u16;
            let cy = area.top() + cursor.point.line.0 as u16;
            if cx < area.right() && cy < area.bottom() {
                if let Some(cell) = buf.cell_mut((cx, cy)) {
                    // Resolve Reset to concrete colors so the cursor is always visible.
                    let fg = match cell.fg {
                        Color::Reset => Color::White,
                        c => c,
                    };
                    let bg = match cell.bg {
                        Color::Reset => Color::Black,
                        c => c,
                    };
                    // Reverse video for block cursor.
                    cell.set_fg(bg);
                    cell.set_bg(fg);
                }
            }
        }
    }
}

fn convert_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Named(name) => match name {
            NamedColor::Black => Color::Black,
            NamedColor::Red => Color::Red,
            NamedColor::Green => Color::Green,
            NamedColor::Yellow => Color::Yellow,
            NamedColor::Blue => Color::Blue,
            NamedColor::Magenta => Color::Magenta,
            NamedColor::Cyan => Color::Cyan,
            NamedColor::White => Color::White,
            NamedColor::BrightBlack => Color::DarkGray,
            NamedColor::BrightRed => Color::LightRed,
            NamedColor::BrightGreen => Color::LightGreen,
            NamedColor::BrightYellow => Color::LightYellow,
            NamedColor::BrightBlue => Color::LightBlue,
            NamedColor::BrightMagenta => Color::LightMagenta,
            NamedColor::BrightCyan => Color::LightCyan,
            NamedColor::BrightWhite => Color::White,
            NamedColor::Foreground | NamedColor::BrightForeground => Color::Reset,
            NamedColor::Background => Color::Reset,
            // Dim colors — map to their base color.
            NamedColor::DimBlack => Color::DarkGray,
            NamedColor::DimRed => Color::Red,
            NamedColor::DimGreen => Color::Green,
            NamedColor::DimYellow => Color::Yellow,
            NamedColor::DimBlue => Color::Blue,
            NamedColor::DimMagenta => Color::Magenta,
            NamedColor::DimCyan => Color::Cyan,
            NamedColor::DimWhite => Color::Gray,
            NamedColor::Cursor => Color::Reset,
            _ => Color::Reset,
        },
        AnsiColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        AnsiColor::Indexed(idx) => Color::Indexed(idx),
    }
}

fn convert_flags(flags: Flags) -> Modifier {
    let mut modifier = Modifier::empty();
    if flags.contains(Flags::BOLD) {
        modifier |= Modifier::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        modifier |= Modifier::ITALIC;
    }
    if flags.contains(Flags::UNDERLINE) {
        modifier |= Modifier::UNDERLINED;
    }
    if flags.contains(Flags::DIM) {
        modifier |= Modifier::DIM;
    }
    if flags.contains(Flags::HIDDEN) {
        modifier |= Modifier::HIDDEN;
    }
    if flags.contains(Flags::STRIKEOUT) {
        modifier |= Modifier::CROSSED_OUT;
    }
    modifier
}
