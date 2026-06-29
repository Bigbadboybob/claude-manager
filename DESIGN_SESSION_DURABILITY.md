# Design — Session durability (daemon-side persist + restore)

**Status:** design / slice plan for review (P0 in TODO.md). Not yet implemented.

## Problem

A `systemctl restart cm-daemon` (every deploy) SIGKILLs all session PTYs. Agent sessions then **vanish and don't come back** — bug-002 was lost this way. That violates the principle (memory `feedback_sessions_user_owned_lifecycle`): a session is a **durable, user-owned** thing — the orchestrator may drive it, but only the user marks it done/delete; a backend restart must never erase it.

## Current state (what the code actually does)

- **The daemon READS a manifest on startup but never WRITES one.** `lib.rs::run()` calls `load_manifest_from_disk(default_manifest_path())`, and `default_manifest_path()` is `~/.cm/tui-sessions.json` — *the TUI's* file. There is no production manifest-write path in the daemon (only test helpers).
- **On a headless host (cm-manager) nobody writes that file** — the TUI is the only writer and there's no TUI there. So the daemon starts "with empty state" and has zero record of its ad-hoc sessions. (Confirmed: no `tui-sessions.json` under cm-manager's `~/.cm/`.)
- **Startup loads metadata only — it never re-spawns agent processes.** Even where the manifest exists (laptop), `load_manifest_from_disk` populates `workspaces` + session *entries*; the live PTY/process is not recreated daemon-side.
- **Transcripts DO persist** on disk (`~/.claude/projects/<encoded>/*.jsonl`), so a conversation can be resumed (`claude --resume <id>` / codex resume).
- **Continuous tasks already survive** restarts via their own `~/.cm/continuous-tasks/<id>/state.json` + the scheduler's supervise/respawn — but they respawn **fresh** (new uid, new conversation; continuity is the `.bug-triage/` memory, not chat history).

Net: the daemon owns the sessions but keeps their registry **in memory only**, so its own restart erases them. The laptop is partly covered (TUI persists + restores on TUI startup), but a daemon restart *without* a TUI restart still kills local sessions too — so this is a universal daemon gap, worst on headless hosts.

## Goal

The daemon restores its own sessions across its own restart: each not-done session reappears **alive, with its conversation history**, at the same uid. Only a user-driven done/delete removes a session from the durable set. Deploys + crashes become transparent.

## Design

