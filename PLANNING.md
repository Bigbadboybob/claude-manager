# Claude Manager — Project Planning

## Overview

A lightweight project planning system built into the TUI. Tasks are markdown files with YAML frontmatter. You manage a prioritized backlog per project, edit tasks with vim in an embedded terminal, and launch them as claude-manager sessions (local or cloud) with one shortcut.

Coming from: Google Docs checklists. Goal: keep that simplicity but add structure (status, difficulty, dependencies, ordering) and tight integration with task dispatch.

## Task Format

Each task is a `.md` file. Title is the only required field for a draft.

```markdown
---
title: Fix flaky test in test_calibration.py
status: backlog
difficulty: 3
depends: [fix-db-pooling]
created: 2026-04-01
---

## Description

The test fails intermittently with a timeout on the DB connection.
Might be a connection pool issue or test isolation problem.

## Prompt

Fix the flaky test in test_calibration.py. It fails intermittently
with a timeout error on the DB connection. Check whether it's a
connection pool issue or a test isolation problem. Write a fix and
verify it passes 50 consecutive runs.
```

### Fields

| Field | Required | Default | Description |
|---|---|---|---|
| `title` | yes | — | Short task name |
| `status` | no | `draft` | `draft`, `backlog`, `in_progress`, `done` |
| `difficulty` | no | — | 1–10 scale |
| `depends` | no | `[]` | List of task slugs this task depends on |
| `created` | auto | today | Set on creation |

### Slug

The filename stem is the task slug: `fix-flaky-test.md` → slug `fix-flaky-test`. Slugs are used in `depends`, `order.json`, and all references. Auto-generated from title on creation (lowercase, hyphens, truncated to ~50 chars). User can override.

### Prompt

The `## Prompt` section in the markdown body is what gets sent to Claude Code on launch. If absent, the title is used as the prompt. This keeps prompts co-located with the task description and editable with vim like everything else.

## Storage

```
~/.cm/projects/
  predictionTrading/
    tasks/
      fix-flaky-test.md
      add-retry-logic.md
      refactor-alpha-models.md
    order.json
  anotherProject/
    tasks/
      ...
    order.json
```

### order.json

Manual ordering within each status group. Flat list of slugs:

```json
["fix-flaky-test", "add-retry-logic", "refactor-alpha-models"]
```

Tasks not in the list are appended at the bottom of their status group. When a task changes status, it keeps its relative position within the new group (appended at bottom of that group by default, user can reorder).

## Status Model

```
draft → backlog → in_progress → done
```

- **draft**: Idea captured, not ready to work on. Won't be launched.
- **backlog**: Ready to work on. Can be launched.
- **in_progress**: Actively being worked on (has a running session or is being worked manually).
- **done**: Completed.

Statuses can move in any direction (e.g. done → backlog to reopen).

## TUI Layout

```
┌─ predictionTrading ──────────────────────────────────────────────────┐
│ Tasks              │                                                  │
│                    │  Fix flaky test in test_calibration.py           │
│  done              │                                                  │
│  ✓ Clean up types  │  Status: backlog    Difficulty: 3               │
│  ✓ Update deps     │  Depends: fix-db-pooling                        │
│                    │  Created: 2026-04-01                             │
│  in_progress       │                                                  │
│  ◉ Fix auth flow   │  ## Description                                  │
│                    │                                                  │
│  backlog           │  The test fails intermittently with a timeout    │
│ ▶ Fix flaky test   │  on the DB connection. Might be a connection     │
│  Add retry logic   │  pool issue or test isolation problem.           │
│  Refactor alpha    │                                                  │
│                    │  ## Prompt                                        │
│  draft             │                                                  │
│  ○ Audit logging   │  Fix the flaky test in test_calibration.py...    │
│  ○ Scraper for X   │                                                  │
│                    │                                                  │
├────────────────────┤                                                  │
│ A-j/k nav          │                                                  │
│ A-e  edit (vim)    │                                                  │
│ A-n  new task      │                                                  │
│ A-l  launch        │                                                  │
│ A-p  project       │                                                  │
│ A-s  status cycle  │                                                  │
│ A-1..4 filter      │                                                  │
└────────────────────┴──────────────────────────────────────────────────┘
```

### Default View

