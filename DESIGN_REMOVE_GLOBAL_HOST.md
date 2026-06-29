# Design: Retire the global `active_host` — host is a per-workspace attribute

Status: **✅ SHIPPED (Phases A–D, 2026-06-29).** Branch
`cm/daemon-side-workflow-execution-autonomous-cloud-wo`. TUI-only — runs
locally, no deploy. `cargo test -p claude-manager-tui` green (657 passed). See
the "Completion" section at the bottom.
Scope: `tui/` only. No daemon, API, or workflow changes. (One additive daemon
crate change landed: `ManifestWorkspace.host_id` + `impl Default for HostId`,
since the manifest schema is daemon-owned.)

## Problem

`A-H` cycles a single global `App::active_host`. That model is wrong:

- **Host is a property of a *workspace*, not of the TUI.** A workspace owns
  exactly one worktree, and that worktree lives on exactly one host (the local
  machine, or `manager`, …). Once a task is created on a host, every session
  you add to it must land on that same host — there is no meaningful "switch
  this existing task to another host" operation, because the worktree can't
  move. So "which host" is decided *once, at task-creation time*, and is then
  fixed for the life of the workspace.
- **A global "current host" is therefore nonsensical.** It conflates "where do
  new *tasks* default to" (a one-time pick, already exposed by the A-n form's
  host picker) with a persistent mode the operator has to remember to toggle.
- **It actively misleads.** The sidebar already renders *all* hosts
  (`visual_items_status_multihost`), so `active_host` doesn't filter anything —
  it only (a) seeds the A-n picker default, (b) silently pins A-s
  (add-session-to-existing-task) to the global host instead of the task's host,
  and (c) paints a `*` next to one host header. (b) is a latent bug: adding a
  session to a `manager` task while `active_host == local` tags the new session
  `local` and it tries to attach a local PTY to a remote worktree.

This is what made the bug-triage orchestrator confusing to reach: nothing about
showing a remote session should require touching a global mode. The focused fix
(all-hosts adoption) already removed the *display* dependency on `active_host`.
This doc removes the abstraction entirely.

## Current state (what `active_host` actually gates)

`active_host: HostId` on `App` (app.rs:3571), initialized from the default host
(3834), with ~184 references in `app.rs`. Real uses, after the all-hosts-adopt
fix:

| Use | Site(s) | Keep? |
|-----|---------|-------|
| A-n new-task form host-picker **default** | form seeded from `active_host`; submit threads `chosen_host` (11024) — already routes by the *picked* host, not `active_host` | replace default source |
| A-s / add-session host (`ts.host_id = active_host`) | 6422, 6609, 12591, 12784 (+ the `try_spawn`/`create_session` host args feeding them) | **change**: inherit workspace host |
| Sidebar "active host" `*`/yellow highlight | 14142, `VisualItem::HostHeader` render | **remove** |
| `A-H` cycle | `cycle_active_host` (10467), keybindings 9619/9691 | **remove** |
| Misc snapshots threaded into spawn paths | 6282/6519/12488/12722 etc. | rewrite to workspace host |

`Workspace` (app.rs, struct) has **no** host field today — a workspace's host is
implicit in its sessions' `host_id`. That's the gap to close.

## Target model

1. **`Workspace` gains `host_id: HostId`** — the host its worktree lives on, set
   once at creation. Persisted in `tui-sessions.json`. This becomes the single
   source of truth for "where do this task's sessions run."
2. **Task creation (A-n / A-N / planning launch) picks the host** via the
   existing form picker, and writes it to `Workspace.host_id`. The picker's
   default becomes `local` (the overwhelmingly common case) rather than a
   global mode — optionally "last-picked host this session" as a soft default
   (in-memory only, not a persisted global).
3. **Add-session (A-s) and every other "new session into an existing
   workspace" path inherit `workspace.host_id`** — no host choice, no global
   read. This fixes the latent remote-worktree/local-PTY mismatch.
4. **The sidebar always shows all hosts** (already true) with **no "active"
   emphasis** — every host header renders identically.
5. **`A-H`, `cycle_active_host`, and `App::active_host` are deleted.**

## Migration plan (phased, each independently shippable + testable)

**Phase A — add `Workspace.host_id` (additive, no behavior change).**
- Add the field; default `local`. Plumb through the manifest
  serializer/deserializer with backward-compat: a persisted workspace lacking
  `host_id` derives it from its first session's `host_id` (else `local`).
- Set it at every workspace-creation site to the host already being used there
  (so behavior is identical). The synthetic adoption workspace (the all-hosts
  adopt path) sets it to the adopted session's host.
- Tests: round-trip a `host_id="manager"` workspace; legacy manifest (no field)
  derives from sessions.

