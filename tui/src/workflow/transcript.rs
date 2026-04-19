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

#[cfg(test)]
mod tests {
    use super::*;

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
