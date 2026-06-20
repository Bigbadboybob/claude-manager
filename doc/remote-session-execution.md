# Remote session execution from the TUI

## Summary

Let the TUI create, attach to, and run sessions (and workflows) on a remote daemon host (cm-manager) — the same `A-n` / `A-s` / `A-a` / `A-f` flows that work locally, but targeting `active_host = manager`. The blocker today is that the TUI resolves all paths locally (worktree, MCP config, argv, env) and ships them to the daemon, which is wrong for a remote host; the fix is a daemon-side resolved-create path so the remote daemon builds its own worktree/argv/env, plus a repo-availability story so "any repo" can be materialized on the VM. Interactive attach then rides the existing ssh-unix tunnel unchanged.

## Problem

`A-H` already switches `active_host` and the tunnel/host-pool/remote-daemon plumbing is built and validated (the daemon-side workflow e2e ran headlessly on cm-manager). But every interactive session op is hard-gated: `guard_local_host_only` (tui/src/app.rs:562) returns an error for any non-local host, applied at `A-n` (create_local_session, ~app.rs:9967), `A-s` (spawn_session_on_workspace, ~app.rs:10329 / ~5603), `A-a` (attach_active, ~app.rs:10244), `A-f`/`A-l`, and the MCP `start_session` (~app.rs:7481). So switching to `manager` and pressing `A-n`/`A-a` just errors with "remote-execution support is deferred."

The guard exists for a concrete reason (tui/src/app.rs comment ~10236): the local-claude branch sends the workspace's local-filesystem worktree path and a per-session MCP config under `~/.cm/mcp/<uid>/` to the daemon. On a remote daemon those paths don't exist — the worktree isn't there, `~/.cm/mcp/<uid>/claude.json` was written on the laptop, and the baked-in `CM_DAEMON_SOCKET` / `CM_TUI_SOCKET` point at the laptop's sockets. The cost: there is no interactive way to run work on the always-on GCP box; remote execution today only happens by driving the daemon directly over the control socket (operator/MCP/headless workflows), not through the normal TUI UX.

## Goals

- `A-n` / `A-s` on `active_host = manager` create a fresh git worktree ON cm-manager and spawn a session there, with all paths resolved daemon-side.
- `A-a` attaches interactively to a remote session — live PTY output and keystroke input over the ssh-unix tunnel, with working resize and kitty-keyboard encoding.
- "Any repo" works: the remote daemon can materialize a repo it doesn't already have (registry + clone-on-demand), not just `~/code/projects/<name>`.
- `A-f` launches a feedback workflow on cm-manager via the existing daemon-side workflow execution.
- Remote sessions render in the TUI sidebar (host-grouped) via the remote daemon's `manifest.watch`.
- The local path is unchanged and unregressed — local `A-n`/`A-s`/`A-a` behave byte-identically; `guard_local_host_only` is replaced only on the paths this doc enables.

## Non-goals

- Cloud push/pull via GCS (`A-p`/`A-l`, the `worker/` dispatch path and `doc/session-push-pull-to-persistent-host.md`). That is a separate, older mechanism; this doc is the live multi-host alternative and does not touch it.
- TLS transport for hosts (the `TcpTls` placeholder in hosts.rs / NOTES Phase 12h). Only the existing `ssh-unix` transport is in scope.
- Unifying the local session-create path onto the new daemon-side RPC. Local keeps its current TUI-side resolution; routing local through the new RPC too is a later cleanup (noted under Risks).
- Multi-host fan-out / running one logical task across hosts simultaneously. One session targets one host (the `active_host` at create time), as today.
- Secrets/credentials provisioning on the VM (Anthropic/OpenAI auth, git creds). cm-manager is already authenticated for claude+codex; this doc assumes that and does not manage credential distribution.

## Current state

Two daemon spawn contracts exist today:

