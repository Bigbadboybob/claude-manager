//! What the terminal the TUI itself runs in (the "outer" terminal) can do,
//! and the single queue of graphics bytes destined for it.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterCaps {
    /// Returned verbatim to inner XTVERSION queries so programs detect the
    /// real terminal (for example `kitty(0.39.1)` or `ghostty 1.1.3`).
    pub version: String,
}

static CAPS: OnceLock<Option<OuterCaps>> = OnceLock::new();
/// Cell size in pixels, packed `width << 16 | height`; 0 = unknown.
static CELL: AtomicU32 = AtomicU32::new(0);
static OUTBOX: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Passthrough is on for this process.
pub fn enabled() -> bool {
    caps().is_some()
}

pub fn caps() -> Option<&'static OuterCaps> {
    CAPS.get().and_then(Option::as_ref)
}

/// Outer cell size in pixels, when known and passthrough is on.
pub fn cell_pixels() -> Option<(u16, u16)> {
    if !enabled() {
        return None;
    }
    let packed = CELL.load(Ordering::Relaxed);
    let (w, h) = ((packed >> 16) as u16, packed as u16);
    (w > 0 && h > 0).then_some((w, h))
}

fn set_cell_pixels(w: u16, h: u16) {
    if w > 0 && h > 0 {
        CELL.store((w as u32) << 16 | h as u32, Ordering::Relaxed);
    }
}

/// Re-read the cell size from the outer terminal's `TIOCGWINSZ`. Cheap;
/// called whenever the TUI's own size changes.
pub fn refresh_cell_size() {
    if !enabled() {
        return;
    }
    if let Some((w, h)) = winsize_cell(io::stdout().as_raw_fd()) {
        set_cell_pixels(w, h);
    }
}

fn winsize_cell(fd: i32) -> Option<(u16, u16)> {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ writes into a winsize we own.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    Some((ws.ws_xpixel / ws.ws_col, ws.ws_ypixel / ws.ws_row))
}

/// Queue bytes for the outer terminal. Safe from any thread; written by
/// [`flush`] on the UI thread between frames.
pub fn queue(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    OUTBOX
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .extend_from_slice(bytes);
}

/// Write queued graphics bytes. Call only between frames.
pub fn flush(out: &mut impl Write) -> io::Result<()> {
    let bytes = std::mem::take(&mut *OUTBOX.lock().unwrap_or_else(|p| p.into_inner()));
    if bytes.is_empty() {
        return Ok(());
    }
    out.write_all(&bytes)?;
    out.flush()
}

/// Turn passthrough on for in-process tests, as if a kitty terminal had
/// answered the startup probe.
#[cfg(test)]
pub fn enable_for_tests(version: &str, cell: (u16, u16)) {
    CAPS.get_or_init(|| Some(OuterCaps { version: version.into() }));
    set_cell_pixels(cell.0, cell.1);
}

/// Decide once, at startup, whether passthrough is on. Must run in raw
/// mode, before anything else reads stdin.
pub fn detect_at_startup() {
    CAPS.get_or_init(|| {
        let caps = detect();
        if caps.is_some() {
            if let Some((w, h)) = winsize_cell(io::stdout().as_raw_fd()) {
                set_cell_pixels(w, h);
            }
        }
        caps
    });
}

fn detect() -> Option<OuterCaps> {
    let forced = std::env::var("CM_KITTY_GRAPHICS").ok();
    match forced.as_deref() {
        Some("0") | Some("false") | Some("off") => return None,
        _ => {}
    }
    let force_on = matches!(forced.as_deref(), Some("1") | Some("true") | Some("on"));
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    if !force_on
        && (!env("TMUX").is_empty() || env("TERM").starts_with("screen") || env("TERM").starts_with("tmux"))
    {
        return None;
    }
    // Probe only terminals that say they may be kitty or ghostty: a probe on
    // any other terminal risks printed garbage or late replies read as keys.
    let hinted = env("TERM").contains("kitty")
        || env("TERM").contains("ghostty")
        || !env("KITTY_WINDOW_ID").is_empty()
        || !env("GHOSTTY_RESOURCES_DIR").is_empty()
        || env("TERM_PROGRAM").eq_ignore_ascii_case("ghostty");
    if !hinted && !force_on {
        return None;
    }
    // SAFETY: isatty on the standard descriptors.
    let ttys = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    let replies = if ttys { probe().unwrap_or_default() } else { Replies::default() };
    if let Some(cell) = replies.cell {
        set_cell_pixels(cell.0, cell.1);
    }
    let version = replies.xtversion.clone().or_else(|| {
        if env("TERM").contains("kitty") || !env("KITTY_WINDOW_ID").is_empty() {
            Some("kitty".into())
        } else if env("TERM").contains("ghostty") || env("TERM_PROGRAM").eq_ignore_ascii_case("ghostty") {
            Some("ghostty".into())
        } else {
            None
        }
    });
    if force_on {
        return Some(OuterCaps {
            version: version.unwrap_or_else(|| "kitty".into()),
        });
    }
    let version = version?;
    (replies.graphics_ok && supports_placeholders(&version)).then_some(OuterCaps { version })
}

