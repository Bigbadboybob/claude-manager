# Decision: migrate `SpawnTarget::TuiLocal` sites to `Daemon` instead of implementing daemon→TUI cross-route proxy

**Date:** 2026-05-27
**Branch:** `cm/support-pushing-and-pulling-to-dedicated-instance`
**Status:** Decided — migration in progress; cross-route attempt parked on `cross-route-attempt-v1` (commit `4093ec4`).

## Problem

Phase 1's daemon-mandatory flip (slice 10f) migrated only the `A-n` / `A-s` "fresh local session of type claude or codex" path to spawn through the daemon. The other ~7 spawn sites in `tui/src/app.rs` and `tui/src/workflow/controller.rs` still use `SpawnTarget::TuiLocal`:

- Manifest-restore on TUI startup (sessions that were daemon-owned before a TUI restart come back as TUI-owned)
- Workflow respawns when a fresh-context role reactivates
- `bash` sessions (the daemon's `SpawnParams` only knows `claude-code` / `codex`)
- A-l (cloud pull) `spawn_resumed_session` path
- A-f cloud workflow launch
- Various replay / resume paths

Sessions spawned via `SpawnTarget::TuiLocal` are registered only in the TUI's `App.workspaces` and pushed to the daemon as snapshot rows in `state.tui_sessions`. They are NOT in `state.sessions`. The daemon's MCP handlers (`send_input`, `kill_session`, `read_session_output`, plus the `resolve_authorized_session` resolver and `list_sessions` lister) only know how to operate on `state.sessions`, so they reject TUI-owned targets with a `Conflict` error:

```
target session '<uid>' is TUI-owned; the daemon does not proxy
mutations / reads to TUI-owned sessions yet. Route this call through
the TUI socket directly, or wait for the post-Phase-1 cross-route
work to land.
```

User-visible consequence: orchestrator agents (MCP-driven roles like the design-doc-impl-loop orchestrator) can drive only daemon-spawned sessions. They cannot read transcripts from or send input to the designer session that wrote a doc, the worker session that's actively iterating, or any session that survived a TUI restart. Today's recovery flow (`/resume` inside a freshly-spawned PTY to bring back a prior transcript) compounded the problem: sessions touched that way ended up TUI-owned and silently unreachable.

## Options considered

### Option A: daemon→TUI cross-route proxy (the inline comment's "post-Phase-1 cross-route work")

- `send_input` / `kill_session` for TUI-owned targets: daemon opens a `UnixStream` to `~/.cm/tui.sock` and forwards the JSON-RPC request, preserving `caller_uid`. TUI re-authorizes and replies; daemon returns the response verbatim.
- `read_session_output` for TUI-owned: daemon reads the transcript file off disk directly (TUI socket doesn't implement this method).
- `resolve_authorized_session` / `list_sessions`: gain TUI-owned branches that project snapshot fields into the wire shape.

### Option B: migrate every `SpawnTarget::TuiLocal` site to `SpawnTarget::Daemon`

- Each of the ~7 spawn sites in `tui/src/app.rs` + the one in `tui/src/workflow/controller.rs` swaps to calling `mcp_start_session` over the daemon's RPC socket instead of `crate::session::spawn_agent_session` directly.
- Daemon's `SpawnParams` gains `bash` as a supported engine so bash sessions also become daemon-owned.
- The workflow controller's respawn lifecycle (kill old PTY, spawn fresh-context replacement) routes through daemon RPCs (`kill_session` + `mcp_start_session`) instead of in-process.
- The manifest-restore path on TUI startup uses `mcp_start_session` with `--resume <transcript_id>` arguments so resumed sessions are daemon-registered from the start.
- The cross-route proxy code becomes unnecessary; the TUI-owned vs daemon-owned distinction collapses for local sessions; the daemon error message about "cross-route work to land" goes away.

## Why we chose Option B

We attempted Option A first and got it most of the way to a converged state on `cross-route-attempt-v1`. The work shipped:

- Forwarding logic for the three handlers
- New `TuiSessionSnapshot` fields: `transcript_path`, `workspace_id`, `generation`, `idle`, `exited`
- A push trigger hooked to idle/running/exited transitions plus a `pending_enter` grace window and per-tick sweep
- 14 cross-route acceptance tests
- An `EnforceHere` vs `DeferToTui` taskless-caller auth policy split

After 24+ review rounds the slice still hadn't fully converged. Each round produced a real bug fix but the fix introduced a new state-freshness gap that the next round caught. The pattern wasn't sloppy work — it was the underlying shape of the problem: **Option A is a sync layer between two competing views of session state, and every TUI-side state transition is a potential opportunity for the daemon's mirror to drift.** Plumbing every new field, every push trigger, every fail-closed default has fundamental complexity that compounds with each session attribute.

Option B dissolves the sync problem. If every session is daemon-owned, the daemon is the single source of truth; there's no mirror to keep fresh. The migration is bigger work in raw LOC and touches more files, but each individual call-site swap is a localized change with clear semantics ("this site used to spawn locally; now it asks the daemon to spawn"). The blast radius is wider but the cognitive load per change is lower.

There is also an architectural cleanliness argument: the `SpawnTarget::TuiLocal` variant existed only because the migration was incomplete. Completing the migration removes the concept entirely. Future maintainers don't have to remember which call sites produce which kind of session.

## Consequences

**What we're taking on:**

- Migration of ~7 spawn sites in `tui/src/app.rs` from `spawn_agent_session` + `SpawnTarget::TuiLocal` to `mcp_start_session` RPC via the daemon
- Workflow respawn lifecycle in `tui/src/workflow/controller.rs` routed through daemon RPCs
- Adding `bash` to `daemon::SpawnParams` / `start_session` so bash sessions are daemon-owned
- Manifest-restore path: pass `--resume <transcript_id>` through to claude when re-spawning daemon-side, so resumed sessions register with the right `transcript_id` from spawn time (no rebind dance, no `/resume`-inside-PTY workaround needed)
- The cross-route refusal in `daemon/src/control/methods.rs:1184` becomes unreachable code that can be deleted
- The `state.tui_sessions` snapshot push from TUI is still needed for *taskless* sessions and for workspace metadata, but its role narrows

**What we're letting go of (for now):**

- The cross-route work on `cross-route-attempt-v1` becomes dead code we don't merge. If migration turns out infeasible mid-flight, we can revisit.
- 14 cross-route tests preserved on the escape-hatch branch are not in the cross-route migration scope. If we ever need to ship Option A, the work is recoverable verbatim.

**Out of scope for this slice:**

- TLS streaming dispatch (separate slice, deferred from 12h)
- The `tui-owned` Conflict error string and its inline `// post-Phase-1 cross-route work to land` doc-comment get removed *only after* all migration sites are done — incremental migration with the refusal still firing for un-migrated sites is acceptable mid-slice
- Cloud-mode A-f path: cloud workers don't run a daemon today, so the cloud branch stays TUI-local until the cloud worker design separately gains a daemon component

## Recovery path if migration stalls

The cross-route work is on `cross-route-attempt-v1`. To resume Option A:

```bash
git checkout cross-route-attempt-v1
# Work continues from commit 4093ec4
```

A fresh feedback workflow against that branch can pick up the remaining 1-3 convergence rounds the cross-route work needed.

## References

- Phase 1 design doc: `doc/persistent-host-daemon.md`
- Phase 3 slice plan: `daemon/NOTES.md`
- Cross-route refusal: `daemon/src/control/methods.rs:1175-1188` (`return_auth_error_if_denied_with_state`)
- Daemon spawn entry point: `daemon/src/session.rs::SpawnParams`, `daemon/src/control/methods.rs::start_session`
- TUI spawn helper: `tui/src/session.rs::spawn_agent_session`
- `SpawnTarget` enum: `tui/src/mcp_config.rs`
