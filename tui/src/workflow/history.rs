//! Tail `~/.claude/history.jsonl` for JSONL-rotation signals.
//!
//! Claude Code keeps this file open (fd) for the process lifetime and appends
//! one JSON record per user input — prompts, `/clear`, `/compact`, etc. Each
//! record carries the `sessionId` that was active when the input was sent
//! and the `project` cwd. The transcript `.jsonl` files (under
//! `~/.claude/projects/<encoded>/`) are opened-written-closed per append, so
//! `/proc/<pid>/fd/` cannot tell us which transcript a claude process is
//! writing to. history.jsonl is the reliable source of "who typed what in
//! which session."
//!
//! Usage: create a `HistoryWatcher` at startup (snapshots the current file
//! size), then call `poll()` on each tick to consume newly-appended entries.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub display: String,
    pub timestamp_ms: u64,
    pub project: String,
    pub session_id: String,
    /// Concatenated text from `pastedContents.<k>.content` fields. Claude
    /// moves large/multiline inputs out of `display` (which becomes a
    /// placeholder like `[Pasted text #1 +10 lines]`) and into this map.
    /// Empty string if no paste content was present.
    pub paste_content: String,
}

pub struct HistoryWatcher {
    path: PathBuf,
    offset: u64,
}

impl HistoryWatcher {
    /// Open the history file and snapshot the current size. `poll` returns
    /// only entries appended AFTER this point.
    pub fn new() -> Option<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let path = home.join(".claude/history.jsonl");
        let offset = fs::metadata(&path).ok().map(|m| m.len()).unwrap_or(0);
        Some(HistoryWatcher { path, offset })
    }

    /// Read and parse any lines appended since the last poll.
    pub fn poll(&mut self) -> Vec<HistoryEntry> {
        let Ok(mut f) = fs::File::open(&self.path) else {
            return Vec::new();
        };
        let Ok(len) = f.metadata().map(|m| m.len()) else {
            return Vec::new();
        };
        if len < self.offset {
            // File was truncated/rotated by someone — reset to end.
            self.offset = len;
            return Vec::new();
        }
        if len == self.offset {
            return Vec::new();
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return Vec::new();
        }
        self.offset = len;
        parse_entries(&buf)
    }
}

fn parse_entries(buf: &str) -> Vec<HistoryEntry> {
    let mut out = Vec::new();
    for line in buf.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let display = v
            .get("display")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let timestamp_ms = v.get("timestamp").and_then(|x| x.as_u64()).unwrap_or(0);
        let project = v
            .get("project")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let session_id = v
            .get("sessionId")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() {
            continue;
        }
        let mut paste_content = String::new();
        if let Some(map) = v.get("pastedContents").and_then(|x| x.as_object()) {
            for (_, val) in map {
                if let Some(s) = val.get("content").and_then(|x| x.as_str()) {
                    if !paste_content.is_empty() {
                        paste_content.push('\n');
                    }
                    paste_content.push_str(s);
                }
            }
        }
        out.push(HistoryEntry {
            display,
            timestamp_ms,
            project,
            session_id,
            paste_content,
        });
    }
    out
}

/// True if `display` signals a session rotation — after claude processes this
/// command, the process starts writing to a NEW `.jsonl` file.
pub fn is_rotation_trigger(display: &str) -> bool {
    let d = display.trim();
    d == "/clear" || d == "/compact" || d.starts_with("/clear ") || d.starts_with("/compact ")
}

/// Find the transcript `.jsonl` in `worktree`'s project dir whose earliest
/// recorded timestamp is at or after `after_ms`. Returns the file stem (sid).
///
/// Used after observing a `/clear` in history.jsonl to locate the new session
/// that the rotation produced. Scans first ~50 lines of each candidate looking
/// for any `"timestamp":"<iso8601>"` field.
pub fn find_post_rotation_sid(worktree: &Path, after_ms: u64) -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path_str = worktree.to_str()?;
    let encoded = path_str.replace('/', "-").replace('.', "-");
    let dir = home.join(format!(".claude/projects/{}", encoded));
    if !dir.is_dir() {
        return None;
    }
    let mut best: Option<(u64, String)> = None;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(first_ts) = earliest_timestamp_ms(&path) else {
            continue;
        };
        // Small slack (-2s) for clock jitter between history.jsonl (ms since
        // epoch) and transcript timestamps (parsed from iso8601 string).
        if first_ts + 2000 < after_ms {
            continue;
        }
        if best.as_ref().map_or(true, |(t, _)| first_ts < *t) {
            best = Some((first_ts, stem.to_string()));
        }
    }
    best.map(|(_, sid)| sid)
}

