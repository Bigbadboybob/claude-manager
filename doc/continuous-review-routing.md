# Continuous reviews and lifecycle stages

Owner policy, 2026-09-10: workers send routine reviews, progress, recoverable failures and completed handoffs to their orchestrator by chat DM. They do not call `notify_user` or DM/mention Owner for these events. The orchestrator reviews the evidence and advances authorized work. Only a reviewed decision or blocker that actually requires Owner may request Owner attention; existing stricter quiet policies still apply. A messaging change does not grant merge, deployment, source-enablement or spending authority.

## Handoff and wake handling

An orchestrator supplies its participant ID, current session UID and stable planning task ID in every worker brief. Resolve participants with `chat_people`; use `ping` and `list_sessions` to verify the manager/task binding. Workers send `chat_send(dm=<orchestrator participant>, request_id=<stable handoff id>, body=...)` with task ID, artifact SHA/path, checks and result, next action and any actual decision needed. Include the reviewed commit when reporting approval. On a timeout retry the identical request with the same request ID and originating daemon, rather than sending a second handoff.

DM delivery wakes a live orchestrator through CM's native notification path. On every wake, process `chat_read(inbox=true, unread_only=true)` before the ordinary cadence gate, follow all cursor pages, and acknowledge receipts only after reading. Verify artifacts rather than accepting a worker's completion claim. Record the disposition in task metadata and the durable index. Deduplicate DM and completion-monitor notifications by task ID, artifact SHA and stage. Reply only when acknowledging actionable work or supplying a decision; do not create acknowledgement ping-pong.

Keep scheduled scans, queue admission, completion monitors and periodic reconciliation. They recover missed/offline notifications and still find new work. A worker handoff is not a new scheduled scan or queue batch. If a parent is replaced, resolve the current live participant for the stable parent task instead of continuing to DM the old UID. If routing is unavailable, retain a pending handoff in the task metadata and durable artifact for the next parent reconciliation; do not replace routine routing failures with Owner alerts. Task-bound channel subscriptions, where configured, follow orchestrator replacement; personal DMs do not.

An open unfinished task retains a visible live session, including review and evidence waits. Worker `report_done` describes a completed work slice; it does not authorize hiding/closing an unfinished task. A continuous parent's `report_done` still completes its scheduled run and must not be omitted. Complete and clean up a task only when its actual evidence, review and operational gates are satisfied.

## Visible stages

The continuous column shows a colored text badge for every subtask. The spinner remains agent activity; idle is never an approval signal. A fixed legend below the list explains the colors and the distinction between the review queue and active review. Missing or unrecognized metadata is shown as **Unstaged**. Terminal planning status overrides stale stage metadata; legacy `metadata.stage` and operator-blocked rows remain readable.

Write `metadata.continuous_stage` at every transition, with an actual timezone-aware UTC `metadata.stage_updated_at` and a concrete `metadata.next_action`. Merge these keys into current metadata; preserve unrelated evidence and bindings. The following values are the shared contract:

| Value | Badge | Meaning / writer |
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

The viewer reads these fields through the existing planning metadata feed. No daemon or API schema change is needed. Updating prompts changes live agent policy; installing the matching TUI is separately required to display the new badges and legend.
