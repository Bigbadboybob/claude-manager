# Sidebar sections

## Summary

Owner-created, collapsible sections in the Sessions-view sidebar (Task sub-view) that group workspaces by project. A section is a display construct stored in the TUI manifest. The viewer publishes its catalogue to each daemon; agent tools can queue durable workspace assignment changes. Sections remain separate from planning initiatives. Membership is explicit per workspace, and a workspace without an explicit assignment inherits its parent task's section through the task tree, so an agent's `create_subtask` lands under its parent's section automatically.

## Agent tools: remote subsection assignment

After installing the updated laptop TUI, open it once so it publishes its existing
section catalogue to the configured daemons. The new tools appear on the next MCP connection. If your client cannot refresh
its MCP connection in place, use the shell fallback below to keep the running
agent session intact.

```python
list_sidebar_sections()
set_session_section(session_id="<CM session UID>", section="<section ID>")
set_session_section(session_id="<scout UID>", section="auto")
set_session_section(session_id="<scout UID>", section="none")
```

`session_id` is a **CM session UID** from `list_sessions`, not a Codex conversation
ID; omit it to change your own workspace. `section` accepts a stable ID or a
unique exact section name. Use IDs when names are ambiguous or literally
`auto`/`none`. Agents should follow Owner's requested organization.

- The change affects the **whole workspace**, as does Alt+E → Section. All live
  sessions sharing it must be within the caller's normal task-tree/workspace
  scope, unless the caller has Owner-granted global permissions. Listing only
  exposes caller-visible workspace membership; `can_assign=false` identifies a
  shared workspace with an out-of-scope session.
- Auto removes the explicit override and follows the existing parent-task/filer
  inheritance. None explicitly keeps the workspace outside sections. Descendants
  on Auto follow changes to their parent; explicit descendant overrides remain.
- Tools address the daemon hosting the caller. They do not transfer sessions,
  edit task parents or initiative ownership, move worktrees, or restart workers.
  Continuous tasks retain their dedicated panel placement.
- Assignment returns **`status="queued"`**, not proof that the laptop rendered it.
  Requests survive viewer disconnection and daemon brain restarts. Identical
  pending requests coalesce; a different request supersedes the pending choice
  for that workspace. The existing manifest stream delivers them on reconnect.
- Call `list_sidebar_sections` to check `workspaces[...].pending` versus
  `observed.choice`, `observed.effective_section_id`, and `observed.receipt`.
  `choice=null` means Auto and `choice=""` means None. Observed values are the
  latest viewer publication and can be stale while it is offline. No publication
  yet produces an explicit install/reopen error rather than a false success.
- If Owner deleted the destination before delivery, the viewer preserves the
  workspace's choice and returns a `section_deleted` receipt. List the current
  sections and make a new request. A successful receipt has `status="applied"`.

Section definitions, folding, appearance, and the saved layout remain owned by
Owner's laptop viewer. This is not synchronization of independent viewers' layout
preferences; use the normal Owner viewer as the publishing layout.

### Existing agents with a cached MCP tool list

The installed daemon RPCs also work from an existing agent's shell. Run on the
agent's owning cloud host (`cm-sessions` or `cm-manager`), using **your own CM UID
returned by `ping`** for the caller. Some Codex shell tool environments omit CM's
environment variables even though the MCP connection has them, so supply the
caller UID and host-local socket explicitly:

```bash
CM_TUI_SESSION_ID='<your own UID from ping>' \
CM_DAEMON_SOCKET="$HOME/.cm/daemon.sock" \
PYTHONPATH=/opt/cm-daemon python3 - <<'PYCODE'
import json
from mcp_server.control_client import call
print(json.dumps(call("sidebar.list", {}), indent=2))
# After resolving the destination and the target CM session UID:
# print(call("sidebar.assign", {"session_id": "<scout UID>", "section": "<section ID>"}))
PYCODE
```

This sends the same Session-caller RPC as the new tools, with the same scope
checks and queued/observed semantics. Put the **target** UID in `session_id`,
never in the caller environment. No Operator token or agent restart is needed.
An updated viewer must still connect once to publish its existing catalogue.

### Delivery and recovery

The daemon persists `~/.cm/sidebar-sections.json` before broadcasting requests.
`sidebar.list` and `sidebar.assign` authenticate the caller and retain normal
session scope. `sidebar.publish` is Operator-only and carries a catalogue plus
observed membership/receipts. It uses the existing coalescing background push
worker; no SSH connection per session or new steady-state network polling is
introduced. Failed publications retry while successful identical payloads are
suppressed.

The viewer saves membership and its `sidebar_receipts` manifest sidecar together
before acknowledgement. This protects a later Owner edit from replay when the
previous acknowledgement was lost. A stale acknowledgement cannot clear a newer
request. A failed save leaves the request pending; damaged daemon state fails
explicitly and is preserved. These optional sidecars require no holder upgrade
or worker lifecycle change. Older viewers keep working, but cannot publish or
apply these requests.

## Problem

Subtasks nest to arbitrary depth, and rendering that tree faithfully in one sidebar is unreadable. What the Owner actually needs is one level of grouping: "everything for project X", where X is a stream of work with one top-level task and a handful of subtasks and side tasks. Today the sidebar is a flat list of workspaces (pinned first, then status-ranked) with no way to draw that boundary.

## Goals

- Create, rename, recolor, reorder, fold, and delete sections from the Sessions view.
- Assign a workspace to a section from the A-e settings form on a workspace or task row.
- A new workspace created with A-n while the cursor is inside a section joins that section.
- A subtask (task with `parent_task_id`) whose parent's workspace is in a section renders in that section without any assignment.
- Everything persists in `~/.cm/tui-sessions.json`, including fold state.

## Non-goals

- Agents do not create, rename, delete, reorder, recolor, or fold sections. Those remain Owner controls; agents can change workspace membership within their normal scope.
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
The choice controls both the heading accent and its background tint.

**F9** opens **Global Settings** from Sessions, Planning, or Messages. The section
tint defaults to **2×** the original RGB intensity. Use **←/→** (or **-/+**) to
preview changes in steps of 0.25, **Enter** to save, **Esc** to cancel, or **Home**
to restore the 2× default. The range is 0–5; **0 turns the tint off**, retaining
section headings and closing rules. The dialog includes color samples and leaves
the task sidebar visible when space permits.

The preference applies across all projects in that laptop's TUI and persists in
`~/.cm/tui-settings.toml`, separate from session and daemon state. Saving applies
immediately: no rebuild, deployment, or TUI restart. Direct file edits are read
at the next TUI launch. A missing setting defaults to 2×:

```toml
sidebar_tint_strength = 2.0
```

Invalid settings fall back to 2× and show an error in Global Settings. A malformed
TOML file is preserved rather than overwritten on save; fix it before saving.
Other settings keys are preserved when saving (TOML comments/formatting are not).

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
| `F9` | Global Settings: section tint strength | Applies across projects; live preview and persistent save |
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
