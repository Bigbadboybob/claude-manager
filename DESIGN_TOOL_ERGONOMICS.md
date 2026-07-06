# Design — Agent-facing MCP tool ergonomics (learning from Claude Code's `Agent` tool)

**Status:** P0 ✅ + P1 (isolation flag + bash advisory) ✅ + P2 (field trim + tool-sprawl docstrings) ✅ IMPLEMENTED (branch `cm/improve-cm-ergonmoics`, 2026-07-06) — `mcp_server/` only, runs locally, no deploy. Deferred (need daemon infra or are deliberate no-ops): P1's push-instead-of-poll north star, auto-clean teardown, bash sentinel-capture, and P2's envelope-*uniformity* (a breaking change, not worth it — see that section). Written 2026-07-06 from a dogfooding pass: an agent (this session) exercised the read-only cm tools live plus one full write round-trip (`start_session` → `send_input_and_wait` → `read_last_turn` → `kill_session` on a throwaway bash session), and compared the experience against the Claude Code `Agent` / `Task*` / `SendMessage` / `Monitor` tools that ship in the harness today.

> **P0 completion note.** `start_session` now takes `wait=true` (one-call spawn-and-run: spawn → await the initial prompt's reply → return `last_message`, `session_uid` always present) and a `schema=` JSON-Schema option; `send_input_and_wait` gained the same `schema=`. The waiting core was extracted from `send_input_and_wait` into a shared `_await_reply` / `_send_and_await` so the transcript-anchoring race fix is closed in exactly one place. Schema validation uses `jsonschema` when installed (added to `requirements.txt`) and degrades to a built-in type+required check otherwise. Tests: `mcp_server/tests/test_spawn_and_run.py` (15) + the full suite (156) green. See the per-item annotations below.

Scope of the *changes* proposed here: `mcp_server/` (tool handlers) and the daemon control socket that backs them. No TUI or planning-board changes are required for P0/P1. Note the two-MCP-copy rule (memory `project_mcp_two_servers`): any new tool must be added to **both** `mcp_server/server.py` and `predictionTrading/scripts/mcp/claude_manager_server.py`.

## Motivation

Claude Code's `Agent` tool can now spawn a subagent **in its own git worktree** (`isolation: "worktree"`, auto-cleaned if unchanged) and returns the subagent's final message as the tool result in a single call. That is a good reference point: it's the same primitive cm's orchestration surface provides — spawn a worker, isolate it, drive it, collect its answer — but packaged as a clean request/response function instead of a terminal you poke. We can learn a lot from the delta.

## The core mismatch

Claude Code models a sub-agent as a **function**: `Agent(prompt)` → result. cm models it as a **long-lived terminal you poke**: spawn a PTY, write bytes, poll for idle, scrape the transcript tail, tear down.

cm's model is genuinely *more* powerful — sessions are persistent, visible in the TUI sidebar, survive daemon restarts, and are workflow-aware. That power is the whole point and we keep it. But the tool *surface* makes the common case ("spawn a worker, get its answer") multi-step and trap-laden, and it leaks PTY/timing mechanics the agent shouldn't have to reason about. The goal of this doc is to add a thin, function-shaped front door over the existing primitives — not to replace them.

## What the dogfooding pass found

### Gap table

| Concern | Claude Code | cm today | Verdict |
|---|---|---|---|
| Spawn + run + get result | `Agent(prompt, run_in_background)` → **1 call**, result returned inline | `start_session` → `wait_for_session_idle`/`send_input_and_wait` → `read_last_turn` → **3–4 calls** | ❌ no one-call spawn-and-run |
| Completion signal | push `<task-notification>`, harness **re-invokes** the orchestrator — it keeps working | **poll, or hold a blocking `wait_*` call open** up to 10–30 min | ❌ no async notify |
| Worktree isolation | `isolation:"worktree"` — **one boolean**, auto-cleaned if unchanged | `create_subtask(worktree_mode="branch")` **then** `start_session(task_id=)` — two concepts, manual merge + `mark_subtask_done` | ⚠️ heavier composition |
| Structured result | Workflow `schema` → validated JSON returned | none — parse a free-text transcript message | ❌ missing |
| Continue an agent | `SendMessage(id, msg)` | `send_input(uid, text)` | ✅ at par |
| Fan-out wait | Monitor / notifications | `wait_for_any_session_idle` (returns each `last_message` inline) | ✅ cm is good here |
| Identity / scope | implicit | `ping` (perms, task, workspace) | ✅ cm is richer (it needs to be) |
| Read a bash/terminal session | `Bash` tool → output inline | **no way** to read output back through MCP | ❌ dead-end |

### Concrete findings

1. **The common case costs 3–4 calls and exposes a race.** To "ask a worker something and get its answer" you `start_session`, then either `wait_for_session_idle` + `read_last_turn`, or `send_input_and_wait`. `send_input_and_wait`'s own docstring spends a paragraph on the "~2s busy window vs. first-token" race it exists to close — a race the `Agent` abstraction never surfaces because there is no "did I catch the previous turn's message" hazard in a function call.

2. **Bash sessions are a read dead-end.** In the live test I ran `echo ...` in a bash session; `send_input_and_wait` returned `completed:true, last_message:null`, and both `read_last_turn` and `read_session_output` returned `[]`. The command ran, but its stdout lives only in PTY scrollback that the agent-facing MCP never exposes. An agent will reasonably expect to read output and hit a silent wall with no error explaining why. (The intended use — "drive a terminal for the *user* to watch" — is real, but nothing tells the agent that.)

3. **The docstrings are doing heroic work papering over leaked mechanics.** `read_session_output` warns about the "page-forward and never reach the end" trap (which is the entire reason `read_last_turn` had to be added); `wait_for_any_session_idle` warns "don't pass a worker you JUST sent input to." Each is a footgun that a function-shaped API doesn't have.

4. **Inconsistent output envelopes.** `list_sessions` / `list_projects` return compact `{"result":[...]}`; `ping` / `get_current_task` / `list_sessions_grouped` return bare, pretty-printed objects. An agent parsing tool output has to special-case per tool.

5. **Doc drift / noise in `list_sessions`.** The docstring promises ~12 per-session fields; the actual payload has ~16 (adds `cols`, `rows`, `continuous_task_id`, `worktree_path`). `cols`/`rows` are pure noise for an orchestrator.

## Proposed changes, ranked by leverage

### P0 — one-call spawn-and-run  ✅ IMPLEMENTED

Add a `wait=true` mode to `start_session` (or a dedicated `spawn_worker` front door) that folds spawn → wait-for-idle → read-tail into one call and returns:

```json
{ "session_uid": "...", "status": "awaiting_input", "last_message": { "role": "assistant", "content": "...", "ts": "..." } }
```

This is the single biggest win — it makes cm's common case match `Agent()`. The existing primitives (`start_session` bare, `wait_for_session_idle`, `read_last_turn`) stay for advanced orchestration where you want to fire-and-forget or interleave. Internally this is just the sequence the orchestrator writes by hand today, moved server-side so the post-send race is closed in one place (reuse `send_input_and_wait`'s transcript-anchoring logic — record the transcript end before spawn, report complete only once a *new* assistant message appears and the session goes quiet).

