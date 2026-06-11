//! Extract the last assistant message from a Claude or Codex JSONL transcript.
//!
//! The workflow runner uses this to capture an outgoing role's "last message" so
//! it can be referenced in subsequent prompts via `{{ roles.X.last_message }}`.
//!
//! Best-effort: returns `None` on any parse failure.
//!
//! The hot-path helpers (`count_messages`, `role_turn_complete`) consult a
//! process-global byte-cursor cache so each tick only parses the bytes appended
//! since the previous call. See the [`cache`] submodule.

use std::fs;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use crate::workflow::toml_schema::Engine;

mod cache;

/// Read just the first line of a file. Used to inspect Codex JSONL session
/// metadata (which lives on the first line) without slurping the whole
/// transcript — these files routinely grow to many MB and the codex session
/// directory accumulates hundreds of them, so the per-file read+parse cost
/// matters when walking the tree to find or list sessions.
pub fn read_first_line(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = String::new();
    reader.read_line(&mut buf).ok()?;
    Some(buf)
}

/// Path to the Claude JSONL for a session id in a given worktree.
pub(crate) fn claude_transcript_path(worktree_path: &Path, session_id: &str) -> Option<PathBuf> {
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
            let Some(first) = read_first_line(&path) else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(first.trim()) else { continue };
            if v.pointer("/payload/id").and_then(|v| v.as_str()) == Some(session_id) {
                return Some(path);
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
///
/// `event_msg`/`agent_message` records are intentionally **skipped**: they
/// mirror the canonical `response_item` (verified 1:1 across real Codex
/// sessions), and surfacing both would expose duplicate turns to workflow
/// templates and the manager→worker handoff.
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

fn is_claude_tool_result(v: &serde_json::Value) -> bool {
    v.pointer("/message/content")
        .and_then(|c| c.as_array())
        .map_or(false, |arr| {
            arr.iter().any(|item| {
                item.get("type").and_then(|t| t.as_str()) == Some("tool_result")
            })
        })
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

/// True if the agent has fully finished its most recent turn — not merely
/// paused between steps. Used by the idle-transition gate to avoid firing
/// mid-turn (long tool calls, thinking pauses) which would otherwise put the
/// next role into action while the outgoing role is still producing output.
///
/// - Claude: the most recent assistant JSONL record has
///   `message.stop_reason == "end_turn"`. `tool_use`, `null`, or anything
///   else means the agent still has work to do.
/// - Codex: the LAST non-token-count event in the rollout is
///   `event_msg` with `payload.type == "task_complete"`. If a
///   `response_item` of type `function_call`, `reasoning`, or `message`
///   follows the most recent `task_complete`, the agent started another
///   step and isn't done yet.
///
/// Returns `false` (conservative — don't fire gate) on any parse failure
/// or missing signal.
pub fn role_turn_complete(
    engine: &Engine,
    worktree_path: &Path,
    session_id: &str,
) -> bool {
    match engine {
        Engine::ClaudeCode => claude_turn_complete(worktree_path, session_id),
        Engine::Codex => codex_turn_complete(session_id),
    }
}

/// 10d-2c-2-1: combined gate predicate. True iff the role has
/// produced a complete assistant turn at or after `baseline`.
/// Mirrors the TUI [`crate::agent::Agent::assistant_turn_completed_since`]
/// default impl — same predicate, just exposed daemon-side so
/// the upcoming on_idle driver (2c-2-2) can run the gate without
/// going through the TUI's Agent trait (the TUI's
/// `Agent::is_idle` already delegates to [`role_turn_complete`],
/// and `Agent::count_assistant_turns` already delegates to
/// [`count_messages`]; this helper just composes them into the
/// single check the gate needs).
///
/// Raw [`role_turn_complete`] alone is NOT a valid workflow
/// gate — it would fire on stale assistant turns produced
/// before the freshly delivered activation prompt. The
/// `count > baseline` half is what scopes the predicate to
/// "new turn since this activation."
pub fn assistant_turn_completed_since(
    engine: &Engine,
    worktree_path: &Path,
    session_id: &str,
    baseline: usize,
) -> bool {
    count_messages(engine, worktree_path, session_id, MessageKind::Assistant)
        > baseline
        && role_turn_complete(engine, worktree_path, session_id)
}

fn claude_turn_complete(worktree_path: &Path, session_id: &str) -> bool {
    let Some(path) = claude_transcript_path(worktree_path, session_id) else {
        return false;
    };
    cache::get(&Engine::ClaudeCode, &path)
        .map(|s| s.turn_complete())
        .unwrap_or(false)
}

fn codex_turn_complete(session_id: &str) -> bool {
    let Some(path) = codex_transcript_path(session_id) else {
        return false;
    };
    cache::get(&Engine::Codex, &path)
        .map(|s| s.turn_complete())
        .unwrap_or(false)
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
    let path = match engine {
        Engine::ClaudeCode => claude_transcript_path(worktree_path, session_id),
        Engine::Codex => codex_transcript_path(session_id),
    };
    let Some(path) = path else { return 0 };
    // Counts are maintained by the byte-cursor cache (see `cache.rs`) —
    // every call only re-parses bytes appended since the previous one.
    // The cache mirrors this function's prior exclusion rules: Claude
    // skips synthetic user-tool-result records; Codex skips
    // `event_msg`/`agent_message` (which mirrors a later canonical
    // `response_item` assistant entry 1:1 — counting both would
    // double the assistant turn count and fire the on_idle gate one
    // turn early).
    let Some(state) = cache::get(engine, &path) else {
        return 0;
    };
    match kind {
        MessageKind::Assistant => state.assistant_count(),
        MessageKind::User => state.user_count(),
    }
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
    if kind == MessageKind::User {
        if is_claude_tool_result(v) {
            return None;
        }
        // Skip entries claude marks as meta (e.g. the `<local-command-caveat>`
        // prelude it injects post-`/clear`) and slash-command invocations
        // (`<command-name>/clear</command-name>` records) — these are not
        // real user-typed turns and would otherwise take the `user[0]` slot
        // in a freshly-cleared session, so `{{ initial_prompt }}` would
        // expand to `/clear` instead of the user's first real prompt.
        if v.get("isMeta").and_then(|m| m.as_bool()) == Some(true) {
            return None;
        }
        if let Some(s) = v.pointer("/message/content").and_then(|c| c.as_str()) {
            if s.trim_start().starts_with("<command-name>") {
                return None;
            }
        }
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

    // `event_msg`/`agent_message` is intentionally NOT surfaced here:
    // it mirrors a `response_item` assistant message 1:1, so emitting
    // both would duplicate every assistant turn in `list_messages`
    // (and shift `{{ roles.X.assistant[N] }}` indexing).
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

    /// Test guard that takes the crate-wide `env_lock` *before* doing
    /// anything fs-related, then creates a tempdir and points HOME at
    /// it. The lock-first ordering matters: `bind_with_tight_umask`
    /// elsewhere in the crate transiently sets the process umask to
    /// `0o177`, and `tempfile::tempdir()` running under that umask
    /// yields a directory with mode `0o600` (no execute) that
    /// `create_dir_all` on subdirectories then fails on.
    ///
    /// Holding the env lock *across* tempdir creation, fs writes, and
    /// every transcript read in the test body keeps the umask stable
    /// at the process default for the duration.
    struct HomeOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        tmp: tempfile::TempDir,
        old: Option<std::ffi::OsString>,
    }
    impl HomeOverride {
        fn new() -> Self {
            let guard = crate::test_support::env_lock();
            // Tempdir creation runs inside the lock so no parallel test
            // can be in the middle of `bind_with_tight_umask` when we
            // mkdtemp(). The default process umask applies.
            let tmp = tempfile::tempdir().expect("tempdir");
            let old = std::env::var_os("HOME");
            unsafe { std::env::set_var("HOME", tmp.path()); }
            HomeOverride { _guard: guard, tmp, old }
        }
        fn path(&self) -> &std::path::Path {
            self.tmp.path()
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

    /// Write a transcript under `home/.claude/projects/<encoded>/<session_id>.jsonl`.
    /// `home` is typically the path returned by `HomeOverride::path()`,
    /// so calls go inside the env-lock-protected scope (no umask race).
    fn write_transcript(home: &std::path::Path, encoded: &str, session_id: &str, content: &str) {
        let proj_dir = home.join(format!(".claude/projects/{}", encoded));
        std::fs::create_dir_all(&proj_dir).unwrap();
        std::fs::write(proj_dir.join(format!("{}.jsonl", session_id)), content).unwrap();
    }

    /// Verify plan extraction on a real transcript line — the same shape we
    /// confirmed by inspecting Claude Code's JSONL output.
    #[test]
    fn extract_plan_from_tool_use() {
        let _h = HomeOverride::new();
        let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_x","name":"ExitPlanMode","input":{"plan":"# Plan\n\n1. step one\n2. step two","planFilePath":"/tmp/p.md"}}]}}"##;
        write_transcript(_h.path(), "-tmp-repo", "sid-test", line);
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
        let _h = HomeOverride::new();
        let lines = vec![
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"ExitPlanMode","input":{"plan":"first plan"}}]}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"ExitPlanMode","input":{"plan":"second plan"}}]}}"##,
        ];
        write_transcript(_h.path(), "-tmp-repo", "sid-test", &lines.join("\n"));
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
        let _h = HomeOverride::new();
        let lines = vec![
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"ExitPlanMode","input":{"plan":"old plan"}}]}}"##,
            r##"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"great, now do something else"}]}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"started working on the new thing"}]}}"##,
        ];
        write_transcript(_h.path(), "-tmp-repo", "sid-test", &lines.join("\n"));
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

    /// Reproduces the production failure. Transcript has TWO assistant entries:
    /// one thinking-only, one text-only. `list_messages` (text-extraction)
    /// returns 1 — that's what the old gate used and why it was off-by-one.
    /// `count_messages` returns 2 — the correct turn count for the gate.
    #[test]
    fn gate_undercounts_when_thinking_only() {
        let _h = HomeOverride::new();
        let lines = [
            // Thinking-only assistant turn (real shape from Claude JSONL).
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"planning..."}]}}"##,
            // Text-only assistant turn.
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"here you go"}]}}"##,
        ];
        write_transcript(_h.path(), "-tmp-repo", "sid-x", &lines.join("\n"));
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
        let _h = HomeOverride::new();
        let pre = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"pre-launch response"}]}}"##;
        write_transcript(_h.path(), "-tmp-repo", "sid-x", pre);
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
    fn claude_turn_complete_only_when_end_turn() {
        let _h = HomeOverride::new();
        // Two session files under the same HomeOverride — can't hold two
        // overrides at once (the guard mutex serializes them).
        let lines_busy = [
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn"}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}],"stop_reason":"tool_use"}}"##,
        ]
        .join("\n");
        let lines_done = [
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}],"stop_reason":"tool_use"}}"##,
            r##"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"done"}],"stop_reason":"end_turn"}}"##,
        ]
        .join("\n");
        write_transcript(_h.path(), "-tmp-repo", "sid-busy", &lines_busy);
        write_transcript(_h.path(), "-tmp-repo", "sid-done", &lines_done);
        let wt = std::path::PathBuf::from("/tmp/repo");
        assert!(!role_turn_complete(&Engine::ClaudeCode, &wt, "sid-busy"));
        assert!(role_turn_complete(&Engine::ClaudeCode, &wt, "sid-done"));
    }

    /// After `/clear`, claude rotates the transcript file. The new file
    /// starts with (1) a `<local-command-caveat>` meta record and (2) the
    /// `<command-name>/clear</command-name>` slash-command record before
    /// any real user input lands. `list_messages(User)` must skip both so
    /// `{{ roles.X.initial_prompt }}` (which is `user[0]` post-baseline)
    /// returns the user's first *real* prompt, not `/clear`.
    #[test]
    fn user_list_skips_meta_and_slash_command_records() {
        let _h = HomeOverride::new();
        let lines = [
            r##"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>Caveat: ..."}}"##,
            r##"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>\n<command-message>clear</command-message>"}}"##,
            r##"{"type":"user","message":{"role":"user","content":"real prompt from user"}}"##,
        ]
        .join("\n");
        write_transcript(_h.path(), "-tmp-repo", "sid-c", &lines);
        let msgs = list_messages(
            &Engine::ClaudeCode,
            &std::path::PathBuf::from("/tmp/repo"),
            "sid-c",
            MessageKind::User,
        );
        assert_eq!(msgs, vec!["real prompt from user".to_string()]);
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

    /// Write a Codex JSONL rollout under the temp HOME for tests. First
    /// line is a session_meta carrying the requested `id`; subsequent
    /// lines are appended verbatim (each on its own line). `home` is
    /// the path returned by `HomeOverride::path()`.
    fn write_codex_transcript(home: &std::path::Path, sid: &str, body_lines: &[&str]) {
        let dir = home.join(".codex/sessions/2026/01/15");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.jsonl", sid));
        let mut content = format!(
            r##"{{"payload":{{"id":"{}","type":"session_meta"}}}}"##,
            sid
        );
        for line in body_lines {
            content.push('\n');
            content.push_str(line);
        }
        std::fs::write(path, content).unwrap();
    }

    /// Expected user/assistant text extraction for a fixture engine, from the
    /// shared `tests/fixtures/transcripts/expected.json` corpus (see that dir's
    /// README). The Python MCP parser asserts the SAME expected on the SAME
    /// fixtures (`mcp_server/tests/test_transcripts.py::SharedFixtureCorpusTest`)
    /// so a drift in either parser's user/assistant text extraction fails CI in
    /// both languages.
    fn corpus_expected(engine: &str) -> (Vec<String>, Vec<String>) {
        let expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/transcripts/expected.json"
        ))
        .expect("expected.json parses");
        let pull = |key: &str| -> Vec<String> {
            expected[engine][key]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        (pull("user"), pull("assistant"))
    }

    /// Shared corpus — CLAUDE. `list_messages` user/assistant text extraction
    /// must match the canonical `expected.json` (isMeta + pure-tool_result
    /// users dropped; text-only).
    #[test]
    fn shared_fixture_corpus_claude_user_assistant_text() {
        let _h = HomeOverride::new();
        let wt = _h.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let encoded = wt.to_str().unwrap().replace('/', "-").replace('.', "-");
        write_transcript(
            _h.path(),
            &encoded,
            "sid-claude",
            include_str!("../../../tests/fixtures/transcripts/claude_text.jsonl"),
        );
        let (exp_user, exp_assistant) = corpus_expected("claude");
        assert_eq!(
            list_messages(&Engine::ClaudeCode, &wt, "sid-claude", MessageKind::User),
            exp_user,
            "claude USER text extraction drifted from the shared corpus",
        );
        assert_eq!(
            list_messages(&Engine::ClaudeCode, &wt, "sid-claude", MessageKind::Assistant),
            exp_assistant,
            "claude ASSISTANT text extraction drifted from the shared corpus",
        );
    }

    /// Shared corpus — CODEX. `list_messages` user/assistant extraction must
    /// match `expected.json`, including the `event_msg`/`agent_message` mirror
    /// dedup (the assistant turn counts once) and dropped lifecycle records.
    #[test]
    fn shared_fixture_corpus_codex_user_assistant_text() {
        let _h = HomeOverride::new();
        let body =
            include_str!("../../../tests/fixtures/transcripts/codex_text.jsonl");
        let lines: Vec<&str> =
            body.lines().filter(|l| !l.trim().is_empty()).collect();
        write_codex_transcript(_h.path(), "sid-codex", &lines);
        let (exp_user, exp_assistant) = corpus_expected("codex");
        // Codex resolves the path by session id, so the worktree arg is ignored.
        let ignored = std::path::Path::new("/ignored-by-codex");
        assert_eq!(
            list_messages(&Engine::Codex, ignored, "sid-codex", MessageKind::User),
            exp_user,
            "codex USER text extraction drifted from the shared corpus",
        );
        assert_eq!(
            list_messages(&Engine::Codex, ignored, "sid-codex", MessageKind::Assistant),
            exp_assistant,
            "codex ASSISTANT extraction drifted (mirror-dedup?) from the shared corpus",
        );
    }

    /// `codex_last_message` returns the canonical `response_item`
    /// assistant text. `agent_message` event_msg records that mirror
    /// it must NOT be considered (they're a streaming sidecar). The
    /// `response_item` always lands AFTER the mirror per real Codex
    /// output, so walking backwards we hit it first either way — but
    /// the explicit skip in the parser guarantees the correct answer
    /// even if hypothetical orderings reverse.
    #[test]
    fn codex_last_message_returns_canonical_response_item() {
        let _h = HomeOverride::new();
        write_codex_transcript(
            _h.path(),
            "sid-canon",
            &[
                r##"{"type":"event_msg","payload":{"type":"agent_message","message":"the answer"}}"##,
                r##"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"the answer"}]}}"##,
            ],
        );
        assert_eq!(
            codex_last_message("sid-canon").as_deref(),
            Some("the answer")
        );
    }

    /// Regression for the dedup case the prior tests masked by using
    /// different text in each shape. With identical text in the pair
    /// (matching real Codex output), `count_messages(Assistant)` must
    /// return exactly 1 — counting both would be the off-by-one bug.
    #[test]
    fn count_messages_assistant_dedupes_mirrored_pair() {
        let _h = HomeOverride::new();
        write_codex_transcript(
            _h.path(),
            "sid-dedupe",
            &[
                r##"{"type":"event_msg","payload":{"type":"agent_message","message":"same text"}}"##,
                r##"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"same text"}]}}"##,
                r##"{"type":"event_msg","payload":{"type":"task_complete"}}"##,
            ],
        );
        let n = count_messages(
            &Engine::Codex,
            std::path::Path::new("/ignored-by-codex"),
            "sid-dedupe",
            MessageKind::Assistant,
        );
        assert_eq!(n, 1, "single logical turn must count exactly once");
    }

    /// `list_messages(Assistant)` must surface exactly one entry per
    /// logical turn even when both record shapes are present — i.e.
    /// the canonical `response_item`.
    #[test]
    fn list_messages_assistant_dedupes_mirrored_pair() {
        let _h = HomeOverride::new();
        write_codex_transcript(
            _h.path(),
            "sid-list-dedupe",
            &[
                r##"{"type":"event_msg","payload":{"type":"agent_message","message":"reply one"}}"##,
                r##"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply one"}]}}"##,
                r##"{"type":"event_msg","payload":{"type":"agent_message","message":"reply two"}}"##,
                r##"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"reply two"}]}}"##,
            ],
        );
        let msgs = list_messages(
            &Engine::Codex,
            std::path::Path::new("/ignored-by-codex"),
            "sid-list-dedupe",
            MessageKind::Assistant,
        );
        assert_eq!(
            msgs,
            vec!["reply one".to_string(), "reply two".to_string()]
        );
    }

    /// User-kind extraction is unaffected by the change.
    #[test]
    fn list_messages_user_does_not_pick_up_agent_message() {
        let _h = HomeOverride::new();
        write_codex_transcript(
            _h.path(),
            "sid-user",
            &[
                r##"{"type":"event_msg","payload":{"type":"agent_message","message":"shouldnt-appear"}}"##,
                r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"real user"}]}}"##,
            ],
        );
        let msgs = list_messages(
            &Engine::Codex,
            std::path::Path::new("/ignored-by-codex"),
            "sid-user",
            MessageKind::User,
        );
        assert_eq!(msgs, vec!["real user".to_string()]);
    }

    /// Two count_messages calls back-to-back with an append in between
    /// should return the cumulative count. The cache must detect the
    /// length change and incrementally apply the new lines on top of
    /// state from the first call. Without the incremental path, the
    /// pre-cache implementation re-read the whole file every time;
    /// this test exercises the cache's fast path explicitly.
    #[test]
    fn cache_incremental_append_updates_count() {
        let _h = HomeOverride::new();
        let initial = [
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"one"}],"stop_reason":"end_turn"}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"two"}],"stop_reason":"end_turn"}}"##,
        ]
        .join("\n");
        write_transcript(_h.path(), "-tmp-repo", "sid-incr", &(initial + "\n"));
        let wt = std::path::PathBuf::from("/tmp/repo");

        let n1 = count_messages(&Engine::ClaudeCode, &wt, "sid-incr", MessageKind::Assistant);
        assert_eq!(n1, 2);

        // Append a third turn. count_messages must reflect it next call.
        let f_path = std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".claude/projects/-tmp-repo/sid-incr.jsonl"))
            .unwrap();
        let appended = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"three"}],"stop_reason":"end_turn"}}"##;
        let mut prev = std::fs::read_to_string(&f_path).unwrap();
        prev.push_str(appended);
        prev.push('\n');
        std::fs::write(&f_path, prev).unwrap();

        let n2 = count_messages(&Engine::ClaudeCode, &wt, "sid-incr", MessageKind::Assistant);
        assert_eq!(n2, 3);
    }

    /// Simulate Claude's `/clear` rotation: the new transcript lives at
    /// a different path (new sid), so the cache should produce a fresh
    /// count for the new file regardless of what the old file accrued.
    /// This is the case that motivates path-keyed entries.
    #[test]
    fn cache_rotation_via_new_path_starts_fresh() {
        let _h = HomeOverride::new();
        let pre = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"old"}],"stop_reason":"end_turn"}}"##;
        write_transcript(_h.path(), "-tmp-repo", "sid-old", &(pre.to_string() + "\n"));
        // The post-/clear transcript shares the encoded worktree dir but
        // gets a different sid → different absolute path.
        let post = [
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"new1"}],"stop_reason":"end_turn"}}"##,
            r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"new2"}],"stop_reason":"end_turn"}}"##,
        ]
        .join("\n");
        write_transcript(_h.path(), "-tmp-repo", "sid-new", &(post + "\n"));
        let wt = std::path::PathBuf::from("/tmp/repo");

        // Prime the old-sid cache entry.
        let n_old = count_messages(&Engine::ClaudeCode, &wt, "sid-old", MessageKind::Assistant);
        assert_eq!(n_old, 1);
        // New sid → new cache entry → count derived from new file only.
        let n_new = count_messages(&Engine::ClaudeCode, &wt, "sid-new", MessageKind::Assistant);
        assert_eq!(n_new, 2);
    }

    /// If a writer flushes a line without its trailing `\n` and we read
    /// the file *before* the `\n` arrives, the partial line should still
    /// be counted on the current call (so the gate isn't artificially
    /// delayed) AND when the `\n` lands later, the count must not
    /// double-count the same line.
    #[test]
    fn cache_partial_trailing_line_is_counted_then_committed() {
        let _h = HomeOverride::new();
        // First line is fully terminated; second line is missing its
        // trailing `\n`.
        let partial_body = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first"}],"stop_reason":"end_turn"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second-partial"}],"stop_reason":"end_turn"}}"##;
        write_transcript(_h.path(), "-tmp-repo", "sid-partial", partial_body);
        let wt = std::path::PathBuf::from("/tmp/repo");

        let n1 = count_messages(&Engine::ClaudeCode, &wt, "sid-partial", MessageKind::Assistant);
        assert_eq!(n1, 2, "partial trailing line must be counted");

        // Now finish the second line (just append the `\n`). Total
        // should still be 2 — not 3.
        let f_path = std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".claude/projects/-tmp-repo/sid-partial.jsonl"))
            .unwrap();
        let mut prev = std::fs::read_to_string(&f_path).unwrap();
        prev.push('\n');
        std::fs::write(&f_path, prev).unwrap();

        let n2 = count_messages(&Engine::ClaudeCode, &wt, "sid-partial", MessageKind::Assistant);
        assert_eq!(n2, 2, "completing the same line must not double-count");
    }
}
