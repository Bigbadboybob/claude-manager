# Remote session execution from the TUI

## Summary

Run the TUI's `A-n` / `A-s` / `A-a` / `A-f` flows against a remote daemon host (cm-manager), not just `local`. The remote daemon resolves its own paths — it creates the worktree, builds argv/env, and writes the per-session MCP config on its own filesystem — so the TUI sends a high-level request instead of laptop-local paths. A repo-availability layer lets the remote daemon materialize any repo, and interactive attach rides the existing ssh-unix tunnel.

## Problem

Every interactive session op is gated to the local host by `guard_local_host_only` (tui/src/app.rs:562): `A-n`, `A-s`, `A-a`, `A-f`, and MCP `start_session` error on any non-local host. The gate exists because the TUI resolves everything locally and ships it to the daemon — the workspace's local-filesystem worktree path, a per-session MCP config under `~/.cm/mcp/<uid>/`, and argv/env carrying the laptop's `CM_DAEMON_SOCKET`/`CM_TUI_SOCKET` — none of which are valid on a remote daemon's filesystem. So the only way to run work on the always-on GCP box is to drive its daemon directly over the control socket (operator/MCP/headless workflows), never through the normal TUI UX.

## Goals

- `A-n` on a remote host creates a fresh git worktree on that host and spawns its first session; `A-s` on a remote-hosted workspace adds another session in that existing remote worktree. All paths resolved daemon-side.
- `A-a` attaches interactively to a remote session — live PTY output and keystroke input over the ssh-unix tunnel, with working resize and kitty-keyboard encoding.
- The remote daemon can materialize any repo it doesn't already have (registry + clone-on-demand), not just `~/code/projects/<name>`.
- `A-f` launches a feedback workflow on a remote-hosted workspace via the existing daemon-side workflow execution.
- Remote sessions render in the host-grouped TUI sidebar.
- The local path is unchanged: local `A-n` / `A-s` / `A-a` / `A-f` behave exactly as today.

## Non-goals

