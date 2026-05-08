//! Claude Code engine strategy. Reads JSONL transcripts under
//! `~/.claude/projects/<encoded-worktree>/<transcript_id>.jsonl`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::cursor::Cursor;
use super::{
    make_prompt_pending_write, resolve_cursor, Agent, AgentCtx, AgentCtxMut, Engine, Message,
    Role,
};

/// Zero-sized strategy struct.
pub struct ClaudeCodeAgent;

impl Agent for ClaudeCodeAgent {
    fn engine(&self) -> Engine {
        Engine::ClaudeCode
    }

    fn submit_prompt(&self, ctx: AgentCtxMut<'_>, text: &str) -> Result<()> {
        ctx.ts.pending_prompt = Some(make_prompt_pending_write(text));
        Ok(())
    }

    fn transcript_path(&self, ctx: AgentCtx<'_>) -> Option<PathBuf> {
        let sid = ctx.ts.transcript_id.as_deref()?;
        claude_transcript_path(ctx.worktree_path, sid)
    }

    fn read_messages(
        &self,
        ctx: AgentCtx<'_>,
        since: Option<&Cursor>,
        limit: usize,
    ) -> Result<(Vec<Message>, Cursor)> {
        let (start_offset, gen) = resolve_cursor(ctx.ts, since);

        // Missing transcript or unbound id → empty result, cursor unchanged.
        let path = match self.transcript_path(ctx) {
            Some(p) => p,
            None => return Ok((Vec::new(), Cursor::new(gen, start_offset))),
        };
        let contents = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return Ok((Vec::new(), Cursor::new(gen, start_offset))),
        };

        parse_lines(&contents, start_offset, limit, gen)
    }

    fn is_idle(&self, ctx: AgentCtx<'_>) -> bool {
        let Some(sid) = ctx.ts.transcript_id.as_deref() else {
            return false;
        };
        crate::workflow::transcript::role_turn_complete(
            &Engine::ClaudeCode,
            ctx.worktree_path,
            sid,
        )
    }

    fn count_assistant_turns(&self, ctx: AgentCtx<'_>) -> usize {
        let Some(sid) = ctx.ts.transcript_id.as_deref() else {
            return 0;
        };
        crate::workflow::transcript::count_messages(
            &Engine::ClaudeCode,
            ctx.worktree_path,
            sid,
            crate::workflow::transcript::MessageKind::Assistant,
        )
    }

    fn interrupt(&self, ctx: AgentCtxMut<'_>) {
        ctx.ts.session.write(&[0x03]);
    }
}

/// Path to a Claude JSONL for a transcript id in a given worktree.
/// Mirrors the encoding used by `workflow::transcript::claude_transcript_path`
/// (which is `pub(crate)` only inside the workflow module — duplicated here
/// to avoid coupling agent → workflow internals).
pub(super) fn claude_transcript_path(
    worktree_path: &Path,
    transcript_id: &str,
) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let path_str = worktree_path.to_str()?;
    let encoded = path_str.replace('/', "-").replace('.', "-");
    Some(
        home.join(format!(".claude/projects/{}", encoded))
            .join(format!("{}.jsonl", transcript_id)),
    )
}

/// Parse a slice of the transcript starting at `start_offset` (line index,
/// 0-based). Advances past every line — including malformed/meta/empty
/// ones — so the cursor offset is monotonic and stable across appends.
/// Returns up to `limit` parsed messages and a cursor pointing one past
/// the last line consumed.
pub(super) fn parse_lines(
    contents: &str,
    start_offset: usize,
    limit: usize,
    generation: u64,
) -> Result<(Vec<Message>, Cursor)> {
    let mut messages: Vec<Message> = Vec::new();
    let mut next_offset = start_offset;

    for (i, line) in contents.lines().enumerate().skip(start_offset) {
        // Stop BEFORE consuming this line if we already have `limit`
        // messages — so the cursor points at the first unprocessed line
        // and a subsequent call resumes here. Also handles `limit == 0`
        // correctly (returns immediately without reading any lines).
        if messages.len() >= limit {
            break;
        }
        next_offset = i + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue, // malformed line: skip, but still advance offset
        };
        if let Some(msg) = render_message(&v) {
            messages.push(msg);
        }
    }

    Ok((messages, Cursor::new(generation, next_offset)))
}

