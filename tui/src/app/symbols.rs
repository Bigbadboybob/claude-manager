//! One-cell symbols for engines, hosts, board items and the live activity
//! badge (Owner requests 2026-10-21/22). This is the only mapping: the
//! sidebar, the org chart, its legend and the A-i peek all read it. Every symbol is plain Unicode one terminal cell
//! wide, also in East Asian contexts (no Ambiguous-width characters, so
//! nothing from U+25A0-25EF's ambiguous ranges), with no emoji presentation
//! and no Nerd Font glyphs. ◆ stays reserved for divisions and the
//! owner-blocked blink.

use crate::theme;
use cm_daemon::host_id::HostId;
use ratatui::style::{Modifier, Style};

/// The engine's mark: Claude ✻ (the mark Claude Code itself uses) in
/// orange, Codex ⬡ in teal, bash `$`.
pub(crate) fn engine(session_type: &str) -> (&'static str, Style) {
    match session_type {
        "claude" | "claude-code" => ("\u{273b}", Style::default().fg(theme::CLAUDE)),
        "codex" => ("\u{2b21}", Style::default().fg(theme::CODEX)),
        "bash" => ("$", Style::default().fg(theme::MUTED)),
        _ => ("-", Style::default().fg(theme::DIM)),
    }
}

/// The engine's word, for places that keep one (info pane, A-i peek).
pub(crate) fn engine_name(session_type: &str) -> &str {
    match session_type {
        "claude" | "claude-code" => "claude",
        "bash" => "shell",
        other => other,
    }
}

/// Mark and word, as the A-i peek shows an engine: `✻ claude`.
pub(crate) fn engine_with_name(session_type: &str) -> String {
    format!("{} {}", engine(session_type).0, engine_name(session_type))
}

pub(crate) const LOCAL: &str = "\u{2302}";

/// ⌂ for this machine (as in the sidebar), else the host's first letter.
pub(crate) fn host(id: &HostId) -> (String, Style) {
    if *id == HostId::local() {
        (LOCAL.into(), Style::default().fg(theme::DIM))
    } else {
        (id.as_str().chars().next().unwrap_or('?').to_string(), Style::default().fg(theme::REMOTE))
    }
}

/// A board item's status mark.
pub(crate) fn board_status(status: &str) -> (&'static str, Style) {
    let bold = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
    match status {
        "open" => ("\u{25cc}", Style::default().fg(theme::MUTED)),
        "active" => ("\u{27a4}", bold(theme::OK)),
        "waiting" => ("\u{25f7}", bold(theme::HEADER)),
        "blocked" => ("\u{2298}", bold(theme::ATTN)),
        "blocked_on_owner" => ("\u{229b}", super::owner_blocked::owner_style()),
        "done" => ("\u{2713}", Style::default().fg(theme::DIM)),
        "dropped" => ("\u{2717}", Style::default().fg(theme::DIM)),
        _ => ("?", Style::default().fg(theme::TEXT)),
    }
}

/// A chart box with no session of its own: Owner, a project, Teams or
/// Continuous. A division keeps ◆, as in the sidebar.
pub(crate) const HEADER_BOX: &str = "\u{229e}";
pub(crate) const DIVISION: &str = "\u{25c6}";

/// Owner leads this task or division (pinned, or standing in for a division
/// root with no live lead): beside the level's leader mark.
pub(crate) const OWNER: &str = "\u{2654}";

pub(crate) fn owner_style() -> Style {
    Style::default().fg(theme::LEAD).add_modifier(Modifier::BOLD)
}

/// "Owner" or "Owner (stand-in)", as a box or field names Owner as lead.
pub(crate) fn owner_label(stand_in: bool) -> &'static str {
    if stand_in { "Owner (stand-in)" } else { "Owner" }
}

/// Org model marks (F6 from teams): a team's current epic chip, a guest
/// member, a manager the drawn line doesn't show (matrix), a vacant member.
pub(crate) const EPIC: &str = "\u{27d0}";
pub(crate) const GUEST: &str = "\u{22b9}";
pub(crate) const MATRIX: &str = "\u{22ef}";
pub(crate) const VACANT: &str = "\u{2205}";