fn earliest_timestamp_ms(path: &Path) -> Option<u64> {
    let content = fs::read_to_string(path).ok()?;
    for (i, line) in content.lines().enumerate() {
        if i >= 50 {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(ts_str) = v.get("timestamp").and_then(|x| x.as_str()) {
            if let Some(ms) = iso8601_to_ms(ts_str) {
                return Some(ms);
            }
        }
    }
    None
}

/// Parse an ISO-8601 timestamp like `"2026-04-20T04:37:57.085Z"` into unix ms.
/// Very narrow parser — only the format claude emits.
fn iso8601_to_ms(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    let hour: u32 = s.get(11..13)?.parse().ok()?;
    let minute: u32 = s.get(14..16)?.parse().ok()?;
    let second: u32 = s.get(17..19)?.parse().ok()?;
    let mut ms: u32 = 0;
    if let Some(dot) = s.find('.') {
        let frac_end = s[dot + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .map(|i| dot + 1 + i)
            .unwrap_or(s.len());
        let frac = &s[dot + 1..frac_end];
        if let Ok(n) = frac.get(..3).unwrap_or(frac).parse::<u32>() {
            ms = n;
        }
    }
    // Days since unix epoch (1970-01-01). Simple calc — works for 1970+.
    let days = days_from_civil(year, month, day)?;
    let secs = (days as u64) * 86_400
        + (hour as u64) * 3600
        + (minute as u64) * 60
        + (second as u64);
    Some(secs * 1000 + ms as u64)
}

/// Howard Hinnant's date algorithm: civil (y,m,d) → days since 1970-01-01.
fn days_from_civil(y: i32, m: u32, d: u32) -> Option<i64> {
    if m < 1 || m > 12 || d < 1 || d > 31 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64;
    let doy = ((153 * (if m > 2 { m - 3 } else { m + 9 } as u32) + 2) / 5
        + d as u32
        - 1) as i64;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era as i64 * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_parses_claude_timestamp() {
        // "2026-04-20T04:37:57.085Z"
        let ms = iso8601_to_ms("2026-04-20T04:37:57.085Z").unwrap();
        // Sanity: > 2025-01-01 and < 2030-01-01.
        assert!(ms > 1_735_000_000_000);
        assert!(ms < 1_900_000_000_000);
    }

    #[test]
    fn iso8601_rejects_garbage() {
        assert!(iso8601_to_ms("not a timestamp").is_none());
        assert!(iso8601_to_ms("").is_none());
    }

    #[test]
    fn parse_entry_from_history_line() {
        let line = r#"{"display":"/clear","pastedContents":{},"timestamp":1776659853477,"project":"/tmp/foo","sessionId":"abc"}"#;
        let parsed = parse_entries(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].display, "/clear");
        assert_eq!(parsed[0].timestamp_ms, 1776659853477);
        assert_eq!(parsed[0].project, "/tmp/foo");
        assert_eq!(parsed[0].session_id, "abc");
        assert_eq!(parsed[0].paste_content, "");
    }

    #[test]
    fn parse_entry_extracts_paste_content() {
        let line = r#"{"display":"[Pasted text #1 +2 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"hello world"}},"timestamp":1,"project":"/p","sessionId":"s"}"#;
        let parsed = parse_entries(line);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].paste_content, "hello world");
    }

    #[test]
    fn skips_lines_without_session_id() {
        let line = r#"{"display":"hi","timestamp":1}"#;
        assert!(parse_entries(line).is_empty());
    }

    #[test]
    fn is_rotation_trigger_detects_clear_compact() {
        assert!(is_rotation_trigger("/clear"));
        assert!(is_rotation_trigger("/compact"));
        assert!(is_rotation_trigger("/compact some notes"));
        assert!(!is_rotation_trigger("clear this please"));
        assert!(!is_rotation_trigger("/help"));
    }
}
