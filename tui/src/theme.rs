//! Semantic color roles for the TUI.
//!
//! Every UI color the TUI draws with lives here under a role name; render
//! code refers to `theme::ROLE` instead of inline `Color::` literals so a
//! future palette retune is a one-file change. This module is the single
//! intentional home of raw `Color::` values in the TUI (the one exception:
//! `terminal_widget.rs`, whose ANSI→ratatui mapping is PTY data, not theme).
//!
//! This started as a pure mechanical extraction — each role is defined as
//! the exact value its call sites already used, so introducing the module
//! changed nothing visually.

use ratatui::style::Color;

/// De-emphasized text and chrome: hints, help lines, separators, inactive
/// pane borders, tree/fold glyphs, exited-session markers.
pub(crate) const DIM: Color = Color::DarkGray;

/// Primary foreground: main text, selected rows, focused dialog borders.
pub(crate) const TEXT: Color = Color::White;

/// Secondary text: readable-but-unselected rows, values, message bodies.
pub(crate) const MUTED: Color = Color::Gray;

/// Headers and accents: task/workflow headers, project names, activity-feed
/// callers, orchestrator question glyphs, detail links.
pub(crate) const HEADER: Color = Color::Cyan;

/// Attention: warnings, the reconnecting `⟳` indicator, confirm-dialog
/// borders, active workflow role, in-progress planning status, status-bar
/// flash messages.
pub(crate) const ATTN: Color = Color::Yellow;
/// Amber for work blocked on an Owner decision (◇/◆ + `OWNER`): warmer than
/// ATTN's yellow so it reads as "needs you", without the alert's red.
pub(crate) const OWNER_BLOCKED: Color = Color::Rgb(255, 176, 0);

/// Positive/running: session running spinner, connected dot, done planning
/// status, workflow-done tick.
pub(crate) const OK: Color = Color::Green;

/// Remote/agent accents: `@host` tags and claude-proposed planning rows.
pub(crate) const REMOTE: Color = Color::Magenta;

/// Errors: error text, conflicting dependencies, error-banner backgrounds.
pub(crate) const ERROR: Color = Color::Red;

/// Foreground drawn on top of an [`ATTN`]-colored badge (the black-on-yellow
/// `[mouse off]` status-bar chip).
pub(crate) const BADGE_FG: Color = Color::Black;

/// Idle "afterglow": a session that went idle within the last couple of
/// minutes — it most likely just finished a turn and is waiting on the
/// operator. A warm light yellow-green picked to pop against both the
/// [`OK`]-green running spinner and the plain [`TEXT`]-white idle dot.
pub(crate) const AFTERGLOW: Color = Color::Rgb(205, 255, 120);

/// Planning board: visual-mode selection background on task/header rows.
pub(crate) const SELECT_BG: Color = Color::Rgb(50, 50, 80);

/// Planning board: selected header-row background (outside visual mode).
pub(crate) const HEADER_SELECT_BG: Color = Color::Rgb(40, 40, 50);

/// Planning board: dependency-conflict row background (unselected rows;
/// selected conflict rows use a full [`ERROR`] background instead).
pub(crate) const CONFLICT_BG: Color = Color::Rgb(80, 0, 0);

/// Bright closing rule for a task-sidebar section, drawn with a heavy glyph.
pub(crate) const SECTION_BOUNDARY: Color = Color::Rgb(215, 220, 230);

/// Work tree (Task sub-view): the focused group's left-edge strip and the
/// leader marks.
pub(crate) const TREE_FOCUS: Color = Color::Rgb(92, 210, 232);
pub(crate) const LEAD: Color = Color::Rgb(255, 205, 90);

/// Messages: navy surfaces, slate chrome, and a cyan focus outline.
/// Content accents stay the same when focus moves between panes.
pub(crate) const CHAT_BG: Color = Color::Rgb(16, 21, 32);
pub(crate) const CHAT_PANEL: Color = Color::Rgb(23, 30, 43);
pub(crate) const CHAT_BORDER: Color = Color::Rgb(62, 79, 101);
pub(crate) const CHAT_FOCUS: Color = Color::Rgb(92, 210, 232);
pub(crate) const CHAT_TEXT: Color = Color::Rgb(221, 229, 242);
pub(crate) const CHAT_MUTED: Color = Color::Rgb(140, 157, 181);
pub(crate) const CHAT_OWNER: Color = Color::Rgb(134, 216, 174);
pub(crate) const CHAT_AGENT: Color = Color::Rgb(187, 166, 246);
pub(crate) const CHAT_TAG: Color = Color::Rgb(233, 190, 120);
pub(crate) const CHAT_SELECTION: Color = Color::Rgb(37, 51, 73);

