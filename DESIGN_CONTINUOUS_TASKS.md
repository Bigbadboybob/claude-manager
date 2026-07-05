# Design: Continuous Tasks

> **Want to _create_ one, not understand the internals?** See **`HOWTO_CONTINUOUS_TASKS.md`** — the operator runbook (recipe, `continuous.create` params, prompt idioms, gotchas, review flow). This doc is the architecture.

**Status:** Phases 1–3 + 3b **implemented and committed** (`13139f5`, `2df9cff`, `40cec61`, `1288044`, `02dc80a`): sidebar + wire field + `kind` column; the `trigger` funnel + FRESH executor + `continuous.*` CRUD; the daemon scheduler + restart recovery + PERSISTENT executor; the stuck-story (completion signal + watchdog + investigator). Phases 1–3 validated end-to-end on a live daemon (smoke test). Phase 4 (queue) **implemented 2026-07-04** (see the Phase-4 entry + DESIGN_SCRAPER_MIGRATION.md §3 for the two naming/scope deltas). Phase 5 (migration) largely superseded by the live triage migrations; Phase 6 (fan-out/cloud) remains. Key review decisions resolved (§18).
**Provenance:** Synthesized from a 9-agent Ultracode design panel (4 recon → 3 competing architectures → judge → synthesis), then refined through review. Winning skeleton: the *trigger-API / extensibility-first* proposal, hardened with the *reliability-first* proposal's idempotency + restart-recovery + audit machinery and the *reuse-first* proposal's primitive-unification grafts.

---

## 1. Goal

Make **continuous tasks** a first-class concept in Claude Manager: long-lived automated units of work that run on a remote host (`cm-manager`), repeatedly — on a schedule and/or triggered by upstream events — producing output the user reviews (a worktree diff, a `NOTES.md`, or a report).

This migrates the user's existing automations (bug / scraper / trading-behavior triage, API-change watch, scraper generation) **off** the trader/aux instances and **into** Claude Manager, where they run as real, attachable Claude PTY sessions the user can read and re-drive — and opens the door to many new ones (nightly code review, code bug-hunt, code perf-hunt, top-source audit, momentum→scraper creation, auto-backtesting).

**Motivation:** the current headless harness (programmatic Claude Code) debugs worse than interactive Claude and gives no visibility. A continuous task spawns a *real* session on a real PTY that shows up in the sidebar with an idle white dot to review — fixing both problems at once.

---

## 2. Locked decisions

1. **Run-mode:** support **both** `fresh` and `persistent`, default **fresh**.
   - **Fresh** (default) — each trigger spawns a *new* session. The prior session is **left idle** for the user to review and dismiss with `A-x` (an idle session costs nothing) — it is **not** auto-killed in v1. Capping to the past *N* sessions is a configurable `retention.keep_sessions` policy (default = keep all), structured as an optional drop-in so it's an easy later addition, not a rework. Continuity across runs is a per-task `NOTES.md` the agent reads/writes.
   - **Persistent** — one live session, prompt delivered on each trigger, auto-compact after N runs.
2. **Scheduler:** **daemon-owned.** `cm-daemon` (Restart=always) owns a cron-like scheduler; each tick fires due tasks. Survives session death, compaction, and daemon restart. *Not* the agent self-scheduling via Claude's `/schedule` — that dies with the session. Scheduling is claude-manager-managed; `Periodic{every_secs}` + a phase offset covers the set, no cron crate until something needs wall-clock/timezone.
3. **Trigger API:** **free-form** `trigger(task_id, prompt?, args?)` is the default; the agent interprets the blob. A task **may optionally** declare named modes (sugar that pre-expands to the free-form prompt **before dispatch** — never the only path). Exposed via the daemon RPC, the MCP server, and the cloud API.
4. **Queue layer:** **`enqueue(queue, payload)`** is the buffered counterpart to `trigger`, for bursty/batched upstreams that `trigger`'s act-now model can't absorb. Substrate is **psql** (visibility), claude-manager-managed: a generic `cm_trigger_queue` table in the `claude_manager` DB on **cm-db** (not local on cm-manager). A `Schedule::Consumer{queue}` continuous task drains it in **batches**. See §9.
5. **Scope ceiling:** cm owns the **steps** (agent *or* bash) and the **queues between them** — it does **not** model the **pipeline**. A pipeline is an emergent group of queue-wired continuous tasks, not a first-class DAG object. Pipeline grouping/topology in the TUI is **deferred to a post-core open decision** (§18) and must not complicate the path to first implementation.

---

## 3. The core abstraction

> **A continuous task = a long-lived pinned worktree + a trigger surface (periodic and/or external/fan-out) + a run-mode (`fresh` | `persistent`) + a review surface (worktree diff / `NOTES.md` / report).**

Built around **two verbs** — `trigger(task_id, prompt?, args?, mode?)` (act now) and `enqueue(queue, payload)` (buffer for a batched Consumer) — and **two fan-out modes** (a synchronous daemon-enforced declared edge, and async queue-chaining). The cron scheduler, the cloud `POST`, the MCP tools, and the momentum→scraper case are all just **callers** of these. A new automation drops in as a task row of **config + a named Skill** — **zero new daemon code per kind**. The same infra is **engine-agnostic**: an agent step (Claude/codex) and a non-agent step (`engine: bash`) are the same primitive with a different "deliver work" branch.

