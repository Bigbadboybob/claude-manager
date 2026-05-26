//! Tests for the `Agent` trait + impls.
//!
//! Strategy: target the pure helpers (parse_lines, resolve_cursor, cursor
//! encode/decode, make_prompt_pending_write, path resolvers, agent_for).
//! PTY-byte-level behavior — what the drainer writes — is covered by the
//! manual feedback-workflow smoke test that's part of Phase 0 exit
//! criteria. The trait methods themselves are thin glue around the helpers.
//!
//! Path-resolver tests use a process-global `HOME` override; a mutex
//! serializes them so parallel test runs don't fight over the env var.

use super::*;

// ---------------------------------------------------------------------------
// Cursor encode/decode
// ---------------------------------------------------------------------------

#[test]
fn cursor_round_trip() {
    let c = Cursor::new(7, 42);
    assert_eq!(c.raw, "v1:7:42");
    assert_eq!(c.parse().unwrap(), (7, 42));
}

#[test]
fn cursor_zero() {
    let c = Cursor::new(0, 0);
    assert_eq!(c.parse().unwrap(), (0, 0));
}

#[test]
fn cursor_rejects_wrong_version() {
    let c = Cursor {
        raw: "v2:1:1".into(),
    };
    assert!(c.parse().is_err());
}

#[test]
fn cursor_rejects_malformed() {
    for bad in ["", "v1", "v1:", "v1:abc:1", "v1:1:abc", "garbage"] {
        let c = Cursor { raw: bad.into() };
        assert!(c.parse().is_err(), "should reject {:?}", bad);
    }
}

// ---------------------------------------------------------------------------
// resolve_cursor / generation handling
// ---------------------------------------------------------------------------

#[test]
fn resolve_cursor_none_starts_at_zero() {
    assert_eq!(resolve_cursor_for_gen(5, None), (0, 5));
}

#[test]
fn resolve_cursor_matching_generation_keeps_offset() {
    let c = Cursor::new(5, 17);
    assert_eq!(resolve_cursor_for_gen(5, Some(&c)), (17, 5));
}

#[test]
fn resolve_cursor_generation_mismatch_restarts() {
    let c = Cursor::new(3, 17);
    // ts is on gen 5; cursor was issued on gen 3 (pre-/clear) → restart.
    assert_eq!(resolve_cursor_for_gen(5, Some(&c)), (0, 5));
}

#[test]
fn resolve_cursor_malformed_cursor_restarts() {
    let c = Cursor {
        raw: "garbage".into(),
    };
    assert_eq!(resolve_cursor_for_gen(2, Some(&c)), (0, 2));
}

// ---------------------------------------------------------------------------
// Claude Code: parse_lines (parser + cursor advancement)
// ---------------------------------------------------------------------------

#[test]
fn claude_empty_transcript_yields_no_messages() {
    let (msgs, cur) = claude_code::parse_lines("", 0, 100, 0).unwrap();
    assert!(msgs.is_empty());
    assert_eq!(cur.parse().unwrap(), (0, 0));
}

#[test]
fn claude_single_user_turn() {
    let line = r##"{"type":"user","message":{"role":"user","content":"hi"}}"##;
    let (msgs, _) = claude_code::parse_lines(line, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[0].content, "hi");
}

#[test]
fn claude_single_assistant_turn() {
    let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"##;
    let (msgs, _) = claude_code::parse_lines(line, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::Assistant);
    assert_eq!(msgs[0].content, "hello");
}

#[test]
fn claude_tool_use_renders_one_liner() {
    let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}"##;
    let (msgs, _) = claude_code::parse_lines(line, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert!(
        msgs[0].content.starts_with("[tool_use: Bash"),
        "content was {:?}",
        msgs[0].content
    );
    assert!(msgs[0].content.contains("command: ls -la"));
}

#[test]
fn claude_thinking_blocks_drop_silently() {
    // Thinking-only assistant turn renders to empty content → message
    // is skipped. The TURN still counts (`count_assistant_turns` covers
    // it via workflow::transcript), but `read_messages` shouldn't surface
    // it as a noisy empty message.
    let line = r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."}]}}"##;
    let (msgs, cur) = claude_code::parse_lines(line, 0, 100, 0).unwrap();
    assert!(msgs.is_empty());
    assert_eq!(cur.parse().unwrap(), (0, 1)); // line was still consumed
}