- Cloud push/pull via GCS (`A-p`/`A-l`, the `worker/` dispatch path). A separate, older mechanism; untouched here.
- TLS transport for hosts (the `TcpTls` placeholder in hosts.rs). Only the existing `ssh-unix` transport is in scope.
- Routing the LOCAL create path through the new RPCs. Local keeps its current TUI-side resolution.
- Multi-host fan-out (one logical task spanning hosts simultaneously). One session targets one host.
- Secrets/credentials provisioning on the VM. cm-manager is assumed already authenticated for claude + codex.
- Remote `in_place` (A-n working in the repo's main checkout — `Workspace::is_in_place()`, app.rs:10004) and remote `seed_from` (A-n/A-s resuming an agent-memory snapshot via `clone_snapshot_for_spawn`, app.rs:10024/10405). Both need cross-host machinery out of scope here — spawning in the repo root on the remote, and materializing + resuming a snapshot on the remote host. A remote `A-n` with `in_place`/`seed_from`, or a remote `A-s` with `seed_from`, is rejected by the TUI (see TUI routing), never silently downgraded. Local `A-n`/`A-s` keep both.

## Current state

Two daemon spawn contracts exist:

- Low-level `start_session` (daemon/src/control/methods.rs `StartSessionParams` ~183): the caller supplies full `argv`/`env`/`working_dir` and the daemon just execs. The TUI's local `A-n` uses it — `create_local_session` (app.rs:9946) calls `worktree::create_worktree` (daemon/src/worktree.rs:84), then `mcp_config::build_args`/`build_env` (tui/src/mcp_config.rs:299/:129) to build argv+env with local paths and a locally-written `~/.cm/mcp/<uid>/claude.json`, and ships it via `rpc_start_session_full` (tui/src/client_session.rs:606).
- Daemon-resolved spawns — `mcp_start_session` (methods.rs ~5772) and `start_workflow` participants (methods.rs ~3127) — call the DAEMON's `mcp_config::build_args`/`build_env` (daemon/src/mcp_config.rs:312/:94), which resolve the daemon-local MCP server (`resolve_server_path` :135, from `daemon.toml`) and venv (`resolve_python_interpreter` :156), inject the daemon's own sockets (methods.rs ~668), and write the MCP config under the daemon's own `~/.cm/mcp/`.

So the daemon can already build correct argv/env for its own host; the interactive create path just never asks it to. The daemon-resolved paths don't create a worktree (`mcp_start_session` spawns in an existing workspace; `start_workflow` takes a `worktree` from the caller), though `create_worktree`/`create_subtask_worktree` (worktree.rs:84/:179) exist as library functions invoked TUI-side today.

Attach is host-agnostic: `try_attach_via_daemon_with_deps` (app.rs:615) takes a `host_id`, dials `host_pool.for_host(host_id)`, and runs `session.attach` → `attach.open` → stream. The ssh-unix tunnel (`ssh -fN -L`, tui/src/host_pool.rs) is a transparent stream forward — PTY-byte, input, and `Resize` frames (daemon/src/control/stream.rs ~670) all ride it, and kitty-keyboard is set at `Term` construction (client_session.rs ~409).

Repo availability is the remaining gap: `DaemonConfig` (daemon/src/config.rs ~104) has no repos field, and `find_local_repo` (daemon/src/worktree.rs:365) only checks `~/code/projects/<name>` + cwd with no clone fallback.

## Proposed design

### Host is a workspace property

A workspace is one git worktree — one directory on one host's filesystem — so all its sessions run on that host; there is no cross-host-within-a-workspace case (a subtask gets its own worktree, i.e. its own workspace). Host lives on each `TerminalSession.host_id`. `A-H` / `active_host` is only "the host the next NEW workspace is created on"; it never relocates existing work. So `A-n` (new workspace) reads `active_host`, while `A-f` / `A-s` / `A-a` (existing workspace/session) read the entity's `host_id` and dial `host_pool.for_host`.

### Two daemon RPCs: `create_session` (A-n) and `add_session` (A-s)

Both are Operator-only (gated by `require_operator`, like `start_session` at dispatch.rs:490): the TUI is an Operator caller, and these accept an explicit `workspace_id` (and, for create, `repo_url`/`slug`), which the Session-callable `mcp_start_session` (methods.rs:5428) refuses — it derives workspace/task from the caller and enforces descendant/task auth. Agents keep using `mcp_start_session`.

Both compute `argv`/`env`/`working_dir` daemon-side via `mcp_config::build_args`/`build_env` (so the MCP config, sockets, and venv are the daemon's own) and return identity only — not a live PTY handle. The TUI follows each with an attach (see TUI routing). Neither accepts the host-local memory-cap triple (`cgroup_prefix` is meaningless off-host, app.rs:888); remote sessions spawn uncapped, with daemon-side cap resolution (via the participant resolver, methods.rs:6046) a later addition. Neither accepts `in_place`/`seed_from` (Non-goals). A shared spawn core — `SpawnParams` build + register + manifest broadcast — backs `start_session`/`mcp_start_session`/`create_session`/`add_session`.

`create_session` (A-n — new workspace, creates a worktree):

- Params: `uid`, `workspace_id`, `label`, `engine`, `repo_url`, `start_branch: Option<String>`, `slug`, `task_id: Option<String>`, `cols`, `rows`.
- Resolve `repo_url` → local path (`find_local_repo`, plus the registry/clone layer below) → `create_worktree(&repo, &slug, start_branch)` → daemon `build_args`/`build_env` → shared core → register the workspace (auto-register on unknown `workspace_id`, as `start_session` does at methods.rs ~516).
- On `git worktree add` failure, clean up the partial worktree and return an error (no orphan).
- Returns `{ session_uid, worktree_path, workspace_id, transcript_path? }`.

`add_session` (A-s — existing workspace, reuses its worktree):

- Params: `uid`, `workspace_id`, `label`, `engine`, `task_id: Option<String>`, `cols`, `rows`. No `repo_url`/`slug`/`start_branch`.
- Look up `workspace_id` in daemon state → its existing `worktree_path` (`NotFound` if unknown to this daemon) → daemon `build_args`/`build_env` → shared core, spawning in that same worktree. Never calls `create_worktree`.
- Returns `{ session_uid, worktree_path, transcript_path? }`.

```
A-n (remote):
  TUI ── rpc_create_session { uid, label, engine, repo_url, start_branch, slug, task_id?, cols, rows }
      └─ host_pool.for_host ── ssh-unix tunnel ──▶ cm-daemon
            resolve repo → create_worktree → build_args/build_env → spawn (shared core) → register + broadcast
            ◀── { session_uid, worktree_path, workspace_id, transcript_path? }
  TUI ── try_attach_via_daemon_with_deps(session_uid, host) ──▶ session.attach / attach.open / stream ──▶ live PTY

A-s (remote): same shape via rpc_add_session { uid, label, engine, workspace_id, task_id?, cols, rows } —
              the daemon looks up workspace_id's worktree; no repo resolve, no create_worktree.
```

### Repo availability (registry + clone-on-demand)

`DaemonConfig` (daemon/src/config.rs ~104) gains a repos section: `repos_dir` (where clones live) and an optional allowlist (`[[repo]] name, url`). Resolution: `find_local_repo` first; on a miss, if the URL is permitted (allowlist or `allow_clone = true`), `git clone <url> <repos_dir>/<name>` then resolve; otherwise `NotFound` naming the repo. Cloning arbitrary URLs runs code-fetch on the VM, so it defaults to an allowlist, with open cloning behind an explicit `allow_clone` flag.

### TUI routing

`create_local_session` (A-n, app.rs:9946) reads `active_host`; `spawn_session_on_workspace` (A-s, app.rs:10314) reads the target workspace's `host_id`. For a local host the existing TUI-side path runs unchanged. For a remote host:

- Build the high-level request (no local worktree, MCP config, or argv/env) and call `rpc_create_session` / `rpc_add_session` (tui/src/client_session.rs) over `host_pool.for_host`, then immediately call `try_attach_via_daemon_with_deps` (app.rs:615) with the returned `session_uid` to open the PTY stream and build the `TerminalSession` (pinned to that host).
- Reject `in_place`/`seed_from` before the RPC: a remote `A-n` with `in_place`/`seed_from`, or a remote `A-s` with `seed_from`, sets a clear status message and issues no RPC.
- The create-site `guard_local_host_only` is replaced by this branch.

Remote sessions reach the sidebar through the existing per-host `manifest.watch` consumers (manifest_watch.rs:205). Two pieces are added: `ManifestEvent::Diff` carries the source host, and the adoption path (app.rs:7964, currently hardcoded `HostId::local()` and workflow-participant-only) tags adopted rows with the right `ts.host_id` and accepts non-workflow daemon-created sessions.

### Attach and reattach

Both the create/add flow and standalone reattach go through `try_attach_via_daemon_with_deps` (app.rs:615) with the session's `host_id`; the tunnel carries the stream unchanged (`session.attach`/`attach.open` client_session.rs ~715/:747, resize, kitty-mode). Reattach (a sidebar row from a prior create, or after a TUI restart) passes `ts.host_id` rather than being gated to local. (`attach_active`, app.rs:10192, is unrelated — it spawns a new session for an empty local workspace.)

### Remote workflows (`A-f`)

`A-f` (`launch_workflow_via_daemon`, app.rs:13125) resolves its host from the focused workspace's session host and routes `start_workflow` to that host. For a remote-hosted workspace the launch goes to the remote daemon over the tunnel against the workspace's remote worktree; participants spawn there and the poller drives them, with the TUI observing via the workflow event/manifest streams. (`start_workflow` is already daemon-driven.)

### Alternatives considered

| Alternative | Why rejected |
|---|---|
| TUI path-maps local→remote paths before sending | Must mirror `~/.cm/mcp`, sockets, venv, and repo layout across hosts; the daemon already resolves its own paths, so it should. |
| Shared filesystem (NFS) for valid remote paths | Heavy, fragile ops dependency; defeats the point of an independent always-on host. |
| Add a "resolve daemon-side" flag to `start_session` | Overloads a method whose contract is "exec this exact argv"; distinct RPCs keep the contracts legible and still share the spawn core. |
| One worktree-creating RPC for both A-n and A-s | A-s adds to an existing workspace and must reuse its worktree; a creating RPC would fork a second branch/worktree, changing A-s semantics. |

## Risks and open questions

- Clone-on-demand fetches caller-named URLs on the VM. Mitigation: allowlist by default; open cloning behind `allow_clone`; never auto-run repo setup scripts on clone.
- Memory caps are host-local (`cgroup_prefix`, app.rs:888), so the local cap triple can't cross hosts. Mitigation: the RPCs omit it; remote sessions spawn uncapped until daemon-side cap resolution (methods.rs:6046) is added.
- The attach stream can stall if the tunnel respawns mid-stream. Mitigation: `host_pool` respawns dead tunnels; validate that attach survives a bounce or surfaces a clean reattach.
- A partial `create_session` (repo resolved, `git worktree add` fails) could orphan a branch/dir. Mitigation: clean up on failure and return a typed error.
- Open question: clones live under `repos_dir` — `~/code/projects` (matching `find_local_repo`) or a dedicated `~/.cm/repos`? Default chosen in Phase 2, confirmed against the VM layout.

## Implementation plan

### Phase 1: Daemon `create_session` + `add_session` RPCs [feedback]

- **Goal:** two Operator-only daemon RPCs spawn a session with daemon-resolved argv/env/MCP-config and return its identity — `create_session` creating a worktree, `add_session` reusing an existing workspace's worktree. Testable against the local daemon.
- **Scope:** daemon/src/control/methods.rs (the two methods + params structs; a shared spawn core factored out of `start_session`), daemon/src/control/dispatch.rs (both arms `require_operator`), daemon/src/worktree.rs `create_worktree` (create only), daemon/src/mcp_config.rs `build_args`/`build_env` (both). Remote sessions spawn uncapped.
- **Out of scope:** repo clone-on-demand (Phase 2), TUI wiring (Phase 3), daemon-side caps, `in_place`/`seed_from`.
- **Acceptance criteria:**
  - `create_session` with `{uid, workspace_id, label, engine=claude-code, repo_url, slug}` for a `find_local_repo`-resolvable repo creates `~/.cm/worktrees/<repo>-<slug>` on `cm/<slug>`, and spawns a session whose argv comes from the daemon's `build_args` and whose env carries the DAEMON's sockets + a daemon-written `~/.cm/mcp/<uid>/claude.json`.
  - `add_session` with `{uid, workspace_id, label, engine}` for a daemon-known workspace does NOT call `create_worktree`, spawns into that workspace's existing `worktree_path` (not a new branch/dir), and returns it; an unknown `workspace_id` returns `NotFound`.
  - Both RPCs are Operator-only — a Session caller gets `Unauthorized`.
  - The response carries the identity and the session is registered with a `ManifestDiff::Added` broadcast.
  - An unresolvable `repo_url` returns a typed error naming the repo; a `git worktree add` failure cleans up (no orphan session).
  - `start_session` / `mcp_start_session` behavior is unchanged (existing tests green).
- **Dependencies:** none.

### Phase 2: Daemon repo availability [feedback]

- **Goal:** the daemon resolves any repo — local fast-path, else clone a permitted URL — so `create_session` works for repos not pre-present.
- **Scope:** daemon/src/config.rs (`repos_dir` + allowlist / `allow_clone`), daemon/src/worktree.rs (a resolver wrapping `find_local_repo` with a clone fallback), wired into `create_session`.
- **Out of scope:** TUI wiring; credentials for private repos (assume host git auth).
- **Acceptance criteria:**
  - An allowlisted URL absent from disk clones into `repos_dir/<name>` then resolves; a second resolve reuses it (no re-clone).
  - A non-allowlisted URL with `allow_clone=false` returns `Unauthorized`/`NotFound` without cloning.
  - No repos section in `daemon.toml` → today's behavior (`find_local_repo` only).
- **Dependencies:** Phase 1.

### Phase 3: TUI remote create/add + attach + sidebar adoption [feedback]

- **Goal:** `A-n` on a remote host, and `A-s` on a remote-hosted workspace, create/add a session on that host; the TUI attaches and the session renders in the host-grouped sidebar.
- **Scope:** tui/src/app.rs (`create_local_session` → `rpc_create_session`, `spawn_session_on_workspace` → `rpc_add_session` on a remote host, each followed by `try_attach_via_daemon_with_deps`; reject `in_place`/`seed_from`; remove the create-site `guard_local_host_only`), tui/src/client_session.rs (`rpc_create_session` + `rpc_add_session`), `ManifestEvent` source-host attribution + the adoption path (app.rs:7964, manifest_watch.rs).
- **Out of scope:** reattach (Phase 4), workflows (Phase 5), local-path unification, daemon-side caps, `in_place`/`seed_from` support.
- **Acceptance criteria:**
  - Remote `A-n` builds a `create_session` request (no local argv/env/working_dir/MCP path, no `cgroup_prefix`) over the remote socket, then attaches; local `A-n` runs the existing path unchanged.
  - Remote `A-s` builds an `add_session` request (existing `workspace_id`, no `repo_url`/`slug`/`start_branch`) over the remote socket; local `A-s` unchanged.
  - A remote `A-n` with `in_place`/`seed_from`, or a remote `A-s` with `seed_from`, is rejected with a status message and issues no RPC; the same options on a local host still work.
  - After create+attach the `TerminalSession` holds a live `Session` with `host_id = remote`.
  - A diff carrying a remote source host adopts a sidebar row with `ts.host_id = remote`.
  - Existing local A-n/A-s tests pass.
- **Dependencies:** Phase 1 (full "any repo" also needs Phase 2).

### Phase 4: Reattach to a remote session [feedback]

- **Goal:** reattaching to an existing remote session (a sidebar row, or after a TUI restart) streams its PTY live over the tunnel — output, input, resize, kitty-mode.
- **Scope:** tui/src/app.rs — the reattach entry point passes the session's `ts.host_id` into `try_attach_via_daemon_with_deps` and is not gated to local.
- **Out of scope:** workflows (Phase 5).
- **Acceptance criteria:**
  - Reattaching a session with a remote `ts.host_id` routes the attach RPCs through that host's socket, ungated.
  - e2e on cm-manager: reattach a remote row, run a command, resize and confirm reflow, confirm Enter submits; reattach survives a tunnel respawn.
  - Local reattach unchanged.
- **Dependencies:** Phase 3.

### Phase 5: Remote workflows (`A-f`) [feedback]

- **Goal:** `A-f` on a remote-hosted workspace launches a feedback workflow on that host.
- **Scope:** tui/src/app.rs (the `A-f` launch path; lift `guard_local_host_only` for a remote workspace host so `start_workflow` routes to that host's socket).
- **Out of scope:** anything beyond routing the already-daemon-driven workflow to the remote host.
- **Acceptance criteria:**
  - `A-f` on a remote-hosted workspace sends `start_workflow` to that host's socket against the remote workspace; local `A-f` unchanged.
  - e2e on cm-manager: create a remote worktree, `A-f` a feedback workflow on it, watch it drive worker→reviewer→manager to `done` from the TUI.
- **Dependencies:** Phase 3.

## Testing strategy

- Unit/integration tests per phase as listed above: Phase 1 pins daemon-side worktree+argv+env resolution and the response shapes against a local repo (no cm-manager needed); Phase 2 pins repo resolve/clone/allowlist; Phase 3 pins the TUI local-vs-remote branch, option rejection, and sidebar adoption at the handler level; Phases 4–5 pin remote routing.
- Live e2e on cm-manager: an operator-socket driver exercises `create_session`/`add_session` directly (worktree created/reused, session spawns, transcript appears); a manual TUI pass covers interactive attach (Phase 4) and a remote workflow (Phase 5).
- Every phase keeps the local daemon + TUI suites green, with the local branch of each touched handler asserted unchanged.

## Rollout / migration

Additive and gated. `create_session`/`add_session` are new; `start_session`/`mcp_start_session` are untouched. Remote behavior is reached only for a non-local host, so single-host users see no change. Enabling "any repo" on cm-manager is a one-time operator step: set `repos_dir` (and an allowlist) in `daemon.toml` and redeploy. No data migration; the `daemon.toml` repos section is optional (absent = today's behavior).
