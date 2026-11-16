//! Screen-first attach: the visible screen of a session as one repaint.
//!
//! A full attach replays the session's whole output ring (1 MiB, 1.4 MB as
//! base64 frames) so the viewer's terminal can rebuild screen and scrollback.
//! On a slow link that made every attach cost seconds (2026-10-13). With
//! `attach.direct {replay: "screen"}` the daemon parses the ring into a
//! throwaway terminal at the viewer's size and sends only what that terminal
//! shows: a few KB that leave a fresh viewer terminal with the same screen,
//! cursor and input modes. Scrollback is fetched separately when the viewer
//! scrolls into it.
//!
//! Uses the parser the TUI renders with (`alacritty_terminal`), so the
//! repaint describes the screen the full replay would have produced. Terminal
//! state that alacritty keeps private (saved cursor, charsets) is not carried;
//! the TUI's first-view resize makes the program redraw, which restores it.
//! The scroll region is private too and is recovered from the ring itself.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config as TermConfig, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
use alacritty_terminal::Term;
use std::fmt::Write as _;

struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Parse `ring` at `cols`x`rows` and return the escape sequences that paint
/// the resulting screen onto a fresh terminal of that size.
pub fn render(ring: &[u8], cols: u16, rows: u16) -> Vec<u8> {
    let (cols, rows) = (usize::from(cols.max(1)), usize::from(rows.max(1)));
    let mut config = TermConfig::default();
    // The TUI enables kitty keyboard tracking; without it alacritty ignores
    // `CSI > N u` and the repaint could not carry the program's key mode.
    config.kitty_keyboard = true;
    config.scrolling_history = 0;
    let mut term = Term::new(config, &Dims { cols, rows }, VoidListener);
    Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut term, ring);
    paint(&term, scroll_region(ring, rows))
}

fn paint(term: &Term<VoidListener>, region: Option<(usize, usize)>) -> Vec<u8> {
    let grid = term.grid();
    let mode = *term.mode();
    let (rows, cols) = (grid.screen_lines(), grid.columns());
    let mut out = String::with_capacity(rows * cols + 256);
    if mode.contains(TermMode::ALT_SCREEN) {
        out.push_str("\x1b[?1049h");
    }
    // Clear below from home rather than ED 2: alacritty's ED 2 pushes the
    // screen into scrollback, which would leave a blank history line.
    out.push_str("\x1b[0m\x1b[H\x1b[J");

    let mut pen = Pen::default();
    for line in 0..rows {
        let row = &grid[Line(line as i32)];
        // Trailing blank default cells are already blank after the clear.
        let end = (0..cols)
            .rev()
            .find(|&c| !is_blank(&row[Column(c)]))
            .map_or(0, |c| c + 1);
        if end == 0 {
            continue;
        }
        let _ = write!(out, "\x1b[{};1H", line + 1);
        for col in 0..end {
            let cell = &row[Column(col)];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            pen.update(&mut out, cell);
            out.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                out.extend(extra.iter());
            }
        }
    }

    // The program's current pen, so output that relies on it continues in
    // the same style.
    pen.update(&mut out, &grid.cursor.template);

    let set = |out: &mut String, flag: TermMode, on: &str| {
        if mode.contains(flag) {
            out.push_str(on);
        }
    };
    set(&mut out, TermMode::APP_CURSOR, "\x1b[?1h");
    set(&mut out, TermMode::APP_KEYPAD, "\x1b=");
    set(&mut out, TermMode::BRACKETED_PASTE, "\x1b[?2004h");
    set(&mut out, TermMode::FOCUS_IN_OUT, "\x1b[?1004h");
    set(&mut out, TermMode::MOUSE_REPORT_CLICK, "\x1b[?1000h");
    set(&mut out, TermMode::MOUSE_DRAG, "\x1b[?1002h");
    set(&mut out, TermMode::MOUSE_MOTION, "\x1b[?1003h");
    set(&mut out, TermMode::SGR_MOUSE, "\x1b[?1006h");
    set(&mut out, TermMode::UTF8_MOUSE, "\x1b[?1005h");
    set(&mut out, TermMode::INSERT, "\x1b[4h");
    set(&mut out, TermMode::LINE_FEED_NEW_LINE, "\x1b[20h");
    if !mode.contains(TermMode::LINE_WRAP) {
        out.push_str("\x1b[?7l");
    }
    if !mode.contains(TermMode::ALTERNATE_SCROLL) {
        out.push_str("\x1b[?1007l");
    }
    let kitty = [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .iter()
    .filter(|(flag, _)| mode.contains(*flag))
    .fold(0u8, |bits, (_, bit)| bits | bit);
    if kitty != 0 {
        let _ = write!(out, "\x1b[>{kitty}u");
    }
    let style = term.cursor_style();
    let shape = match style.shape {
        CursorShape::Block | CursorShape::HollowBlock => 2,
        CursorShape::Underline => 4,
        CursorShape::Beam => 6,
        CursorShape::Hidden => 2,
    };
    if style.shape != CursorShape::Block || style.blinking {
        let _ = write!(out, "\x1b[{} q", shape - usize::from(style.blinking));
    }
    // DECSTBM homes the cursor, so it goes before the final position.
    if let Some((top, bottom)) = region {
        let _ = write!(out, "\x1b[{};{}r", top, bottom);
    }

    let cursor = &grid.cursor;
    let (line, col) = (cursor.point.line.0.max(0) as usize, cursor.point.column.0);
    if cursor.input_needs_wrap && col + 1 == cols {
        // Rewrite the last cell so the next character wraps, as it would have.
        let cell = &grid[cursor.point.line][cursor.point.column];
        let _ = write!(out, "\x1b[{};{}H", line + 1, col + 1);
        pen.update(&mut out, cell);
        out.push(if cell.flags.contains(Flags::WIDE_CHAR_SPACER) { ' ' } else { cell.c });
        pen.update(&mut out, &grid.cursor.template);
    } else {
        let _ = write!(out, "\x1b[{};{}H", line + 1, col + 1);
    }
    if !mode.contains(TermMode::SHOW_CURSOR) {
        out.push_str("\x1b[?25l");
    }
    out.into_bytes()
}

