# Continuous teams in the sidebar

**Status: retired (phase 5, 2026-11-21).** The dedicated third pane and its
column toggle were removed after continuous teams moved into the org model.

Continuous teams render in the main Sessions sidebar, inside their org section,
as tinted blocks. A legacy scheduler orchestrator and its workers are ordinary
rows in that same sidebar; matching still uses `managed_by_uid`,
`parent_task_id`, and managed same-task sessions, so no row is hidden during
migration.

`A-h` and `A-l` no longer move between columns. `A-g` searches the one sidebar.
An old `continuous_column_on` value is ignored and written back as `false`;
`hide_continuous` remains only for manifest compatibility. `A-c` is free.
`Alt+C` opens the continuous-teams dashboard, also available from F9 Global
Settings with `c`.

The dashboard lists each marked team with its lead, host, wake history, missed
runs, blocked state, bound queue depth/age, and open items. The daemon's shared
`#continuous` monitor checks every 30 seconds for missed or late wakes, old
queue items, and a dead lead.

See [doc/continuous-teams.md](doc/continuous-teams.md) and
[HOWTO_CONTINUOUS_TASKS.md](HOWTO_CONTINUOUS_TASKS.md). The former panel
implementation is historical; production layout and navigation no longer call
its dedicated-column path.
