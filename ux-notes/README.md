# ux-notes

Inbox for UX friction notes about claude-manager — written by agents (or the operator) right after a session that surfaced the friction, while it's fresh. Triage happens later via the `/triage-ux-notes` skill.

## Layout

- `ux-notes/*.md` — **inbox**: untriaged notes. New notes always land here.
- `done/` — every item in the note is resolved: implemented, verified already fixed, declined with a reason, or handed off to a tracked planning task. This doubles as the archive.
- `partial/` — some items resolved; the rest consciously deferred without a tracking task. The note's triage log says what remains.
- `blocked/` — nothing could move: every actionable item is waiting on a decision, a dependency, or an in-flight branch. The triage log names the unblocker.

A note's folder tells you its state at a glance; the `## Triage log` appended to the note tells you what happened to each item and where.

## Writing a note

- Filename: `YYYY-MM-DD-<slug>.md`.
- Open with a context paragraph: what session/work produced the note, from whose perspective.
- Numbered items, most severe first. Each item: the observed behavior (concretely — what you called, what happened) and a concrete suggestion.
- Optional closing section: what's working well (keep) — it protects good behavior from being "fixed".

## Triage

Run `/triage-ux-notes` (see `.claude/skills/triage-ux-notes/SKILL.md`). It verifies each claim against current code, gets operator sign-off on a per-item plan, implements what's implementable, appends a `## Triage log` to the note, and moves it to the folder matching the outcome. `partial/` and `blocked/` notes get revisited with `/triage-ux-notes all`.
