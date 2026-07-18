# Proper async subagent wait (branch `cm/proper-cm-subagent-wait`)

## Goal

Agents that launch or prompt cm sessions should never park in a blocking
`wait_*` MCP call. Instead, spawning/prompting registers an **async monitor**
that fires when the watched session goes (semantically) idle, waking the
caller by delivering a message into its session. This reopens
`DESIGN_TOOL_ERGONOMICS.md` P1 ("push instead of poll — north star"), which
was closed as "cm-wait is good enough" — it isn't: cm-wait requires the agent
to know a script path and burn a background-bash slot, and it can't be made
automatic.

Prereq discovered while designing: PTY message injection (the wake channel)
is slow and glitchy. Three root causes, each with a fix:

1. **Mangled prompts while the operator types** — injection races operator
   keystrokes into the same composer. Fix: the daemon proxies all operator
   input, so gate injection on an operator-quiet window.
2. **"Busy" while a background subagent runs** — `idle` is a PTY-output
   quietness heuristic; spinners keep it noisy forever. Fix: semantic idle
   from transcript shape (fallback) and Claude Code `Stop` hooks (real fix).
3. **Slow** — fixed kitty-arming settle delays on every delivery + poll
   cadence. Fix: cache kitty-armed per session; hook events replace polling.

Delivery redesign: **never inject mid-turn.** A cm-installed `Stop` hook can
return `{"decision":"block","reason":...}` and Claude continues processing
the reason as its next instruction — so mid-turn deliveries go to a
per-session inbox consumed at the next turn boundary (atomic, no typing),
and PTY injection remains only for the already-idle case (safest, and
further gated by operator-quiet).

## Slices

- **S0 — spike: resolve hook ambiguities empirically.**
  (a) Does `Stop` fire when the turn ends while a background task is still
  running? (b) Do hooks passed via `--settings` merge with or replace
  project-settings hooks? (c) Does `Stop` block+reason actually make Claude
  continue with the reason as instruction (verify shape + `stop_hook_active`
  loop guard)? (d) What does the hook receive on stdin / in env (need
  `CM_TUI_SESSION_ID` for inbox routing)?

