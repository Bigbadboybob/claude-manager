//! Codex engine strategy. Reads JSONL transcripts under
//! `~/.codex/sessions/YYYY/MM/DD/<id>.jsonl`. Codex's transcript path is
//! per-user-and-date, not per-worktree, so `transcript_path` ignores
//! `ctx.worktree_path` here.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::cursor::Cursor;
use super::{
    make_prompt_pending_write, resolve_cursor, Agent, AgentCtx, AgentCtxMut, Engine, Message,
    Role,
};

/// Zero-sized strategy struct.
pub struct CodexAgent;

impl Agent for CodexAgent {
    fn engine(&self) -> Engine {
        Engine::Codex
    }

    fn submit_prompt(&self, ctx: AgentCtxMut<'_>, text: &str) -> Result<()> {
        ctx.ts.pending_prompt = Some(make_prompt_pending_write(text));
        Ok(())
    }

    fn transcript_path(&self, ctx: AgentCtx<'_>) -> Option<PathBuf> {
        let sid = ctx.ts.transcript_id.as_deref()?;
        codex_transcript_path(sid)
    }

    fn read_messages(
        &self,
        ctx: AgentCtx<'_>,
        since: Option<&Cursor>,
        limit: usize,
    ) -> Result<(Vec<Message>, Cursor)> {
        let (start_offset, gen) = resolve_cursor(ctx.ts, since);

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
            &Engine::Codex,
            ctx.worktree_path,
            sid,
        )
    }

    fn count_assistant_turns(&self, ctx: AgentCtx<'_>) -> usize {
        let Some(sid) = ctx.ts.transcript_id.as_deref() else {
            return 0;
        };
        crate::workflow::transcript::count_messages(
            &Engine::Codex,
            ctx.worktree_path,
            sid,
            crate::workflow::transcript::MessageKind::Assistant,
        )
    }

    fn interrupt(&self, ctx: AgentCtxMut<'_>) {
        ctx.ts.session.write(&[0x03]);
    }
}

/// Walk `~/.codex/sessions/YYYY/MM/DD/*.jsonl` for the file whose
/// first-line `payload.id` matches `transcript_id`.
pub(super) fn codex_transcript_path(transcript_id: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let sessions = home.join(".codex/sessions");
    if !sessions.is_dir() {
        return None;
    }
    find_codex_file(&sessions, transcript_id)
}

fn find_codex_file(dir: &Path, transcript_id: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(hit) = find_codex_file(&path, transcript_id) {
                return Some(hit);
            }
        } else if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
            let Some(first) = read_first_line(&path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(first.trim()) else {
                continue;
            };
            if v.pointer("/payload/id").and_then(|v| v.as_str()) == Some(transcript_id) {
                return Some(path);
            }
        }
    }
    None
}

fn read_first_line(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = String::new();
    reader.read_line(&mut buf).ok()?;
    Some(buf)
}

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
            Err(_) => continue,
        };
        if let Some(msg) = render_message(&v) {
            messages.push(msg);
        }
    }

    Ok((messages, Cursor::new(generation, next_offset)))
}

/// Convert a single Codex JSONL entry into a `Message`. Codex shapes:
/// `response_item` carries the actual user/assistant turns; `event_msg`
/// frames task lifecycle events (`task_complete`, `agent_message`); some
/// older shapes use top-level `role`/`content`. We surface user and
/// assistant content; tool-call envelopes show as one-line summaries.
fn render_message(v: &serde_json::Value) -> Option<Message> {
    // Top-level `event_msg` frames don't carry user/assistant content
    // we want to surface (lifecycle metadata), with one exception:
    // `agent_message` events can carry assistant text.
    let top_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let payload_type = v
        .pointer("/payload/type")
        .and_then(|t| t.as_str())
        .unwrap_or("");

    // function_call / function_call_output / reasoning render as tool-ish.
    if top_type == "response_item"
        && (payload_type == "function_call" || payload_type == "function_call_output")
    {
        let name = v
            .pointer("/payload/name")
            .and_then(|n| n.as_str())
            .unwrap_or("?");
        let line = format!("[tool_use: {}]", name);
        return Some(Message {
            role: Role::Tool,
            content: line,
            ts: extract_ts(v),
        });
    }

    // `event_msg`/`agent_message` is **intentionally skipped**.
    // Empirically (verified across 20+ real Codex sessions) every
    // `agent_message` is mirrored 1:1 by a later `response_item` with
    // role=="assistant" carrying the same text — `agent_message` is a
    // streaming UI sidecar, not an independent turn. Surfacing both
    // would double-count assistant turns (firing the `on_idle` gate
    // one turn early) and emit duplicate Messages to readers. The
    // `response_item` branch below is the canonical assistant record.
    //
    // Ordering near turn end is deterministic:
    //   agent_message → response_item assistant → token_count → task_complete
    // so by the time `is_idle` returns true (gated on `task_complete`),
    // the canonical `response_item` is already on disk — no race.
    if top_type == "event_msg" && payload_type == "agent_message" {
        return None;
    }

    let role_str = v
        .pointer("/payload/role")
        .and_then(|r| r.as_str())
        .or_else(|| v.pointer("/role").and_then(|r| r.as_str()));

    let role = match role_str {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        _ => return None,
    };

    let content_val = v
        .pointer("/payload/content")
        .or_else(|| v.pointer("/content"));
    let content = render_content(content_val)?;
    Some(Message {
        role,
        content,
        ts: extract_ts(v),
    })
}

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
            "text" | "output_text" | "input_text" => {
                if let Some(s) = item.get("text").and_then(|v| v.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(s);
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn extract_ts(v: &serde_json::Value) -> f64 {
    v.get("timestamp")
        .and_then(|t| t.as_f64())
        .unwrap_or(0.0)
}