fn is_blank(cell: &Cell) -> bool {
    cell.c == ' '
        && cell.bg == Color::Named(NamedColor::Background)
        && !cell.flags.intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT)
        && cell.zerowidth().is_none()
}

/// SGR state written so far; emits a full reset-and-set when a cell's style
/// differs from the previous one.
#[derive(Clone, Default, PartialEq)]
struct Pen {
    key: Option<(Color, Color, Option<Color>, u16)>,
}

const STYLE_FLAGS: Flags = Flags::BOLD
    .union(Flags::DIM)
    .union(Flags::ITALIC)
    .union(Flags::ALL_UNDERLINES)
    .union(Flags::INVERSE)
    .union(Flags::HIDDEN)
    .union(Flags::STRIKEOUT);

impl Pen {
    fn update(&mut self, out: &mut String, cell: &Cell) {
        let flags = cell.flags & STYLE_FLAGS;
        let key = (cell.fg, cell.bg, cell.underline_color(), flags.bits());
        if self.key == Some(key) {
            return;
        }
        self.key = Some(key);
        out.push_str("\x1b[0");
        for (flag, code) in [
            (Flags::BOLD, "1"),
            (Flags::DIM, "2"),
            (Flags::ITALIC, "3"),
            (Flags::UNDERLINE, "4"),
            (Flags::DOUBLE_UNDERLINE, "4:2"),
            (Flags::UNDERCURL, "4:3"),
            (Flags::DOTTED_UNDERLINE, "4:4"),
            (Flags::DASHED_UNDERLINE, "4:5"),
            (Flags::INVERSE, "7"),
            (Flags::HIDDEN, "8"),
            (Flags::STRIKEOUT, "9"),
        ] {
            if flags.contains(flag) {
                out.push(';');
                out.push_str(code);
            }
        }
        color(out, cell.fg, 30, 90, 38, NamedColor::Foreground);
        color(out, cell.bg, 40, 100, 48, NamedColor::Background);
        if let Some(underline) = cell.underline_color() {
            match underline {
                Color::Indexed(n) => {
                    let _ = write!(out, ";58:5:{n}");
                }
                Color::Spec(rgb) => {
                    let _ = write!(out, ";58:2::{}:{}:{}", rgb.r, rgb.g, rgb.b);
                }
                Color::Named(n) if (n as usize) < 16 => {
                    let _ = write!(out, ";58:5:{}", n as usize);
                }
                Color::Named(_) => {}
            }
        }
        out.push('m');
    }
}

