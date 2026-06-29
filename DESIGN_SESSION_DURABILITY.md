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

- **S1 — Persist.** Daemon writes `daemon-sessions.json` on session create/exit/status-change (atomic write, debounced). Round-trips the re-spawn fields. No behavior change yet (write-only). *Verify:* kill the daemon, inspect the file has the live sessions.
- **S2 — Restore (no-resume).** On startup, re-spawn persisted not-done sessions in their worktrees with rebuilt argv/env, same uid — but a FRESH conversation first (no `--resume`), to isolate the spawn/registry mechanics from resume correctness. *Verify:* restart daemon → sessions reappear alive at the same uid; TUI reattaches.
- **S3 — Resume.** Add `--resume <transcript_id>` (claude) / codex resume to the restore spawn so history carries over. *Verify:* a codeword held only in the pre-restart conversation survives the restart.
- **S4 — TUI coordination.** Ensure the TUI's restore/reattach path binds to the daemon-respawned session instead of double-spawning; reconcile with the deferred-remote-reattach flow. Resolve the **frozen-pane UX** (P1) in the same pass: during the restart window the TUI should show "restoring", then rebind.
- **S5 — Continuous unification (decision).** Either leave continuous tasks to the scheduler (skip them in restore) OR let restore resume them too (orchestrator keeps its conversation across restarts instead of respawning fresh). See Open Questions.

## Open questions / decisions

- **Two writers.** Separate `daemon-sessions.json` (clean, but the TUI must learn to read it for remote hosts) vs. the daemon writing `tui-sessions.json` only when headless (no `tui.sock`). Lean: separate file; the TUI already polls `list_sessions` for remote hosts, so it doesn't need to read the daemon's file.
- **Continuous orchestrator.** Today it respawns fresh via the scheduler. Resuming it on restart (S5) would preserve its conversation — arguably better — but must not double-spawn with the scheduler's respawn. Decide whether continuous = scheduler-owned (skip in restore) or restore-owned (resume).
- **Resume fidelity.** `claude --resume` resumes by session id; confirm the daemon's `transcript_id` maps to it, and that the resumed process re-enters cleanly (no plan-mode / trust-dialog snag — see `reference_headless_claude_trust_dialog`). Codex resume is a separate mechanism.
- **Re-spawn races.** Restore runs before the control listener accepts, so the TUI can't reattach mid-spawn; confirm ordering.
- **Argv reconstruction** vs persisting argv: rebuilding from engine+transcript+worktree+env avoids staleness but must match what `mcp_start_session` / `create_session` originally built.

## Risks

- Resuming a wrong/garbage transcript could confuse an agent — guard with existence + validity checks, fall back to fresh on failure (never block startup).
- A restore that re-spawns a session whose task was already user-deleted → honor the done/delete tombstone strictly.
- Memory-cap inheritance must be re-applied on restore (the wrap was argv-level).
