# Sidebar sections

## Sessions sidebar

The Workspaces (`Task`) sub-view draws org member blocks. Each block has the member's main row, assistants and workers, with reports nested under their manager. Team labels group members, and continuous teams appear in boxes with their member subtrees. Leader marks follow the manager hierarchy. Space or Enter folds synthetic headers; Alt+Space folds a member block on its main row. When `org.snapshot` is unavailable, the sidebar uses the old workspace tree.

Owner creates sections with Alt+N. On a member's main row, Alt+e offers **Section (member)**: `auto` follows the manager's section, `none` leaves the member loose, and an explicit section assigns the member. Assistants and workers follow their member. Alt+n inside a section assigns the new member there. Section headers support Space/Enter to fold, Alt+e to edit, Alt+x to delete, and Alt+J/Alt+K to reorder.

Ctrl+Alt+e on any session row opens its checkout's workspace settings, which offer **Section override (this checkout)** (Alt+e on that row stays the session/member form). A checkout assigned to a different section appears there as a `↳ M · <checkout>` continuation. Overrides that would split a block—on the checkout holding a member's main or inside a continuous box—are ignored and shown on the one-time conflict list. Migration copied the old workspace sections: main checkouts supply member assignments, unambiguous other checkouts become overrides, and unresolved choices appear on the conflict list.

## Agent tools: remote section assignment

Open the updated laptop TUI once to publish the section catalogue to the daemons. `list_sidebar_sections()` shows published choices and receipts. `set_session_section(section="<section ID>", session_id="<CM session UID>")` queues a member assignment; omit `session_id` for yourself. Use `workspace=True` for a checkout override. A section may be a stable ID, a unique exact name, `auto`, or `none`. Session scope applies; a workspace override requires scope over its live sessions.

A queued request is not proof of rendering. It survives viewer disconnection and daemon restarts. Inspect pending and observed choices and receipts with `list_sidebar_sections()` after the viewer reconnects. A deleted destination returns `section_deleted`; select a current section and retry. The viewer owns section definitions and layout.

## Layout

Section colors tint their rows and headers. Continuous teams keep their boxed headers and subtrees.

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

The second row, **Activity badges**, is a checkbox (Space or ←/→ toggles it,
↑/↓ moves between rows); it previews live and saves with the tint as
`activity_badges = true|false` (default on).

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

### Activity badge

Each live agent row ends with a compact, right-aligned badge saying what the
agent is doing right now: a mark plus at most a time or a count, never more
than 6 cells. The F6 org chart cards show the same badge. The full wording
(job labels, the tool asking permission, the error kind) is in the A-i peek's
`Doing` line and the F6 info pane's `doing` line, e.g. `background: cargo
test --workspace`, `waiting on permission: Bash for 1m`, `rate limited
(usageLimitExceeded), resumes in 12m`.

| Badge | Meaning |
|---|---|
| `⚙ 3m`, `⚙ 3m+2` | working on its turn for 3 minutes (with two background jobs). Past 6 cells the space goes, then the job count (`⚙12m+2`, `⚙12m`) |
| `⧗2`, `⧗` | turn over, background jobs still running: how many, when known. Never the job's label |
| `☛` | waiting on a human (permission, a question, input) |
| `⚠12m`, `⚠` | stopped on a rate or usage limit (Claude `rate_limit`, Codex `usageLimitExceeded`, quota/429), with the time until it resumes when known. Drawn reversed and bold so sessions that need waking stand out |
| `✕` | stopped on another error, such as a server error |
| `↻` | the engine is retrying a failed request |
| `⏱6m` | working, but no progress for 6 minutes |

Idle, starting, unknown and exited sessions have no badge; their state glyph
says enough. Rows that already carry a status (hidden, owner-blocked, revive
failed) show none either. The session name has priority: when the row is too
narrow for the whole name, a blank cell and the badge, the badge is dropped
and the name is never cut for it.

The badge reads only the daemon's `agent_state` (doc/SESSION_STATE.md),
which the TUI already receives on `manifest.watch`; it adds no polling. The
engines do not report the tool a working session is running (`open_tool` is
filled only while a permission prompt is open), so a working badge shows how
long the turn has run rather than the current tool. The marks live in
`symbols::doing` (`tui/src/app/symbols.rs`), the label choice in
`agent_state::badge`. Turn badges off with the second row of F9 Global
Settings; the A-i peek and the info pane keep their full line either way.
