# Seamless daemon restart (in-place re-exec) — design proposal

Branch: `cm/fix-cm` (design; implementation phases land on their own branches)

Status: **revised after adversarial review round 1** (2026-08-18, two independent reviews — see Review log). The mechanism survives review; the original spec did not. The load-bearing changes: the rollback guarantee is narrowed to recoverable failures (crash-class failures in the new image cannot roll back — anything stronger requires the holder split), the freeze step is a real quiescence barrier instead of a flag, handed-off identities ride as FDs (pidfds, pinned executables) instead of names, rehydrate is a transaction against escrowed FDs with a commit gate, and phase 2 is preceded by prerequisite refactors that are ordinary, individually-testable daemon changes.

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

A `daemon.restart` operator RPC that replaces the daemon's code by `exec()`ing the new binary **in the same process**. Hosted sessions are never signaled and never resumed — they simply keep running, mid-turn included. PTY bytes produced during the swap wait in the kernel buffer (bounded — see Quiesce). The control plane (socket RPCs, attach streams) blips for well under a second and self-heals.

The rollback guarantee, stated honestly (review R1): a bad new binary is caught **before** the exec (old daemon keeps running) for every failure the preflight can see, and a **recoverable** rehydrate failure (a `Result` error under the new image's top-level recovery guard) rolls back to the pinned old binary without touching the children. A **crash-class** failure in the new image — segfault, abort, OOM-kill, panic-with-abort, an uncaught early exit — cannot execute any rollback path: the process dies, the PTY masters and listener close with it, and the children are lost exactly as in today's restart. This residual is minimized (the rehydrate path runs read-only against escrowed FDs, under `catch_unwind`, with panic=unwind, doing as little as possible before commit) but it cannot be engineered away inside a single process. If unconditional survival of a bad binary ever becomes a requirement, the holder/brain split stops being optional — see "Why re-exec instead of a process split".

## Out of scope

- **Holder/brain process split** (a tiny stable PTY-holder process + a freely restartable logic daemon). The architecturally pure version; deliberately deferred — but review R1 sharpened its trigger condition: the split becomes mandatory the day we require rollback from crash-class new-image failures, because no in-process design can provide it.
- **Machine reboots.** Children die with the machine; the existing startup-restore + resume path (hardened by H1 pinning) remains the answer.
- **TUI redeploys.** The TUI is stateless-ish and user-facing; restarting it is cheap and stays as-is (minus its current dependence on also killing the daemon).
- **Changing a *running* session's spawn-time properties** (env, hook wiring). That inherently needs that one session respawned (`A-R`), never the fleet.

## Proven base (phase 1 — done, 2026-08-18) and what it did NOT prove

A sandbox proof (`scripts/reexec_proof.py`, appendix) validated four load-bearing OS assumptions in one run:

1. a forked child survives the parent's `exec()` and remains parented to the same PID;
2. a `pidfd` is re-acquirable from the stored numeric pid after exec (superseded by design: we now hand off the spawn-time pidfds themselves — strictly stronger);
3. PTY output written by the child and **left unread across the exec** is intact in the kernel buffer afterwards — bytes produced during the swap are buffered, not lost;
4. the inherited master FD (CLOEXEC cleared) keeps flowing normally in the new program image.

Review caveat (R3): the proof deliberately had **no reader thread**. The real daemon runs a per-session reader continuously draining the PTY into a process-memory fanout ring — at exec, up to one read-buffer of bytes (8 KiB today) can sit on a reader thread's stack between `read()` and `fanout.push()`, already removed from the kernel and about to vanish with the thread. Assumption 3 therefore holds only if every reader is **quiesced at a post-push safe point** before the exec (see Quiesce). The proof also had one child, no concurrent writers, and no signal handlers; none of its results speak to the quiescence problem, which review identified as the design's real center of gravity.

## Mechanism

### The restart sequence

`daemon.restart` (Operator-only — see Security posture; params: `binary_path` defaulting to the shared-target release build) is executed by a single **restart coordinator**; a second `daemon.restart` while one is in flight is refused (`restart_in_progress`). The sequence:

1. **Pin the executables.** Open `binary_path` read-only and hold the FD; from here on, the new binary is that inode — preflight and exec both go through it (`/proc/self/fd/N` for the subprocess, `execveat`/`fexecve` for the exec), so the checked artifact IS the executed artifact (no pathname TOCTOU — R7). Snapshot the *current* image the same way: open `/proc/self/exe` (still maps the original inode even if a deploy already overwrote the path — which our deploy flows DO, both `cp` on cm-manager and `cargo build` locally, so a rollback *pathname* would resolve to the new binary and make rollback a disguised retry). Both FDs ride in the manifest with CLOEXEC cleared; verify-handoff asserts new-inode ≠ rollback-inode.
2. **Preflight, old image still in charge.** Run `<pinned-new-fd> --verify-handoff <dry-manifest>` as a subprocess: parse the manifest format + current state files (`daemon-sessions.json`, workflow runs, continuous state), check the state schema version, run the MCP preflight, exit 0/1. Runs WITHOUT the state lock (it shells out for seconds). Any failure → the RPC returns an error and **nothing happened**. Because the dry run races live mutations, step 3 re-validates: after quiescence, the coordinator re-checks the schema-version/shape assertions against the frozen snapshot (cheap, in-process) so the preflighted state and the handed-off state cannot silently diverge (R15).
3. **Quiesce — a prepare/commit/abort barrier, not a flag** (R2, R3, R4, R10, R12). The drain flag (H3) closes the front door (spawns/revives refuse, continuous fires skip), but flags don't stop in-flight work; every subsystem must reach an acknowledged safe point, bounded by a timeout after which the restart ABORTS with `restart_busy` (un-drain, release permits, report inline — nothing happened):
   - **Mutating RPCs**: an in-flight-mutation counter (RAII guard at dispatch entry) must reach zero — a spawn that already passed the drain check would otherwise fork a child that misses the manifest and is orphaned by the exec.
   - **PTY readers**: each reader pauses at a post-`push` boundary and acks. After the ack, no bytes are in flight on any reader stack; everything is either in the fanout ring (persisted implicitly via transcripts; the ring itself is expendable) or still in the kernel buffer, where it survives the exec. This is what makes the byte-level continuity claim true (the phase-1 proof assumed it by construction).
   - **Reapers**: the coordinator takes the exclusive **reap permit** (see prerequisite P1): no reaper consumes a `waitpid` status during the handoff window, so a child exiting mid-swap stays a zombie the new image reaps with full status after commit. Without this, a status consumed microseconds before the exec is lost between `waitpid` and persistence (R4).
   - **Prompt/input deliveries**: the detached delivery paths (prompt body … pause … Enter) and every raw PTY writer reach a between-writes safe point or the restart aborts — a prompt frozen unsubmitted, or a partial `write_all`, is corrupted terminal input no rehydrate can repair (R10).
   - **Memory-cap watchers**: each watcher acks a no-signal safe point (never between its SIGTERM and delayed SIGKILL) and checkpoints its in-memory policy state — protected PID set, `last_high` breach counter, kill-log baselines, operator-kill attribution — into the manifest, so re-adoption resumes the same policy rather than recomputing a different one from the current cgroup (R12).
   - **Poller / scheduler / SIGHUP watcher / transcript detectors**: paused at tick boundaries.
   - Then: one **checked** (not best-effort) persistence pass — sessions, workflow runs, continuous state — with `fsync` on files and containing directories, so the snapshot the new image rebuilds from is the snapshot we froze (R15).
4. **FD manifest.** For every live session record `{uid, generation, transcript_id, child_pid, child_start_time, pty_master_fd, pidfd, cgroup_prefix, watcher_checkpoint}`, plus the control-socket listener FD, the TLS listener FD when `[tls]` is configured (R13), and the two pinned executable FDs from step 1. The **spawn-time pidfd itself is handed off** — pid-reuse becomes structurally impossible for signaling (`pidfd_send_signal`, `waitid(P_PIDFD, …)`), with `child_start_time` (`/proc/<pid>/stat` field 22) retained as a cross-check and PPid==self verified before any signal-capable object is built (R6). One canonical master FD per session; reader/writer handles re-derive by dup post-exec. Serialize to a **sealed memfd**: magic, schema version, explicit length, checksum; `pwrite` then `F_SEAL_WRITE|F_SEAL_GROW|F_SEAL_SHRINK|F_SEAL_SEAL`; the reader uses `pread` (a memfd's file offset survives exec — a plain `read` after the writer's append sees EOF, R8), enforces a small size cap, rejects duplicate FDs, stdio-range FDs, and any FD whose `fstat`/type doesn't match its manifest role. Only the memfd's number rides in env (`CM_REEXEC_MANIFEST_FD`); the attempt counter, rollback FD number, and everything else live *inside* the sealed manifest — env is a bootstrap pointer, not a trust surface (R8, R9).
   CLOEXEC discipline (R9): with spawns quiesced (step 3 — nothing can fork), audit the whole FD table to CLOEXEC (`close_range(0, ~0, CLOSE_RANGE_CLOEXEC)` equivalent, sparing stdio), then clear the flag on exactly the manifest-listed FDs. An abort after this point restores flags before releasing the barrier. As defense in depth, every ordinary child-spawn env builder strips `CM_REEXEC_*` (today nothing sets them, but a handoff daemon's environ carries them).
5. **Exec.** Block SIGHUP and SIGTERM first: the signal *mask* survives exec while caught dispositions reset to default — and SIGHUP's default is terminate, so an operator's config-reload reflex (`kill -HUP`) landing between the exec and the new image's handler install would kill the daemon mid-rehydrate with no rollback (R13). Then `execveat(new_binary_fd, "", AT_EMPTY_PATH)` — same PID: children unaffected, systemd (on cm-manager) sees nothing, the socket stays bound via the inherited listener FD — no connection-refused window, only queued backlog. Exec uses an explicit `envp` (never `setenv` on the shared environ — R9), with argv[0] updated to the new binary's path for `ps` hygiene. All logging near the exec point goes straight to stderr (unbuffered); the phase-1 proof demonstrated stdio buffers dying at exec.
6. **Rehydrate — a transaction against escrowed FDs** (R5). The new image detects `CM_REEXEC_MANIFEST_FD` **before** `bind_socket` and **before** `restore_sessions` — normal startup would probe the socket path, connect to its own inherited listener, and refuse with `AddrInUse`; and legacy restore must never run on the handoff path, because it spawns `--resume` duplicates of children that are still alive (R13). It reinstalls signal handlers, unblocks the mask, validates the manifest (seals, magic, checksum, FD types), then:
   - Treat the inherited FDs as **untouched escrow**. Build candidate sessions from CLOEXEC **dups** using a non-killing adoption type — `DaemonSession`'s normal `Drop` SIGKILLs its child, so constructing real sessions eagerly means a failure on session N+1 unwinds by killing verified children 1..N (R5). No reader/reaper threads start, no disk writes happen (no tombstones, no persists, no generation bumps), until every record validates.
   - **Commit gate**: all records verified (pidfd alive or honestly-exited, PPid==self, start-time cross-check, transcript_id vs on-disk state) → atomically promote candidates into the registry, arm normal Drop semantics, start readers (draining the kernel backlog) and pidfd-poll reapers, release the reap permit, re-adopt memory-cap watchers from their checkpoints, rebuild workflow/continuous/monitor-adjacent state from the fsynced disk snapshot, bump `manifest.watch` generation (attach clients treat it as a new stream — fanout byte offsets restart at zero), clear the drain flag, consume-and-clear `CM_REEXEC_*` from the environment before serving or spawning anything (R14).
   - A child whose pidfd reports exited: reap via `waitid(P_PIDFD)` for full status, tombstone honestly. Never signal anything pre-commit.
   - (H3's drain flag is deliberately unpersisted, so a rehydrated image structurally cannot come up draining — the handoff path must preserve that property and never serialize it into the manifest.)
7. **Verify.** The caller's RPC connection died at the exec (in-flight connections are the one thing that can't be handed off meaningfully), so the contract is fire-and-verify: the caller polls `daemon.health`, which gains `{reexec_generation, build_id}` (git sha + build mtime baked in at compile time). cm-redeploy asserts the build_id changed and the session count survived.

### Failure classes, exhaustively

Two classes (R1), with different guarantees:

**Recoverable** (a `Result` error / caught panic under the new image's top-level recovery guard, which never `exit()`s while escrow FDs are live):

- `--verify-handoff` fails → RPC error, old daemon untouched. The common failure (schema drift, broken binary) lands here, before any point of no return.
- Quiescence times out → restart aborts with `restart_busy`: un-drain, restore CLOEXEC flags, release permits, report inline. Old daemon untouched.
- `execveat` itself fails → it *returns into the old image*: same abort path, report inline (the caller's connection is still alive in this case). Old daemon untouched.
- Rehydrate fails (validation error, corrupt manifest, resource exhaustion caught as Result) → children are alive and unsignaled (no code path signals a child pre-commit, and candidates are non-killing by construction). The new image writes a crash note and `execveat`s the **pinned rollback FD** from the manifest with attempt+1 (both recorded inside the sealed manifest, not env); the old binary rehydrates state it wrote itself — which is still true because the failed image was disk-read-only pre-commit. On a manifest that fails validation outright: touch no escrow FD, never trust a rollback path or PID from a corrupt manifest — the rollback FD is only used when the manifest's integrity envelope (magic/checksum/seals) verifies; otherwise fall through to the terminal case.
- Attempt ≥ 2 (a strict, finite state machine — attempt rides in the sealed manifest, so it cannot be looped by env alone) → **terminal fallback**: deliberately SIGKILL and reap the exact inherited pidfds (or, with no usable manifest, enumerate own children via /proc and kill those), close all inherited FDs, then run legacy startup-restore, which resumes from the H1-pinned stamps. Sessions **are killed deliberately, then resumed** — never abandoned alive. The pre-review spec said sessions "may die" here; review (R7, and independently the fresh-eyes pass) showed the truth was worse than death: falling through with live orphans while restore spawns `--resume` duplicates is two live writers per conversation — the 2026-08-18 split-brain class, mechanically reproduced.

**Crash-class** (segfault, abort, OOM-kill, panic=abort, uncaught early exit in the new image): no rollback is possible — the old code no longer exists and process death closes every PTY master and the listener. Children receive SIGHUP from PTY teardown; the next daemon start runs legacy restore. This is today's failure mode, reached only when preflight passed AND the new image crashed (not erred) mid-rehydrate. Minimized by keeping the pre-commit window small, read-only, and unwind-safe; eliminated only by the holder split.

### What survives vs. what blips

| Plane | Across re-exec |
|---|---|
| Session child processes, mid-turn work, background children | Untouched — never signaled |
| PTY byte streams | Lossless — readers quiesced at a push boundary pre-exec, remainder kernel-buffered through the swap. Kernel PTY buffers are small (~64 KiB): a chatty child blocks on write mid-swap (a stall, never loss), so the new image must start readers promptly |
| Fanout ring (scrollback) + byte offsets | **Lost / reset to zero** — daemon-memory. The TUI keeps its own pane history; attach clients must treat the `reexec_generation` bump as a new stream, not a resumable offset |
| Transcripts, inbox files, workflow events, monitor processes (MCP-server-resident) | Untouched — not daemon-resident |
| `done_report` / activity / turn timestamps on LIVE sessions | Carried in the manifest — a worker that called `report_done` pre-restart must not regress to `awaiting_input` and strand an `until="final"` watcher (R11) |
| Recently-exited tombstones, kill attribution | Rebuilt from the fsynced persistence pass (they must be IN it — R11) |
| Attach tickets | **Invalidated** — short-TTL capabilities are reminted by clients, never serialized (R11) |
| TUI-pushed state (task tree snapshots, workflow-definition overrides) | Re-pushed by the TUI on the `manifest.watch` generation bump; scope-sensitive RPCs answer from last-persisted data until then (R11) |
| Memory-cap cgroup scopes | Survive; watchers re-adopt from **checkpointed** policy state (protected set, breach counters, baselines), not recomputed from the live cgroup (R12) |
| Control socket | No unbind — listener FD inherited; backlog queues during the sub-second gap |
| TLS listener (when configured) | Inherited via the manifest, same as the Unix listener (R13) |
| In-flight RPCs at the swap instant | Dropped connections; callers retry (MCP client and hook already retry-tolerant) |
| TUI attach streams | Drop at exec; auto-reattach (phase 5 extends the existing remote transport-EOF flow to local) |
| Workflow poller / continuous scheduler in-memory state | Rebuilt from disk, same as a normal startup |

**State-inventory rule** (R11): every field of `DaemonState` and every per-session in-memory cell gets a row in a handoff table classifying it as *handed off* (manifest), *rebuilt* (disk), *invalidated* (clients re-derive), or *quiesced away* (guaranteed empty by the barrier). Building that table is part of phase 3, and "unclassified" fails the phase's review — the incomplete inventory was a review finding, not an oversight to repeat.

## Security posture

`daemon.restart` is arbitrary code replacement for a process that holds every session's PTY. Review R14 found the current Operator gate fail-open: with `CM_OPERATOR_TOKEN` unset, operator-caller validation is disabled (compat behavior in `control/operator.rs`), so any same-UID process that can reach the 0600 socket — including a hosted session — could call it. Therefore:

- `daemon.restart` (and it alone, initially) **refuses to run when operator authentication is not configured**, on both local and ssh-trust deployments. Fail closed for code replacement even where everything else stays compat.
- `binary_path` is restricted to a pinned/approved artifact policy (default: the shared-target release path, resolved then inode-pinned per step 1); the RPC never execs a caller-supplied path outside it.
- `CM_REEXEC_*` env is a bootstrap pointer only (FD number); all trust lives inside the sealed, checksummed memfd, which is validated for memfd-ness (fstat + `/proc/self/fd` name) and seals before anything is believed. A fresh daemon start (launched from a leaked env, the 2026-08-18 pattern) that sees `CM_REEXEC_*` but fails validation scrubs the vars and boots as a normal fresh start.
- A validated handoff must additionally look like one: the manifest's listener FD is bound to the expected socket path, and its children are parented to this PID — otherwise reject and boot fresh.
- The new image consumes and clears `CM_REEXEC_*` before serving requests or spawning anything; ordinary child-spawn env construction strips `CM_REEXEC_*` as defense in depth.
- `env_sanitize` keeps scrubbing Claude-identity vars at startup as today; it must never grow a wildcard that eats `CM_REEXEC_*` on the handoff path (the validation-failure scrub above is the only place they're removed).

## Codex sessions: lineage pinning gap (folded in from operator review)

H1 pinned **claude** session ids at spawn, and the cm Stop hook re-stamps the resume key every turn — so claude sessions are resumable-from-birth and never more than one turn stale. **Codex sessions have neither**: rollout ids rotate on compact, codex runs no cm hook, and the daemon pins only the spawn-time rollout id. Re-exec makes this moot for deploys (children never die), but every hard path still resumes codex from a stale lineage: the decision table's drain+legacy and machine-reboot rows, and the terminal fallback above. Restart-survivability must cover codex too:

- **Scope addition — LANDED (phase 4f, this slice):** daemon-side codex rollout tracking, the codex analogue of the claude Stop-hook re-stamp. Mechanism refined from the original sketch: instead of tailing the `~/.codex/sessions/YYYY/MM/DD/` directories (which can only guess which rollout is this session's), the daemon observes **ground truth** — the codex process holds its live rollout file open, so a per-session watch (`transcript_detect::spawn_codex_rollout_watch`, armed for every codex session at the `start_session` spawn funnel) re-reads `/proc/<child pid>/fd` every ~5s (bounded descendant walk through the npm launcher / `systemd-run` wrap, shape-matched against the rollout layout) and re-stamps the resume identity through the same `set_transcript_path` flow (generation bump + persist) on rotation. `transcript_id_from_path` now extracts the trailing uuid from `rollout-<ts>-<uuid>` stems — `codex resume <id>` takes the `payload.id` uuid, not the file stem — and `compose_restore_params` normalizes legacy stem-shaped persisted ids at restore time, so both restore and revive resume the post-compact lineage.
- Residual caveat, narrowed: only a compact that lands **within the last observation interval** (~5s) before a hard stop can still resume pre-compact history; a codex build that stops holding its rollout open would silently reopen the full gap (the watch then never re-stamps — pre-4f behavior, logged only for permission errors). Known integration gap: the re-exec adoption path promotes sessions without passing through `start_session`, so phase-4 rehydrate must re-arm the codex rollout watch post-commit.

## Why re-exec instead of a holder/brain process split

The split is the textbook answer: a tiny PTY-holder that never changes owns children + master FDs, and the brain restarts freely over an IPC boundary. Rejected for now because it buys little that re-exec doesn't for the failures we actually have (code deploys with a working binary), at materially higher cost: a new long-lived process to supervise, a new IPC surface whose own stability becomes the constraint, a state-migration project to get there, and two lifecycles for operators to reason about. Re-exec's real constraints after review are two: the new binary must rehydrate the old binary's state (bounded by the schema version + verify-handoff preflight, with the deliberate-kill legacy fallback as the escape hatch), and **crash-class failures in the new image are unrecoverable** (bounded by preflight + a minimal read-only pre-commit window). If version-compat pain proves chronic, or unconditional rollback becomes a requirement, the split is the graduation path and nothing in this design blocks it — the FD manifest *is* the holder protocol, embryonically, and the quiescence barrier is exactly the seam the holder IPC would need.

## Why not systemd FDSTORE / socket activation

FDSTORE solves exactly this for systemd-managed services, but the local daemon is TUI-launched with no unit, and adopting one would make the daemon's lifecycle diverge between local (the primary mode) and cm-manager. Re-exec is mechanism-identical in both. On cm-manager, deploys switch from `systemctl stop/cp/start` to `cp + daemon.restart` (safe against the overwritten path because rollback is inode-pinned, step 1); systemd keeps `Restart=always` for crashes and owns boot.

## Restart decision table (operator-facing, goes in CLAUDE.md when built)

| Change | Primitive | Sessions |
|---|---|---|
| Config value (`mcp_server_path`, `api_*`, `notify_command`) | `daemon.reload_config` (H2) | untouched |
| Daemon code | `daemon.restart` (re-exec) | untouched |
| Re-exec machinery itself / incompatible state schema | drain (H3) → legacy restart | killed deliberately, resumed from H1-pinned stamps (codex: at most one ~5s observation interval stale — phase-4f rollout tracking) |
| Machine reboot | startup-restore | die, resume from H1-pinned stamps (same codex caveat) |

The three hardenings interlock rather than being obsoleted: H2 removes most *reasons* to restart, H1 makes the surviving hard-restart cases lossless-in-identity, H3 gives them a clean seam and is reused as the front door of re-exec's quiescence barrier.

## Phases

Review restructured these: the original phase 2 ("handoff skeleton") assumed daemon internals that don't exist yet. Those land first as **ordinary, individually-testable daemon changes** with no exec anywhere near them — each is a good change on its own.

1. **OS proof** — DONE (appendix; see the reader-thread caveat in "Proven base").
2. **Prerequisite refactors** (no re-exec code):
   a. Reaper conversion: blocking per-session `waitpid` threads → pidfd-poll reapers gated on a shared **reap permit** (R4).
   b. PTY reader safe points: pause/ack at a post-push boundary (R3).
   c. Non-killing adoption constructor for sessions (candidate type whose Drop never signals) (R5).
   d. Restart coordinator + quiescence barrier: in-flight mutation counter, writer/delivery safe points, watcher checkpoint+ack, generalizing H3's drain (R2, R10, R12).
   e. Fail-closed operator gate for lifecycle RPCs (R14).
3. **Handoff skeleton** behind a `CM_REEXEC=1` dev flag: sealed-manifest write/read (pread, seals, checksum, FD-type validation), CLOEXEC audit + allowlist, pinned-FD `execveat`, signal-mask bracketing, handoff detection ahead of bind/restore, minimal rehydrate of a single bash session; scratch-daemon-sandbox e2e proving PTY continuity through a real daemon re-exec **with a live reader draining** (the condition phase 1 skipped).
4. **Full rehydrate**: escrow/commit-gate transaction, complete state-inventory table (fail review on "unclassified"), pidfd handoff + `waitid(P_PIDFD)`, watcher re-adoption from checkpoints, workflow/continuous rebuild, manifest.watch generation bump, `--verify-handoff` + schema version + frozen-snapshot revalidation, rollback-FD exec + attempt state machine + deliberate-kill terminal fallback, codex rollout tracking (landed as 4f — /proc fd observation; see the codex section). Sandbox matrix: idle claude session, mid-turn session (background command writing), bash session, workflow participant, session that dies mid-handoff, corrupt manifest, rollback exec.
5. **TUI local auto-reattach** — DONE: the transport-EOF → `pending_remote_reattach` flow is host-agnostic (the latched EOF-no-`End`-frame flag gates, not the row's host), and the S5 socket-HUP watchdog covers local rows too (a peer-closed attach socket can surface as a `POLLHUP` spin with no read — the e2e reproduced it). Offset/generation reset needed no protocol change: `attach.open` carries no client byte offset (the daemon replays its current ring), so a reattach is a new stream by construction — pinned by an e2e that rebinds across a ring reset and proves BOTH halves (input reaches the new daemon, output renders) move together. cm-redeploy's TUI-kill deletion moved to phase 6 — the script changes wholesale when `daemon.restart` lands, and changing it earlier would break its current legacy contract.
6. **`daemon.restart` RPC + cm-redeploy switch**: build → verify-handoff → RPC → poll health `build_id`; keep `--hard` as the legacy path; delete cm-redeploy's TUI kill.
7. **cm-manager rollout** + CLAUDE.md decision table.

## Risks

- **Crash-class new-image failure** — unrecoverable by construction (see Goal); bounded by preflight + minimal read-only pre-commit window + unwind-safe rehydrate; eliminated only by the holder split. Named first because pretending otherwise was review finding R1.
- **State-schema drift between binaries** — bounded by the schema version + verify-handoff preflight + post-freeze revalidation, with drain+legacy restart as the escape hatch. Rule: state-format changes bump the version and ship with a legacy restart, everything else re-execs.
- **Quiescence latency/liveness** — a barrier that can't be reached (wedged writer, stuck delivery) must abort the restart, not hang the daemon: bounded waits, `restart_busy`, full un-drain on abort.
- **Pid reuse** — structurally closed for signaling by handing off spawn-time pidfds; start-time + PPid checks retained as cross-checks. Tombstone on mismatch, never signal.
- **FD hygiene across generations** — whole-table CLOEXEC audit before allowlist-clear (with spawns quiesced); rehydrate closes every inherited FD not named in the manifest; phase-3 review includes the audit.
- **Half-rehydrated daemon** — escrow + commit gate + read-only-until-commit + rollback exec of a pinned inode + finite attempt machine; the terminal fallback kills-and-reaps deliberately so the worst case degrades to today's behavior, never below it (never split-brain).
- **Believing it worked when it didn't** — `daemon.health` build_id/generation + cm-redeploy assertion; the 2026-08-03 lesson (process-alive ≠ working) applies doubly here.
- **Codex lineage staleness on hard paths** — closed by phase-4f rollout tracking down to a ≲5s window (a compact within the last observation interval before the hard stop); the re-exec adoption path still needs to re-arm the watch post-commit (phase-4 integration), and a codex build that stops holding its rollout fd open would reopen the gap silently.

## Review log

- **2026-08-18, round 1** — two independent adversarial reviews against the original spec: a codex session (15 findings: 10 blocker, 4 should-fix, 1 nit-level; verdict "not safe to implement as specified") and a fresh-eyes self-review (12 findings + nits; 2 blockers). Overlapping blockers: pathname-based rollback defeated by deploy overwrite; drain-not-quiescence; legacy-fallback split-brain (live orphans + resumed duplicates); handoff-unaware startup self-refusing on `AddrInUse`; manifest validation; signal-disposition reset across exec. Codex-only: crash-class rollback impossibility (narrowed the Goal), reader-stack byte loss, exit-status loss window, `Drop`-kills-child during partial rehydrate, CLOEXEC-window FD/env leak into concurrent spawns, half-applied prompt deliveries, watcher policy-state loss, memfd offset gotcha, fail-open operator gate. Self-review-only: codex lineage gap (operator-directed scope), portable-pty's missing raw-FD adoption constructor (subsumed into P2c/R5's adoption type), TLS listener (independently found by both). This document is the post-review revision; phases 2–4 above encode the required changes. Verdict after synthesis: the mechanism is sound and worth building, the original spec was not — build phase 2 (prerequisites) first, and treat the narrowed rollback guarantee as a permanent property of the single-process design, not a TODO.

## Appendix: phase-1 proof

`scripts/reexec_proof.py` (committed with this doc; run 2026-08-18, exit 0):

```
new-image pid 1061653: child 1061654 alive, ppid=1061653 (still our child)
pidfd re-acquired: fd 4
pre-exec bytes survived in PTY buffer: True
PTY flows post-exec: True
```

(The old image's own pre-exec print is absent from captured output for a fittingly instructive reason: it sat in stdio's userspace buffer, which — unlike kernel PTY buffers — is process memory and does not survive `exec`. Daemon logging around the exec point must write unbuffered — promoted to a phase-3 requirement.)

Stage 1 spawns `bash --norc` on a fresh PTY, writes a command, deliberately leaves the output unread, clears CLOEXEC on the master, and `execv`s itself. Stage 2 (same PID, new image) verifies the child's PPid still equals our PID, re-opens a pidfd, and reads both the pre-exec and a post-exec command's output from the inherited master FD. See "Proven base" for what this deliberately did not exercise (readers, concurrency, signals).