/// Rainbow the `notify_user` attention alert color cycles through, one step
/// per frame. 7 colors — coprime with the 6-frame pulse, so the glyph size
/// and color never re-lock into a short loop (full repeat period is
/// lcm(6,7)=42 frames ≈ 5s), keeping the bead visually lively the whole
/// time it's pending.
pub(crate) const ALERT_RAINBOW: &[Color] = &[
    Color::Rgb(255, 70, 70),   // red
    Color::Rgb(255, 140, 0),   // orange
    Color::Rgb(255, 225, 50),  // yellow
    Color::Rgb(90, 230, 90),   // green
    Color::Rgb(60, 220, 220),  // cyan
    Color::Rgb(90, 150, 255),  // blue
    Color::Rgb(225, 90, 230),  // magenta
];

/// Named accent colors a user can assign to sessions, workspaces, tasks
/// and sections via the A-e settings forms. Persisted by NAME (first tuple
/// field) so the palette can be retuned later without migrating saved
/// manifests. The eight base names (`red` … `pink`) are the original
/// palette with their original values, so stored choices look the same;
/// each hue gained a `light-` and a `dark-` shade, plus neutrals
/// (2026-10-20). Layout in the picker: [`COLOR_GRID`].
pub(crate) const USER_COLORS: &[(&str, Color)] = &[
    ("red", Color::Rgb(255, 105, 105)),
    ("orange", Color::Rgb(255, 160, 70)),
    ("yellow", Color::Rgb(240, 210, 70)),
    ("green", Color::Rgb(110, 220, 110)),
    ("cyan", Color::Rgb(80, 210, 210)),
    ("blue", Color::Rgb(110, 160, 255)),
    ("magenta", Color::Rgb(215, 110, 235)),
    ("pink", Color::Rgb(255, 130, 180)),
    ("light-red", Color::Rgb(255, 170, 170)),
    ("light-orange", Color::Rgb(255, 200, 145)),
    ("light-yellow", Color::Rgb(250, 235, 150)),
    ("light-green", Color::Rgb(175, 240, 175)),
    ("light-cyan", Color::Rgb(160, 235, 235)),
    ("light-blue", Color::Rgb(170, 200, 255)),
    ("light-magenta", Color::Rgb(235, 175, 245)),
    ("light-pink", Color::Rgb(255, 185, 215)),
    ("dark-red", Color::Rgb(190, 55, 55)),
    ("dark-orange", Color::Rgb(200, 110, 30)),
    ("dark-yellow", Color::Rgb(180, 150, 30)),
    ("dark-green", Color::Rgb(55, 160, 65)),
    ("dark-cyan", Color::Rgb(30, 150, 155)),
    ("dark-blue", Color::Rgb(60, 100, 210)),
    ("dark-magenta", Color::Rgb(155, 60, 180)),
    ("dark-pink", Color::Rgb(200, 75, 130)),
    ("white", Color::Rgb(235, 235, 235)),
    ("silver", Color::Rgb(185, 190, 200)),
    ("grey", Color::Rgb(135, 140, 150)),
    ("slate", Color::Rgb(100, 115, 140)),
];

/// The picker's grid: one column per hue, rows light / base / dark, then a
/// neutrals row led by "none" (default styling). `Some(None)` is the none
/// cell, `None` an empty cell.
pub(crate) const COLOR_GRID: [[Option<Option<&str>>; 8]; 4] = [
    [Some(Some("light-red")), Some(Some("light-orange")), Some(Some("light-yellow")), Some(Some("light-green")),
     Some(Some("light-cyan")), Some(Some("light-blue")), Some(Some("light-magenta")), Some(Some("light-pink"))],
    [Some(Some("red")), Some(Some("orange")), Some(Some("yellow")), Some(Some("green")),
     Some(Some("cyan")), Some(Some("blue")), Some(Some("magenta")), Some(Some("pink"))],
    [Some(Some("dark-red")), Some(Some("dark-orange")), Some(Some("dark-yellow")), Some(Some("dark-green")),
     Some(Some("dark-cyan")), Some(Some("dark-blue")), Some(Some("dark-magenta")), Some(Some("dark-pink"))],
    [Some(None), Some(Some("white")), Some(Some("silver")), Some(Some("grey")), Some(Some("slate")), None, None, None],
];

/// Grid cell of a stored color; `None` and unknown names sit on "none".
pub(crate) fn color_grid_position(current: Option<&str>) -> (usize, usize) {
    for (r, row) in COLOR_GRID.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            if *cell == Some(current) {
                return (r, c);
            }
        }
    }
    (3, 0)
}

/// Move one step in the grid, skipping empty cells and clamping at edges.
pub(crate) fn color_grid_step((row, col): (usize, usize), dr: i32, dc: i32) -> (usize, usize) {
    let (mut r, mut c) = (row as i32, col as i32);
    loop {
        let (nr, nc) = (r + dr, c + dc);
        if !(0..COLOR_GRID.len() as i32).contains(&nr) || !(0..8).contains(&nc) {
            return (row, col);
        }
        (r, c) = (nr, nc);
        if COLOR_GRID[r as usize][c as usize].is_some() {
            return (r as usize, c as usize);
        }
        if dr != 0 {
            // Moving down into a short row: land on its last filled cell.
            if let Some(last) = COLOR_GRID[r as usize].iter().rposition(Option::is_some) {
                return (r as usize, last);
            }
        }
    }
}

