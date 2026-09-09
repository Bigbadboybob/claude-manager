# Sidebar sections

## Summary

Owner-created, collapsible sections in the Sessions-view sidebar (Task sub-view) that group workspaces by project. A section is a pure display construct stored in the TUI manifest; it never touches the planning API, the daemon, or the MCP surface. Membership is explicit per workspace, and a workspace without an explicit assignment inherits its parent task's section through the task tree, so an agent's `create_subtask` lands under its parent's section automatically.

## Problem

Subtasks nest to arbitrary depth, and rendering that tree faithfully in one sidebar is unreadable. What the Owner actually needs is one level of grouping: "everything for project X", where X is a stream of work with one top-level task and a handful of subtasks and side tasks. Today the sidebar is a flat list of workspaces (pinned first, then status-ranked) with no way to draw that boundary.

## Goals

- Create, rename, recolor, reorder, fold, and delete sections from the Sessions view.
- Assign a workspace to a section from the A-e settings form on a workspace or task row.
- A new workspace created with A-n while the cursor is inside a section joins that section.
- A subtask (task with `parent_task_id`) whose parent's workspace is in a section renders in that section without any assignment.
- Everything persists in `~/.cm/tui-sessions.json`, including fold state.

## Non-goals

- No agent-facing MCP tool for sections. Sections are the Owner's organisation of the board.
- No change to the planning view, the continuous column, or the `backtests` group.
- No change to the Status sub-view (flat running/idle partition), which stays a "what is happening right now" view.
- Not a replacement for subtasks: task hierarchy still lives in `parent_task_id`.

## Current state

- Sidebar rows are built by `visual_items_task` (`tui/src/app/nav.rs`), one `WorkspaceHeader` per non-closed, non-past, non-continuous workspace, with `TaskHeader` / `Session` / `WorkflowHeader` rows underneath.
- Workspace order is the status-ranked order of `App::workspaces` with pinned workspaces floated to the top (`resort_workspaces_for_pin`).
- The cursor is `Cursor::{Workspace, Task, Session, Backtest}`; `navigate` walks `visual_items()` and skips non-selectable rows.
- Display-only task state already rides the manifest as a sidecar: `Manifest::task_colors` in `daemon/src/manifest.rs`, hydrated in `App::new` and written in `save_session_manifest`.
- The backtests group is the precedent for a selectable, foldable header with no PTY behind it (`Cursor::Backtest`, Space/Enter fold toggle in `input.rs`).

## Proposed design

### Data model

Two new sidecar fields on the shared `Manifest` struct (`daemon/src/manifest.rs`), both serde-default and skipped when empty so old manifests round-trip byte-identically:

```rust
pub struct SidebarSection {
    pub id: String,            // "sec-<hex nanos>", stable
    pub name: String,
    pub color: Option<String>, // USER_COLORS name
    pub folded: bool,
}
pub sections: Vec<SidebarSection>,                 // display order
pub workspace_sections: HashMap<String, String>,   // workspace id -> section id (explicit)
```

Mirrored on `App` as `sections` and `workspace_sections`. Membership is a sidecar map rather than a field on `Workspace` so the ~40 `Workspace { .. }` literal sites stay untouched.

### Resolving a workspace's section

`App::section_of_workspace(wi) -> Option<&str>`:

1. Explicit: `workspace_sections[ws.id]` if that section still exists.
2. Inherited: for each task bound to this workspace, follow `parent_task_id` (and, for `propose_task` rows, `metadata.filer.parent_task`) to the parent task, take the parent's workspace, and recurse. Bounded depth with a visited set.
3. Otherwise loose (no section).

Explicit assignment always wins, so the Owner can pull a subtask out of its parent's section. Deleting a section drops its explicit assignments; inherited members follow whatever their parent resolves to.

### Layout

`visual_items_task` emits, in order: each section in `sections` order as `SectionHeader(id)`, then, unless folded, the section's member workspaces (in the existing sorted order, so pinned members float within their section); then a separator and the loose workspaces exactly as today. An empty section still renders so it can be created before anything is assigned. Continuous-only and past workspaces stay excluded regardless of section. Rows inside a section render with one extra leading space.

