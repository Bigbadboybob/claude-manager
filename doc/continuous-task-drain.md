# Graceful drain for continuous tasks

Status: implementation started locally, 2026-09-06; not deployed. Companion to the [Codex migration plan](continuous-codex-migration.md). This document owns the drain contract; the migration plan owns cutover order and rollout gates. No production task has been paused, restarted or migrated by this implementation work.

## Implementation checkpoint

The admission foundation is implemented in [`continuous/drain.rs`](../daemon/src/continuous/drain.rs), the [RPC handlers](../daemon/src/control/methods.rs), and the [scheduler](../daemon/src/continuous/scheduler.rs):

- Operator-only `continuous.drain {task_id}` persists a request identity, pause and admitted fire/run/UID. Repeated requests reuse it; plain pause preserves it; explicit resume clears it. `continuous.list` includes `drain` and `drain_status`.
- Fire admission rechecks pause/drain under the existing task flock. A persisted admission revision prevents a fire prepared before a stop/resume from landing afterward. The gate precedes claim, spawn, compaction and delivery.
- A separate per-task lifecycle flock serializes pause/drain with watchdog and account-recovery interventions. The watchdog reloads its snapshot under this guard; recovery checks the live pause/drain/run/block before replay or retirement. Late investigator restart/escalation verdicts are refused during drain.
- Account diagnostics continue during drain. A completed/unanswered turn without completion records a drain blocker; it does not close the run, replay the batch, kill the session or spend its wedge-close allowance. Normal `report_done` clears that diagnostic.
- Status reads live and retained-exit session edges, including workers and grandchildren spawned after the request. A done report must be followed by the final turn end; interim idle and missing session evidence do not prove completion.

The interaction layer is now implemented locally:

- [`monitor_state.py`](../mcp_server/monitor_state.py) journals each session's MCP producers atomically under `~/.cm/monitor-state/`. Registration and delivery persist before starting; restarts and pruning retain unfinished results. A fresh daemon launch supplies `CM_MONITOR_TRACKING_V1`; resumed/legacy conversations cannot initialize positive coverage or upgrade an untracked journal just by reconnecting MCP. An accepted send RPC or a removed inbox file alone cannot settle an unverified delivery.
- [`completion.rs`](../daemon/src/continuous/completion.rs) validates journal coverage for the orchestrator and known descendants. Missing, malformed, interrupted or unverified records remain blockers. A bounded Codex rollout reader requires an explicit `task_complete` after the current report/input; real CLI lifecycle validation is still pending.
- [`continuous_drain.rs`](../daemon/src/control/continuous_drain.rs) exposes session-scoped `continuous.context`, `continuous.ack_drain` and `continuous.checkpoint_drain`. Receipts bind to request/run/fire/UID. Checkpoints require settled known workers and journals plus explicit background-work/reconciliation attestations and notes. New input, changed journals or changed workers invalidate the checkpoint. `report_done` must follow the checkpoint and the final turn must end afterward.
- Stop notices wait for an engine-specific turn boundary and revalidate under the lifecycle lock immediately before body/Enter delivery. Resume invalidates queued notices; a durable submission time and request-bound acknowledgment survive daemon restart. This path sends no Ctrl-C. A continuing Claude Stop hook invalidates the old idle/report signal instead of advertising a finished turn while processing inbox messages.
- The TUI's **Alt+Shift+C** menu lists definitions across hosts, including exited tasks. `s` requests stop; `r` resumes without forcing a fire. Resume uses the displayed request/admission revision. Host errors and mutation/refresh gaps cannot display a cached positive stop confirmation.

**Not deployed; still not a production cutover gate.** Existing MCP processes lack the new journal/tools and do not gain them from a brain restart. Missing legacy coverage remains blocked: a verified legacy handover/reconciliation path must be implemented before their cutover. The same applies to already-exited sessions and retained interrupted/ambiguous deliveries: this implementation exposes them rather than automatically discarding or replaying them. Background jobs outside CM are covered by the orchestrator's explicit attestation, not inferred from process quietness. B2 must separately handle retirement of verified finished sessions without treating an arbitrary kill as completion.

Local validation: 149 continuous-related Rust tests, 109 Python monitor/hook/tool/routing tests, and two TUI targeting/render/status tests pass; workspace compilation passes with existing warnings. Cases include both admission orders, stale resume/notice/receipt rejection, late descendant workers and monitors, trailing turns, post-checkpoint input, journal write failure/restart retention, missing evidence, scheduler intervention guards, and no optimistic remote status. Phase C must still prove the complete lifecycle on the pinned remote Codex CLI before any production task moves.

