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