#[test]
fn claude_multi_turn_preserves_order() {
    let lines = [
        r##"{"type":"user","message":{"role":"user","content":"a"}}"##,
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"b"}]}}"##,
        r##"{"type":"user","message":{"role":"user","content":"c"}}"##,
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"d"}]}}"##,
    ];
    let content = lines.join("\n");
    let (msgs, _) = claude_code::parse_lines(&content, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0].content, "a");
    assert_eq!(msgs[1].content, "b");
    assert_eq!(msgs[2].content, "c");
    assert_eq!(msgs[3].content, "d");
}

#[test]
fn claude_meta_records_skipped() {
    // The post-/clear `<local-command-caveat>` record carries isMeta: true
    // and should not appear in read_messages output.
    let lines = [
        r##"{"type":"user","isMeta":true,"message":{"role":"user","content":"<local-command-caveat>..."}}"##,
        r##"{"type":"user","message":{"role":"user","content":"real"}}"##,
    ];
    let content = lines.join("\n");
    let (msgs, _) = claude_code::parse_lines(&content, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "real");
}

#[test]
fn claude_slash_command_record_skipped() {
    // The `<command-name>/clear</command-name>` record isn't a real user turn.
    let lines = [
        r##"{"type":"user","message":{"role":"user","content":"<command-name>/clear</command-name>"}}"##,
        r##"{"type":"user","message":{"role":"user","content":"real prompt"}}"##,
    ];
    let content = lines.join("\n");
    let (msgs, _) = claude_code::parse_lines(&content, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "real prompt");
}

#[test]
fn claude_pure_tool_result_user_skipped() {
    // User entries that contain only tool_result items aren't real user turns.
    let line = r##"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}}"##;
    let (msgs, _) = claude_code::parse_lines(line, 0, 100, 0).unwrap();
    assert!(msgs.is_empty());
}

#[test]
fn claude_malformed_line_is_skipped_offset_advances() {
    // Malformed lines must not poison the cursor — offset still advances
    // past them so a later append doesn't get clobbered.
    let lines = [
        "{not valid json",
        r##"{"type":"user","message":{"role":"user","content":"first"}}"##,
        "another garbage line",
        r##"{"type":"user","message":{"role":"user","content":"second"}}"##,
    ];
    let content = lines.join("\n");
    let (msgs, cur) = claude_code::parse_lines(&content, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].content, "first");
    assert_eq!(msgs[1].content, "second");
    assert_eq!(cur.parse().unwrap(), (0, 4)); // all 4 lines consumed
}

#[test]
fn claude_cursor_advances() {
    let lines = [
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a"}]}}"##,
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"b"}]}}"##,
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"c"}]}}"##,
    ];
    let content = lines.join("\n");
    let (msgs1, cur1) = claude_code::parse_lines(&content, 0, 2, 0).unwrap();
    assert_eq!(msgs1.len(), 2);
    assert_eq!(msgs1[1].content, "b");
    let (_, off) = cur1.parse().unwrap();

    let (msgs2, _) = claude_code::parse_lines(&content, off, 100, 0).unwrap();
    assert_eq!(msgs2.len(), 1);
    assert_eq!(msgs2[0].content, "c");
}

#[test]
fn claude_cursor_stable_across_appends() {
    let original = [
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a"}]}}"##,
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"b"}]}}"##,
    ]
    .join("\n");
    let (msgs1, cur1) = claude_code::parse_lines(&original, 0, 100, 0).unwrap();
    assert_eq!(msgs1.len(), 2);
    let (_, off) = cur1.parse().unwrap();

    let appended = format!(
        "{}\n{}",
        original,
        r##"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"c"}]}}"##
    );
    let (msgs2, _) = claude_code::parse_lines(&appended, off, 100, 0).unwrap();
    assert_eq!(msgs2.len(), 1);
    assert_eq!(msgs2[0].content, "c");
}

#[test]
fn claude_limit_is_honored() {
    let lines: Vec<String> = (0..5)
        .map(|i| {
            format!(
                r##"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"msg{}"}}]}}}}"##,
                i
            )
        })
        .collect();
    let content = lines.join("\n");
    let (msgs, _) = claude_code::parse_lines(&content, 0, 2, 0).unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].content, "msg0");
    assert_eq!(msgs[1].content, "msg1");
}