fn color(out: &mut String, c: Color, base: usize, bright: usize, ext: usize, default: NamedColor) {
    match c {
        Color::Named(n) if n == default => {}
        Color::Named(n) if (n as usize) < 8 => {
            let _ = write!(out, ";{}", base + n as usize);
        }
        Color::Named(n) if (n as usize) < 16 => {
            let _ = write!(out, ";{}", bright + n as usize - 8);
        }
        // Dim variants come from rendering, not from the program; the cell
        // keeps the program's own color, so anything else is a default.
        Color::Named(_) => {}
        Color::Indexed(n) => {
            let _ = write!(out, ";{ext};5;{n}");
        }
        Color::Spec(rgb) => {
            let _ = write!(out, ";{ext};2;{};{};{}", rgb.r, rgb.g, rgb.b);
        }
    }
}

/// The scroll region (1-based, inclusive) the ring leaves in force, when it
/// is not the whole screen. alacritty keeps it private, so read the last
/// DECSTBM (`CSI top ; bottom r`) that no full reset (`ESC c`) follows.
fn scroll_region(ring: &[u8], rows: usize) -> Option<(usize, usize)> {
    let mut found = None;
    let mut i = 0;
    while i + 1 < ring.len() {
        if ring[i] != 0x1b {
            i += 1;
            continue;
        }
        if ring[i + 1] == b'c' {
            found = None;
            i += 2;
            continue;
        }
        if ring[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let start = i + 2;
        let mut j = start;
        while j < ring.len() && (ring[j].is_ascii_digit() || ring[j] == b';') {
            j += 1;
        }
        if j < ring.len() && ring[j] == b'r' {
            let params = std::str::from_utf8(&ring[start..j]).unwrap_or("");
            let mut parts = params.split(';');
            let top = parts.next().and_then(|p| p.parse::<usize>().ok()).filter(|&n| n > 0).unwrap_or(1);
            let bottom =
                parts.next().and_then(|p| p.parse::<usize>().ok()).filter(|&n| n > 0).unwrap_or(rows);
            found = Some((top, bottom));
        }
        i = j.max(i + 2);
    }
    found
        .map(|(top, bottom)| (top.min(rows), bottom.min(rows)))
        .filter(|&(top, bottom)| top < bottom && (top, bottom) != (1, rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term_after(bytes: &[u8], cols: usize, rows: usize) -> Term<VoidListener> {
        let mut config = TermConfig::default();
        config.kitty_keyboard = true;
        config.scrolling_history = 1000;
        let mut term = Term::new(config, &Dims { cols, rows }, VoidListener);
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut term, bytes);
        term
    }

    /// Everything a viewer shows or acts on: every cell (character, colors,
    /// style), the cursor, and the input modes.
    fn state(term: &Term<VoidListener>) -> (Vec<String>, (i32, usize), u32) {
        let grid = term.grid();
        let lines = (0..grid.screen_lines())
            .map(|l| {
                let row = &grid[Line(l as i32)];
                (0..grid.columns())
                    .map(|c| {
                        let cell = &row[Column(c)];
                        format!(
                            "{:?}{:?}{:?}{:?}{:?}|",
                            cell.c,
                            cell.fg,
                            cell.bg,
                            cell.flags & (STYLE_FLAGS | Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER),
                            cell.zerowidth()
                        )
                    })
                    .collect()
            })
            .collect();
        let mode = *term.mode() & !(TermMode::URGENCY_HINTS | TermMode::VI);
        (lines, (grid.cursor.point.line.0, grid.cursor.point.column.0), mode.bits())
    }

    /// The repaint reproduces the full replay's screen, and the program's
    /// next output lands the same way on both.
    fn assert_faithful(ring: &[u8], cols: usize, rows: usize, then: &[u8]) {
        let repaint = render(ring, cols as u16, rows as u16);
        assert_eq!(
            term_after(&repaint, cols, rows).grid().history_size(),
            0,
            "a repaint never scrolls: {:?}",
            String::from_utf8_lossy(&repaint),
        );
        let full = term_after(&[ring, then].concat(), cols, rows);
        let fast = term_after(&[&repaint[..], then].concat(), cols, rows);
        assert_eq!(state(&fast), state(&full), "repaint: {:?}", String::from_utf8_lossy(&repaint));
    }

    #[test]
    fn plain_scrolling_output() {
        let ring: Vec<u8> = (0..500).flat_map(|n| format!("line {n} of output\r\n").into_bytes()).collect();
        assert_faithful(&ring, 80, 24, b"next prompt $ ");
    }

    #[test]
    fn colors_styles_wide_and_combining_characters() {
        let ring = "\x1b[1;31mbold red\x1b[0m \x1b[38;5;208mindexed\x1b[0m \
            \x1b[48;2;10;20;30;3mrgb italic bg\x1b[0m \x1b[4:3;58:5:9mcurly\x1b[0m\r\n\
            \x1b[7minverse\x1b[27m \x1b[9mstrike\x1b[0m \x1b[2mdim\x1b[0m 漢字 e\u{301}\r\n\
            \x1b[44m   blue blanks   \x1b[0m\r\n\x1b[32mgreen pen continues";
        assert_faithful(ring.as_bytes(), 40, 8, b" more green\r\nnext");
    }

    #[test]
    fn alt_screen_cursor_and_input_modes() {
        let ring = "shell history\r\n\x1b[?1049h\x1b[?1h\x1b=\x1b[?2004h\x1b[?1004h\
            \x1b[?1000h\x1b[?1006h\x1b[>1u\x1b[?25l\x1b[2J\x1b[5;10Hfull screen app\x1b[6 q";
        assert_faithful(ring.as_bytes(), 60, 12, b"\x1b[1;1Hredraw");
        let term = term_after(&render(ring.as_bytes(), 60, 12), 60, 12);
        assert!(term.mode().contains(TermMode::ALT_SCREEN | TermMode::BRACKETED_PASTE));
    }

    #[test]
    fn scroll_region_survives_for_the_next_output() {
        // An inline viewport: lines 1-20 scroll, 21-24 stay put.
        let mut ring = b"\x1b[1;20r".to_vec();
        for n in 0..60 {
            ring.extend(format!("\x1b[20;1Hhistory {n}\r\n").into_bytes());
        }
        ring.extend(b"\x1b[22;1Hstatus bar\x1b[20;1H");
        assert_faithful(&ring, 50, 24, b"\r\nnew line one\r\nnew line two\r\n");
    }

    #[test]
    fn a_full_screen_ending_in_a_pending_wrap() {
        assert_faithful("x".repeat(80 * 30).as_bytes(), 80, 24, b"next");
    }

    #[test]
    fn pending_wrap_at_the_last_column() {
        let ring = format!("{}", "x".repeat(30));
        assert_faithful(ring.as_bytes(), 30, 5, b"wrapped");
    }

    #[test]
    fn repaint_is_a_screenful_not_the_ring() {
        let ring: Vec<u8> = (0..20_000)
            .flat_map(|n| format!("\x1b[3{}mrow {n} with some text\x1b[0m\r\n", n % 8).into_bytes())
            .collect();
        assert!(ring.len() > 500_000);
        let repaint = render(&ring, 200, 50);
        assert!(repaint.len() < 20_000, "repaint was {} bytes", repaint.len());
        assert_faithful(&ring, 200, 50, b"");
    }
}