## Outcome and scope

An operator can ask a continuous task to **stop after current work**. The daemon stops admitting new periods or consumer batches, lets admitted work and its workers finish, and reports when the task is safe to checkpoint or migrate. It stays paused until explicitly resumed. Requesting drain does not interrupt a running turn or shorten its runtime allowance.

Deliver the feature in two layers:

| Layer | Deliverables | Purpose |
|---|---|---|
| Admission and status | Operator `continuous.drain`, durable scheduling gate, outstanding-obligation readout, restart and resume behavior | Required for a safe cutover even when an operator drives it entirely through `scripts/cm-op`. |
| Agent and operator interaction | Durable turn-boundary notice, request-bound acknowledgments, CM context/status, TUI **Stop after current work** and resume actions | Makes graceful stop usable for ongoing operation and visible to the agent. |

Both layers remain in the requested feature scope and the planned pre-migration delivery. The distinction sets implementation order; it does not defer the interaction layer. The new Alt+Shift+C menu adds task controls alongside the existing continuous-session panel. It seeds the focused task/host, preserves selection across refreshes and polls each host independently. General continuous-task editing is outside this feature.

The proposed first boundary is the **current period or already-claimed batch**, including workers needed to complete it. A fire admitted just before drain may finish its single claim/delivery. Finishing only individual items already started would also require durable per-item checkpoints and recovery of the unprocessed, already-acknowledged remainder; that is a separate extension. Producers can continue enqueueing while a consumer drains; later items stay pending.

## Existing behavior and code boundaries

[`continuous.pause`](../daemon/src/control/methods.rs) persists `paused` without stopping an existing turn. [`trigger`](../daemon/src/control/methods.rs) originally checked that flag only before arming its per-task `in_flight` guard; the local implementation now rechecks under that guard and rejects stale admission revisions. `continuous.run_now`, scheduled fires and agent `trigger` converge on this path.

The host-wide [`daemon.drain`](../daemon/src/control/methods.rs) is for restart coordination. It blocks spawn-shaped operations and sends a notice through the Claude Stop-hook inbox. A task drain must leave the current run's worker spawns and monitor delivery usable, and support Codex as well as Claude.

Several [scheduler](../daemon/src/continuous/scheduler.rs) paths use `paused`. Keep admission and intervention gates separate from diagnostic eligibility:

| Code path | During task drain |
|---|---|
| `trigger` and due-fire admission | Refuse new fires; revalidate under the per-task lock when arming `in_flight`. |
| `watchdog_pass` (Fresh runtime watchdog) | Keep the paused skip: do not launch investigators or escalate-kill the draining run. Revalidate before an intervention based on an older scheduler snapshot. |
| `should_supervise` / persistent respawn | Keep the paused skip; the shared trigger gate also rejects a stale supervision attempt. |
| `persistent_stall_pass` | Keep its existing paused skip; this pass reports stalls and does not close runs or spawn/kill sessions. The active-drain diagnostic path is `auth_wedge_pass`. |
| `auth_wedge_pass` | **Split this guard.** Allow observation of the active draining run, while preserving the ordinary paused-task skip and forbidding recovery from bypassing drain. |

Concretely, retain `paused=true` as the admission gate and add a persisted optional `drain` record. In `auth_wedge_pass`, replace the unconditional paused exclusion with a predicate equivalent to `paused && drain.is_none()`; the migration's engine-aware probe work independently replaces its Claude-only exclusion. Keep the existing enabled/run-identity checks. A drained task with no active run has nothing to probe. This exception belongs in the diagnostic pass, not in `trigger`, the watchdog or supervision.

Observation and recovery actions need separate checks. Detect and surface account failures or missing completion during drain. The admission foundation conservatively retains the run and records a diagnostic instead of closing it; any later automatic wedge closure requires proof that the matching run was abandoned and must retain unresolved batch obligations. Successful account proof may report recovery readiness but cannot unpause, claim work, respawn, or retire an active session. Revalidate the current pause/drain and run identity at each mutation boundary so a scheduler snapshot taken before drain cannot perform a late intervention. Normal completion and monitor delivery remain allowed.

## Durable API and state

Add operator-only `continuous.drain {task_id}`. Under the existing task lock, set `paused=true` and create or reuse a drain record with a request ID, request time, and identities of work already admitted: fire token, run sequence and session UID where present. Handle the spawn window where `in_flight` exists before `last_run` is committed. Persist before returning success. Do not hold global daemon locks while waiting for a process, monitor or network call.