1. **Daemon-owned durable session registry.** The daemon persists its session set to its own file (proposal: `~/.cm/daemon-sessions.json`, distinct from the TUI's `tui-sessions.json` to avoid two-writer contention — see Open Questions). Written atomically on session create / status-change / exit. Each record carries enough to **re-spawn**: `uid`, `session_type`/engine, `workspace_id` → worktree path, `task_id`, `managed_by_uid`, `transcript_id` (for resume), `label`, `status`, memory-cap params, and the continuous-task tag if any.

2. **Restore on startup.** For each persisted **not-done** session, re-spawn it: launch the engine in its worktree with `--resume <transcript_id>` (so the conversation continues) plus the rebuilt MCP env (`mcp_config::build_env`), re-registering at the **same uid**. Rebuild argv from durable inputs rather than persisting a possibly-stale argv. Skip done/exited records.

3. **User-owned lifecycle.** A session leaves the durable registry only on `mark_subtask_done` / `kill_session` / explicit delete — never on restart. The daemon's reaper marks `status: exited` (kept, restorable until the user acts) vs `done` (removed).

## Slice plan

- **S1 — Persist. ✅ DONE (2026-06-29).** Daemon writes `daemon-sessions.json` on session create / exit / transcript-bind (atomic temp-file + rename). Round-trips the re-spawn fields. Write-only — no restore yet, so sessions still die on restart (S2 fixes that). *Verified by* 5 unit tests (build/round-trip/no-op + the real `start_session` and `set_transcript_path` RPC hooks). See "S1 — landed" below for what shipped and the decisions made during impl.
- **S2 — Restore (no-resume). ✅ DONE (2026-06-29).** On startup (`lib.rs::run`, before the accept loop + scheduler), `restore_sessions` reads `daemon-sessions.json` and re-spawns each in-scope session at its **same uid** with rebuilt argv/env (`build_args` + `build_env` + the persisted cap wrap) — a FRESH conversation (no `--resume`), isolating spawn/registry mechanics from resume. Scope: plain agent/bash sessions; continuous (scheduler-owned), workflow (poller-owned), and exited entries are skipped. Never blocks startup (per-session best-effort). *Verified by* 2 unit tests (scope filter + real end-to-end respawn-at-same-uid carrying identity, skipping the rest). See "S2 — landed" below.
- **S3 — Resume. ✅ DONE (2026-06-29).** The restore spawn now carries the resume key: claude `--resume <id>`, codex `resume` subcommand (mirrors the TUI's proven `claude_args`/`codex_args`). claude sets `transcript_path` explicitly (resume continues the same file); codex/fresh arm a detector to bind the new transcript. *Verified by* unit tests (build_args resume both engines + compose_restore_params resume/fresh) AND a real local end-to-end loop: spawn a session → SIGKILL the daemon → restart → the session reappears alive at its same uid (`scripts`-style operator-socket harness). The codeword-survives-restart check with a real claude is the final on-cm-manager verification. See "S3 — landed" below.
- **S4 — TUI coordination. ✅ DONE (2026-06-29).** Turned out to be **composition-complete**: the TUI's remote-reattach state machine (built earlier this session — `transport_eof` detection → mark reconnecting `⟳` → requeue → retry with a FRESH attach (new ticket) every `REMOTE_REATTACH_RETRY_INTERVAL` → rebind-in-place on success, settle to `exited` after the cap) already handles a remote daemon restart: the daemon dies with no `End` frame (it has no SIGTERM handler → abrupt exit → socket EOF), so the session is kept reconnecting, not exited; the render reads the slot fresh each frame so the swapped-in session unfreezes; and the phantom-dedup gate stops a double-spawn. The ONLY thing missing was that this auto-retry would *give up* after the ~30s cap if the daemon stayed down longer, with no manual override (the user's "A-r doesn't clear it"). So S4 makes **`A-r` a "reconnect now" lever** (`nudge_remote_reconnects`): it accelerates in-flight reconnects (clears the throttle + resets the attempt budget) and REVIVES remote sessions that gave up (clears `exited`, requeues) — the daemon may have restored them since. Verified by the existing reattach suite + a new `ar_nudge_*` test; TUI 656 green. The happy-path auto-reattach is the emergent behavior of this client machinery + the deployed daemon restore — no new code needed for it.
- **S5 — Continuous: resume too. ✅ DONE (2026-06-29).** Restore now re-spawns supervised-persistent continuous sessions (the orchestrator) resumed at their same uid, so the orchestrator keeps its conversation across a restart instead of the scheduler fresh-respawning it. The coordination is purely ordering + a registry check: restore runs BEFORE the scheduler starts and brings the session back ALIVE at `current_session_uid`, so the scheduler's `session_is_dead(uid)` is false → supervision skips the respawn (and `reconcile_orphans` skips it too, since it only orphans dead-session guards). `continuous_restorable` gates restore on exactly the scheduler's supervised set (Persistent + supervise + enabled + !paused); fresh/non-supervised/paused/missing stay scheduler-owned. Restore also clears any stale `in_flight` guard (restart-caught-mid-fire) so the scheduler isn't blocked. Verified by 3 unit tests (gate predicate + end-to-end continuous restore + in_flight clear). Unbounded context growth across restarts is accepted for now (cached input; autocompact = TODO P3). See "S5 — landed" below.

## Decisions (locked 2026-06-29)

1. **Separate `daemon-sessions.json`** — not the TUI's `tui-sessions.json`. The TUI already learns remote sessions via `list_sessions` polling, so it needn't read the daemon's file; this avoids two-writer contention.
2. **Resume continuous tasks too** (the orchestrator's conversation persists across restarts). Context-ballooning is acceptable (input is cached; autocompact comes later). Restore + scheduler must coordinate to avoid a double-spawn (see S5).
3. **Resume must NEVER block startup.** On any resume failure (bad transcript, trust dialog per `reference_headless_claude_trust_dialog`, etc.) fall back to a fresh spawn at the same uid; pre-trust the worktree. Startup always completes.

## Remaining impl questions (settle during slices, not blocking)

- **Re-spawn races.** Restore runs before the control listener accepts, so the TUI can't reattach mid-spawn; confirm ordering.
- **Argv reconstruction** vs persisting argv: rebuilding from engine+transcript+worktree+env avoids staleness but must match what `mcp_start_session` / `create_session` originally built. Mem-cap wrap is argv-level — re-apply on restore.
- **Scheduler hook** for S5: where the scheduler's dead-persistent respawn must yield to restore.

## Risks

- Resuming a wrong/garbage transcript could confuse an agent — guard with existence + validity checks, fall back to fresh on failure (never block startup).
- A restore that re-spawns a session whose task was already user-deleted → honor the done/delete tombstone strictly.
- Memory-cap inheritance must be re-applied on restore (the wrap was argv-level).

## S1 — landed (what shipped + decisions made during impl)

Files: `daemon/src/session.rs`, `daemon/src/state.rs`, `daemon/src/lib.rs`, `daemon/src/control/methods.rs`, `daemon/src/transcript_detect.rs`.

- **`DaemonSession::to_manifest_entry()`** (session.rs) projects a live session → `ManifestEntry`, deriving `transcript_id` from `transcript_path` via `transcript_id_from_path()` (file stem = the `--resume` uuid for both engines).
- **`DaemonState::build_daemon_manifest()`** (state.rs) rebuilds each workspace's `sessions` vec **from `state.sessions`** (the live registry — authoritative), NOT from `state.workspaces[].sessions` (only ever populated by a TUI-seeded manifest). Only workspaces owning a live session are emitted; worktree metadata is cloned from `state.workspaces` (that's the restore cwd).
- **`save_daemon_sessions()` / `persist_sessions_best_effort()` / `default_daemon_sessions_path()`** (state.rs) — atomic write (unique temp name per write → concurrent writers can't corrupt; rename is last-writer-wins, every write a full snapshot). Path lives on a new `DaemonState::daemon_sessions_path: Option<PathBuf>` field — `Some(default)` set in `lib.rs::run()`, `None` in tests so the suite never touches real `~/.cm/` (persist is a no-op when `None`; a focused test sets a tempdir).
- **Hooks** (best-effort, swallow+log on error): `start_session` post-insert (the single production spawn funnel — `mcp_start_session` + `create_session` both route through it), `handle_session_exit` post-remove, `set_transcript_path` (method) on-change, and `run_detector` (transcript_detect.rs) on bind.

**Key findings that shape S2/S3:**
- **Headless resume IS viable.** `mcp_start_session` arms a daemon-side transcript detector (`run_detector`/`spawn_queued_detector`) that writes `transcript_path` with **no TUI involved**. So ad-hoc subtask sessions on cm-manager (the bug-002 case) DO get a `transcript_id` persisted → S3 `--resume` works headless. The detector-path hook is what captures it.
- **`ManifestEntry` has NO mem-cap fields, and it's constructed in 29 literal sites** across daemon + tui. Adding fields breaks all 29. Mem-cap is a *restore-spawn* input (re-apply the cap), not a *persist* concern → **deferred to S2**, where the spawn rebuild needs it and the field-add + 29-site fix lands together. (Risks §"Memory-cap inheritance" tracks this.)
- **No periodic flush thread in S1 — deferred to S2.** A timer-flush would clobber `daemon-sessions.json` with empty state on every fresh restart (S1 has no restore, so the daemon boots empty and a flush would erase the file before anything reads it). Once S2's restore populates `state.sessions` at startup *before* any flush, a debounced periodic flush becomes safe and captures non-hooked mutations (workflow-context set, etc.). Until then, the lifecycle hooks are sufficient (and don't clobber).

## S2 — landed (what shipped + decisions made during impl)

Files: `daemon/src/manifest.rs`, `daemon/src/session.rs`, `daemon/src/state.rs`, `daemon/src/control/methods.rs`, `daemon/src/lib.rs`, `tui/src/app.rs`.

- **`ManifestEntry` gained the mem-cap triple** (`memory_cap_soft_bytes` / `memory_cap_hard_bytes` / `cgroup_prefix`, all `#[serde(default, skip…)]`); `to_manifest_entry()` carries them; 28 literal constructors across daemon + tui were updated (mechanical `: None`).
- **`state::read_daemon_sessions(path)`** — parse-don't-apply reader (distinct from `load_manifest_from_disk`, which would clobber live `state.workspaces`).
- **`methods::restore_sessions(&state)`** — reads the file, then for each in-scope entry calls `restore_one_session`, which **rebuilds the spawn** via `compose_daemon_spawn_params` (argv `build_args` + env `build_env`, deterministic from uid+engine) + re-applies the persisted cap wrap, layers on `global_perms`/`managed_by_uid`, and calls `start_session` at the **same uid**. `restore_in_scope` = `last_exit.is_none() && continuous_task_id.is_none() && workflow_run_id.is_none()`.
- **Wired in `lib.rs::run`** before the poller/scheduler + accept loop.

**Decisions made during impl:**
- **Rebuild argv, do NOT persist a recipe.** `build_args`/`build_env` are deterministic from `(uid, engine, mcp_server_path)`; `wrap_with_systemd_run`'s only non-determinism (the systemd unit nonce) is harmless on restart (old scope is gone → new scope name). Rebuild is actually *more* robust than a persisted recipe: a session restored after a deploy picks up the *current* socket/config layout, not a stale snapshot. So S1's `ManifestEntry` file shape was sufficient (just needed mem-cap fields).
- **Pre-trust is free.** `claude_trust::maybe_pretrust_for_spawn` runs inside `DaemonSession::spawn` itself, so restore (→ `start_session` → spawn) pre-trusts the worktree automatically (decision 3 / `reference_headless_claude_trust_dialog`).
- **Cap graceful-degrade.** Re-apply the cap only if its persisted cgroup prefix still `is_dir()`; else restore uncapped + log (never fail the spawn).
- **Scope deferrals:** continuous + workflow sessions are skipped (owned by the scheduler / poller); S5 makes the scheduler *resume* continuous sessions; workflow-participant restore is a later slice.
- **Periodic flush still deferred** — fold into S5/later once continuous-resume ordering is settled. The lifecycle hooks already keep the file converged for ad-hoc sessions.

## S3 — landed (what shipped + decisions made during impl)

Files: `daemon/src/mcp_config.rs`, `daemon/src/control/methods.rs`.

- **`mcp_config::build_args` gained a `resume_session_id: Option<&str>` param.** claude appends `--resume <id>` after `--mcp-config`; codex prepends the `resume` SUBCOMMAND (first arg) with the SESSION_ID as the trailing positional — mirrors the TUI's proven `claude_args`/`codex_args` exactly. Threaded through `compose_daemon_spawn_params` (every other caller passes `None`: `create_session`, `compose_continuous_spawn_params`, `mcp_start_session`, `resolve_workflow_spawn_program`).
- **`compose_restore_params`** (extracted from `restore_one_session`, pure/testable) passes `resume = e.transcript_id.as_deref()`. For claude it ALSO sets `transcript_path` explicitly (resume continues the SAME `<id>.jsonl` file → `ready` immediately + persist re-records the id). The mem-cap wrap stays OUTERMOST (applied after the resume argv).
- **`restore_one_session`** arms a transcript detector after `start_session` for codex (resume writes a NEW rollout) and for any fresh restore — snapshotting ids before the spawn. Skipped for claude-resume (path set explicitly; the pre-existing file would never bind via a new-file scan) and bash.

**Decisions made during impl:**
- **No `transcript_id` → fall back to fresh** (decision 3): a session that never bound a transcript restores as a new conversation rather than blocking.
- **Reversed the daemon's "TUI owns argv" stance for restore.** `daemon/src/mcp_config.rs`'s header notes slice 10c-e-3b deliberately removed argv resolution from the general `start_session` path. Restore is a NEW daemon-internal spawn caller (like `mcp_start_session`/`create_session`) that legitimately needs argv resolution, so adding resume to the daemon's `build_args` is consistent with that pattern, not a regression of it.
- **codex transcript freshness across restarts** is handled by the post-spawn detector (it binds codex's new rollout and the persist hook records it), so a codex session resumes the LATEST conversation on each successive restart — not the stale original.

## Deploy (when ready — needs user coordination)

S1+S2+S3 are a complete unit. Deploy them TOGETHER to cm-manager in ONE restart (a lone S1/S2 deploy would kill sessions with weaker/no continuity — net-negative). The restart KILLS cm-manager's live sessions; restore brings them back resumed — so it must be coordinated with the user (it briefly drops the orchestrator + bug sessions, then they reappear). Final on-target verification: the codeword-survives-restart check with a real claude session. Build: `cargo build --release -p cm-daemon` → scp → atomic swap + `systemctl restart cm-daemon` (per `reference_manager_vm` / CLAUDE.md). S4 (TUI reattach coordination + frozen-pane UX) and S5 (continuous resume) remain.

## S1 implementation notes (from mapping the daemon — historical; S1 now landed)

- `DaemonState::load_manifest_from_disk` (`daemon/src/state.rs`) loads only `manifest.workspaces` + `manifest.bindings` — **NOT** the live session registry. Live sessions are `state.sessions: HashMap<uid, DaemonSession>`, separate from `state.workspaces[].sessions` (the production spawn path does **not** appear to mirror sessions into the workspace entries — only test code pushes `ManifestEntry` there). So there is no ready snapshot serializer to reuse.
- **S1 = build a `Manifest` and write it.** Convert each `DaemonSession` in `state.sessions` → `ManifestEntry`, place it in its workspace (keyed by `workspace_id`) inside a clone of `state.workspaces`, attach `state.bindings`, and write atomically (temp-file + rename) to **`~/.cm/daemon-sessions.json`** (a NEW path — `state::default_manifest_path()` is `tui-sessions.json`; add a `default_daemon_sessions_path()`).
- Needs a `DaemonSession → ManifestEntry` conversion (build it; check `daemon/src/session.rs` for an existing helper first). **Confirm `ManifestEntry` carries every restore input:** `transcript_id` (for `--resume`), `session_type`/engine, `task_id`, `managed_by_uid`, mem-cap params, continuous tag. Worktree path comes from the workspace's `worktree_path`. Add fields to `ManifestEntry` if missing (it's `#[serde(default)]`-friendly, so additive is safe).
- **Hook the write** on session spawn (`start_session` / `mcp_start_session` / `create_session` in `daemon/src/control/methods.rs`), session exit (the reaper), and status changes. Debounce.
- Mirror `load_manifest_from_disk` with a `save_daemon_sessions(&self, path)` on `DaemonState` (atomic write).
- **Verify S1:** spawn a session on cm-manager, `systemctl restart cm-daemon`, confirm `~/.cm/daemon-sessions.json` exists with the session + its re-spawn fields. (S1 is write-only — no restore yet, so sessions still die; S2 adds the re-spawn.)