## 4. The design seam

`cm-daemon` owns the **reliability-critical layer**: scheduling, supervision/auto-restart, idempotent triggers, the trigger/enqueue entry points, the visibility plane. The **user owns task logic**: the Skill, the per-task `NOTES.md`, what the agent SSHes to, what it writes to the worktree. *The daemon never learns what bug-hunt does — only how to fire it reliably, on schedule, with the trigger blob, and how to make output reviewable.*

---

## 5. Architecture overview

### Module layout — a sibling of `daemon/src/workflow/`

A new `daemon/src/continuous/` module mirrors the workflow module (the proven template — `WorkflowPoller` is already a Restart=always-resident background tick-thread that drives sessions, fresh-respawns participants, and delivers prompts):

| New file | Twin of | Role |
|---|---|---|
| `continuous/task.rs` | `workflow/run.rs` | The durable `ContinuousTask` record + persistence (flock + atomic tmp+rename + `validate_task_id` allowlist) |
| `continuous/scheduler.rs` | `workflow/poller.rs` | The tick loop + supervisor (`catch_unwind` per tick, chunked-sleep shutdown) |
| `continuous/runlog.rs` | `workflow/events.rs` | Append-only `runs.jsonl` audit (flock + symlink-reject) |
| `continuous/queue.rs` | — | The `cm_trigger_queue` accessor (enqueue / atomic batch-claim); see §9 for the access-path sub-decision |

**Rejected:** the *reuse-first* "a continuous task IS a single-role `WorkflowRun`." `WorkflowRun` is intrinsically multi-role (roles `BTreeMap`, `active_role`, `on_idle`/transition machinery, `iteration`-keyed idempotency races); overloading it is conceptual debt. A **sibling record** is the correct structural call.

### The funnel

```
methods::trigger(state, caller, TriggerParams { task_id, prompt?, args?, mode? })
```
in `daemon/src/control/methods.rs`, registered as a `"trigger"` arm in `dispatch_request` and added to `DAEMON_METHODS` in `control_client.py` (the `DaemonMethodsAlignment` test pins the two together).

**Resolution order — all sugar collapses to free-form before dispatch:** `prompt` if present → else expand `config.modes[mode]` → else `config.default_prompt`. `args` is merged into the final blob the agent reads; the daemon does **not** parse it. The handler then: validate `task_id` → `load_one()` → `fire_token` idempotency check → set `in_flight` → branch on the **task's** `run_mode` (callers never choose the mode) → spawn/deliver → append a `runs.jsonl` line.

### Wiring

`daemon/src/lib.rs::run()` constructs `ContinuousScheduler::new(state).start()` immediately after `WorkflowPoller::start()` (~L516), same **FATAL-on-spawn-failure** posture, and `scheduler.shutdown()` beside `poller.shutdown()` (~L557). **Local-first is not regressed:** zero `state.json` files on disk = an empty `readdir` per tick and nothing else.

---

## 6. Data model

Authoritative copy is **daemon disk**; the planning row and queue table are mirrors/edges.

1. **Planning task row (thin mirror).** Config under the existing `metadata` JSONB bag (`sql/010_task_metadata.sql`) in a `continuous` namespace:
   ```jsonc
   metadata.continuous = {
     run_mode, engine: "claude"|"codex"|"bash",
     schedule: { kind: "periodic"|"on_demand"|"consumer", every_secs?, queue?, batch_max?, window_secs?, depth_threshold? },
     skill?, default_prompt, modes?, downstream?, enqueue_to?,        // enqueue_to = downstream queue for chaining
     compact_every?, review_surface, host,
     retention?: { keep_sessions?: N, transcript_days?: 30 }
   }
   ```
   *Gotcha:* a `metadata` PATCH replaces the whole object — writers read-modify-write the full dict.

2. **`kind` column — DONE (Phase 1, `sql/011`).** Idempotent `ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'oneshot'`. **Sole reason:** the legacy cloud `dispatch_daemon` claims `status='backlog' AND is_cloud=true AND project IS NULL`; `claim_next_task` / `count_dispatchable` now exclude `kind='continuous'`. `kind` is surfaced in `api/models.py` + the MCP shape.

3. **`ContinuousTask` (authoritative)** — `daemon/src/continuous/task.rs` at `~/.cm/continuous-tasks/<id>/state.json`:
   ```
   task_id, label, project, host_id (pin to cm-manager), repo, workspace_id, worktree_path,
   engine: { Claude, Codex, Bash } (default Claude),
   run_mode: { Fresh, Persistent } (default Fresh),
   schedule: { OnDemand, Periodic{every_secs}, Consumer{queue, batch_max, window_secs, depth_threshold}, Cron(String) },
   next_fire_at: u64,            // persisted reliability anchor — absolute epoch, O(1) compare
   last_fired_at, skill, default_prompt, modes: Map<String, ModePreset>,
   current_session_uid,          // durable swappable slot; identity is task_id, uid swaps per fresh tick
   run_count, compact_every, enabled, paused, supervise,
   max_runtime_secs: Option<u32>,   // fresh hang-watchdog (§11)
   in_flight: Option<InFlight{ fire_token, session_uid, started_at }>,
   last_run: Option<RunRecord{ seq, fire_token, started_at, finished_at, session_uid,
                               status: Pending|Running|Idle|Done|Failed|Stuck|Orphaned, trigger_source }>,
   consecutive_failures: u32,
   downstream: Vec<String>,      // declared synchronous fan-out allowlist
   enqueue_to: Option<String>,   // downstream queue for async chaining
   retention: Retention{ keep_sessions: Option<u32>, transcript_days: u32 },  // default keep_sessions=None (keep all), 30d
   ```
   Persistence reuses `run.rs` verbatim (flock, atomic tmp+rename, `load_one`/`load_all`/`modify`, `validate_task_id`) + `runs.jsonl`. **No cron crate** until `Cron` is actually needed.

