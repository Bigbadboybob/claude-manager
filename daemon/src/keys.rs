//! One key, encoded for a session's PTY: the mapping behind `send_key`.
//!
//! The daemon's `session.send_key` RPC and the TUI's `send_key` handler both
//! call [`key_bytes`], and the TUI's own keystroke encoder takes its
//! encoding from [`encoding_for_mode`], so a key an agent sends is the byte
//! sequence Owner's keypress would produce in the same keyboard mode.
//!
//! - Kitty keyboard mode (`DISAMBIGUATE_ESC_CODES`, which claude and codex
//!   enable): Enter is `CSI 13 u`, the encoding every CM submit path already
//!   uses ([`crate::workflow::pty_tracker::enter_bytes_for_mode`]); Escape is
//!   `CSI 27 u` and ctrl-c `CSI 99;5 u`, as kitty's disambiguation requires.
//! - Legacy mode (bash, or an engine that left kitty mode): `\r`, `\x1b`,
//!   `\x03`, and xterm sequences for the rest.
//!
//! Exactly one key is encoded: no text body, never a trailing Enter.

use alacritty_terminal::term::TermMode;
use terminput::{Encoding, KeyCode, KeyEvent, KeyModifiers, KittyFlags};

/// The named keys `send_key` accepts (any single printable character is
/// accepted too).
pub const KEY_NAMES: &[&str] = &[
    "enter", "escape", "tab", "shift-tab", "up", "down", "left", "right", "home", "end",
    "pageup", "pagedown", "backspace", "delete", "space", "ctrl-c", "ctrl-d",
];

/// The keyboard encoding for a terminal mode: kitty with the flags the
/// program enabled, else xterm. The TUI's keystroke encoder uses this too.
pub fn encoding_for_mode(mode: TermMode) -> Encoding {
    if !mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
        return Encoding::Xterm;
    }
    let mut flags = KittyFlags::DISAMBIGUATE_ESCAPE_CODES;
    if mode.contains(TermMode::REPORT_EVENT_TYPES) {
        flags |= KittyFlags::REPORT_EVENT_TYPES;
    }
    if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
        flags |= KittyFlags::REPORT_ALTERNATE_KEYS;
    }
    if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
        flags |= KittyFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
    }
    Encoding::Kitty(flags)
}

/// Parse a `send_key` key: a name from [`KEY_NAMES`] (case-insensitive;
/// `esc` and `return` also work) or one printable character.
pub fn parse_key(key: &str) -> Result<KeyEvent, String> {
    let plain = |code| KeyEvent::new(code);
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c)).modifiers(KeyModifiers::CTRL);
    let event = match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => plain(KeyCode::Enter),
        "escape" | "esc" => plain(KeyCode::Esc),
        "tab" => plain(KeyCode::Tab),
        "shift-tab" | "backtab" => KeyEvent::new(KeyCode::Tab).modifiers(KeyModifiers::SHIFT),
        "up" => plain(KeyCode::Up),
        "down" => plain(KeyCode::Down),
        "left" => plain(KeyCode::Left),
        "right" => plain(KeyCode::Right),
        "home" => plain(KeyCode::Home),
        "end" => plain(KeyCode::End),
        "pageup" => plain(KeyCode::PageUp),
        "pagedown" => plain(KeyCode::PageDown),
        "backspace" => plain(KeyCode::Backspace),
        "delete" => plain(KeyCode::Delete),
        "space" => plain(KeyCode::Char(' ')),
        "ctrl-c" => ctrl('c'),
        "ctrl-d" => ctrl('d'),
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if !c.is_control() => plain(KeyCode::Char(c)),
                _ => {
                    return Err(format!(
                        "unknown key {key:?}: use one of {} or a single printable character",
                        KEY_NAMES.join(", ")
                    ))
                }
            }
        }
    };
    Ok(event)
}

/// Encode an input event for a program in `mode`; None when it has no
/// encoding. The TUI's keystroke path and [`key_bytes`] both use this.
///
/// terminput 0.5 writes kitty PageUp/PageDown as `CSI 5~u` and, modified,
/// `CSI 5~;2u`. The kitty protocol keeps the legacy tilde form for these
/// keys (`CSI 5~`, `CSI 5;2~`), as terminput already does for Delete, so
/// the form is repaired here.
pub fn encode(event: terminput::Event, mode: TermMode) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    let n = event.encode(&mut buf, encoding_for_mode(mode)).ok()?;
    if n == 0 {
        return None;
    }
    Some(fix_tilde_csi_u(&buf[..n]))
}