- Low-level `start_session` (daemon/src/control/methods.rs `StartSessionParams` ~183, `fn start_session` ~410): the caller supplies full `argv`, full `env`, and `working_dir`; the daemon just execs. The comment at methods.rs ~174 is explicit: "the daemon doesn't interpret an engine name … agent-specific knowledge stays TUI-side where the config files live." The TUI's local `A-n` uses exactly this: `create_local_session` (app.rs:9946) calls `worktree::create_worktree` (daemon/src/worktree.rs:84) to make the worktree, `mcp_config::build_args` / `build_env` (tui/src/mcp_config.rs:299 / :129) to build argv+env with LOCAL paths and a locally-written `~/.cm/mcp/<uid>/claude.json`, then ships it all via `rpc_start_session_full` (tui/src/client_session.rs:606).
- High-level daemon-resolved spawns: `mcp_start_session` (methods.rs ~5772) and workflow participants in `start_workflow` (methods.rs ~3127) call the DAEMON's own `mcp_config::build_args` / `build_env` (daemon/src/mcp_config.rs:312 / :94), which resolve the daemon-local MCP server (`resolve_server_path` :135, from `daemon.toml` `mcp_server_path`), the daemon-local venv (`resolve_python_interpreter` :156), and inject the daemon's own `CM_DAEMON_SOCKET` / `CM_TUI_SOCKET` (methods.rs ~668). The daemon writes the per-session MCP config to ITS OWN `~/.cm/mcp/`.

So the daemon already knows how to build correct argv/env for its own host — the interactive create path simply never asks it to. What the daemon-resolved paths do NOT currently do is create a worktree: `mcp_start_session` spawns in an existing workspace; `start_workflow` takes a `worktree` from the operator. Worktree creation (`create_worktree` :84, `create_subtask_worktree` :179) exists as a library function but is invoked TUI-side today.

Attach is already host-agnostic. `ClientSession::new` (tui/src/client_session.rs ~289) does `session.attach` → dial `attach_addr` → `attach.open` → stream, all over `config.daemon_socket` which comes from `host_pool.for_host(host_id)` (app.rs ~932). The ssh-unix tunnel (`ssh -fN -L <local>:<remote>`, tui/src/host_pool.rs) is a transparent stream forward; PTY-byte frames, input frames, and `Resize` frames (daemon/src/control/stream.rs ~670) all ride it, and kitty-keyboard is set at `Term` construction on both paths (client_session.rs ~409). The only thing stopping interactive remote attach is the `guard_local_host_only` at app.rs:10244.

The repo gap: `daemon/src/config.rs` `DaemonConfig` (~104) has no repos field; `find_local_repo` (daemon/src/worktree.rs:365) only checks `~/code/projects/<name>` and cwd, with no clone fallback. cm-manager has no `~/.cm/projects` registry. So even with daemon-side resolution, the remote daemon can't locate or fetch an arbitrary repo.

## Proposed design

### Mechanism overview

Add a high-level, worktree-creating, daemon-resolved create RPC and route remote `A-n`/`A-s` to it; lift the guard on the attach and workflow paths (which already work over the tunnel once the session exists remotely).

```
A-n on active_host=manager
      │  TUI sends a HIGH-LEVEL request (no local paths):
      │    { uid, workspace_id, label, engine, repo_url, start_branch, slug, task_id?, cols, rows }
      ▼
host_pool.for_host(manager) ── ssh-unix tunnel ──▶ cm-manager cm-daemon
                                                      │ 1. resolve repo (registry / clone-on-demand)  [Phase 2]
                                                      │ 2. create_worktree(repo, slug, start_branch)  → remote worktree path
                                                      │ 3. daemon mcp_config::build_args/build_env     → daemon-local argv/env + ~/.cm/mcp/<uid>
                                                      │ 4. spawn PTY (existing start_session core)
                                                      │ 5. register workspace+session; broadcast ManifestDiff::Added
                                                      ▼
                                            returns { session_uid, worktree_path, workspace_id, transcript_path? }
A-a ──▶ session.attach/attach.open/stream over the SAME tunnel (already works) ── live PTY
```

### Host is workspace-scoped (`active_host` only seeds new workspaces)

A workspace is one git worktree — one directory on one host's filesystem — so every session in it necessarily runs on that one host; there is no cross-host-within-a-workspace case (a subtask gets its own worktree, i.e. its own workspace). Host is therefore a property of the WORKSPACE (today carried on each `TerminalSession.host_id`, which all sessions in a workspace agree on), not a global mode. `A-H` / `active_host` means only "the host the NEXT new workspace is created on" — it cannot relocate existing work, because a worktree can't move across machines with a keystroke. Consequently every op on an EXISTING workspace/session must resolve its host from that entity, never from the global `active_host`: `A-n` (new workspace) reads `active_host`; `A-f` / `A-s` / `A-a` read the workspace's/session's `host_id` (the workflow-respawn path already does this via `guard_local_host_only(&ts.host_id, …)`, app.rs:1230).