Tasks sorted by status in this order: **done, in_progress, backlog, draft**. Within each status group, tasks are in the manual order from `order.json`. Status group headers are shown as labels.

**Autofocus**: On open, the cursor lands at the junction between in_progress and backlog — the first backlog item. This puts active work visible above and upcoming work below. If no in_progress tasks exist, focus the first backlog item. If no backlog, focus first draft.

### Main Panel

Two modes:

1. **Detail view** (default): Renders the task's markdown as formatted text. Shows frontmatter fields as a header, then the markdown body below.

2. **Edit mode** (Alt+e): Spawns vim in an embedded alacritty terminal (same approach as CM sessions) editing the task's `.md` file. On vim exit, the TUI re-parses the file and returns to detail view. All keyboard input goes to vim while it's active — Alt shortcuts still work for navigation since they're captured by the TUI layer.

### Sidebar

The ordered task list. Each entry shows a status indicator and the task title (truncated if needed):

- `✓` done
- `◉` in_progress
- ` ` backlog (no indicator, clean)
- `○` draft

Selected task is highlighted. Arrow keys / Alt+j/k to navigate.

### Reordering

Alt+J / Alt+K (shift) to move the selected task up/down within its status group in `order.json`. Moving across status boundaries changes the task's status.

## Keyboard Shortcuts

| Shortcut | Action |
|---|---|
| Alt+j / Alt+k | Navigate task list |
| Alt+J / Alt+K | Reorder task (move up/down) |
| Alt+e | Edit task in vim |
| Alt+n | New task (creates draft, opens in vim) |
| Alt+s | Cycle status forward (draft→backlog→in_progress→done) |
| Alt+S | Cycle status backward |
| Alt+l | Launch task as claude-manager session |
| Alt+d | Delete task (with confirmation) |
| Alt+p | Switch project |
| Alt+1 | Filter: show all |
| Alt+2 | Filter: hide done |
| Alt+3 | Filter: backlog + in_progress only |
| Alt+4 | Filter: in_progress only |

## Launch Integration

Alt+l on a task triggers the launch flow:

1. Extract prompt from the `## Prompt` section (or title if absent).
2. Choose target: **local** (spawns in TUI as a CM session) or **cloud** (dispatches to worker VM).
3. **Autostart** behavior:
   - **Cloud (default: on)**: Prompt is entered and submitted automatically. Claude starts working.
   - **Local (default: off)**: Prompt is typed into Claude Code's input character-by-character (not pasted as a block) but NOT submitted. User sees the prefilled prompt and can edit before pressing Enter.
4. Task status moves to `in_progress`.

The session is linked back to the task — the task detail view can show session status (running, blocked, done).

### Prefill Implementation

For local non-autostart launches, write the prompt bytes to the PTY without a trailing `\n`. Characters appear in Claude Code's input line. User reviews, edits if needed, presses Enter to start.

## Project Switching

Alt+p opens a project selector (simple list popup or cycle). Each project is a directory under `~/.cm/projects/`. The TUI remembers which project was last open (stored in `~/.cm/last_project`).

Projects are auto-discovered from `~/.cm/projects/` directories that contain a `tasks/` subdirectory.

## Creating a New Task

Alt+n:
1. Prompt for a title (inline in the sidebar or a small input box).
2. Generate slug from title.
3. Create `~/.cm/projects/<project>/tasks/<slug>.md` with minimal frontmatter:
   ```markdown
   ---
   title: <title>
   status: draft
   created: 2026-04-08
   ---
   ```
4. Append slug to `order.json`.
5. Open the new file in vim (edit mode) so user can add description, prompt, etc.

## Dependencies

Dependencies are informational. The sidebar can show a visual indicator when a task's dependencies aren't all `done` (e.g. dim the title or show a lock icon). No hard enforcement — you can still launch a task with unmet deps.

## Search

Alt+/ opens a search prompt. Full-text search across all task files in the current project (titles, descriptions, prompts). Results replace the sidebar list — navigate to a result and press Enter/Esc to jump to it or cancel. Implementation: just grep the `tasks/` directory.

## Decisions

- **Editor**: Respect `$EDITOR`, fall back to `vim`.
- **Task archival**: Not needed initially. Done tasks stay in the list — the autofocus at the backlog boundary keeps them out of the way. Revisit if volume becomes an issue.
- **Multi-select**: Not needed.
