//! Process-global byte-cursor cache for transcript stats.
//!
//! `count_messages` and `role_turn_complete` used to call
//! `fs::read_to_string` on the whole transcript every workflow tick — at
//! 10Hz across multiple roles, with multi-MB transcripts, that dominated
//! the tick budget. This cache keeps a per-path entry holding
//! `(mtime, len, bytes_consumed)` plus the engine-specific parsed state.
//! On each call we stat the file: unchanged → return cached state; grew
//! → seek to `bytes_consumed` and parse only the appended bytes.
//!
//! Invalidation: keying on the on-disk path is enough. Claude rotates
//! the transcript on `/clear` by creating a NEW file at a NEW path, so
//! the next call lands on a fresh entry with state from offset 0. If a
//! file ever shrinks (`len < bytes_consumed`) we treat it as a reset
//! and re-parse from 0 — append-only logs never shrink in practice,
//! but if the world surprises us, the safe behavior is to recompute.
//!
//! Memory growth is bounded by the number of distinct transcript files
//! the TUI has touched; the entries are small (~80B + a few counters)
//! and stale ones (post-`/clear` rotation) are leaked deliberately — at
//! TUI session scale that's negligible and the alternative (eviction
//! policy) buys nothing.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

use crate::workflow::toml_schema::Engine;

/// Engine-specific parsed-state snapshot. Counts are cumulative across
/// every complete line consumed so far; the tail fields capture the
/// most recent relevant record, which is all `role_turn_complete` needs.
#[derive(Clone, Debug)]
pub(super) enum State {
    Claude {
        assistant_turns: usize,
        user_turns: usize,
        /// Most recent assistant line's `stop_reason == "end_turn"`.
        /// `None` until the first assistant line is seen.
        last_assistant_end_turn: Option<bool>,
    },
    Codex {
        assistant_turns: usize,
        user_turns: usize,
        /// Tracks the LATEST relevant turn-state event seen so far:
        ///   - `Some(true)`  — last relevant event was `task_complete`
        ///   - `Some(false)` — last relevant event was `function_call`
        ///                     / `reasoning` / `message`
        ///   - `None` — neither has been seen yet
        latest_turn_state: Option<bool>,
    },
}

impl State {
    fn fresh(engine: &Engine) -> Self {
        match engine {
            Engine::ClaudeCode => State::Claude {
                assistant_turns: 0,
                user_turns: 0,
                last_assistant_end_turn: None,
            },
            Engine::Codex => State::Codex {
                assistant_turns: 0,
                user_turns: 0,
                latest_turn_state: None,
            },
        }
    }

    pub(super) fn assistant_count(&self) -> usize {
        match self {
            State::Claude { assistant_turns, .. } | State::Codex { assistant_turns, .. } => {
                *assistant_turns
            }
        }
    }

    pub(super) fn user_count(&self) -> usize {
        match self {
            State::Claude { user_turns, .. } | State::Codex { user_turns, .. } => *user_turns,
        }
    }

    pub(super) fn turn_complete(&self) -> bool {
        match self {
            State::Claude { last_assistant_end_turn, .. } => {
                last_assistant_end_turn.unwrap_or(false)
            }
            State::Codex { latest_turn_state, .. } => latest_turn_state.unwrap_or(false),
        }
    }
}

