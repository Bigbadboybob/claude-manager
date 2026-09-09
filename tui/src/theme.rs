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

/// Named accent colors a user can assign to sessions, workspaces, and
/// tasks via the A-e settings forms. Persisted by NAME (first tuple
/// field) so the palette can be retuned later without migrating saved
/// manifests. Order is the ←/→ cycle order in the forms.
pub(crate) const USER_COLORS: &[(&str, Color)] = &[
    ("red", Color::Rgb(255, 105, 105)),
    ("orange", Color::Rgb(255, 160, 70)),
    ("yellow", Color::Rgb(240, 210, 70)),
    ("green", Color::Rgb(110, 220, 110)),
    ("cyan", Color::Rgb(80, 210, 210)),
    ("blue", Color::Rgb(110, 160, 255)),
    ("magenta", Color::Rgb(215, 110, 235)),
    ("pink", Color::Rgb(255, 130, 180)),
];

/// Resolve a stored palette name to its `Color`. Unknown names (palette
/// drift, hand-edited manifest) resolve to `None` = default styling.
pub(crate) fn user_color(name: &str) -> Option<Color> {
    USER_COLORS.iter().find(|(n, _)| *n == name).map(|(_, c)| *c)
}

/// Step an `Option<palette name>` one slot through `USER_COLORS`,
/// treating `None` (default) as an extra slot in the cycle so the same
/// key both sets and clears a color.
pub(crate) fn cycle_user_color(current: Option<&str>, forward: bool) -> Option<String> {
    let len = USER_COLORS.len();
    // Positions: 0 = None, 1..=len = palette entries. An unknown stored
    // name behaves like None so cycling recovers from palette drift.
    let pos = match current {
        None => 0,
        Some(name) => USER_COLORS
            .iter()
            .position(|(n, _)| *n == name)
            .map(|i| i + 1)
            .unwrap_or(0),
    };
    let next = if forward { (pos + 1) % (len + 1) } else { (pos + len) % (len + 1) };
    if next == 0 {
        None
    } else {
        Some(USER_COLORS[next - 1].0.to_string())
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
