# Design — Two-column continuous panel (TUI)

**Status:** ✅ SHIPPED (S1–S5, 2026-06-29). Decisions locked with the user 2026-06-29. All slices below landed; commits on branch `cm/daemon-side-workflow-execution-autonomous-cloud-wo`. TUI-only (runs locally — no deploy).

## Goal

Give continuous tasks (orchestrators) and the subtasks they spawn their own dedicated, **nested** sidebar column, instead of a flat section sorted to the bottom of the single sidebar. Toggle it with `A-C`; navigate between the main column and the continuous column with a unified left/right cursor.

## Current state (from mapping app.rs)

- Sessions-view layout: `Layout::horizontal([Min(40) terminal, Length(36) sidebar])` (app.rs ~13709). Terminal LEFT, one 36-col sidebar RIGHT.
- Sidebar rows are `VisualItem`s built by `visual_items()` → `visual_items_status()` / `visual_items_task()` (+ multihost variants). Continuous sessions (tagged `continuous_task_id`) are pulled out and sorted to the **bottom** behind a non-selectable `ContinuousHeader`; `A-c` (`hide_continuous`) hides them. Subtasks spawned by an orchestrator (`managed_by_uid` → orchestrator uid) appear **flat** in that section — no nesting.
- Cursor: `Cursor::{Workspace(wi), Task{wi,task_id}, Session(wi,si)}`. `navigate(±1)` is purely **vertical** over `visual_items()`. No column concept. `active_session()` resolves the focused session from the cursor.

## Decisions (locked)

1. **Layout = extra far-right column.** `A-C` ON adds a third pane: `[Min(40) terminal | Length(36) main | Length(36) continuous]`. `A-C` OFF = exactly today (`[Min(40) terminal | Length(36) main]`). The terminal pane shrinks (Min-40 floor) when the column is on — fine on a wide terminal, which is when you'd turn it on.
2. **`A-C` vs `A-c`.** `A-C` (new) toggles continuous-session *placement*: dedicated column (ON) vs bottom-of-main-sidebar (OFF, today). `A-c` (existing) is the master *hide* — hides continuous sessions entirely in either mode. Both persisted in the manifest.
3. **Nav-key reshuffle** (free `A-h`/`A-l` for left/right column nav):
   - retire the `A-H` host-switcher (`cycle_active_host`) — global host is being retired anyway; `active_host` stays at its `local` default (new sessions go local).
   - `toggle_session_hidden`: `A-h` → **`A-H`**.
   - `push_active`: `A-p` → **`A-[`**; `pull_active`: `A-l` → **`A-]`**. (Caveat: Alt+`[` can collide with the CSI escape introducer; verify it registers, else pick alternates — low stakes, cloud-only ops.)
   - `A-h` → column-nav **LEFT**; `A-l` → column-nav **RIGHT**.
4. **Nesting** in the continuous column: each orchestrator (continuous session) is a parent; sessions whose `managed_by_uid` is that orchestrator's uid render indented underneath (tree glyphs `├`/`└`). Direct children first; transitive (a subtask's subtask) deferred unless trivial.

## Cursor model change

Add a **column dimension**. New `SidebarColumn { Main, Continuous }` + `App.cursor_column`. The continuous column has its OWN item list (`visual_items_continuous()`), distinct from the main `visual_items()`. `navigate(±1)` operates within the active column's list. `A-h`/`A-l` switch `cursor_column` and clamp the cursor to a selectable row in the new column (remember per-column vertical position). `active_session()` resolves a `Cursor::Session(wi,si)` regardless of column (continuous sessions still live in workspaces), so the terminal pane shows whatever's selected in either column.

When `A-C` is OFF there is no continuous column → `cursor_column` is forced to `Main` and `A-h`/`A-l` are no-ops (or could still be used later).

## Builders

- `visual_items()` / `visual_items_*()`: when `A-C` is ON, STOP emitting the continuous section (it moves to the new column). When OFF, unchanged (continuous at the bottom).
- New `visual_items_continuous()`: emit, per orchestrator (sessions with `continuous_task_id`), a parent row then its `managed_by_uid` children indented. Reuses the `Session(wi,si)` VisualItem (+ a depth/indent hint). `ContinuousHeader` per orchestrator or one column header — TBD in S2.

## Slice plan

- **S1 — Keybinding reshuffle** (self-contained, no panel yet): retire `A-H` host-switch; hide→`A-H`; push→`A-[`; pull→`A-]`; `A-h`/`A-l` freed (left unbound for now). Update CLAUDE.md keybinding docs. *Verify:* hide/push/pull on the new keys; A-h/A-l do nothing yet; tests green.
- **S2 — Continuous-column builder + nesting**: `visual_items_continuous()` (orchestrator → nested `managed_by_uid` children). Main builders gate the continuous section on `!continuous_column_on`. Pure data + unit tests (nesting, ordering, hide interplay). No render yet.
- **S3 — Layout + render + `A-C` toggle**: split the sidebar region into main|continuous when on; render both lists; `A-C` toggles `continuous_column_on` (persisted). Cursor stays in Main this slice.
- **S4 — Cursor column dimension + `A-h`/`A-l` nav**: `SidebarColumn` + `cursor_column`; `A-h`/`A-l` switch columns + clamp; `navigate` per-column; focused-row highlight in the active column only. *Verify:* drive the cursor across columns; `active_session()` shows the right session; tests.
- **S5 — Polish**: tree glyphs, empty-column states, A-C+A-c interplay, the planning-view nesting parity (subtasks under continuous parents) if warranted, docs.

## Open questions (settle during slices)

- One column header vs per-orchestrator headers (S2).
- Transitive nesting depth (S2) — start with direct children.
- Does the continuous column respect the Status/Task `A-v` sub-view, or is it always task-nested? (Lean: always nested by orchestrator, independent of `A-v`.)
