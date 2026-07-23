# Smarter soft-cap reaping

Companion to `DESIGN_MEMORY_CAP.md`. That doc defines the memory-cap mechanism (systemd-run scope, MemoryHigh/MemoryMax, the per-session watcher); this doc fixes the watcher's victim-selection policy, which currently eats long-running background processes (monitors, watchdog loops) that are innocent bystanders to a breach.

## Context: what the reaper is today

cm has exactly one mechanism that kills processes it didn't explicitly spawn: the soft-cap breach killer in `daemon/src/session_watch.rs::run_watcher` / `handle_breach` (twin: `tui/src/session_watch.rs` for TUI local-spawn). There is no lifecycle-event orphan sweep anywhere in the codebase — `continuous/scheduler.rs::reconcile_orphans` is run-state bookkeeping (no signals), `kill_session` pidfd-SIGKILLs the session leader only, worktree removal never touches processes.

The watcher's current policy:

1. At spawn, snapshot `cgroup.procs` after a 750ms stabilize window (`STABILIZE_MS`) → the `protected` set. For 2 more seconds (`FOLLOWUP_MS`) adopt new PIDs whose PPID is protected. **The set is then frozen forever** — every process the agent spawns after ~2.75s (i.e. every tool call, every background monitor) is an eligible victim. That's deliberate (kill the runaway grandchild, not the agent), and this doc does not change it.
2. Poll `memory.events.high` every 1s (`POLL_INTERVAL_MS`). On **every** increment, `handle_breach` picks the **highest-RSS unprotected PID** and SIGTERMs it, waits 500ms (`SIGTERM_GRACE_MS`), then SIGKILLs.

Two failure modes fall out:

- **Serial mowing.** `memory.events.high` keeps incrementing while memory sits above MemoryHigh, so a session hovering near its soft cap kills one unprotected process per poll tick — the hog first, then progressively smaller innocents, one per second, until the pressure clears.
- **Pointless collateral.** When the true hog is short-lived (already exited by the time the counter increment is observed), the "highest-RSS unprotected" PID may be a 5MB shell loop whose death cannot possibly relieve a multi-GB breach.

Kill forensics exist (`~/.cm/memory_kills/<uid>.jsonl`, `KillStatus` wire values `killed_by_us` / `protected` / `already_dead` / `no_pids`), and exit attribution joins them in `daemon/src/reaper.rs::is_cap_kill` + `kill_status_priority`. But nothing tells the *agent* its process was killed — a reaped monitor is a silent `signal 9` mystery (see the CLAUDE.md "On signal 9" section, which exists precisely because of this).

## Incident attribution: the 2026-07-23 watchdog kills (resolved — NOT cm)

The incident that prompted this doc (two TPU-campaign watchdog `run_in_background` shell loops killed 46s apart, 20:10:01Z and 20:10:47Z) was **not cm**. Forensics on the box eliminated every external killer with direct evidence: cm has no sweep (code + full git history), the cm Stop hook and all four predictionTrading hooks are kill-free, systemd-oomd inactive, earlyoom absent, kernel journal empty for the window (no global or memcg OOM), systemd-tmpfiles didn't run that day, no cron/timer fired, and no agent ran a kill command (all transcripts swept, subagents included).

The actual mechanism is **Claude Code's own background-task manager**: the v2.1.218 binary carries a low-memory sweep — stop reasons `lowMemStop` / `sweepStaleStop`, telemetry `tengu_bg_retire_pinned_low_mem` / `tengu_bg_dispatch_low_mem`, a `pressure_level` monitor (macOS memorystatus; `MemAvailable` on Linux), and `tengu_bg_dispatch_sigkill_escalate` (matching the observed no-exit-banner SIGKILLs). The trigger window was another agent's `systemd-run --scope -p MemoryMax=8G uv run pytest test/ -q` (20:09:44→20:14:53Z) driving box-wide memory low; death #1 landed 17s after pytest started. It explains every observable: age-independent kills-on-sight during the pressure window only, canaries surviving a later quiet window, and the immunity boundary sitting exactly at "what the harness tracks" (setsid-detached and systemd-run processes untouched).