#[test]
fn claude_limit_zero_returns_no_messages_and_no_advance() {
    // Regression: an earlier impl pushed a message before checking the
    // limit, so `limit == 0` returned 1 message and advanced the cursor.
    let line = r##"{"type":"user","message":{"role":"user","content":"hi"}}"##;
    let (msgs, cur) = claude_code::parse_lines(line, 0, 0, 0).unwrap();
    assert!(msgs.is_empty());
    // Cursor must NOT advance past line 0 — that line wasn't consumed.
    assert_eq!(cur.parse().unwrap(), (0, 0));
}

#[test]
fn claude_cursor_carries_generation() {
    let line = r##"{"type":"user","message":{"role":"user","content":"hi"}}"##;
    let (_, cur) = claude_code::parse_lines(line, 0, 100, 9).unwrap();
    let (gen, _off) = cur.parse().unwrap();
    assert_eq!(gen, 9);
}

// ---------------------------------------------------------------------------
// Codex: parse_lines (different schema)
// ---------------------------------------------------------------------------

#[test]
fn codex_empty_transcript_yields_no_messages() {
    let (msgs, cur) = codex::parse_lines("", 0, 100, 0).unwrap();
    assert!(msgs.is_empty());
    assert_eq!(cur.parse().unwrap(), (0, 0));
}

#[test]
fn codex_single_assistant_response_item() {
    let line = r##"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi from codex"}]}}"##;
    let (msgs, _) = codex::parse_lines(line, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::Assistant);
    assert_eq!(msgs[0].content, "hi from codex");
}

#[test]
fn codex_user_input_text_renders() {
    let line = r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"do the thing"}]}}"##;
    let (msgs, _) = codex::parse_lines(line, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[0].content, "do the thing");
}

#[test]
fn codex_function_call_renders_as_tool() {
    let line = r##"{"type":"response_item","payload":{"type":"function_call","name":"shell"}}"##;
    let (msgs, _) = codex::parse_lines(line, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].role, Role::Tool);
    assert!(msgs[0].content.contains("tool_use: shell"));
}

#[test]
fn codex_event_msg_lifecycle_is_filtered() {
    // task_complete / token_count events aren't surfaced as messages.
    let line = r##"{"type":"event_msg","payload":{"type":"task_complete"}}"##;
    let (msgs, _) = codex::parse_lines(line, 0, 100, 0).unwrap();
    assert!(msgs.is_empty());
}

#[test]
fn codex_event_msg_agent_message_is_dropped_as_mirror() {
    // `event_msg`/`agent_message` is intentionally skipped — it mirrors
    // a `response_item` assistant message 1:1 (verified empirically
    // across real Codex sessions). Surfacing both would double-count
    // assistant turns and duplicate every workflow handoff message.
    let line = r##"{"type":"event_msg","payload":{"type":"agent_message","message":"all done"}}"##;
    let (msgs, _) = codex::parse_lines(line, 0, 100, 0).unwrap();
    assert!(
        msgs.is_empty(),
        "agent_message must be dropped — response_item is canonical"
    );
}

#[test]
fn codex_paired_agent_message_and_response_item_emit_one_message() {
    // The real schema: an `event_msg`/`agent_message` (streaming sidecar)
    // followed by a `response_item` assistant message with IDENTICAL
    // text. Reader must see exactly ONE assistant Message — the
    // response_item.
    let lines = [
        r##"{"type":"event_msg","payload":{"type":"agent_message","message":"final answer"}}"##,
        r##"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final answer"}]}}"##,
    ];
    let content = lines.join("\n");
    let (msgs, _) = codex::parse_lines(&content, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1, "must dedupe to exactly one Message");
    assert_eq!(msgs[0].role, Role::Assistant);
    assert_eq!(msgs[0].content, "final answer");
}

#[test]
fn codex_multi_turn_preserves_order() {
    let lines = [
        r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"a"}]}}"##,
        r##"{"type":"response_item","payload":{"role":"assistant","content":[{"type":"output_text","text":"b"}]}}"##,
        r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"c"}]}}"##,
    ];
    let content = lines.join("\n");
    let (msgs, _) = codex::parse_lines(&content, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!((msgs[0].role, msgs[0].content.as_str()), (Role::User, "a"));
    assert_eq!((msgs[1].role, msgs[1].content.as_str()), (Role::Assistant, "b"));
    assert_eq!((msgs[2].role, msgs[2].content.as_str()), (Role::User, "c"));
}

