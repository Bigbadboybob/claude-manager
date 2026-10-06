# Task channels for continuous tasks

Owner ruling, 2026-09-14: every continuous task gets its own chat channel. The
orchestrator @mentions its workers there, workers @mention the orchestrator,
a mention wakes the addressed session natively, and the operator (or
`/triage-review`) posts landing dispositions into the same channel. This
replaces the 2026-09-10 DM handoff contract. The schedule still admits NEW
work; it is never the clock for work already in flight.

Source handoff: `~/.cm/handoffs/chat-migration-20260914/HANDOFF.md`.

## What already existed

- `ContinuousTask.messaging: Option<TaskChannel>` binds a task to a channel
  (`daemon/src/messaging/tasks.rs`). The binding survives orchestrator
  replacement: every fire calls `handover(new_uid)`, and the 2 s
  `messaging::tasks::refresh` loop re-binds a scheduler-owned subscription
  for the current session (`Store::bind_task_subscription`), which installs a
  continuous `notify=wake` monitor on the channel and carries the acknowledged
  message checkpoint across UIDs.
- Mentions and DMs wake both engines natively with no registration
  (`preferences.rs` defaults → `wake_intents.rs` → `delivery.rs` →
  Claude session socket / Codex app-server). A mention addressed to a busy
  session is queued and surfaces at the next turn boundary; it is never
  dropped. Delivery does not depend on the scheduler's transcript-tail gate.
- On cm-manager, 8 of 14 tasks were bound to one shared channel
  (`#orchestrators`); the rest had no binding. Nothing created channels,
  joined workers, or kept the `<task>-orchestrator` name across respawns
  (a dead predecessor's name stays reserved, hence
  `scraper-triage-orchestrator-1d8`).

## What this change adds (daemon)

1. **Channel per task, created by the daemon.** Path `ct/<slug>` where
   `<slug>` is the continuous `task_id` lowercased with `_` → `-`
   (`ct-` is the parent segment, auto-created). Display name = task label.
   Created as Owner on the messaging coordinator; on a replica the creation
   is forwarded once the coordinator is reachable. `continuous.create`
   creates and binds synchronously when it can and otherwise leaves it to
   the refresh loop. `TaskChannelPolicy` on the record: `auto` (default),
   `manual` (operator supplies `messaging.channel_id`), `off`.
   The binding record gains `path` for display.
2. **Members are computed, not declared.** Every refresh tick the daemon
   joins, for each bound task, every live session that is (a) tagged with the
   task (`continuous_task_id`), (b) transitively `managed_by_uid` from any
   session that is or was the task's orchestrator, or (c) bound to the task's
   planning task or a planning descendant (daemon `task_tree`). Joins are
   self-joins as that participant; the roster keeps exited members as
   `present:false`. Helpers without a child task join through (b).
3. **Orchestrator name is owned by the scheduler.** After a fire binds the
   subscription, the daemon assigns `<task_id>-orchestrator` to the current
   session: the prior holder (a dead or superseded orchestrator of the same
   task) is marked `released` through an `identity.update`, and the name is
   allocated for the new participant. `released` names are not reserved.
   `allocate` refuses any name ending in `-orchestrator` for a participant
   that is not the active task-subscription actor (`reserved_name`), so a
   worker can no longer take the parent's name from its brief.
4. **Discovery without `list_sessions`.** `chat_open` returns a `continuous`
   block for orchestrators and members: `{task_id, planning_task_id,
   channel:{id,path}, orchestrator:{participant_id, name, session_uid},
   role}`. `continuous.list` exposes `planning_task_id` and `messaging`
   (`channel_id`, `path`, `subscription_id`, `revision`, `session_uid`,
   `members`).
5. **Operator/migration entry point.** `continuous.ensure_channel {task_id}`
   creates/attaches the channel, binds it, assigns the name and joins current
   members — idempotent, used by the migration and for repair.
6. **Held runs surface in the channel.** When the scheduler first holds a run
   for missing Codex tail evidence it posts a system notice into the task
   channel (once per seq), so a wedged run is visible where the work is.
7. **Retention.** `continuous.delete` archives the channel (posting pause,
   history readable). Pause keeps it.
8. **Wake latch horizon.** A delivered-but-unread chat wake batch stops
   coalescing new arrivals after two minutes (`messaging::delivery`), so a
   mention to a session that ignored an earlier wake still wakes it.
9. **Bounded reconcile passes.** The refresh loop performs at most six
   coordinator joins per pass; the first migration pass joined ~60 workers
   under the store lock and starved RPCs and replica sync for two minutes.

## Contract change (policy)

`doc/continuous-review-routing.md` becomes the channel contract; the deployed
copies (`~/.cm/policies/…` on each host, the `~/.cm/docs/continuous-tasks/`
bundle) are regenerated from it. Workers post handoffs in the task channel
mentioning the orchestrator; orchestrators dispatch, review and return work by
mentioning the worker; the operator posts landed sha + deploy time + terminal
disposition mentioning the orchestrator. The "ready for approval" packet
(HANDOFF §3b) is the worker's exit criterion and the orchestrator's promotion
gate. DMs remain for private one-offs only.

## Migration of the 14 existing tasks

`scripts/migrate_task_channels.py` (runs through `scripts/cm-op --ssh
cm-manager`): for each task, `continuous.ensure_channel`, then
`continuous.update default_prompt` replacing the 2026-09-10 routing paragraph
with the channel paragraph. Schedules and pause state are untouched.
Backups of every prior prompt land under `~/.cm/audits/task-channels-<date>/`.

## Open items deliberately left

- Orchestrator-spawned helpers do not get a synthetic child task; they join
  the channel through the managed-by rule.
- Cross-host mentions already work through replication; a worker on another
  host is a member only if the task's daemon can see it, which today means
  same-host workers.