4. **`cm_trigger_queue` (the edge)** — psql, in `claude_manager`@cm-db (§9).

---

## 7. Daemon scheduler

`ContinuousScheduler` in `continuous/scheduler.rs` — a structural twin of `WorkflowPoller`: a named `cm-continuous-scheduler` thread running `run_loop { while !shutdown { catch_unwind(tick_once); chunked-sleep } }`. The ~250ms tick is the **scan** granularity, never the schedule resolution.

`tick_once` phases (mirrors `poll_once`'s **collect-under-lock / drop-lock / act** discipline — never hold the `DaemonState` mutex across a spawn or PTY write; the reaper's `on_exit` re-acquires it):

1. **Restart reconciliation** (first tick, cheap thereafter): any task with `in_flight` set but `session_uid` absent / `last_exit` → `last_run.status = Orphaned`, append orphan line, clear `in_flight`.
2. **Load:** `ContinuousTask::load_all()` (disk is the authority). Maintain a `BinaryHeap<(next_fire_at, task_id)>` due-index rebuilt on change.
3. **Supervision pass** (before due check): dead `Persistent supervise=true` sessions respawn; `Fresh` runs past `max_runtime_secs` with no completion signal escalate to the stuck-task path (§11).
4. **Due check:** snapshot the due set under the lock — `Periodic` (`next_fire_at <= now`) **and** `Consumer` (queue has unclaimed rows past `window_secs` OR `depth_threshold`) — then **drop the lock**.
5. **Fire:** call `methods::trigger` in-process with an internal Operator caller + a fresh `fire_token` (Consumer fires claim a batch first; §9).
6. **Advance:** `modify()` — `last_fired_at = now`, `next_fire_at = now + every_secs` (**catch-up-once:** recompute from now, never backfill missed slots; a per-task `backfill` flag is the documented later escape hatch), append the fire line; exponential-capped back-off on `consecutive_failures`.

**Restart recovery is free:** `state.json` + `next_fire_at` on disk; an overdue `next_fire_at` fires once on the next tick. `catch_unwind` keeps one panicking task from killing the daemon. A `daemon.toml [scheduler]` section (additive `#[serde(default)]`): `enabled`, tick interval, `max_worktrees` disk guard, `default_cap` (≈1 GB memory cap for the headless spawn path, per-task overridable).

---

## 8. Trigger API — one verb, three callers, one handler