#[test]
fn codex_malformed_line_skipped_offset_advances() {
    let lines = [
        "not json",
        r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"x"}]}}"##,
    ];
    let content = lines.join("\n");
    let (msgs, cur) = codex::parse_lines(&content, 0, 100, 0).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(cur.parse().unwrap(), (0, 2));
}

#[test]
fn codex_limit_zero_returns_no_messages_and_no_advance() {
    let line = r##"{"type":"response_item","payload":{"role":"user","content":[{"type":"input_text","text":"hi"}]}}"##;
    let (msgs, cur) = codex::parse_lines(line, 0, 0, 0).unwrap();
    assert!(msgs.is_empty());
    assert_eq!(cur.parse().unwrap(), (0, 0));
}

#[test]
fn codex_cursor_advances() {
    let lines: Vec<String> = (0..3)
        .map(|i| {
            format!(
                r##"{{"type":"response_item","payload":{{"role":"assistant","content":[{{"type":"output_text","text":"m{}"}}]}}}}"##,
                i
            )
        })
        .collect();
    let content = lines.join("\n");
    let (m1, c1) = codex::parse_lines(&content, 0, 2, 0).unwrap();
    assert_eq!(m1.len(), 2);
    let (_, off) = c1.parse().unwrap();
    let (m2, _) = codex::parse_lines(&content, off, 100, 0).unwrap();
    assert_eq!(m2.len(), 1);
    assert_eq!(m2[0].content, "m2");
}

// ---------------------------------------------------------------------------
// submit_prompt — the queue shape it produces
// ---------------------------------------------------------------------------

#[test]
fn submit_prompt_pending_write_trims_trailing_whitespace() {
    let pw = make_prompt_pending_write("hello\n\n");
    assert_eq!(pw.text, "hello");
}

#[test]
fn submit_prompt_pending_write_preserves_internal_whitespace() {
    let pw = make_prompt_pending_write("line1\nline2");
    assert_eq!(pw.text, "line1\nline2");
}

#[test]
fn submit_prompt_pending_write_marks_submit_true() {
    let pw = make_prompt_pending_write("x");
    assert!(pw.submit, "submit must be true so the drainer fires Enter");
}

#[test]
fn submit_prompt_pending_write_has_quiet_window() {
    use std::time::Duration;
    let pw = make_prompt_pending_write("x");
    // 2s quiet window matches the existing fire_transition floor.
    // Test the exact value because the workflow gate's timing assumptions
    // depend on it; a future engine-specific override would change this.
    assert_eq!(pw.require_quiet, Duration::from_secs(2));
}

// ---------------------------------------------------------------------------
// Selectors
// ---------------------------------------------------------------------------

#[test]
fn agent_for_dispatches_by_session_type() {
    assert_eq!(agent_for("claude").engine(), Engine::ClaudeCode);
    assert_eq!(agent_for("codex").engine(), Engine::Codex);
    // Unknown defaults to Claude (bash never goes through Agent in real use).
    assert_eq!(agent_for("bash").engine(), Engine::ClaudeCode);
}

#[test]
fn agent_for_engine_dispatches_correctly() {
    assert_eq!(agent_for_engine(&Engine::ClaudeCode).engine(), Engine::ClaudeCode);
    assert_eq!(agent_for_engine(&Engine::Codex).engine(), Engine::Codex);
}

// ---------------------------------------------------------------------------
// assistant_turn_completed_since — the workflow gate predicate
// ---------------------------------------------------------------------------
//
// The default impl is `count > baseline && is_idle`. Test it via a mock
// `Agent` whose count and idle outputs we control. AgentCtx must point at
// a `TerminalSession`, but the mock ignores ctx — so we can build a static
// dummy via `&*Box::leak`. The impl never reads the TS for the gate path.

struct MockAgent {
    count: usize,
    idle: bool,
}