/// A board item that carries a flag (holder idle, overdue, …).
pub(crate) const FLAG: &str = "\u{2691}";

pub(crate) fn flag_style() -> Style {
    Style::default().fg(theme::ERROR).add_modifier(Modifier::BOLD)
}

/// How urgent a held item's mark is, for picking one per session.
fn board_rank(status: &str, flagged: bool) -> u8 {
    if flagged {
        return 0;
    }
    match status {
        "blocked_on_owner" => 1,
        "blocked" => 2,
        "waiting" => 3,
        "active" => 4,
        "open" => 5,
        _ => 6,
    }
}

/// The one mark a card shows for a session's held items: a flag if any is
/// flagged, else the most urgent status.
pub(crate) fn held_mark<'a>(items: impl IntoIterator<Item = (&'a str, bool)>) -> Option<(&'static str, Style)> {
    let (status, flagged) = items.into_iter().min_by_key(|&(s, f)| board_rank(s, f))?;
    Some(if flagged { (FLAG, flag_style()) } else { board_status(status) })
}

/// What a session is doing right now: the kind of its live activity badge
/// (see `agent_state::badge`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Doing {
    /// Its foreground turn is running.
    Working,
    /// Its turn has ended, with background jobs still running.
    Background,
    /// It waits on a human: a permission prompt, a question, input.
    Waiting,
    /// It stopped on a rate or usage limit and needs waking later.
    RateLimited,
    /// It stopped on any other error.
    Errored,
    /// The engine is retrying a failed request.
    Retrying,
    /// It is working, but has shown no progress for a while.
    Stalled,
    /// Its Claude transcript is past the rotation threshold (#24).
    Heavy,
}

/// The badge's mark and style. A rate limit stands out (bold, reversed),
/// since Owner scans for it to wake those sessions.
pub(crate) fn doing(kind: Doing) -> (&'static str, Style) {
    let bold = |c| Style::default().fg(c).add_modifier(Modifier::BOLD);
    match kind {
        Doing::Working => ("\u{2699}", Style::default().fg(theme::OK)),
        Doing::Background => ("\u{29d7}", Style::default().fg(theme::OK).add_modifier(Modifier::DIM)),
        Doing::Waiting => ("\u{261b}", bold(theme::ATTN)),
        Doing::RateLimited => ("\u{26a0}", bold(theme::ATTN).add_modifier(Modifier::REVERSED)),
        Doing::Errored => ("\u{2715}", bold(theme::ERROR)),
        Doing::Retrying => ("\u{21bb}", Style::default().fg(theme::ATTN)),
        Doing::Stalled => ("\u{23f1}", Style::default().fg(theme::ATTN)),
        Doing::Heavy => ("\u{29c9}", Style::default().fg(theme::ATTN)),
    }
}

/// The badge text's style: quiet for routine work, the mark's colour (not
/// reversed) when the session needs someone.
pub(crate) fn doing_text(kind: Doing) -> Style {
    match kind {
        Doing::Working | Doing::Background => Style::default().fg(theme::MUTED),
        Doing::RateLimited => Style::default().fg(theme::ATTN).add_modifier(Modifier::BOLD),
        other => doing(other).1,
    }
}

/// Every mark with its meaning, for the legend (`?` in the org chart).
pub(crate) fn legend() -> Vec<(String, Style, &'static str)> {
    let mut out = Vec::new();
    for (t, word) in [("claude", "claude"), ("codex", "codex"), ("bash", "bash")] {
        let (s, st) = engine(t);
        out.push((s.to_owned(), st, word));
    }
    out.push((LOCAL.to_owned(), Style::default().fg(theme::DIM), "local"));
    out.push(("s".to_owned(), Style::default().fg(theme::REMOTE), "host initial"));
    for (status, word) in [
        ("open", "open"),
        ("active", "active"),
        ("waiting", "waiting"),
        ("blocked", "blocked"),
        ("blocked_on_owner", "on Owner"),
        ("done", "done"),
        ("dropped", "dropped"),
    ] {
        let (s, st) = board_status(status);
        out.push((s.to_owned(), st, word));
    }
    out.push((FLAG.to_owned(), flag_style(), "flagged"));
    out.push((OWNER.to_owned(), owner_style(), "Owner leads"));
    out
}