/// The stored value a grid cell picks.
pub(crate) fn color_grid_value((row, col): (usize, usize)) -> Option<String> {
    COLOR_GRID[row][col].flatten().map(str::to_string)
}

/// Resolve a stored palette name to its `Color`. Unknown names (palette
/// drift, hand-edited manifest) resolve to `None` = default styling.
pub(crate) fn user_color(name: &str) -> Option<Color> {
    USER_COLORS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// A user accent faded for a stale-idle row: same hue, lower brightness, so
/// the row recedes like other stale rows but keeps the color Owner chose.
pub(crate) fn faded(color: Color) -> Color {
    match color {
        Color::Rgb(r, g, b) => {
            let f = |c: u8| (u16::from(c) * 55 / 100) as u8;
            Color::Rgb(f(r), f(g), f(b))
        }
        _ => DIM,
    }
}

/// Task-sidebar section surfaces, with optional user-color accents.
/// Keep a neutral base so missing colors are visible rather than near black.
/// Terminal opacity is applied by the terminal emulator; ANSI colors have no alpha.
pub(crate) fn sidebar_section_bg(color: Option<&str>, strength: f64) -> Color {
    if strength <= 0.0 { return Color::Reset; }
    let (r, g, b) = match color.and_then(user_color) {
        Some(Color::Rgb(r, g, b)) => (16 + r / 12, 18 + g / 12, 22 + b / 12),
        _ => (25, 29, 38),
    };
    // Float-to-u8 conversion saturates, keeping large intensities in range.
    Color::Rgb((r as f64 * strength).round() as u8,
        (g as f64 * strength).round() as u8, (b as f64 * strength).round() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-12: a session set to blue showed grey once it went stale-idle,
    /// because the stale fade replaced the accent. The fade keeps the hue.
    #[test]
    fn faded_accent_keeps_its_hue_and_loses_brightness() {
        let blue = user_color("blue").unwrap();
        let Color::Rgb(r, g, b) = faded(blue) else { panic!("rgb in, rgb out") };
        assert!(b > r && b > g, "still blue: {r},{g},{b}");
        let Color::Rgb(_, _, b0) = blue else { unreachable!() };
        assert!(b < b0, "dimmer than the full accent");
        assert_eq!(faded(Color::Blue), DIM, "non-rgb falls back to the stale grey");
    }

    /// The original eight names keep resolving to their original values,
    /// so manifests saved before the larger palette look the same.
    #[test]
    fn old_color_names_resolve_unchanged() {
        for (name, rgb) in [
            ("red", (255, 105, 105)), ("orange", (255, 160, 70)), ("yellow", (240, 210, 70)),
            ("green", (110, 220, 110)), ("cyan", (80, 210, 210)), ("blue", (110, 160, 255)),
            ("magenta", (215, 110, 235)), ("pink", (255, 130, 180)),
        ] {
            assert_eq!(user_color(name), Some(Color::Rgb(rgb.0, rgb.1, rgb.2)), "{name}");
        }
        assert!(USER_COLORS.len() >= 24);
        assert_eq!(user_color("mauve-nonexistent"), None);
    }

    /// Every palette entry is reachable in the grid exactly once, and every
    /// grid name is in the palette.
    #[test]
    fn the_grid_covers_the_palette_exactly() {
        let mut seen = std::collections::HashSet::new();
        for row in COLOR_GRID {
            for name in row.into_iter().flatten().flatten() {
                assert!(user_color(name).is_some(), "{name}");
                assert!(seen.insert(name), "{name} twice");
            }
        }
        assert_eq!(seen.len(), USER_COLORS.len());
    }

    #[test]
    fn grid_moves_clamp_and_skip_empty_cells() {
        assert_eq!(color_grid_position(Some("blue")), (1, 5));
        assert_eq!(color_grid_position(None), (3, 0));
        assert_eq!(color_grid_position(Some("gone")), (3, 0));
        assert_eq!(color_grid_step((1, 5), -1, 0), (0, 5));
        assert_eq!(color_grid_step((0, 5), -1, 0), (0, 5), "top edge");
        assert_eq!(color_grid_step((1, 7), 0, 1), (1, 7), "right edge");
        assert_eq!(color_grid_step((2, 7), 1, 0), (3, 4), "down into the short neutrals row");
        assert_eq!(color_grid_step((3, 4), 0, 1), (3, 4), "no cells right of slate");
        assert_eq!(color_grid_value((3, 0)), None);
        assert_eq!(color_grid_value((2, 0)).as_deref(), Some("dark-red"));
    }
}
