# Claude Manager — UX Ideas

Future UX improvements beyond the core CLI. Build these after the basic `cm add` / `cm queue` / `cm open` / `cm done` flow is working.

## Interactive Mode (`cm`)

Running `cm` with no arguments drops you into an interactive TUI (terminal UI). This is the primary way to manage your Claudes day-to-day.

### Dashboard View

```
┌─ Claude Manager ──────────────────────────────────────────────────┐
│                                                                    │
│  Blocked (2)         Running (3)           Backlog (5)            │
│  ● Fix flaky test    ◉ Refactor parser     ○ Add caching layer   │
│  ● Add retry logic   ◉ Update deps         ○ Write migration     │
│                      ◉ Fix auth flow        ○ Audit logging       │
│                                             ○ Clean up types      │
│                                             ○ Scraper for X       │
│                                                                    │
│  [Tab] switch column  [Enter] open  [a] add  [d] done  [?] help  │
└────────────────────────────────────────────────────────────────────┘
```

Three columns. Blocked is on the left because that's what needs you. Tasks move right-to-left naturally: backlog → running → blocked. You work left-to-right through the blocked column.

Notifications pop up inline when a task moves to blocked:

```
  ⏎ "Fix flaky test" is now blocked (was running for 8m)
```

### Task Focus View

Press Enter on any task to see more detail:

```
┌─ Fix flaky test in test_calibration.py ───────────────────────────┐
│                                                                    │
│  Status: blocked (3m)    Repo: predictionTrading    VM: worker-a1 │
│                                                                    │
│  Prompt:                                                           │
│  Fix the flaky test in test_calibration.py. It fails              │
│  intermittently with a timeout error on the DB connection.         │
│                                                                    │
│  [o] open terminal   [d] done   [c] cancel   [Esc] back          │
└────────────────────────────────────────────────────────────────────┘
```

Pressing `o` opens the ttyd session — either in a browser tab or embedded in the terminal if we can figure that out.

### Workflow

The intended flow in interactive mode:

1. You open `cm`
2. See 2 tasks in blocked column
3. Arrow to the first one, press Enter → see what it is
4. Press `o` → browser opens with the Claude session
5. You read what Claude did, answer its question, Claude resumes
6. Close the browser tab, back to `cm`
7. That task moves back to "running" in real-time
8. Move to the next blocked task, repeat
9. Meanwhile, a notification pops up: another task just finished and moved to blocked
10. Deal with it when you're ready

## Task Backlog — Smooth Kick-off UX

Adding tasks should be frictionless. Several entry points:

### Quick add (inline)

```
cm add "Fix the flaky test in test_calibration" --repo predictionTrading
```

One line, repo defaults to a configured default if you only work on one repo.

### Batch add from a file

Write a simple text file:

```
# tasks.txt
Fix the flaky test in test_calibration.py
Add retry logic to the scraper pipeline
Refactor the alpha model base class to use async
```

```
cm add --file tasks.txt --repo predictionTrading
```

All three go into the backlog.

### Add from interactive mode

Press `a` in the dashboard:

```
┌─ Add Task ────────────────────────────────────────────────────────┐
│                                                                    │
│  Repo: predictionTrading                                          │
│  Prompt: _                                                         │
│                                                                    │
│  (multiline — press Ctrl+D to submit, Esc to cancel)              │
└────────────────────────────────────────────────────────────────────┘
```

### Add from spec files

For bigger tasks, write a markdown spec and reference it:

```
cm add --repo predictionTrading --prompt-file specs/refactor-alpha-models.md
```

### TODO list integration

The backlog IS the todo list. No separate system. You manage your Claude work in one place:

```
cm backlog

  #  Pri  Task                                          Repo
  1  0    Fix flaky test in test_calibration.py          predictionTrading
  2  0    Add retry logic to scraper pipeline            predictionTrading
  3  1    Refactor alpha model base class                predictionTrading
  4  1    Write migration for new schema                 predictionTrading
  5  2    Audit logging coverage                         predictionTrading

cm reorder 5 --priority 0    # bump audit logging to top priority
```

In interactive mode, you can reorder by dragging (j/k to move selection, J/K to move the task up/down in priority).

Tasks can also be `draft` status — ideas you haven't fleshed out yet. They sit in the backlog but won't be dispatched until you mark them ready:

```
cm add --draft "Something about improving the calibration model"
cm edit 6 --prompt "Improve calibration model by adding cross-validation..."
cm ready 6    # now it can be dispatched
```

## Notifications

When a task moves to blocked, you should know about it without staring at the dashboard.

Options (progressive):
1. **Terminal bell** — `cm` interactive mode rings the bell when a task blocks
2. **Desktop notification** — if running in a desktop environment
3. **Telegram** — push notification to your phone (you already have a bot set up)
4. **Sound** — audible chime

## Mobile

The portal (Phase 5) needs to be phone-friendly. The core mobile flow:

1. Get a push notification: "Fix flaky test is blocked"
2. Open the portal on your phone
3. See the blocked queue
4. Tap a task → see what Claude needs
5. Tap "Open terminal" → ttyd works on mobile browsers
6. Type a response on your phone keyboard (or voice-to-text)
7. Claude resumes, you close the tab

ttyd already works on mobile browsers, so this mostly comes down to the portal being responsive.

## Voice (Future)

STT for task creation:
- "Hey add a task to fix the flaky test in test calibration for the trading repo"
- Parsed into: `cm add --repo predictionTrading --prompt "Fix the flaky test in test_calibration.py"`

TTS for queue status:
- "What's blocked?" → "Two tasks are blocked. Fix flaky test, waiting 3 minutes. Add retry logic, waiting 12 minutes."
