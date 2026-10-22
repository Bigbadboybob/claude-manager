use alacritty_terminal::term::TermMode;
use crossterm::event::Event as CrosstermEvent;
use terminput_crossterm::to_terminput;

/// Convert a crossterm Event into raw ANSI bytes suitable for writing to a PTY.
/// Uses Kitty keyboard encoding if the terminal has enabled it, otherwise Xterm
/// legacy. The encoding is the daemon's shared key mapping
/// (`cm_daemon::keys`), which `send_key` uses too.
pub fn event_to_bytes(event: &CrosstermEvent, term_mode: &TermMode) -> Option<Vec<u8>> {
    let terminput_event = to_terminput(event.clone()).ok()?;
    cm_daemon::keys::encode(terminput_event, *term_mode)
}
