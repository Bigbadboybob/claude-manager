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
- **Owner as leader.** A task pinned to `"owner"` (A-e Lead, or an
  orchestrator's `org(action="set_lead", session_uid="owner")`) is led by
  Owner, and a division root with no live lead has Owner as stand-in. Such a
  box opens its title with ♔ (who may set or clear an Owner pin: see Known
  limits), and a division box says "Owner (stand-in)" or
  "Owner" at the top; the info pane's `lead` line names it. Sessions under it
  keep their own leads and marks. The Sessions sidebar shows the same on task
  and division headers: the level's leader mark with ♔ (`★♔`). One rule
  decides it on both sides: `cm_daemon::org::task_lead`.
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
- **Stacks.** A **leader** is a task box with live reports (the same rule as
  the leader marks; a continuous orchestrator counts as one). A leader's
  reports that have no reports of their own stack in one column straight
  under it, a compact list hanging from a spine. Reports that have reports of
  their own fan out side by side as branches; a leader with both draws its
  stack as one column to the left of its branches. Only leaders stack: the
  reports of Owner, a project, Teams, Continuous, a division or Other always
  sit side by side, so root tasks stay in one row.
- Apart from stacks, every level is laid out in one row, left to right; the
  chart grows as wide as it needs and the viewport pans to it.

## Team mode

When the daemon serves the org model (`org.snapshot`, doc/org-model.md) and it
has members besides Owner, the chart is drawn from teams and managers instead
of tasks. Old daemons and APIs keep the task chart above; the switch needs no
setting. The snapshot is polled only while something shows it (see
[polling](org-tui.md#polling)).

- **Owner** at the top, then the **teams** parents-first (divisions, then
  teams, then by name). Each team box puts its lead on top (`★ handle`) and
  its current epics as chips (`⟐ name`, from the board overlay's groups).
- **Members** sit under their manager's box: a member whose manager is the
  team lead hangs from the team box, and one managed by another member hangs
  from that member. A manager line that crosses teams (manager outside the
  member's team) is a **matrix** case and carries `⋯` with the manager's
  handle.
- A member who is a **guest** in another team also appears there, as a
  second box marked `⊹`.
- **Assistants** are every other session that reaches a member (doc/org-model.md
  O5: as durable as main sessions). They are found by walking up from the
  session until a member's main session or a team's task is reached:
  1. the daemon's member stamp (`org.member_id`, S4), when present;
  2. its spawner (`managed_by_uid`), transitively: EP's assistant and that
     assistant's own sub-worker, each in its own ephemeral worktree, are all
     EP's;
  3. its task's `parent_task_id` chain: a task some member's main session is
     bound to, or a team's coordinator task (then the team's lead box);
  4. its workspace: the member's home workspace, else its main session's.

  Placement never depends on how long a session lives or what kind of
  workspace it is in. Assistants fold under the box as `+N assistants` with
  their state glyphs; `a` expands or folds them (a card then names the ones
  placed through the task tree, and the info pane's `attached` line says how
  each one got there). Ended assistants are hidden.
- A **vacant** member, the team lead included, is drawn empty with `∅` and
  its stand-in.
- A **standby** team is greyed.
- Sessions Owner started directly (no spawner) that reach no member sit in an
  **Owner's sessions** box under Owner, by task or workspace. **Unassigned**
  holds only the rare rest (spawned by a session that leads nowhere either)
  and is usually absent.
- A box with more than three report boxes that have no reports of their own
  (Owner's sessions, a lead's lanes) stacks them in one column, as leaders
  do, so a wide fan-out never needs horizontal scrolling; up to three sit side
  by side.

The info pane shows, for a team: kind and status, lead, parent, member counts
by role, board, channel and epics; for a member: kind and status, team,
manager (matrix noted), guest teams, vacancy, mandate, focus and the board
items it holds. (The four S5 facts replace mandate and focus when they land.)

`R` opens the roster (doc/org-tui.md).

## Keys

| Key | Action |
|---|---|
| `h` / `l` | previous / next box along the row. A stack counts as one slot of its leader's row (left of the leader's branches); at the end of a stack's row, or of a leader's row, `h`/`l` carry on along the row above (to the leader's neighbours). Coming back to a branch or stack lands on the box it was left from, however deep |
| `j` / `k` | down to the report last focused under this box (else the first; a stack comes first) / up to the boss. Inside a stack, `j`/`k` move down and up the list; `k` on its first item goes to the leader, and the last item is the bottom of the branch. Each box also remembers its last-focused session, so `k` then `j` lands where you were |
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
| `a` | team mode: expand or fold the focused member's assistants |
| `R` | team mode: open the roster |
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
| ⟐ | team mode: an epic chip on a team box |
| ⊹ | team mode: a guest member's box in another team |
| ⋯ | team mode: a matrix manager line (manager outside the member's team) |
| ∅ | team mode: a vacant member |
| ⚙ ⧗ ☛ ⚠ ✕ ↻ ⏱ | the activity badge: working, background jobs, waits on you, rate limited, error, retrying, stalled (second legend line; see [activity badge](sidebar-sections.md#activity-badge)) |

Cards use only the symbols. The info pane and the A-i peek keep the word
beside each one (`⬡ codex`, `➤ sejd #14 ACTIVE`). The Sessions sidebar rows
show no engine name, so they are unchanged.

## Info pane

Each session line on a full card carries the compact activity badge between
its label and the right-edge marks, as in the Sessions sidebar (`⚙ 3m+2`,
`⧗2`, `☛`, `⚠12m`; at most 6 cells, no job labels). When the card is too
narrow for the whole label and the badge, the badge is dropped rather than
the label cut. F9 turns it off on cards.

For the focused session, the pane shows:
- name, engine, status and how long it has been in that status;
- what it is doing (`doing`), the activity badge in full (job labels, the
  tool asking permission, the error kind), F9 toggle or not;
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

## Known limits

- An Owner pin is set, cleared or overridden only by Owner (A-e) or an
  orchestrator: a session with global permissions, or one whose task sits
  above the target in its `parent_task_id` chain (at most 32 steps, stopping
  at a loop). A metadata PATCH can't touch the pin; it is kept as stored. But
  an agent can still re-parent a task under its own with `parent_task_id`
  (`update_task`) and so become "above" it; that gap is tracked separately.

## Code

Team mode is `tui/src/app/orgchart_teams.rs` (`team_chart_model`, the info
pane's member and team lines) over the snapshot client in
`tui/src/app/org_model.rs`. `tui/src/app/orgchart.rs` contains the model (`org_model`), the layout
(`layout`), the keys, drawing and the board poll. The chart reuses
`task_leads`, `task_level`, `leader_glyph` and `planning.task_division`
(`app/tree.rs`), the palette jump,
`task_parent_map` and the continuous column's `continuous_rows_unfiltered`
(`app/nav.rs`).
