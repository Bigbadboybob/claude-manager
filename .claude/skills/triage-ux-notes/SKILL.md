---
name: triage-ux-notes
description: Triage the ux-notes/ inbox — verify each note's claims against current code, implement the improvements that fit, hand off or decline the rest, append a triage log, and move the note to done/, partial/, or blocked/. Triggers when the user asks to "triage ux notes", "process the ux-notes inbox", or runs /triage-ux-notes [all | <note-path>].
---

# Triage UX notes

`ux-notes/` is an inbox of UX friction notes written by agents right after the session that surfaced the friction. Triaging a note means turning every item in it into one of five explicit dispositions — implemented, already-fixed, task-filed, declined, or blocked — then filing the note where its aggregate state says it belongs. A note never leaves the inbox with an item unaccounted for.

Folder semantics (see also `ux-notes/README.md`):

| Folder | Meaning |
|---|---|
| `ux-notes/*.md` | Inbox — untriaged |
| `done/` | Every item ∈ {implemented, already-fixed, declined, task-filed}. Doubles as the archive. |
| `partial/` | Some items resolved; the rest deferred without a tracking task |
| `blocked/` | Nothing implemented; ≥1 item waiting on a named decision / dependency / in-flight branch |

## Phase 0 — Scope

- Default: the inbox, oldest note first, **one note fully through Phases 1–4 before starting the next**.
- `/triage-ux-notes all`: also revisit `partial/` and `blocked/` (their logs say what was left — check whether the world changed).
- `/triage-ux-notes <path>`: just that note.
- Read the note and enumerate discrete items. A numbered section can hide several distinct asks (e.g. "refuse loudly AND return worktree_path") — split them. Each bullet in a "smaller frictions" section is its own item. Give items stable ids (1, 2a, 2b, …) — the triage log and the operator conversation both key off them.

## Phase 1 — Verify before believing

Notes describe the code as of their date; this repo moves fast and notes go stale. For each item, check the claim against **current source** before planning anything:

- Primary surfaces: `mcp_server/server.py`, `daemon/src/control/` (dispatcher, auth, mcp handlers), session-spawn paths, `tui/src/control/`. Fan out an Explore agent for wide notes rather than reading serially.
- An item whose complaint no longer reproduces is `already-fixed` — record the file:line evidence in the log.
- Check the unmerged landscape too (planning backlog, `cm/*` branches, NOTES.md files, memory): a fix that exists but isn't merged makes the item `blocked (waiting on <branch/task>)`, not a reimplement.

## Phase 2 — Classify and gate

For each item propose exactly one disposition:

- **implement** — mechanical-to-moderate, contained surface, verifiable this session.
- **task-filed** — real but design-heavy or cross-cutting: file it via `propose_task` with the note path + item id in the description. The backlog owns it now; for the note it counts as resolved.
- **declined** — we choose not to do it; the log records why.
- **blocked** — can't act and can't decide alone: name what unblocks it.
- **already-fixed** — from Phase 1, with evidence.

**Gate: present the per-item table (id → claim status → proposed disposition → one-line rationale) to the operator and get confirmation before implementing anything or calling `propose_task`.** This is the Phase-7 convention; sizing calls and declines are judgment calls the operator may override.

## Phase 3 — Implement

- **Scale is the norm, not the exception.** A typical note approves several independent items; implement them as a multi-agent pipeline, not serially inline: group items into file-surface-disjoint clusters, fan each cluster out to an implementation agent in an **isolated worktree** (each commits on its own temp branch as transport), merge the branches back conflict-free-or-resolve, then run an **adversarial review wave** over the combined diff (per-cluster spec-compliance lenses + merge-seams + compat, refute-by-default verification) and fix what survives. The 2026-08-11 inaugural run caught a high-severity injection bug this way that four implementers and clean tests all missed. Only drop to inline implementation for a note with one or two trivial items.
- Work on the current worktree branch. **Leave all changes unstaged; never commit** — the operator reviews and commits. (Temp-branch commits inside throwaway agent worktrees are fine — they're transport, and the final merge gets reset back to unstaged.)
- Repo gotchas that bite exactly this kind of change:
  - Tool-surface changes must land in **both MCP server copies**: `mcp_server/server.py` AND predictionTrading's `scripts/mcp/claude_manager_server.py`.
  - Paired auth paths drift: `daemon/src/control/auth.rs::check_session_caller` vs `tui/src/control/methods.rs::caller_authorized_for`.
  - New fields on manifest/`state.json`/`daemon.toml` structs need `#[serde(default)]` or old files stop deserializing.
- `cargo build --workspace` clean + targeted tests for every implemented item. Python side has no linter wired — read your own diff.
- Track activation lag for the report: daemon changes need a daemon restart; MCP-server changes take effect on the next MCP spawn; TUI changes on next TUI build/launch.

## Phase 4 — Record and file

1. Append a triage log to the note (or a new dated subsection under the existing log on re-triage):

   ```markdown
   ---

   ## Triage log

   ### 2026-08-10 — cm/ux-notes-triage
   | # | Item | Disposition | Where / why |
   |---|------|-------------|-------------|
   | 1 | start_session mints workspace | implemented | daemon/src/control/mcp.rs:412 (unstaged) |
   | 2a | create_subtask base param | task-filed | planning task 9f3c21ab |
   | 2b | return base sha | implemented | mcp_server/server.py:880 |
   | 3 | monitor mode="final" | blocked | waiting on cm/cm-monitor merge |
   | 4 | status="reported" | declined | convention suffices; revisit if it recurs |
   ```

2. Pick the folder by the rules in the table up top. Precedence when mixed: any unresolved item without a tracking task → `partial/`; all-unresolved-and-waiting → `blocked/`; otherwise `done/`.
3. Move with plain `mv` (not `git mv` — everything stays unstaged; git's rename detection pairs it at commit time since the note body is unchanged apart from the appended log).
4. Report to the operator: per-item outcomes, what needs a restart to take effect, and anything filed to the backlog.

Edge cases: a note with no actionable items (pure praise / observations) gets a one-line triage log and goes straight to `done/`. If the operator rejects the whole plan at the gate, the note stays in the inbox untouched — the gate conversation is not a triage log.
