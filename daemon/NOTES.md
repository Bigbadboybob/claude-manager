# `app.rs` rewire — slicing plan

**PHASE 1 SHIPPED.** All named acceptance criteria in
`doc/persistent-host-daemon.md` are verified end-to-end on this branch.
The load-bearing reconnect / ring-buffer property is pinned by the two
tests in `daemon/src/control/stream.rs::tests`
(`attach_stream_replay_survives_disconnect_reconnect` +
`attach_stream_replay_survives_multiple_disconnect_reconnect`, slice
10g) — both mutation-verified against `PtyByteFanout::subscribe`'s
replay path.

Phase 1 final shape: 28 commits, 75 files (+60,246 / -4,029 LOC),
+701 tests added across cm-daemon / claude-manager-tui / Python
mcp_server. Design doc `doc/persistent-host-daemon.md` was authored
once and **not modified** during the slice arc (Q4 stability
directive — the doc text is the historical record of the rollout
strategy, including the now-removed `CM_USE_DAEMON_SOCKET` opt-in
that slice 10f flipped to mandatory).

Phase-1 deliverables — every daemon-side primitive the design doc
names: protocol types, scaffold, worktree relocation, attach-ticket
allocator, workflow-submodule relocation, PtyByteFanout, term_shim
FSM, LastExit schema, ManifestWatcher, reaper with per-spawn
baseline, detached daemon spawn, full app.rs RPC rewire (10c-e),
daemon-side MCP surface (10d-mcp-surface), workflow controller
relocation (10d-workflow-controller), manifest ownership flip
(10e), default flip to daemon-mandatory (10f), and the reconnect
ring-buffer replay test (10g). Plus the pure-function
`session.attach` / `attach.open` handlers, daemon-side memory-cap
watcher with per-spawn kill-log baseline, and the per-method
control-socket routing that lets workflow methods continue talking
to the TUI socket while session/PTY methods route to the daemon.

The original "what remains is the load-bearing slice" framing is
preserved below as the design history — what FOLLOWS the framing is
the actual committable plan that landed.

## Field-level inventory

### App fields that move to the daemon

| Field on `App` | Daemon-side owner | Notes |
|---|---|---|
| `workspaces: Vec<Workspace>` | Daemon `SessionsRegistry` + `WorkspacesRegistry` | Per-workspace session list + tombstones. Daemon owns the persistent state; TUI keeps an in-memory mirror updated via `manifest.watch`. |
| `Workspace.sessions: Vec<TerminalSession>` | Daemon `DaemonSession` map keyed by uid | TUI hydrates a `ClientSession` (Term + EventLoop + shim) per attached uid. Detached sessions exist only daemon-side. |
| `Workspace.tombstones: Vec<SessionTombstone>` | Daemon manifest | 30-day retention stays in `~/.cm/tui-sessions.json`. |
| Session PTY (`Session.term`, `Session.sender`, `Session.pty_writer`, memory cap, cgroup) | Daemon `DaemonSession` | The split per the doc's "Session struct split" section: daemon owns OS PTY + cap + cgroup + exited flag + wakeup tracker. |
| Workflow runs (currently in `controller.rs`, the one workflow module still TUI-side) | Daemon workflow controller | Already-relocated submodules become the consumers; controller follows once Session moves daemon-side. |
| Manifest persistence (`tui-sessions.json` reads/writes) | Daemon `ManifestPersister` | Daemon does the file I/O; TUI subscribes via `manifest.watch` for diffs. Slice-9 `last_exit` schema add already in place. |
| Memory-cap kill detection (currently `session_watch.rs`) | Daemon reaper | Slice-12 producer already lands; full path wires when sessions live daemon-side. |
| MCP control socket dispatch (`control/server.rs` + `queue.rs` + `methods.rs`) | Daemon dispatch table | Task #17. Methods take `&App` today; they take `&DaemonState` post-rewire. |

### App fields that stay TUI-side

| Field | Rationale |
|---|---|
| Cursor / focus state | Pure UI — no persistent meaning across daemon restart. |
| `InputMode` (modal dialogs, pickers) | Local input state; never written to disk. |
| Viewport / scroll position | Render-time state. |
| Activity feed (`A-,`) | Display-only event log; subscribes to daemon events. |
| `Config` (theme, layout knobs) | TUI rendering config; unrelated to daemon. |
| Planning view's `PlanningClient` (talks to FastAPI) | Already host-independent per the design doc. |
| Toast / notification state | Display layer. |

### Bridge layer (TUI-side wrappers that proxy to daemon)