Implications for this doc: cm is exonerated for that incident, and the phases below fix a *latent, different* failure mode (cm's own cap-watcher mowing, which will bite the first time a capped session hovers at its soft cap). The harness's sweeper is outside cm's control — the durable guidance for agents is: critical monitors must NOT be harness-tracked background shells. Use a session cron (what the campaign converged on), a `systemd-run --user` transient scope, or a cm-side monitor (`monitor_sessions`); Phase 3's notice text and the Phase 4 docs commit should carry this guidance.

## Goals / non-goals

Goals, in phase order: (1) stop serial mowing, (2) stop pointless collateral, (3) make kills visible to the agent in-band, (4) give agents an explicit "don't reap this" marker.

Non-goals for this doc (candidate follow-ups, deliberately deferred): age-weighted badness scoring, killing unprotected subtree roots instead of single PIDs, an MCP `protect_process` registration tool, sub-cgroup partitioning (`payload/` vs `monitors/`), a first-class `cm-monitor` primitive riding systemd-run, per-session reap-policy knob in A-e. None of these are prerequisites for the phases below.

Invariants preserved throughout: the hard cap (MemoryMax, kernel OOM) is never weakened — everything here is soft-cap victim-selection policy only. Exit attribution (`is_cap_kill` decision table, per-spawn baseline probing) keeps working for every new record shape. Both watcher twins (daemon + TUI) change in lockstep — they are copy-twins by prior decision ("a future refactor could share; out of scope", `session_watch.rs` header), and this doc keeps parity rather than unifying them.

---

## Phase 1 — Breach-episode dedup (stop serial mowing)

**Change.** Treat a burst of `memory.events.high` increments as one *episode* with at most one kill. In `run_watcher`'s poll loop: after any `handle_breach` invocation, arm a cooldown (`EPISODE_COOLDOWN_MS`, default **30_000**); while the cooldown is live, keep updating `last_high` (so stale increments can't fire later) but skip `handle_breach` entirely — no kill, no record.

**Why skipping the record is safe for attribution.** `probe_kill_log_since` only needs *one* post-baseline record to classify the eventual exit, and the episode's first observation always writes one (any `KillStatus`). Suppressed observations within the cooldown add no attribution signal — they'd only bloat the JSONL at 1 line/sec.

**Why 30s.** After a SIGKILL lands, reclaim takes a few polls to drop usage below MemoryHigh, and `memory.events.high` keeps incrementing meanwhile — those increments are echoes of the breach we already acted on, not new demand. 30s comfortably covers reclaim lag while still letting genuinely sustained pressure claim a next victim within a minute. Constant lives next to the other four timing constants in both twins.

**Structure for testability.** The poll loop's fire/skip decision moves into a small pure gate — `struct BreachGate { last_fired_at: Option<Instant>, cooldown: Duration }` with `fn observe(&mut self, counter_grew: bool, now: Instant) -> bool` — unit-tested with synthetic instants (no sleeps). The loop calls the gate; `handle_breach` itself is unchanged this phase.

**Tests.**
- Gate unit tests: first increment fires; increments inside the cooldown don't; increment after expiry fires again; non-growth never fires regardless of gate state.
- Integration (existing fake-cgroup harness pattern in `session_watch.rs` tests — real dirs, hand-written `cgroup.procs` / `memory.events`): three rapid `high` bumps → exactly one kill record; bump, wait past cooldown (test-shortened constant via injected `Duration`), bump again → two records.

**Acceptance.** A session parked above its soft cap with N unprotected processes loses at most 1 process per 30s instead of 1 per second. Kill-log line volume during a sustained episode drops to ~1 line per episode.

## Phase 2 — Minimum-RSS victim floor (stop pointless collateral)

**Change.** In `handle_breach`'s selection scan, a candidate is *viable* only if `rss_kb >= floor_kb` where `floor_kb = max(MIN_VICTIM_RSS_KB, soft_cap_bytes / 20 / 1024)` — i.e. an absolute floor of **32_768 KB (32 MB)** or **5% of the soft cap**, whichever is larger. Below-floor PIDs are skipped exactly like protected ones for selection purposes (they remain visible to the scan for forensics).

**New terminal case.** Unprotected PIDs exist but none are viable → write a record with new `KillStatus::NoViableTarget` (wire: `"no_viable_target"`), surfacing the highest-RSS unprotected candidate's identity (pid/comm/argc/sha/rss) the same way the `Protected` case surfaces its best PID. No signals sent; the kernel MemoryMax is the remaining killer — which is the correct outcome: if nothing big enough to matter is killable, let the hard cap decide rather than shooting bystanders.

