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
sessions that are neither members nor proposed follow in folded groups
(Space or Enter on a header opens it):

- **assistants of <member>**: sessions that reach the member as F6 places them
  ([team mode](org-chart.md#team-mode)). They are part of the member's office
  and are not offered for adoption; `p` on one deliberately promotes it to a
  member of its own (reporting to that member), which is the only reason to
  adopt an assistant.
- **N other sessions**: the few that reach no member, mostly Owner's own.
  Adopting one (`y`, then `A`) makes it a member of its own.

| Key | Action |
|---|---|
| `j` / `k` | move |
| `y` / `x` | approve / decline the proposal (a decline stops it being proposed again); on an other-session row, mark / unmark it |
| Space | cycle approve → decline → undecided; on an other-session row, toggle the mark; on a group header, fold or open it |
| `p` | on an assistant, mark / unmark it for promotion to a member (`y` and Space leave assistants alone) |
| `Y` | approve every undecided proposal that has no conflict (never an assistant or other session) |
| `e` `t` `m` `w` | edit the handle, team, manager or home workspace (Enter saves, Esc cancels); team and manager take a name, handle or `owner` |
| `K` | cycle the kind (agent / continuous) |
| `A` | apply the decisions and adopt the marked sessions, folded or not; rejected rows stay with their reason (`stale`, `handle_taken`, `cycle`, …) |
| `r` | reload |
| Esc, `q` | back to the chart |

Nothing is written before `A`. Adopting sends `org.adopt {session_uid, handle,
kind, manager, team_id?, home_workspace?, request_id}` for each marked row, one
at a time, to the session's own host; after an error the rows not yet sent
keep their marks for another `A`. Hire, move and retire come later.

## Sessions sidebar: Team sub-view

`A-v` cycles **Status → Task → Team**. The Team sub-view lists the teams
parents-first. Under each team come its members, lead first (`★`; a vacant
member shows `∅`), and under each member its main session, then its
assistants, placed exactly as F6 places them
([team mode](org-chart.md#team-mode): member stamp, spawner chain, task
chain, workspace). Another member's main session always stays with that
member. Sessions Owner started that reach no member follow under **Owner's
sessions**, the rare rest under **Unassigned**, and continuous sessions stay
in the continuous column. Team and member rows are headers the cursor stops on;
Space or Enter folds one (`▸`, with the number of sessions it hides), and the
folds are kept in the manifest (`folded_org`). `A-g` still reaches a session
inside a fold.

The view is skipped when the daemon has no org model. Until a snapshot with
members arrives, it draws the Task tree (the sidebar title says "Tasks") and
behaves exactly like it. It
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
| `c` | close the epic: `d` done or `c` cancelled, then the close plan opens |

The **close plan** (`epic.close_plan`, doc/org-model.md §7.6) lists the
epic's open items, its team, the team's members and their workspaces, each
with its options and the default picked. `j`/`k` move, Space/`l` and `h` step
a row's choice, `A` applies (`epic.close_apply`). Rows someone else must
decide come back as pending with who must decide. Esc or `q` closes it. Group
writes send `expected_revision`, and a `revision_changed` refusal reloads the
board. A prompt or menu keeps the group (id and revision) and item it opened
on, so a poll that moves rows doesn't change its target.

## Planning view: placing tasks

When the API serves placement (`boards.on_boards`: probed once on entering the
planning view, then polled every 30 s only while the On-boards strip is shown
and the terminal is focused, with `since_version` so an idle board answers
`unchanged`; a full read at most every 5 minutes):

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

## Polling

The org snapshot (`org.snapshot`) is read every 5 s while the chart, the
roster or the place dialog is open, and every 15 s while only a background
screen wants it: the Team sub-view, the board overlay showing groups, or the
On-boards strip. Background polls stop when the terminal loses focus (focus
reporting) or another view is shown. Polls send `since_version` and get
`unchanged` back when nothing moved; a full read happens at most every 5
minutes, and whenever a screen opens.

## Code

| File | What |
|---|---|
| `tui/src/app/org_model.rs` | snapshot types, polling, `owner_rpc`, support checks |
| `tui/src/app/orgchart_teams.rs` | the F6 team-mode model and info pane |
| `tui/src/app/roster.rs` | the roster overlay |
| `tui/src/app/team_view.rs` | the Team sub-view |
| `tui/src/app/board/epics.rs` | the Alt+B group tree, group menu and close plan |
| `tui/src/app/place.rs` | the planning chooser, place dialog and on-boards strip |