- **`ClientSession`** — `Term<EventProxy>` + `EventLoop` + `StreamReader`/`StreamWriter` shim (slice 8). Constructed in the TUI; reads PTY bytes off the daemon's `attach.open`-bootstrapped dedicated connection.
- **`SessionsView` mirror** — local `BTreeMap<uid, ManifestEntry>` populated by `manifest.watch` subscribe + diffs. The TUI reads this for rendering; never writes directly.
- **`WorkflowView` mirror** — local `Vec<WorkflowRun>` populated by `events.subscribe` (slice 2 of the doc — that's the *next* RPC after Phase 1) or, for the Phase-1 staging, by file-tailing `~/.cm/workflow-runs/*/events.jsonl` on the daemon's filesystem.
- **Operator RPC client** — a thin `cm_daemon_client` wrapper around the existing `control::protocol` types (already daemon-relocated). One control connection per TUI process, plus one dedicated connection per attached session.

## Slice sequence

Each slice leaves the tree compiling and the default user-visible path (TUI bound to `tui.sock`, daemon optional) working. The opt-in gate `CM_USE_DAEMON_SOCKET=1` graduates from "exercise the daemon scaffold" through each slice until the final flip makes it the default.

### Slice 10a — Daemon-side App-state shell (SHIPPED)

Subdivided into two commits when it turned out (a) ManifestEntry/Workspace/SessionTombstone relocation touches ~50 sites across `app.rs`, `control/methods.rs`, `agent/`, and `workflow/controller.rs`, and (b) the placeholder-dispatcher work is independently testable. Same overall goal.

**10a-shell:**

- New `daemon::state::DaemonState` carrying the daemon's mutable session/manifest state. Skeletal in this commit — just `sessions: HashMap<String, DaemonSession>` from slice 7. Full `workspaces` / `tombstones` arrive with 10a-types.
- Daemon's accept loop wires from `drop(stream)` (slice 2 placeholder) to a real read-dispatch-write loop. Each connection: read 4-byte length prefix + JSON body, deserialize into `Request`, call `dispatch_request(state, req)`, serialize the `Response` with length prefix, close.
- `dispatch_request` returns `UnknownMethod` for every method as a placeholder. The point is to prove the wire path works end-to-end before slice 10b relocates `methods.rs` and starts routing to real handlers. `session_attach` / `attach_open` (the pure functions from slice 5's review) wire in at 10b too.

**10a-types** (next commit):

- Relocate `ManifestEntry`, `Workspace`, `SessionTombstone` from `tui/src/app.rs` to `daemon/src/manifest.rs` alongside the existing `LastExit` and `ManifestDiff`. TUI gains transitional re-export shims so call sites keep compiling.
- `DaemonState` grows `workspaces` and `tombstones` fields.
- Daemon's startup loads the existing `~/.cm/tui-sessions.json` into `DaemonState` (read-only — TUI still writes the file until slice 10e flips manifest ownership).

**Working-set check:** TUI ignores the daemon. Default path unchanged. `cargo test --workspace` green after each sub-slice.

### Slice 10b — Move `control/methods.rs` to daemon (SHIPPED)

- Relocate `methods.rs` from `tui/src/control/` to `daemon/src/control/methods.rs`. Methods take `&mut DaemonState` instead of `&mut App`.
- TUI keeps `control/server.rs` + `control/queue.rs` for now — its socket still services MCP agents.
- Daemon's dispatcher wires the relocated methods to its own accept loop. With opt-in on, MCP agents (which got `CM_DAEMON_SOCKET` injected by slice 11's env branch) actually reach handlers.
- Pure-function `session_attach` / `attach_open` wire into the dispatcher as one-liners (the `TODO(slice-17)` comments in `daemon/src/attach.rs`).
- TUI's own `dispatch_control` is a thin shim that re-routes to RPC against the daemon if opt-in is on, else falls back to the legacy local dispatch.

**Working-set check:** with opt-in off, default MCP path unchanged. With opt-in on, MCP works against the daemon. Either way the tree is green.

### Slice 10c — Session-spawn split (SHIPPED across 10c-{a,b,c,d,e-1,e-2,e-3a,e-3b,e-3c} + ~7 review-fix rounds)

The slice the reviewer flagged as the hardest one to keep working-set-green. Sub-divided aggressively below; each sub-slice leaves the tree compiling and the default `A-n`/`A-s` flow unchanged. The opt-in (`CM_USE_DAEMON_SOCKET=1`) gradually unlocks daemon-side session handling.

The end goal: `tui/src/session.rs::Session` (which couples alacritty `Term`, `EventLoop`, PTY fd, memory cap, cgroup, and exit tracking) splits into:
- **`DaemonSession`** (already a skeletal struct in `daemon/src/session.rs` from slice 7) — owns the OS PTY child, memory cap, cgroup, exit watcher, and the `PtyByteFanout` that broadcasts bytes to attached clients. Lives in `DaemonState.sessions: HashMap<uid, DaemonSession>`.
- **`ClientSession`** (new in 10c-e) — owns alacritty `Term` + `EventLoop` + `EventProxy`, fed by `term_shim::StreamReader`/`StreamWriter` (already built and tested in slice 8). The TUI's `Workspace.sessions` becomes `Vec<ClientSession>` (or similar).

#### Sub-slices

**10c-a — `DaemonSession::spawn` primitive.** Move PTY-child creation (the `alacritty_terminal::tty::new` call from `tui/src/session.rs:106`) into a `DaemonSession::spawn(uid, shell, args, working_dir, env, memory_cap)` method. Tests use the existing `PtyByteFanout` to confirm bytes flow from the child into the fanout. The TUI is *not* yet calling this — `tui/src/session.rs::Session::new` keeps doing what it does today.

**10c-b — daemon `start_session` method handler.** Wire a real `start_session` JSON-RPC handler (replacing the slice-10b `UnknownMethod` placeholder for this method only). Handler takes `params: { workspace_id, label, session_type, ... }` and calls `DaemonSession::spawn` from 10c-a, inserts into `DaemonState.sessions`, returns the new uid. Tested with the dispatcher unit-test pattern from 10b. Other session-mutation methods (`send_input`, `kill_session`, `read_session_output`) stay deferred until 10c-d.

**10c-c — wire `session.attach` / `attach.open` with live registry.** Now that `DaemonState.sessions` is populated, the dispatcher arm for `session.attach` can validate the requested uid against the live registry before minting a ticket (the slice-10b-review fix that punted these). `attach.open` consumes the ticket and looks up the session's `PtyByteFanout`. The stream-transition (where the dedicated connection becomes a `StreamFrame` stream attached to the fanout) is the new piece — `handle_connection` grows a branch for "this is an attach.open consumer; switch to streaming mode."

**10c-d — daemon `send_input` / `kill_session` / `read_session_output`.** Each method's body relocates from `tui/src/control/methods.rs` to `daemon/src/control/methods.rs` (taking `&mut DaemonState` instead of `&mut App`). These all need the live `DaemonState.sessions` registry from 10c-b; the relocation is mechanical once the registry exists.

**10c-e — TUI `ClientSession` + opt-in spawn rewire.** TUI's `Session::new` becomes `ClientSession::new(stream_reader, stream_writer, …)`. Behind the opt-in, the TUI's `A-n` / `A-s` flow:
1. Calls `start_session` RPC against the daemon → gets new uid.
2. Calls `session.attach { uid }` → gets a ticket.
3. Dials a fresh connection to `attach_addr`, sends `attach.open { ticket }`.
4. Constructs `ClientSession` over the resulting stream.

Opt-in off: TUI keeps the existing direct-`tty::new` path. Both paths coexist through 10e.

**10c-e was further subdivided in practice:**
- **10c-e-1** — alacritty trait impls on the `StreamReader`/`StreamWriter` wrapper (`AttachedPty`).
- **10c-e-2** — `ClientSession::new` itself, full RPC dance, returning a working `Term` + `EventLoop`. Several review rounds landed bidirectional input, TOCTOU fixes, cols/rows plumbing, backpressure, lazy memory_cap classification, Shape-B opportunistic drain.
- **10c-e-3a** — per-spawn `SpawnTarget::Daemon | TuiLocal` routing in `mcp_config::build_env`. Decouples "the daemon exists" from "this particular spawn is daemon-side." Required because workflow respawns and attach-active are still TUI-local; pre-3a a global `CM_USE_DAEMON_SOCKET` would mis-route their MCP callbacks.
- **10c-e-3b** — argv/env/cwd wire shape for `start_session`. Daemon no longer interprets a session_type tag; TUI builds full argv (with `--mcp-config`, Codex `-c` overrides, resume tokens, systemd-run wrap) via the same code path the local `Session::new` uses, with `SpawnTarget::Daemon`. Daemon execs verbatim. Element-wise argv parity tests pin the contract.
- **10c-e-3b-fix** — uid passthrough. TUI is source of truth for the uid (because MCP config bakes it at config-write time, before the daemon sees the spawn). `StartSessionParams.uid` is required and used verbatim with format-validation + collision guard.
- **10c-e-3c** — actual wire of `try_spawn_via_daemon` into `A-n`/`A-s` + interactive smoke (PTY mechanics only — see slice-ordering note below).

### Rejected findings (10c-e-3)

Standing rejections from the review cycle. Grep this section before
reopening any of these — the reasoning is recorded; rejecting in
place beats round-by-round re-litigation.

- **`Ok(buf.len())` in `term_shim::StreamWriter::write` without
  guaranteed full drain** — rounds 26, 30, 31, 33. The tail is
  drained opportunistically on inbound EventLoop calls via
  `attached_pty.rs::drain_pending` (Shape B). Quiescent-session
  caveat is the documented tradeoff. Escalate to Shape A (per-attach
  writer thread) only on smoke evidence of stuck input in practice.
  Deflection comments live at `term_shim.rs::tests`-adjacent
  `impl Write for StreamWriter` and the `Ok(buf.len())` return
  site itself.

- **Daemon-spawned MCP callers see `UnknownMethod`/`Unauthorized`
  for TUI-served methods** — rounds 19, 27, 28, 29. Intentional
  during Phase 1's migration. Daemon-side MCP surface lands in
  `10d-mcp-surface`. Adding TUI-socket fallback proxying would
  re-introduce the silent-fallback bug class we've fixed three
  times during this phase (slices 11/13/17). Deflection comment in
  `daemon/src/control/dispatch.rs` module-level doc.

**Working-set check (revised through 10c-e-3):**
- After **10c-a**: TUI behavior unchanged (`DaemonSession::spawn` exists but isn't called from production).
- After **10c-b**: TUI behavior unchanged (`start_session` daemon-side, but TUI's opt-in path doesn't call it yet).
- After **10c-c**: TUI behavior unchanged (attach methods serve real responses, but no caller exercises the stream transition yet — only integration tests).
- After **10c-d**: TUI behavior unchanged (more methods are daemon-routable but the TUI's opt-in path still spawns locally).
- After **10c-e-{1,2,3a,3b,3b-fix}**: Opt-in OFF byte-identical to today. Opt-in ON is structurally complete but `try_spawn_via_daemon` not yet called from the user-facing handlers.
- After **10c-e-3c**: Opt-in ON does the full RPC dance from a user keystroke. Smoke (PTY mechanics) is viable.

### Rejected findings (sub-2c)

Standing rejections from the sub-2c review cycle. Same protocol
as the 10c-e-3 list above — record the reasoning here so future
review doesn't re-litigate.

- **Daemon-minted sessions cannot call TUI-only methods until 10d-workflow-controller** — sub-2c review round-13. Reviewer reported: daemon-minted sessions (those spawned via `mcp_start_session`) calling `workflow_transition`, `create_subtask`, `start_workflow`, etc. reach the TUI socket per the sub-2c per-method router, but the TUI's auth check rejects them with "caller session not found" because the TUI's auth walks `App.workspaces` (TUI-minted sessions) and doesn't know about daemon-minted ones.

  This is the intentional slice boundary between sub-2c (per-method routing infrastructure) and 10d-workflow-controller (controller relocation):

  - sub-2c delivers routing infrastructure: route+method+destination bound as one decision; `DAEMON_METHODS` set authoritative; `CM_USE_DAEMON_SOCKET=1` opt-in respected; method/path resolution unified.
  - 10d-workflow-controller moves the workflow controller to the daemon, adds `workflow_transition` / `workflow_done` / `start_workflow` / `stop_workflow` / `get_workflow_state` / `list_workflows` to daemon dispatch (and to `DAEMON_METHODS`), and adds TUI→daemon session push (the **durable** direction) so the daemon's workflow-method auth recognizes TUI-minted sessions.

  Adding daemon→TUI session push in sub-2c would be **throwaway** — that push direction goes away after 10d (push reverses to TUI→daemon). Don't do throwaway work; defer to 10d which handles the durable shape.

  The named Phase 1 acceptance criterion "MCP agent inside a daemon-spawned session can call workflow_transition, workflow_done" is satisfied at Phase 1 completion (via 10d-workflow-controller), not at each intermediate slice. Sub-2c's contribution to that criterion is the routing primitive that 10d builds on.

  Subtask methods (`create_subtask`, `list_subtasks`, `mark_subtask_done`) are NOT named in Phase 1 acceptance criteria; they remain TUI-only and outside `DAEMON_METHODS`. Daemon-minted sessions calling them will get the same auth rejection until they relocate (likely as sub-2d or part of 10d-workflow-controller depending on the planning-API coupling).

  **Round-12 test 1 scoping**: the sub-2c review listed "End-to-end: spawn an MCP agent via daemon, call `workflow_transition` against an active workflow run; assert the event appears in events.jsonl" as test #1. That end-to-end test was NOT implemented in sub-2c because it would fail in the current transitional state (auth rejection). The sub-2c test surface instead pins the routing primitive: `PerMethodRoutingTests::test_tui_only_method_routes_to_tui_socket_under_daemon_spawn` verifies the resolver picks the TUI socket for `workflow_transition` regardless of daemon pinning, and `LoudFailureOnUnreachableTargetTests::test_tui_only_method_fails_loudly_when_tui_socket_missing` pins the no-silent-cross-routing contract. The end-to-end test rightly belongs to 10d-workflow-controller, where `workflow_transition` reaches a daemon handler that auths against the daemon's session map.

### Rejected findings (10d-2c-1)

Standing rejections from the 10d-2c-1 review cycle.

- **Two-file atomicity between state.json and events.jsonl** — 10d-2c-1 review rounds 2 → 3 → 4 → 5.

  The `workflow_transition` / `workflow_done` handlers write two separate files under different locks:
  - `state.json` (under flock on `state.json.lock`) records the authoritative workflow state.
  - `events.jsonl` (under a per-run `Mutex` lock on the writer) carries the event the TUI tail observes to deliver activation prompts and finish state.

  Across review rounds we cycled through the failure modes:
  - **Round-2's original shape (event BEFORE try_modify)**: auth check ran INSIDE try_modify, so a rejected call left an event on disk that the TUI tail would deliver as a forged prompt. **Security regression.** Rejected.
  - **Round-3 shape (event AFTER try_modify, no recovery)**: state.json advanced first; if append_event failed, state had moved but no event landed; workflow stalled.
  - **Round-4 shape (event INSIDE try_modify closure)**: closure returned Err on event-write failure → try_modify aborted → state stayed consistent. BUT: if the event write succeeded and the state.json save THEN failed, we had the inverse hole (event-no-state-save) — TUI would deliver prompts for a role the daemon didn't know was active.
  - **Round-5 shape (event AFTER try_modify, with retry + idempotency)**: added bounded retry on append_event and an `active_role == to` idempotency short-circuit so caller retries recover. Idempotency was load-bearing for recovery, but it had a subtle role-boundary bug: an external caller retry after state advanced would hit Unauthorized because `active_role` had moved past the caller's bound role, so the auth check (`wf_role == active_role`) rejected the retry before the idempotency check could fire. The reviewer surfaced this in round 6.
  - **Round-6 shape (event AFTER try_modify, with retry + ROLLBACK on exhaustion — current)**: state.json mutation is captured as a pre-mutation snapshot INSIDE the closure (under flock, after validation). If `append_event_with_retry` exhausts, the snapshot is written back via a follow-up `run::modify`. External caller-side retry sees the pre-mutation state — auth still matches the caller's bound role; the full RMW re-runs from scratch. Round-5's idempotency short-circuit is REMOVED — rollback replaces it.

  **A truly atomic two-file commit still requires a WAL or a single-file commit log.** That's an entire durability slice, not a fix that fits within 10d-2c-1's scope. Round-6 restores transactional "both commit or neither commit" semantics for the recoverable case (any append_event failure that the rollback path can address); the remaining failure mode is "rollback save also fails", documented below.

  **Round-6 rollback failure mode**: if `run::modify(rollback_snap)` itself returns Err (disk-write impossible — full disk, read-only fs, permissions drift), `state.json` is left in the (uncommitted) post-mutation shape with no matching event. The daemon logs loudly with the run_id, the original event-write error, AND the rollback error. **This is unrecoverable territory** — matches the disk-write-impossible failure class. Manual recovery: remove `~/.cm/workflow-runs/<run_id>/state.json.lock`, edit `state.json` by hand to restore consistency (the daemon's log line carries the pre-mutation snapshot indirectly via the rolled-back `active_role` / `iteration` / `status` values to compare against). The original event-write failure is the primary error returned to the caller; the rollback failure is a secondary log line.

  **Durable fix belongs to a later durability slice.** Likely shape: write the (state-mutation, event) pair as a single record into a workflow commit log (`~/.cm/workflow-runs/<id>/journal`), with `state.json` and `events.jsonl` becoming derived projections that get rebuilt at boot from the journal tail. That's a load-bearing slice across the workflow controller + TUI tail + daemon and far out of scope for 10d-2c-1.

  **If a later reviewer pass surfaces another atomicity-split-brain finding on `workflow_transition` / `workflow_done` outside the durability slice, point them to this entry and move on.** The cap is intentional. (Reviewer surfaced this in rounds 2, 3, 4, 5, 6 of 10d-2c-1 review. Round 6 restores transactional semantics for the recoverable case via rollback; remaining gaps belong to the WAL slice.)

### Rejected findings (10d-2a)

Standing rejections from the 10d-2a review cycle.

- **Workflow events.rs further symlink-traversal hardening** — 10d-2a review round 27. Beyond the {0700 dir / 0600 file (round 25), parent-dir hardening + run-id validation + `O_NOFOLLOW` + `fchmod` (round 26), ancestor-symlink rejection via `verify_no_symlinks_in_path` (round 27)} stack already in place, further symlink/race hardening on this path (e.g. `openat`-per-component, canonicalize-and-compare, `fs::Dir` handle threading, TOCTOU-tight directory mutations) is **deferred to a dedicated security audit slice**.

  Threat model boundary: the realistic vector here is "local group user on a shared machine." On a developer's single-user laptop — the primary use case for this project — that's not the relevant threat. The current stack closes the realistic class for the multi-user scenario without piecemeal further hardening.

  Further hardening should be batched with a **holistic `~/.cm/` audit**: the existing `~/.cm/daemon.sock` `0o600`-in-`0o700` pattern, `~/.cm/memory_kills` perms, `~/.cm/tui-sessions.json` perms, the on-disk manifest writers, etc. Doing the audit as one slice keeps the threat-model conversation in one place; doing it piecemeal as part of unrelated feature slices means every feature slice gets re-litigated against the same threat surface.

  If a later reviewer pass surfaces another symlink/race finding on `daemon/src/workflow/events.rs` (or similar) in a non-security-audit slice, point them to this entry and move on. The cap is intentional.

  Specifically included in the deferred set: `~/.cm` root chmod-ensure when the root is freshly created (vs. pre-existing). In practice the daemon socket bind path already chmods `~/.cm` to `0o700` before any workflow code runs, so the realistic exposure window is zero. Belongs in the holistic `~/.cm` audit.

  Also in the deferred set: hardening the no-HOME `/tmp/.cm` fallback root itself. The fallback is **pre-existing daemon-wide behavior** (`daemon/src/path.rs::dot_cm_dir`, which `runs_dir` and the daemon socket bind path both consume) — NOT introduced by 10d-2a. On a shared host with no HOME, `/tmp/.cm` is in shared-writable territory and another local user could pre-create it as a symlink/owned dir. Realistic Phase 1 deployments (local dev laptops; containers with HOME set) don't hit this. A real fix is a daemon-wide root resolution change — `XDG_RUNTIME_DIR`, or refuse to operate without `HOME`, or a runtime-state dir under `/run/user/<uid>/` — and that's a separate slice from workflow events writer scaffolding.

  (Reviewer surfaced this in rounds 28, 30, 31 of 10d-2a review. Each time deferred via this entry. Future reviewer passes on `/tmp/.cm` hardening — or on any other class enumerated above — get the same deferral; worker should reference this entry by name in the deflection reply and proceed.)

### Slice 10d-memory-cap-relocation — Cgroup-OOM watcher relocation (SHIPPED)

> **Note**: Promoted to its own slice during 10c-e-3b-fix2 review. The plumbing for memory-cap End-frame attribution (`SpawnParams.kills_dir` populated when `memory_cap_bytes` is set, daemon-side cgroup_path round-trip on the `start_session` response) lands in 10c-e-3b-fix2. The *producer* of kill-log records — the cgroup-OOM watcher — relocates here. Sequenced before mcp-surface because the memory_cap_kill named acceptance criterion is foundational.

- Relocate `tui/src/session_watch.rs::spawn_watcher` to the daemon side, consuming the daemon's `DaemonSession.cgroup_path` (received from `start_session.cgroup_path`) and writing JSONL records into `~/.cm/memory_kills/<uid>.jsonl` using the existing `daemon/src/path.rs` perms helpers.
- Daemon-side reaper's `LastExitProbe::snapshot` already scans this directory on End-frame emission (slice 10c-e-2 review-6) — no changes needed there once the watcher writes records.
- TUI's `tui/src/session_watch.rs` keeps writing for local-spawn sessions (the daemon path doesn't exercise that for daemon-attached sessions because there's no local cgroup ownership).
- Named acceptance: a daemon-spawned session that's killed by cgroup OOM surfaces `memory_cap_kill: true` on the End frame and the cap-kill toast renders. The TODO(10d-memory-cap-relocation) markers in `Session::new_attached` and `try_spawn_via_daemon` come out.

**Working-set check:** opt-in off, local watcher writes records (unchanged). Opt-in on with cap, daemon-side watcher writes records; cap-kill attribution works end-to-end.

**Status: shipped.** Producer relocated to `daemon/src/session_watch.rs` (the TUI watcher in `tui/src/session_watch.rs` stays for local-spawn parity). Daemon's `start_session` spawns the watcher BEFORE the lock-held arm_reaper/insert when `verified_cgroup_path` and `memory_cap_bytes` are both `Some` — same conditions under which `SpawnParams.kills_dir` is populated, so the writer's directory and the reaper's read directory can't drift. `spawn_watcher` is fallible (`io::Result<JoinHandle>`) with an injectable `WatcherSpawnFn` for failure testing; production passes `default_watcher_spawn_fn()` (wraps `Builder::new().name().spawn()`), tests pass closures that return `Err` to exercise the resource-exhaustion path. On spawn-failure: drop `pending` (Drop pidfd-SIGKILLs the child + waitpids), return `Internal` — no registry residue. On uid-collision after watcher spawn: drop `pending` + drop `watcher_handle` (watcher self-terminates via cgroup-vanish after the SIGKILL). Watcher's `JoinHandle` is stashed on `DaemonSession.watcher_handle` so a future bounded-join can hook in; current contract is drop = detach, matching the TUI watcher. End-to-end test in `daemon/src/session_watch.rs::producer_end_to_end_kills_and_writes_record` (gated `#[ignore]` for the ~5s runtime; explicitly invoked with `cargo test ... -- --ignored`) drives a real spawned child + synthetic cgroup dir, proves the breach-detect → pidfd-kill → JSONL-write chain. Failure-injection test in `control::methods::tests::watcher_spawn_failure_unwinds_with_no_registry_residue` pins the no-panic / no-registry-residue contract under spawn failure. The TUI's `Session::new_attached` TODO markers were removed; comments updated to reference the daemon-side producer. Hard cap is currently recorded as 0 in the daemon JSONL (forensic-only; consumer reads existence, not values); a future wire bump can add the real value if forensic tools need it.

**Slice 10d watcher-fix #1: cgroup discovery from /proc (security).** Pre-fix the daemon trusted `params.cgroup_path` from the caller and the watcher signalled PIDs based on it. A buggy or malicious caller could pre-populate an existing cgroup with PIDs from unrelated processes (shell, another worker) and have the daemon SIGKILL them on the first memory breach. Post-fix `StartSessionParams.cgroup_path` is removed from the wire (serde silently drops the field on legacy callers — `caller_supplied_cgroup_path_is_silently_dropped` pins this). The daemon discovers the cgroup by reading `/proc/<spawn-pid>/cgroup` after Phase 1 spawn via `crate::path::discover_session_cgroup_path`, polling up to 500ms for the systemd-run scope to materialize. Discovery verifies the basename matches `cm-sess-*.scope`; mismatch → `Internal` + `pending` drop SIGKILLs the child. Local-spawn parity: `tui/src/session.rs::Session::new` uses the same shared helper with `pty.child().id()` post-spawn; the predicted path from `wrap_with_systemd_run` is no longer authoritative. Tests: pure-function `parse_cgroup_v2_line_*` family (happy, non-cm-sess basename, wrong-prefix scope, v1-only content, deleted-suffix stripping, non-absolute path), `discover_session_cgroup_path_rejects_test_process_pid` (live integration), `cap_request_outside_cm_sess_scope_returns_internal` + `caller_supplied_cgroup_path_is_silently_dropped` (start_session integration), and TUI-side `memory_cap_bytes_travels_on_wire_but_caller_cgroup_path_is_dropped` (wire-shape pin).

**Slice 10d watcher-fix #2: record-on-every-breach.** Pre-fix the watcher returned silently from `handle_breach` when there was nothing it could (or should) kill — empty cgroup, all-PIDs-protected, target-already-dead. Then the kernel's `MemoryMax` eventually OOM-killed the agent itself, and the End-frame's `memory_cap_kill` stayed `false`. Post-fix the watcher writes a JSONL record on EVERY breach observation with a `kill_status: "killed_by_us" | "protected" | "already_dead" | "no_pids"` forensic enum. The JSONL record format now includes `kill_status`; old-format records (without it) get the forward-compat default `"killed_by_us"` since pre-fix-#2 producers only wrote records when they actually killed.

**Slice 10d watcher-fix #1.5: refine consumer (correctness).** Initially fix #2 flipped `memory_cap_kill: true` on ANY record past baseline. Reviewer caught that the watcher fires on `memory.events high` (the soft-limit counter) which can be a recoverable spike — a process that touched the high watermark, kernel reclaimed pages, process exited cleanly. A `protected`/`no_pids` record + clean exit then incorrectly flagged cap-kill (phantom toast). Post-#1.5 the consumer joins `kill_status` with the kernel exit signal via `crate::reaper::is_cap_kill(kill_status, exit_signal)`: `killed_by_us` → unconditional `true`; `protected`/`no_pids`/`already_dead` require a signal exit (`WTERMSIG.is_some()`) to flag. `KernelExitStatus` grew a `signal: Option<i32>` field; the reaper passes `status.signal` from `wait_for_child`'s `WTERMSIG`. `LastExitProbe::snapshot` and `crate::reaper::build_last_exit_since` both route through `is_cap_kill`. `probe_kill_log_since` parses each post-baseline record's `kill_status` (defaulting to `killed_by_us` for old-format records); returns `Option<String>` for the most-decisive status via `kill_status_priority` (killed_by_us > already_dead > protected = no_pids). Tests: 9 `is_cap_kill_*` table-row tests (every row of the kill_status × signal matrix); `probe_picks_killed_by_us_when_mixed_with_lower_priority` and `probe_picks_lower_priority_when_no_killed_by_us` for the priority logic; `build_last_exit_protected_record_plus_signal_is_cap_kill` and `build_last_exit_protected_record_plus_clean_exit_is_not_cap_kill` for the integration; LastExitProbe-level tests in `session_watch.rs` get both signal-exit and clean-exit variants.

**Slice 10d watcher-fix #2 (build portability): daemon is Linux-only at crate root.** The daemon uses pidfd, cgroup-v2, /proc discovery, and systemd-run — Linux-specific. Pre-fix the per-module `#![cfg(any(target_os = "linux", test))]` gates and `#[cfg(not(target_os = "linux"))]` stubs in `session_watch.rs` (plus per-function gates in `path.rs`) suggested portability that the daemon doesn't actually have, while breaking the build on non-Linux because unconditional refs (the `methods.rs` watcher-spawn arm, the `path.rs` cgroup helpers) wouldn't resolve. Post-fix `#![cfg(target_os = "linux")]` lands at the top of `daemon/src/lib.rs` and `daemon/src/main.rs`; per-module / per-function gates removed. The crate-root doc comment explains the choice. On non-Linux, `cargo build` of `cm-daemon` produces an empty rlib + no bin; the TUI's existing dependency on `cm-daemon` for shared types transitively gates it Linux-only too, which matches actual capability (the TUI also wants cgroup-v2 / systemd-run for the memory-cap feature).

**Slice 10d watcher-fix #3: startup-window early OOM.** Pre-fix the watcher captured `last_high = read_memory_high_count(&cgroup_path)` AFTER stabilize+followup (2.75s after start). A high-counter increment during that window — model load, large JSON inflate, an ML workload hitting the soft limit immediately — silently lost because `last_high` inherited the post-stabilize value and the early breach was treated as baseline. Post-fix the watcher reads `initial_high` at the TOP of `run_watcher` (before stabilize starts) and seeds `last_high = initial_high` for the main poll loop. First post-stabilize poll catches any window-internal increment. Stabilize/followup phases stay — they suppress ACTING on the noisy startup phase (the protected-PID snapshot needs to settle) but the breach baseline is now anchored at watcher-start. Tagged `#[ignore]` for the ~7s runtime; explicit invocation: `cargo test ... watcher_detects_breach_during_stabilize_window -- --ignored`. Test: synthetic cgroup with `memory.events high=5` at watcher start, externally bump to `high=6` during stabilize, assert kill-log gets a `no_pids` record (empty cgroup) post-stabilize. Pre-fix this assertion failed (no record written) because `last_high` was sampled after the bump.

**Slice 10d watcher-fix #4: operator-kill flag.** `is_cap_kill` returned true on `protected`/`no_pids`/`already_dead` + any signal exit. But a signal exit can be operator-driven A-w via the `kill_session` RPC (pidfd-SIGKILL from slice 10c-b), not just kernel-driven `MemoryMax`. A transient soft-limit breach record (e.g. `protected` from a spike that the kernel reclaimed before SIGKILL was needed) followed by an operator A-w would render as a cap-kill toast on a user-driven kill. Post-fix `LastExitProbe` carries an `operator_kill_requested: AtomicBool` field; the daemon's `kill_session` RPC handler calls `mark_operator_kill_requested()` on the session's `LastExitProbe` (which Arc-survives the session itself via `SharedLastExit`) BEFORE the pidfd-SIGKILL goes out. `is_cap_kill` gains a third parameter joining the flag with `kill_status` and the kernel exit signal. See **fix #5** below for the post-#4 refinement to the table.

**Slice 10d watcher-fix #5a: operator override extends to `killed_by_us` (race).** Pre-#5a `is_cap_kill(Some("killed_by_us"), _, _) → true` unconditionally. But the `killed_by_us` record only proves the watcher *attempted* a SIGKILL — not that the watcher's signal actually delivered the killing blow. Concurrent race: watcher writes record → operator's `kill_session` RPC fires before the watcher's signal lands → operator's pidfd-SIGKILL wins. Pre-#5a this rendered as cap-kill despite the operator being the proximate cause. Post-#5a the operator flag wins regardless of `kill_status`: `if operator_kill_requested { return false; }` is the first arm of `is_cap_kill`. Trade-off (documented on `is_cap_kill`): if the watcher genuinely caused the death AND an operator A-w fired concurrently (rare), the toast won't show cap-kill — accepted as the safer failure mode (operator who pressed A-w knows they did; missing cap-kill toast is less confusing than a phantom cap-kill claim on a user-initiated exit). Final decision table:
  - `killed_by_us` + any exit + `!operator` → `true`
  - `killed_by_us` + any exit + `operator` → **`false`** (operator override; rare race accepted)
  - `protected`/`no_pids`/`already_dead` + signal + `!operator` → `true`
  - `protected`/`no_pids`/`already_dead` + signal + `operator` → `false`
  - Clean exit / no record → `false`

Tests: renamed `is_cap_kill_killed_by_us_supersedes_operator_flag` → `is_cap_kill_operator_overrides_killed_by_us_in_race` with inverted assertions for the operator case; `build_last_exit_killed_by_us_plus_operator_kill_is_still_cap_kill` → `build_last_exit_killed_by_us_plus_operator_kill_is_overridden`; `capped_session_killed_by_us_plus_operator_kill_still_cap_kill` → `capped_session_killed_by_us_plus_operator_kill_attributes_to_operator`; new named-acceptance E2E `session_watch::tests::race_killed_by_us_versus_operator_attributes_to_operator` (Arc'd LastExitProbe, full step sequence: watcher writes record → operator marks flag → kernel SIGKILL → snapshot returns false).

**Slice 10d watcher-fix #5b: main.rs bin gate (build portability, take 2).** Pre-fix-#5b the bin had `#![cfg(target_os = "linux")]` at the crate root — but Cargo still treats `main.rs` as a bin target, and the crate-level cfg removed `main()` on non-Linux, producing `error[E0601]: 'main' function not found`. Wrong fix shape. Post-fix-#5b the bin removes the crate-level cfg and uses dual `main` definitions selected by an outer cfg on each function: the Linux arm runs `cm_daemon::run()`, the non-Linux arm eprintln-exits with an explanation. Cargo's "bin must have a main" is satisfied on every target. Lib stays `#![cfg(target_os = "linux")]` at the crate root (the library IS Linux-only — non-Linux callers see crate-not-found, matching the TUI's implicit Linux-only-ness via alacritty PTY).

**Slice 10d watcher-fix #6: pre-watcher startup race.** Earlier #3 anchored the breach baseline at `run_watcher`-start. But the ~1s gap between cgroup discovery (where `start_session` verifies the path) and the watcher thread actually entering its loop (pidfd_open + arm_reaper + lock-held insert + Builder::spawn) was still uncovered. A breach during that window would seed `last_high` to the post-breach value, silently absorbing it. Post-fix-#6 the caller reads `memory.events high` IMMEDIATELY after cgroup discovery via the new public `crate::session_watch::read_memory_events_high()` and passes the value as `initial_high` to `spawn_watcher` → `run_watcher`. The remaining "child spawned but not yet moved into cgroup" sub-window is kernel-level and inactionable (microseconds). Tests: new `session_watch::tests::watcher_detects_breach_during_pre_watcher_window` (`#[ignore]` for the ~7s runtime) writes `memory.events high=5`, has the test code read the seed externally (mirroring `start_session`'s pre-spawn read), then bumps to `high=6` BEFORE the watcher thread starts. With `initial_high=5` seed passed in, the first post-stabilize poll observes the breach. Pre-fix the watcher would have read `high=6` at top of `run_watcher` and treated it as baseline.

**Slice 10d watcher-fix #7: SIGKILL-specific signal check.** Pre-fix `is_cap_kill` flipped the `protected`/`no_pids`/`already_dead` rows on ANY signal exit (`exit_signal.is_some()`). But the kernel's `MemoryMax` enforcement sends SIGKILL specifically; SIGTERM/SIGINT/SIGHUP/SIGABRT are user-driven (service-manager `systemctl stop`, Ctrl-C in a detached agent, manual `kill -TERM`, panic via abort, …). A transient soft-limit breach record followed by a non-SIGKILL signal would render as cap-kill. Post-fix the comparison is `exit_signal == Some(libc::SIGKILL)` — uses the libc constant rather than a hardcoded 9 for platform-correctness. Operator A-w fires SIGKILL via pidfd_send_signal (slice 10c-b), so the operator-override case still applies via #5a's `if operator_kill_requested { return false; }` early return. Tests: `is_cap_kill_protected_plus_sigkill_exit_is_true` (renamed from `_signal_exit_is_true`, the `Some(15)` SIGTERM assertion removed since SIGTERM no longer flips); new `is_cap_kill_protected_plus_sigkill_is_true` (libc constant pin), `is_cap_kill_protected_plus_sigterm_is_false` (named acceptance), `is_cap_kill_protected_plus_non_sigkill_signal_is_false` (sweep across SIGINT/SIGHUP/SIGQUIT/SIGABRT/SIGPIPE/SIGUSR1/SIGUSR2), `is_cap_kill_protected_plus_sigkill_with_operator_is_false` (operator override still wins), `build_last_exit_protected_plus_sigterm_is_not_cap_kill` (reaper integration); LastExitProbe-level `capped_session_protected_record_plus_sigterm_is_not_cap_kill`, `capped_session_protected_record_plus_sigint_is_not_cap_kill`, `capped_session_protected_record_plus_sigabrt_is_not_cap_kill`, `capped_session_protected_record_plus_sigkill_is_cap_kill`.

### Slice 10d-mcp-surface — Daemon-side MCP tool surface (SHIPPED across sub-1 / sub-2a / sub-2b-{1,2,3} / sub-2c + ~15 review rounds)

> **Note**: Originally folded into 10c-e. Separated during 10c-e-3 review when it became clear the surface is large (Session-caller descendant-task-tree validation, `propose_task`, workflow tools, subtask tools, kill/list/start_session-for-agents, …) and the smoke can validate PTY mechanics without it.

- Daemon-side dispatch for `Session`-caller MCP requests (the agent-orchestration tools): `propose_task`, `workflow_transition`, `workflow_done`, `create_subtask`, `list_subtasks`, `mark_subtask_done`, `list_sessions`, `start_session` (Session-caller), `send_input`, `read_session_output`, `kill_session`, `wait_for_session_idle`, `wait_for_workflow_done` / `wait_for_workflow_stop`.
- Descendant-task-tree authorization: a Session caller can only act on sessions in its own task tree (the TUI's existing rule).
- This unblocks A-n daemon-spawned Claude actually invoking MCP tools (the "MCP from inside the session" bullet that was dropped from the 10c-e-3c smoke).

**Working-set check:** opt-in off, MCP routes to `tui.sock` as today. Opt-in on, MCP routes to `daemon.sock` and Session-caller validation kicks in.

**Slice 10d-mcp-surface-1 — Scaffolding only (shipped):** sub-1 was originally scoped to flip Session-caller dispatch for the four already-implemented arms + add `list_sessions`. Review caught **three findings** that made the dispatch flip unsafe to ship in sub-1:

  - **Finding #1 (auth widening):** the same-workspace rule daemon-side widened access for task-bound callers vs the TUI's `caller_authorized_for` rule (`tui/src/control/methods.rs:166`), which restricts task-bound callers to their task subtree. The Phase 1 acceptance criterion says "CM_TUI_SESSION_ID scoping behaves identically to today (descendant-only)" — same-workspace violates that for task-bound callers.
  - **Finding #2 (`list_sessions` wire mismatch):** the response shape `{sessions: [{uid, workspace_id, title}]}` doesn't match what the Python MCP tool's caller code reads at `mcp_server/server.py:660` (iterates as a top-level list, reads `label`, `idle`, `managed_by_uid`).
  - **Finding #3 (`start_session` wire mismatch):** Python MCP tool sends `{type, label, prompt?, task_id?}` (`mcp_server/server.py:359`) but daemon requires `uid`, `workspace_id`, `argv`, `working_dir`. Session callers would get `InvalidParams` even when auth passes.

**Sub-1 ships as scaffolding only — Session-caller dispatch arms reverted to Operator-only with `TODO(slice 10d-mcp-surface-2)` markers in `daemon/src/control/dispatch.rs` (`dispatch_send_input`, `dispatch_kill_session`, `dispatch_read_session_output`, `dispatch_start_session`, `dispatch_list_sessions`).** What sub-1 retains:

  - `daemon/src/control/auth.rs` (new module): `check_session_caller(state, caller_uid, target_uid) -> AuthDecision` with table-row enum variants. The Phase 1 same-workspace rule is wrong for task-bound callers, but the module + `AuthDecision` shape is the right scaffold for sub-2's task-subtree implementation.
  - `DaemonSession` / `SpawnParams` / `PendingSessionInner` gain `workspace_id`, `session_type`, `managed_by_uid`, `task_id` — threaded through the two-phase spawn so sub-2 has everything it needs for the task-subtree auth.
  - `StartSessionParams` gains `session_type` (defaults to `"claude-code"`), `managed_by_uid`, `task_id` — daemon-spawned sessions carry these to `list_sessions`.
  - `list_sessions` method body returns the Python MCP tool's wire shape: top-level JSON array of `{session_uid, label, type, state, idle, managed_by_uid}`. `include_exited` + `task_id` params accepted (no-ops until sub-2 / slice 10e).
  - 7 `auth::tests` (table-row unit tests) survive — they test the module in isolation.
  - 3 `list_sessions_*` tests: Operator returns Python-MCP-tool wire shape (positive); accepts `include_exited` + `task_id` params as no-ops (positive); Session caller still Unauthorized pending sub-2 (negative).
  - Dispatch tests for the five reverted arms renamed `*_session_caller_still_unauthorized_pending_sub_2` with assertions on the `sub-2` pointer in the error message.

**Sub-2 sub-slicing plan.** Sub-2 is too large for a single commit (per the sub-1 cadence lesson — 10c was the last slice that tried to bundle too much). Split into sub-sub-slices, each its own commit on clean review:

**Sub-2a — Shipped (unstaged at handoff time).** Task-tree plumbing + auth relaxation + Session-caller dispatch flip for the four read/mutate methods. `start_session` deferred to sub-2b alongside `propose_task` (the wire-shape question is paired with `mcp_start_session` design).

  - `DaemonState.task_tree: HashMap<task_id, Option<parent_task_id>>` added — TUI-pushed snapshot, replace-not-merge semantics.
  - `task.update_tree` method + Operator-only dispatch arm. Session callers can't rewrite the tree (would escape their own auth scope).
  - `auth.rs::task_is_self_or_descendant_of` helper mirrors `tui/src/control/methods.rs::task_is_self_or_descendant_of` exactly (same `MAX_TASK_DEPTH=64` cap, cycle detection, missing-task graceful default).
  - `auth.rs::check_session_caller` rule extended to mirror TUI's `caller_authorized_for`:
    - self → Allow.
    - tasked caller + target's task is self-or-descendant → Allow (workspace-agnostic, mirrors TUI's branch-mode subtask shape).
    - tasked caller + taskless target → OutOfScope.
    - tasked caller + sibling task (no descendant relationship) → OutOfScope (no workspace fall-back — round-1 widening lesson).
    - taskless caller + same workspace → Allow.
    - taskless caller + different workspace → OutOfScope.
  - Dispatch flip for `send_input`, `kill_session`, `read_session_output`, `list_sessions` via shared `authorize_session_caller_for_session_param` helper. Operator callers bypass; Session callers go through `check_session_caller`.
  - `list_sessions`:
    - Session-caller arm: caller_uid passed into the method body, which filters to entries the caller is authorized for via `check_session_caller`. Operator callers pass `None` and see all.
    - `task_id` filter now honored: walks `state.task_tree` via `task_is_self_or_descendant_of`. Taskless sessions excluded when `task_id` is set.
    - `include_exited` still a no-op (tombstones land in slice 10e).
  - Tests: 22 `auth::tests` (15 new — taskless/tasked combinations, walk edge cases, cycle/depth defenses), 4 `task.update_tree` (Operator-pushes replace, snapshot-not-merge, Session-caller-rejected, e2e descendant-across-workspaces), repurposed dispatch tests for `send_input` / `kill_session` / `read_session_output` / `list_sessions` (self / same-workspace / cross-workspace flow), `list_sessions_honors_task_id_filter`, `list_sessions_session_caller_taskless_scopes_to_own_workspace`, `list_sessions_session_caller_not_in_registry_is_unauthorized`. The `*_session_caller_still_unauthorized_pending_sub_2` tests for the four flipped arms are gone; `start_session_session_caller_still_unauthorized_pending_sub_2` stays (sub-2b owns its re-enable).

  - **sub-2a — Task subtree + auth relaxation + dispatch flip for the already-implemented methods.**
    - `DaemonSession.task_id` (already threaded in sub-1's scaffolding).
    - `DaemonState` gets a task-subtree view. **Source-of-truth question** to resolve before coding: is the daemon authoritative for the task tree (loads from + writes to the planning API), or does the TUI continue owning it and the daemon mirrors a snapshot? The current state is "TUI owns it"; Phase 1 doc allows either. Cheapest first cut: TUI pushes `task.update_tree(tasks: [{task_id, parent_task_id}, …])` whenever it mutates `App.tasks`, daemon stores it on `DaemonState.tasks: HashMap<task_uid, parent_task_uid>` and uses it for the descendant walk. Reuse the `MAX_TASK_DEPTH=64` cap from `tui/src/control/methods.rs::task_is_self_or_descendant_of`.
    - Extend `crate::control::auth::check_session_caller` with a `Allow if target.task_id is self-or-descendant of caller.task_id` branch. Keep the self-only base. The `same_workspace_sibling_is_out_of_scope_pending_task_plumbing` test gets repurposed: same-workspace + same-task-subtree → Allow; same-workspace + different-subtrees → OutOfScope.
    - Flip the five reverted dispatch arms (`send_input`, `kill_session`, `read_session_output`, `start_session`, `list_sessions`) back to the auth-checked path. Replace the `TODO(slice 10d-mcp-surface-2)` markers with the wired auth helper. `start_session` Session-caller derives missing fields from the caller's session: `workspace_id` from `caller.workspace_id`, `working_dir` from the workspace's `worktree_path`, `argv` from `mcp_config::build_args(SpawnTarget::Daemon, engine, new_uid, …)`. Wire surface stays the strict shape; a sibling `mcp_start_session` method takes the Python tool's `{type, label, prompt?, task_id?}` minimal shape and constructs the full SpawnParams internally.
    - `list_sessions` task_id filter (sub-1 no-op) → honor the filter via the same task-subtree walk.
    - Estimated 3-5 commits with the same per-finding reviewer rhythm as 10c-e / 10d-memory-cap.

  - **sub-2b-1 — Shipped.** `resolve_authorized_session` daemon dispatch + `transcript_path` plumbing + `idle` + `generation` tracking. Named criterion "MCP agent inside a daemon-spawned session can call read_session_output" now ships end-to-end through the Python tool's `resolve_authorized_session` → file-read compose pattern. Key surfaces:
    - `DaemonSession.transcript_path: Option<String>` + `DaemonSession.generation: u64` + `DaemonSession.last_activity_at: Arc<Mutex<Option<Instant>>>` (initialized to `Some(spawn_time)` so fresh sessions report `idle: false` for `IDLE_THRESHOLD` post-spawn).
    - `session.set_transcript_path` RPC: Operator-only push from TUI, increments `generation` iff path differs (idempotent re-pushes don't bump the agent's cursor).
    - `compute_session_state_and_idle` shared helper: single source of truth for the `(state, idle)` pair both `resolve_authorized_session` and `list_sessions` return — no drift.
    - `DaemonSession::send_input_and_stamp` (via `InputHandle`): every PTY write path stamps activity. `methods::send_input` AND `stream::handle_input_frame` both route through it.
    - TUI push hookups at every `transcript_id` mutation site: discovery loop, history rotation, seeded workspace, seeded session, workflow controller launch.
    - **Encapsulation deferred**: `transcript_id` stays a public field on `TerminalSession` for now. The audit + per-site push wiring is complete; making the field private with a `set_transcript_id` method that also pushes would need either `&Workspace` plumbing into the setter OR a "needs_push" return value. Real refactor; queued as sub-2b-2 housekeeping if we want it.

  - **sub-2b-2 — `propose_task` daemon dispatch.**
    - Shape A (recommended): daemon directly POSTs to the planning API. Adds an HTTP client to the daemon (`reqwest` with `rustls-tls` to avoid the openssl native-dep mess); daemon reads `CM_API_TOKEN` + API URL from env/config; mirrors what `tui/src/control/methods.rs::propose_task` does today via TUI's planning_client. `propose_task` is then daemon-authoritative, working even when the TUI is dead — matches Phase 1's "daemon survives TUI restarts" goal.
    - Shape B (alternative): daemon forwards to TUI via a `proxy.propose_task` method on `tui.sock`. Faster to land but breaks the daemon-survives-TUI property. Not the long-term answer.
    - Auth: any Session caller may propose (matches TUI's current rule — anyone can add a draft task to the backlog). Operator callers also allowed.
    - Files: `daemon/src/planning_client.rs` (new, small HTTP wrapper); `methods::propose_task`; dispatch arm; tests against a stub HTTP server (`wiremock` or hand-rolled `tokio` listener) for path/payload/auth-header pinning; e2e through `daemon/tests/accept_loop.rs`.
    - Per-tool sub-slice; small if the daemon already has `tokio` (it does, for the cgroup watcher).

  - **sub-2b-3 — `mcp_start_session` minimal-params + Session-caller flip.** Last piece of sub-2b. Once shipped, the named criterion "MCP agent inside a daemon-spawned session can call start_session" ships and sub-2b closes.

    **Problem shape.** The Python MCP `start_session` tool (`mcp_server/server.py:361`) sends `{type, label, prompt?, task_id?}` — much smaller than the daemon's current `start_session` wire shape `{uid, workspace_id, label, session_type, argv, working_dir, env, cols, rows, ...}` which TUI's `ClientSession::new` (`tui/src/client_session.rs::rpc_start_session_full`) builds. Today a Session-caller hitting daemon's `start_session` with the minimal shape gets `InvalidParams` (missing required fields). Sub-1's dispatch arm also explicitly rejects Session callers with `Unauthorized` (`start_session_session_caller_still_unauthorized_pending_sub_2` test). Both blocks need lifting.

    **Decision: separate method, not discriminated union.** New daemon method `mcp_start_session` accepts the minimal shape; the existing `start_session` keeps the full shape for TUI use. Reasons:
    - The two shapes have different security postures — the TUI is trusted to supply `argv` verbatim (it builds via `mcp_config::build_args`); a Session caller cannot be trusted to send arbitrary argv. A separate method makes the security boundary obvious in the wire surface (and in code review).
    - A discriminated union would force every call site through the same params struct, which couples the two evolution paths (a future field added for one would affect the other's schema).
    - Method-name distinction lets Python's `control_client.call("mcp_start_session", …)` route cleanly without payload-shape sniffing.

    **Resolution rules** (`mcp_start_session` derives the full SpawnParams from minimal params + caller context):
    - `uid`: daemon generates fresh via `new_session_uid()` (same format the TUI uses: `ts-<nanos>-<counter>`). Pre-spawn validation ensures no collision with `state.sessions`.
    - `workspace_id`: copy from caller's `DaemonSession.workspace_id`.
    - `working_dir`: look up the workspace in `DaemonState.workspaces`; use its `worktree_path`. Error `NotFound` if the workspace isn't registered or has no worktree.
    - `task_id`: when supplied by the caller, must be self-or-descendant of caller's task per `task_is_self_or_descendant_of` (sub-2a's auth walk). When omitted, default to caller's own `task_id`. A taskless caller supplying a `task_id` → `Unauthorized` (mirrors the TUI's `start_session` rule).
    - `argv`: derive from `type`. Daemon needs a minimal type→argv mapping (the inverse of what slice 10c-e-3b removed). Scoped to MCP-only callers, NOT the TUI's path — TUI continues to build argv via `mcp_config::build_args` and send the full wire. Document the exception in the method's doc comment so future readers understand why daemon has the mapping back. Mapping:
        - `"claude-code"` → `claude --dangerously-skip-permissions --mcp-config <per-session-path>`.
        - `"codex"` → `codex --dangerously-bypass-approvals-and-sandbox -c <per-session-overrides>`.
        - `"bash"` → `/bin/bash` (no MCP injection — matches the TUI's `bash` path).
    - `env`: daemon injects `CM_TUI_SESSION_ID=<new_uid>`, `CM_DAEMON_SOCKET=<abs-path>`, `CM_TUI_SOCKET=""` (authoritative empty — same pattern as TUI's `build_env(SpawnTarget::Daemon, …)`). Plus any vars the caller supplied (additive, daemon-injected pins win).
    - `cols`/`rows`: defaults to 80×24. A future enhancement could plumb the caller's terminal size, but Python MCP tool doesn't know it today.
    - `memory_cap_bytes` / `cgroup_path`: inherit from caller's session. Matches user-mental-model: a subtask agent should run under the same memory cap as the parent. **Caveat**: the daemon doesn't currently expose memory_cap_bytes on `DaemonSession` (sub-2b-1 carries `last_activity_at`/`transcript_path`/`generation` but not the cap). Need to add: `DaemonSession.memory_cap_bytes: Option<u64>` + `DaemonSession.cgroup_path: Option<String>` (already exists I think — verify at start). Inherit at `mcp_start_session` time.
    - `transcript_path`: `None` at spawn (no clone seed for MCP-spawned subtasks). TUI's existing detector + `session.set_transcript_path` push covers the binding post-spawn.

    **MCP-config-file shape (where the work lands).** Claude needs `--mcp-config <path-to-json>` pointing at a per-session JSON file that lists the MCP servers + their env. Codex takes inline `-c` overrides. Both are built TUI-side today in `tui/src/mcp_config.rs`. The daemon needs equivalent functionality. Two options:
    1. **Relocate `mcp_config.rs` to a shared crate** (or to daemon, with TUI re-importing). Heavy refactor; touches many TUI call sites.
    2. **Daemon-local minimal version**: a small subset of `mcp_config.rs` covering JUST what `mcp_start_session` needs (write claude JSON, build codex overrides). Code-duplication cost is real but bounded; can be unified in a later cleanup slice.

    **Lean option 2** for sub-2b-3. The TUI's `mcp_config.rs` keeps growing with workflow-aware logic, plan-mode behavior, etc.; relocating it sweeps in unrelated concerns. A daemon-local helper at `daemon/src/mcp_config.rs` (or inline in `methods::mcp_start_session`) keeps the diff scoped. Future cleanup slice (probably during 10d-workflow-controller relocation) merges the two.

    **Dispatcher arm**: new `mcp_start_session` arm. Both Operator and Session callers allowed; Session-caller path runs `check_session_caller`-style auth on `task_id` if supplied. The existing `start_session` arm keeps its current Session-caller-Unauthorized behavior (TUI is the only legitimate `start_session` caller; agents go through `mcp_start_session`).

    **Tests**:
    - Spawn via `mcp_start_session` with minimal params; observe daemon resolves uid/workspace_id/argv/working_dir from caller context; child runs correctly.
    - Auth: Session caller, no `task_id` supplied → inherits caller's task. Explicit `task_id` in same subtree → Allow. Different-subtree task_id → Unauthorized. Taskless caller + explicit task_id → Unauthorized.
    - Wire shape: each engine type produces the expected argv (claude → `--mcp-config <path>`, codex → `-c <overrides>`, bash → `/bin/bash`).
    - Existing `start_session_session_caller_still_unauthorized_pending_sub_2` test (the sub-1 holdover) STAYS as-is — Session callers still can't use the FULL-shape `start_session`; only `mcp_start_session`.
    - Python side: `mcp_server/server.py:361 start_session` switches its `control_client.call` from `"start_session"` to `"mcp_start_session"` when `CM_DAEMON_SOCKET` is set; falls back to `"start_session"` against the TUI socket otherwise (TUI continues to handle the minimal shape via its existing `tui/src/control/methods.rs::start_session`).

    **Estimated work**: per-tool sub-slice; medium-to-large. New daemon-local MCP config helper, new method, new dispatch arm, new tests, Python-side routing flip, plus the `DaemonSession.memory_cap_bytes` field addition if it isn't already there.

  - **sub-2c — Workflow MCP methods (`workflow_transition`, `workflow_done`).** These touch the workflow controller. Phase 1 keeps the controller TUI-side (`tui/src/workflow/controller.rs`) and the daemon writes to `events.jsonl` which the TUI tails (per the design doc's intentional staging — Phase 2 cuts the file dependency). Order question: sub-2c can ship before `10d-workflow-controller` (the controller stays TUI-side; daemon just writes events.jsonl which is shared via fs). Or interleave with `10d-workflow-controller` if that lands first. NOTES.md should record the order chosen when sub-2c starts.

  - **sub-2d — Subtask methods (`create_subtask`, `list_subtasks`, `mark_subtask_done`) + blocking-wait methods (`wait_for_session_idle`, `wait_for_workflow_done`, `wait_for_workflow_stop`).** Subtask methods interact with the planning API + worktree spawn. Blocking waits are polling helpers; consider implementing them at the MCP layer (Python `control_client`) as poll loops around `list_sessions` + workflow event stream rather than daemon-side, depending on how chatty they get.

**Out of scope for all of sub-2:** the workflow controller relocation itself (`tui/src/workflow/controller.rs` → daemon) is `10d-workflow-controller`. Manifest ownership flip is 10e. Default socket flip is 10f.

### Slice 10d-workflow-controller — Workflow controller relocation (SHIPPED across 10d-1 / 10d-2 / 10d-3 + ~15 review rounds for 10d-2c-1 alone)

#### 10d-1 — TUI → daemon session-snapshot push (scaffolding) — SHIPPED

Wire shape: `tui.update_sessions_snapshot` (Operator-only) carrying a full-replace snapshot of `{uid, task_id, label, type, hidden, workflow_run_id, workflow_role}` per TUI session. Lands in `DaemonState.tui_sessions: HashMap<String, TuiSessionSnapshot>`, with `tui_sessions_pushed: bool` distinguishing "deliberately empty" from "never pushed". Helper `lookup_session_any` returns `SessionViewAny { uid, daemon_owned, task_id, workspace_id, workflow_run_id, workflow_role }` checking `state.sessions` first, then `state.tui_sessions`. No callers yet — 10d-2 wires the workflow-method auth consumer.

TUI side: push site is `App::save_session_manifest` (the documented funnel; every session-list mutation already flows through it before returning Ok, per the convention at the top of the helper section). Universal coverage without site-by-site audit — at the cost of one extra local UDS round-trip per save (opt-in gated; sub-ms vs. the disk write the funnel already does). Sites that mutate the task tree call the existing `push_state_to_daemon` wrapper (task tree + snapshot — the snapshot half is redundant with the funnel hook but correct and idempotent).

Strict-fatal: the RPC helper surfaces socket failures as `Err`; the TUI's `push_tui_sessions_to_daemon` then `eprintln!`s a clear message. Round-11 invariant: under opt-in the daemon is a hard dependency, so failure must not be silently swallowed.

Tests (4 new, all green 5x):
- `rpc_tui_update_sessions_snapshot_full_replace` — Operator push lands; second push replaces (not merges).
- `rpc_tui_update_sessions_snapshot_empty_push_sets_pushed_flag` — pushed flag flips even on empty payload.
- `rpc_tui_update_sessions_snapshot_rejects_session_caller` — `Caller::Session` → `Unauthorized`, state untouched.
- `rpc_tui_update_sessions_snapshot_surfaces_socket_failure` — bad socket → `Err` propagates to caller.

#### 10d-2 — Workflow controller relocation, auth consumer (proposal)

**What moves to daemon:** the workflow controller state machine (`tui/src/workflow/controller.rs`, ~2050 lines): role table (engine, context policy, activation prompt), transition graph (static `on_idle` + dynamic from `workflow_transition`/`workflow_done`), template expansion (`{{ roles.<role>.assistant[N] }}` etc.), workflow-run id allocation, and the `events.jsonl` writer. Becomes `daemon/src/workflow/controller.rs` invoked by `start_workflow`/`stop_workflow`/`get_workflow_state` dispatch arms. The auth consumer for `workflow_transition`/`workflow_done` lives here: a `Caller::Session(uid)` call walks `state.lookup_session_any(uid)` → matches `workflow_run_id` against the controller's active run → authorizes.

**What stays in TUI:** the **observer** half. `App` keeps its `workflow_runs: Vec<WorkflowRun>` mirror but populates it exclusively via `events.jsonl` tail (`~/.cm/workflow-runs/<id>/events.jsonl`) instead of in-memory controller state. Keybindings (`A-f` / `A-u` / `A-o` / `A-y`) become RPC calls. The TOML loader (`workflows/feedback.toml` + custom defs) becomes daemon-side at startup — a `daemon.config_dir` env or path passed via launch. TUI calls `list_workflows` to surface options in the launcher UI.

**State machine location:** daemon. The TUI must not mutate workflow state — it observes via events.jsonl + manifest watch.

**TOML loader access:** daemon loads at startup from `${CM_WORKFLOWS_DIR:-$HOME/.cm/workflows}` plus a built-in fallback bundling `feedback.toml`. `list_workflows` exposes the loaded defs; `start_workflow` references them by name. TUI does NOT parse TOML in opt-in mode.

#### 10d-3 — DAEMON_METHODS expansion (proposal)

**What moves:** `workflow_transition` and `workflow_done` flip from file-writer (TUI tails `events.jsonl`) to **socket-routed methods** that take a `workflow_run_id` + state and write the event server-side under controller-held locks. The file becomes a daemon-owned append log, not a producer/consumer rendezvous. DAEMON_METHODS in `mcp_server/server.py` gets these two added; control_client routes them to the daemon socket. Auth consumer is the controller's session→run→role table from 10d-2 — Session-caller for a participant in the active run is authorized; everyone else is `Unauthorized`.

**Why not in 10d-2:** 10d-2 establishes the controller-side machinery; 10d-3 flips the wire shape MCP agents see. Doing them in one slice would conflate "controller works" with "file→socket migration", both with their own failure modes. Sequencing them lets the controller be exercised end-to-end (TUI-driven start, agents writing to events.jsonl as today) before changing the agent-facing surface.

**Working-set check (after all of 10d-1/2/3):** opt-in off, workflows work locally. Opt-in on, workflows run daemon-side; the TUI is a thin observer.



### Slice 10e — Manifest ownership flip (SHIPPED across 10e-a / 10e-b / 10e-c / 10e-d)

- Daemon becomes the only writer of `~/.cm/tui-sessions.json`. TUI's `save_session_manifest` becomes a no-op (or a debug-assert) when opt-in is on.
- TUI populates its in-memory mirror exclusively via `manifest.watch`. The slice-9 `ManifestWatcher` broadcaster wires to actual diffs.
- `last_exit` flows end-to-end: daemon reaper detects cap kill → updates manifest → broadcasts `ManifestDiff::Exited` → TUI mirror reflects → toast renders. Named acceptance criterion green for the detached path.
- Subsumes the 10c-e-3 `worktree_path` auto-register fallback in `start_session` — the daemon learns about new workspaces via `manifest.watch` instead.

**Working-set check:** opt-in off, TUI owns manifest as today. Opt-in on, daemon owns it; TUI follows.

### Slice 14 / 10g — Reconnect / ring-buffer replay integration test (SHIPPED as the final-slice commit)

> **Note on test flake-class** (carried forward from 10c-e-3b-fix2 review). The daemon's real-PTY-spawn tests
> (`cm_tui_session_id_env_is_injected_for_child`, `exited_session_is_removed_from_registry_within_bound`,
> `registry_remove_races_safely_against_insert`, `start_session_default_cols_rows_used_when_not_provided`,
> `start_session_with_explicit_cols_rows_sizes_pty_accordingly`,
> `end_frame_for_signal_kill_with_baseline_relative_record_carries_cap_kill_true`) and the TUI's
> `client_session_new_with_explicit_size_spawns_pty_at_that_size` all flake under workspace concurrency
> when build artifacts are cold. Per-package sequential runs (daemon-only, TUI-only) pass cleanly; the
> failures only appear under `cargo test --workspace` + concurrent rustc work. The reconnect test will
> exercise more of the same shape (spawn real bash, kill TUI, reattach, observe replay) — so:
>
> - Use generous bounded deadlines for the spawn / kill / probe windows. The slice-10c-e tests use 2-3s;
>   the reconnect test should use 5s+ for the same operations.
> - Run with `--test-threads=1` in CI specifically for the reconnect-test target, or quarantine it to
>   a non-`--workspace` invocation. The serial run avoids the PTY-resource-contention flake.
> - The reconnect test's "TUI restart picks up the existing daemon session" assertion is the load-bearing
>   one. Bound it on `state.sessions.contains_key(&uid)` polling rather than wall-clock sleeps.

### Slice 10f — Default flip + cleanup (SHIPPED)

- Daemon mode is now mandatory. `CM_USE_DAEMON_SOCKET=1` is a silent no-op.
- 9 Rust opt-in gate sites removed (`tui/src/main.rs`, `daemon_launch.rs` ×2, `manifest_watch.rs` ×1, `app.rs` ×7). `opt_in_enabled()` function deleted.
- Python `resolve_socket_route()` opt-in branch removed; routing now keyed entirely off explicit `CM_DAEMON_SOCKET` / `CM_TUI_SOCKET` env vars set by `build_env`.
- TUI `main.rs` startup error message tightened: lists `cargo build -p cm-daemon`, `CM_DAEMON_BINARY` override, and `~/.cm/` permission checks as fixes.
- 7 Rust tests + 4 Python tests deleted (premise contradictory under daemon-mandatory); 5 tests renamed/repurposed; 2 net-new (`daemon_auto_launch_unconditional_post_flip`, `should_spawn_is_unconditional_post_flip`).
- `tui/src/control/server.rs` + `queue.rs` removal deferred to a follow-up (still hosts the workflow-side socket; the per-method routing convention from sub-2c means both sockets stay active for now).

**Working-set check:** daemon is always-on, hard-required. The historical legacy single-process path is gone.

## Phase 1 ship log — actual vs. estimated

Per-slice estimates pre-flight vs. what landed (commits beyond
the named slice are review-fix rounds rolled into the same slice's
arc):

| Slice | Estimate | Actual |
|---|---|---|
| 10a | 2–3 | 2 (10a-shell + 10a-types) |
| 10b | 3–4 | 1 (mechanically large but landed cleanly) |
| 10c | 5–8 | 13 (subdivisions + 7 review-fix rounds for 10c-e-2 alone) |
| 10d-memory-cap | 2–3 | 1 |
| 10d-mcp-surface | 3–5 | 10 (sub-1 / sub-2a / sub-2b-{1,2,3} / sub-2c + ~15 review rounds) |
| 10d-workflow-controller | 1–2 | 7 (10d-2c-1 ran 15 rounds across rollback / iteration / atomicity) |
| 10e | 1–2 | 4 (10e-a / 10e-b / 10e-c / 10e-d) |
| 10f | 2 | 1 |
| 10g (reconnect) | — (named acceptance, no estimate) | 1 |
| **Total Phase-1 slice commits** | **~15–20** | **40** |
| Plus design doc + sub-plan + housekeeping | — | (balance to 28 commits on this branch — the design doc, sub-plans, async wait_for_*, lowercase compare, etc. predate slice 10a) |

The actual count is 2× the estimate. The compounding factor was
review-fix rounds, mostly concentrated in 10c-e-2 (7 rounds) and
10d-2c-1 (15 rounds). The 10d-2c-1 multi-round arc surfaced most
of the meta-lessons captured below — that pain was the cost of
learning the rollback-vs-retry-vs-event-ordering invariants the
hard way.

## Phase 1 follow-ups (deferred)

Cross-slice follow-ups surfaced during Phase 1, consolidated here
for the next planning pass. Distinct from **design-doc Phase 2**
(see "Phase 2: Workflow events over RPC" below) — these are
Phase-1-introduced gaps, not the next-phase roadmap. Most have
inline references back to the slice where they surfaced; the
in-line "Future work" / "Known costs" sections below carry the
original detailed context.

1. **10e-d M4 — attach-drain wiring lacks unit-test coverage**.
   `drain_terminal_events`'s `try_emit_cap_kill_toast` call is
   inspection-verified only; a PTY integration test would close
   the coverage gap. Single-line wiring; risk is low but the
   contract is load-bearing.

2. **10f Q1 — mid-session daemon-crash recovery policy**. Today:
   `manifest.watch` reconnect loop (1s → 30s exp backoff) handles
   transient crashes; RPC errors surface via existing status
   paths. Open question: TUI auto-relaunch of the daemon on N
   consecutive ECONNREFUSED. Conservative default in Phase 1.

3. **`tui/src/control/server.rs` + `queue.rs` removal**. Workflow-
   side socket lives here; per-method routing (sub-2c) keeps both
   sockets active. Track as cleanup once the workflow controller
   itself relocates daemon-side (Phase 2's natural follow-up to
   10d-workflow-controller).

4. **Socket-close cancellation for `mcp_start_session` slot wait**
   (sub-2b-3 review-9). Bounded 20s wait is sufficient for
   acceptance; the cleaner shape (abort wait on client socket
   close) needs async runtime or a peer-poll thread.

5. **TUI crash-cleanup bound for `tui_sessions`** (10d-1 round-3).
   Daemon retains the last-pushed TUI sessions snapshot until the
   restarted TUI's first push replaces it. Stale-snapshot window
   on TUI crash. Durable fix: daemon-side connection-lifecycle
   awareness for the TUI's RPC connection.

6. **`DAEMON_METHODS` set split** (10d-1 round-4). Today the
   frozenset serves both Python routing AND dispatch-alignment
   testing. Cleaner shape: `SESSION_CALLABLE_METHODS` (Python
   routes) ∪ `OPERATOR_ONLY_METHODS` (TUI-pushed). Inline comment
   on the entry flags intent.

7. **Workflow `events.jsonl` torn-record durability above
   `PIPE_BUF`** (10d-2a round-6). Phase 1 payloads <1KB; realistic
   exposure zero. A durable fix (truncate-to-last-good-offset on
   daemon restart, or fsync-per-record with LSN) is a dedicated
   slice.

8. **PTY-test isolation slice** — consolidates known flake-class
   issues:
   - `read_session_output_with_cursor_returns_only_new_bytes`
   - `workflow_transition_rollback_preserves_concurrent_tui_role_sessions_update`
   - `client_session_write_200kib_arrives_at_daemon_pty_without_drops`
   All pass on `--test-threads=1` / isolated; fail on workspace
   parallelism. Same root cause class.

9. **Symlink-traversal hardening** (10d-2a round-27). Beyond the
   current stack (`O_NOFOLLOW`, parent-dir hardening, ancestor-
   symlink rejection), further hardening (`openat`-per-component,
   canonicalize-and-compare, `fs::Dir` handle threading) is
   deferred to a dedicated security audit slice.

10. **Per-worktree spawn+detect serialization replacement**
    (sub-2b-3 review-4). Today's per-worktree mutex is correct
    but coarse. A content-association approach (spawn-tag env var
    echoed into the first transcript line, match-by-tag) removes
    serialization where latency matters.

## Phase 1 meta-lessons

Recurring patterns that surfaced through the slice arc. Future
slices applying these should converge faster than this one did.

1. **Surface decisions pre-coding.** Every slice that opened with
   a 6-item audit (handler-read + ownership + wire shape +
   atomicity + race surfaces + invariants) caught problems at
   plan-time rather than r1/r2 review. Slices that skipped the
   audit hit avoidable review rounds. The discipline is cheap;
   skipping it isn't.

2. **Per-test audit log on cleanup slices.** 10f's "kept-
   converted, deleted, net-new" table for the opt-in test sweep
   documented each touched test's fate. Prevents "test currently
   disabled" rot AND surfaces over-aggressive deletions before
   they ship.

3. **Mutation-verify discipline.** Every gate that mattered got
   "remove the gate, observe the test fail with the expected
   message, revert." Surfaced real coverage gaps (10e-d M4 not
   testable by unit suite, 10g r1 first-frame contract, 10g r2
   single-emit-seed contract) AND validated that passing tests
   pass for the right reason — not just because the structure
   happens to align.

4. **Honest reporting of inspection-only verification.** When a
   test can't reach a code path (10e-d M4 — `drain_terminal_events`
   needs a real PTY the unit suite doesn't have), document the
   gap explicitly rather than claim coverage. Tracked as a Phase 2
   item, not handwaved.

5. **First-principles test contract pinning.** 10g rounds 1 and 2
   were both "test currently passes but for the wrong reason"
   findings. The reviewer's first finding tightened the assertion
   (drain-until-substring → first-frame-only); the second
   eliminated a PTY-echo dual-emit at the source. The contract is
   the production invariant, not whatever the test happens to
   observe.

6. **"Global mutation IS the test-isolation hazard"** (10e-b r3).
   When parallel tests flake on a shared atomic / env-var / static
   the right answer is usually "make the state per-handle (or
   structural) instead of global," not "lock more." Lock-based
   fixes accumulate; structural fixes don't.

7. **"Don't do throwaway work — defer to the slice that owns it."**
   sub-2c could have added a daemon→TUI session-push direction,
   but that direction reverses in 10d. The fix's shape is what
   10d owns; building-then-discarding is pure churn. Multiple
   slices applied this principle to defer fixes to their natural
   slice.

8. **Reviewer Q&A pattern.** Surface open questions upfront with
   defaults, get explicit confirm/refine. Slices that did this
   (10e-d, 10f, 10g) avoided design-rework rounds. Slices that
   started coding without the Q&A often hit "the design should
   have been X, not Y" review rounds.

## Phase 2: Workflow events over RPC

**Goal** (from `doc/persistent-host-daemon.md` §"Phase 2:
Workflow events over RPC"): drop the TUI's file-tail of
`events.jsonl`. Workflow state + events flow exclusively through
`events.subscribe` (streaming RPC) and `workflow.get_state` (RPC).
After this lands, the TUI doesn't touch the daemon's filesystem
for any runtime data — precondition for Phase 3 (multi-host).

The design doc's Phase 2 description stands; this section pins
the implementation specifics + slice sequence that the doc
intentionally leaves open.

### Design ambiguities resolved (defaults baked into the slice plan)

The design doc leaves three implementation choices open. Defaults
below mirror Phase 1's `manifest.watch` (10e) precedent for
consistency:

1. **`events.subscribe(filter)` — no filter param.** Daemon
   broadcasts every event; TUI subscribes to all and filters
   client-side by `run_id` when rendering a specific run.
   Matches `manifest.watch`'s no-filter design. Cheaper than
   per-subscriber filter state; events.jsonl traffic is low
   (a feedback workflow generates <100 events end-to-end).

2. **`workflow.get_state` — full `WorkflowRun` snapshot.**
   Daemon serializes the existing `cm_daemon::workflow::run::WorkflowRun`
   struct verbatim, including `history`, `events_offset`,
   `rejected_findings`, `role_baselines`, etc. Cheaper than
   designing a slim snapshot type; gives the TUI everything it
   currently reads from disk in one call.

3. **Reconnect/replay — snapshot-then-live frame model, no
   replay buffer.** Mirrors `manifest.watch`'s 10e-b shape:
   on subscribe, daemon sends one `WorkflowStateSnapshot` frame
   per active run (`workflow.get_state` payload), then live
   `WorkflowEvent` frames for subsequent events. Fresh subscribers
   get current state via the snapshot, future state via the
   live stream. No separate ring-buffer needed.

### Slice sequence

Each slice leaves the tree green. The architecture closely
mirrors 10e (manifest.watch), so most slices have a Phase 1
analog and should compress vs. their 10e counterparts.

#### Slice 11a — Daemon-side broadcaster on `events::read_new`

Mirror of 10e-b's `ManifestWatcher`. Add a broadcaster wrapper
around the existing `daemon/src/workflow/events.rs::read_new`
loop in the daemon's poller. Each tick's parsed `Event` goes
to both the existing in-daemon dispatch AND a fan-out broadcast
to all subscribers (`sync_channel(N)` + `try_send` + `retain`
for slow-subscriber drop, same shape as `ManifestWatcher`).

- New type: `WorkflowEventWatcher` in `daemon/src/workflow/`
  with `subscribe()` returning `(Receiver<WorkflowEvent>, SubscriptionGuard)`.
- Wire into `daemon/src/state.rs::DaemonState`.
- The poller's existing tick callbacks invoke
  `state.workflow_event_watcher.broadcast(event)` after the
  in-daemon dispatch settles.

**Acceptance**: T1 — one subscriber sees every event the poller
processes, in order. T2 — slow subscriber is dropped without
blocking the broadcast. T3 — concurrent broadcasts all delivered.
T4 — RAII guard drops slot on receiver drop.

**Dependencies**: Phase 1 (already shipped).

#### Slice 11b — `events.subscribe` RPC + streaming consumer

Mirror of 10e-b's `dispatch_manifest_watch` + `handle_manifest_watch_stream`.

- New `StreamKind` variants: `WorkflowEventStateSnapshot`
  (initial frame per active run), `WorkflowEvent` (live diff
  frame), and a `WorkflowEventEnd`-equivalent on disconnect.
- New dispatch arm `dispatch_events_subscribe` (Operator-only,
  empty params).
- `daemon/src/control/stream.rs::handle_events_subscribe_stream`
  — on subscribe, iterate `state.workflow_runs`, emit one
  `WorkflowEventStateSnapshot` per active run, then drive
  the live channel. Heartbeat ticks for idle-disconnect
  detection (same 30s default as `manifest.watch`).
- Wire the dispatcher's outcome handler in `daemon/src/lib.rs`.

**Acceptance**: T5-T10 mirror 10e-b's tests — snapshot-then-live,
slow subscriber drop, concurrent broadcasts, reconnect,
Session-caller rejected, heartbeat idle-disconnect, RAII guard,
no accumulation across many reconnects.

**Dependencies**: 11a.

#### Slice 11c — `workflow.get_state(run_id)` RPC

Cold-read snapshot for TUI attach paths that need state without
subscribing to the stream (e.g. workflow history view on `A-y`).

- New dispatch arm `dispatch_workflow_get_state` (Operator-only,
  params `{run_id: String}`).
- Returns full `WorkflowRun` serialized as JSON.
- Error cases: `NotFound` for unknown `run_id`.

**Acceptance**: T11 — happy path returns expected snapshot.
T12 — unknown run_id → NotFound. T13 — Session caller rejected.

**Dependencies**: 11a structurally; can land in parallel with 11b.

#### Slice 11d — TUI consumer module + App integration

Mirror of 10e-c's `manifest_watch.rs` + `drain_manifest_watch_events`.

- New `tui/src/workflow_watch.rs` module:
  - `WorkflowEventConsumer` with reconnect loop (1s → 30s exp
    backoff).
  - `WorkflowEvent` event-channel enum: `Snapshot(WorkflowRun)`,
    `Event(WorkflowEvent)`.
  - `should_spawn` + `maybe_spawn_for_app()` matching
    manifest_watch's shape.
- `App.workflow_watch_rx: Option<Receiver<WorkflowWatchEvent>>`
  field; spawn at `App::new`.
- `App::drain_workflow_watch_events` in main-loop tick, applies
  events to `App.workflow_runs` via existing controller paths.
  Conservative-merge on snapshot (mirror 10e-c r1 F1's pattern):
  daemon's snapshot is authoritative for fields the TUI hasn't
  observed yet (history beyond local `events_offset`); local
  in-memory state wins for fields the TUI has already applied.

**Acceptance**: T14-T20 — consumer spawns, snapshot arrives,
diff events apply, reconnect-resilience, channel-disconnect
exits outer loop.

**Dependencies**: 11b + 11c (the consumer needs both RPCs).

#### Slice 11e — Delete TUI file-tail

The reveal slice. Switch the TUI's workflow-view code path
from file-tail (`tui/src/workflow/events.rs::read_new` reads,
direct `~/.cm/workflow-runs/<id>/state.json` reads) to the
RPC-driven path landed in 11d.

- Audit and delete file-read sites in TUI:
  - `workflow::events::read_new` callers in `tui/src/workflow/controller.rs`.
  - Any direct `state.json` reads.
- Workflow controller's tick draws events from
  `workflow_watch_rx` instead of file-tailing.
- Daemon still WRITES `events.jsonl` for durability (the design
  doc explicitly preserves this — single producer, two
  consumers: file durability + RPC broadcast).

**Acceptance** (design-doc named):
- Grep proves TUI no longer references
  `tui/src/workflow/events.rs::read_new` or any path under
  `~/.cm/workflow-runs/` for reads.
- Manual smoke: feedback-mode workflow worker → reviewer →
  manager → done with `A-y` history correct.
- `events.jsonl` durability unchanged (compare a feedback run
  before/after; same records).

**Dependencies**: 11d.

#### Slice 11f — Reconnect acceptance test

Named acceptance gate for Phase 2 (per design doc):
> "Killing the TUI mid-workflow and reattaching shows the
> current active role and recent transitions (via
> `workflow.get_state` + last N events from the daemon's
> broadcast buffer)."

In-process integration test in `daemon/src/control/stream.rs::tests`
mirroring 10g's pattern:
1. Spawn daemon-side workflow run with a few transitions.
2. Subscribe attach1; observe snapshot + diff frames; drop.
3. Subscribe attach2 (fresh wire); assert FIRST frame is
   `WorkflowEventStateSnapshot` containing the post-transition
   `WorkflowRun` (history reflects all transitions made before
   the disconnect).

Mutation-verify by dropping the snapshot-send in
`handle_events_subscribe_stream` and confirming the test fails
with the same shape of contract-violation error 10g surfaces.

**Acceptance**: T21 (named); T22 mirror of 10g's multi-reconnect
(3 cycles, each first-frame is the snapshot).

**Dependencies**: 11b + 11d + 11e (full path must be live).

### Phase 1 follow-ups absorbed vs. deferred

Of the 10 "Phase 1 follow-ups" items above:
- **Absorbed into Phase 2**: none directly. They're parallel
  axes. 11e's file-tail deletion does NOT subsume #3
  (`tui/src/control/server.rs` removal) — that's the workflow
  control SOCKET, distinct from the events file-tail.
- **Still deferred**: all 10. Phase 2 is workflow-events-RPC
  scoped; the carry-overs remain valid as future-slice
  candidates.

### Estimated commit count

- 11a: 1 commit (broadcaster + tests).
- 11b: 1-2 commits (RPC + streaming consumer + 6 tests).
- 11c: 1 commit (get_state RPC + 3 tests).
- 11d: 1-2 commits (TUI consumer + App integration + 7 tests).
- 11e: 1-2 commits (file-tail deletion + workflow-view path flip).
- 11f: 1 commit (reconnect acceptance test).

Total estimate: 6-9 commits. Phase 1's 10e analog ran ~7
commits (10e-a/b/c/d + r1 review rounds), so Phase 2's
6-9-commit estimate has Phase 1's review-multiplier built in.
Review-fix rounds expand the count if they surface real
findings — which is the point of the feedback workflow.

### What this Phase 2 plan is NOT

- A spec for `events.subscribe`'s wire-level JSON shape — that
  lives in the slice 11b PR.
- A decision on Phase 3 multi-host transport. Phase 2 closes
  the file-system dependency that blocks Phase 3 but doesn't
  start Phase 3.
- A revision to `doc/persistent-host-daemon.md`. The design
  doc stands; ambiguity resolutions captured above are
  implementation defaults, not doc updates.

## Future work

- **Socket-close cancellation for `mcp_start_session` slot wait** (sub-2b-3 review-9): the bounded 20s wait closes the orphan-on-client-timeout bug, but the cleaner shape is to abort the wait when the client socket closes. The daemon's current RPC framework (synchronous per-connection threads, blocking reads) doesn't expose a "client disconnected" signal during a blocking wait; wiring it would require either an async runtime or a peer-poll thread. Deferred — the bounded wait alone is sufficient for the named acceptance criterion.

## Known costs / future work

- **Per-worktree spawn+detect serialization for the MCP path** (sub-2b-3 review-4 #2). `mcp_start_session` holds a per-worktree mutex from spawn through detector binding so concurrent same-worktree spawns can't cross-bind transcript files. Spawns in different worktrees don't serialize against each other. Acceptable today because workflow agents in the same worktree spawn sequentially in practice (the workflow controller drives transitions one role at a time). A future slice can replace serialization with content-association — e.g. inject a spawn-tag env var that the engine echoes into its first transcript line, then match-by-tag instead of match-by-newest — if latency becomes a problem.

- **TUI crash-cleanup bound for `tui_sessions`** (10d-1 round-3). If the TUI crashes without graceful shutdown, the daemon retains the last-pushed `tui_sessions` snapshot until the restarted TUI's first push replaces it. During that window, a stale uid could authorize as a valid TUI session for workflow methods (post-10d-2), but the downstream session operation would fail because the session itself doesn't exist anywhere actionable. Mitigation in 10d-2: when auth via `tui_sessions` succeeds, treat "session uid resolves to a known task but no live session anywhere" as a clear error, not a bug — document under "transitional crash semantics" when that slice lands. Durable fix: daemon-side connection-lifecycle awareness for the TUI's control-client connection — clear `tui_sessions` when the TUI's RPC connection drops. Requires a connection model that distinguishes TUI from other callers; defer to a later slice, not blocking Phase 1.

- **`DAEMON_METHODS` split: Python-routable vs. dispatch-alignment** (10d-1 round-4). The Python-side `DAEMON_METHODS` frozenset currently serves two roles: (1) routing decisions in `control_client.call` (Python callers route methods in the set to the daemon socket), and (2) the alignment-test snapshot in `test_socket_route_selection.py` (must equal daemon's dispatch arms). 10d-1 added `tui.update_sessions_snapshot` to the set purely for role (2) — Python never calls it; it's Operator-only push from the TUI. Cleaner shape would be `SESSION_CALLABLE_METHODS` (role 1, what Python routes) and `OPERATOR_ONLY_METHODS` (TUI-pushed; not Python-routable), with the alignment test asserting `union == dispatch_arms`. Defer; the inline comment on the entry signals the intent for now.

- **Workflow events.jsonl torn-record durability above PIPE_BUF** (10d-2a round-6). `WorkflowEventsWriter::append_event` issues exactly one `write_all` of the full payload (optional leading `\n` + JSON + trailing `\n`) to an `O_APPEND` fd. For payloads under `PIPE_BUF` (~4KB on Linux) this is atomic at the kernel level — a daemon kill mid-write either writes the whole record or none of it. For larger payloads (e.g. a `workflow_transition` whose `prompt` field carries an oversize template expansion) the kernel may split the write, and a crash in the gap leaves a torn record. The round-6 tailer's parse-on-EOF fallback recovers torn records whose bytes happen to be a complete JSON object (the common case when only the trailing newline was lost); truly partial JSON still holds the offset and is retried — correct for an in-flight writer, but a crash leaves it stuck. Phase 1 events are typically <1KB; realistic exposure is zero. A durable fix (truncate-to-last-good-offset on daemon restart, or fsync-per-record with LSN ordering) belongs to a dedicated durability slice, not piecemeal here.

## Known flake-class issues (deferred)

- `control::methods::tests::read_session_output_with_cursor_returns_only_new_bytes` flakes under high test-binary parallelism. The test spawns a real `bash` PTY and asserts that a since-cursor read excludes pre-cursor output. Under parallel load, the bash startup banner / first-prompt `[?2004h` interleaves into the post-cursor read window — the assertion sees pre-cursor text in the response. Reproduces only with `cargo test --workspace`; passes serialized (`--test-threads=1`) or run alone. Surfaced during sub-2b-3 review-3 — not introduced by that slice. Defer to a dedicated PTY-test-isolation slice; the fix likely needs either a synchronization signal (waiting for the prompt before issuing the marker echo) or a stronger cursor that anchors on a known sentinel rather than byte offsets.

- `tui::workflow::controller::tests::workflow_transition_rollback_preserves_concurrent_tui_role_sessions_update` (10d-2c-1 round-12 F1 acceptance). The test uses background-thread polling against on-disk state to synchronize the rollback assertion — inherently timing-sensitive under high parallelism. Reviewer surfaced flake during 10d-2c-2-1; fix candidate is a deterministic signal (explicit barrier or a test-only hook on the rollback path) replacing the polling. Defer; out of scope for 2c-2-1, which doesn't touch the rollback code path.

- `tui::client_session::tests::client_session_write_200kib_arrives_at_daemon_pty_without_drops` (and the rest of the cm-tui `ClientSession::new` real-PTY suite) flakes under the same workspace-concurrency conditions as the daemon-side PTY tests above. Resurfaced during 10d-2a rounds 2–3 (each round added more `daemon/src/workflow/events.rs` tests, slightly perturbing the parallel scheduler enough to re-hit the flake). Passes reliably in isolation (`cargo test -p claude-manager-tui client_session_write_200kib`). Same root cause class: a real-PTY test in `tui/src/client_session.rs` racing the daemon's spawn+attach under parallel cold-build load. Defer with the existing PTY-test-isolation slice. The 10d-2a round-3 review noted reaching this and accepted further hardening of this class belongs to that dedicated slice, not piecemeal in feature slices.

## What this doc is NOT

- An implementation. The point is to make the slicing committable; each slice gets its own design pass when it's the next-up.
- A schedule. Slices land when they're ready; no ETAs.
- A defense of the current scope. If a reviewer reading this catches a missing field or a wrong boundary, that's the right place to push back — better here than mid-rewire.
