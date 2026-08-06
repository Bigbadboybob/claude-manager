//! Transcript-tail probe — the ground-truth read behind the scheduler's
//! auth-expiry + consumer-wedge detection (`auth_wedge_pass`).
//!
//! ## Why a tail probe and not the Stop hook
//!
//! The 2026-08-03 incident (cm-manager's `~/.claude/.credentials.json`
//! truncated → every session logged out) proved the cm Stop hook is NOT a
//! usable signal for auth failure: an auth-error turn writes a synthetic
//! assistant record + a `turn_duration` system record and **never runs Stop
//! hooks** (verified against the wedged orchestrator's transcript — healthy
//! turns log a `stop_hook_summary`, auth turns don't, same CLI version). So
//! `session.turn_ended` never arrives and any hook-anchored detector is blind
//! exactly when it matters. The daemon reads the transcript file itself.
//!
//! ## What the tail says
//!
//! A Claude Code transcript is JSONL, one record per line. The shapes this
//! probe distinguishes (from the incident evidence):
//!
//!   - **Auth failure** — a synthetic assistant record:
//!     `{"type":"assistant","error":"authentication_failed",
//!       "isApiErrorMessage":true,"message":{"model":"<synthetic>","content":
//!       [{"type":"text","text":"Login expired · Please run /login"}]}}`
//!     followed by `{"type":"system","subtype":"turn_duration"}`.
//!   - **Completed turn** — the file ends (modulo bookkeeping records) with
//!     `{"type":"system","subtype":"turn_duration"}`.
//!   - **Delivered-but-unanswered prompt** — the last substantive record is a
//!     `user` record: a prompt was pasted and no assistant response ever
//!     started (the engine is dead in a way that didn't even record a 401).
//!   - **Mid-turn** — the last substantive record is a healthy `assistant`
//!     record (streaming, or a `tool_use` awaiting its result — possibly a
//!     legitimately long blocking tool call). NOT evidence of completion.
//!
//! Bookkeeping records (`file-history-snapshot`, `last-prompt`, `summary`,
//! `attachment`, and non-`turn_duration` system records like
//! `stop_hook_summary`) are skipped while scanning backwards.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the transcript's tail to read. A single record can be large
/// (tool results embed whole command outputs), but the *last few* records are
/// what we classify; 256 KiB comfortably covers them while bounding the read.
const TAIL_READ_BYTES: u64 = 256 * 1024;

/// Cap on the number of tail lines scanned backwards before giving up.
/// Prevents a pathological transcript (e.g. thousands of tiny bookkeeping
/// records) from turning the probe into a full-file parse.
const MAX_SCAN_LINES: usize = 200;

/// Shape of the transcript's last substantive record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailShape {
    /// The last turn COMPLETED (`system/turn_duration` is the newest
    /// substantive record). Combined with a stale mtime + a still-`Running`
    /// run, this is the consumer-wedge signal: the agent finished talking and
    /// nothing will ever `report_done`.
    TurnComplete,
    /// A `user` record is newest: a prompt was delivered and no response ever
    /// started. Same wedge treatment as [`TailShape::TurnComplete`] once
    /// stale — it is the "engine dead without even a 401 record" shape.
    AwaitingResponse,
    /// A healthy `assistant` record is newest — the turn is (plausibly) still
    /// in flight, e.g. a long blocking tool call. Never treated as wedged.
    MidTurn,
}

/// Probe result: the tail shape plus the auth-error text when the newest
/// assistant record is a synthetic `authentication_failed` message.
#[derive(Debug, Clone)]
pub struct TailProbe {
    pub shape: TailShape,
    /// `Some(banner text)` when the newest assistant record carries
    /// `error: "authentication_failed"` — e.g. `"Login expired · Please run
    /// /login"` or `"Please run /login · API Error: 401 OAuth access token
    /// has been revoked."`. Auth-error turns DO end (a `turn_duration`
    /// follows), so this composes with `shape == TurnComplete`.
    pub auth_error: Option<String>,
}

