# Explicit model choice

CM never leaves a session on its engine's own default model. Every Claude or
Codex spawn names its model in the argv (`claude --model <id>`, `codex -m <id>`),
including fresh starts, resumes, `A-R` revive/restart, startup restore, forks,
continuous orchestrators and workers, workflow roles, and MCP
`start_session` / `create_subtask`. The daemon's `start_session` refuses an
agent spawn that names no model; a viewer older than the daemon gets that
refusal and must be updated. Bash sessions have no model.

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
```

Every value is checked when the config loads, so a bad entry fails the load,
not a later spawn. `gpt-6-luna` is the cheap Codex option.

## Resolution

1. **Explicit**: MCP `model=` on `start_session`, `fork_session`; a TUI form's
   edited Model field.
2. **Task default**: `metadata.models.<claude|codex>` on the task, else on the
   nearest ancestor that sets one (subtasks inherit).
3. **Engine default** from `[models]`.

A respawn of an existing session (revive, resume, restore, workflow `fresh`
activation, continuous re-fire) reuses the model recorded on the session.
Sessions adopted across a brain restart that predate recording re-resolve
through the same order.

The resolved model is recorded on the session and shown in `list_sessions`
(`model`, plus `model_source` = `explicit` / `task` / `recorded` /
`engine_default` on spawn results), the `A-i` peek and the F6 info pane.

## Setting a task's default

- MCP: `propose_task(model=..., model_engine=...)` and
  `create_subtask(model=..., model_engine=...)` store the default for one
  engine (`model_engine` defaults to the caller's own engine).
- API: `POST /tasks/{id}/model {"engine": "codex", "model": "gpt-6-luna"}`;
  `"model": null` clears it. `GET /tasks/{id}/model?engine=codex` returns the
  inherited value. Never PATCH `metadata` for this; PATCH replaces the whole bag.
- TUI: `A-e` on a task has Claude and Codex rows. Empty means inherit, and the
  row shows what it inherits.
- Continuous tasks: `continuous.create` / `continuous.update` take `model`
  (`""` clears it). Workflow roles take an optional `model` in the TOML.

## TUI forms

`A-n`, `A-s`, `A-F` and the `A-f` launch dialogs have a Model row prefilled
with the task default, else the engine default (shown dimmed with its origin).
On the row, ←/→ cycle the presets and typing edits freely; in the `A-f`
dialogs, Alt+o cycles the presets. An untouched prefill sends no explicit
model, so the spawn resolves it again on the host. A new subtask under
"Reports to" still picks up its parent's default.

## Validation and unknown models

Model strings are opaque. CM checks only that a string is non-empty, at most
128 characters, starts with a letter or digit, and uses only letters, digits
and `._-:/@+[]`, so it is safe in argv. CM does not check that the engine knows the model. An unknown
model starts the engine, and its first turn fails. `list_sessions` then
reports `agent_state="errored"` with the engine's error next to the `model`
field.