Signature sketch (additive to `start_session`):

```
start_session(type, label, prompt, task_id=None, global_perms=False,
              wait=False, timeout_s=600, schema=None)
```

### P0 — structured output  ✅ IMPLEMENTED

Add a `schema` param to the spawn-and-run path (and to `send_input_and_wait`) that instructs the worker to emit JSON matching the schema and validates it at the tool layer before returning — exactly like Workflow's `schema` option. Return the parsed object as `result` alongside (or instead of) `last_message`. Orchestrators today parse free-text transcript tails; this is the difference between reliable and flaky fan-out. It composes with P0's `wait=true`.

### P1 — worktree isolation as a flag  ✅ IMPLEMENTED (spawn side)

Let the spawn path take `isolated=true` (or `worktree="branch"`) so "give this worker its own worktree" is one call instead of `create_subtask(worktree_mode="branch")` + `start_session(task_id=)` + a later manual merge + `mark_subtask_done`. Under the hood it still creates the branch-mode subtask and binds the session — but the agent sees one call. Mirror `Agent`'s "auto-clean if unchanged" on the teardown side: if the isolated worktree has no diff at close, remove it without ceremony.

> **Done:** `start_session(isolated=true)` composes `create_subtask(worktree_mode="branch")` + the spawn in one call (pure Python in `mcp_server/`, no daemon change — `create_subtask` already pushes the task-tree edge so the descendant-auth walk passes immediately). The result carries `task_id` (the branched subtask) + `worktree_path` so the orchestrator can merge and `mark_subtask_done`. A taskless caller gets a clear error (isolation needs a task to branch from), and an explicit `task_id` arg is ignored (the subtask is always a child of the caller's task). **Deferred:** the "auto-clean if unchanged" teardown — that lives in `mark_subtask_done`'s no-diff path (git-state logic), tracked as a P2-ish follow-up; today teardown stays the explicit `mark_subtask_done`.

### P1 — fix the bash / transcript-less read dead-end  ✅ IMPLEMENTED (advisory)

At minimum, make `send_input_and_wait` / `read_last_turn` / `read_session_output` return an **explicit** error or advisory for transcript-less sessions ("bash sessions have no transcript — redirect output to a file, or use type=claude-code") instead of a silent `null` / `[]`. Better: capture PTY output for bash by having `send_input_and_wait` wrap the command with a sentinel (`__cm_begin__` / `__cm_end_$rc__`) and return the captured span as `last_message.content`. That turns a bash session into a usable one-shot command runner for agents.

> **Done:** all four read/wait paths (`read_last_turn`, `read_session_output`, `send_input_and_wait`/`_await_reply`, and `start_session(wait=true)`) now attach a `note` field whenever they come back with no readable output because no transcript is bound — explaining the bash dead-end and pointing at "redirect to a file / use an agent type / poll again if it's a fresh agent". The note appears only when there's genuinely nothing to return (a bound agent read never carries it). **Deferred:** the "better" sentinel-capture — the daemon exposes **no** PTY-scrollback read method (confirmed: only transcript reads exist), so capturing bash stdout needs a new daemon+TUI control method to snapshot the terminal screen. That's real infra beyond a Python-only change; filed as a follow-up. `engine_str()` also can't distinguish bash from claude-code (deliberate wire-compat quirk — both report `"claude-code"`), so the advisory is honest about both the bash and the still-starting cases rather than asserting which.

### P1 — push instead of poll (north star)

The deepest fix. The daemon already emits `manifest.watch` broadcasts. If a "session X went idle / exited" event could **re-invoke the orchestrator agent** the way the harness re-invokes on `<task-notification>`, the entire `wait_for_*` blocking family — and the races bundled with it — could be retired for orchestrators that opt in. This is a bigger lift (it needs an agent-facing delivery channel, not just a TUI-facing one) and is filed here as direction, not a near-term slice.

### P2 — normalize envelopes & trim fields  ✅ IMPLEMENTED (trim + docstrings); envelope-uniformity deliberately skipped

Pick one response shape (all `{"result": ...}` or all bare — recommend bare object, since `{"result":[...]}` adds a layer for no gain), stop pretty-printing some and compacting others, drop `cols`/`rows` from `list_sessions`, and reconcile the docstring field list with the real payload.

> **Done:** `cols`/`rows` are dropped from the `list_sessions` / `list_sessions_grouped` projection (`_list_sessions_raw` in `mcp_server/`), and the `list_sessions` docstring now matches the real payload (added `continuous_task_id` + `worktree_path`, which were undocumented). **Deliberately NOT done — the envelope *uniformity*.** On investigation the split is not a bug to fix but a structural fact: the `{"result": [...]}` wrapping is FastMCP auto-wrapping any tool that returns a bare `list` (`list_sessions`, `list_projects`, `list_subtasks`, `list_tasks`, `list_workflows`); dict-returning tools (`ping`, `get_current_task`, `list_sessions_grouped`, …) serialize bare. Unifying it means either renaming that `result` key (breaks every skill/orchestrator that reads `["result"]` — `design-doc-impl-loop` and `parallel-impl-wave` both do) or wrapping every dict tool too (breaks the rest). The layer is cosmetic and the churn is a real breaking change, so the right call is to **document** the split (now noted in the `list_sessions` docstring) rather than churn it. The pretty-vs-compact difference is harness-side rendering of the tool result, not a wire difference — nothing to normalize. Net: fix the genuine noise (fields + doc drift), leave the load-bearing shape alone.

### P2 — consolidate the wait/read sprawl  ✅ IMPLEMENTED (docstrings)

Six overlapping tools carry race caveats: `read_session_output`, `read_last_turn`, `send_input`, `send_input_and_wait`, `wait_for_session_idle`, `wait_for_any_session_idle`. Present a small high-level front door — spawn-and-run (P0), `send_input_and_wait` (continue-and-get-reply), `wait_for_any_session_idle` (wait-any) — and mark the rest "advanced primitives" in their docstrings so an agent reaches for the right three first.

> **Done:** `read_session_output` and `wait_for_session_idle` now open with a "Low-level primitive — prefer \<front-door tool\> for the common case" lead-in (`read_session_output` → `read_last_turn`; `wait_for_session_idle` → `start_session(wait=true)` / `send_input_and_wait` / `wait_for_any_session_idle`). `send_input` already points at `send_input_and_wait`. So an agent scanning tool docs meets the three front-door tools first and the primitives clearly labelled as advanced. (Pure docstring changes — no wire/behavior change.)

## What cm already gets right (keep it)

- **`ping` self-identity** — perms/task/workspace in one call. No Claude Code analog; cm needs it because sessions are long-lived and cross-referenced.
- **`wait_for_any_session_idle`** with inline `last_message` — a genuinely good fan-out primitive, ahead of hand-rolling a poll loop.
- **`send_input_and_wait` and `read_last_turn` existing at all** — they show the pain was already felt and fixed pointwise. The work here is to make them (and the new spawn-and-run) the *default* path rather than the workaround an agent discovers after hitting the trap.

## Open questions

1. **`wait=true` and long turns.** A worker that takes >`timeout_s` to first-token returns `timed_out` with no message. Do we want the spawn-and-run to also return the `session_uid` on timeout (yes — so the orchestrator can fall back to polling the still-alive session)? Proposed: always return `session_uid`, with `completed`/`timed_out` flags mirroring `send_input_and_wait`.
2. **Schema enforcement engine.** Reuse Workflow's schema-validation path, or a lighter JSON-Schema check in the MCP layer? The worker is a free-running Claude/Codex, not a tool-call, so enforcement is "re-prompt on mismatch" not "reject at the tool boundary" — needs a retry budget.
3. **Isolation cleanup semantics.** `Agent` auto-cleans an unchanged worktree. cm subtasks preserve the branch ref for merge history (`mark_subtask_done` keeps the ref). Reconcile: auto-remove the *worktree* on a no-diff close, but keep the branch-ref-preservation contract.
4. **Push-notification delivery.** What is the agent-facing channel for a "session idle" wake-up? The harness re-invokes on `<task-notification>`; cm has no equivalent inbound path to a running agent today. This gates P1-push and may be out of scope for an MCP-only change.
