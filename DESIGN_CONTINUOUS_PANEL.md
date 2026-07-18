# Design — Two-column continuous panel (TUI)

**Status:** ✅ SHIPPED (S1–S5, 2026-06-29). Decisions locked with the user 2026-06-29. All slices below landed; commits on branch `cm/daemon-side-workflow-execution-autonomous-cloud-wo`. TUI-only (runs locally — no deploy).

## Row indicator glyphs (authoritative legend)

Rendered by `draw_continuous_column` (app.rs). Human-oriented copy of this legend: HOWTO_CONTINUOUS_TASKS.md → "Continuous-column glyph legend".

- **spinner (green)** — session Running.
- **`●` white** — operator action needed: the row's planning task is raw-`blocked` (`session_needs_human`; reads `api_status`, NOT the derived `task_status()`). Truthful only under the planning-status convention: orchestrators set `blocked` exclusively for operator-action states (fix awaiting review / explicit human decision); `/triage-review` flips merged-but-monitoring subtasks back to `running` to preserve the invariant.
- **`◉` cyan + `↳ text` sub-line** — parked operator question (`metadata.operator_question`, P3 Feature 1); wins over `●`/`◇` while idle.
- **`○` yellow sub-line(s)** *(2026-07-18)* — **dispatch pending**, one line per index issue: the operator unblocked an issue in the orchestrator's `.<task>/index.yaml` (cleared `blocked_reason` + dated `# OPERATOR <date>` comment) but the orchestrator hasn't acknowledged (`operator_ack` ≥ directive date) nor is a live subtask mapped to it. Not a row glyph — a per-issue line under the orchestrator row, so it composes with any row state. Data path: daemon `continuous.dispatch_pending` (tolerant line-scan of index.yaml, reviewable tasks only — `daemon/src/continuous/dispatch_pending.rs`) → per-host 30s TUI poller (`spawn_dispatch_pending_pollers`) → planning-liveness filter (`issue_awaits_dispatch`) → render. Closes the gap where an approved-awaiting-dispatch issue was indistinguishable from untracked for up to a full cycle (~hours).
- **`◇` dim** — idle, orchestrator-has-it; next fire advances it.
- **`⟳` yellow** — remote attach stream lost, auto-reconnect in progress.

## Revision (2026-06-29) — single toggle, column-only, respawn-robust nesting

After live use, three changes (the original S1–S5 model is superseded where it conflicts):

1. **Single toggle.** The dedicated column is now the SINGLE continuous control, bound to **`A-c`**. The old `A-C` (column toggle) and the separate `A-c` master-hide (`hide_continuous`) are gone — merged into one key. ON = column shown; OFF = continuous tasks hidden entirely. `hide_continuous` is retained on the manifest only for back-compat round-trip; it's never consulted.
2. **Continuous only in the column.** The main sidebar builders (`visual_items_status`, `visual_items_status_multihost`, `visual_items_task`) now ALWAYS exclude continuous members — there is no "continuous at the bottom of the main sidebar" mode anymore. A continuous task renders only in the column (when on) or nowhere (when off); never in both. A workspace whose only sessions are continuous members is skipped in the main builders (no bare header).
3. **Respawn-robust nesting.** A subtask nests under an orchestrator when its `managed_by_uid == orchestrator.uid` **OR** its task's `parent_task_id == orchestrator.task_id`. The task-tree link is the robust one: a continuous orchestrator that respawns gets a NEW session uid, orphaning the `managed_by_uid` of subtasks spawned by the prior instance — but the TASK tree doesn't change, so they still group correctly. This fixed the "only the current instance's child nests, the rest leak into main" bug. Implemented by `continuous_members()` (the exclusion set) + `task_parent_map()`, both consumed by the column builder and all three main builders.

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
   - `push_active`: `A-p` → **`A-9`**; `pull_active`: `A-l` → **`A-0`**. (Originally tried `A-[`/`A-]`, but Alt+`[` can collide with the CSI escape introducer; digits deliver cleanly.)
   - `A-h` → column-nav **LEFT**; `A-l` → column-nav **RIGHT**.
4. **Nesting** in the continuous column: each orchestrator (continuous session) is a parent; sessions whose `managed_by_uid` is that orchestrator's uid render indented underneath (tree glyphs `├`/`└`). Direct children first; transitive (a subtask's subtask) deferred unless trivial.

## Cursor model change

Add a **column dimension**. New `SidebarColumn { Main, Continuous }` + `App.cursor_column`. The continuous column has its OWN item list (`visual_items_continuous()`), distinct from the main `visual_items()`. `navigate(±1)` operates within the active column's list. `A-h`/`A-l` switch `cursor_column` and clamp the cursor to a selectable row in the new column (remember per-column vertical position). `active_session()` resolves a `Cursor::Session(wi,si)` regardless of column (continuous sessions still live in workspaces), so the terminal pane shows whatever's selected in either column.

When `A-C` is OFF there is no continuous column → `cursor_column` is forced to `Main` and `A-h`/`A-l` are no-ops (or could still be used later).

## Builders

- `visual_items()` / `visual_items_*()`: when `A-C` is ON, STOP emitting the continuous section (it moves to the new column). When OFF, unchanged (continuous at the bottom).
- New `visual_items_continuous()`: emit, per orchestrator (sessions with `continuous_task_id`), a parent row then its `managed_by_uid` children indented. Reuses the `Session(wi,si)` VisualItem (+ a depth/indent hint). `ContinuousHeader` per orchestrator or one column header — TBD in S2.

## Slice plan

- **S1 — Keybinding reshuffle** (self-contained, no panel yet): retire `A-H` host-switch; hide→`A-H`; push→`A-9`; pull→`A-0`; `A-h`/`A-l` freed (left unbound for now). Update CLAUDE.md keybinding docs. *Verify:* hide/push/pull on the new keys; A-h/A-l do nothing yet; tests green.
- **S2 — Continuous-column builder + nesting**: `visual_items_continuous()` (orchestrator → nested `managed_by_uid` children). Main builders gate the continuous section on `!continuous_column_on`. Pure data + unit tests (nesting, ordering, hide interplay). No render yet.
- **S3 — Layout + render + `A-C` toggle**: split the sidebar region into main|continuous when on; render both lists; `A-C` toggles `continuous_column_on` (persisted). Cursor stays in Main this slice.
- **S4 — Cursor column dimension + `A-h`/`A-l` nav**: `SidebarColumn` + `cursor_column`; `A-h`/`A-l` switch columns + clamp; `navigate` per-column; focused-row highlight in the active column only. *Verify:* drive the cursor across columns; `active_session()` shows the right session; tests.
- **S5 — Polish**: tree glyphs, empty-column states, A-C+A-c interplay, the planning-view nesting parity (subtasks under continuous parents) if warranted, docs.

## Open questions (settle during slices)

- One column header vs per-orchestrator headers (S2).
- Transitive nesting depth (S2) — start with direct children.
- Does the continuous column respect the Status/Task `A-v` sub-view, or is it always task-nested? (Lean: always nested by orchestrator, independent of `A-v`.)
