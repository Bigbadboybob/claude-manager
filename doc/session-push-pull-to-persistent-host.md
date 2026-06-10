# Session push/pull to a persistent daemon host

## Summary

Add a real "move this session to the always-on host" capability: `A-p` commits the worktree, transfers the session transcript directly over the host's SSH connection, has the host daemon materialize the worktree and spawn `claude --resume`, then hands off (the local session tombstones; the host daemon owns the live PTY and the TUI observes/attaches it under that host). `A-l` is the reverse. This replaces the deprecated ephemeral GCS/cloud-worker push (`tui/src/backend.rs` `do_push`/`do_pull`) with a path that targets the persistent `cm-manager` daemon over the multi-host substrate that `doc/persistent-host-daemon.md` already built.

## Problem

Today `A-p` (`do_push`, `tui/src/backend.rs:427`) routes a session to an ephemeral cloud worker: it pushes a `cm/push-<id8>` branch, uploads the transcript to `gs://cm-sessions`, and creates an `is_cloud=true, project=null` API task that the dispatch daemon claims to launch a throwaway VM. The operator never attaches to that PTY — state shuttles through GCS, and the worker only re-uploads on preemption, so normal completion silently loses cloud-side work. There is no way to take a live local session and continue it on the persistent `cm-manager` daemon — the thing the operator actually wants when closing the laptop (e.g. before a flight). The multi-host substrate exists (`hosts.toml`, per-host `manifest.watch`, daemon-owned sessions, attach), but the migration step — get the transcript + worktree onto the host and have its daemon own a resumed PTY — is unbuilt. `doc/persistent-host-daemon.md` Phase 4 deletes the ephemeral path and explicitly leaves `A-p`/`A-l` "free for future use" (`doc/persistent-host-daemon.md:385`); this doc fills that gap. Verified the hard way on 2026-06-05: migrating a session to `cm-manager` required hand-running git clone + scp + `claude --resume` over SSH.

## Goals

- `A-p` on a local session migrates it to a chosen persistent host: after it completes, the session runs under that host's daemon, shows under the host in the TUI (`A-H`), is attachable with full history, and the local session is tombstoned (handoff).
- The transcript reaches the host current (no stale snapshot) via direct SSH transfer, not GCS.
- The host worktree is materialized from git (commit WIP → push branch → clone/checkout on host).
- `A-l` on a host-owned session brings it back to local with current history and tombstones the host session.
- The migration is atomic at the handoff boundary: the local session is only tombstoned after the host daemon confirms it owns the resumed session (mirrors the existing `PushComplete`-gated cleanup at `tui/src/app.rs:11095`).

## Non-goals

- Removing the ephemeral GCS/dispatch path and its API plumbing — that is `doc/persistent-host-daemon.md` Phase 4; this doc only stops routing `A-p`/`A-l` to it (the deletion is coordinated, not duplicated, here).
- Live two-way mirroring / keeping local and host copies in sync — explicitly handoff, not mirror (chosen to avoid two-writer transcript conflicts).
- rsync-style transfer of uncommitted working-tree state — worktree materialization is git-based; uncommitted changes are committed to the push branch first.
- TCP+TLS host transport — this targets `ssh-unix` hosts (what `cm-manager` is); the direct-SSH transfer rides the same SSH config.
- Workflow-run migration (moving an in-flight feedback workflow to a host) — single sessions only; workflow-on-host is the daemon-side-workflow doc's territory.

## Current state