/// Classify the transcript's tail. `None` when the file can't be read, is
/// empty, or no substantive record appears within the scan bounds — callers
/// treat that as "can't judge" and skip (conservative, same contract as the
/// stall pass's unreadable-mtime skip).
pub fn probe_transcript_tail(path: &Path) -> Option<TailProbe> {
    let text = read_tail(path)?;
    // The first (possibly truncated by the tail seek) line is dropped unless
    // the read started at offset 0.
    let mut shape: Option<TailShape> = None;
    let mut auth_error: Option<String> = None;
    for line in text.lines().rev().take(MAX_SCAN_LINES) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            // A truncated first-line-of-tail or a corrupt record — skip.
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("system") => {
                if v.get("subtype").and_then(|s| s.as_str()) == Some("turn_duration")
                    && shape.is_none()
                {
                    shape = Some(TailShape::TurnComplete);
                    // Keep scanning: the auth verdict needs the nearest
                    // assistant record (it precedes the turn_duration).
                }
            }
            Some("assistant") => {
                if v.get("error").and_then(|e| e.as_str()) == Some("authentication_failed") {
                    auth_error = Some(assistant_text(&v));
                }
                if shape.is_none() {
                    shape = Some(TailShape::MidTurn);
                }
                break; // newest assistant reached — both facts are settled
            }
            Some("user") => {
                if shape.is_none() {
                    shape = Some(TailShape::AwaitingResponse);
                }
                break; // an older assistant record can't be "the newest"
            }
            // file-history-snapshot / last-prompt / summary / attachment /
            // unknown — bookkeeping, keep scanning.
            _ => {}
        }
    }
    shape.map(|shape| TailProbe { shape, auth_error })
}

/// First text block of an assistant record's message content, for the alert
/// detail. Falls back to the bare error tag when the content shape surprises.
fn assistant_text(v: &serde_json::Value) -> String {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                (b.get("type").and_then(|t| t.as_str()) == Some("text"))
                    .then(|| b.get("text").and_then(|t| t.as_str()))
                    .flatten()
            })
        })
        .unwrap_or("authentication_failed")
        .to_string()
}

