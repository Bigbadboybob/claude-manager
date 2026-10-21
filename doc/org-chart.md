# Org chart (F6)

F6 opens a full-screen org chart of every agent session on every host. It
shows reporting lines only; nothing on it grants or implies approval rights.
F6, Esc or `q` closes it.

## Layout

- **Owner** is at the top. Its reports are the orgs:
  - one box per **division**, an initiative whose `kind` is `division`;
  - **Standalone**, for task trees outside any division, plus a box for each
    workspace whose sessions have no task;
  - **Continuous**, one box per continuous orchestrator. Its workers sit under
    it, grouped exactly as in the continuous column (`A-c`).
- Under each org come the root tasks, with subtasks nested along
  `parent_task_id` at any depth. If a task in the chain has no session of its
  own, it still gets a box so the chain stays connected. A subtask of a
  continuous worker nests under that worker.
- A task box lists the task's sessions. The lead session comes first; it is
  chosen by the shared lead rule (`task_leads`). The other sessions follow in
  spawn order.
- **Leader marks.** Only the lead of a box with reports carries a mark, and
  the mark gives its level below Owner: ★ for a root task (level 1), ☆ for
  its subtask (level 2), ✦ for level 3, and ✧ for level 4 and deeper. A
  task's level counts its `parent_task_id` chain; a continuous worker without
  its own task counts the boxes above it. Leads with no reports carry no
  mark. ◆ stays reserved for divisions. The Sessions sidebar uses the same
  marks from the same function, `tree::leader_glyph`.
- Each session line shows the sidebar's state glyph (spinner, afterglow,
  settled or stale ●, ◐, ?, ✗, alert, owner-blocked, ⟳; × when exited), then
  the label, the engine, and a host marker: ⌂ for local, otherwise the first
  letter of the host name, in magenta.
- Bash sessions are hidden by default; `b` shows them. Opening the chart on a
  bash session shows just that one, until the chart closes.
- Once a box has more than three reports with no reports of their own, those
  reports stack in a column, so a 50-report lead stays two boxes wide.

## Keys

| Key | Action |
|---|---|
| `h` / `l` | previous / next sibling |
| `j` / `k` | first report / boss |
| `gg` / `G` | Owner / last box |
| `Tab` / `S-Tab` | next / previous session in the box |
| `/` `n` `N` | search session labels and box names, then step through matches; the search unfolds a match's ancestors |
| `zc` `zo` `za` | fold / unfold / toggle the box's subtree; a folded box shows how many sessions it hides |
| `+` / `-` | zoom: full cards, then compact (one line: name and lead glyph), then dense (one glyph per box) |
| `H` `J` `K` `L`, arrows | pan |
| `c` | centre on the focused box |
| `i` | show or hide the info pane |
| Enter (or `A-t`) | open the Sessions view with that session focused; folded sidebar headers above it unfold |

The chart opens on the session focused in the Sessions view. Moving the focus
scrolls it into view; panning stops that until the focus moves again.

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