**Phase B — route add-session by workspace host (fixes the latent bug).**
- Change the A-s / add-session paths (6422, 6609, 12591, 12784 and their
  `try_spawn_via_daemon` / `create_session` host args) to read
  `workspace.host_id` instead of `self.active_host`.
- Test: add a session to a `manager` workspace while `active_host == local` →
  the new session is tagged `manager` and routed to the manager daemon. (This
  test fails today.)

**Phase C — make A-n's host the workspace host; default not-global.**
- A-n already threads `chosen_host`; write it to the new `Workspace.host_id`.
- Change the picker default from `self.active_host` to `local` (or in-memory
  last-pick). Planning-launch and A-N (subtask) inherit the parent/explicit
  host.

**Phase D — delete `active_host` + `A-H`.**
- Remove `App::active_host`, `cycle_active_host`, the `A-H` keybinding, and the
  HostHeader active highlight (all headers render neutral). Update the
  status-bar/help text that mentions A-H. Delete `active_host`-specific tests
  (`t_g3e_active_host_cycle`, the `* active` assertions); keep/retarget the
  `a_n_submit_routes_by_chosen_host_not_active_host`-style tests to assert
  routing by workspace host.

## Edge cases & risks

- **A workspace with mixed-host sessions** shouldn't exist (one worktree, one
  host). After Phase B it can't be created. Phase A's derivation picks the first
  session's host; if a legacy manifest somehow has mixed hosts, log + take the
  first (and Phase B prevents recurrence).
- **Backward-compat manifests**: the derive-from-sessions fallback (Phase A)
  keeps old `tui-sessions.json` files loading without a host field.
- **The default-host pick**: dropping the global means a user who *usually*
  works on `manager` re-picks it per task. Mitigate with an in-memory
  last-pick default (resets on restart) — explicitly NOT a persisted global,
  per the "no global host mode" principle.
- **Help/onboarding**: remove A-H from CLAUDE.md keybinding docs and the TUI
  status bar.

## Test plan

- Phase A: manifest round-trip + legacy-derive unit tests.
- Phase B: add-session-on-manager-while-active-local routes to manager (the bug
  this fixes).
- Phase C: A-n writes workspace host; default is local.
- Phase D: no `active_host` symbol remains; sidebar renders no `*`; all existing
  multihost render tests pass with neutral headers.
- Full `cargo test -p claude-manager-tui` green at each phase.

## Out of scope

Daemon/API/workflow code (host already per-session there). `hosts.toml` format
(unchanged). The reconnect/tunnel-warming machinery (unchanged — `spawn_per_host`
already warms every configured host on startup, which is exactly what makes the
always-show-all-hosts model work).

## Completion (2026-06-29)

All four phases landed. What shipped, against the plan:

- **Phase A** — `Workspace.host_id` + `ManifestWorkspace.host_id` (both default
  `local`, `#[serde(default)]`). Backward-compat: a persisted workspace with no
  field derives from its first session's host (else `local`). Every
  workspace-creation site sets it to the host already in use there (save/restore,
  adopt — `resolve_adopt_workspace` gained a `host: &HostId` param —, cloud
  provision/pull/task-sync → local). `impl Default for HostId` added so
  `ManifestWorkspace` keeps deriving `Default`. Tests:
  `manifest_workspace_host_id_round_trips_and_legacy_defaults_local`.
- **Phase B** — add-session / tombstone-restore / designer-resurrect /
  `launch_into_workspace` now read `workspace.host_id`, not the global. The
  latent bug (add-session-to-manager-task-while-global-local) is fixed and pinned
  by `launch_into_workspace_guards_on_workspace_host_not_global`.
- **Phase C** — A-n form defaults the host field to `local`
  (`a_n_form_defaults_host_to_local`); `launch_from_plan` uses a local host var.
  Routing is by the form's chosen host (`a_n_submit_routes_by_chosen_host`).
- **Phase D** — deleted `App::active_host`, `cycle_active_host`, the `A-H`
  host-cycler keybinding (A-H now toggles session-hidden), and the HostHeader
  `*`/yellow active highlight (headers render neutral). Updated the
  `guard_local_host_only` error message (no more "A-H"), CLAUDE.md, and the
  status-bar help. Deleted the cycle-specific tests
  (`t_g3e_active_host_cycle`, `cycle_active_host_single_host_shows_hint`,
  `t_g3e_new_session_inherits`); retargeted the host-routing /
  watch-consumer / pool-fallback / mcp-spawn tests off `active_host` onto the
  per-workspace / per-caller model.

Net: the global "current host" concept is gone. Host is decided once, at
task-creation time, and is fixed for the workspace's life. `cargo test -p
claude-manager-tui` → 657 passed.
