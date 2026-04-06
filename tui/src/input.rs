use alacritty_terminal::term::TermMode;
use crossterm::event::Event as CrosstermEvent;
use terminput::Encoding;
use terminput_crossterm::to_terminput;

/// Convert a crossterm Event into raw ANSI bytes suitable for writing to a PTY.
/// Uses Kitty keyboard encoding if the terminal has enabled it, otherwise Xterm legacy.
pub fn event_to_bytes(event: &CrosstermEvent, term_mode: &TermMode) -> Option<Vec<u8>> {
    let terminput_event = to_terminput(event.clone()).ok()?;
    let mut buf = [0u8; 64];

    let encoding = if term_mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        // Build KittyFlags from the terminal mode.
        let mut flags = terminput::KittyFlags::DISAMBIGUATE_ESCAPE_CODES;
        if term_mode.contains(TermMode::REPORT_EVENT_TYPES) {
            flags |= terminput::KittyFlags::REPORT_EVENT_TYPES;
        }
        if term_mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
            flags |= terminput::KittyFlags::REPORT_ALTERNATE_KEYS;
        }
        if term_mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
            flags |= terminput::KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        }
        Encoding::Kitty(flags)
    } else {
        Encoding::Xterm
    };

    let written = terminput_event.encode(&mut buf, encoding).ok()?;
    if written == 0 {
        return None;
    }
    Some(buf[..written].to_vec())
}