A precursor bugfix already landed this routing for the existing-entity ops: `A-f` now resolves the launch host from `ws.sessions.first().host_id` (app.rs `launch_workflow_via_daemon`), `A-s` from the target workspace's sessions (and the new session inherits that host, not `active_host`), and `A-a` spawns locally (it only runs on an empty workspace, whose worktree is local) — each guarded with `guard_local_host_only` so a remote host fails loud instead of misrouting. This fixed a real crash: `A-f` previously used `active_host`, so launching a workflow on a local workspace while `A-H` was parked on `manager` fired a doomed cross-host launch (local worktree path + local uids sent to the remote daemon) and froze the TUI on the 150s `start_workflow` RPC over a flaky tunnel. Phase 3 below builds directly on this: once `create_session` can make a remote workspace, that workspace carries a remote `host_id`, so these same ops route to the remote daemon instead of being guarded off — no further per-op host plumbing needed.

### Daemon: `create_session` RPC (daemon-side resolution)

Add an Operator+Session-callable method (e.g. `create_session`, name TBD) that takes a high-level request and does the resolution the TUI does today, but on the daemon host:

- Params: `uid`, `workspace_id`, `label`, `engine` (claude-code/codex/bash), `repo_url`, `start_branch: Option<String>`, `slug`, `task_id: Option<String>`, `cols`, `rows`, plus the optional memory-cap triple. NO `argv`/`env`/`working_dir`/`worktree_path` — those are the daemon's to compute.
- Body: resolve the repo to a local path (Phase 1 uses `find_local_repo`; Phase 2 adds registry+clone) → `worktree::create_worktree(&repo, &slug, start_branch)` → `mcp_config::build_args(engine, uid, None, configured_mcp_server_path)` + `mcp_config::build_env(uid, None)` (the daemon-side ones) → assemble the same `SpawnParams` the existing `start_session` core uses → spawn → register the workspace (auto-register on unknown `workspace_id`, as `start_session` already does at methods.rs ~516) and the session → broadcast the manifest diff.
- Returns: `{ session_uid, worktree_path, workspace_id, transcript_path? }` so the TUI can bind its `TerminalSession` to the daemon-created identity.
- Reuse, don't fork: factor the spawn tail (`SpawnParams` build + register + manifest broadcast) shared with `start_session`/`mcp_start_session` so the three entry points converge on one spawn core. `create_session` = `resolve repo + create worktree + daemon build_args/build_env` then call that shared core.

This is the literal "daemon-side path resolution" the guard's error message defers to. It works against the LOCAL daemon too (the daemon is just `local` in that case), so Phase 1 is fully testable without cm-manager.

### Daemon: repo availability (registry + clone-on-demand)

Extend `DaemonConfig` (daemon/src/config.rs ~104) with a repos story so the daemon can materialize "any repo":

- `daemon.toml` gains a repos section, e.g. `repos_dir = "~/code/projects"` (where clones live) and an optional allowlist `[[repo]] name = "...", url = "..."` for repos the daemon may clone.
- Repo resolution becomes: `find_local_repo(repo_url)` first (unchanged fast path); on `None`, if the URL is permitted (allowlist or `allow_clone = true`), `git clone <url> <repos_dir>/<name>` (shallow acceptable) then resolve; otherwise return a clear `NotFound` naming the repo so the TUI can surface "repo not available on host `manager`".
- Security: cloning arbitrary URLs executes remote-named code-fetch on the VM. Default to an allowlist; gate open clone behind an explicit `allow_clone` flag. See Risks.

### TUI: route remote create to `create_session` + lift the create guards