/// Convert a single Claude JSONL entry into a `Message`, or `None` if the
/// entry has no surface-able content (meta lines, slash-command records,
/// tool-result-only user lines).
fn render_message(v: &serde_json::Value) -> Option<Message> {
    let ty = v.get("type").and_then(|t| t.as_str())?;
    let role = match ty {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };

    // Skip meta records (post-/clear caveat injected by claude).
    if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
        return None;
    }

    let content_val = v.pointer("/message/content");

    // Skip user records that are pure slash-command invocations or pure
    // tool results — they aren't real user turns.
    if matches!(role, Role::User) {
        if let Some(s) = content_val.and_then(|c| c.as_str()) {
            if s.trim_start().starts_with("<command-name>") {
                return None;
            }
        }
        if is_pure_tool_result(content_val) {
            return None;
        }
    }

    let content = render_content(content_val)?;
    let ts = extract_ts(v);
    Some(Message { role, content, ts })
}

fn is_pure_tool_result(content: Option<&serde_json::Value>) -> bool {
    let Some(arr) = content.and_then(|c| c.as_array()) else {
        return false;
    };
    !arr.is_empty()
        && arr.iter().all(|item| {
            item.get("type").and_then(|t| t.as_str()) == Some("tool_result")
        })
}

/// Stringify a Claude `message.content` field into the `Message.content`
/// surface form: text concatenated with newlines; tool_use rendered as a
/// one-line summary; thinking dropped (too noisy for outside callers).
fn render_content(content: Option<&serde_json::Value>) -> Option<String> {
    let c = content?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string());
    }
    let arr = c.as_array()?;
    let mut out = String::new();
    for item in arr {
        let t = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match t {
            "text" | "output_text" => {
                if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
                    push_separated(&mut out, s);
                }
            }
            "tool_use" => {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                let summary = tool_use_summary(item);
                let line = match summary {
                    Some(s) => format!("[tool_use: {} ({})]", name, s),
                    None => format!("[tool_use: {}]", name),
                };
                push_separated(&mut out, &line);
            }
            "tool_result" => {
                let body = item
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| truncate(s, 80))
                    .unwrap_or_else(|| "<binary>".to_string());
                let line = format!("[tool_result: {}]", body);
                push_separated(&mut out, &line);
            }
            "thinking" => { /* skip */ }
            _ => { /* unknown — skip */ }
        }
    }
    if out.is_empty() {
        // Caller skips empty messages; return empty to signal "nothing
        // surface-able from this entry".
        None
    } else {
        Some(out)
    }
}

fn push_separated(buf: &mut String, s: &str) {
    if !buf.is_empty() {
        buf.push('\n');
    }
    buf.push_str(s);
}

/// Pick a representative input field for the tool-use summary. Best-effort —
/// readers just need a hint, not full fidelity.
fn tool_use_summary(item: &serde_json::Value) -> Option<String> {
    let input = item.pointer("/input")?;
    let obj = input.as_object()?;
    for k in ["command", "description", "file_path", "url", "query", "pattern"] {
        if let Some(v) = obj.get(k).and_then(|v| v.as_str()) {
            return Some(format!("{}: {}", k, truncate(v, 60)));
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let prefix: String = s.chars().take(max).collect();
        format!("{}…", prefix)
    }
}

fn extract_ts(v: &serde_json::Value) -> f64 {
    if let Some(t) = v.get("timestamp").and_then(|t| t.as_f64()) {
        return t;
    }
    // Claude entries use ISO 8601 strings; we don't pull in a date crate
    // for them. Tests assert ordering via `Message`-list order, not ts.
    0.0
}