- Ephemeral push/pull: `do_push` (`tui/src/backend.rs:427`) and `do_pull` (`tui/src/backend.rs:604`). `do_push` finds the latest transcript via `find_latest_session(worktree_path)` (`backend.rs:390`), commits + pushes `cm/push-<id8>`, `gcloud storage cp` to `gs://cm-sessions/<id>/`, and creates/updates an `is_cloud=true` API task. `do_pull` reverses it from GCS and emits `PullComplete` → `spawn_resumed_session` (`tui/src/app.rs:10685`). Triggered by `push_active`/`pull_active` (`app.rs:11042`/`11130`) bound to `A-p`/`A-l` (`app.rs:8700`).
- Transcript location: `~/.claude/projects/<encoded-cwd>/<session_id>.jsonl`, where `get_project_path(cwd)` replaces `/` and `.` with `-` (`backend.rs:383`). Subagent transcripts live under `<session_id>/subagents/`.
- Multi-host substrate (already merged, from `doc/persistent-host-daemon.md`): hosts declared in `~/.cm/hosts.toml` (`tui/src/hosts.rs`); `HostPool` dials each host's daemon over an `ssh -N -L <local>:<remote_socket>` tunnel with a per-spawn random socket path and reachability backoff (`tui/src/host_pool.rs:283`, `:950`); `A-H` switches active host; sidebar groups by host.
- Daemon session ownership: `start_session` (`daemon/src/control/methods.rs:410`, params struct at `:159`) execs a caller-supplied `argv` verbatim, auto-registers a workspace from `worktree_path` if unknown (`methods.rs:520`), accepts `transcript_path` for resume flows, registers the session in `DaemonState.sessions`, and broadcasts a `ManifestDiff` over `manifest.watch`.
- Resume-spawn analog: `try_spawn_via_daemon_with_deps` (`tui/src/app.rs:803`) threads `resume_session_id` into `mcp_config::build_args(SpawnTarget::Daemon, engine, uid, workflow_meta, resume_session_id)` (`app.rs:854`) which adds `--resume <id>`, then routes the spawn through `host_pool.for_host(host_id)` (`app.rs:927`). `spawn_restored_session` (`app.rs:4943`) is the restore-on-restart caller.
- Observe/attach: per-host `manifest.watch` consumer (`tui/src/manifest_watch.rs:214`, always-on per `:142`) feeds the TUI a host daemon's live sessions; `try_attach_via_daemon_with_deps` (`app.rs:616`) attaches to a session on a specific host via `host_pool.for_host(host_id)`.
- Metadata mirror: `push_worker` (`tui/src/push_worker.rs`) continuously pushes task-tree / tui-sessions / workflow-defs metadata to each host daemon — metadata only, no transcripts or PTYs (`TuiSessionRow` at `push_worker.rs:324`).
- `cm-manager` readiness (provisioned 2026-06-05, see [[project_pushpull_dedicated_vm]]): current `cm-daemon`, Claude+Codex auth, `github-pat` git creds (private clone works), `gsutil`. So the host can clone repos and spawn authenticated claude sessions today.

## Proposed design

A push is laptop-driven (the laptop holds the source transcript + repo) and ends in a host-daemon-owned resumed session. The flow:

```
  A-p (target host H)                          cm-daemon on host H
  ────────────────────                         ───────────────────
  1. git: commit WIP -> push cm/push-<id8>
  2. scp transcript (+subagents) ──────────►   lands at ~/.claude/projects/<encoded host wt>/<id>.jsonl
  3. RPC session.adopt(repo_url,branch,        ┌─ create worktree ~/.cm/worktrees/<slug> (clone+checkout)
       slug, session_id, type, label,    ───►  ├─ spawn claude --resume <id> (build_args + transcript_path)
       task_id, uid, env)                      ├─ register in DaemonState.sessions, bind workspace
                                               └─ broadcast ManifestDiff::SessionAdded  ──► manifest.watch
  4. on adopt OK: TUI attaches (observe) under host H, tombstones the LOCAL session (handoff)
```

### Host selection

`A-p` targets a persistent host, not the ephemeral pool. If exactly one non-local host is declared in `hosts.toml`, use it; if more than one, show a one-line host picker; if none, surface "no host configured" (and, transitionally, offer the old ephemeral push — see Phase 5). The chosen `HostId` threads through the whole flow so transfer, adopt, attach, and observe all route to the same daemon via `host_pool.for_host(host_id)`.