- In `create_local_session` (app.rs:9946) and `spawn_session_on_workspace` (app.rs ~10314): branch on `active_host`. Local → existing TUI-side path (unchanged). Remote → build the high-level request (no local worktree creation, no local MCP config, no local argv/env) and call a new `rpc_create_session` (tui/src/client_session.rs) over `host_pool.for_host(active_host)`; bind the returned `session_uid`/`worktree_path` into a `TerminalSession` pinned to `active_host`.
- Replace `guard_local_host_only` at the A-n/A-s sites with the remote branch. The guard helper stays for the still-unsupported paths until their phases land.
- Sidebar observation: ensure the TUI subscribes to `manifest.watch` on the active remote host so daemon-created remote sessions render (host-grouped). Confirm during implementation whether multi-host `manifest.watch` subscription is already wired (the local restore path reads the on-disk manifest at app.rs ~4415, which is local-only); if not, add a per-host subscription when a remote host becomes active.

### TUI: interactive attach on remote (lift the A-a guard)

Remove `guard_local_host_only` at `attach_active` (app.rs:10244). The attach stack already routes through `host_pool.for_host` and the tunnel is transparent (confirmed: `rpc_session_attach` client_session.rs ~715, `rpc_attach_open` ~747, `dispatch_session_attach` daemon/src/control/dispatch.rs ~895, resize stream.rs ~670). The session must exist remotely with valid remote paths first — i.e. created via `create_session` — so this phase depends on the create path.

### TUI: remote workflows (lift the A-f guard)

`start_workflow` is already daemon-driven and proven headless on cm-manager. Route `A-f` on a remote `active_host` to the remote daemon's `start_workflow` over the tunnel (it takes a `worktree`/workspace, which by then exists remotely), and lift the A-f guard. Participants spawn on cm-manager; the poller drives them; the TUI observes via the workflow event/manifest streams.

### Alternatives considered

| Alternative | Why rejected |
|---|---|
| TUI resolves paths, then path-maps local→remote before sending | Brittle: must mirror `~/.cm/mcp`, sockets, venv, repo layout across hosts; the daemon already resolves its own paths correctly, so map nothing — let the daemon do it. |
| Shared filesystem (NFS) so local paths are valid remotely | Heavy ops dependency, fragile, doesn't generalize beyond one mount; defeats the point of an independent always-on host. |
| Extend low-level `start_session` with a "resolve daemon-side" flag instead of a new RPC | Overloads a method whose contract is "exec this exact argv"; a distinct `create_session` keeps the low-level escape hatch stable and the contracts legible. Implementation still shares the spawn core. |
| Use GCS push/pull (`A-p`/`A-l`) for the dedicated host | Different model (ephemeral workers, async, no live attach) and has an open upload-on-completion gap; the user wants live interactive sessions on an always-on box. Out of scope (non-goal). |
| Require repos to be pre-cloned on the VM (no clone-on-demand) | Acceptable as Phase 1's interim (works for present repos) but fails the stated "any repo" goal; clone-on-demand is Phase 2. |

## Risks and open questions