/// Terminals known to implement kitty's Unicode placeholders.
fn supports_placeholders(version: &str) -> bool {
    let v = version.to_ascii_lowercase();
    v.contains("kitty") || v.contains("ghostty")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Replies {
    graphics_ok: bool,
    xtversion: Option<String>,
    cell: Option<(u16, u16)>,
    da1: bool,
}

const PROBE_ID: u32 = 31;

fn probe() -> Option<Replies> {
    let query = format!(
        "\x1b_Gi={PROBE_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[>q\x1b[16t\x1b[c"
    );
    let mut stdout = io::stdout();
    stdout.write_all(query.as_bytes()).ok()?;
    stdout.flush().ok()?;
    let deadline = Instant::now() + Duration::from_millis(300);
    let mut buf = Vec::new();
    loop {
        let replies = parse_replies(&buf);
        if replies.da1 {
            return Some(replies);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Some(replies);
        }
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let rc = unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as i32) };
        if rc <= 0 {
            continue;
        }
        let mut chunk = [0u8; 1024];
        // SAFETY: reading into a local buffer from stdin.
        let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            return Some(parse_replies(&buf));
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
}

/// Pick the probe's answers out of whatever arrived on stdin.
fn parse_replies(buf: &[u8]) -> Replies {
    let mut replies = Replies::default();
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] != 0x1b {
            i += 1;
            continue;
        }
        let kind = buf[i + 1];
        let body_start = i + 2;
        match kind {
            // APC / DCS, terminated by ST.
            b'_' | b'P' => {
                let Some(end) = find(&buf[body_start..], b"\x1b\\") else {
                    break;
                };
                let body = &buf[body_start..body_start + end];
                if kind == b'_' {
                    if let Some(g) = body.strip_prefix(b"G") {
                        let text = String::from_utf8_lossy(g);
                        let (control, message) = text.split_once(';').unwrap_or((&text, ""));
                        if control.split(',').any(|kv| kv == format!("i={PROBE_ID}")) && message == "OK" {
                            replies.graphics_ok = true;
                        }
                    }
                } else if let Some(v) = body.strip_prefix(b">|") {
                    let v = String::from_utf8_lossy(v).trim().to_string();
                    if !v.is_empty() {
                        replies.xtversion = Some(v);
                    }
                }
                i = body_start + end + 2;
            }
            b'[' => {
                let Some(len) = buf[body_start..].iter().position(|b| (0x40..=0x7e).contains(b)) else {
                    break;
                };
                let params = &buf[body_start..body_start + len];
                match buf[body_start + len] {
                    b'c' if params.starts_with(b"?") => replies.da1 = true,
                    b't' => {
                        let text = String::from_utf8_lossy(params);
                        let fields: Vec<&str> = text.split(';').collect();
                        if let ["6", h, w] = fields.as_slice() {
                            if let (Ok(h), Ok(w)) = (h.parse(), w.parse()) {
                                if h > 0 && w > 0 {
                                    replies.cell = Some((w, h));
                                }
                            }
                        }
                    }
                    _ => {}
                }
                i = body_start + len + 1;
            }
            _ => i += 1,
        }
    }
    replies
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitty_probe_replies_are_parsed() {
        let buf = b"\x1b_Gi=31;OK\x1b\\\x1bP>|kitty(0.39.1)\x1b\\\x1b[6;20;10t\x1b[?62;22c";
        assert_eq!(
            parse_replies(buf),
            Replies {
                graphics_ok: true,
                xtversion: Some("kitty(0.39.1)".into()),
                cell: Some((10, 20)),
                da1: true,
            }
        );
    }

    #[test]
    fn a_terminal_without_graphics_only_answers_da1() {
        let replies = parse_replies(b"\x1b[?1;2c");
        assert!(replies.da1);
        assert!(!replies.graphics_ok);
        assert_eq!(replies.xtversion, None);
    }

    #[test]
    fn graphics_errors_and_partial_replies_are_not_ok() {
        assert!(!parse_replies(b"\x1b_Gi=31;EINVAL:bad\x1b\\\x1b[?62c").graphics_ok);
        assert!(!parse_replies(b"\x1b_Gi=31;OK").graphics_ok);
    }

    #[test]
    fn only_placeholder_capable_terminals_qualify() {
        assert!(supports_placeholders("kitty(0.39.1)"));
        assert!(supports_placeholders("ghostty 1.1.3"));
        assert!(!supports_placeholders("WezTerm 20240203"));
        assert!(!supports_placeholders("konsole"));
    }
}