### Step 1 — git (reuse existing logic)

Reuse `do_push`'s git steps (`backend.rs:455-496`): `git checkout -b cm/push-<id8>`, `git add -A`, `git commit -m "WIP: <name>"`, `git push -u origin cm/push-<id8>`. This is the WIP-commit half of the chosen git-based worktree decision.

### Step 2 — direct-SSH transcript transfer

A new backend helper `transfer_transcript_to_host(host, worktree_slug, session_id)` computes the HOST-side transcript directory deterministically — `<host home>/.claude/projects/<encoded(host_worktree_path)>/`, where `host_worktree_path = /home/<ssh_user>/.cm/worktrees/<slug>` and `encoded` is the same `/`+`.`→`-` rule as `get_project_path` — then `scp`s `<id>.jsonl` and the `<id>/subagents/` dir to that path over the host's SSH config (`ssh_host` from `hosts.toml`). The encoded path must match the cwd the host session will run in, or `--resume` won't find the transcript; this is computed once and used by both transfer and adopt. `scp`/`rsync` runs against the `ssh_host` alias (the same alias the `HostPool` tunnel uses), not the forwarded socket.

### Step 3 — daemon `session.adopt` RPC

New Operator-only daemon method `session.adopt` (dispatch arm in `daemon/src/control/dispatch.rs`, impl in `daemon/src/control/methods.rs`). Params: `repo_url`, `branch`, `slug`, `session_id`, `session_type`, `label`, `task_id`, `uid`, `env`, `cols`, `rows`. It:

1. Materializes the worktree at `~/.cm/worktrees/<slug>` if absent, via the daemon's own `worktree::create_worktree` + `git fetch origin <branch>` + checkout (the daemon already owns worktrees on the host). Idempotent: if the worktree exists on the right branch, reuse it.
2. Builds the claude argv via `mcp_config::build_args(SpawnTarget::Daemon, engine, uid, None, Some(session_id))` so it execs `claude --resume <session_id> …` (same path `try_spawn_via_daemon` uses).
3. Spawns through the existing `start_session` internals with `worktree_path` set (auto-registers the workspace), `transcript_path` pointing at the transferred file, `task_id`, and `session_type` — registering the session in `DaemonState.sessions` and broadcasting `ManifestDiff::SessionAdded`.
4. Returns `{uid, workspace_id}`.

`session.adopt` is essentially "create-worktree + resume-spawn, server-side" — it reuses `start_session`'s registration/broadcast rather than inventing a parallel path.

### Step 4 — observe + handoff

