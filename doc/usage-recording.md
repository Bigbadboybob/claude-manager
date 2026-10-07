# Usage recording (phase 1)

Each daemon records, once a minute, how many agents are running and in what
state, which models they use, how many subagents they have and which are
actually working, together with Owner's availability level. The data is for
later dashboards; nothing reads it in the TUI yet.

## Storage

Per host, append-only JSON lines in `~/.cm/metrics/usage/YYYY-MM-DD.jsonl`
(UTC day). One sample is about 1–2 KB, so a host writes ~2 MB a day. Files
older than two days are gzipped in place (`.jsonl.gz`); files older than 180
days are deleted. Records survive daemon restarts; a restart only leaves a gap
of a minute or so. Hosts record independently (laptop, cm-sessions,
cm-manager); combine them by `host` / `daemon_id`.

## Records

```text
sample = {v: 1, type: "sample", ts: unix_s, host, daemon_id,
  owner: {level: away|around|focused|on-call|null, changed_at},
  agents: {total, continuous, owner,
           by_state: {working, working-background, idle, waiting-on-human,
                      errored, starting, unknown},
           by_kind_state: {continuous: {…}, owner: {…}},
           by_engine: {claude-code, codex},
           by_model: {<model>: n, unknown: n}},
  subagents: {total, active, sessions: [{uid, engine, total, active}]},
  shells: {total, running, idle},
  owner_input: {input_s, sessions: [uid], resolution_s: 5}}

availability = {v: 1, type: "availability", ts, host, daemon_id,
                from, to, changed_at, event_id}
```

- **Agents** are live `claude-code` and `codex` sessions on this host (bash
  panes excluded). **Continuous** means a continuous orchestrator or a session
  it manages; everything else counts as **owner** work.
- **State** is the engine-reported `agent_state.state`
  ([SESSION_STATE.md](SESSION_STATE.md)); keys with no sessions are omitted.
- **Model** is the newest real turn in the session's transcript tail: Claude's
  assistant `message.model`, Codex's `turn_context.model`. Only the last 64 KB
  is read, and only after the file grew.
- **Subagents.** Claude: the session's
  `~/.claude/projects/<project>/<session-id>/subagents/agent-*.jsonl` files.
  Codex (0.160 multi-agent): child rollouts whose `session_meta` has
  `thread_source: "subagent"` and `parent_thread_id` equal to the session's
  rollout id; only the last two days of `~/.codex/sessions` are indexed, and
  each file's first line is read once. A subagent is **active** when its
  transcript grew in the last 90 s, or (Claude) its newest record is an
  unanswered `tool_use` and it grew in the last 10 minutes. Codex child
  rollouts also carry `agent_nickname` and `agent_path`, which a later phase
  can record.
- **Shells** are bash panes, counted separately (running = PTY activity per
  the legacy state fallback); Owner counts long shell jobs as active work.
- **Owner input** is how much Owner typed since the previous sample, from the
  daemon's per-session last-keystroke stamp (W0b viewer-input tracking): every
  5 s tick in which some session got a new keystroke adds 5 s to `input_s`,
  and `sessions` lists the sessions typed into. Typing in the TUI's chat or
  board overlays is not session input and is not counted.
- **Availability** lines are written whenever the replicated level changes
  (checked every 5 s), so the history between samples is complete. After a
  restart the level already on disk is not recorded again.

## Reading

- Daemon RPC `usage.read {since?, until?, type?, limit?}` returns
  `{records, truncated}` for this host, oldest first (at most 5000;
  `since` defaults to one hour ago). Times are unix seconds, RFC 3339, or
  relative like `6h` / `2d`. Session and Operator callers may read it.
- `scripts/cm-usage [--since 2d] [--type sample|availability]
  [--format summary|csv|json] [--ssh HOST]… [--all-hosts]` reads the files
  directly, plus the same files on each `--ssh` host or every ssh host in
  `~/.cm/hosts.toml`, and merges them into one timeline. Availability lines
  are replicated to every host, so they are de-duplicated by `event_id`; with
  several hosts the summary and CSV are one row per minute summed across hosts
  (Owner input capped at 60 s).
