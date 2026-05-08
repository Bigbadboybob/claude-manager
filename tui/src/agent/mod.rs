//! Engine-strategy abstraction over Claude Code and Codex agent sessions.
//!
//! `TerminalSession` (in `app`) owns identity (`uid`), transcript binding
//! (`transcript_id`), generation, and PTY state. The `Agent` trait is a
//! zero-sized strategy: per-engine logic for encoding a prompt, parsing
//! the transcript, and idle detection. Workflow gating, MCP tools, and
//! any future caller talk to sessions via this trait so engine differences
//! are localized to the impl modules.
//!
//! Closing a session is intentionally NOT a trait method — that mutates
//! workspace-level state (tombstones live on `Workspace`, not on the
//! session) and lives at the app layer.

mod claude_code;
mod codex;
mod cursor;

#[cfg(test)]
mod tests;

pub use claude_code::ClaudeCodeAgent;
pub use codex::CodexAgent;
pub use cursor::Cursor;
#[allow(unused_imports)]
pub use cursor::CursorParseError;

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::app::{PendingWrite, TerminalSession};

// Re-export Engine from workflow::toml_schema. The enum stays there because
// workflow TOML deserialization keys off it; the agent module is just a user.
pub use crate::workflow::toml_schema::Engine;

/// Role of a transcript message after engine-specific normalization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

/// One normalized message extracted from a transcript file.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    /// Text content. Tool calls render as a one-line summary like
    /// `[tool_use: BashTool (command: ls -la)]`.
    pub content: String,
    /// Unix timestamp seconds, or 0.0 if the line had no parseable timestamp.
    pub ts: f64,
}

/// Bag of refs the engine needs to operate on a session. Identity and
/// transcript binding live on `TerminalSession`; `worktree_path` lives on
/// `Workspace` (sessions have no worktree field of their own). Claude
/// Code's transcript path needs both, so the trait takes them packaged.
#[derive(Clone, Copy)]
pub struct AgentCtx<'a> {
    pub ts: &'a TerminalSession,
    pub worktree_path: &'a Path,
}

/// Mutable variant for `submit_prompt` and `interrupt`. Not Copy — `&mut`
/// can't be Copy. Methods that take it consume it once; no reuse needed.
pub struct AgentCtxMut<'a> {
    pub ts: &'a mut TerminalSession,
    pub worktree_path: &'a Path,
}

/// Engine strategy for an agent session. Implementations are zero-sized
/// strategy structs (no per-session state — that lives on `TerminalSession`).
pub trait Agent: Send + Sync {
    fn engine(&self) -> Engine;

    /// Queue a prompt for delivery. Returns immediately. Actual delivery
    /// is deferred to the TUI main loop's drain pass, which fires when the
    /// PTY first goes quiet (`PendingWrite::wait_for_quiet`) and schedules
    /// a separate Enter keystroke 10s later — preventing the body+Enter
    /// from being seen as a single paste, which Codex would treat as
    /// literal text.
    fn submit_prompt(&self, ctx: AgentCtxMut<'_>, text: &str) -> anyhow::Result<()>;

    /// Cursor-based read of the transcript. Cursors include the generation
    /// they were issued under; on mismatch (`/clear` rebound the file),
    /// the parser silently restarts at offset 0 of the current transcript
    /// and the returned cursor carries the new generation.
    fn read_messages(
        &self,
        ctx: AgentCtx<'_>,
        since: Option<&Cursor>,
        limit: usize,
    ) -> anyhow::Result<(Vec<Message>, Cursor)>;

    /// Resolve the on-disk transcript path. Returns `None` when the
    /// session is freshly spawned (or just-cleared) and the transcript ID
    /// hasn't been detected yet.
    fn transcript_path(&self, ctx: AgentCtx<'_>) -> Option<PathBuf>;

    /// Idle = the most recent assistant turn is complete (no pending
    /// tool_use / mid-stream tokens). Used as the turn-complete predicate
    /// inside `assistant_turn_completed_since`. Raw `is_idle` alone is
    /// NOT a valid workflow gate — use the helper below.
    fn is_idle(&self, ctx: AgentCtx<'_>) -> bool;

    /// Count of assistant turns visible in the current transcript. A
    /// "turn" is one top-level assistant entry, regardless of whether
    /// its content is text, thinking, or tool_use only.
    fn count_assistant_turns(&self, ctx: AgentCtx<'_>) -> usize;

    /// Workflow gate: true iff the role has produced a complete assistant
    /// turn at or after `baseline`. The default impl combines
    /// `count_assistant_turns(ctx) > baseline` with `is_idle(ctx)`. Raw
    /// `is_idle` alone would fire on stale output before the freshly
    /// delivered prompt produced a response.
    fn assistant_turn_completed_since(
        &self,
        ctx: AgentCtx<'_>,
        baseline: usize,
    ) -> bool {
        self.count_assistant_turns(ctx) > baseline && self.is_idle(ctx)
    }

    /// Send 0x03 (ctrl-C) directly to the PTY. Synchronous, not queued.
    fn interrupt(&self, ctx: AgentCtxMut<'_>);
}

/// Pick the right `Agent` for a session's `session_type` string ("claude"
/// or "codex"). Anything else (notably "bash") returns the Claude impl;
/// callers must not invoke trait methods on bash sessions — they have no
/// transcript to parse.
pub fn agent_for(session_type: &str) -> &'static dyn Agent {
    match session_type {
        "codex" => &CodexAgent,
        _ => &ClaudeCodeAgent,
    }
}

/// Convenience: pick by `Engine` enum value.
pub fn agent_for_engine(engine: &Engine) -> &'static dyn Agent {
    match engine {
        Engine::ClaudeCode => &ClaudeCodeAgent,
        Engine::Codex => &CodexAgent,
    }
}

/// Build the `PendingWrite` used by `submit_prompt`. Shared between engines:
/// the body/Enter separation and quiet-window timing don't differ at queue
/// time — engine-specific Enter encoding (kitty keyboard for Codex) is
/// chosen by the drainer at fire time based on the live terminal mode.
pub(crate) fn make_prompt_pending_write(text: &str) -> PendingWrite {
    PendingWrite::wait_for_quiet(
        text.trim_end().to_string(),
        true,
        Duration::from_secs(1),
        Duration::from_secs(2),
        Duration::from_secs(180),
    )
}

/// Resolve `since` against the session's current generation. Returns
/// `(effective_line_offset, current_generation)`. On generation mismatch
/// (or malformed cursor), silently restart at offset 0 of the current
/// transcript.
pub(crate) fn resolve_cursor(
    ts: &TerminalSession,
    since: Option<&Cursor>,
) -> (usize, u64) {
    resolve_cursor_for_gen(ts.generation, since)
}

/// Pure version of `resolve_cursor` that doesn't need a `TerminalSession`.
/// Exposed for tests; impls funnel through `resolve_cursor`.
pub(crate) fn resolve_cursor_for_gen(
    cur_gen: u64,
    since: Option<&Cursor>,
) -> (usize, u64) {
    match since {
        None => (0, cur_gen),
        Some(c) => match c.parse() {
            Ok((g, off)) if g == cur_gen => (off, cur_gen),
            _ => (0, cur_gen),
        },
    }
}
