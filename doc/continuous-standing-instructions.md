# Continuous tasks: standing instructions vs. the per-fire dispatch

Owner question, 2026-09-14: the periodic orchestrator prompt is very long; is
re-pasting it every fire the only way to keep it in context?

## The problem it fixes

Until 2026-09-14 every fire pasted the whole orchestrator prompt (16–40 KB) into
the session's PTY. With `run_mode = persistent` and `compact_every = 16`, a
session carried up to sixteen copies of the same text between compactions, and
every chat wake ran with whatever copy had survived compaction. The repeated
paste was there because the prompt would otherwise fall out of context after a
`/compact`.

## What CM does now

A continuous task has two prompt parts:

| Field | Purpose | Size | Delivered how |
|---|---|---|---|
| `standing_instructions` | Lane, GATE, lifecycle, the step-by-step cycle procedure, the task-channel contract | 15–40 KB | Written once into the worktree as the engine's own project-instruction file; the engine loads it into its prompt prefix |
| `default_prompt` | The per-fire dispatch ("run one cycle now", where the standing instructions are, close with `report_done`) | ~0.6 KB | Pasted at each fire, wrapped in the run-identity header |

`continuous::instructions::materialize` writes the file before every fire, at
`continuous.create`, and whenever `continuous.update` changes
`standing_instructions`. The file is rewritten only when its content changes,
starts with the marker `<!-- cm-standing-instructions v1 -->`, is added to the
checkout's `.git/info/exclude`, and is never written over a file that lacks the
marker (a user's own file at that path makes the fire log an error and proceed).

Engine mapping, verified on the pinned builds:

- **Codex 0.153.4** reads `AGENTS.override.md`, which *replaces* `AGENTS.md`
  for that checkout, so the file carries the repository's own doc (followed
  through the worktree's `AGENTS.md`, usually a symlink to `CLAUDE.md`) and then
  the standing section. Codex injects project docs on a thread's first turn and
  again on the first turn after every `compacted` event (checked in the
  health-alert-triage rollout: each compaction was followed by a fresh 33 KB
  injection). CM now spawns Codex with `-c project_doc_max_bytes=262144`; the
  32 KiB default already truncated predictionTrading's 33.7 KB `CLAUDE.md`.
- **Claude Code 2.1.270** reads `CLAUDE.local.md` in addition to `CLAUDE.md`,
  so only the standing section is written.
- **bash** tasks have no file; their prompt is the command line.

The dispatch names the file and tells a thread that predates it to read the
file first. Chat wakes and monitor notices are not fires: the standing text says
so explicitly, so a woken orchestrator handles the message without starting a
cycle.

## Operating

- `continuous.create {…, standing_instructions, default_prompt}` — supply both;
  with `standing_instructions` set, keep `default_prompt` to the dispatch.
- `continuous.update {task_id, standing_instructions}` — replaces the standing
  text and rewrites the file immediately; an empty string clears it and removes
  the CM-written file. Applies to the next fresh thread or the first turn after
  the next compaction; a live persistent thread keeps the copy it has until then.
- `continuous.list` reports `standing_instructions_bytes`, `instructions_file`
  and `default_prompt_bytes`.
- `scripts/migrate_standing_instructions.py --host <host>` moves an existing
  task's whole prompt into `standing_instructions` and installs the dispatch
  (idempotent; backups under `~/.cm/audits/standing-instructions-<stamp>/`).
- Workers spawned into the orchestrator's own checkout see the file too; the
  standing section tells a non-orchestrator to ignore it.

## Verification recipe

After a migration: `git -C <worktree> status --short` is clean, the file starts
with the marker, `continuous.list[].instructions_file` names it, and the next
fire's transcript shows the short dispatch (`# Current CM continuous run …
section of \`AGENTS.override.md\``) followed by a normal cycle. On a fresh Codex
thread the first turn's `# AGENTS.md instructions for <worktree>` user message
contains the standing section.