- **S1 — injection safety + semantic idle, no new architecture.**
  (a) Daemon: track `last_operator_input_at` per session (attach write path
  only — control-plane `send_input` doesn't count); the agent-kitty-async
  delivery defers until the operator has been quiet ~10s (bounded, logged).
  (b) Python monitor: upgrade the idle rule with transcript-shape semantic
  idle — a session whose transcript ends in a completed assistant turn is
  `awaiting_input` even if the PTY is noisy (spinner). Fixes false-busy for
  background-subagent orchestrators.

- **S2 — the async monitor + automatic registration.**
  New MCP tool `monitor_sessions(session_uids, mode, note)` — registers a
  background watch (MCP-server-resident asyncio task reusing
  `_monitor_sessions`), returns a monitor id immediately; on fire, delivers
  a `[cm-monitor]` message into the CALLER's own session via daemon
  `send_input` (self-target is auth-allowed), gated on caller idle, with
  delivery verification + one retry. Auto-registration: `start_session`
  (wait=false) and `send_input` gain `notify_on_done=true` default —
  spawning IS registering; dedupe one active monitor per (caller, target).
  Blocking `wait_*` docstrings demoted to advanced primitives.

- **S3 — hook-based idle + inbox delivery (claude-code engine).**
  Spawn-time `--settings` injection of a cm Stop hook (per S0 findings).
  Hook: (1) reports turn-end to the daemon (semantic-idle event → new
  field surfaced via `resolve_authorized_session`), (2) drains the
  per-session inbox `~/.cm/inbox/<uid>/` and block+reasons pending
  messages at the turn boundary. Monitor fire path: target busy → inbox;
  target idle → gated PTY injection (with post-write re-check to close the
  went-idle race). Codex sessions stay on S2's gated-PTY-on-idle.

## Status

- [x] **S0 spike — DONE (2026-07-18, claude CLI 2.1.214, all findings favorable).**
  (a) `Stop` FIRES while a background Bash task is still running (hook's
  pgrep saw the live `sleep`; stdin even carries a `background_tasks`
  field). (b) `--settings` hooks MERGE with project-settings hooks (both
  fired, project first). (c) block+reason WORKS: `{"decision":"block",
  "reason":...}` → Claude continues, treating reason as the next
  instruction (verified end-to-end: injected "reply BANANA" → final output
  BANANA); second Stop arrives with `stop_hook_active: true` as the loop
  guard. (d) Hook inherits the session env including `CM_TUI_SESSION_ID`
  (verified from a real cm-spawned session), and stdin carries
  `session_id`, `transcript_path`, `last_assistant_message`,
  `background_tasks`, `permission_mode`. Spike artifacts in scratchpad.
- [x] **S1 — DONE.** (a) Daemon: `last_operator_input_at` cell on
  `DaemonSession` (stamped ONLY by the attach-stream input path via
  `InputHandle::write_and_stamp_operator`); both agent-prompt delivery
  threads now call `await_operator_quiet` (4s quiet window, 250ms poll,
  180s max defer, logged) before writing. Tests: 6
  (`operator_quiet_tests` in methods.rs). (b) Python:
  `transcript_turn_complete` in monitor.py (tail-scan for a main-chain
  assistant `end_turn`, sidechain-aware, claude-code only) wired into
  `_monitor_sessions`, `wait_for_session_idle`, and `_await_reply` as a
  debounced (3s) fallback when READY but PTY-busy — completions carry
  `idle_source: "transcript"`. Tests: 12 (test_semantic_idle.py).
- [x] **S2 — DONE.** `mcp_server/async_monitor.py`: MCP-server-resident
  monitor registry; `register_monitor` spawns a background watch
  (reuses `_monitor_sessions`, retries daemon-restart TransportErrors),
  formats a `[cm-monitor <id>]` fire message with each worker's final
  reply, gates on caller-at-prompt, injects via daemon `send_input`
  (self-target), verifies the marker landed in the caller's transcript,
  redelivers once, retains the result either way. New tools:
  `monitor_sessions` / `list_monitors` / `cancel_monitor`.
  Auto-registration: `send_input` (now async, `notify_on_done=True`
  default) and `start_session` (wait=false + prompt + non-bash) attach
  a `monitor` to their result; auto-monitors dedupe per target set.
  Blocking-wait docstrings demoted. Tests: 8 (test_async_monitor.py).
  Note: full mcp suite has 3 PRE-EXISTING failures (missing pytest dep,
  google-libs logging, operator.mark_subtask_done route drift).
- [x] **S3 — DONE.**
  - **Hook**: `mcp_server/hooks/cm_stop_hook.py` (stdlib-only,
    fail-open, loop-safe via consume-before-block). At every turn
    boundary it (a) self-reports `session.turn_ended` to the daemon
    (3s timeout) and (b) drains `~/.cm/inbox/<uid>/*.json`, emitting
    Stop `{"decision":"block","reason":...}` when messages are pending
    — atomic turn-boundary delivery, zero PTY typing.
  - **Spawn injection**: `mcp_config::claude_settings_hook_arg` builds
    the inline `--settings` JSON (Stop hook, 15s timeout, interpreter
    via the shared venv-aware resolver); injected in BOTH argv
    composers — daemon `build_args` and the TUI's `claude_args` (which
    reuses the daemon helper). Fail-open when the script is absent
    (pre-deploy hosts spawn hook-less).
  - **Daemon**: `session.turn_ended` RPC (session-caller self-report;
    self-target Allow, scope-checked otherwise) stamps
    `last_turn_end_at`; new `last_input_at` stamped by every input
    path; `resolve_authorized_session` now reports `semantic_idle`
    (null = no hook data / true = turn-end postdates last input /
    false = turn pending). Wire-verified e2e against a scratch daemon:
    null → turn_ended → true → send_input → false, intruder caller
    unauthorized.
  - **Python**: monitor/wait loops treat hook-confirmed
    `semantic_idle` as a debounce-skip on the transcript-shape rule;
    `async_monitor` delivery now prefers the inbox for mid-turn
    callers (consumption = delivery), taking the message back for
    gated PTY injection when the caller reaches its prompt unconsumed
    (hook-less callers) or the wait cap passes.
  - **Route table**: `session.turn_ended` added to `DAEMON_METHODS`;
    also fixed the pre-existing `operator.mark_subtask_done` alignment
    drift (that suite failure is now green).
  - Tests: 3 Rust (mcp_config settings-injection) + 6 hook subprocess
    tests + 2 inbox-delivery tests. Full suites: daemon 947 / TUI 688
    / accept_loop 5 green; mcp 191 tests with only the 2 pre-existing
    env failures (missing pytest module; google-libs logging).

## Deploy notes / follow-ups

- **cm-manager**: needs BOTH the new daemon binary AND an mcp_server
  rsync that includes the new `hooks/` dir (hook injection is
  fail-open until it lands). Existing long-lived orchestrator sessions
  keep running hook-less (PTY-on-idle delivery path) until their next
  respawn picks up `--settings`.
- **predictionTrading twin MCP server**
  (`scripts/mcp/claude_manager_server.py`) does not yet expose
  `monitor_sessions`/`list_monitors`/`cancel_monitor` or the
  `notify_on_done` params — port them if PT-side orchestrators should
  get async waits (the project_mcp_two_servers rule).
- **Orchestrator skills** (design-doc-impl-loop, parallel-impl-wave,
  continuous-task prompts) still describe cm-wait/blocking waits as
  the monitoring pattern — update them to "spawn/prompt, end turn,
  wait for the [cm-monitor] wake-up".
- The TUI-local `send_input` route (laptop: TUI drainer) keeps its
  existing echo-quiet + hard-deadline gating; the daemon-side
  typing-quiet gate covers headless/remote and continuous fires. If
  laptop-side mangling recurs, port `await_operator_quiet` into the
  TUI drainer as a follow-up.
- Full-stack live smoke (real claude worker + real monitor fire on the
  production daemon) needs the rebuilt daemon running — do after
  merge/restart: spawn a worker via `start_session(prompt=...)`, end
  turn, confirm the `[cm-monitor ...]` message arrives + `A-y`-style
  sanity on `~/.cm/inbox/` staying empty.