Expose the request and derived `draining`, `drained`, or `blocked` status through `continuous.list`/operator status, with the outstanding run, workers and deliveries. “Request accepted” means admission is closed; it does not mean work is finished. An idle task with no obligations may become drained immediately. New state fields deserialize absent on old task records; they do not change engine defaults.

Make repeated requests idempotent while a drain remains active. Resume through `continuous.pause {task_id, paused:false}` clears the record atomically with unpausing and resumes the unchanged schedule. Plain pause callers retain their existing behavior; a second plain pause must not accidentally erase an active drain request. Bind any asynchronous status update or acknowledgment to the request and its run/session identity so old work cannot complete or re-pause a later request.

The migration operation consumes a verified drained state. It must revalidate outstanding obligations before committing; a previously displayed status is not authorization to retire a session that has become busy again.

## Finish current obligations

Close admission using the same lock/guard as `trigger`. If the fire wins the race, its admitted work may finish. If drain wins, the fire returns without spawning or claiming a batch. Scheduled, manual, agent, maintenance and supervision fires all obey that boundary. A process-local flag or mutex alone is insufficient across persistence/restart paths.

Let the admitted run use worker spawns and follow-ups needed to complete its work, receive final monitors, reconcile artifacts, checkpoint, and call `report_done`. Do not apply the host-wide spawn prohibition. The drain instruction prohibits starting an independent cycle or collecting another batch.

Mark drained only when:

- `in_flight` has cleared through the normal fire lifecycle and the admitted run is terminal.
- The orchestrator has finished its final turn, including work after `report_done`.
- Required workers and background obligations have finished, including workers created for the admitted run after the request.
- Completion deliveries and required acknowledgments have settled, and failed/partially processed items have been reconciled.

Use existing CM task/session edges and monitor state, exposing any information the operator must verify separately. A quiet transcript, an idle session, or the run's terminal flag alone is insufficient. Untracked or ambiguous obligations remain visible blockers; do not invent a positive drain proof. Pending backlog can remain, provided its next actions are recorded for later work.

A failed worker, account hold or missing completion stays visible while draining. Drain does not kill, force completion, or rewrite queue progress to turn a blocker green. Healthy work finishes; a task that cannot settle stays paused and its migration is deferred.

## Notices and TUI

Expose the request in CM context/status and deliver a durable notice at a safe turn boundary. The message asks the orchestrator to finish the admitted period/batch and workers, checkpoint and report completion. Reuse the delivery machinery where it can prove this contract for the selected engine; a Claude-only Stop-hook inbox is not sufficient for Codex.

No Ctrl-C, mid-turn forced input, injected `/exit`, deadline kill or automatic `force_done` is part of drain. The scheduling gate works even before the agent sees or acknowledges the notice. Retain notice delivery/acknowledgment state across daemon restart, and discard a stale notice before delivery after resume or a superseding request. An agent acknowledgment confirms receipt; it does not substitute for settled work.

The new TUI control resolves the focused continuous task and its owning daemon, submits the request asynchronously and renders progress from daemon status. Show **Stopping after current work**, **Stopped**, or the blocking obligation, plus an explicit resume action. A remote error or disconnected view must not optimistically display “Stopped.” Task selection and status must remain available when the orchestrator session has exited. Do not expand this into a general task editor or silently add `run_now` semantics to resume.

## Validation and delivery

Implement and test admission/status first, then integrate the notices and TUI against that same contract. Required cases:

- Periodic and consumer requests during a healthy long tool call; current work completes without interruption and no additional batch is claimed.
- A drain racing a manual/scheduled fire on either side of guard admission, including claim/delivery still in flight.
- Workers spawned to finish the admitted batch, interim worker turns, final monitors arriving late, and trailing orchestrator work after `report_done`.
- Ordinary pause versus drain in each scheduler guard; account/wedge diagnostics remain available, and stale watchdog/recovery snapshots cannot kill or restart a draining task.
- Duplicate requests, resume with an undelivered notice, late acknowledgments and restart during drain.
- Paused/exited tasks, failed workers, partial batches and unknown obligations; no false drained state.
- Local and remote TUI targeting/status, plus notice delivery for both Claude and the exact production Codex CLI.

No test should interrupt a production task. Use controlled sessions and fixtures for races and failures, and the migration canary for the real engine lifecycle. Ship the documented operator RPC, status, notice and TUI behavior together before the migration's Phase C exit gate.