**Daemon RPC:**
```jsonc
trigger { "task_id", "prompt"?, "args"?, "mode"? }
// → { "fired": true, "fire_token", "session_uid", "run_mode" }  |  { "fired": false, "reason": "duplicate_fire_token"|"busy"|"paused" }
```
Operator + Session callable via a `dispatch_trigger` wrapper (like `start_workflow`'s).

**Continuous CRUD** (operator-gated via `require_operator`): `continuous.create {...}` (creates the worktree **once**, `created=false` on collision; registers `state.workspaces`+`state.task_workspaces`; mirrors `metadata.continuous`), `continuous.list` (the health read), `continuous.pause`, `continuous.run_now` (manual fire), `continuous.delete { gc? }`.

**MCP** (`mcp_server/server.py` + `/opt/cm-daemon` copy + predictionTrading copy): `trigger`, `enqueue`, and `continuous.*` added to `DAEMON_METHODS` in `control_client.py` (routes to `~/.cm/daemon.sock`).

**Cloud API** (`api/main.py`): `POST /tasks/{id}/trigger` forwards over ssh-unix to cm-manager's daemon as an Operator-caller `trigger`.

**Auth is bimodal:** Operator for the cron tick / cloud POST / TUI; Session (descendant- or downstream-allowlist-scoped) for agent fan-out (§12).

---

## 9. Queue layer (`enqueue` + Consumer + chaining)

The buffered counterpart to `trigger`, for upstreams that are bursty or that you want to **batch** (the trigger-acts-instantly model breaks under bursts).

**`enqueue(queue, payload)`** — cheap, idempotent insert into a generic table. Bursts are just rows.
```sql
CREATE TABLE cm_trigger_queue (
  id BIGSERIAL PRIMARY KEY,
  queue       TEXT NOT NULL,            -- serves momentum AND future producers
  created_at  TIMESTAMPTZ DEFAULT now(),
  payload     JSONB NOT NULL,
  dedupe_key  TEXT,                     -- optional coalescing of duplicate events
  claimed_at  TIMESTAMPTZ,             -- NULL = pending
  batch_id    TEXT,
  status      TEXT DEFAULT 'pending'   -- pending|claimed|done|failed
);
```
**Atomic batch claim** (standard work-queue idiom, safe under concurrent consumers):
```sql
UPDATE cm_trigger_queue SET claimed_at=now(), batch_id=$1, status='claimed'
WHERE id IN (SELECT id FROM cm_trigger_queue WHERE queue=$2 AND claimed_at IS NULL
             ORDER BY created_at LIMIT $batch_max FOR UPDATE SKIP LOCKED)
RETURNING *;
```

**`Schedule::Consumer{queue, batch_max, window_secs, depth_threshold}`** — the scheduler fires the consumer when **either** `window_secs` elapsed **or** depth ≥ `depth_threshold` (whichever first), claims a batch, and delivers the batch as the args blob to one session (dedupe + prioritize in-task). Bounds latency (window) and resource use (batch cap).

**Queue chaining** — a task with `enqueue_to` set inserts its results into a downstream queue at end-of-run (Queue1 → TaskA → Queue2 → TaskB). The async complement to the synchronous declared-edge `trigger`.

**Substrate & location (locked: option b).** psql, in `claude_manager`@cm-db — *not* the task backlog (`propose_task` would create one task-row per event, the granularity we're avoiding). Visibility is the win: `SELECT` the backlog, see pending/claimed/failed.

**Access-path sub-decision (queue phase, not a blocker):** the daemon doesn't talk psql directly today (it reaches the DB via the API). Default plan: the **API owns the table** (reusing `dispatch/db.py` patterns) and exposes enqueue/claim endpoints the daemon calls — both are co-located on cm-manager. Alternative: give the daemon a direct psql client for the queue. Decide when building the queue phase.

---

## 10. Run-mode executors

The mode is a **task property** (`ContinuousTask.run_mode`); callers never pick it. The "deliver work" step branches on `engine` (agent: type a prompt via `spawn_agent_prompt_delivery`; bash: pass the batch/args via argv/env/stdin).

**FRESH (default, white-dot-per-run):** spawn a *new* session per trigger and **leave prior session(s) idle** for the user to review/dismiss (`A-x`) — **no auto-retire in v1**. Mint a new uid (`ts-<hex>-<hex>`), compose params via `compose_continuous_spawn_params` over `compose_daemon_spawn_params` (pinned to the durable worktree, tagged `continuous_task_id`), call the existing **`start_session` pipeline** (not `PendingSession::spawn` — preserves two-phase race-safety, lock-drop-before-spawn, `claude_trust` pretrust, env injection, registry insert, `ManifestDiff::Added`), then deliver the resolved prompt. Continuity is the per-task `NOTES.md`.

> **Optional retention/prune step (configurable, easy later add).** When `retention.keep_sessions = N` is set, after spawning the new session the executor retires the oldest idle fresh session(s) beyond N via **`kill_session` semantics** — `mark_operator_kill_requested()` + `session.kill()` while **leaving the entry in the registry** so the reaper broadcasts `ManifestDiff::Exited` + a tombstone (a bare `state.sessions.remove()` SIGKILLs via `Drop` but emits NO `Exited`, so a remote observer never sees the prune). v1 default is keep-all (the step is a no-op); the machinery being in place is what makes capping a config change, not a rework.

**PERSISTENT (one live session):** resolve `state.sessions.get(current_session_uid).input_handle()`, clone it out, **drop the state lock**, then deliver the prompt via a dedicated `spawn_persistent_prompt_delivery` detached thread that mirrors the FRESH path's `spawn_agent_prompt_delivery` — same kitty-Enter mechanics (`AGENT_KITTY_ENTER` / `agent_paste_payload` / settle+gap). On `run_count % compact_every == 0` it prepends an **inline `/clear`** (same hardcoded kitty-Enter) to keep the **same uid/PTY** — deliberately **not** `fresh_reset::reset_fresh_role`/`PtyModeTracker`, because a tracker attached mid-session hasn't observed the agent's startup kitty/bracketed-paste escapes and would mis-detect raw `\r` mode, so `/clear` wouldn't submit. *(Implemented in Phase 3; this is the consistent-with-FRESH delivery, not the poller's finalize drainer.)* Guard: a dead/`None` `input_handle()` promotes to a fresh respawn.

**Worktree discipline:** `create_worktree` once at `continuous.create`, reused every tick (`created=false`) — **never per trigger** (the disk-growth bound). `claude_trust` auto-pretrusts claude-engine tasks; codex/bash are the un-pretrusted exception.

---

## 11. Supervision & failure surfacing

The scheduler's `tick_once` doubles as supervisor — reliability lives in the Restart=always daemon, not the agent.

**Auto-restart of dead persistent sessions:** probe `current_session_uid` (`try_wait` / absent / `last_exit`); if dead and not paused, respawn through `start_session` with a NOTES-re-read prompt + a `runs.jsonl {event:"supervised_restart"}` line.

**Completion signal + stuck-task escalation (resolved in review).** A fresh run should *confirm it finished* — cleanest: the skill calls a small `report_done` MCP (or idle-after-output = done) — so we distinguish "still working" from "wedged." When a fresh run exceeds `max_runtime_secs` with no completion signal:
1. **Snapshot debug artifacts** first — copy the wedged session's transcript, last PTY screen, and `NOTES.md` into the run dir so nothing is lost.
2. **Spawn an investigator agent** that inspects the stuck run and picks a bounded action via an MCP call: `mark_unstuck` (reset the runtime timer — it's actually progressing), `restart` (retire + fresh spawn), or `escalate` (kill + raise to the user via the alert/notify path). Capped attempts so a pathological task can't loop.

This turns a silent hang into a self-heal or a clean escalation. (Phase 3+; rides on the completion signal.)

**Three durable failure-surfacing layers:**
1. `runs.jsonl` — append-only (flock + symlink-reject), one line per fire/exit/respawn/orphan/stuck.
2. `last_run.status` in `state.json` (`Pending→Running→Done`/`Failed`/`Stuck`/`Orphaned`).
3. Live broadcast on `manifest.watch` (`ManifestDiff::Added/Exited` carry `continuous_task_id`).

**Critical discipline:** `manifest.watch` is **lossy** (bounded 32-slot `try_send`) — a reconnecting remote TUI rebuilds from a `continuous.list` / `state.json` **snapshot**, never replay. Observability the user *sees*: green spinner (running), white dot (idle/reviewable), red glyph (`Failed`/`Stuck`). **Still deferred:** a speculative `schedule_watcher` live broadcaster (over-built for v1).

---

## 12. Fan-out — two modes

**(a) Synchronous declared edge** — for low-rate, act-now fan-out. The `trigger` method is Session-callable; when `caller == Caller::Session`, the daemon authorizes the target if **either** it is self-or-descendant of the caller's task (`auth::task_is_self_or_descendant_of`) **or** the target appears in the **caller task's** `downstream` allowlist. Edges are **declared config validated by the reliability layer**; adding one is a one-line config change. **A missing/typo'd `downstream` entry returns a clear, distinct error** — never a silent drop.

**(b) Async queue-chaining** — for bursty/batched fan-out. The upstream `enqueue`s into a downstream queue (`enqueue_to`); a `Consumer` task drains it in batches (§9). This is the right mode whenever instant downstream action isn't guaranteed.

**Not used for headless fan-out:** `create_subtask` / `send_input` route to `tui.sock` (a headless cm-manager has no TUI). Daemon-routed primitives: `trigger`, `enqueue`, `propose_task` (new backlog row via `planning_client.rs`), `start_session`/`start_workflow`.

**Proving cases:** keep a *simple, low-rate* synchronous `trigger` edge as the spine proving case. The flagship **momentum → scraper** flow uses **mode (b)**: momentum events are bursty and want batching, so the producer `enqueue`s filtered events and a Consumer drains them — see §16.

---

## 13. Non-agent pipeline steps & pipeline grouping

Decomposed in review into three questions; the scope ceiling is decision #5.

- **Observe non-agent steps (yes).** A non-agent step (e.g. the pure-Python `sourceFilter`/originality filter) is represented in cm either as an `engine: bash` continuous task (cm-spawned, attachable bash PTY for logs) **or** as a bash **log-tail** of an externally-supervised systemd service (`journalctl -fu <svc>`). The common case (resolved): **systemd keeps the lifecycle, cm tails the logs** — observability without cm owning supervision. Don't double-own lifecycle.
- **Run/supervise non-agent steps (à la carte).** A *periodic* non-agent step is a clean `engine: bash` Consumer (cm schedules + claims a batch + invokes the script). A *long-running daemon* stays on systemd with cm tailing — unless you explicitly want cm to own it (persistent-bash + `supervise`).
- **Model the pipeline (NO / deferred).** No `Pipeline`/DAG object. The queue is the edge; queue depth is the backpressure signal; a `pipeline:<name>` group tag is the grouping. **Pipeline grouping + topology display in the TUI is a deferred open decision** (§18) — the user wants grouping *and*, because flows aren't fully linear, eventual topology, which is where complexity grows. Revisit after core implementation; do not build now.

This delivers the observability wins (attachable bash logs, eventual grouped collapsible section, queue depth) at small marginal cost — `engine: bash` is incremental on the engine-agnostic infra, and grouping reuses the §14 continuous-section machinery — while refusing the orchestration-layer bloat.

---

## 14. Sidebar UX

Reuse the existing host-grouping machinery; **one** wire-shape change.

**Wire field — DONE (Phase 1).** `continuous_task_id: Option<String>` threaded `StartSessionParams` → `SpawnParams` → `DaemonSession` → `ManifestDiff::Added` → `ManifestEntry` (serde-default) → `TerminalSession`, *and* read through the Added-diff adoption path (`adopt_daemon_workflow_participant_on_host`) so a Phase-2 adopted/remote continuous session keeps its tag.

**Continuous section — DONE (Phase 1).** `VisualItem::ContinuousHeader` (non-selectable, `continuous (N)` count badge); all three builders (`visual_items_status`, `visual_items_status_multihost`, `visual_items_task`) partitioned in lockstep with continuous sessions sorted to the bottom; the four exhaustive-match touch sites handled. `A-c` hides/shows the section, persisted in the TUI session manifest like `view`.

**Remote indicator — DONE.** Magenta `@<host>` tag on the `WorkspaceHeader`, keyed off `ts.host_id != HostId::local()` (`ab8f87f`, on `main`). Optional per-session badge deferred. *Do not conflate `host_id` (ssh-unix) with the legacy `is_cloud` GCP flag.*

**Known caveat (carried):** the sidebar is non-scrollable and caps at `list_height`; with many idle fresh sessions accumulating (no-auto-kill), a bottom-sorted continuous section can scroll off-screen. `A-c` + the count badge mitigate; reserving rows / above-the-fold is a follow-up — and another reason `retention.keep_sessions` will be wanted.

**Hard dependency:** running *live* on cm-manager over ssh-unix depends on **lifting `guard_local_host_only()`** (`app.rs:562`, the daemon-side remote-path-resolution work). Until then, remote continuous sessions are restore-skipped, not live.

---

## 15. Skills pattern

The task logic is a **Skill**; the daemon owns none of it. `ContinuousTask.skill` names a Skill (e.g. `bug-hunt`, already one; `scraper-workflow`, already one). The `default_prompt` is a thin template (*"Run skill `<X>` with args `<blob>`. Read `NOTES.md` first; append findings; write reports to `./reports/`."*) delivered to the spawned (fresh) or live (persistent) PTY.

Because the trigger spawns a **real** Claude PTY on cm-manager, each automation is interactively debuggable: the user `A-a` attaches and watches / re-drives the **same** skill the scheduler runs. **No skill-runner abstraction** — the skill is just the prompt the existing delivery path delivers. A new automation == config + a reusable Skill + a one-line prompt.

> **MUST signal completion (verified on cm-manager 2026-06-27).** A periodic FRESH task's prompt **must** end by calling **`report_done`** (or the session must exit) — otherwise Phase 3b's due-skip-active sees `last_run.status == Running` forever and **never re-fires** (a never-completing run blocks the schedule, which is correct: no pile-up). The deploy+verify smoke proved this: a bash task whose session just idled fired *once*; the same task completed each run (exit → `Done`) fired every 3s as expected. So the `default_prompt` template should append *"…then call `report_done` to signal you're finished."*, and migrated skills (triage, etc.) must call it. **Open follow-up for `engine: bash`:** a bash step should likely run-and-exit (the prompt as `argv`) rather than interactive-bash + typed-prompt, so it exits cleanly per run.

---

## 16. Migration plan

| Existing automation | New form | Output | Notes |
|---|---|---|---|
| **bug triage** (trader) | continuous, `fresh` + periodic | `NOTES.md` + worktree diff | skill SSHes into trader for local data; review worktree, merge manually (**no PR**) |
| **scraper triage** (trader) | same shape | worktree | SSH-to-trader |
| **trading-behavior triage** (trader) | same shape | worktree | SSH-to-trader |
| **daily signal review** (email) | **KILLED** | — | signal-stats page covers it; no task created |
| **API-change watch** (email) | continuous, `fresh` + weekly (`every_secs=604800`) | report file in worktree | report-at-end, **no email** |
| **scraper generation** (aux) | **DEFERRED — cross-project migration, its own sub-design** | — | re-arch below; build the generic core (queue + Consumer + engine:bash + fan-out) that makes it config + skills |
| **momentum detection** (NEW) | producer `enqueue`s filtered events | feeds the chain | see below |

**Scraper-gen re-architecture (deferred, needs predictionTrading context).** Today it's an in-process asyncio pipeline on aux: `momentum_events (cursor) → Merger (Serper+Perplexity+Gemini attribution, RAG) → Filter → OriginalityFilter → [Telegram HumanReview] → Creator (headless Claude + scraper-workflow skill)`. Target — a *visible, restartable* version where each port becomes a durable queue and each headless service becomes an attachable session:
- The cheap **filter** stage (`engine: bash`, or kept external on the trader) **`enqueue`s filtered momentum events** → **Queue 1** (`cm_trigger_queue`, claude-manager-managed).
- A **`fresh`/no-kill Claude Consumer** drains Queue 1 in batches: agentic websearch + decide which sources to act on (replacing the single-Serper/Perplexity+Gemini attribution **and** the Telegram human gate — taking the user out of the loop) → `enqueue_to` **Queue 2** (proposed scraper creations).
- A second **Claude task** drains Queue 2 and creates the scrapers — *largely the existing `Creator`*, since it's already a headless Claude + `scraper-workflow` skill; the migration is mostly "run it as a visible continuous task."

The generic core makes all three a config + skill drop-in. The producer wiring, the agentic-websearch skill, and decommissioning the in-process Merger/Filter/HumanReview are the deferred cross-project work.

All live cm-manager migrations are gated on lifting `guard_local_host_only()`.

---

## 17. Roadmap

**Phase 1 — Sidebar org + wire field + `kind` column** *(M)* — **DONE, committed `13139f5`.** `continuous_task_id` threaded end-to-end (+ adoption path); `ContinuousHeader` section in all three builders; `A-c` toggle (persisted); `sql/011` kind column + GCP-dispatcher exclusion + api/mcp shaping. Plumbing only — nothing sets the tag yet.

**Phase 2 — The trigger funnel** *(L)* — `trigger` verb end-to-end, **manual-only**.
- `methods::trigger` (prompt/mode/default → free-form); `dispatch_request` arm (Operator+Session) + `DAEMON_METHODS` + MCP tool in all three copies.
- `compose_continuous_spawn_params` + FRESH executor (**no auto-kill**; `start_session` funnel; `spawn_agent_prompt_delivery`).
- `fire_token` idempotency + `in_flight` guard + `runs.jsonl` (`continuous/task.rs` twin of `run.rs`); `continuous.create/list/pause/run_now/delete`; worktree pinned once.

**Phase 3 — Scheduler + restart recovery + supervision** *(L)* — **DONE, committed `40cec61`** (except the stuck-story → Phase 3b).
- `ContinuousScheduler` wired FATAL into `lib.rs::run`; `tick_once` (load_all, `Periodic` fire, catch-up-once, per-fire panic isolation); restart orphan-reconciliation (closes the Phase-2 `in_flight` residual). *(BinaryHeap due-index deferred — linear scan is fine at current N.)*
- PERSISTENT executor + supervision (dead-persistent respawn, `consecutive_failures` backoff); auto-compact-after-N.
- `daemon.toml [scheduler]` (`enabled`, tick, `max_worktrees`, `default_cap` ≈1 GB + per-task `mem_cap_bytes`).
**Phase 3b — Stuck-story** *(L)* — **DONE, committed `02dc80a`.** `report_done` completion signal + clean-exit→Done; due-skip-active (no fresh-run pile-up); FRESH-only watchdog (Running past `max_runtime_secs` + live session → snapshot + investigator, capped by `max_investigations` → auto-escalate); investigator agent + `resolve_stuck` (`mark_unstuck`/`restart`/`escalate`); the investigator's own runtime is bounded (`abandon_timed_out_investigator`). Escalate surfaces via `last_run=Stuck` (red glyph) + the `Exited` broadcast + `runs.jsonl`, not an active push.

**Phase 4 — Queue layer** *(M–L)* — **DONE (2026-07-04; spec + deltas in DESIGN_SCRAPER_MIGRATION.md §3).** Two deltas from the sketch: the table shipped as **`queue_items`** (sql/012, api-owned — matching the no-prefix table convention) rather than `cm_trigger_queue`, and **`enqueue_to` chaining stayed inert** (the first producers are agents → the MCP `enqueue` tool; end-of-run auto-chaining remains Phase 6). Shipped: `queue_items` + burst-coalescing `dedupe_key` partial unique index (pending/claimed only); API `POST /queues/{q}/items` / `GET stats` / `claim` (`FOR UPDATE SKIP LOCKED`) / `ack` / `requeue`; daemon `continuous/queue.rs` (ureq, planning-cred chain); scheduler Consumer due-check (depth ≥ threshold OR window elapsed; depth polls cached 30s; 60s not-before spacing) with the run-active gate in BOTH run modes (a Consumer prompt must end in `report_done`, auto-appended); `trigger` claim → stage `<worktree>/.queue/batch-<seq>.json` → deliver → ack, with release-on-failure (claimed-at-fire semantics); `enqueue` + `queue.stats` RPC (bimodal) + MCP tools.

**Phase 5 — Migrate automations + non-agent steps** *(M)* — **gated on the guard lift.** Triage tasks (fresh+periodic, SSH-to-trader, worktree); API-watch (fresh+weekly); kill daily-signal-review; verify restart-mid-run e2e. `engine: bash` continuous tasks + non-agent log-tail observe.

**Phase 6 — Fan-out + cloud surface + future tasks** *(M)* — declared-edge auth + queue-chaining; `POST /tasks/{id}/trigger`; transcript-30d GC; new automations as pure config (code-review, code-bug-hunt, code-perf-hunt, top-source-audit, auto-backtesting).

**Deferred (post-core open decisions):** pipeline grouping + topology display (§13/§18); the scraper-gen cross-project migration (§16, its own sub-design); per-session remote badge; `retention.keep_sessions` auto-prune (machinery designed, default off).

---

## 18. Resolved decisions & open questions

**Resolved in review** (baked into the design above):
- Fresh = no auto-kill, idle-for-`A-x`, `retention.keep_sessions` configurable (default keep-all) → §2/§10.
- Completion signal + max-runtime watchdog + investigator-agent (`mark_unstuck`/`restart`/`escalate`) → §11.
- Catch-up: once-then-forward default, per-task `backfill` flag as the later escape hatch → §7.
- Memory cap: `default_cap` ≈ 1 GB, per-task overridable → §7.
- GC: low priority; worktrees stay until manual cleanup; transcripts pruned at 30 days → §6/§17.
- Scheduling claude-manager-managed; `every_secs` + phase offset, no cron crate yet → §2.
- Queue substrate: psql, claude-manager-managed `cm_trigger_queue` in `claude_manager`@cm-db (fork **b**) → §9.
- Non-agent steps: systemd-owned lifecycle + cm bash-tail observe; `engine: bash` for periodic → §13.

**Still open:**
1. **Pipeline grouping + topology** — grouping sessions by pipeline is wanted; flows aren't fully linear, so topology visualization is the complex part. **Deferred to a post-core decision** (§13). Don't build now.
2. **Queue access path** — API-owned table + endpoints the daemon calls (default) vs direct psql from the daemon (§9). Decide at the queue phase.
3. **Scraper-gen migration specifics** — producer wiring into Queue 1, the agentic-websearch skill, Queue-1/2 schemas, decommissioning the in-process stages. Cross-project; its own sub-design (§16).
4. ~~**Investigator-agent details**~~ — RESOLVED in Phase 3b (`02dc80a`): built-in daemon-constructed prompt (not a skill); actions `mark_unstuck`/`restart`/`escalate`; capped by `max_investigations` (default 2) → auto-escalate; the investigator is itself runtime-bounded.

---

## 19. Risks (standing, not optional)

- **cm-manager disk growth** — many long-lived worktrees + accumulating idle fresh sessions (no-auto-kill) + transcripts + run dirs + `runs.jsonl`. Mitigation: one durable worktree per task (reuse on collision), `runs.jsonl` rotation, 30-day transcript GC, `max_worktrees` guard, and (later) `retention.keep_sessions`.
- **Uncapped headless spawns** — `compose_daemon_spawn_params` sets no memory-cap triple; a runaway skill could OOM the VM. Mitigation: `[scheduler] default_cap` ≈1 GB, per-task.
- **Concurrent-trigger idempotency** — cloud POST + cron tick + MCP fan-out racing one task. Mitigation: exclusive flock on `state.json.lock` (sentinel), `fire_token` dedup, `in_flight` guard.
- **Queue backlog / consumer lag** — a Consumer that can't keep up grows the queue unboundedly. Mitigation: `depth_threshold` fires sooner under load; `continuous.list` / queue `SELECT` surfaces depth; alert on a depth ceiling. Batch-claim correctness rests on `FOR UPDATE SKIP LOCKED`.
- **Lock discipline** — a tick holding `DaemonState` across a spawn/PTY write deadlocks (the reaper re-acquires it). Mirror collect-snapshot-then-act; funnel through `start_session`.
- **Per-tick disk scan** — `load_all()` every ~250ms scales poorly. Mitigation: `BinaryHeap<next_fire_at>` due-index + O(1) compare.
- **Kitty-Enter / bracketed-paste** — a bare `\n` won't submit Claude/codex. Reuse `spawn_agent_prompt_delivery` / the poller finalize drainer (agent engine only; bash gets argv/stdin).
- **Fan-out auth escalation** — a Session-caller `trigger` to a non-descendant must be gated by the `downstream` allowlist; missing/typo'd entry → clear error, not silent drop.
- **Fresh prune (when enabled) must use `kill_session` semantics** — a bare `sessions.remove()` SIGKILLs but emits no `Exited`/tombstone, so a remote observer never sees the prune. Use `mark_operator_kill_requested()` + `session.kill()`.
- **MCP triple-copy drift** — `trigger`/`enqueue`/`continuous.*` must land in `mcp_server/server.py`, `/opt/cm-daemon/mcp_server`, **and** `predictionTrading/scripts/mcp`; cm-manager needs scp + `systemctl restart cm-daemon`. The `DaemonMethodsAlignment` test pins `dispatch.rs` ↔ `DAEMON_METHODS`.
- **Legacy GCP dispatcher collision** — `kind='continuous'` excluded from `claim_next_task`/`count_dispatchable` (done, `sql/011`).
- **Hard dependency on lifting `guard_local_host_only()`** — until daemon-side remote-path-resolution ships, cm-manager continuous sessions are restore-skipped; the migration can't run live.
- **`claude_trust` only covers claude** (+ systemd-run-wrapped claude); a codex/bash task in a fresh worktree isn't auto-trusted and could hang on a trust dialog.

---

## Appendix — recon anchors

- `daemon/src/workflow/poller.rs` — `WorkflowPoller`, the background-thread template.
- `daemon/src/control/methods.rs` — `start_session` (the spawn choke point), `compose_daemon_spawn_params`, `create_session`, `kill_session`, `mcp_start_session` + `spawn_agent_prompt_delivery`, `start_workflow`.
- `daemon/src/session.rs` — `PendingSession::spawn`, `DaemonSession`, `InputHandle::write_and_stamp`, `kill()`.
- `daemon/src/workflow/{run.rs,fresh_reset.rs,events.rs}` — persistence + fresh-reset + audit twins.
- `daemon/src/manifest.rs` — `ManifestWatcher`, `ManifestDiff`, `ManifestEntry` (visibility plane).
- `daemon/src/claude_trust.rs` — `maybe_pretrust_for_spawn`.
- `daemon/src/worktree.rs` — `create_worktree` (reuse-on-collision), `resolve_repo`.
- `daemon/src/{lib.rs,state.rs,control/dispatch.rs}` — wiring, `DaemonState`, RPC routing + `require_operator`.
- `dispatch/db.py` — `claim_next_task` / `count_dispatchable` (queue-claim + kind exclusion patterns to mirror for `cm_trigger_queue`).
- Scraper-gen: `~/code/projects/predictionTrading/applications/scraperGeneration/` — `scraperGeneration.py` (orchestrator), `eventPoller.py` (cursor poll), `data/SCRAPER_CREATION.WORKFLOW.md` (the existing Creator skill prompt).
