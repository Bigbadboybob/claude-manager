//! Byte-level splitter that removes kitty graphics commands
//! (`ESC _ G … ESC \`) from a pane's PTY stream before Alacritty parses
//! it, and notices the pixel-size / version queries Alacritty ignores.
//!
//! Sequences can span reads, so the filter is a small state machine. Only
//! an `ESC` or `ESC _` at the very end of a read is held back; everything
//! else that is not a graphics command passes through in order.

/// Upper bound on one graphics command. A larger command is discarded up
/// to its terminator rather than buffered.
pub const MAX_COMMAND_BYTES: usize = 64 << 20;

/// Longest CSI parameter string the filter tracks when looking for queries.
const MAX_CSI_BYTES: usize = 32;

const ESC: u8 = 0x1b;
const CAN: u8 = 0x18;
const SUB: u8 = 0x1a;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// `CSI 16 t` — cell size in pixels.
    CellSizePixels,
    /// `CSI 14 t` — text area size in pixels.
    TextAreaPixels,
    /// `CSI > q` / `CSI > 0 q` — XTVERSION.
    XtVersion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterEvent {
    /// Control data and payload of one graphics command: the bytes between
    /// `ESC _ G` and `ESC \`.
    Graphics(Vec<u8>),
    Query(Query),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    ApcStart,
    Graphics,
    GraphicsEsc,
    Discard,
    DiscardEsc,
    Csi,
}

pub struct ApcFilter {
    state: State,
    body: Vec<u8>,
    csi: Vec<u8>,
}

impl Default for ApcFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl ApcFilter {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            body: Vec::new(),
            csi: Vec::new(),
        }
    }

    /// Filter `input`, appending pass-through bytes to `out` and completed
    /// graphics commands / queries to `events`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>, events: &mut Vec<FilterEvent>) {
        let mut i = 0;
        while i < input.len() {
            match self.state {
                State::Ground => {
                    let rest = &input[i..];
                    match rest.iter().position(|&b| b == ESC) {
                        Some(p) => {
                            out.extend_from_slice(&rest[..p]);
                            self.state = State::Esc;
                            i += p + 1;
                        }
                        None => {
                            out.extend_from_slice(rest);
                            i = input.len();
                        }
                    }
                }
                State::Esc => {
                    let b = input[i];
                    i += 1;
                    match b {
                        b'_' => self.state = State::ApcStart,
                        b'[' => {
                            out.extend_from_slice(&[ESC, b'[']);
                            self.csi.clear();
                            self.state = State::Csi;
                        }
                        ESC => out.push(ESC),
                        _ => {
                            out.extend_from_slice(&[ESC, b]);
                            self.state = State::Ground;
                        }
                    }
                }
                State::ApcStart => {
                    let b = input[i];
                    if b == b'G' {
                        i += 1;
                        self.body.clear();
                        self.state = State::Graphics;
                    } else {
                        // Some other APC: Alacritty swallows it as before.
                        out.extend_from_slice(&[ESC, b'_']);
                        self.state = State::Ground;
                    }
                }
                State::Graphics => {
                    let rest = &input[i..];
                    match rest.iter().position(|&b| matches!(b, ESC | CAN | SUB)) {
                        Some(p) => {
                            if !self.push_body(&rest[..p]) {
                                i += p;
                                continue;
                            }
                            i += p + 1;
                            if rest[p] == ESC {
                                self.state = State::GraphicsEsc;
                            } else {
                                self.body.clear();
                                self.state = State::Ground;
                            }
                        }
                        None => {
                            self.push_body(rest);
                            i = input.len();
                        }
                    }
                }
                State::GraphicsEsc => {
                    if input[i] == b'\\' {
                        i += 1;
                        events.push(FilterEvent::Graphics(std::mem::take(&mut self.body)));
                        self.state = State::Ground;
                    } else {
                        // An ESC that is not ST aborts the string and starts
                        // a new sequence; reprocess this byte after it.
                        self.body.clear();
                        self.state = State::Esc;
                    }
                }
                State::Discard => {
                    let rest = &input[i..];
                    match rest.iter().position(|&b| matches!(b, ESC | CAN | SUB)) {
                        Some(p) => {
                            i += p + 1;
                            self.state = if rest[p] == ESC {
                                State::DiscardEsc
                            } else {
                                State::Ground
                            };
                        }
                        None => i = input.len(),
                    }
                }
                State::DiscardEsc => {
                    if input[i] == b'\\' {
                        i += 1;
                        self.state = State::Ground;
                    } else {
                        self.state = State::Esc;
                    }
                }
                State::Csi => {
                    let b = input[i];
                    if b == ESC {
                        i += 1;
                        self.state = State::Esc;
                        continue;
                    }
                    i += 1;
                    out.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        if let Some(q) = csi_query(&self.csi, b) {
                            events.push(FilterEvent::Query(q));
                        }
                        self.state = State::Ground;
                    } else if b == CAN || b == SUB || self.csi.len() >= MAX_CSI_BYTES {
                        self.state = State::Ground;
                    } else {
                        self.csi.push(b);
                    }
                }
            }
        }
    }

    /// Append to the command body, switching to discard mode (and returning
    /// false) when the command would exceed [`MAX_COMMAND_BYTES`].
    fn push_body(&mut self, bytes: &[u8]) -> bool {
        if self.body.len() + bytes.len() > MAX_COMMAND_BYTES {
            self.body = Vec::new();
            self.state = State::Discard;
            return false;
        }
        self.body.extend_from_slice(bytes);
        true
    }
}