/// Read the last [`TAIL_READ_BYTES`] of the file as (lossy) UTF-8. When the
/// read starts mid-file, the first line is likely truncated — the parse loop
/// tolerates it (`from_str` fails → skipped).
fn read_tail(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len == 0 {
        return None;
    }
    let start = len.saturating_sub(TAIL_READ_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    f.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Structural validity check for `~/.claude/.credentials.json` — the
/// scheduler's credentials preflight (the 2026-08-03 incident began with this
/// file truncated to 243 bytes, which made every subsequent fire dead before
/// any transcript said so). Returns `Some(reason)` when the file EXISTS but is
/// structurally broken:
///
///   - empty, or unparseable JSON (the truncation shape), or
///   - parses but its `claudeAiOauth` object has a missing/empty
///     `accessToken`.
///
/// A MISSING file is `None` (healthy): keychain-managed and API-key setups
/// legitimately have no file. A valid-JSON file with no `claudeAiOauth` key at
/// all is also `None` — an unknown future format is not evidence of breakage.
pub fn credentials_file_problem(path: &Path) -> Option<String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        // Exists but unreadable (perms?) — can't judge; don't alert.
        Err(_) => return None,
    };
    if bytes.is_empty() {
        return Some("file is empty".to_string());
    }
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return Some(format!(
                "not valid JSON ({} bytes — truncated?)",
                bytes.len()
            ));
        }
    };
    let Some(oauth) = v.get("claudeAiOauth") else {
        return None; // unknown-but-valid format: not our call
    };
    let token_ok = oauth
        .get("accessToken")
        .and_then(|t| t.as_str())
        .is_some_and(|t| !t.trim().is_empty());
    if token_ok {
        None
    } else {
        Some("claudeAiOauth.accessToken is missing or empty".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_lines(dir: &tempfile::TempDir, lines: &[&str]) -> std::path::PathBuf {
        let path = dir.path().join("transcript.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    // Real shapes from the 2026-08-03 incident transcript
    // (a31e9025-b4ec-4548-af1f-a49bd30edf82.jsonl), trimmed to the fields the
    // probe reads.
    const AUTH_ERROR_LINE: &str = r#"{"type":"assistant","error":"authentication_failed","isApiErrorMessage":true,"apiErrorStatus":401,"message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"Login expired · Please run /login"}]}}"#;
    const TURN_DURATION_LINE: &str =
        r#"{"type":"system","subtype":"turn_duration","durationMs":157114}"#;
    const STOP_HOOK_LINE: &str =
        r#"{"type":"system","subtype":"stop_hook_summary","hookCount":1}"#;
    const SNAPSHOT_LINE: &str = r#"{"type":"file-history-snapshot"}"#;
    const USER_LINE: &str = r#"{"type":"user","message":{"role":"user","content":"go"}}"#;
    const ASSISTANT_TEXT_LINE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done."}],"stop_reason":"end_turn"}}"#;
    const ASSISTANT_TOOL_LINE: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#;

    /// The incident's exact wedge shape: user prompt → synthetic 401 assistant
    /// → turn_duration → snapshot. Turn complete AND auth error.
    #[test]
    fn auth_error_turn_is_complete_with_auth_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[USER_LINE, AUTH_ERROR_LINE, TURN_DURATION_LINE, SNAPSHOT_LINE],
        );
        let p = probe_transcript_tail(&path).expect("probe");
        assert_eq!(p.shape, TailShape::TurnComplete);
        assert_eq!(
            p.auth_error.as_deref(),
            Some("Login expired · Please run /login"),
        );
    }

    /// A healthy completed turn: assistant text → stop hook → turn_duration.
    /// Complete, NO auth error.
    #[test]
    fn healthy_completed_turn_has_no_auth_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[USER_LINE, ASSISTANT_TEXT_LINE, STOP_HOOK_LINE, TURN_DURATION_LINE],
        );
        let p = probe_transcript_tail(&path).expect("probe");
        assert_eq!(p.shape, TailShape::TurnComplete);
        assert!(p.auth_error.is_none());
    }

    /// A trailing tool_use assistant record = mid-turn (long tool call), never
    /// a wedge candidate.
    #[test]
    fn trailing_tool_use_is_mid_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(&dir, &[USER_LINE, ASSISTANT_TOOL_LINE]);
        let p = probe_transcript_tail(&path).expect("probe");
        assert_eq!(p.shape, TailShape::MidTurn);
        assert!(p.auth_error.is_none());
    }

    /// A trailing user record = delivered prompt with no response.
    #[test]
    fn trailing_user_record_is_awaiting_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[ASSISTANT_TEXT_LINE, TURN_DURATION_LINE, USER_LINE, SNAPSHOT_LINE],
        );
        let p = probe_transcript_tail(&path).expect("probe");
        assert_eq!(p.shape, TailShape::AwaitingResponse);
    }

    /// An OLDER auth error superseded by a healthy completed turn does NOT
    /// re-flag: the scan stops at the newest assistant record.
    #[test]
    fn healthy_turn_after_old_auth_error_clears_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_lines(
            &dir,
            &[
                AUTH_ERROR_LINE,
                TURN_DURATION_LINE,
                USER_LINE,
                ASSISTANT_TEXT_LINE,
                TURN_DURATION_LINE,
            ],
        );
        let p = probe_transcript_tail(&path).expect("probe");
        assert_eq!(p.shape, TailShape::TurnComplete);
        assert!(p.auth_error.is_none(), "old 401 must not re-alert");
    }

    /// Unreadable / empty / no-substantive-records files are "can't judge".
    #[test]
    fn unreadable_or_empty_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(probe_transcript_tail(&dir.path().join("missing.jsonl")).is_none());
        let empty = write_lines(&dir, &[]);
        assert!(probe_transcript_tail(&empty).is_none());
        let noise = write_lines(&dir, &[SNAPSHOT_LINE, "not json at all"]);
        assert!(probe_transcript_tail(&noise).is_none());
    }

    // ----- credentials preflight -----

    #[test]
    fn credentials_missing_file_is_healthy() {
        let dir = tempfile::tempdir().unwrap();
        assert!(credentials_file_problem(&dir.path().join("nope.json")).is_none());
    }

    #[test]
    fn credentials_truncated_json_is_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        // The incident shape: a valid file cut off mid-way (243 of ~500 bytes).
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat0"#).unwrap();
        let problem = credentials_file_problem(&path).expect("flagged");
        assert!(problem.contains("not valid JSON"), "{}", problem);
    }

    #[test]
    fn credentials_empty_token_is_flagged_valid_token_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, r#"{"claudeAiOauth":{"accessToken":""}}"#).unwrap();
        assert!(credentials_file_problem(&path).is_some());
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abc","expiresAt":1}}"#,
        )
        .unwrap();
        assert!(credentials_file_problem(&path).is_none());
    }

    /// Valid JSON without a `claudeAiOauth` key (unknown future format) is
    /// not flagged.
    #[test]
    fn credentials_unknown_format_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(&path, r#"{"someFutureShape":true}"#).unwrap();
        assert!(credentials_file_problem(&path).is_none());
    }
}