impl Agent for MockAgent {
    fn engine(&self) -> Engine {
        Engine::ClaudeCode
    }
    fn submit_prompt(&self, _ctx: AgentCtxMut<'_>, _text: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn read_messages(
        &self,
        _ctx: AgentCtx<'_>,
        _since: Option<&Cursor>,
        _limit: usize,
    ) -> anyhow::Result<(Vec<Message>, Cursor)> {
        Ok((Vec::new(), Cursor::new(0, 0)))
    }
    fn transcript_path(&self, _ctx: AgentCtx<'_>) -> Option<std::path::PathBuf> {
        None
    }
    fn is_idle(&self, _ctx: AgentCtx<'_>) -> bool {
        self.idle
    }
    fn count_assistant_turns(&self, _ctx: AgentCtx<'_>) -> usize {
        self.count
    }
    fn interrupt(&self, _ctx: AgentCtxMut<'_>) {}
}

/// Build a stub AgentCtx that the mock doesn't actually read. Constructs a
/// TerminalSession with a dummy PTY (`/bin/true` exits immediately; the PTY
/// is harmless) — only because Rust's borrow checker insists on a real
/// `&TerminalSession`. The gate predicate logic doesn't touch ts fields.
fn with_stub_ctx<T>(f: impl FnOnce(AgentCtx<'_>) -> T) -> T {
    use std::collections::HashMap;
    use std::path::PathBuf;
    let session = crate::session::Session::new(
        "/bin/true",
        &[],
        80,
        24,
        None,
        HashMap::new(),
        None,
    )
    .expect("dummy session");
    let ts = crate::app::TerminalSession {
        uid: "test".into(),
        label: "test".into(),
        session_type: "claude".into(),
        session,
        status: crate::app::SessionStatus::Idle,
        last_write_at: None,
        transcript_id: None,
        generation: 0,
        pending_jsonl_files: None,
        hidden: false,
        idle_timeout_secs: 0,
        burst_threshold: 0,
        pending_prompt: None,
        pending_clear: None,
        workflow_run_id: None,
        workflow_role: None,
        task_id: None,
        last_delivery: None,
        notify_on_idle: false,
        pending_enter: None,
        created_at: std::time::Instant::now(),
        managed_by_uid: None,
        seeded_from_snapshot: None,
        preserved_last_exit: None,
    };
    let wt = PathBuf::from("/tmp");
    let ctx = AgentCtx {
        ts: &ts,
        worktree_path: &wt,
    };
    f(ctx)
}

#[test]
fn gate_fires_when_new_turn_and_idle() {
    let m = MockAgent { count: 2, idle: true };
    with_stub_ctx(|ctx| {
        assert!(m.assistant_turn_completed_since(ctx, 1));
    });
}

#[test]
fn gate_blocks_when_new_turn_but_not_idle() {
    // Count grew but turn isn't done — must not fire (mid tool call).
    let m = MockAgent { count: 2, idle: false };
    with_stub_ctx(|ctx| {
        assert!(!m.assistant_turn_completed_since(ctx, 1));
    });
}

#[test]
fn gate_blocks_when_idle_but_no_new_turn() {
    // The reviewer's stale-idle-vs-new-turn case: idle is true (last
    // assistant turn from BEFORE activation completed) but count hasn't
    // grown past baseline. Raw is_idle would fire here; the helper must not.
    let m = MockAgent { count: 1, idle: true };
    with_stub_ctx(|ctx| {
        assert!(!m.assistant_turn_completed_since(ctx, 1));
    });
}

#[test]
fn gate_blocks_when_count_below_baseline() {
    // Pathological — count somehow shrunk (transcript truncated?). Don't fire.
    let m = MockAgent { count: 0, idle: true };
    with_stub_ctx(|ctx| {
        assert!(!m.assistant_turn_completed_since(ctx, 1));
    });
}

// ---------------------------------------------------------------------------
// Path resolvers (need HOME override)
// ---------------------------------------------------------------------------

struct HomeOverride {
    _guard: std::sync::MutexGuard<'static, ()>,
    tmp: tempfile::TempDir,
    old: Option<std::ffi::OsString>,
}
impl HomeOverride {
    fn new() -> Self {
        let guard = crate::test_support::home_lock();
        let tmp = tempfile::tempdir().unwrap();
        let old = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }
        HomeOverride {
            _guard: guard,
            tmp,
            old,
        }
    }
}
impl Drop for HomeOverride {
    fn drop(&mut self) {
        unsafe {
            if let Some(h) = self.old.take() {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }
    }
}

#[test]
fn claude_transcript_path_encodes_worktree() {
    let h = HomeOverride::new();
    let p = claude_code::claude_transcript_path(
        std::path::Path::new("/home/user/proj.dir"),
        "abc-def",
    )
    .unwrap();
    let expected = h
        .tmp
        .path()
        .join(".claude/projects/-home-user-proj-dir")
        .join("abc-def.jsonl");
    assert_eq!(p, expected);
}

#[test]
fn claude_transcript_path_returns_none_without_home() {
    let _g = crate::test_support::home_lock();
    let old = std::env::var_os("HOME");
    unsafe {
        std::env::remove_var("HOME");
    }
    let p = claude_code::claude_transcript_path(
        std::path::Path::new("/x"),
        "sid",
    );
    if let Some(h) = old {
        unsafe {
            std::env::set_var("HOME", h);
        }
    }
    assert!(p.is_none());
}

#[test]
fn codex_transcript_path_returns_none_when_no_sessions_dir() {
    let _h = HomeOverride::new();
    // `.codex/sessions` doesn't exist under the temp HOME → None.
    assert!(codex::codex_transcript_path("anything").is_none());
}

#[test]
fn codex_transcript_path_finds_matching_payload_id() {
    let h = HomeOverride::new();
    let dir = h.tmp.path().join(".codex/sessions/2026/01/15");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout.jsonl");
    std::fs::write(
        &path,
        r##"{"payload":{"id":"the-id","type":"session_meta"}}
"##,
    )
    .unwrap();
    let found = codex::codex_transcript_path("the-id").unwrap();
    assert_eq!(found, path);
}

// ---------------------------------------------------------------------------
// Trait-level gate test for Codex agent_message
// ---------------------------------------------------------------------------

/// Build a `TerminalSession` for tests that need to call trait methods
/// against a real session struct. `/bin/true` exits immediately; the PTY
/// is harmless because the gate path doesn't read it.
fn make_codex_test_session(transcript_id: &str) -> crate::app::TerminalSession {
    use std::collections::HashMap;
    let session = crate::session::Session::new(
        "/bin/true",
        &[],
        80,
        24,
        None,
        HashMap::new(),
        None,
    )
    .expect("dummy session");
    crate::app::TerminalSession {
        uid: "codex-test".into(),
        label: "codex-test".into(),
        session_type: "codex".into(),
        session,
        status: crate::app::SessionStatus::Idle,
        last_write_at: None,
        transcript_id: Some(transcript_id.into()),
        generation: 0,
        pending_jsonl_files: None,
        hidden: false,
        idle_timeout_secs: 0,
        burst_threshold: 0,
        pending_prompt: None,
        pending_clear: None,
        workflow_run_id: None,
        workflow_role: None,
        task_id: None,
        last_delivery: None,
        notify_on_idle: false,
        pending_enter: None,
        created_at: std::time::Instant::now(),
        managed_by_uid: None,
        seeded_from_snapshot: None,
        preserved_last_exit: None,
    }
}

#[test]
fn gate_fires_for_codex_real_turn_shape() {
    // End-to-end gate test using the real Codex turn shape observed in
    // production rollouts: agent_message (sidecar) → response_item
    // assistant (canonical) → token_count → task_complete. The gate
    // must see exactly ONE new assistant turn, not two.
    let h = HomeOverride::new();
    let sid = "the-codex-id";
    let dir = h.tmp.path().join(".codex/sessions/2026/01/15");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout.jsonl");
    let lines = [
        r##"{"payload":{"id":"the-codex-id","type":"session_meta"}}"##,
        // The real ordering at end-of-turn (timestamps within ~25ms).
        r##"{"type":"event_msg","payload":{"type":"agent_message","message":"work done"}}"##,
        r##"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"work done"}]}}"##,
        r##"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"##,
        r##"{"type":"event_msg","payload":{"type":"task_complete"}}"##,
    ];
    std::fs::write(&path, lines.join("\n")).unwrap();

    let ts = make_codex_test_session(sid);
    let wt = std::path::PathBuf::from("/tmp/some-worktree");
    let ctx = AgentCtx {
        ts: &ts,
        worktree_path: &wt,
    };
    let agent = CodexAgent;

    // Baseline 0, exactly ONE new turn (the response_item; agent_message
    // is dropped as a mirror). Gate fires.
    assert!(
        agent.assistant_turn_completed_since(ctx, 0),
        "gate should fire after a complete assistant turn"
    );
    // Critical regression check: the count must be 1, not 2.
    // With the mirror counted, baseline=1 would still fire here, which
    // is the off-by-one the previous implementation introduced.
    assert!(
        !agent.assistant_turn_completed_since(ctx, 1),
        "gate must NOT fire when only one real turn exists past baseline"
    );

    // Sanity: the trait counter reports exactly 1 turn (mirror suppressed).
    assert_eq!(
        agent.count_assistant_turns(ctx),
        1,
        "agent_message must NOT be double-counted alongside response_item"
    );
    assert!(agent.is_idle(ctx));
}
