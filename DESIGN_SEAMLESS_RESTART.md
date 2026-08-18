# Seamless daemon restart (in-place re-exec) — design proposal

Branch: `cm/fix-cm` (design; implementation phases land on their own branches)

## Why this exists (incident record)

Every daemon deploy today kills every hosted session, and the restart path has been the single largest source of production incidents in this project:

- **Stale resume** — the resume key froze at spawn, so restarts silently resumed month-old conversations (fixed by the per-turn re-stamp, `7b5d24e`; bit again anyway on 2026-08-17 for sessions whose stamps predated the fix's deployment).
- **Silent MCP preflight failure** — a restart served broken sessions for ~45 minutes with no signal (fixed in `7b5d24e`).
- **Venv-less worktree** — a restart from the wrong cwd made every spawned session invisible (`15ab84f`).
- **Env leak, 2026-08-18** — a restart armed from inside a hosted session bequeathed that session's `CLAUDE_CODE_SESSION_ID` to the daemon and thence to every respawned child; transcripts stopped persisting, every programmatic observer served a frozen snapshot for ~10 hours, and watcher agents spawned takeover sessions on false "dead lane" evidence (scrub fixed in `aad547d`).
- **Unresumable fresh spawns** — sessions killed by a restart before their first transcript detection had no resume key at all and came back as amnesiacs (fixed by spawn-time session-id pinning, hardening H1).
- **Inert config** — a config-only change (`mcp_server_path`) required a full session-killing restart to take effect, which is what set the 2026-08-18 cascade in motion (fixed by hot-reload, hardening H2).

The structural observation behind all of this: **the code that churns is brain-level** (auth, monitors, manifest, workflows, config, policy) **while the PTY layer it shares a process with is stable and owns the durable things** (child processes, master FDs). Our only deploy primitive kills the durable thing to swap the changing thing. This doc removes that coupling.

## Goal

A `daemon.restart` operator RPC that replaces the daemon's code by `exec()`ing the new binary **in the same process**. Hosted sessions are never signaled and never resumed — they simply keep running, mid-turn included. PTY bytes produced during the swap wait in the kernel buffer. The control plane (socket RPCs, attach streams) blips for well under a second and self-heals. A bad new binary is caught **before** the exec (old daemon keeps running), and a failed rehydrate rolls back to the old binary without ever touching the children.

## Out of scope

- **Holder/brain process split** (a tiny stable PTY-holder process + a freely restartable logic daemon). The architecturally pure version; deliberately deferred — see "Why re-exec instead of a process split".
- **Machine reboots.** Children die with the machine; the existing startup-restore + resume path (hardened by H1 pinning) remains the answer.
- **TUI redeploys.** The TUI is stateless-ish and user-facing; restarting it is cheap and stays as-is (minus its current dependence on also killing the daemon).
- **Changing a *running* session's spawn-time properties** (env, hook wiring). That inherently needs that one session respawned (`A-R`), never the fleet.

## Proven base (phase 1 — done, 2026-08-18)

A sandbox proof (`scripts/reexec_proof.py`, appendix) validated the four load-bearing OS assumptions in one run:

1. a forked child survives the parent's `exec()` and remains parented to the same PID;
2. a `pidfd` is re-acquirable from the stored numeric pid after exec;
3. PTY output written by the child and **left unread across the exec** is intact in the kernel buffer afterwards — bytes produced during the swap are buffered, not lost;
4. the inherited master FD (CLOEXEC cleared) keeps flowing normally in the new program image.

## Mechanism

### The restart sequence

`daemon.restart` (Operator-only; params: `binary_path` defaulting to the shared-target release build) runs:

1. **Preflight, old image still in charge.** Run `<new-binary> --verify-handoff <dry-manifest>` as a subprocess: the new binary parses the manifest format + current state files (`daemon-sessions.json`, workflow runs, continuous state), checks the state schema version, runs the MCP preflight, exits 0/1. Any failure → the RPC returns an error and **nothing happened** — the old daemon keeps serving. This is the property today's restart fundamentally lacks: a bad deploy leaves you with the old daemon, not with nothing.
2. **Freeze.** Set the drain flag internally (H3's machinery, automatic here — no operator step): spawns/revives refuse, continuous fires skip. Flush all persistence.
3. **FD manifest.** For every live session record `{uid, child_pid, child_start_time, pty_master_fd, cgroup_prefix, generation, transcript_id}`, plus the control-socket listener FD. Since H1, `transcript_id` exists from birth for claude sessions (spawn-time pin), so the manifest and the on-disk state can cross-check identity even for a session that never completed a turn. `child_start_time` is `/proc/<pid>/stat` field 22, the pid-reuse guard. Clear CLOEXEC on exactly the listed FDs. Serialize to a memfd whose number rides in `CM_REEXEC_MANIFEST_FD`, alongside `CM_REEXEC_ATTEMPT` and the old binary's path (`CM_REEXEC_ROLLBACK_BIN`) for the rollback path. (`env_sanitize` must exempt `CM_REEXEC_*` — they are the one legitimate cross-exec channel.)
4. **Exec.** `execv(new_binary, same argv)`. Same PID: children unaffected, systemd (on cm-manager) sees nothing, the socket file stays bound via the inherited listener FD — there is no connection-refused window, only queued backlog.
5. **Rehydrate.** The new image sees `CM_REEXEC_MANIFEST_FD` and takes the handoff path instead of startup-restore: rebuild the registry binding inherited FDs; `pidfd_open` each child and verify start-time matches the manifest (mismatch = pid reuse = that session is genuinely gone → tombstone it honestly, never signal the pid); re-arm the reaper on the fresh pidfds; re-adopt memory-cap watchers via `cgroup_prefix` (the cgroup scopes survived — same mechanism as today's restore); rebuild workflow/continuous/monitor-adjacent state from disk exactly as startup does today; bump a `manifest.watch` generation so the TUI refreshes; clear the drain flag. (H3's drain flag is deliberately unpersisted, so a rehydrated image structurally cannot come up draining — the handoff path must preserve that property and never serialize it into the manifest.)
6. **Verify.** The caller's RPC connection died at the exec (in-flight connections are the one thing that can't be handed off meaningfully), so the contract is fire-and-verify: the caller polls `daemon.health`, which gains `{reexec_generation, build_id}` (git sha + build mtime baked in at compile time). cm-redeploy asserts the build_id changed and the session count survived.

### Failure paths, exhaustively

- `--verify-handoff` fails → RPC error, old daemon untouched. The common failure (schema drift, broken binary) lands here, before any point of no return.
- `execv` itself fails → it *returns into the old image*: restore CLOEXEC flags, un-drain, report the error over a fresh… the caller's connection is still alive in this case, so report inline. Old daemon untouched.
- Rehydrate fails fatally → children are alive and unsignaled (no code path in the new image signals a child before its pidfd + start-time are verified). The new image writes a crash note and execs `CM_REEXEC_ROLLBACK_BIN` with the same manifest and `CM_REEXEC_ATTEMPT+1`; the old binary rehydrates state it wrote itself. Attempt ≥ 2 → stop exec-looping: fall through to legacy startup-restore semantics (adopt-by-resume off the H1-pinned stamps). Sessions may die in this last-ditch case — but reaching it requires both the new and old binaries to fail rehydrating, and the preflight to have been wrong about the new one.

### What survives vs. what blips

| Plane | Across re-exec |
|---|---|
| Session child processes, mid-turn work, background children | Untouched — never signaled |
| PTY byte streams | Intact — kernel-buffered through the swap |
| Transcripts, inbox files, workflow events, monitors (they live in each session's MCP-server process, not the daemon) | Untouched — not daemon-resident |
| Memory-cap cgroup scopes | Survive; watchers re-adopt by `cgroup_prefix` |
| Control socket | No unbind — listener FD inherited; backlog queues during the sub-second gap |
| In-flight RPCs at the swap instant | Dropped connections; callers retry (MCP client and hook already retry-tolerant) |
| TUI attach streams | Drop at exec; auto-reattach (phase 4 extends the existing remote transport-EOF flow to local) |
| Workflow poller / continuous scheduler in-memory state | Rebuilt from disk, same as a normal startup |

## Why re-exec instead of a holder/brain process split

The split is the textbook answer: a tiny PTY-holder that never changes owns children + master FDs, and the brain restarts freely over an IPC boundary. Rejected for now because it buys little that re-exec doesn't, at materially higher cost: a new long-lived process to supervise, a new IPC surface whose own stability becomes the constraint, a state-migration project to get there, and two lifecycles for operators to reason about. Re-exec's one real constraint — the new binary must rehydrate the old binary's state — is bounded by the schema version + verify-handoff preflight, with legacy restart as the documented escape hatch. If version-compat pain proves chronic in practice, the split is the graduation path and nothing in this design blocks it (the FD manifest *is* the holder protocol, embryonically).

## Why not systemd FDSTORE / socket activation

FDSTORE solves exactly this for systemd-managed services, but the local daemon is TUI-launched with no unit, and adopting one would make the daemon's lifecycle diverge between local (the primary mode) and cm-manager. Re-exec is mechanism-identical in both. On cm-manager, deploys switch from `systemctl stop/cp/start` to `cp + daemon.restart`; systemd keeps `Restart=always` for crashes and owns boot.

## Restart decision table (operator-facing, goes in CLAUDE.md when built)

| Change | Primitive | Sessions |
|---|---|---|
| Config value (`mcp_server_path`, `api_*`, `notify_command`) | `daemon.reload_config` (H2) | untouched |
| Daemon code | `daemon.restart` (re-exec) | untouched |
| Re-exec machinery itself / incompatible state schema | drain (H3) → legacy restart | die, resume from H1-pinned stamps |
| Machine reboot | startup-restore | die, resume from H1-pinned stamps |

The three hardenings interlock rather than being obsoleted: H2 removes most *reasons* to restart, H1 makes the surviving hard-restart cases lossless-in-identity, H3 gives them a clean seam and is reused internally as re-exec's freeze.

## Phases

1. **OS proof** — DONE (appendix).
2. **Handoff skeleton** behind a `CM_REEXEC=1` dev flag: manifest write, CLOEXEC discipline, exec, minimal rehydrate of a single bash session; scratch-daemon-sandbox e2e proving PTY continuity through a real daemon re-exec.
3. **Full rehydrate**: registry + pidfd/start-time guard + reaper re-arm, mem-cap re-adoption, workflow/continuous rebuild, manifest.watch generation bump, internal drain-freeze, `--verify-handoff` + schema version, rollback exec + attempt cap. Sandbox matrix: idle claude session, mid-turn session (background command writing), bash session, workflow participant.
4. **TUI local auto-reattach**: extend the remote transport-EOF → `pending_remote_reattach` flow to `local` host rows; delete cm-redeploy's TUI kill.
5. **`daemon.restart` RPC + cm-redeploy switch**: build → verify-handoff → RPC → poll health `build_id`; keep `--hard` as the legacy path.
6. **cm-manager rollout** + CLAUDE.md decision table.

## Risks

- **State-schema drift between binaries** — the central risk; bounded by the schema version + verify-handoff preflight, with drain+legacy restart as the escape hatch. Rule: state-format changes bump the version and ship with a legacy restart, everything else re-execs.
- **Pid reuse** between manifest write and rehydrate — start-time comparison; on mismatch, tombstone, never signal.
- **FD hygiene across generations** — the rehydrate closes every inherited FD not named in the manifest; CLOEXEC audit is part of phase 2's review.
- **Half-rehydrated daemon** — rollback exec + attempt cap; no path signals a child pre-verification, so the worst case degrades to today's behavior, never below it.
- **Believing it worked when it didn't** — `daemon.health` build_id/generation + cm-redeploy assertion; the 2026-08-03 lesson (process-alive ≠ working) applies doubly here.

## Appendix: phase-1 proof

`scripts/reexec_proof.py` (committed with this doc; run 2026-08-18, exit 0):

```
new-image pid 1061653: child 1061654 alive, ppid=1061653 (still our child)
pidfd re-acquired: fd 4
pre-exec bytes survived in PTY buffer: True
PTY flows post-exec: True
```

(The old image's own pre-exec print is absent from captured output for a fittingly instructive reason: it sat in stdio's userspace buffer, which — unlike kernel PTY buffers — is process memory and does not survive `exec`. Daemon logging around the exec point must write unbuffered.)

Stage 1 spawns `bash --norc` on a fresh PTY, writes a command, deliberately leaves the output unread, clears CLOEXEC on the master, and `execv`s itself. Stage 2 (same PID, new image) verifies the child's PPid still equals our PID, re-opens a pidfd, and reads both the pre-exec and a post-exec command's output from the inherited master FD.