/// The org model's marks, for the legend when F6 draws teams.
pub(crate) fn org_legend() -> Vec<(String, Style, &'static str)> {
    vec![
        (EPIC.to_owned(), Style::default().fg(theme::HEADER), "epic"),
        (GUEST.to_owned(), Style::default().fg(theme::MUTED), "guest"),
        (MATRIX.to_owned(), Style::default().fg(theme::MUTED), "manager elsewhere"),
        (VACANT.to_owned(), Style::default().fg(theme::DIM), "vacant"),
    ]
}

/// The activity badge's marks with their meanings: the legend's second line.
pub(crate) fn activity_legend() -> Vec<(String, Style, &'static str)> {
    let mut out = Vec::new();
    for (kind, word) in [
        (Doing::Working, "working"),
        (Doing::Background, "background jobs"),
        (Doing::Waiting, "waits on you"),
        (Doing::RateLimited, "rate limited"),
        (Doing::Errored, "error"),
        (Doing::Retrying, "retrying"),
        (Doing::Stalled, "stalled"),
        (Doing::Heavy, "huge transcript (rotates)"),
    ] {
        let (s, st) = doing(kind);
        out.push((s.to_owned(), st, word));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    #[test]
    fn every_symbol_is_one_cell_wide_even_in_cjk_terminals() {
        use unicode_width::UnicodeWidthStr;
        let mut all: Vec<(String, &str)> = legend().into_iter().chain(activity_legend()).chain(org_legend()).map(|(s, _, w)| (s, w)).collect();
        all.extend([(engine("other").0.to_owned(), "unknown engine"), (board_status("weird").0.to_owned(), "unknown status"), (HEADER_BOX.to_owned(), "header box")]);
        for (s, word) in all {
            assert_eq!(Span::raw(s.clone()).width(), 1, "{word}: {s:?}");
            assert_eq!(s.width_cjk(), 1, "{word}: {s:?} is East Asian Ambiguous");
        }
    }

    #[test]
    fn glyphs_do_not_double_up() {
        // ◆ is the division header and the owner-blocked blink only.
        let marks: Vec<String> = legend().into_iter().chain(activity_legend()).chain(org_legend()).map(|(s, _, _)| s).collect();
        assert!(!marks.contains(&DIVISION.to_owned()), "{marks:?}");
        assert!(!marks.contains(&HEADER_BOX.to_owned()) && HEADER_BOX != DIVISION);
        let unique: std::collections::HashSet<&String> = marks.iter().collect();
        assert_eq!(unique.len(), marks.len(), "each mark means one thing: {marks:?}");
    }

    #[test]
    fn engines_hosts_and_board_marks() {
        assert_eq!(engine("claude").0, "\u{273b}");
        assert_eq!(engine("claude-code"), engine("claude"));
        assert_eq!(engine("claude").1.fg, Some(theme::CLAUDE));
        assert_eq!(engine("codex").1.fg, Some(theme::CODEX));
        assert_ne!(engine("claude").0, engine("codex").0);
        assert_eq!(engine_with_name("claude-code"), "\u{273b} claude");
        assert_eq!(host(&HostId::local()).0, LOCAL);
        assert_eq!(host(&HostId::new("sessions")).0, "s");
        // A flag wins, then the most urgent status.
        assert_eq!(held_mark([("active", false), ("blocked", false)]).unwrap().0, board_status("blocked").0);
        assert_eq!(held_mark([("open", false), ("active", true)]).unwrap().0, FLAG);
        assert!(held_mark(std::iter::empty::<(&str, bool)>()).is_none());
    }
}
