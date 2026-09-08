//! Resumable conversation catalog, resolved on the session host.
use crate::workflow;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// One resumable conversation.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TranscriptCandidate {
    /// The resume key: claude's file stem / codex's thread uuid.
    pub id: String,
    /// Last modification of the transcript file.
    pub modified: SystemTime,
    /// File size in bytes — a cheap proxy for conversation length.
    pub size_bytes: u64,
    /// The first real user message, single-line, truncated. Empty when
    /// the head of the file carried nothing human-typed.
    pub preview: String,
    /// Label of the sidebar row currently bound to this transcript
    /// (filled in by the caller — the lister has no sidebar view).
    pub bound_to: Option<String>,
}

/// How many lines of a transcript's head to scan for a preview.
const PREVIEW_SCAN_LINES: usize = 80;
/// Preview width in chars.
pub const PREVIEW_CHARS: usize = 90;
/// Upper bound on candidates returned (newest first).
const MAX_CANDIDATES: usize = 60;

/// List the resumable transcripts for `engine` (`"claude"` / `"codex"`)
/// in `worktree`, newest first. Anything else (bash) has no transcripts
/// and yields an empty list.
pub fn list_transcript_candidates(worktree: &Path, engine: &str) -> Vec<TranscriptCandidate> {
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        return Vec::new();
    };
    list_transcript_candidates_in(&home, worktree, engine)
}

/// `HOME`-parameterized body of [`list_transcript_candidates`] so tests
/// can point it at a fixture tree.
pub fn list_transcript_candidates_in(
    home: &Path,
    worktree: &Path,
    engine: &str,
) -> Vec<TranscriptCandidate> {
    let mut out = match engine {
        "claude" => list_claude(home, worktree),
        "codex" => list_codex(home, worktree),
        _ => Vec::new(),
    };
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out.truncate(MAX_CANDIDATES);
    out
}

/// `~/.claude/projects/<encoded worktree>/*.jsonl` — the same encoding
/// `App::detect_session_id` / `list_jsonl_files` use.
fn list_claude(home: &Path, worktree: &Path) -> Vec<TranscriptCandidate> {
    let Some(path_str) = worktree.to_str() else {
        return Vec::new();
    };
    let encoded = path_str.replace('/', "-").replace('.', "-");
    let dir = home.join(".claude/projects").join(encoded);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        // Claude writes a zero-byte or metadata-only file for a session
        // that never got a message; nothing to resume there.
        if meta.len() == 0 {
            continue;
        }
        out.push(TranscriptCandidate {
            id: stem.to_string(),
            modified: meta.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: meta.len(),
            preview: claude_preview(&path),
            bound_to: None,
        });
    }
    out
}

/// `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` whose first-line
/// `payload.cwd` is the worktree — mirrors `App::walk_codex_sessions`,
/// plus a preview.
fn list_codex(home: &Path, worktree: &Path) -> Vec<TranscriptCandidate> {
    let Some(wt_str) = worktree.to_str() else {
        return Vec::new();
    };
    let root = home.join(".codex/sessions");
    let mut out = Vec::new();
    walk_codex(&root, wt_str, &mut out);
    out
}

fn walk_codex(dir: &Path, wt_str: &str, out: &mut Vec<TranscriptCandidate>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_codex(&path, wt_str, out);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(first) = workflow::transcript::read_first_line(&path) else {
            continue;
        };
        let Ok(head) = serde_json::from_str::<serde_json::Value>(first.trim()) else {
            continue;
        };
        if head.pointer("/payload/cwd").and_then(|v| v.as_str()) != Some(wt_str) {
            continue;
        }
        let Some(id) = head.pointer("/payload/id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        out.push(TranscriptCandidate {
            id: id.to_string(),
            modified: meta.modified().unwrap_or(UNIX_EPOCH),
            size_bytes: meta.len(),
            preview: codex_preview(&path),
            bound_to: None,
        });
    }
}

/// Read up to `PREVIEW_SCAN_LINES` lines of `path`.
fn head_lines(path: &Path) -> Vec<String> {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(file)
        .lines()
        .take(PREVIEW_SCAN_LINES)
        .map_while(Result::ok)
        .collect()
}

/// First human-typed user message in a claude transcript. Skips tool
/// results (array content with `tool_result` blocks), meta/system
/// injections, and anything that starts with a tag (`<command-name>`,
/// `<system-reminder>`, `<local-command-stdout>`…), which is how claude
/// marks non-human user turns.
fn claude_preview(path: &Path) -> String {
    for line in head_lines(path) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        if v.get("isMeta").and_then(|b| b.as_bool()) == Some(true) {
            continue;
        }
        let Some(content) = v.pointer("/message/content") else {
            continue;
        };
        let text = match content {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(blocks) => {
                let mut acc = String::new();
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            acc.push_str(t);
                            acc.push(' ');
                        }
                    }
                }
                acc
            }
            _ => continue,
        };
        if let Some(p) = as_preview(&text) {
            return p;
        }
    }
    String::new()
}

/// First human-typed user message in a codex rollout. Codex records the
/// prompt twice — as an `event_msg` / `user_message` and as a
/// `response_item` user message — and wraps environment context /
/// AGENTS.md in tagged blocks; either shape works, tagged text is
/// skipped.
fn codex_preview(path: &Path) -> String {
    for line in head_lines(path) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let text: Option<String> = match v.get("type").and_then(|t| t.as_str()) {
            Some("event_msg")
                if v.pointer("/payload/type").and_then(|t| t.as_str()) == Some("user_message") =>
            {
                v.pointer("/payload/message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            }
            Some("response_item")
                if v.pointer("/payload/role").and_then(|r| r.as_str()) == Some("user") =>
            {
                v.pointer("/payload/content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
            }
            _ => None,
        };
        if let Some(p) = text.as_deref().and_then(as_preview) {
            return p;
        }
    }
    String::new()
}

/// Collapse whitespace, reject tag-led / empty text, truncate. `None`
/// means "not a human message, keep looking".
pub fn as_preview(text: &str) -> Option<String> {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() || collapsed.starts_with('<') {
        return None;
    }
    let cleaned: String = collapsed.chars().filter(|c| !c.is_control()).collect();
    let mut out: String = cleaned.chars().take(PREVIEW_CHARS).collect();
    if cleaned.chars().count() > PREVIEW_CHARS {
        out.push('\u{2026}');
    }
    Some(out)
}
