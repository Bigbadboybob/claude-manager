//! Extract the last assistant message from a Claude or Codex JSONL transcript.
//!
//! The workflow runner uses this to capture an outgoing role's "last message" so
//! it can be referenced in subsequent prompts via `{{ roles.X.last_message }}`.
//!
//! Best-effort: returns `None` on any parse failure. Deliberately simple — we read
//! the whole file and scan backwards. Transcripts stay small enough that this is
//! fine.

use std::fs;
use std::path::{Path, PathBuf};

use crate::workflow::toml_schema::Engine;

/// Path to the Claude JSONL for a session id in a given worktree.
fn claude_transcript_path(worktree_path: &Path, session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path_str = worktree_path.to_str()?;
    let encoded = path_str.replace('/', "-").replace('.', "-");
    Some(
        home.join(format!(".claude/projects/{}", encoded))
            .join(format!("{}.jsonl", session_id)),
    )
}

/// Walk `~/.codex/sessions/YYYY/MM/DD/*.jsonl` to find the file whose first-line
/// `payload.id` matches `session_id`.
fn codex_transcript_path(session_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let sessions = home.join(".codex/sessions");
    if !sessions.is_dir() {
        return None;
    }
    find_codex_file(&sessions, session_id)
}

fn find_codex_file(dir: &Path, session_id: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(hit) = find_codex_file(&path, session_id) {
                return Some(hit);
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            if let Ok(contents) = fs::read_to_string(&path) {
                if let Some(first) = contents.lines().next() {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(first) {
                        if v.pointer("/payload/id").and_then(|v| v.as_str()) == Some(session_id) {
                            return Some(path);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Extract the last assistant text message from a Claude JSONL transcript.
///
/// Claude lines look like:
///   {"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"..."}]}}
pub fn claude_last_message(worktree_path: &Path, session_id: &str) -> Option<String> {
    let path = claude_transcript_path(worktree_path, session_id)?;
    let contents = fs::read_to_string(&path).ok()?;
    for line in contents.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(text) = extract_text_from_content(v.pointer("/message/content")) {
            return Some(text);
        }
    }
    None
}

/// Extract the last assistant-ish message from a Codex JSONL transcript.
///
/// Codex line shapes we've seen:
///   {"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"..."}]}}
///   {"payload":{"role":"assistant","content":"..."}}   (older shape)
pub fn codex_last_message(session_id: &str) -> Option<String> {
    let path = codex_transcript_path(session_id)?;
    let contents = fs::read_to_string(&path).ok()?;
    for line in contents.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = v
            .pointer("/payload/role")
            .and_then(|r| r.as_str())
            .or_else(|| v.pointer("/role").and_then(|r| r.as_str()));
        if role != Some("assistant") {
            continue;
        }
        // Try content array shape first.
        if let Some(text) = extract_text_from_content(v.pointer("/payload/content")) {
            return Some(text);
        }
        // Fallback: string content.
        if let Some(s) = v.pointer("/payload/content").and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

/// Join all text-bearing items in a `content` array (claude-style or codex-style).
fn extract_text_from_content(content: Option<&serde_json::Value>) -> Option<String> {
    let arr = content?.as_array()?;
    let mut buf = String::new();
    for item in arr {
        let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let is_text_like = matches!(t, "text" | "output_text" | "");
        if !is_text_like {
            continue;
        }
        if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(s);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

/// Dispatch: return last assistant message for the given engine + session.
pub fn last_message(
    engine: &Engine,
    worktree_path: &Path,
    session_id: &str,
) -> Option<String> {
    match engine {
        Engine::ClaudeCode => claude_last_message(worktree_path, session_id),
        Engine::Codex => codex_last_message(session_id),
    }
}

/// Extract the first user-typed message from a Claude JSONL transcript.
///
/// We skip messages that look synthetic (tool results, system-prompt echoes, empty).
pub fn claude_first_user_message(worktree_path: &Path, session_id: &str) -> Option<String> {
    let path = claude_transcript_path(worktree_path, session_id)?;
    let contents = fs::read_to_string(&path).ok()?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        // Skip tool_result continuations — we want a real user turn.
        if is_claude_tool_result(&v) {
            continue;
        }
        if let Some(text) = extract_user_text(v.pointer("/message/content")) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn is_claude_tool_result(v: &serde_json::Value) -> bool {
    v.pointer("/message/content")
        .and_then(|c| c.as_array())
        .map_or(false, |arr| {
            arr.iter().any(|item| {
                item.get("type").and_then(|t| t.as_str()) == Some("tool_result")
            })
        })
}

/// Extract the first user message from a Codex transcript.
pub fn codex_first_user_message(session_id: &str) -> Option<String> {
    let path = codex_transcript_path(session_id)?;
    let contents = fs::read_to_string(&path).ok()?;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let role = v
            .pointer("/payload/role")
            .and_then(|r| r.as_str())
            .or_else(|| v.pointer("/role").and_then(|r| r.as_str()));
        if role != Some("user") {
            continue;
        }
        if let Some(text) = extract_user_text(v.pointer("/payload/content")) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(s) = v.pointer("/payload/content").and_then(|v| v.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// User content may be a plain string OR an array of {type, text} items.
/// We join text-bearing items, skipping tool results.
fn extract_user_text(content: Option<&serde_json::Value>) -> Option<String> {
    let c = content?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    let arr = c.as_array()?;
    let mut buf = String::new();
    for item in arr {
        let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == "tool_result" {
            continue;
        }
        if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
            if !buf.is_empty() {
                buf.push('\n');
            }
            buf.push_str(s);
        }
    }
    if buf.is_empty() { None } else { Some(buf) }
}

/// Dispatch: return the first user-typed message for the given engine + session.
pub fn first_user_message(
    engine: &Engine,
    worktree_path: &Path,
    session_id: &str,
) -> Option<String> {
    match engine {
        Engine::ClaudeCode => claude_first_user_message(worktree_path, session_id),
        Engine::Codex => codex_first_user_message(session_id),
    }
}

/// Which kind of message to extract when listing from a transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    /// Real user-typed turns only (skips tool_result continuations).
    User,
    /// Assistant text turns.
    Assistant,
}

/// Return the text of the `ExitPlanMode` plan, but ONLY if it was in the
/// most-recent assistant message of the transcript.
///
/// The intended use is "the user just accepted a plan and kicked off the
/// workflow" — in that case the last assistant line is the plan tool_use and we
/// surface it as workflow context. If the conversation has moved past the plan
/// (more assistant messages since), we return `None` rather than resurface a
/// stale plan from 10 messages back.
///
/// The plan lives at `message.content[i].input.plan` for items with
/// `type: "tool_use"` and `name: "ExitPlanMode"`. `list_messages(Assistant)`
/// intentionally skips tool_use items, so the plan would otherwise be invisible
/// to templates.
///
/// Codex has no equivalent structured plan, so `latest_plan` returns `None` for
/// Codex sessions.
pub fn latest_plan(
    engine: &Engine,
    worktree_path: &Path,
    session_id: &str,
) -> Option<String> {
    let Engine::ClaudeCode = engine else { return None };
    let path = claude_transcript_path(worktree_path, session_id)?;
    let contents = fs::read_to_string(&path).ok()?;

    // Find the LAST assistant line and check it. If it contains ExitPlanMode,
    // return the plan; otherwise return None regardless of what earlier lines
    // contained.
    let last_assistant = contents
        .lines()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .find_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                Some(v)
            } else {
                None
            }
        })?;

    let arr = last_assistant
        .pointer("/message/content")
        .and_then(|c| c.as_array())?;
    for item in arr {
        if item.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            continue;
        }
        if item.get("name").and_then(|n| n.as_str()) != Some("ExitPlanMode") {
            continue;
        }
        if let Some(plan) = item.pointer("/input/plan").and_then(|p| p.as_str()) {
            if !plan.trim().is_empty() {
                return Some(plan.to_string());
            }
        }
    }
    None
}

/// Find a Claude session id in the same worktree whose transcript has newer
/// activity than `bound_sid`'s — evidence that the session "drifted" to a new
/// id (e.g. the user ran `/clear` or `/compact`, which Claude Code handles by
/// starting a fresh session + JSONL without informing the parent PTY host).
///
/// `excluded_sids` are sids that must NOT be returned — typically the sids
/// other active sessions in the TUI are already bound to. Without this, when
/// two claude sessions share a worktree the drift detector for session A can
/// grab session B's current (actively-written-to) sid, causing both sessions
/// to appear to share history.
///
/// Returns `Some(new_sid)` when drift is detected, `None` when the bound sid is
/// still the most-recently-active one (or there's no unclaimed candidate).
/// Compares the latest-entry timestamp recorded *inside* the JSONL, not
/// filesystem mtime, because mtime can be bumped by unrelated file operations.
///
/// Codex sessions are not currently checked for drift because their storage
/// layout (`~/.codex/sessions/YYYY/MM/DD/`) makes per-worktree scanning more
/// expensive and we haven't seen drift in practice.
pub fn detect_claude_sid_drift(
    worktree_path: &Path,
    bound_sid: &str,
    excluded_sids: &[&str],
) -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path_str = worktree_path.to_str()?;
    let encoded = path_str.replace('/', "-").replace('.', "-");
    let dir = home.join(format!(".claude/projects/{}", encoded));
    if !dir.is_dir() {
        return None;
    }
    let bound_path = dir.join(format!("{}.jsonl", bound_sid));
    let bound_latest = latest_jsonl_timestamp(&bound_path).unwrap_or_default();

    let mut best: Option<(String, String)> = None; // (sid, latest_ts)
    let Ok(entries) = fs::read_dir(&dir) else { return None };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if stem == bound_sid {
            continue;
        }
        if excluded_sids.iter().any(|s| *s == stem) {
            continue;
        }
        let Some(ts) = latest_jsonl_timestamp(&path) else { continue };
        // Strictly newer than bound to count as drift.
        if ts <= bound_latest {
            continue;
        }
        match &best {
            Some((_, best_ts)) if *best_ts >= ts => {}
            _ => best = Some((stem.to_string(), ts)),
        }
    }
    best.map(|(sid, _)| sid)
}

/// Return the largest `timestamp` string found in the file, or `None`.
fn latest_jsonl_timestamp(path: &Path) -> Option<String> {
    let contents = fs::read_to_string(path).ok()?;
    let mut latest: Option<String> = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(ts) = v.get("timestamp").and_then(|t| t.as_str()) {
            match &latest {
                Some(cur) if cur.as_str() >= ts => {}
                _ => latest = Some(ts.to_string()),
            }
        }
    }
    latest
}

/// Count JSONL entries whose top-level type indicates `kind`, regardless of
/// whether their content is text-bearing.
///
/// This exists because `list_messages` intentionally skips entries that only
/// contain `thinking` or `tool_use` (they have no surfaceable text), but for
/// the idle-transition gate we want to know "did the agent take a turn?" —
/// which counts even thinking-only or tool-use-only turns.
pub fn count_messages(
    engine: &Engine,
    worktree_path: &Path,
    session_id: &str,
    kind: MessageKind,
) -> usize {
    let want = match kind {
        MessageKind::User => "user",
        MessageKind::Assistant => "assistant",
    };
    let path = match engine {
        Engine::ClaudeCode => claude_transcript_path(worktree_path, session_id),
        Engine::Codex => codex_transcript_path(session_id),
    };
    let Some(path) = path else { return 0 };
    let Ok(contents) = fs::read_to_string(&path) else {
        return 0;
    };
    let mut n = 0;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = match engine {
            Engine::ClaudeCode => v.get("type").and_then(|t| t.as_str()),
            Engine::Codex => v
                .pointer("/payload/role")
                .and_then(|r| r.as_str())
                .or_else(|| v.pointer("/role").and_then(|r| r.as_str())),
        };
        if ty == Some(want) {
            // Exclude Claude's synthetic user-tool-results — they aren't a
            // real user turn. For User kind only.
            if matches!(engine, Engine::ClaudeCode)
                && kind == MessageKind::User
                && is_claude_tool_result(&v)
            {
                continue;
            }
            n += 1;
        }
    }
    n
}

/// Return all messages of `kind` in the given transcript, in order.
pub fn list_messages(
    engine: &Engine,
    worktree_path: &Path,
    session_id: &str,
    kind: MessageKind,
) -> Vec<String> {
    let path = match engine {
        Engine::ClaudeCode => claude_transcript_path(worktree_path, session_id),
        Engine::Codex => codex_transcript_path(session_id),
    };
    let Some(path) = path else { return Vec::new() };
    let Ok(contents) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let text = match engine {
            Engine::ClaudeCode => extract_claude_line(&v, kind),
            Engine::Codex => extract_codex_line(&v, kind),
        };
        if let Some(t) = text {
            let trimmed = t.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    out
}

fn extract_claude_line(v: &serde_json::Value, kind: MessageKind) -> Option<String> {
    let want = match kind {
        MessageKind::User => "user",
        MessageKind::Assistant => "assistant",
    };
    if v.get("type").and_then(|t| t.as_str()) != Some(want) {
        return None;
    }
    if kind == MessageKind::User && is_claude_tool_result(v) {
        return None;
    }
    let content = v.pointer("/message/content");
    match kind {
        MessageKind::User => extract_user_text(content),
        MessageKind::Assistant => extract_text_from_content(content),
    }
}

fn extract_codex_line(v: &serde_json::Value, kind: MessageKind) -> Option<String> {
    let want = match kind {
        MessageKind::User => "user",
        MessageKind::Assistant => "assistant",
    };
    let role = v
        .pointer("/payload/role")
        .and_then(|r| r.as_str())
        .or_else(|| v.pointer("/role").and_then(|r| r.as_str()));
    if role != Some(want) {
        return None;
    }
    let content = v.pointer("/payload/content");
    let arr_text = match kind {
        MessageKind::User => extract_user_text(content),
        MessageKind::Assistant => extract_text_from_content(content),
    };
    if arr_text.is_some() {
        return arr_text;
    }
    // Codex sometimes has content as a raw string.
    content.and_then(|c| c.as_str()).map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes tests that mutate the process-global HOME env var. Without
    /// this they race each other when run in parallel and clobber the
    /// expected `.claude/projects/...` directory layout.
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    struct HomeOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
        old: Option<std::ffi::OsString>,
    }
    impl HomeOverride {
        fn new(tmp: tempfile::TempDir) -> Self {
            let guard = home_lock();
            let old = std::env::var_os("HOME");
            unsafe { std::env::set_var("HOME", tmp.path()); }
            HomeOverride { _guard: guard, _tmp: tmp, old }
        }
    }
    impl Drop for HomeOverride {
        fn drop(&mut self) {
            if let Some(h) = self.old.take() {
                unsafe { std::env::set_var("HOME", h); }
            } else {
                unsafe { std::env::remove_var("HOME"); }
            }
        }
    }

    fn write_transcript(tmp: &tempfile::TempDir, encoded: &str, session_id: &str, content: &str) {
        let proj_dir = tmp.path().join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(proj_dir.join(format!("{}.jsonl", session_id)), content).unwrap();
    }

    /// Verify plan extraction on a real transcript line — the same shape we
    /// confirmed by inspecting Claude Code's JSONL output.
    #[test]
    fn extract_plan_from_tool_use() {
        let tmp = tempfile::tempdir().unwrap();
        let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_x","name":"ExitPlanMode","input":{"plan":"# Plan\n\n1. step one\n2. step two","planFilePath":"/tmp/p.md"}}]}}"##;
        write_transcript(&tmp, "-tmp-repo", "sid-test", line);
        let _h = HomeOverride::new(tmp);
        let plan = latest_plan(
            &Engine::ClaudeCode,
            &std::path::PathBuf::from("/tmp/repo"),
            "sid-test",
        );
        assert!(plan.is_some(), "plan should be extracted");
        let plan = plan.unwrap();
        assert!(plan.contains("step one"), "plan was: {:?}", plan);
        assert!(plan.contains("step two"), "plan was: {:?}", plan);
    }

    #[test]
    fn extract_plan_returns_latest_when_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = vec![
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"ExitPlanMode","input":{"plan":"first plan"}}]}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"ExitPlanMode","input":{"plan":"second plan"}}]}}"##,
        ];
        write_transcript(&tmp, "-tmp-repo", "sid-test", &lines.join("\n"));
        let _h = HomeOverride::new(tmp);
        let plan = latest_plan(
            &Engine::ClaudeCode,
            &std::path::PathBuf::from("/tmp/repo"),
            "sid-test",
        );
        assert_eq!(plan.as_deref(), Some("second plan"));
    }

    /// If the last assistant message is NOT ExitPlanMode, an earlier plan
    /// doesn't get resurfaced — avoids showing a stale plan the user has
    /// already moved past.
    #[test]
    fn stale_plan_is_not_returned() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = vec![
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"ExitPlanMode","input":{"plan":"old plan"}}]}}"##,
            r##"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"great, now do something else"}]}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"started working on the new thing"}]}}"##,
        ];
        write_transcript(&tmp, "-tmp-repo", "sid-test", &lines.join("\n"));
        let _h = HomeOverride::new(tmp);
        let plan = latest_plan(
            &Engine::ClaudeCode,
            &std::path::PathBuf::from("/tmp/repo"),
            "sid-test",
        );
        assert_eq!(plan, None, "stale plan should not be returned");
    }

    #[test]
    fn codex_returns_none_for_plan() {
        let wt = std::path::PathBuf::from("/tmp/repo");
        assert!(latest_plan(&Engine::Codex, &wt, "anything").is_none());
    }

    /// Reproduces the `/clear` scenario. A claude session starts with sid
    /// `old`, user runs `/clear` so claude starts writing to a new sid
    /// `new` with newer in-file timestamps. Drift detection should return
    /// `Some("new")`.
    #[test]
    fn detects_drift_after_clear() {
        let tmp = tempfile::tempdir().unwrap();
        let old_lines = r##"{"type":"user","timestamp":"2026-04-19T20:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"type":"assistant","timestamp":"2026-04-19T20:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"##;
        let new_lines = r##"{"type":"user","timestamp":"2026-04-19T20:30:00.000Z","message":{"role":"user","content":[{"type":"text","text":"after /clear"}]}}
{"type":"assistant","timestamp":"2026-04-19T20:30:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"fresh"}]}}"##;
        write_transcript(&tmp, "-tmp-repo", "old-sid", old_lines);
        write_transcript(&tmp, "-tmp-repo", "new-sid", new_lines);
        let _h = HomeOverride::new(tmp);
        let wt = std::path::PathBuf::from("/tmp/repo");

        let drift = detect_claude_sid_drift(&wt, "old-sid", &[]);
        assert_eq!(
            drift.as_deref(),
            Some("new-sid"),
            "drift detector must find the newer sid"
        );
    }

    /// No drift when the bound sid is already the most-recent one.
    #[test]
    fn no_drift_when_bound_is_newest() {
        let tmp = tempfile::tempdir().unwrap();
        let old_lines = r##"{"type":"user","timestamp":"2026-04-19T19:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"old"}]}}"##;
        let bound_lines = r##"{"type":"user","timestamp":"2026-04-19T20:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"new"}]}}"##;
        write_transcript(&tmp, "-tmp-repo", "stale-sid", old_lines);
        write_transcript(&tmp, "-tmp-repo", "bound-sid", bound_lines);
        let _h = HomeOverride::new(tmp);
        let wt = std::path::PathBuf::from("/tmp/repo");

        let drift = detect_claude_sid_drift(&wt, "bound-sid", &[]);
        assert_eq!(drift, None, "should not migrate when bound is already newest");
    }

    /// Two sessions share a worktree ("claud" and "claude"). `"claud"`'s file
    /// is stale, `"claude"`'s is actively being written. Without the exclusion
    /// list, drift detection for `"claud"` would steal `"claude"`'s sid, which
    /// is exactly the glitch reported after a TUI restart.
    #[test]
    fn drift_excludes_other_sessions_sids() {
        let tmp = tempfile::tempdir().unwrap();
        let claud_stale = r##"{"type":"user","timestamp":"2026-04-19T19:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"old claud"}]}}"##;
        let claude_active = r##"{"type":"user","timestamp":"2026-04-19T21:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"active claude"}]}}"##;
        write_transcript(&tmp, "-tmp-repo", "claud-sid", claud_stale);
        write_transcript(&tmp, "-tmp-repo", "claude-sid", claude_active);
        let _h = HomeOverride::new(tmp);
        let wt = std::path::PathBuf::from("/tmp/repo");

        // No exclusion: drift detector happily migrates to claude-sid. BUG.
        assert_eq!(
            detect_claude_sid_drift(&wt, "claud-sid", &[]).as_deref(),
            Some("claude-sid"),
        );

        // With claude-sid excluded (because "claude" session is bound to it),
        // drift detector correctly finds no migration candidate.
        assert_eq!(
            detect_claude_sid_drift(&wt, "claud-sid", &["claude-sid"]),
            None,
            "must not poach another session's bound sid"
        );
    }

    /// Picks the newest when multiple drift candidates exist (e.g. user did
    /// `/clear` more than once or opened several claude instances).
    #[test]
    fn drift_picks_newest_among_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let bound = r##"{"type":"user","timestamp":"2026-04-19T18:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"bound"}]}}"##;
        let cand_a = r##"{"type":"user","timestamp":"2026-04-19T19:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"a"}]}}"##;
        let cand_b = r##"{"type":"user","timestamp":"2026-04-19T21:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"b"}]}}"##;
        write_transcript(&tmp, "-tmp-repo", "bound", bound);
        write_transcript(&tmp, "-tmp-repo", "cand-a", cand_a);
        write_transcript(&tmp, "-tmp-repo", "cand-b", cand_b);
        let _h = HomeOverride::new(tmp);
        let wt = std::path::PathBuf::from("/tmp/repo");

        let drift = detect_claude_sid_drift(&wt, "bound", &[]);
        assert_eq!(drift.as_deref(), Some("cand-b"));
    }

    /// Reproduces the production failure. Transcript has TWO assistant entries:
    /// one thinking-only, one text-only. `list_messages` (text-extraction)
    /// returns 1 — that's what the old gate used and why it was off-by-one.
    /// `count_messages` returns 2 — the correct turn count for the gate.
    #[test]
    fn gate_undercounts_when_thinking_only() {
        let tmp = tempfile::tempdir().unwrap();
        let lines = [
            // Thinking-only assistant turn (real shape from Claude JSONL).
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"planning..."}]}}"##,
            // Text-only assistant turn.
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"here you go"}]}}"##,
        ];
        write_transcript(&tmp, "-tmp-repo", "sid-x", &lines.join("\n"));
        let _h = HomeOverride::new(tmp);
        let wt = std::path::PathBuf::from("/tmp/repo");

        let text_count = list_messages(
            &Engine::ClaudeCode,
            &wt,
            "sid-x",
            MessageKind::Assistant,
        )
        .len();
        let turn_count = count_messages(&Engine::ClaudeCode, &wt, "sid-x", MessageKind::Assistant);

        assert_eq!(text_count, 1, "list_messages undercounts by design");
        assert_eq!(turn_count, 2, "count_messages counts real turns");
    }

    /// End-to-end simulation: a worker session with 1 pre-launch assistant
    /// turn. After launch (baseline=1), appending another assistant turn
    /// should trip the gate (current=2 > baseline=1). Proves the path the
    /// TUI uses actually fires.
    #[test]
    fn gate_fires_after_new_assistant_turn() {
        let tmp = tempfile::tempdir().unwrap();
        let pre = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"pre-launch response"}]}}"##;
        write_transcript(&tmp, "-tmp-repo", "sid-x", pre);
        let _h = HomeOverride::new(tmp);
        let wt = std::path::PathBuf::from("/tmp/repo");

        // At launch: baseline = current count.
        let baseline =
            count_messages(&Engine::ClaudeCode, &wt, "sid-x", MessageKind::Assistant);
        assert_eq!(baseline, 1);

        // Worker stops; no new turns yet.
        let current =
            count_messages(&Engine::ClaudeCode, &wt, "sid-x", MessageKind::Assistant);
        assert!(
            !(current > baseline),
            "gate must not fire when nothing changed (current={}, baseline={})",
            current,
            baseline
        );

        // Worker produces a new turn — even if it's thinking-only.
        let new_turn = "\n".to_string()
            + r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."}]}}"##;
        let proj_dir = std::env::var_os("HOME")
            .map(|h| {
                std::path::PathBuf::from(h).join(".claude/projects/-tmp-repo")
            })
            .unwrap();
        let f_path = proj_dir.join("sid-x.jsonl");
        let mut prev = std::fs::read_to_string(&f_path).unwrap();
        prev.push_str(&new_turn);
        std::fs::write(&f_path, prev).unwrap();

        let current2 =
            count_messages(&Engine::ClaudeCode, &wt, "sid-x", MessageKind::Assistant);
        assert_eq!(current2, 2, "new thinking-only turn must be counted");
        assert!(
            current2 > baseline,
            "gate should fire: current={} > baseline={}",
            current2,
            baseline
        );
    }

    #[test]
    fn extract_text_claude_shape() {
        let v: serde_json::Value = serde_json::json!([
            {"type": "text", "text": "hello"},
            {"type": "text", "text": "world"}
        ]);
        assert_eq!(extract_text_from_content(Some(&v)), Some("hello\nworld".into()));
    }

    #[test]
    fn extract_text_codex_shape() {
        let v: serde_json::Value = serde_json::json!([
            {"type": "output_text", "text": "hi from codex"}
        ]);
        assert_eq!(extract_text_from_content(Some(&v)), Some("hi from codex".into()));
    }

    #[test]
    fn skips_non_text_items() {
        let v: serde_json::Value = serde_json::json!([
            {"type": "tool_use", "name": "bash"},
            {"type": "text", "text": "real text"}
        ]);
        assert_eq!(extract_text_from_content(Some(&v)), Some("real text".into()));
    }
}