- Risk: clone-on-demand runs `git clone` of caller-named URLs on the VM (code fetch + arbitrary network). Mitigation: default to a `daemon.toml` allowlist; gate open cloning behind an explicit `allow_clone` flag; never auto-run repo setup scripts on clone without opt-in.
- Risk: regressing the local create path. Mitigation: remote routing is a new branch; local keeps the existing path; Phase 3 acceptance includes "local A-n/A-s/A-a unchanged (existing tests green)". The guard helper is removed only per-site, per-phase.
- Risk: remote sessions don't appear in the sidebar because the TUI only reads the local on-disk manifest (app.rs ~4415). Mitigation: Phase 3 confirms/adds per-host `manifest.watch` subscription; acceptance includes "a daemon-created remote session renders in the host-grouped sidebar."
- Risk: attach stream stalls/disconnects when the tunnel respawns mid-stream. Mitigation: `host_pool` already respawns dead tunnels; validate attach survives a tunnel bounce, or surfaces a clean reattach, in Phase 4's e2e.
- Risk: a remote session created but the worktree creation half-fails (repo resolved, `git worktree add` errors) leaves an orphan branch/dir. Mitigation: `create_session` cleans up a partially-created worktree on spawn failure (mirror the local path's fail-fast-before-worktree ordering at app.rs:9967), and returns a typed error.
- Open question (does not block Phase 1): should local `A-n` eventually route through `create_session` too (one path for both)? Deferred — unify after the remote path is proven (non-goal here).
- Open question: where should clones live on cm-manager — `~/code/projects` (matches `find_local_repo`) or a dedicated `~/.cm/repos`? Phase 2 picks `repos_dir` with a default; confirm with the operator's VM layout.

## Implementation plan

### Phase 1: Daemon `create_session` RPC (daemon-side worktree + argv/env resolution)

- **Goal:** a new daemon RPC creates a worktree and spawns a session with daemon-resolved argv/env/MCP-config, returning the created identity — exercised against the local daemon.
- **Scope:** daemon/src/control/methods.rs (new `create_session` method + params struct; factor the shared spawn tail out of `start_session`), daemon/src/control/dispatch.rs (dispatch arm, Operator+Session callable), reuse daemon/src/worktree.rs `create_worktree` and daemon/src/mcp_config.rs `build_args`/`build_env`.
- **Out of scope for this phase:** repo clone-on-demand (Phase 2), any TUI wiring (Phase 3), attach/workflow guard changes (Phases 4–5).
- **Acceptance criteria:**
  - A test: `create_session` with `{uid, workspace_id, label, engine=claude-code, repo_url, slug}` for a repo present via `find_local_repo` creates a worktree at the expected `~/.cm/worktrees/<repo>-<slug>` path on `cm/<slug>`, spawns a session whose argv comes from the daemon's `build_args` and whose env contains the DAEMON's `CM_DAEMON_SOCKET`/`CM_TUI_SOCKET` and a daemon-written `~/.cm/mcp/<uid>/claude.json` (not a TUI-supplied one).
  - A test: the response carries `{session_uid, worktree_path, workspace_id}` and the session is registered in `state.sessions` with a broadcast `ManifestDiff::Added`.
  - A test: an unknown `repo_url` (not resolvable, clone not yet implemented) returns a typed `NotFound`/`InvalidParams` naming the repo; a `git worktree add` failure cleans up and returns an error (no orphan session).
  - The existing low-level `start_session` and `mcp_start_session` paths are unchanged (existing tests green); the shared spawn-tail refactor is behavior-preserving.
- **Dependencies:** none.

### Phase 2: Daemon repo availability (registry + clone-on-demand)

- **Goal:** the daemon can resolve "any repo" — fast-path local, else clone a permitted URL — so `create_session` works for repos not pre-present.
- **Scope:** daemon/src/config.rs (`DaemonConfig` gains `repos_dir` + optional repo allowlist / `allow_clone`), daemon/src/worktree.rs (repo resolution helper that wraps `find_local_repo` with a clone fallback into `repos_dir`), wiring into `create_session`'s repo-resolution step.
- **Out of scope for this phase:** TUI wiring; credential management for private repos (assume host git auth).
- **Acceptance criteria:**
  - A test: with a configured `repos_dir` and an allowlisted URL, resolving a repo absent from disk performs a clone into `repos_dir/<name>` and then resolves; a second resolve reuses it (no re-clone).
  - A test: a URL not on the allowlist (with `allow_clone=false`) returns a clear `Unauthorized`/`NotFound` rather than cloning.
  - A test: `daemon.toml` without a repos section preserves today's behavior (`find_local_repo` only; no clone) — backward compatible.
- **Dependencies:** Phase 1.

### Phase 3: TUI routes remote `A-n`/`A-s` to `create_session` + lifts create guards + remote sidebar

- **Goal:** pressing `A-n`/`A-s` with `active_host = manager` creates a worktree+session on cm-manager and it renders in the sidebar.
- **Scope:** tui/src/app.rs (`create_local_session` ~9946 and `spawn_session_on_workspace` ~10314: local vs remote branch; remove the A-n/A-s `guard_local_host_only` sites ~9967/~10329), tui/src/client_session.rs (new `rpc_create_session`), tui/src/host_pool.rs / manifest subscription (subscribe to the active remote host's `manifest.watch` so remote sessions render).
- **Out of scope for this phase:** interactive attach (Phase 4), workflows (Phase 5), unifying the local path.
- **Acceptance criteria:**
  - A handler-level test: with `active_host = remote`, `A-n` builds a high-level `create_session` request (no `argv`/`env`/`working_dir`/local MCP path) and sends it via the remote host's socket; with `active_host = local`, the existing local path runs unchanged (asserted).
  - A test: the returned `{session_uid, worktree_path, workspace_id}` binds a `TerminalSession` pinned to `active_host=remote`.
  - A test: a daemon-broadcast remote session appears in the host-grouped sidebar (manifest.watch subscription for the active remote host).
  - Existing local `A-n`/`A-s` tests pass; the A-n/A-s guard-site tests are updated to assert the remote branch (not a hard error).
- **Dependencies:** Phase 1 (Phase 2 enables the full "any repo"; without it, remote create works only for repos already on the VM).

### Phase 4: Interactive attach on remote sessions

- **Goal:** `A-a` on a remote session streams its PTY live over the tunnel — output, input, resize, kitty-mode — and an end-to-end run on cm-manager works.
- **Scope:** tui/src/app.rs (`attach_active` ~10192: remove `guard_local_host_only` ~10244; ensure the attach path uses `host_pool.for_host(active_host)` end-to-end), validation of the existing stream stack (client_session.rs attach steps, daemon/src/control/stream.rs).
- **Out of scope for this phase:** workflows (Phase 5).
- **Acceptance criteria:**
  - A test (handler-level): `attach_active` with a non-local host no longer short-circuits on the guard and routes the attach RPCs through the active host's socket.
  - Manual/e2e on cm-manager: create a session via `A-n` on `manager`, `A-a` into it, type a command and see output, resize the window and confirm reflow, and confirm Enter submits (kitty-mode) — documented in the testing strategy as a scripted operator-socket + manual-attach check.
  - Local attach unchanged (existing tests green).
- **Dependencies:** Phase 3.

### Phase 5: Remote workflows from the TUI (`A-f`)

- **Goal:** `A-f` on `active_host = manager` launches a feedback workflow on cm-manager (participants spawn there; the daemon poller drives them).
- **Scope:** tui/src/app.rs (the `A-f` launch path `open_workflow_launch` / `launch_workflow_via_daemon`: route to the remote host's socket and remove the A-f `guard_local_host_only` site), confirm the TUI observes the remote run via the workflow event/manifest streams.
- **Out of scope for this phase:** anything beyond routing the already-daemon-driven workflow to the remote host.
- **Acceptance criteria:**
  - A handler-level test: `A-f` with `active_host = remote` sends `start_workflow` to the remote host's socket against a remote workspace; local `A-f` unchanged.
  - E2e on cm-manager: `A-n` a worktree on `manager`, `A-f` a feedback workflow on it, observe it drive worker→reviewer→manager to `done` from the TUI.
- **Dependencies:** Phase 3 (a remote workspace must be creatable first).

## Testing strategy

- Per-phase unit/integration tests named in each phase's acceptance criteria: Phase 1 pins daemon-side worktree+argv+env resolution and the response shape (against a local repo, no cm-manager needed); Phase 2 pins repo resolve/clone/allowlist; Phase 3 pins the TUI local-vs-remote branch and remote-session sidebar rendering at the handler level; Phases 4–5 pin guard removal + remote routing at the handler level.
- Live e2e on cm-manager (mirrors the existing-session-binding validation): a `scripts/`-style operator-socket driver can exercise `create_session` directly (worktree created on the VM, session spawns, transcript appears), and a manual TUI pass validates interactive attach (Phase 4) and a remote workflow (Phase 5) — the parts that can't be unit-tested (live PTY rendering, keystrokes).
- Regression guard: the local session-create/attach/workflow paths are the load-bearing day-to-day flows; every phase keeps the local daemon + TUI suites green, and the local branch of each touched handler is asserted unchanged.

## Rollout / migration

Additive and gated. The new `create_session` RPC is additive; `start_session`/`mcp_start_session` are untouched. Remote behavior is reached only when `active_host != local`, so existing single-host users see no change. Repo availability needs a one-time operator step on cm-manager: set `repos_dir` (and an allowlist) in `daemon.toml` and redeploy the daemon (the same stop/cp/start flow already used). No on-disk state migration; no schema/data changes beyond the additive `daemon.toml` repos section (absent section = today's behavior).
