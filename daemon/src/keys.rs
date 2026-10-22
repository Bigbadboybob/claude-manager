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
/// terminput 0.5 writes kitty PageUp/PageDown as `CSI 5~u` / `CSI 6~u`; the
/// kitty protocol keeps the legacy `CSI 5~` / `CSI 6~` for these (as for
/// Delete, which it already gets right), so the stray `u` is dropped here.
pub fn encode(event: terminput::Event, mode: TermMode) -> Option<Vec<u8>> {
    let mut buf = [0u8; 64];
    let n = event.encode(&mut buf, encoding_for_mode(mode)).ok()?;
    if n == 0 {
        return None;
    }
    let mut out = buf[..n].to_vec();
    if out.ends_with(b"~u") {
        out.pop();
    }
    Some(out)
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