/// Per-path cache entry. `committed_state` is the parse state at the
/// last newline boundary we've seen — it advances only when we observe
/// a `\n` terminator, so an in-progress (un-terminated) trailing line
/// can come and go without poisoning the count. `cached_state` is the
/// state including that trailing partial, so a file legitimately
/// missing a trailing newline (test fixtures, or a writer that hasn't
/// flushed the final `\n` yet) still surfaces every line to readers.
#[derive(Clone, Debug)]
struct Entry {
    mtime: SystemTime,
    len: u64,
    /// Byte offset of the last `\n` we've consumed. The bytes up to
    /// this point have been applied to `committed_state`.
    bytes_consumed: u64,
    committed_state: State,
    cached_state: State,
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Entry>> {
    static C: OnceLock<Mutex<HashMap<PathBuf, Entry>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Return the cached `State` for `path`, refreshing it incrementally from
/// disk if the file has changed since the last call. `None` means the
/// file doesn't exist or its metadata wasn't readable — caller should
/// fall back to a zero-count / not-idle answer.
pub(super) fn get(engine: &Engine, path: &Path) -> Option<State> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let len = meta.len();

    let mut map = cache().lock().ok()?;
    let entry = map.entry(path.to_path_buf()).or_insert_with(|| Entry {
        mtime: SystemTime::UNIX_EPOCH,
        len: 0,
        bytes_consumed: 0,
        committed_state: State::fresh(engine),
        cached_state: State::fresh(engine),
    });

    // Engine mismatch (shouldn't happen in production — Claude and Codex
    // paths never collide — but reset rather than silently merging).
    let engine_matches = matches!(
        (&entry.committed_state, engine),
        (State::Claude { .. }, Engine::ClaudeCode) | (State::Codex { .. }, Engine::Codex)
    );
    if !engine_matches {
        entry.committed_state = State::fresh(engine);
        entry.cached_state = State::fresh(engine);
        entry.bytes_consumed = 0;
        entry.mtime = SystemTime::UNIX_EPOCH;
        entry.len = 0;
    }

    // Unchanged — return cached.
    if entry.mtime == mtime && entry.len == len {
        let cached = entry.cached_state.clone();
        // Load-bearing: dropping the mutex guard explicitly before
        // returning measurably eliminates a TUI lag regression observed
        // when a heavy background (non-workflow) session runs alongside
        // a paused workflow. The mechanism isn't fully understood —
        // `cache::get` is only called from the workflow tick on a single
        // thread, so the contention story doesn't hold — but the
        // perturbation is reliable: without these explicit drops,
        // `drain_terminal_events` and `draw` phases spike to
        // hundreds of ms; with them, they don't. Best guess is a
        // compiler-level effect (different lifetime metadata changing
        // codegen for the surrounding function), so leave the drops in
        // place even though NLL would otherwise free the guard at the
        // same logical point.
        drop(map);
        return Some(cached);
    }

    // File shrank or rotated underneath us — reset to a fresh parse.
    if len < entry.bytes_consumed {
        entry.committed_state = State::fresh(engine);
        entry.cached_state = State::fresh(engine);
        entry.bytes_consumed = 0;
    }

    // Read the bytes from the last commit point through current EOF —
    // not just appended bytes, because the trailing partial line may
    // have grown.
    let mut file = fs::File::open(path).ok()?;
    if file.seek(SeekFrom::Start(entry.bytes_consumed)).is_err() {
        return Some(entry.cached_state.clone());
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return Some(entry.cached_state.clone());
    }
    let slice = match std::str::from_utf8(&buf) {
        Ok(s) => s,
        // Non-UTF8 bytes mid-append (shouldn't happen with JSON) — leave
        // the cache untouched and try again next tick when more bytes
        // may have flushed.
        Err(_) => return Some(entry.cached_state.clone()),
    };

    // Commit bytes up to the last newline boundary. Trailing bytes after
    // the final `\n` are an in-progress line — fold them into
    // `cached_state` so the count is right NOW, but leave
    // `committed_state` and `bytes_consumed` alone so the next tick can
    // re-derive `cached_state` correctly if the partial line grows.
    let commit_end = match slice.rfind('\n') {
        Some(p) => p + 1,
        None => 0,
    };
    apply_lines(&mut entry.committed_state, &slice[..commit_end]);
    entry.bytes_consumed += commit_end as u64;

    let mut cached = entry.committed_state.clone();
    if commit_end < slice.len() {
        apply_lines(&mut cached, &slice[commit_end..]);
    }
    entry.cached_state = cached.clone();
    entry.mtime = mtime;
    entry.len = len;
    // See the load-bearing comment on the fast-path return above —
    // same rationale applies here.
    drop(map);
    Some(cached)
}

fn apply_lines(state: &mut State, slice: &str) {
    for line in slice.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match state {
            State::Claude {
                assistant_turns,
                user_turns,
                last_assistant_end_turn,
            } => apply_claude_line(&v, assistant_turns, user_turns, last_assistant_end_turn),
            State::Codex {
                assistant_turns,
                user_turns,
                latest_turn_state,
            } => apply_codex_line(&v, assistant_turns, user_turns, latest_turn_state),
        }
    }
}

fn apply_claude_line(
    v: &serde_json::Value,
    assistant_turns: &mut usize,
    user_turns: &mut usize,
    last_assistant_end_turn: &mut Option<bool>,
) {
    let Some(ty) = v.get("type").and_then(|t| t.as_str()) else {
        return;
    };
    match ty {
        "assistant" => {
            *assistant_turns += 1;
            let sr = v
                .pointer("/message/stop_reason")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            *last_assistant_end_turn = Some(sr == "end_turn");
        }
        "user" => {
            // Exclude synthetic tool_result records: they aren't real
            // user turns. Mirrors the `is_claude_tool_result` guard in
            // the legacy `count_messages` path.
            if !is_claude_tool_result(v) {
                *user_turns += 1;
            }
        }
        _ => {}
    }
}

fn apply_codex_line(
    v: &serde_json::Value,
    assistant_turns: &mut usize,
    user_turns: &mut usize,
    latest_turn_state: &mut Option<bool>,
) {
    // Role-bearing path: response_item carries the canonical user/assistant
    // turns. `event_msg`/`agent_message` is intentionally NOT counted — it
    // mirrors a later `response_item` assistant entry 1:1 and would
    // double-count. (See the long comment in `count_messages` for the
    // empirical basis.)
    let role = v
        .pointer("/payload/role")
        .and_then(|r| r.as_str())
        .or_else(|| v.pointer("/role").and_then(|r| r.as_str()));
    match role {
        Some("assistant") => *assistant_turns += 1,
        Some("user") => *user_turns += 1,
        _ => {}
    }

    // Turn-complete tracking. Mirrors `codex_turn_complete`: the most
    // recent relevant event wins. Walking forward and overwriting at
    // each relevant event yields the same answer as walking backward
    // and stopping at the first.
    let top = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let ptype = v.pointer("/payload/type").and_then(|t| t.as_str()).unwrap_or("");
    if top == "event_msg" && ptype == "task_complete" {
        *latest_turn_state = Some(true);
    } else if top == "response_item"
        && matches!(ptype, "function_call" | "reasoning" | "message")
    {
        *latest_turn_state = Some(false);
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