**Classifier updates** (`daemon/src/reaper.rs`):
- `is_cap_kill`: add `"no_viable_target"` to the `protected | no_pids | already_dead` row — SIGKILL exit + no operator kill → `true` (the kernel's MemoryMax kill after we declined to act *is* a cap kill), any other exit → `false`. The unknown-status → `false` fallback already handles old-binary/new-record skew conservatively.
- `kill_status_priority`: `"no_viable_target"` → priority 1 (same tier as `protected`/`no_pids` — an observation, not an action; a later `killed_by_us` in the same spawn must still win).

**Interplay with Phase 1.** The floor is what turns the cooldown from "mowing, but slower" into "no mowing": once the hog is gone, remaining small processes are non-viable, every subsequent episode records `no_viable_target`, and nothing else dies. This is exactly the transcript scenario (watchdog loops killed after the real consumer finished).

**Tests.**
- Selection: cgroup with one 2GB unprotected + one 5MB unprotected → 2GB picked (unchanged); hog exits, next breach with only the 5MB left → `no_viable_target` record, 5MB PID alive and named in the record; below-floor PIDs never picked even when they are the largest unprotected.
- Floor math: cap small enough that the absolute floor dominates; cap large enough that the 5% term dominates.
- `reaper.rs` decision-table rows: `no_viable_target` × {SIGKILL, SIGTERM, clean} × {operator, no-operator}; priority ordering vs `killed_by_us`.

**Acceptance.** No process below the floor is ever signaled by the watcher. A breach with no viable target produces a `no_viable_target` record and zero kills, and a subsequent kernel hard-cap kill of the session still classifies `memory_cap_kill: true`.

## Phase 3 — Deliver kill notices to the agent (end the signal-9 mystery)

**Change.** When the watcher actually kills (the `KilledByUs` path, immediately after `write_kill_log_to`), also drop a notice into the session's async-monitor inbox: `~/.cm/inbox/<session_uid>/<millis>-reap-<pid>.json`, body `{"text": "..."}` — byte-compatible with `mcp_server/async_monitor.py::_write_inbox`, so the existing cm Stop hook (`mcp_server/hooks/cm_stop_hook.py`) drains it at the next turn boundary with **zero hook or MCP changes**. Write is atomic (`.tmp` + rename), best-effort (failure logged, never blocks the kill path).

**Message text** (one line, self-contained, actionable):

    [cm-reaper] Killed pid 1234 (comm "python3", 2.1 GB RSS) in this session's process scope: memory soft cap (2.0 GB) was breached and this was the largest eligible process. If it was a background process you still need (monitor, watchdog, server), restart it — ideally under systemd-run --user or with CM_REAP_PROTECT=1 (see CLAUDE.md "On signal 9").

(The `CM_REAP_PROTECT` sentence ships in Phase 4's docs commit; until then the message ends at "restart it".)

**Plumbing.** The watcher twins get an `inbox_dir: Option<PathBuf>` input threaded the same way `kills_dir` already is (daemon: from config/home resolution at `spawn_watcher` call sites; TUI twin likewise). `None` (e.g. tests that don't care) disables notices. Directory creation routes through `ensure_dot_cm_subdir` for the `<uid>` subdir to match cm's 0700 posture — the Python side's plain `makedirs` tolerates pre-existing dirs, so there's no conflict whichever side creates it first.

**Scope and known limits** (accepted for v1, stated in the doc so nobody re-litigates them in review):
- Only `KilledByUs` notifies. `no_viable_target` / `protected` notices ("you're over cap and nothing can be killed") are a plausible extension but are warnings, not events — deferred to avoid nagging loops.
- Only claude-code sessions spawned with the cm Stop hook consume the inbox; codex/bash sessions leave the file unconsumed (harmless — `memory_kills` remains the forensic source of truth for them). No PTY-injection fallback from the Rust watcher: that machinery is MCP-server-resident and not worth duplicating for a notice.
- An idle (at-prompt) agent sees the notice only when its next turn ends. Acceptable: the common case — agent mid-task, its background process dies, next Stop fires within the same working session — is exactly the case that matters.

**Tests.**
- Writer unit tests (daemon): kill path produces a well-formed `{"text": ...}` file under the uid dir; filename pattern `<millis>-reap-<pid>.json`; write failure (unwritable dir) doesn't panic and the kill still proceeds; `inbox_dir: None` writes nothing.
- One integration assertion piggybacking the existing fake-cgroup kill test: after the SIGKILL, the inbox file exists and mentions the victim pid.
- Manual e2e (verification checklist, not CI): capped session, agent spawns a memory hog in background, breach fires, agent's next turn ends → hook injects the notice text as the follow-up instruction.

**Acceptance.** A cap kill inside a live claude-code session surfaces to that agent as a `[cm-reaper]` message at its next turn boundary, naming pid/comm/RSS and the cap. No behavioral change for sessions without the hook.

## Phase 4 — `CM_REAP_PROTECT=1` opt-out marker

**Change.** During `handle_breach`'s selection scan, read `/proc/<pid>/environ` (same-uid, always readable for session children) for each unprotected candidate and skip any whose environment contains the exact NUL-delimited entry `CM_REAP_PROTECT=1`. Agents self-serve with zero plumbing:

    CM_REAP_PROTECT=1 ./watchdog.sh &

The marker lives on the process itself — it survives daemon restarts, needs no registry, and is inherited by the marked process's children (env inheritance), which is the semantics a monitor wrapper wants.

**Guardrail — protection is advisory, not an immortality cloak:**
- Soft-cap only. MemoryMax/kernel OOM and session-teardown kills are untouched by the marker.
- Budget override: sum the RSS of marker-skipped candidates during the scan; if `marked_rss_kb > soft_cap_bytes / 2 / 1024` (half the cap), the markers are ignored for this episode and selection proceeds over the full unprotected set — a "protected" monitor fleet that *is* the memory problem still gets reaped. The resulting kill record carries an extra field `"protect_override": true` (the JSONL writer grows an optional flag; `probe_kill_log_since` parses records via `serde_json::Value`, so extra fields are ignored by old readers — forward-compatible by construction).
- Marked-but-below-floor processes need no special handling — the Phase 2 floor already excludes them before the marker is consulted; check the floor first, environ second (cheaper predicate first, and it avoids /proc reads for tiny PIDs).

**New terminal case.** Viable unprotected PIDs exist but all are marker-skipped (and under the override budget) → record new `KillStatus::MarkedProtected` (wire: `"marked_protected"`), surfacing the highest-RSS skipped PID, no signals. Classifier: join the `protected | no_pids | already_dead | no_viable_target` row in `is_cap_kill` (SIGKILL exit → the kernel killed a session we declined to police — still a cap kill) and priority 1 in `kill_status_priority`.

**No environ caching for v1.** Breaches are episodic (Phase 1: ≥30s apart) and cgroups hold tens of PIDs, not thousands; a fresh `/proc/<pid>/environ` read per candidate per episode is noise. (`/proc/<pid>/environ` reflects the execve-time environment, so a marker can't be dropped at runtime — acceptable: kill the process and restart unmarked if you change your mind.)

**Docs commit (part of this phase):** CLAUDE.md "On signal 9" section and AGENT_ORCHESTRATION.md gain the marker convention + the systemd-run alternative; Phase 3's notice text gains its final sentence.

**Tests.**
- Environ matching: exact `CM_REAP_PROTECT=1` entry matches; `CM_REAP_PROTECT=0`, `CM_REAP_PROTECT=1x`, and substring-inside-another-var do not (NUL-boundary parse, not string contains).
- Selection: marked hog + unmarked mid-size → unmarked picked even though smaller; all-viable-marked under budget → `marked_protected` record, nothing killed; marked set over half the cap → largest marked killed, record has `protect_override: true`.
- Classifier rows for `marked_protected`, mirroring Phase 2's.

**Acceptance.** A background process launched with `CM_REAP_PROTECT=1` survives soft-cap episodes (unless the marked set itself exceeds half the cap), and every skip/override is visible in the kill log.

---

## Rollout & verification

Order is 1 → 2 → 3 → 4 as phased above; each phase is independently shippable and independently revertable. Phases 1+2 change only victim selection (pure daemon/TUI Rust, no wire or schema changes); Phase 3 adds a write-only side channel consumed by an existing hook; Phase 4 adds two things Phase 2 already built the pattern for (a skip predicate + a terminal-case record).

Deploy per the standard daemon flow (local `cargo build --workspace` for the TUI twin; `cargo build --release -p cm-daemon` → scp → `systemctl restart cm-daemon` for cm-manager, which restarts live sessions — coordinate per DESIGN_SESSION_DURABILITY.md restore semantics).

End-to-end verification checklist (on a capped local session):
1. Spawn a fat background hog + a tiny background loop; drive memory over the soft cap. Expect: hog killed (one `killed_by_us` record), loop alive, `no_viable_target` records on subsequent episodes, no further kills — Phases 1+2.
2. Same run in a claude-code session: `[cm-reaper]` notice arrives at the next turn boundary — Phase 3.
3. Re-run with the hog launched under `CM_REAP_PROTECT=1` and a second unmarked mid-size process: unmarked one dies first; then push the marked set over half the cap and confirm the override record — Phase 4.
4. Kernel hard-cap sanity: park a session over MemoryMax, confirm the exit still classifies `memory_cap_kill: true` with the new record statuses in the log.