On `adopt` OK, the TUI: (a) records the session under the host (it will also arrive via that host's `manifest.watch` diff) and attaches via `try_attach_via_daemon_with_deps(host_id=H, …)` for live observation; (b) tombstones the LOCAL session and clears the local workspace's live entry — gated on the adopt success exactly like `finish_push` gates cleanup on `PushComplete` today (`app.rs:11095`). If `adopt` fails, nothing is tombstoned and the error surfaces (the laptop keeps the authoritative session). The pushed branch + transferred transcript on the host are harmless leftovers on failure.

### Pull (reverse)

`A-l` on a host-owned session: daemon flushes/identifies the current transcript; the laptop `scp`s `<id>.jsonl` (+subagents) host→local into the local encoded project dir; ensures the local worktree exists (it usually still does, or `git fetch`+checkout the branch); spawns a local `claude --resume <id>` via `try_spawn_via_daemon(host_id=local, resume_session_id=…)` (the `spawn_resumed_session` path, `app.rs:10685`); then tells the host daemon to kill/tombstone its session. Same handoff atomicity in reverse: the host session is only torn down after the local resume is live.

### Alternatives considered

| Alternative | Why rejected |
|---|---|
| Keep using GCS as the transfer rendezvous | Stale-snapshot failure mode (host got a day-old transcript on 2026-06-05); extra hop + gcloud auth; the SSH connection to the host already exists. |
| rsync the working tree (incl. uncommitted) onto the host | No version anchor, can drift from git, heavier; user chose git-based materialization. |
| Mirror (keep local + host copies in sync) | Two-writer transcript conflicts (which `.jsonl` wins); user chose handoff. |
| Laptop drives `git clone` + spawn on the host over raw SSH | Bypasses the daemon's session registry, so the session wouldn't show in the TUI or be attachable (exactly the dead-end hit on 2026-06-05); the daemon must own the spawn. |
| Stream the transcript over a new daemon RPC instead of scp | Transport-agnostic but more to build; direct `scp` over the existing SSH config is the smallest correct step for ssh-unix hosts. |

## Risks and open questions

- Transcript-path encoding mismatch: if the host worktree cwd and the encoded transcript dir disagree, `--resume` silently starts a fresh session. Mitigation: compute the encoded host path once from the canonical `~/.cm/worktrees/<slug>` and reuse it for both transfer and adopt; Phase 2 acceptance asserts the resumed session loads the transferred history (turn count > 0).
- Host worktree base assumption: `host_worktree_path` assumes `/home/<ssh_user>/.cm/worktrees/<slug>`. If a host's home or worktree base differs, the path is wrong. Mitigation: have `session.adopt` return the actual worktree path and have the daemon expose its worktree base; transfer after adopt-prepare if needed. Open question: single `adopt` RPC (laptop computes path) vs. two-step (daemon returns path, then transfer) — resolve in Phase 2; the doc assumes single-RPC with a deterministic path and a daemon-returned confirmation.
- WIP commit semantics: `git add -A` + commit sweeps everything into the push branch (matches `do_push`). Large/binary/untracked junk could bloat the branch. Mitigation: same behavior as today's push; note it; a `.gitignore` audit is out of scope.
- Re-push / idempotency: pushing the same session twice, or a slug whose worktree already exists on the host. Mitigation: `adopt` is idempotent on the worktree and rejects a duplicate live `uid` (the daemon's collision guard, `methods.rs:554`); a re-push reuses the worktree and re-transfers the transcript.
- Handoff atomicity across a flaky tunnel: adopt succeeds but the attach/observe RPC fails. Mitigation: tombstone is gated on `adopt` success (the session is safely on the host); attach is best-effort and retried by the `manifest.watch` reconnect — a failed attach does not lose the session.
- Auth/creds drift on the host: relies on the host's `github-pat` (clone) + Claude auth staying valid. Mitigation: documented in [[project_pushpull_dedicated_vm]]; `adopt` surfaces a clear error if clone or spawn fails.
- Coexistence with the ephemeral path during Phase 5: until the old `do_push`/`do_pull` are removed, both could be reachable. Mitigation: rebind `A-p`/`A-l` to the host path in one change; keep the ephemeral functions only behind an explicit fallback until `persistent-host-daemon.md` Phase 4 deletes them.

## Implementation plan

### Phase 1: Direct-SSH transcript transfer primitive

- **Goal:** A backend helper transfers a session's transcript (+subagents) from the laptop to a host's correct on-disk path over the host's SSH config.
- **Scope:** New `transfer_transcript_to_host(host: &HostConfig, slug: &str, session_id: &str)` in `tui/src/backend.rs` (or a new `tui/src/host_transfer.rs`): compute `host_worktree_path` + `encoded` dir, `scp` `<id>.jsonl` and `<id>/subagents/` to `<ssh_host>:<host project dir>/`, creating the remote dir first (`ssh <host> mkdir -p`). Reuse `get_project_path` encoding (`backend.rs:383`). Read `ssh_host`/`ssh_user` from `tui/src/hosts.rs`.
- **Out of scope for this phase:** worktree materialization, spawn, keybind wiring.
- **Acceptance criteria:**
  - Given a local session transcript and a configured ssh-unix host, the helper places `<id>.jsonl` at exactly `<host home>/.claude/projects/<encoded(/home/<ssh_user>/.cm/worktrees/<slug>)>/<id>.jsonl` on the host (verified by SSH-listing the remote path), and the subagents dir alongside it.
  - The computed encoded path matches `get_project_path` for the same worktree path (unit test on the encoding helper).
  - Transfer failure (host unreachable / scp error) returns a typed error, transfers nothing partially observable as success.
  - `cargo build --workspace` green; encoding unit test passes.
- **Dependencies:** none

### Phase 2: Daemon `session.adopt` RPC

- **Goal:** A host daemon, given a repo/branch/slug and a pre-transferred transcript, materializes the worktree and spawns a registered, attachable `claude --resume` session.
- **Scope:** New `session.adopt` dispatch arm (`daemon/src/control/dispatch.rs`) + impl (`daemon/src/control/methods.rs`): `worktree::create_worktree` + fetch/checkout `<branch>` (idempotent), `mcp_config::build_args(SpawnTarget::Daemon, engine, uid, None, Some(session_id))`, then spawn via the existing `start_session` internals with `worktree_path` + `transcript_path` + `task_id` set; register in `DaemonState.sessions`; broadcast `ManifestDiff::SessionAdded`; return `{uid, workspace_id, worktree_path}`. Operator-only (`require_operator`).
- **Out of scope for this phase:** the laptop-side orchestration, keybinds, tombstone/handoff, pull.
- **Acceptance criteria:**
  - Calling `session.adopt` against a local daemon (transcript pre-placed by Phase 1's helper or a fixture) yields a session in `DaemonState.sessions` whose PTY is running `claude --resume <id>` in `~/.cm/worktrees/<slug>`, and `count_assistant_turns`/transcript read on it shows the transferred history (turn count > 0, not a fresh session).
  - The session is returned by `list_sessions` and a `ManifestDiff::SessionAdded` is observed on a `manifest.watch` subscription.
  - Calling `adopt` twice for the same slug reuses the worktree and the second call fails the `uid` collision guard rather than double-spawning.
  - `cargo build --workspace` green; an integration test exercises adopt → list → manifest diff.
- **Dependencies:** Phase 1 (for the end-to-end test fixture; the RPC itself can be unit-tested with a hand-placed transcript)

### Phase 3: TUI push-to-host orchestration + handoff

- **Goal:** `A-p` migrates a local session to a chosen host end-to-end, with atomic handoff.
- **Scope:** New `push_active_to_host` flow in `tui/src/app.rs` + a `BackendCommand::PushToHost` arm in `tui/src/backend.rs`: host selection (single non-local host, else picker), git commit+push (reuse `do_push:455-496`), Phase 1 transfer, Phase 2 `session.adopt` RPC via `host_pool.for_host(host_id)`, then on success `try_attach_via_daemon_with_deps(host_id=H,…)` for observation and tombstone the local session (reuse the `finish_push`/`PushComplete` gating at `app.rs:11095`). Rebind `A-p` (`app.rs:8700`) to this flow.
- **Out of scope for this phase:** pull (Phase 4), removing the ephemeral path (Phase 5).
- **Acceptance criteria:**
  - `A-p` on a local claude session migrates it to the `manager` host: after completion the session appears under the `manager` host on `A-H`, attaches with full prior history, and the local session is tombstoned; the local worktree's live entry is cleared.
  - On a forced `adopt` failure (e.g. host unreachable), the local session is NOT tombstoned and an error is surfaced.
  - Manual smoke checklist in the PR: push the bayes session to `manager`, quit the TUI, confirm over SSH the host PTY is alive, reattach, see history.
  - `cargo build --workspace` green; existing TUI integration tests pass.
- **Dependencies:** Phases 1, 2

### Phase 4: Pull-from-host (reverse handoff)

- **Goal:** `A-l` brings a host-owned session back to local with current history and tombstones the host session.
- **Scope:** `pull_active_from_host` in `tui/src/app.rs` + `BackendCommand::PullFromHost` in `tui/src/backend.rs`: identify the host session's current transcript, `scp` host→local into the local encoded project dir, ensure the local worktree (`git fetch`+checkout the branch if missing), spawn a local `claude --resume` via the `spawn_resumed_session` path (`app.rs:10685`, `host_id=local`), then RPC the host daemon to kill/tombstone its session. Rebind `A-l` (`app.rs:8704`).
- **Out of scope for this phase:** removing the ephemeral path (Phase 5).
- **Acceptance criteria:**
  - `A-l` on a `manager`-host session restores it locally: a local `claude --resume` session appears with the host's latest history, and the host session is tombstoned (gone from the host's `manifest.watch`).
  - On a forced local-resume failure, the host session is NOT torn down.
  - `cargo build --workspace` green; integration test for the host→local transcript transfer + local resume.
- **Dependencies:** Phases 1, 2, 3

### Phase 5: Make host push/pull the default; retire ephemeral routing

- **Goal:** `A-p`/`A-l` mean host push/pull; the ephemeral GCS/dispatch path is no longer the default route.
- **Scope:** Point `A-p`/`A-l` solely at the host flows; remove or gate-behind-explicit-fallback the `BackendCommand::Push`/`Pull` ephemeral arms (`backend.rs` `do_push`/`do_pull`) and the `is_cloud` task creation in the push path. Update `CLAUDE.md` and status-bar hints. Coordinate the actual file deletions with `doc/persistent-host-daemon.md` Phase 4 (don't duplicate the API/worker/dispatch teardown here).
- **Out of scope for this phase:** deleting `api/dispatch_daemon.py`, `dispatch/`, `worker/startup.sh` (that's persistent-host-daemon.md Phase 4).
- **Acceptance criteria:**
  - `A-p`/`A-l` invoke only the host flows; `git grep` shows no live `A-p`/`A-l` → ephemeral `do_push`/`do_pull` routing (the functions may still exist pending Phase 4 deletion, but are unbound/behind a flag).
  - CLAUDE.md and the status bar describe `A-p`/`A-l` as host push/pull.
  - All TUI integration tests pass.
- **Dependencies:** Phases 3, 4

## Testing strategy

- Unit (laptop): the transcript-path encoding helper (`get_project_path` parity) and host-path computation (Phase 1).
- Integration (daemon): `session.adopt` against a local daemon with a fixture transcript — assert a registered, resumed session whose transcript read shows transferred history, plus the `manifest.watch` diff and the `uid` collision guard (Phase 2). Host→local transfer + local resume (Phase 4).
- Integration (TUI): push-to-host happy path with a local daemon standing in for the host (loopback `ssh-unix`), asserting host-owned session + local tombstone; and the adopt-failure-no-tombstone case (Phase 3).
- Manual / e2e: the real `cm-manager` round-trip — `A-p` the bayes session, quit TUI, verify the host PTY over SSH, reattach and see history; then `A-l` it back. PTY rendering and SSH timing are validated here (the render layer isn't unit-testable, per `doc/persistent-host-daemon.md:404`). Written checklist in each phase's PR.

## Rollout / migration

Phases 1–2 add capability with no behavior change (new helper + new RPC, unwired). Phase 3 rebinds `A-p` to the host flow but leaves the ephemeral `do_push`/`do_pull` functions intact, so rollback is reverting the keybind. Phase 4 adds the reverse. Phase 5 makes the host path the only `A-p`/`A-l` route but defers the actual ephemeral-code deletion to `persistent-host-daemon.md` Phase 4, so the cloud fallback remains revert-able until the operator has run on the host model long enough to be confident. No on-disk schema changes: the host transcript/worktree layout is identical to local; the only new persisted state is ordinary manifest entries (now carrying `host_id` for the migrated session, already an additive field). No database migration.
