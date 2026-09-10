use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::{Flags, LineLength};
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

/// Current scrollback offset of `term`, in lines scrolled up from the live
/// tail. `0` = the viewport is pinned to the tail (what `render` shows is
/// live output). Same value `render` reads per-frame from
/// `renderable_content().display_offset`; exposed so chrome (the terminal
/// pane's "▲ scrollback" cue) can check it before the widget is built.
/// Briefly locks the term.
pub fn scrollback_offset(term: &Arc<FairMutex<Term<EventProxy>>>) -> usize {
    term.lock().grid().display_offset()
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let term = self.term.lock();
        let content = term.renderable_content();
        let cursor = content.cursor;
        let display_offset = content.display_offset as i32;
        let selection = content.selection;

        for indexed in content.display_iter {
            let point = indexed.point;
            let cell = &indexed.cell;

            let x = area.left() + point.column.0 as u16;
            // Convert absolute grid line to viewport-relative row.
            let viewport_line = point.line.0 + display_offset;
            if viewport_line < 0 {
                continue;
            }
            let y = area.top() + viewport_line as u16;

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

            let (mut fg, mut bg) = if cell.flags.contains(Flags::INVERSE) {
                (bg, fg)
            } else {
                (fg, bg)
            };

            // Invert cells that fall inside an active selection range.
            // For non-block selections, skip cells past the end of actual content so
            // the highlight stops at the text instead of running to the right edge.
            if let Some(range) = selection {
                let in_range = range.contains(point);
                let past_content = !range.is_block
                    && point.column >= term.grid()[point.line].line_length();
                if in_range && !past_content {
                    std::mem::swap(&mut fg, &mut bg);
                    if matches!(fg, Color::Reset) {
                        fg = Color::Black;
                    }
                    if matches!(bg, Color::Reset) {
                        bg = Color::White;
                    }
                }
            }

            if let Some(ratatui_cell) = buf.cell_mut((x, y)) {
                // Alacritty stores tabs in the grid for text extraction, but
                // their cursor movement has already been applied by the parser.
                // Emitting one here moves the OUTER terminal cursor again and
                // lets subsequent diff cells overwrite the neighboring sidebar.
                // Grid control characters are blank display cells, not commands.
                ratatui_cell.set_char(if cell.c.is_control() { ' ' } else { cell.c });
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
            let cursor_viewport_line = cursor.point.line.0 + display_offset;
            let cy = if cursor_viewport_line >= 0 {
                area.top() + cursor_viewport_line as u16
            } else {
                // Cursor is above the viewport when scrolled — skip rendering.
                area.bottom()
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::vte::ansi::Processor;
    use ratatui::backend::{Backend, CrosstermBackend};

    use crate::session::{terminal_config, TermSize};

    fn terminal(columns: usize, screen_lines: usize) -> Arc<FairMutex<Term<EventProxy>>> {
        let (tx, _) = std::sync::mpsc::channel();
        Arc::new(FairMutex::new(Term::new(
            terminal_config(),
            &TermSize {
                columns,
                screen_lines,
            },
            EventProxy::new(tx),
        )))
    }

    fn feed(term: &Arc<FairMutex<Term<EventProxy>>>, bytes: &[u8]) {
        let mut parser: Processor = Processor::new();
        parser.advance(&mut *term.lock(), bytes);
    }

    fn frame(term: &Arc<FairMutex<Term<EventProxy>>>, pane: Rect, screen: Rect) -> Buffer {
        let mut buf = Buffer::empty(screen);
        for y in 0..screen.height {
            for x in pane.right()..screen.width {
                buf[(x, y)]
                    .set_char(if x == pane.right() { '│' } else { '#' })
                    .set_fg(Color::White)
                    .set_bg(Color::Indexed(53));
            }
        }
        TerminalWidget::new(term, false).render(pane, &mut buf);
        buf
    }

    // Buffer-only assertions miss cursor-moving bytes. Exercise the production
    // diff/backend and interpret its ANSI output in a second terminal, just as
    // the laptop does. The sidebar is unchanged between frames, so it cannot
    // repair a pane update that accidentally writes beyond its boundary.
    fn draw_into_terminal(
        previous: &Buffer,
        next: &Buffer,
        outer: &Arc<FairMutex<Term<EventProxy>>>,
    ) {
        let mut bytes = Vec::new();
        let mut backend = CrosstermBackend::new(&mut bytes);
        backend.draw(previous.diff(next).into_iter()).unwrap();
        backend.flush().unwrap();
        feed(outer, &bytes);
    }

    #[test]
    fn tabbed_output_does_not_overwrite_sidebar_on_incremental_redraw() {
        // The test runner may set NO_COLOR; exercise the laptop's colored output.
        crossterm::style::force_color_output(true);
        for left in [0, 1, 3, 7] {
            let screen = Rect::new(0, 0, 48, 5);
            let pane = Rect::new(left, 1, 23, 3);
            let inner = terminal(pane.width as usize, pane.height as usize);
            let outer = terminal(screen.width as usize, screen.height as usize);
            feed(&inner, b"xxxxxxxxxxxxxxxxxxxxxxx");
            let mut previous = frame(&inner, pane, screen);
            draw_into_terminal(&Buffer::empty(screen), &previous, &outer);

            for input in [
                b"\r\x1b[2K\tstatus".as_slice(),
                b"\r\x1b[2Kone\tmore\toutput",
                b"\r\x1b[2Kxxxxxxxxxxxxxxxxxxxxxxx",
                b"\r\x1b[2K\t\tend",
            ] {
                feed(&inner, input);
                let next = frame(&inner, pane, screen);
                draw_into_terminal(&previous, &next, &outer);
                let term = outer.lock();
                for y in 0..screen.height {
                    for x in pane.right()..screen.width {
                        let actual = &term.grid()[Line(y as i32)][Column(x as usize)];
                        assert_eq!(
                            actual.c.to_string(),
                            next[(x, y)].symbol(),
                            "sidebar overwritten at ({x}, {y}), pane left={left}"
                        );
                        assert_eq!(
                            actual.bg,
                            AnsiColor::Indexed(53),
                            "sidebar background overwritten at ({x}, {y}), pane left={left}"
                        );
                    }
                }
                for x in pane.left()..pane.right() {
                    assert_eq!(
                        term.grid()[Line(pane.y as i32)][Column(x as usize)]
                            .c
                            .to_string(),
                        next[(x, pane.y)].symbol(),
                        "pane content misplaced at column {x}, pane left={left}"
                    );
                }
                drop(term);
                previous = next;
            }
        }
    }

    #[test]
    fn tabs_keep_their_style_and_grid_text_in_scrollback() {
        let inner = terminal(12, 3);
        feed(&inner, b"\x1b[44m\x1b[2J\ta\r\n\tb\r\n\tc\r\n\td");
        inner
            .lock()
            .scroll_display(alacritty_terminal::grid::Scroll::Delta(1));
        assert_eq!(scrollback_offset(&inner), 1);
        let pane = Rect::new(1, 1, 12, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
        TerminalWidget::new(&inner, false).render(pane, &mut buf);

        for (row, letter) in ['a', 'b', 'c'].into_iter().enumerate() {
            let y = pane.y + row as u16;
            assert_eq!(buf[(pane.x, y)].symbol(), " ");
            assert_eq!(buf[(pane.x, y)].bg, Color::Blue);
            assert_eq!(buf[(pane.x + 8, y)].symbol(), letter.to_string());
            // Copy/selection still sees the original tab; only display changes.
            assert_eq!(inner.lock().grid()[Line(row as i32 - 1)][Column(0)].c, '\t');
        }
    }
}
