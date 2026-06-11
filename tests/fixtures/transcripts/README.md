# Shared transcript-parsing fixtures

The Claude and Codex transcript formats are parsed **twice**: once in Rust
(`daemon/src/workflow/transcript.rs`, drives workflow template rendering + the
idle gate) and once in Python (`mcp_server/transcripts/`, backs the
`read_session_output` MCP tool). The two encode the same knowledge of these
JSONL formats independently and could silently drift.

These fixtures pin the **agreed surface** in BOTH languages: the ordered TEXT
content of `user` and `assistant` turns, including the format-specific skip
rules both parsers share —

- Codex `event_msg`/`agent_message` mirrors the canonical `response_item`
  assistant 1:1; both parsers drop the mirror so a turn counts **once**.
- Codex `event_msg` lifecycle records (`task_complete`) are dropped.
- Claude `isMeta` records and pure-`tool_result` user turns are dropped.

NOT pinned here: tool-call rendering. The two parsers **diverge by design** —
the Python parser surfaces a `[tool_use: …]` one-liner (so agents reading
`read_session_output` see tool activity), while the Rust `list_messages` is
text-only (template rendering wants prose, not tool noise). So the fixtures
contain no `tool_use` assistant lines.

`expected.json` is the canonical extraction. If you change either parser and a
test here fails, the parsers have drifted — reconcile them, don't just bless the
new output.

Consumed by:
- Rust: `daemon/src/workflow/transcript.rs` → `shared_fixture_corpus_*` tests.
- Python: `mcp_server/tests/test_transcripts.py` → `SharedFixtureCorpusTest`.