/// `CSI <n> ~ [;<mods>] u` → `CSI <n> [;<mods>] ~`; anything else unchanged.
fn fix_tilde_csi_u(bytes: &[u8]) -> Vec<u8> {
    let body = bytes.strip_prefix(b"\x1b[").and_then(|b| b.strip_suffix(b"u"));
    if let Some(body) = body {
        if let Some(t) = body.iter().position(|&c| c == b'~') {
            let (num, rest) = (&body[..t], &body[t + 1..]);
            let digits = |s: &[u8]| !s.is_empty() && s.iter().all(u8::is_ascii_digit);
            let mods_ok = rest.is_empty() || rest.strip_prefix(b";").is_some_and(digits);
            if digits(num) && mods_ok {
                let mut out = b"\x1b[".to_vec();
                out.extend_from_slice(num);
                out.extend_from_slice(rest);
                out.push(b'~');
                return out;
            }
        }
    }
    bytes.to_vec()
}

/// The bytes one key sends to a program in `mode`.
pub fn key_bytes(key: &str, mode: TermMode) -> Result<Vec<u8>, String> {
    let event = parse_key(key)?;
    if event.code == KeyCode::Enter && event.modifiers.is_empty() {
        return Ok(crate::workflow::pty_tracker::enter_bytes_for_mode(mode).to_vec());
    }
    encode(terminput::Event::Key(event), mode)
        .ok_or_else(|| format!("key {key:?} has no encoding in this keyboard mode"))
}

/// Whether `mode` is kitty keyboard mode, for reporting.
pub fn is_kitty(mode: TermMode) -> bool {
    mode.contains(TermMode::DISAMBIGUATE_ESC_CODES)
}

/// The `TermMode` for a set of kitty progressive-enhancement flags.
pub fn mode_for_flags(flags: u16) -> TermMode {
    let mut mode = TermMode::empty();
    for (bit, m) in [
        (1, TermMode::DISAMBIGUATE_ESC_CODES),
        (2, TermMode::REPORT_EVENT_TYPES),
        (4, TermMode::REPORT_ALTERNATE_KEYS),
        (8, TermMode::REPORT_ALL_KEYS_AS_ESC),
        (16, TermMode::REPORT_ASSOCIATED_TEXT),
    ] {
        if flags & bit != 0 {
            mode |= m;
        }
    }
    mode
}

/// The kitty keyboard flag stack as last known, persisted across brain
/// restarts beside the replay ring (`crate::fanout_persist`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KeyboardSnapshot {
    /// Pushed entries we saw, bottom first; the current flags are the top.
    pub stack: Vec<u16>,
    /// Entries may sit below `stack` that we never saw (the output before
    /// we started watching). With `stack` empty that makes the current
    /// mode unknown.
    pub floor_unknown: bool,
}

/// Escape-sequence scan state, carried across chunk boundaries.
#[derive(Clone, Debug, Default)]
enum Scan {
    #[default]
    Ground,
    Esc,
    Csi(Vec<u8>),
}

/// The longest CSI parameter string the scanner keeps; a longer one is
/// not a keyboard sequence.
const MAX_CSI_PARAMS: usize = 32;
/// Kitty caps its stack too; anything deeper drops the oldest entry.
const MAX_STACK: usize = 32;

/// A session's kitty keyboard mode, followed as its output streams past:
/// `CSI > f u` pushes flags, `CSI < n u` pops n entries, `CSI = f ; m u`
/// sets (m=1), ORs (2) or clears (3) the current flags, and `ESC c` resets.
/// A sequence split across chunks is carried over. Only the main stack is
/// tracked (claude and codex run inline, without the alternate screen).
#[derive(Clone, Debug, Default)]
pub struct KeyboardTracker {
    state: KeyboardSnapshot,
    scan: Scan,
}

impl KeyboardTracker {
    /// A session watched from its first byte: legacy until it pushes.
    pub fn fresh() -> Self {
        Self::default()
    }

    /// A session whose earlier output we never saw.
    pub fn unknown() -> Self {
        Self::restored(KeyboardSnapshot { stack: Vec::new(), floor_unknown: true })
    }

    pub fn restored(state: KeyboardSnapshot) -> Self {
        Self { state, scan: Scan::Ground }
    }

    pub fn snapshot(&self) -> KeyboardSnapshot {
        self.state.clone()
    }

    /// The current mode, or None when it is unknown.
    pub fn mode(&self) -> Option<TermMode> {
        match self.state.stack.last() {
            Some(&flags) => Some(mode_for_flags(flags)),
            None if self.state.floor_unknown => None,
            None => Some(TermMode::empty()),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.scan = match std::mem::take(&mut self.scan) {
                Scan::Ground => match b {
                    0x1b => Scan::Esc,
                    _ => Scan::Ground,
                },
                Scan::Esc => match b {
                    b'[' => Scan::Csi(Vec::new()),
                    b'c' => {
                        // RIS: a full reset leaves no flags pushed.
                        self.state = KeyboardSnapshot::default();
                        Scan::Ground
                    }
                    0x1b => Scan::Esc,
                    _ => Scan::Ground,
                },
                Scan::Csi(mut params) => match b {
                    0x1b => Scan::Esc,
                    0x20..=0x3f if params.len() < MAX_CSI_PARAMS => {
                        params.push(b);
                        Scan::Csi(params)
                    }
                    0x40..=0x7e => {
                        if b == b'u' {
                            self.apply(&params);
                        }
                        Scan::Ground
                    }
                    _ => Scan::Ground,
                },
            };
        }
    }

