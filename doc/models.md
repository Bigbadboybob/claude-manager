# Explicit model and effort

CM never leaves a session on its engine's own default model or reasoning
effort. Every Claude or Codex spawn names both in the argv (`claude --model
<id> --effort <level>`, `codex -m <id> -c model_reasoning_effort="<level>"`),
including fresh starts, resumes, `A-R` revive/restart, startup restore, forks,
continuous orchestrators and workers, workflow roles, and MCP
`start_session` / `create_subtask`. The daemon's `start_session` refuses an
agent spawn that names no model or no effort; a viewer older than the daemon
gets that refusal and must be updated. Bash sessions have neither.

Effort works exactly like the model: the same resolution order, recorded on
the session the same way, shown beside it, and set the same ways. The
sections below name the model; each applies to the effort too.

## Effort levels

| CM level | Claude (`--effort`) | Codex (`-c model_reasoning_effort=`) |
|---|---|---|
| `low` | `low` | `low` |
| `medium` | `medium` | `medium` |
| `high` | `high` | `high` |
| `xhigh` (default) | `xhigh` | `xhigh` |
| `max` | `max` | refused: codex has no `max` |

Levels are case-insensitive on input and stored lowercase. A Codex spawn also
carries the level to the CM-owned app-server: `native_codex` passes the `-c`
override to both the frontend and the backend and sets
`config.model_reasoning_effort` on every thread start, resume and fork, so a
thread never falls back to `~/.codex/config.toml`'s `model_reasoning_effort`
(per-spawn flags override that file, which stays as it is).

## Defaults: `[models]`

The engine defaults and the presets offered by the TUI live in a `[models]`
table, in `~/.cm/daemon.toml` (daemon-composed spawns: MCP, continuous,
workflows, revive) and in `~/.cm/tui-settings.toml` (spawns the TUI composes).
Keep the two in step on a host.

```toml
[models]
claude = "claude-opus-5-5"          # default for claude sessions (full id, not an alias)
codex = "gpt-6.1-sol"               # default for codex sessions (codex-lb)
claude_choices = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"]
codex_choices = ["gpt-6.1-sol", "gpt-6-luna", "gpt-6-astra"]
claude_effort = "xhigh"             # default effort for claude sessions
codex_effort = "xhigh"              # default effort for codex sessions (no "max")
```

Every value is checked when the config loads, so a bad entry fails the load,
not a later spawn. `gpt-6-luna` is the cheap Codex option.

## Resolution

1. **Explicit**: MCP `model=` / `effort=` on `start_session`, `fork_session`;
   a TUI form's edited Model or Effort.
2. **Task default**: `metadata.models.<claude|codex>` (effort:
   `metadata.efforts.<claude|codex>`) on the task, else on the nearest
   ancestor that sets one (subtasks inherit).
3. **Engine default** from `[models]`.

A respawn of an existing session (revive, resume, restore, fork, workflow
`fresh` activation, continuous re-fire) reuses the value recorded on the
session. A session that predates recording (adopted across a brain restart,
restored from an old manifest) takes what its transcript last ran on, else the
task or engine default, reported as `default_on_resume` because the
conversation may have run something else. In the transcript:

- Claude's assistant entries carry `requestedModel` (the id the session asked
  for, with any context suffix such as `claude-opus-5-5[1m]`), `message.model`
  (the bare id the API answered as) and `effort`. CM reads `requestedModel`
  and falls back to `message.model` for older transcripts; reading only
  `message.model` would resume a 1M session at the default context.
- Codex's `turn_context` carries `payload.model` and `payload.effort`.

Adopted sessions are backfilled once, before the brain serves, by up to 8
transcript readers (each reads a bounded tail). Measured on cm-sessions
against its 70 largest real transcripts (19 GB, up to 3.2 GB each): 0.3-1.2 s.

The task lookup is bounded: restore prefetches task defaults concurrently with
a 5 s budget (each request also times out at 3 s), then falls back to
`[models]`.

The resolved values are recorded on the session and shown in `list_sessions`
(`model` / `model_source`, `effort` / `effort_source`), the `A-i` peek and the
F6 info pane. The `A-i` peek on a task shows its resolved Models and Efforts
per engine. Sources:

| Source | Meaning |
|---|---|
| `explicit` | Named by the caller (`model=` / `effort=`, an edited form field). |
| `task` | The task's default, or its nearest ancestor's. |
| `recorded` | Recorded on the session being respawned. |
| `engine_default` | `[models]` config default. |
| `transcript` | Read from the conversation's transcript. |
| `default_on_resume` | A resume with nothing recorded or in the transcript: the task or config default, logged. |
| `task_invalid_fallback` | The task's default is not valid for this engine (a codex task default of `max` effort): the config default, logged. The form prefill skips such a value too. |
| `lookup_timeout` | The task default lookup missed the restore budget or timed out: the config default for now. Provisional: the next respawn looks the task up again instead of reusing it. |

## Setting a task's default

- MCP: `propose_task(model=, effort=, model_engine=)` and
  `create_subtask(model=, effort=, model_engine=)` store the defaults for one
  engine slot: `model_engine` if given, else the engine the model id names
  (`claude-*` → claude; `gpt-*`, `luna`, `sol`, `astra` → codex). An id that
  names no engine, or an effort without a model, needs `model_engine`; a
  model that contradicts `model_engine` is refused.
- API: `POST /tasks/{id}/model {"engine": "codex", "model": "gpt-6-luna",
  "effort": "medium"}` sets either or both; a field sent as `null` clears it
  and an absent field is left alone. `GET /tasks/{id}/model?engine=codex`
  returns the inherited values. Never PATCH `metadata` for this; PATCH
  replaces the whole bag.
- TUI: `A-e` on a task has Claude and Codex rows, each with its Effort
  (Shift+←/→). Empty means inherit, and the row shows what it inherits.
- Continuous tasks: `continuous.create` / `continuous.update` take `model` and
  `effort` (`""` clears). Workflow roles take optional `model` and `effort` in
  the TOML; `start_workflow` takes `role_models` / `role_efforts`.

## Cloud workers

Dispatch passes the task's own Claude model and effort to a worker VM as
instance metadata (`claude-model` / `claude-effort`, else `CM_WORKER_CLAUDE_MODEL` / `CM_WORKER_CLAUDE_EFFORT`, default `claude-opus-5-5` / `xhigh`),
and `worker/startup.sh` launches `claude --model … --effort …`; a resumed
worker keeps what its transcript ran on unless the task names a value. Warm
pool workers start Claude before a task is assigned, so they always run the
configured worker default, not the task's.

## TUI forms

`A-n`, `A-s`, `A-F` and the `A-f` launch dialogs have a Model row prefilled
with the task default, else the engine default (shown dimmed with its origin),
and the Effort on the same row. On the row, ←/→ cycle the model presets,
typing edits the model freely, and Shift+←/→ step the effort (no wrap); in
the `A-f` dialogs, Alt+o cycles the presets. An untouched prefill sends no
explicit value, so the spawn resolves it again on the host. A fork's row
starts from the source session's recorded values. A new subtask under
"Reports to" still picks up its parent's default.

## Validation and unknown models

Model strings are opaque. CM checks only that a string is non-empty, at most
128 characters, starts with a letter or digit, and uses only letters, digits
and `._-:/@+[]`, so it is safe in argv. CM does not check that the engine knows the model. An unknown
model starts the engine, and its first turn fails. `list_sessions` then
reports `agent_state="errored"` with the engine's error next to the `model`
field.