Folded header: `▸ Name  (n)` with a running/idle rollup. Unfolded: `▾ Name`. Colored by the section's color, else `theme::HEADER`; selection keeps White+BOLD.

In the Task sidebar, a background tint covers the section heading and its
workspace, task, workflow, and session rows. The tint follows the section color,
with a neutral fallback. Internal workspace separators share the tint; section
boundaries and loose workspaces use the terminal's default background. Each
section ends with a heavy, bold, near-white horizontal rule, including a folded
section or the final section without loose workspaces below it. Internal
workspace separators stay thin and dim. Filtering
or scrolling away the heading does not remove a member's tint. Planning keeps
its default background and the initiative coordinator `◆` glyph.

Select a section heading and press **Alt+E** to change its color. The palette is
red, orange, yellow, green, cyan, blue, magenta, and pink, plus the neutral default.
The choice controls both the heading accent and its background tint. The tint
uses three times the original RGB intensity; this does not change terminal alpha.

Terminal colors do not include alpha. Whether these colored cells inherit the
window's transparency is controlled by the terminal emulator; CM does not change
the terminal's opacity setting.

For [Ghostty 1.2+](https://ghostty.org/docs/config/reference#background-opacity-cells),
set `background-opacity-cells = true` to apply your existing
`background-opacity` to colored cells as well. This affects all explicit cell
backgrounds in the terminal, including other applications.

### Cursor and keys

New `Cursor::Section(String)`. It is selectable; `active_workspace_index`, `active_session`, `cursor_task_id` return `None` for it, so task/session actions no-op with a status hint.

| Key | On a section header | Elsewhere in Sessions view |
|---|---|---|
| `A-N` | new section | new section |
| `A-e` | Section settings: name, color | Workspace/Task settings gain a **Section** field (←/→ cycles none + sections) |
| `A-x` | delete section (confirm; workspaces become loose) | unchanged |
| `Space` / `Enter` | fold / unfold | unchanged |
| `A-J` / `A-K` | move section down / up | unchanged |
| `A-n` | new workspace joins this section | new workspace joins the cursor row's section, if any |

`A-N` is free in the Sessions view (Planning uses it for "new subtask"). The `Char('N')` / `Char('n')+SHIFT` idiom matches the existing `A-W` / `A-R` / `A-H` handling.

### Inheritance at creation time

`create_local_session` and `create_remote_session` capture `cursor_section_id()` before spawning and record it for the new workspace id after the push. Planning-view launches have no sidebar cursor, so they land loose unless the task tree places them.

### Persistence

`save_session_manifest` writes both fields; `App::new` hydrates them, dropping assignments whose section or workspace no longer exists.

### Alternatives considered

| Alternative | Why rejected |
|---|---|
| Store the section on the API task row (`metadata.section`) | Sections are the Owner's display state for one board; the planning API is shared across machines and agents and would need write plumbing for a pure-display field. |
| Render the full subtask tree in the sidebar | Infinite depth, poor readability; the Owner's stated need is one grouping level. |
| Section field on `Workspace` / `ManifestWorkspace` | Touches every `Workspace { .. }` literal (~40 sites); the sidecar map mirrors `task_colors` and needs none of that. |
| Sections in the Status sub-view too | Status is a flat running/idle partition; grouping would fight its sort. Can be added later behind the same resolver. |

## Implementation plan

1. `daemon/src/manifest.rs`: `SidebarSection`, `sections`, `workspace_sections` + round-trip test.
2. `tui/src/app.rs` / `persist.rs`: App fields, hydrate, save.
3. `tui/src/app/nav.rs`: `Cursor::Section`, `VisualItem::SectionHeader`, `section_of_workspace`, layout in `visual_items_task`, navigate/clamp, fold toggle, reorder.
4. `tui/src/app/draw.rs`: header row, in-section indent, settings forms, help footer.
5. `tui/src/app/input.rs`: `A-N`, `InputMode::SectionSettings`, Section field on Workspace/Task settings, delete confirm, Space/Enter fold, `A-J/K`.
6. `tui/src/app/lifecycle.rs`: A-n inheritance in `create_local_session` / `create_remote_session`.
7. Tests: layout (explicit + inherited + folded + loose), cursor clamp on delete, manifest round-trip.
8. Docs: CLAUDE.md keybindings.
