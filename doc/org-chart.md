# Org chart (F6)

F6 opens a full-screen org chart of every agent session on every host. It
shows reporting lines only; nothing on it grants or implies approval rights.
F6, Esc or `q` closes it and, like Enter, returns to the Sessions view with
the chart's focused session selected.

## Layout

- **Owner** is at the top. Its reports are the **projects** (the task rows'
  `project`, by name), then **Other** for tasks with no project. A subtask
  without a project takes its nearest ancestor's. Other appears only when
  something is in it.
- Each project splits in two:
  - **Teams**, the normal work: one box per **division** (an initiative whose
    `kind` is `division`), then the project's other root tasks, then a box
    for each workspace whose sessions have no task. Such a workspace joins
    the project its repository is named after, else Other.
  - **Continuous**, only when the project has any: one box per continuous
    orchestrator. Its workers sit under it, grouped exactly as in the
    continuous column (`A-c`).
  The split labels are constants at the top of `orgchart.rs`
  (`TEAMS_LABEL`, `CONTINUOUS_LABEL`, `OTHER_PROJECT_LABEL`).
- Under each split or division come the root tasks, with subtasks nested along
  `parent_task_id` at any depth. If a task in the chain has no session of its
  own, it still gets a box so the chain stays connected. A subtask of a
  continuous worker nests under that worker.
- A task box lists the task's sessions. The lead session comes first; it is
  chosen by the shared lead rule (`task_leads`). The other sessions follow in
  spawn order.
- **Leader marks.** Only the lead of a task with live reports carries a
  mark, and the mark gives its level below Owner: ★ for a root task
  (level 1), ☆ for its subtask (level 2), ✦ for level 3, and ✧ for level 4
  and deeper. A level counts the task's `parent_task_id` chain. A live report
  is a child task that is not done or archived and has a live agent
  (claude/codex) session somewhere in its subtree; a sessionless, bash-only
  or finished subtask doesn't count, and neither does a worker with no task
  of its own. ◆ stays reserved for divisions. The chart draws exactly the
  Sessions sidebar's marks: both come from `App::leader_marks` over one
  `OrgIndex` (`app/tree.rs`).
- Each session line shows the sidebar's state glyph (spinner, afterglow,
  settled or stale ●, ◐, ?, ✗, alert, owner-blocked, ⟳; × when exited), then
  the label and, at the right edge, three one-cell symbols (see Symbols): the
  mark of the board item it holds, its engine, and its host.
- Bash sessions are hidden by default; `b` shows them. Opening the chart on a
  bash session shows just that one, until the chart closes.
- Every level is laid out in one row, left to right, however many reports a
  box has; the chart grows as wide as it needs and the viewport pans to it.

## Keys

| Key | Action |
|---|---|
| `h` / `l` | previous / next sibling |
| `j` / `k` | down to the report last focused under this box (else the first) / up to the boss. Each box also remembers its last-focused session, so `k` then `j` lands where you were |
| `gg` / `G` | Owner / last box |
| `Tab` / `S-Tab` | next / previous session in the box |
| `/` `n` `N` | search session labels and box names, then step through matches; the search unfolds a match's ancestors |
| Space or `za`, `zc`, `zo` | toggle, fold, unfold the box's subtree; a folded box shows how many sessions it hides |
| `zM` / `zR` | fold every project, split and division / unfold everything |
| `?` | show or hide the symbol legend above the help line |
| `+` / `-` | zoom one of ten levels: four full-card widths (more sessions per card as they widen), four compact widths (one line: name and lead glyph), and two dense (one glyph per box) |
| `H` `J` `K` `L`, arrows | pan |
| `c` | centre on the focused box |
| `i` | show or hide the info pane |
| Enter (or `A-t`) | open the Sessions view with that session focused; folded sidebar headers above it unfold. Enter on a box with no session stays in the chart |
| F6, Esc, `q` | the same, except that with no session selected (a box without one, or a header the chart opened on and you haven't moved from) the sidebar cursor goes back exactly where it was when the chart opened |

The chart opens fully unfolded (folds last only while it is open), on the
Sessions view's focus: its session; for a task or workspace header, that
box with no session selected yet; anything else, Owner. It never reopens on
the previous visit's box. Within a box the selection is kept by session uid,
so a lead exiting or `b` re-sorting the box doesn't move it.

After any focus move, Tab, search jump, zoom, fold or info-pane toggle the
viewport pans to keep the focused box on screen. A pan or `c` stops that until
the next focus move; meanwhile, if a refresh moves the focused box, the
viewport moves with it, so the box stays where you left it.

## Symbols

Every symbol is plain Unicode one terminal cell wide, also where East Asian
Ambiguous characters render wide (none is Ambiguous, none has an emoji
presentation, and a test checks both widths); no Nerd Font or terminal
graphics are needed. On a card a blank cell separates the engine and host
marks, so even a wide rendering never covers the host. The mapping is in one place,
`tui/src/app/symbols.rs`, which the chart, its legend (`?`) and the Sessions
view's A-i peek all use.

| Symbol | Meaning |
|---|---|
| ✻ (orange) | Claude, the mark Claude Code itself uses |
| ⬡ (teal) | Codex |
| `$` | bash |
| ⌂ | local host; any other host shows its first letter in magenta |
| ◌ ➤ ◷ ⊘ ⊛ ✓ ✗ | board item open, active, waiting, blocked, blocked on Owner, done, dropped |
| ⚑ | a flagged board item (a card shows a flag first, else the most urgent status) |
| ⊞ | a box with no session of its own: Owner, a project, Teams or Continuous |
| ◆ | a division (as in the sidebar; ◆ is otherwise only the owner-blocked blink) |
| ∘ | a task box whose sessions are all further down its subtree |
| ▸ | a folded box |

Cards use only the symbols. The info pane and the A-i peek keep the word
beside each one (`⬡ codex`, `➤ sejd #14 ACTIVE`). The Sessions sidebar rows
show no engine name, so they are unchanged.

## Info pane

For the focused session, the pane shows:
- name, engine, status and how long it has been in that status;
- age since spawn (taken from the uid, so it survives restarts) and last
  activity;
- task, boss (the parent box and its lead), and the number of direct reports;
- host and worktree;
- the board items the session holds, with board, number, status badge, ETA
  and flags.

Board items come from the planning API's open-board list plus `board.read` on
each board. They are matched by the holders' `session_uid`. A board that
fails to read waits its normal turn, and its error is listed by name in the
pane. The chart re-lists
boards every 60 s and re-reads each board every 30 s, with one request in
flight at a time, and only while the chart is open.

For Owner, an org or a division, the pane shows the box's kind, its boss, its
number of reports and how many sessions are under it.

## Code

`tui/src/app/orgchart.rs` contains the model (`org_model`), the layout
(`layout`), the keys, drawing and the board poll. The chart reuses
`task_leads`, `task_level`, `leader_glyph` and `planning.task_division`
(`app/tree.rs`), the palette jump,
`task_parent_map` and the continuous column's `continuous_rows_unfiltered`
(`app/nav.rs`).