    fn apply(&mut self, params: &[u8]) {
        let Some((&op, rest)) = params.split_first() else { return };
        let text = std::str::from_utf8(rest).unwrap_or("");
        let mut nums = text.split(';').map(|n| n.parse::<u16>().ok());
        let first = nums.next().flatten();
        let stack = &mut self.state.stack;
        match op {
            b'>' => {
                if stack.len() == MAX_STACK {
                    stack.remove(0);
                }
                stack.push(first.unwrap_or(0));
            }
            b'<' => {
                for _ in 0..first.unwrap_or(1).max(1) {
                    if stack.pop().is_none() {
                        break;
                    }
                }
            }
            b'=' => {
                let flags = first.unwrap_or(0);
                let how = nums.next().flatten().unwrap_or(1);
                match (stack.last_mut(), how) {
                    (Some(top), 1) => *top = flags,
                    (Some(top), 2) => *top |= flags,
                    (Some(top), 3) => *top &= !flags,
                    (Some(_), _) => {}
                    // The top entry is unseen: only a plain set makes it
                    // known (popping it later still reveals the unseen floor).
                    (None, 1) if self.state.floor_unknown => stack.push(flags),
                    (None, _) if self.state.floor_unknown => {}
                    // Kitty keeps a base entry of 0 below every push.
                    (None, 1) => stack.push(flags),
                    (None, 2) => stack.push(flags),
                    (None, _) => {}
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KITTY: TermMode = TermMode::DISAMBIGUATE_ESC_CODES;

    fn legacy() -> TermMode {
        TermMode::empty()
    }

    /// Every key's bytes in both keyboard modes (Owner 2026-10-22).
    #[test]
    fn every_key_encodes_for_both_keyboard_modes() {
        let cases: &[(&str, &[u8], &[u8])] = &[
            // key, legacy, kitty
            ("enter", b"\r", b"\x1b[13u"),
            ("escape", b"\x1b", b"\x1b[27u"),
            // Kitty keeps Owner's own keystrokes' encoding (the TUI types
            // through `encode` too): CSI-u for Tab and Backspace.
            ("tab", b"\t", b"\x1b[9u"),
            ("shift-tab", b"\x1b[Z", b"\x1b[9;2u"),
            ("up", b"\x1b[A", b"\x1b[A"),
            ("down", b"\x1b[B", b"\x1b[B"),
            ("right", b"\x1b[C", b"\x1b[C"),
            ("left", b"\x1b[D", b"\x1b[D"),
            ("home", b"\x1b[H", b"\x1b[H"),
            ("end", b"\x1b[F", b"\x1b[F"),
            ("pageup", b"\x1b[5~", b"\x1b[5~"),
            ("pagedown", b"\x1b[6~", b"\x1b[6~"),
            ("backspace", b"\x7f", b"\x1b[127u"),
            ("delete", b"\x1b[3~", b"\x1b[3~"),
            ("space", b" ", b" "),
            ("ctrl-c", b"\x03", b"\x1b[99;5u"),
            ("ctrl-d", b"\x04", b"\x1b[100;5u"),
            ("1", b"1", b"1"),
            ("y", b"y", b"y"),
            ("é", "é".as_bytes(), "é".as_bytes()),
        ];
        for (key, want_legacy, want_kitty) in cases {
            assert_eq!(key_bytes(key, legacy()).unwrap(), *want_legacy, "{key} legacy");
            assert_eq!(key_bytes(key, KITTY).unwrap(), *want_kitty, "{key} kitty");
        }
        // Every advertised name is covered above.
        for name in KEY_NAMES {
            assert!(cases.iter().any(|(k, ..)| k == name), "{name} untested");
        }
    }

    /// Enter is the one encoding every CM submit path uses, and no key
    /// carries a trailing Enter.
    #[test]
    fn enter_matches_the_submit_path_and_no_key_adds_one() {
        for mode in [legacy(), KITTY] {
            assert_eq!(
                key_bytes("enter", mode).unwrap(),
                crate::workflow::pty_tracker::enter_bytes_for_mode(mode)
            );
            for key in KEY_NAMES.iter().copied().chain(["1", "y"]) {
                if key == "enter" {
                    continue;
                }
                let bytes = key_bytes(key, mode).unwrap();
                assert!(!bytes.ends_with(b"\r") && !bytes.ends_with(b"\n") && !bytes.ends_with(b"\x1b[13u"), "{key}: {bytes:?}");
            }
        }
    }

    /// Review of a41f14d (2): modified PageUp/PageDown keep the tilde form.
    #[test]
    fn modified_pageup_and_pagedown_use_the_tilde_form_in_kitty_mode() {
        let key = |code, mods| terminput::Event::Key(KeyEvent::new(code).modifiers(mods));
        let cases = [
            (KeyCode::PageUp, KeyModifiers::SHIFT, "\x1b[5;2~"),
            (KeyCode::PageUp, KeyModifiers::ALT, "\x1b[5;3~"),
            (KeyCode::PageDown, KeyModifiers::CTRL, "\x1b[6;5~"),
            (KeyCode::PageDown, KeyModifiers::NONE, "\x1b[6~"),
            (KeyCode::Delete, KeyModifiers::SHIFT, "\x1b[3;2~"),
        ];
        for (code, mods, want) in cases {
            let got = encode(key(code, mods), KITTY).unwrap();
            assert_eq!(String::from_utf8_lossy(&got), want, "{code:?} {mods:?}");
        }
        // Other CSI-u forms are left alone.
        assert_eq!(fix_tilde_csi_u(b"\x1b[99;5u"), b"\x1b[99;5u");
        assert_eq!(fix_tilde_csi_u(b"\x1b[13u"), b"\x1b[13u");
    }

    fn mode_of(t: &KeyboardTracker) -> Option<bool> {
        t.mode().map(is_kitty)
    }

    /// Push, pop and set, each split across chunk boundaries.
    #[test]
    fn the_tracker_follows_push_pop_and_set_across_chunks() {
        let mut t = KeyboardTracker::fresh();
        assert_eq!(mode_of(&t), Some(false), "a fresh session starts legacy");
        for chunk in [&b"text \x1b"[..], b"[", b">", b"1", b"u more"] {
            t.feed(chunk);
        }
        assert_eq!(mode_of(&t), Some(true), "push split over four chunks");
        t.feed(b"\x1b[=0;1u");
        assert_eq!(mode_of(&t), Some(false), "set to 0");
        t.feed(b"\x1b[=1;2");
        t.feed(b"u");
        assert_eq!(mode_of(&t), Some(true), "OR 1, split");
        t.feed(b"\x1b[<");
        t.feed(b"u");
        assert_eq!(mode_of(&t), Some(false), "popped back to the base");
        t.feed(b"\x1b[>1u\x1b[>3u\x1b[<2u");
        assert_eq!(mode_of(&t), Some(false), "pop 2");
        t.feed(b"\x1b[>1u\x1bc");
        assert_eq!(mode_of(&t), Some(false), "RIS resets");
        // Other CSI sequences, queries and noise change nothing.
        t.feed(b"\x1b[>1u\x1b[?u\x1b[31m\x1b[2J\x1b[");
        t.feed(b"5~");
        assert_eq!(mode_of(&t), Some(true));
    }

    /// An adopted session with no saved mode: unknown until its output
    /// shows a push or a plain set; a pop past what we saw is unknown again.
    #[test]
    fn an_unseen_floor_stays_unknown_until_the_program_says() {
        let mut t = KeyboardTracker::unknown();
        assert_eq!(mode_of(&t), None);
        t.feed(b"\x1b[=1;2u");
        assert_eq!(mode_of(&t), None, "OR onto an unseen value");
        t.feed(b"\x1b[>1u");
        assert_eq!(mode_of(&t), Some(true));
        t.feed(b"\x1b[<u");
        assert_eq!(mode_of(&t), None, "back to the unseen floor");
        t.feed(b"\x1b[=0;1u");
        assert_eq!(mode_of(&t), Some(false), "a plain set is known");
        // The snapshot round-trips.
        let restored = KeyboardTracker::restored(t.snapshot());
        assert_eq!(mode_of(&restored), Some(false));
        let json = serde_json::to_string(&t.snapshot()).unwrap();
        assert_eq!(serde_json::from_str::<KeyboardSnapshot>(&json).unwrap(), t.snapshot());
    }

    #[test]
    fn names_are_case_insensitive_and_bad_keys_are_refused() {
        assert_eq!(key_bytes("ENTER", KITTY).unwrap(), b"\x1b[13u");
        assert_eq!(key_bytes("Esc", legacy()).unwrap(), b"\x1b");
        assert_eq!(key_bytes("Y", legacy()).unwrap(), b"Y", "a single character keeps its case");
        for bad in ["", "yes", "ctrl-x", "\n", "\x1b", "f1"] {
            let err = key_bytes(bad, legacy()).unwrap_err();
            assert!(err.contains("unknown key"), "{bad:?}: {err}");
        }
    }
}
