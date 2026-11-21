# Continuous teams in the sidebar

**Status: retired (phase 5, 2026-11-21).** The dedicated third pane and its
column toggle were removed after continuous teams moved into the org model.

Continuous teams now render in the main Sessions sidebar, inside their org
section, as tinted blocks. The block header carries cadence, next/last run, and
health. A legacy scheduler orchestrator and its workers are ordinary rows in
that same org section; the old matching rules still identify its workers by
`managed_by_uid`, `parent_task_id`, or a managed same-task session, so no row is
lost during migration.

`A-h` and `A-l` no longer move between sidebar columns. `A-g` searches the one
sidebar. An old `continuous_column_on` value in `tui-sessions.json` is ignored
and rewritten as `false`; `hide_continuous` remains only for manifest
compatibility. `A-c` is free. `Alt+C` opens the continuous-teams dashboard,
also available from F9 Global Settings with `c`.

The dashboard lists every marked team with its lead, host, wake history, missed
runs, blocked state, bound queue depth/age, and open items. The daemon's shared
`#continuous` monitor checks every 30 seconds for a missed or late wake, an old
queue item, or a dead lead.

See [doc/continuous-teams.md](doc/continuous-teams.md) for the team setup and
[HOWTO_CONTINUOUS_TASKS.md](HOWTO_CONTINUOUS_TASKS.md) for the operator recipe.
The implementation history below is retained as an archive of the retired
panel; it is not a current keybinding or layout contract.

## Archived implementation history

The former panel used `SidebarColumn::Continuous`, `visual_items_continuous()`,
a third `ratatui` layout pane, and persisted `continuous_column_on`. Those
helpers remain only where they make old manifests or migration tests load
safely; production layout and navigation no longer call them.
