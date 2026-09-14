# Continuous reviews and lifecycle stages

Owner policy, 2026-09-14: every continuous task has its own chat channel, and all routine coordination between the orchestrator and its workers happens there. Workers post reviews, progress, recoverable failures, blockers and completed handoffs in the task channel and mention the orchestrator; the orchestrator dispatches, reviews, returns and promotes work by mentioning the worker. A mention wakes the addressed session natively. Neither side calls `notify_user` or DMs/mentions Owner for these events. Only a reviewed decision or blocker that actually requires Owner may request Owner attention; existing stricter quiet policies still apply. A messaging change does not grant merge, deployment, source-enablement or spending authority.

The schedule admits NEW work; it is never the clock for work already in flight. A worker that finishes a slice, hits a blocker or needs a review says so in the channel immediately, and the orchestrator answers on that wake, not on its next scheduled cycle.

## The task channel

CM creates and owns one channel per continuous task at `ct/<task-slug>` (for example `#ct/bug-triage`), named after the task label. The channel is bound to the task, not to any session UID: an orchestrator replacement inherits the binding, its subscription and the unacknowledged channel checkpoint. The daemon joins the current orchestrator and every worker it can attribute to the task (a session tagged with the task, a session spawned by any current or previous orchestrator instance or by one of its workers, or a session bound to the task's planning task or a planning descendant). Nobody registers anything; a worker that did not choose a name yet can still be mentioned by participant ID. Paused tasks keep their channel; a deleted task's channel is archived (posting pause, history readable). Owner can read every task channel without joining.

`chat_open()` returns a `continuous` block for every participant of a task channel: `task_id`, `planning_task_id`, `channel.id`/`channel.path`, `role` (`orchestrator` or `member`) and `orchestrator.participant_id`/`orchestrator.name`/`orchestrator.session_uid` for the CURRENT parent. Use it instead of `list_sessions` to address the parent; re-read it after any doubt, because a replacement parent is a new participant.

## Orchestrator names and identity

The scheduler assigns every continuous-task orchestrator the name `<task_id>-orchestrator` (for example `health-alert-triage-orchestrator`) when it binds the session to the task channel, releasing the name from the previous instance so no collision suffix appears. Names ending in `-orchestrator` are reserved for the session bound as a continuous task's parent; a worker that tries to claim one is refused (`reserved_name`). Workers choose a short codename on their first `chat_send(name=...)`: one word, or two short words joined by a dash, ideally the subtask id (`scraper-066-fix`). Do not recreate a session to rename it; use `chat_rename` with `chat_open().name.revision`.

Route mentions using participant IDs, never mutable names. A rename preserves the session UID and participant ID, existing DMs/groups, memberships, watches and historical mentions. Old names remain searchable aliases. A replacement with a new UID has a new identity; resolve it through `chat_open().continuous.orchestrator` or `chat_people` filtered by the planning task, never from a cached ID in a brief. Include this policy in every orchestrator prompt and worker brief; the deployed shared copy is `~/.cm/policies/continuous-review-routing.md`.

## Handoff and wake handling

There are two paths into the orchestrator. Scheduled scans or consumer queue admission find and admit work. Channel mentions request progress, technical review or a disposition for existing work. Process those handoffs on native wakes, before cadence gates; they do not start another scan or queue batch. Periodic reconciliation and completion monitors remain the recovery path for missed messages and stale lifecycle records.

**Worker → orchestrator.** `chat_send(channel="ct/<task-slug>", mentions=[<orchestrator participant id from chat_open().continuous>], request_id=<stable handoff id>, body=...)`. Say which step you completed (root cause written, fix committed, gates green, rebased, blocked, need a decision), the task ID, branch head SHA and artifact paths, the checks you ran with their results, and the next action you expect. Post each step as it happens; a single final message after hours of silence is the failure mode this policy exists to remove. On a timeout retry the identical request with the same request ID and originating daemon rather than sending a second handoff. Reserve DMs for private one-offs.

**Orchestrator → worker.** Dispatch, feedback, corrections and promotions are channel posts mentioning the worker's participant ID (`chat_open().continuous` on the parent side lists members through the channel roster; `chat_people(query=<subtask id or label>)` resolves a worker). An orchestrator wakes on every post in its task channel through the scheduler-owned subscription; on every wake read `chat_read(inbox=true, unread_only=true)` and the channel since the last checkpoint, follow all cursor pages, acknowledge receipts only after reading, verify artifacts rather than accepting a completion claim, record the disposition in task metadata and the durable index, and answer in the channel. Deduplicate mentions and completion-monitor notifications by task ID, artifact SHA and stage. Reply only when acknowledging actionable work or supplying a decision; do not create acknowledgement ping-pong.

**Operator → orchestrator.** `/triage-review` and Owner post dispositions into the task channel mentioning the orchestrator: landed SHA, deploy time and target, and the terminal disposition per task, or a returned-for-correction verdict. The orchestrator acknowledges in the channel and reconciles metadata and index; it never re-dispatches an item the operator marked landed or done. Index directives remain the periodic-reconciliation fallback until every parent has migrated.

**Busy sessions.** A mention addressed to a session in the middle of a turn is queued natively and surfaces at its next turn boundary; it is not lost and does not need a resend. A session that has exited is not woken; its channel history waits for the replacement.

Keep scheduled scans, queue admission, completion monitors and periodic reconciliation. They recover missed notifications and still find new work. A worker handoff is not a new scheduled scan or queue batch. If the channel is temporarily unavailable, retain a pending handoff in the task metadata and durable artifact for the next parent reconciliation; do not replace routine routing failures with Owner alerts.

An open unfinished task retains a visible live session, including review and evidence waits. Worker `report_done` describes a completed work slice; it does not authorize hiding/closing an unfinished task. A continuous parent's `report_done` still completes its scheduled run and must not be omitted. If an open task has lost its session, restore it through the existing task/worktree binding when recovery is healthy; otherwise record the specific recovery blocker without pretending it has a live worker. Preserve paused tasks. Complete and clean up a task only when its actual evidence, review and operational gates are satisfied.

A run the scheduler holds for missing transcript evidence is announced in the task channel by a system notice. The orchestrator finishes that cycle and calls `report_done`; the operator inspects the session and uses `continuous.force_done` only if it is actually wedged.

## Ready for approval

Owner ruling, 2026-09-14: by the time Owner sees a completed task it is deployment-ready and needs only approval. Sign-off is the only human step. The orchestrator drives the worker over the channel until all of the following hold, and only then promotes the task to `owner_review` with one approval packet:

1. The branch is rebased on current `origin/main` (or merge-tree clean against it), verified at promotion time and re-verified whenever main moves before approval.
2. Targeted tests are green on the rebased tree, plus the repository's full type-check gate, lint on touched files and the lane gates that apply (registry coverage for scrapers, spec validation for specialized builds, integration-marked DB tests for timeline/audit changes). Receipts are posted in the channel, not asserted.
3. Notes live where the repository expects them (`agent_docs/...`, `NOTES-<ID>.md`), never a repo-root `NOTES.md` that conflicts on every merge.
4. Operational gates are done: registrations, build/table rows in the right state, config diffs shown as one-line diffs, live probe receipts where a network path changed.
5. An independent review inside the lane (the orchestrator or a reviewer worker) read the three-dot diff against the root cause and posted a verdict with risks, blast radius and behaviour deltas.
6. Cross-lane collisions are resolved: the parent checked its own queue and the other task channels for the same files or commits and sequenced or dropped duplicates before surfacing.
7. The approval packet is ONE channel message mentioning Owner/the reviewer: task ID, branch head SHA, what it fixes, diff stat, gate receipts, behaviour change with defaults, dependencies/sequencing, deploy targets, rollback line and any open Owner call.

The worker brief states this list as its exit criteria. Returning work for correction is a channel post mentioning the worker and moves the stage back to `implementing`.

## Worktree synchronization and routine conflict recovery

Owner policy, 2026-09-13: continuous orchestrators must synchronize with current upstream before admission, but fast-forwardability is not an admission gate. Try a normal fast-forward first. If the orchestrator branch has diverged or the worktree has routine conflicts, inspect and preserve local commits and working-tree changes, integrate current upstream, resolve mechanically clear conflicts autonomously, and verify the resulting tree and retained artifacts before continuing. Glossary access timestamps and other mechanically recoverable bookkeeping changes must not block a cycle or require Owner review.

Do not discard or overwrite uncertain work merely to obtain a clean branch. Preserve recoverable state on a named backup branch, commit or durable artifact as appropriate, and keep implementation work on child branches/worktrees. Escalate only when substantive intent is ambiguous, merge/deployment authority is required, or safe preservation cannot be verified. A task-local instruction requiring `git merge --ff-only` is interpreted as a preferred first attempt, not a reason to stop after a safely recoverable divergence.

## Reconciliation and terminal cleanup

Start reconciliation with bulk planning rows joined by stable parent task ID, live daemon session summaries, the task channel since the last checkpoint and the durable index/journal. Compare stages, reviewed commit IDs, next actions and pending handoffs before opening individual transcripts. Inspect exceptions: a newer artifact without a disposition, an unhandled review queue, open work without a live session, or a terminal task whose worker remains live. A terminal index disposition is not a reason to exclude its leftover session from this check.

Separate message acceptance, native submission, observed wake, inbox acknowledgment and the persisted review disposition. These are different checkpoints. `notification_status` does not mark messages read. Preserve uncertain/pending delivery and the artifact; do not manufacture approval from idle state or repeatedly resend new handoffs. A model/backend failure may require separate recovery.

For a genuinely terminal task, verify that the latest worker artifact matches the final disposition and that no work is still active before authorized closure. Re-read planning metadata after writes. Use CM's guarded worktree cleanup and verify its per-path results: `phase=complete` can still mean the checkout was retained. Preserve branches, transcripts and useful artifacts. Closing an adoption-created empty workspace only changes viewer visibility; it does not complete planning work or reap its checkout.

If cleanup reports `live_process_reference`, identify the specific process and its session ownership. An orphan MCP helper can outlive its closed session. Do not bypass the protection or kill by a broad process-name match; resolve only a verified obsolete helper within the authorized cleanup scope, then use a fresh preview. See [task cleanup and recovery](task-worktree-cleanup.md).

**Check host routing before declaring a worker missing.** Chat spans paired hosts, but an agent's session-control MCP calls query only its local daemon. A cm-sessions reviewer cannot address a cm-manager worker by passing its UID to local `send_input`; post in the task channel instead. Use the existing `scripts/cm-op --ssh cm-manager ...` route only for Owner-authorized remote inspection or control that chat cannot do (kill, force_done, transcript reads). A local `not_found` does not justify replacement or reimplementation. See [cross-host session control](AGENT_QUICKSTART.md#controlling-a-session-on-another-host).

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
| `owner_review` | Needs you | Orchestrator reviewed the result, the readiness list holds and a specific Owner decision is required; orchestrator |
| `deploying` | Deploy | Authorized merge/deployment underway; orchestrator/deployer |
| `monitoring` | Verify | Post-change operational validation; worker/orchestrator |
| `waiting` | Waiting | Named external/data dependency, with next check; orchestrator |
| `done` | Done | Actual task complete; orchestrator |

Keep planning status `running` for internal review queues, active review and other work the orchestrator handles. Use `blocked` only for an actual Owner decision. Workers may request internal review but must not declare their own work approved for Owner. A reviewer records `metadata.reviewed_commit` when moving to `owner_review`; returning a patch for correction moves it to `implementing`. The operator records `landed_commit`, `deployed_at` and `owner_decision_<YYYY_MM_DD>` on landing. Clear resolved blockers/questions so they cannot leave stale attention markers.

The viewer reads these fields through the existing planning metadata feed. No daemon or API schema change is needed. Updating prompts changes live agent policy; installing the matching TUI is separately required to display the stage colors and legend.
