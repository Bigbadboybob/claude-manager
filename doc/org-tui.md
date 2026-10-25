# Org model in the TUI

The viewer's side of the org model (doc/org-model.md §11, slices S2 and S3).
The org chart's team mode is in [org-chart.md](org-chart.md#team-mode).

Every screen here checks for support first and degrades on its own: an old
daemon or planning API answers `unknown method` (or a bare FastAPI
`Not Found`), and the screen keeps today's behavior. Nothing needs a setting.
`tui/src/app/org_model.rs` holds the snapshot client and the shared
`is_unsupported` / `owner_rpc` helpers. `owner_rpc` tries the local daemon
first, then the others, and reports "unsupported" only when every host that
answered said so.

## Roster (`R` in the F6 chart)

Owner approves the proposed members in one pass (`org.roster`, then
`org.roster_apply`). Each proposal row shows the handle, team, manager, kind
and home workspace, any conflicts, and Owner's decision. Live claude and codex
sessions that are neither members nor proposed follow as **adopt** rows.

| Key | Action |
|---|---|
| `j` / `k` | move |
| `y` / `x` | approve / decline the proposal (a decline stops it being proposed again) |
| Space | cycle approve → decline → undecided; on an adopt row, adopt it now |
| `Y` | approve every undecided proposal that has no conflict |
| `e` `t` `m` `w` | edit the handle, team, manager or home workspace (Enter saves, Esc cancels); team and manager take a name, handle or `owner` |
| `K` | cycle the kind (agent / continuous) |
| `A` | apply the decisions; rejected rows stay with their reason (`stale`, `handle_taken`, `cycle`, …) |
| `r` | reload |
| Esc, `q` | back to the chart |

Adopting sends `org.adopt {session_uid, handle, kind, manager, team_id?,
home_workspace?, request_id}` to the session's own host. Hire, move and retire
come later.

## Sessions sidebar: Team sub-view

`A-v` cycles **Status → Task → Team**. The Team sub-view lists the teams
parents-first. Under each team come its members, lead first (`★`; a vacant
member shows `∅`), and under each member its main session, then the other
sessions of its home workspace (else of its main session's workspace).
Another member's main session always stays with that member. Sessions no
member claims follow under **Unassigned**, and continuous sessions stay in the
continuous column. Team and member rows are headers: the cursor moves over
sessions.

The view is skipped when the daemon has no org model. Until a snapshot with
members arrives, it draws the Task tree (the sidebar title says "Tasks"). It
is saved like the other sub-views (`view = "team"` in the manifest); an older
viewer reads that as Status.

## Board overlay (Alt+B): headings and epics

A board's groups (`board.read` → `groups[]`) draw as a folding tree: flagged
items first, then each heading or epic (`⟐ name`, its status, its PoC and team as a
chip, and the status counts of the items under it) with its items and subgroups under it, then loose items, then
a strip of closed groups. Space on a group row folds it.

`g` on a group opens its menu (`g` anywhere else still refreshes):

| Key | Action |
|---|---|
| `e` | promote the heading to an epic (Owner or the board lead) |
| `p` | set the epic's PoC (a member handle) |
| `r` | rename |
| `n` | new heading under it |
| `c` | close the epic: opens the close plan |

The **close plan** (`epic.close_plan`, doc/org-model.md §7.6) lists the
epic's open items, its team, the team's members and their workspaces, each
with its options and the default picked. `j`/`k` move, Space/`l` and `h` step
a row's choice, `A` applies (`epic.close_apply`). Rows someone else must
decide come back as pending with who must decide. Esc or `q` closes it. Group
writes send `expected_revision`, and a `revision_changed` refusal reloads the
board.

## Planning view: placing tasks

When the API serves placement (`boards.on_boards`, polled every 30 s and only
while the planning view is shown):

- **`A-f`** opens a chooser with **Launch** preselected, so Enter keeps
  today's launch flow (`l` also launches). **Place on a board…** (`p`, or ↓
  then Enter) opens the place dialog. A placed task also offers **Unplace**
  (`u`), which returns it to the backlog. On an old API, `A-f` is the plain
  launch, as before.
- **Place dialog:** Tab moves between fields and ←/→ changes the value:
  - **board:** an open board;
  - **epic:** none, or one of the board's active epics;
  - **holder:** nobody (open), a member, or *hire a new member…* (Enter,
    then a handle and an optional title).

  Enter places the task with `item.place {task_id, board, group_id?, holder?,
  role: "owner", replace?, request_id}`. A member is named by its main
  session uid, else its handle. A hire runs `org.hire` first and then places
  the task with the new handle, and the dialog says so if the API can't hire
  yet. Placing a task that is on another board sends `replace`.
- **Board chip.** A placed task shows `[board #n mark]` before its title,
  and the detail pane adds `Board: board #n (status) · group`, noting when
  the status mirror is off.
- **On boards** strip, read-only, under the board: open epics (board, name,
  status, PoC, open count) and board-native items (board, number, title,
  holders). `A-b` hides or shows it.

## Code

| File | What |
|---|---|
| `tui/src/app/org_model.rs` | snapshot types, polling, `owner_rpc`, support checks |
| `tui/src/app/orgchart_teams.rs` | the F6 team-mode model and info pane |
| `tui/src/app/roster.rs` | the roster overlay |
| `tui/src/app/team_view.rs` | the Team sub-view |
| `tui/src/app/board/epics.rs` | the Alt+B group tree, group menu and close plan |
| `tui/src/app/place.rs` | the planning chooser, place dialog and on-boards strip |
