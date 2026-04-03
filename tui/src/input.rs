use crossterm::event::Event as CrosstermEvent;
use terminput_crossterm::to_terminput;

/// Convert a crossterm Event into raw ANSI bytes suitable for writing to a PTY.
pub fn event_to_bytes(event: &CrosstermEvent) -> Option<Vec<u8>> {
    let terminput_event = to_terminput(event.clone()).ok()?;
    let mut buf = [0u8; 64];
    let written = terminput_event
        .encode(&mut buf, terminput::Encoding::Xterm)
        .ok()?;
    if written == 0 {
        return None;
    }
    Some(buf[..written].to_vec())
}
