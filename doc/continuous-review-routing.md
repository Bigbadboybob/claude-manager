# Continuous reviews and lifecycle stages

Owner policy, 2026-09-10: workers send routine reviews, progress, recoverable failures and completed handoffs to their orchestrator by chat DM. They do not call `notify_user` or DM/mention Owner for these events. The orchestrator reviews the evidence and advances authorized work. Only a reviewed decision or blocker that actually requires Owner may request Owner attention; existing stricter quiet policies still apply. A messaging change does not grant merge, deployment, source-enablement or spending authority.

## Orchestrator names and identity

Every continuous-task orchestrator uses a descriptive `<task>-orchestrator` name, for example `health-triage-orchestrator`, `scraper-triage-orchestrator` or `cm-bug-triage-orchestrator`. This convention takes precedence over the general short-codename suggestion. Choose it with the first `chat_send(name=...)`; an already named orchestrator reads `chat_open()` and calls `chat_rename(name="<task>-orchestrator", expected_name_revision=<name.revision>, request_id="<new-id>")`. Use CM's accepted name if a collision adds a suffix. Do not recreate a session to rename it.

Route messages, mentions and handoffs using participant IDs, never mutable names. A rename preserves the session UID and participant ID, existing DMs/groups, memberships, watches and historical mentions. Old names remain searchable aliases. Resume can preserve identity; a replacement with a new UID has a new identity and must be resolved by its stable parent task binding as described below. Include this policy in every orchestrator prompt and worker brief; the deployed shared copy is `~/.cm/policies/continuous-review-routing.md`.

## Handoff and wake handling

There are two paths into the orchestrator. Scheduled scans or consumer queue admission find and admit work. Worker DMs request progress, technical review or a disposition for existing work. Process those handoffs on native message wakes, before cadence gates; they do not start another scan or queue batch. Periodic reconciliation and completion monitors remain the recovery path for missed messages, unavailable transports and stale lifecycle records.

An orchestrator supplies its participant ID, current session UID and stable planning task ID in every worker brief. Resolve participants with `chat_people`; use `ping` and `list_sessions` to verify the manager/task binding. Workers send `chat_send(dm=<orchestrator participant>, request_id=<stable handoff id>, body=...)` with task ID, artifact SHA/path, checks and result, next action and any actual decision needed. Include the reviewed commit when reporting approval. On a timeout retry the identical request with the same request ID and originating daemon, rather than sending a second handoff.

DM delivery wakes a live orchestrator through CM's native notification path. On every wake, process `chat_read(inbox=true, unread_only=true)` before the ordinary cadence gate, follow all cursor pages, and acknowledge receipts only after reading. Verify artifacts rather than accepting a worker's completion claim. Record the disposition in task metadata and the durable index. Deduplicate DM and completion-monitor notifications by task ID, artifact SHA and stage. Reply only when acknowledging actionable work or supplying a decision; do not create acknowledgement ping-pong.

Keep scheduled scans, queue admission, completion monitors and periodic reconciliation. They recover missed/offline notifications and still find new work. A worker handoff is not a new scheduled scan or queue batch. If a parent is replaced, resolve the current live participant for the stable parent task instead of continuing to DM the old UID. If routing is unavailable, retain a pending handoff in the task metadata and durable artifact for the next parent reconciliation; do not replace routine routing failures with Owner alerts. Task-bound channel subscriptions, where configured, follow orchestrator replacement; personal DMs do not.

An open unfinished task retains a visible live session, including review and evidence waits. Worker `report_done` describes a completed work slice; it does not authorize hiding/closing an unfinished task. A continuous parent's `report_done` still completes its scheduled run and must not be omitted. If an open task has lost its session, restore it through the existing task/worktree binding when recovery is healthy; otherwise record the specific recovery blocker without pretending it has a live worker. Preserve paused tasks. Complete and clean up a task only when its actual evidence, review and operational gates are satisfied.

## Reconciliation and terminal cleanup

Start reconciliation with bulk planning rows joined by stable parent task ID, live daemon session summaries, current parent bindings and the durable index/journal. Compare stages, reviewed commit IDs, next actions and pending handoffs before opening individual transcripts. Inspect exceptions: a newer artifact without a disposition, an unhandled review queue, open work without a live session, or a terminal task whose worker remains live. A terminal index disposition is not a reason to exclude its leftover session from this check.

Separate message acceptance, native submission, observed wake, inbox acknowledgment and the persisted review disposition. These are different checkpoints. `notification_status` does not mark messages read, and an accepted DM cannot wake a legacy session that lacks native transport. Preserve uncertain/pending delivery and the artifact; do not manufacture approval from idle state or repeatedly resend new handoffs. A supported transport migration and a model/backend failure may require separate recovery.

For a genuinely terminal task, verify that the latest worker artifact matches the final disposition and that no work is still active before authorized closure. Re-read planning metadata after writes. Use CM's guarded worktree cleanup and verify its per-path results: `phase=complete` can still mean the checkout was retained. Preserve branches, transcripts and useful artifacts. Closing an adoption-created empty workspace only changes viewer visibility; it does not complete planning work or reap its checkout.

If cleanup reports `live_process_reference`, identify the specific process and its session ownership. An orphan MCP helper can outlive its closed session. Do not bypass the protection or kill by a broad process-name match; resolve only a verified obsolete helper within the authorized cleanup scope, then use a fresh preview. See [task cleanup and recovery](task-worktree-cleanup.md).

## Visible stages

The continuous column colors each subtask's label by its lifecycle stage, leaving the full available width for the task name. Focused labels use the normal white/bold selection highlight, overriding their stage color; moving focus away restores the stage color. Idle age does not replace stage color. The spinner remains agent activity; idle is never an approval signal. A fixed legend below the list explains the colors and the distinction between the review queue and active review. Missing or unrecognized metadata uses the **Unstaged** color. Terminal planning status overrides stale stage metadata; legacy `metadata.stage` and operator-blocked rows remain readable.

Write `metadata.continuous_stage` at every transition, with an actual timezone-aware UTC `metadata.stage_updated_at` and a concrete `metadata.next_action`. Merge these keys into current metadata; preserve unrelated evidence and bindings. The following values are the shared contract:

| Value | Legend label | Meaning / writer |
|---|---|---|
| `queued` | Queue | Admitted, waiting for dispatch; orchestrator |
| `investigating` | Investigate | Evidence gathering; worker |
| `implementing` | Build | Implementation or requested correction; worker |
| `review_queued` | Review Q | Artifact handed to the orchestrator, waiting its turn; worker |
| `reviewing` | Reviewing | Orchestrator is actively reviewing the artifact; orchestrator |
| `owner_review` | Needs you | Orchestrator reviewed the result and a specific Owner decision is required; orchestrator |
| `deploying` | Deploy | Authorized merge/deployment underway; orchestrator/deployer |
| `monitoring` | Verify | Post-change operational validation; worker/orchestrator |
| `waiting` | Waiting | Named external/data dependency, with next check; orchestrator |
| `done` | Done | Actual task complete; orchestrator |

Keep planning status `running` for internal review queues, active review and other work the orchestrator handles. Use `blocked` only for an actual Owner decision. Workers may request internal review but must not declare their own work approved for Owner. A reviewer records `metadata.reviewed_commit` when moving to `owner_review`; returning a patch for correction moves it to `implementing`. Clear resolved blockers/questions so they cannot leave stale attention markers.

The viewer reads these fields through the existing planning metadata feed. No daemon or API schema change is needed. Updating prompts changes live agent policy; installing the matching TUI is separately required to display the stage colors and legend.