fn csi_query(params: &[u8], final_byte: u8) -> Option<Query> {
    match (params, final_byte) {
        (b"16", b't') => Some(Query::CellSizePixels),
        (b"14", b't') => Some(Query::TextAreaPixels),
        (b">", b'q') | (b">0", b'q') => Some(Query::XtVersion),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&[u8]]) -> (Vec<u8>, Vec<FilterEvent>) {
        let mut filter = ApcFilter::new();
        let mut out = Vec::new();
        let mut events = Vec::new();
        for chunk in chunks {
            filter.feed(chunk, &mut out, &mut events);
        }
        (out, events)
    }

    /// Every possible split of `input` into two reads, and byte-at-a-time,
    /// must produce the same result as one read.
    fn assert_split_invariant(input: &[u8]) -> (Vec<u8>, Vec<FilterEvent>) {
        let whole = run(&[input]);
        for cut in 0..=input.len() {
            assert_eq!(run(&[&input[..cut], &input[cut..]]), whole, "split at {cut}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        assert_eq!(run(&bytes), whole, "byte at a time");
        whole
    }

    #[test]
    fn plain_text_and_other_escapes_pass_through() {
        let input = b"hello \x1b[1;31mred\x1b[0m \x1b]0;title\x07 \x1bM end";
        let (out, events) = assert_split_invariant(input);
        assert_eq!(out, input);
        assert!(events.is_empty());
    }

    #[test]
    fn graphics_command_is_removed_across_any_split() {
        let input = b"a\x1b_Gi=7,a=T,U=1,f=100;QUJD\x1b\\b";
        let (out, events) = assert_split_invariant(input);
        assert_eq!(out, b"ab");
        assert_eq!(
            events,
            vec![FilterEvent::Graphics(b"i=7,a=T,U=1,f=100;QUJD".to_vec())]
        );
    }

    #[test]
    fn consecutive_commands_and_text_keep_order() {
        let input = b"\x1b_Gm=1;AAAA\x1b\\x\x1b_Gm=0;BBBB\x1b\\\x1b\x1b[H";
        let (out, events) = assert_split_invariant(input);
        assert_eq!(out, b"x\x1b\x1b[H");
        assert_eq!(
            events,
            vec![
                FilterEvent::Graphics(b"m=1;AAAA".to_vec()),
                FilterEvent::Graphics(b"m=0;BBBB".to_vec()),
            ]
        );
    }

    #[test]
    fn non_graphics_apc_is_left_for_alacritty() {
        let input = b"\x1b_Xfoo\x1b\\z";
        let (out, events) = assert_split_invariant(input);
        assert_eq!(out, input);
        assert!(events.is_empty());
    }

    #[test]
    fn interrupted_command_is_dropped_and_next_sequence_survives() {
        // ESC that is not ST aborts; CAN aborts.
        let input = b"\x1b_Gi=1;AA\x1b[2Jok\x1b_Gi=2\x18after";
        let (out, events) = assert_split_invariant(input);
        assert_eq!(out, b"\x1b[2Jokafter");
        assert!(events.is_empty());
    }

    #[test]
    fn queries_are_reported_and_passed_through() {
        let input = b"\x1b[16t\x1b[14t\x1b[>q\x1b[>0q\x1b[18t\x1b[6n";
        let (out, events) = assert_split_invariant(input);
        assert_eq!(out, input);
        assert_eq!(
            events,
            vec![
                FilterEvent::Query(Query::CellSizePixels),
                FilterEvent::Query(Query::TextAreaPixels),
                FilterEvent::Query(Query::XtVersion),
                FilterEvent::Query(Query::XtVersion),
            ]
        );
    }

    #[test]
    fn trailing_escape_is_held_until_the_next_read() {
        let mut filter = ApcFilter::new();
        let (mut out, mut events) = (Vec::new(), Vec::new());
        filter.feed(b"abc\x1b", &mut out, &mut events);
        assert_eq!(out, b"abc");
        filter.feed(b"[m", &mut out, &mut events);
        assert_eq!(out, b"abc\x1b[m");
    }

    #[test]
    fn oversized_command_is_discarded_through_its_terminator() {
        let mut filter = ApcFilter::new();
        let (mut out, mut events) = (Vec::new(), Vec::new());
        filter.feed(b"\x1b_G", &mut out, &mut events);
        let chunk = vec![b'A'; 1 << 20];
        for _ in 0..(MAX_COMMAND_BYTES >> 20) + 1 {
            filter.feed(&chunk, &mut out, &mut events);
        }
        filter.feed(b"\x1b\\tail", &mut out, &mut events);
        assert_eq!(out, b"tail");
        assert!(events.is_empty());
    }
}
