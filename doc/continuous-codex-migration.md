# Continuous-task migration to Codex

Drafted 2026-09-06 from the live inventory at approximately 21:02 UTC; revised after Fable's two reviews, the task refresh at 21:20–21:21 UTC, and probe/consumer-volume checks at approximately 21:42 UTC.
Status, 2026-09-07 15:32 UTC: all ten production definitions now target Codex. The eight originally active tasks have fresh conversations verified against cm_pool/gpt-5.6-sol/CLI 0.153.4; behavior-triage and scraper-opt remain paused without new sessions. No Claude session remains on cm-manager. The morning operator direction authorized these cutovers after fresh health checks, before the former 24-hour pilot wait. Scheduled and repeated production observations remain explicitly pending; engine cutover is complete, production observation is ongoing. See the [execution record](continuous-codex-migration-execution-20260907.md) for current evidence, running work and limitations.

cm-manager runs pinned Codex CLI **0.153.4**, with **gpt-5.6-sol** routed through the private `cm_pool` provider. Two accounts are enrolled. Real requests, the installed account probe, and orchestrator/worker/investigator rollouts verify this model and provider. Local Codex has no model override. The [daily update checker](codex-update-checker.md) notifies on releases and retains the migration version hold. The former CLI is backed up at `/var/backups/cm-codex/codex-0.134.0-20260906T220213Z.tar.gz`.

The full daemon suite passes **1,246 tests, four ignored**. The migration work found and repaired Codex MCP coverage propagation and recovery of retained task bindings whose idle workspace entry disappeared at restart. Real canaries now prove healthy drain, persistent worker reuse, scheduled compaction, post-compaction restore, a Codex watchdog investigator, omitted-completion detection, and exact partial-batch recovery. Production sessions were preserved through holder/brain deployments; no admitted production work was interrupted. Private inventories, backups and runtime evidence are under `~/.cm/migrations/continuous-codex/20260907-pool-support/` on cm-manager.

## Outcome and agreed decisions

Operator direction, 2026-09-07 UTC: full ownership without per-task approvals is authorized. The morning instruction to check health and perform the remaining migration supersedes the earlier 24-hour pilot waiting gate. Continue checkpoint notifications through claude-manager. Preserve the no-interruption rule, originally paused tasks, queue reconciliation, fresh Codex conversations and the exact cm-manager model. Configuration-only paused-task cutovers may proceed while an active pilot cycle is observed. Scheduled/repeated-cycle counts below remain validation criteria rather than evidence already collected. Stop an affected cutover on unresolved obligations or failed evidence; never force completion to meet a deadline.

Move every continuous orchestrator and its future workers to Codex on cm-manager, using `gpt-5.6-sol`. Start each migrated orchestrator with a fresh Codex conversation and a written handover in its existing worktree. Keep `run_mode = persistent`: the conversation resets once at cutover, not every cycle.

Keep task IDs, planning parents, workspaces, worktrees, schedules, queue bindings, run counts, run history, and project memory. Retain Claude transcripts as reference material; do not translate their internal records into Codex rollouts. Existing workers finish their current work before being retired or handed over.

**Finish current work before cutover.** Request a graceful drain: stop admitting new periods or consumer batches, let admitted work and its workers finish, then checkpoint and retire the idle sessions. Migration never interrupts a running task to meet the rollout schedule. If work cannot finish, surface the blocker and defer that task.

The cm-manager Codex user configuration specifies `model = "gpt-5.6-sol"`; the disposable request above verifies successful resolution by the pinned CLI. CM orchestrator, worker and investigator runtime checks have passed. Local CM launch defaults have been changed in this worktree; local Codex receives no model override. The local daemon currently has no continuous tasks. All ten cm-manager fleet engine cutovers are complete; the remaining work is the production observation recorded below.

**Deployment scope of the existing diff:** besides the A-n/A-s and other local launch defaults, [`feedback.toml`](../workflows/feedback.toml), [`review.toml`](../workflows/review.toml), and [`design-doc.toml`](../workflows/design-doc.toml) switch their bundled workflow roles to Codex. Those definitions ship to `/opt/cm-daemon/workflows/` on cm-manager on deployment and change subsequent workflow launches there. Include them explicitly in the deployment manifest and validation; a deployment containing these TOMLs is also a production default change. The production engine switch is now verified for installed templates and the effective override layer, following repaired transcript binding and fresh-reviewer delivery tests. A default-engine review launch completed successfully. Production review-worker `needs_mcp` differs from this worktree, so its existing value was preserved during the engine-only deployment. The open TUI can still republish stale definitions after a brain restart; see the execution record for the resynchronization requirement.

Rollout order and observation gates below are migration policy. They are not existing scheduler settings. No schedule, reasoning-effort setting, memory cap, review policy, or production permission changes are implied.

The [graceful-drain design](continuous-task-drain.md) and [Codex account-probe design](codex-account-probe.md) own those subsystem contracts. This document owns migration dependencies and rollout gates. The requested drain controls and notices remain in scope; moving their design into a companion document does not defer them.

## 1. Fleet inventory

At the original inventory, all ten definitions used Claude, persistent sessions, and supervision. Eight were unpaused, and all ten had fired. Two were deliberately paused. Existing sessions' individual model overrides were not audited; the host's Claude model setting was `opus[1m]`. See the execution record for current engine state.

| Task | Current schedule | State | Compact every | Proposed wave |
|---|---|---|---:|---|
| `api-update` | Periodic, 24 h | Active | 16 | 1: pilot |
| `bug-triage` | Periodic, 3 h | Active | 16 | 2 |
| `perf-triage` | Periodic, 6 h | Active | 16 | 2 |
| `scraper-triage` | Periodic, 6 h | Active | 16 | 2 |
| `code-bughunt` | Periodic, 72 h | Active | 4 | 2, last |
| `momentum-detective` | Consumer `momentum-detective`; batch 5, window 1 h, depth threshold 1 | Active | 12 | 3, first |
| `scraper-creation` | Consumer `scraper-creation-proposals`; batch 10, window 1 h, depth threshold 0 | Active | 16 | 3 |
| `structured-scraper-creation` | Consumer `structured-source-proposals`; batch 5, window 6 h, depth threshold 0 | Active; seq 3 completed; draft parent awaits binding | 16 | 3, last |
| `behavior-triage` | Periodic, 24 h | Paused | 16 | 4: configure, keep paused |
| `scraper-opt` | Consumer `scraper-optimization-tasks`; batch 5, window 1 h, depth threshold 3 | Paused | 64 | 4: configure, keep paused |

Consumer windows and thresholds are recorded values, not promises of a fixed fire interval. Preserve them exactly. Refresh this inventory before execution and include any tasks created since this draft.

**Structured-creation run reconciliation:** operator `run_now` started seq 1 at **21:07:42 UTC on 2026-09-06**, claiming one item into `.queue/batch-1.json`. At the review refresh, `report_done` had marked it **Done at 21:15:14 UTC**, with `in_flight=null`. The complete transcript contains no `create_subtask` or worker-launch attempt; the run recorded an existing-scraper reuse proposal as **SP-1**, requiring an owner decision, and reported zero workers. This successful branch does not validate worker creation. At that snapshot, the definition still had `planning_task_id=null` and the live session had `task_id=structured-scraper-creation`; both were repaired during the September 7 cutover. Preserve the batch, SP-1 outcome and remaining owner action in the handover; do not replay this completed item just because the parent is missing.

The run also reported prompt/recorder CLI flag drift and a validation import failure caused by missing `GCLOUD_PROJECT_ID` in its worktree. Capture and reproduce these as specialized-workflow prerequisites before Wave 3; the live prompt subsequently incorporated both corrections, and the execution pass verified the recorder flags and validator import/help with the documented process-local workaround. Full disabled-record validation remains part of the specialized worker path.

## 2. Current constraints that shape the implementation

These findings come from the current code and live records, rather than assumptions about Codex:

- [`continuous.update`](../daemon/src/control/methods.rs) cannot change `engine`. The field is absent from its parameter struct; an engine-only request must not be treated as a successful migration. `continuous.delete` removes the task's state and run-log directory, so delete/recreate would lose the history we want to keep.
- A persistent trigger reuses `current_session_uid` while that session is alive. Relabeling the definition alone would keep delivering work to Claude. The replacement needs a new session UID and a fresh Codex transcript.
- `continuous.pause` blocks new triggers and supervision; it does not stop an already-running turn, cancel monitors, or retire a session. The local trigger implementation now rechecks pause/drain under its per-task fire guard, including an admission revision to invalidate pre-stop work. B2’s guarded engine/configuration transition is deployed and the pilot cutover exercised it.
- The per-task drain admission/status, bound acknowledgment and checkpoint contract is deployed and its healthy Codex lifecycle has passed. The existing host-wide `daemon.drain` serves restart orchestration, gates worker spawns and uses a Claude Stop-hook notice; it is unsuitable for draining one task while its current workers finish. The new continuous drain UI is part of this worktree; it is a new control surface, not an addition beside previously existing pause/run-now controls.
- The former [scheduler account/wedge pass](../daemon/src/continuous/scheduler.rs) skipped non-Claude tasks. The deployed engine-aware pass now uses a separate Codex tail parser and account proof, including worker/monitor obligations; it does not merely remove the old engine check.
- Claude recovery consumes `claude-probe-state.json`, produced externally every 30 minutes under `flock`. The deployed Codex producer/reader follows the same architecture using `codex-probe-state.json`. Codex tail handling closes proven missing-completion wedges while preserving worker/monitor obligations and holding unresolved consumer batches. Live partial-batch recovery passed before the pilot.
- [`spawn_investigator`](../daemon/src/control/methods.rs) formerly always spawned `claude-code`; the deployed B3 implementation selects Codex for Codex tasks and Claude for Claude/bash tasks. The scheduler watchdog calls this path, and its disposable runtime investigator test passed.
- The MCP [semantic idle helper](../mcp_server/monitor.py) is Claude-specific. Codex has transcript readers and a PTY fallback, while [asynchronous monitor delivery](../mcp_server/async_monitor.py) falls back when no Claude Stop hook consumes its inbox. These paths require real Codex verification.
- Consumer batches are staged under `.queue/batch-<seq>.json` and acknowledged around delivery, before the orchestrator completes the batch. A failed cutover or account error must preserve evidence of unfinished items.
- All ten original prompts contained explicit Claude worker-spawn instructions. The private source branch updates all ten, and all ten live prompts now match their reviewed sources after individual guarded cutovers. Changing an engine or host model alone does not change worker instructions.
- `structured-scraper-creation` originally lacked a `planning_task_id`. The reserved parent `bf0aac2b-39c3-4479-a718-55b8f9015cc6` was activated and bound after retirement of the drained old session. Its fresh Codex session has the correct planning UUID and has successfully created real children. Updating only a definition would not rebind an already-live session.
- `ContinuousTask.engine` is required on persisted records and has **no serde default**. The local B3 implementation changes specifically `ContinuousCreateParams.engine` to an explicit Codex default function; the shared enum's `#[default] Claude` and required record field remain unchanged. Existing records are not converted by this default.
- Initial CLI checks found cm-manager at **0.134.0**, versus local **0.153.2**. cm-manager has since been upgraded to **0.153.4** as recorded above. The [pre-trust code](../daemon/src/codex_trust.rs) and prompt-delivery fixes reference observations against **0.149.1**, including rollout-grounded confirmation and the 1024-byte PTY boundary. The upgrade resolves the older-version baseline; a local pass or a model config line still does not establish compatibility on cm-manager.
- Codex rollout rotation across `/compact` already has support: [`spawn_codex_rollout_watch` / `observe_codex_rollout_once`](../daemon/src/transcript_detect.rs) update the persisted transcript identity, and restore paths re-arm the watcher. Validate this existing support against the selected production CLI rather than treating it as an unimplemented feature.

## 3. Phase A — capture state and prepare portable prompts

Deliver a timestamped migration manifest for all tasks before retiring any session. Store it outside the task directories and worktrees so ordinary cleanup cannot remove it. Restrict access to the owning user; it contains operational context, not credentials.

For each task capture:

- Exact task definition, run count, latest run, run-log position, workspace and planning IDs, schedule, original paused/enabled flags, and current session UID.
- Current prompt and named prompt presets, their hashes and source revision, and worker prompt references.
- Worktree path, git status, task memory directories, relevant `NOTES.md`, and the locations of Claude transcripts and any Claude-only memory that needs a written summary.
- Open subtasks and their stage, live worker UIDs, outstanding monitors or pending deliveries, and any operator directive awaiting acknowledgment.
- For consumers, the latest staged batches and queue item IDs/statuses. Do not copy authentication files or connection secrets into the manifest.

Refresh runtime fields after the task is paused and drained. The initial snapshot is an inventory, not authority to overwrite newer state.

Historical completion events have a known audit defect: `report_done` formerly hardcoded `"fresh"` even for persistent tasks, including structured creation's seq 1. The fix is deployed for new events. Capture mode from the task definition and matching `fired` event, correlate completion by seq/fire token/session UID, and annotate the mismatch without rewriting historical logs. The defect does not invalidate its completion timestamp or `Done` status.

Capture a host runtime manifest as well: the daemon's actual Codex executable resolution, resolved binary path/hash, `codex --version`, package/install pin, daemon/MCP revisions, effective configuration sources and relevant model overrides. The selected remote installation is now **0.153.4**, with the prior 0.134.0 package retained for rollback; local 0.153.2 is only comparison evidence. Record the exact installed version and backup in the manifest and recheck for drift before rollout. Do not infer compatibility from a version floor alone. Phase C must prove a real model request and CM launch on this exact remote installation.

Once each old session is idle and checkpointed, archive its final transcript and necessary memory files into the restricted migration backup before retirement. Keep those copies outside normal session-retention cleanup for the rollout and rollback window; do not commit transcript contents to either repository.

Prepare a Codex version of every orchestrator prompt and referenced worker brief. Change actual spawn instructions to `type="codex"`; preserve task IDs, worktree reuse, deduplication rules, review/merge boundaries, completion signaling, and the task's existing scope. Keep these instructions explicit:

- Read the handover and task memory before scanning or spawning work.
- Reconcile existing subtasks from their files and git state; do not recreate them from remembered descriptions.
- Wait for the required worker result using the CM monitor contract; do not equate an interim assistant turn with finished work.
- Call `report_done` when the whole cycle is complete, including empty, failed, or blocked cycles.
- Continue the current operator-acknowledgment and planning-status conventions.

Audit Claude-specific commands and skill references. Codex must be able to read the actual instruction files; retain valid `.claude/skills/.../SKILL.md` paths where those files are authoritative and instruct it to open them explicitly. Do not assume a slash command is installed, rename skill directories, or mechanically replace every occurrence of “Claude.”

Keep prompt sources and live values synchronized. PredictionTrading staging commit `4aad0ab6e` on `cm/continuous-codex-prompts-20260907` maps all ten through `scripts/python/apply_orchestrator_prompt.py`:

| Task | Source relative to predictionTrading |
|---|---|
| `api-update`, `behavior-triage`, `bug-triage`, `code-bughunt`, `perf-triage`, `scraper-triage` | `scripts/python/docs/continuous-prompts/<task-id>.md` |
| `scraper-opt` | `applications/scraperOptimization/docs/CM_ORCHESTRATOR_PROMPT.md` |
| `scraper-creation` | `applications/scraperGeneration/docs/CREATION_ORCHESTRATOR_PROMPT.md` |
| `momentum-detective` | `applications/scraperGeneration/docs/DETECTIVE_ORCHESTRATOR_PROMPT.md` |
| `structured-scraper-creation` | `applications/scraperGeneration/docs/STRUCTURED_CREATION_ORCHESTRATOR_PROMPT.md` |

`DETECTIVE_WORKER_PROMPT.md` is a worker brief, not an orchestrator `default_prompt`. Audit it separately. The six new periodic-task sources capture their live prompts; the new scraper-opt source preserves its live body without overwriting a historically different template. The source branch is not pushed or merged. Apply each live prompt only while its task is paused for its wave, and make required worker briefs available in the actual consumer worktrees before their cutovers. Verify live content by re-reading it, not just from the update response.

**Exit gate:** complete task and host manifests, reviewed prompt diffs for all ten tasks, an identified planning parent and reconciled seq-1 outcome for structured creation, and no unexplained prompt-source drift. Carry its reported recorder/validation failures into specialized-workflow validation. Do not interrupt any run that starts after this snapshot; request a drain and refresh its checkpoint when it finishes.

## 4. Phase B — implement graceful drain, engine switching and Codex recovery

All components in this phase are implementation deliverables, not merely evidence to collect during the canary. Ship and test them before production migration, with an explicit hard gate before Wave 3 consumers.

### B1. Per-task graceful stop

Implement the [graceful-drain design](continuous-task-drain.md). Its admission/status layer is the minimum needed for an operator-driven cutover: an operator-only `continuous.drain`, a durable gate against new fires/claims, and a readout of outstanding run, worker and delivery obligations. Build that layer first. The durable agent notice, request-bound acknowledgments and new TUI **Stop after current work**/resume controls remain planned deliverables before migration.

The initial boundary is the current period or already-claimed batch and its workers. Finish admitted work without interruption; remain paused after it settles. A blocked or ambiguous obligation prevents a positive drained result. Producers may continue enqueueing for later processing.

Reuse `paused=true` plus a persisted drain record. **Split the paused guard in `auth_wedge_pass` specifically** so draining runs remain observable (skip ordinary paused tasks via a predicate equivalent to `paused && drain.is_none()`). Keep paused gates on fire admission, the Fresh watchdog and persistent supervision. The separate `persistent_stall_pass` retains its existing paused skip; it only reports stalls and does not intervene in session execution. Diagnostic/recovery mutations must revalidate drain and run identity; account recovery cannot bypass the gate or retire an active draining session. The companion design maps these requirements to the code paths and race tests.

**Migration dependency:** both feature layers pass their focused tests, and Phase C proves drain/finish/resume with the actual Codex runtime before any production cutover. No live task is force-stopped to supply that proof.

### B2. Guarded engine migration

The deployed operator-only `continuous.migrate_engine` operation implements the contract below. Keep ordinary config edits in `continuous.update`; reject attempted engine edits there with a useful error pointing to the migration operation.

The migration operation should accept the target engine and expected source identity/configuration, support a read-only dry run, and work in both directions for rollback. Use expected current session UID, run count, and source config hash/revision to reject stale requests. A retry after an uncertain response must either recognize the completed migration or report the actual conflicting state.

Before committing, require all of the following:

1. The task is paused and its graceful drain is complete; no fire is in flight, no run remains nonterminal, and no worker or delivery obligation remains active.
2. The old orchestrator is gone from the live registry/holder, and its retirement is durably recorded. For a never-fired task, an absent old UID is valid.
3. The expected task/workspace/planning identity still matches. Worktree and prompt prerequisites are valid.
4. No scheduler fire, manual trigger, unpause, or competing migration can cross the validation/commit boundary using stale state.

Implement the fourth condition with the existing task-lock discipline and a shared guarded transition, rather than a process-local mutex alone. Revalidate pause and the relevant engine/configuration state when a trigger arms `in_flight`; a trigger that read the old task before a pause must not spawn or deliver after cutover. Respect the established task-lock/daemon-state lock ordering. Do not hold global locks while waiting for process exit or doing network I/O.

The commit changes the engine and clears the retired `current_session_uid`. It preserves the original history, IDs, worktree, scheduler values and paused state, records an auditable engine-change event, and does not spawn, consume a queue item, or unpause. If an auxiliary audit write fails after a durable state change, report partial completion accurately so a retry cannot duplicate the operation. Do not clear a current account-blocked run merely to satisfy the gate; reconcile that run and its batch first.

Retirement must also prevent restore/re-adoption from resurrecting the old Claude session after a daemon restart. Validate the manifest/tombstone and current-session identity rules, including stale entries with the same continuous task ID but a different engine or session UID.

### B3. Creation defaults and investigator engine

Change **`ContinuousCreateParams.engine`** to use an explicit create-only serde default function returning `Engine::Codex`, and update the creation runbook/examples. Leave the shared enum's `#[default] Claude` unchanged and keep persisted `ContinuousTask.engine` required. Test omitted-engine creation as Codex, explicit Claude/Codex/bash requests, existing records retaining their engine, and missing-engine records still failing deserialization. This is a creation-default change, not a legacy-record conversion.

Change `spawn_investigator` to use Codex for a Codex task and Claude for a Claude task. Preserve the existing Claude investigator policy for explicit bash tasks, which are outside this fleet; record that exception in the runbook. Verify investigator prompt delivery, snapshot access, task grouping, `resolve_stuck`, runtime limits and investigation caps. Test the watchdog calling this path, even though the current ten tasks are persistent and the existing runtime watchdog targets fresh runs. There must be no unconditional Claude fallback for Codex tasks. On cm-manager, investigators must prove the same `gpt-5.6-sol` runtime selection as orchestrators and workers.

### B4. Engine-aware tail probe and wedge closer

Extend the continuous probe interface to dispatch by the **active run's validated session engine** and normalize Claude and Codex observations into account-error and turn-state results. Implement a Codex rollout-tail parser using fixtures from the exact CLI selected in Phase A. Distinguish completed turns and delivered-but-unanswered prompts from healthy in-progress tool calls, compaction/rollout rotation and unknown or unreadable state. Unknown records fail conservatively and surface a diagnostic; they do not become a fabricated completion or account error. Use observed structured error records, not ordinary assistant prose, to classify Codex authentication/usage failures.

Wire this into the scheduler's auth/wedge pass and engine-specific alerts. Reuse the existing wedge grace, close limit and escalation policy: a proven abandoned run can become `Failed` with an attributed `wedge_closed` event; a healthy long-running call cannot. Guard updates against the current engine, seq, session UID and fire/delivery boundary so an old turn cannot close a new run. A confirmed account blocker takes precedence over wedge closure and holds further fires/claims. Keep all Claude behavior working during the mixed-engine rollout.

For consumers, a closed or account-blocked run does not prove that its acknowledged batch was processed. Retain the staged batch and per-item outcomes; keep unresolved work held for reconciliation rather than letting a close/refire path abandon it. Test completed-without-`report_done`, unanswered delivery, a healthy long tool call, stale completion after a new fire, account errors, wedge escalation and compaction rotation. The drain observer must see these outcomes while preserving its no-new-work gate.

### B5. A real Codex account-probe producer and recovery consumer

Implement the [Codex account-probe design](codex-account-probe.md): versioned `scripts/codex-usage-probe`, installed as `~/.cm/bin/codex-usage-probe`, an external cron entry invoking the script (which acquires its own `flock` internally), atomic `~/.cm/codex-probe-state.json`, and a strict engine-specific reader in scheduler recovery. Follow Claude's existing producer/file-consumer architecture. The proposed initial cadence is 30 minutes; an operator can request an earlier check under the same lock. There is no daemon-resident probe worker or separate scheduler.

The probe must complete a bounded real request with the pinned cm-manager CLI and `gpt-5.6-sol`, using the host account without task/project instructions or CM tools. Recovery requires fresh successful Codex evidence started after the hold, matching the current runtime/model/configuration and guarded task/run/session identity. A successful Claude probe or model config line cannot release a Codex hold. Preserve Claude's existing producer and compatibility behavior.

Reconcile processed versus unfinished staged items before releasing consumer holds, retaining deduplication keys and ambiguous-work blockers. During drain, successful proof only exposes recovery readiness; it cannot unpause, claim, respawn or retire an active session. The companion design owns the schema, timeout, lock, deployment and validation details.

**Migration dependency:** install and test both the external producer and daemon reader/recovery path before the canary. Phase C must show a real successful probe on the selected remote installation; failure cases use fixtures. Both parts and their evidence are mandatory before Wave 3.

**Exit gate:** the drain operation/UI, migration operation, create-only default, engine-aware investigator, Codex tail/wedge closer, and Codex account-probe producer plus recovery consumer are implemented and covered by focused tests. Include both migration directions, never-fired and paused tasks, active-run rejection, stale requests, duplicate retries, concurrent trigger/drain/pause/unpause, restart around commit, and history preservation. Tests must prove that retirement cannot produce two orchestrators and that graceful drain cannot interrupt current work. None of the Codex recovery components may be deferred to evidence collection in Phase C or beyond Wave 3.

## 5. Phase C — validate Codex lifecycle behavior

Use a disposable continuous task, worktree and queue on cm-manager before migrating a production task. Keep its downstream actions confined to test fixtures. Verify **0.153.4, the exact installation selected in Phase A**, under the daemon's launch environment, including an actual successful `gpt-5.6-sol` request. The installed-version check and standalone CLI/account-probe model resolution have passed. CM orchestrator/worker/investigator model selection and the lifecycle checks have passed; see the execution record. If a compatibility fix requires another CLI version, record it and repeat the gate there; neither the config value nor a local pass is sufficient.

Exercise the Phase B implementations below and fix any remaining incompatibilities. Confirm the resulting runtime model using rollout/session evidence rather than assuming the host config wins over every project override.

| Surface | Required evidence |
|---|---|
| Launch and MCP | On the pinned remote CLI, pre-trust and rollout-grounded submission confirmation work; short, multiline and over-1024-byte prompts each arrive intact once. Codex starts without a setup/update/trust modal and can call the CM tools. |
| Model | A real request completes with `gpt-5.6-sol`; orchestrator, worker, investigator and account probe have runtime evidence of that model, with no unintended project or role override. |
| Worker lifecycle | Create a subtask, start its Codex worker, receive its final completion, read the artifact, and follow up in the same task/worktree. Also exercise momentum-detective's same-task worker pattern. |
| Permissions and grouping | The replacement uses the existing planning parent and can manage its descendants with the existing permissions. The TUI groups them under the correct continuous task. Do not add global permissions to make a test pass. |
| Completion and wake-ups | `report_done` closes the intended run. Interim turns do not finish final monitors. A worker finishing while its orchestrator is busy eventually wakes it exactly as intended, without losing or duplicating the input. |
| Missing completion | A Codex turn that ends without `report_done` is detected by the wedge handling; a long healthy tool call is not falsely closed. |
| Graceful drain | During a real healthy run, the drain request admits no new period/batch and sends no interrupt. Current workers, final monitors and `report_done` finish before status becomes drained; it stays paused across restart until explicit resume. |
| Investigator | The watchdog's disposable fresh-run scenario launches a Codex investigator, which reads the snapshot and completes the supported verdict path within the existing caps. |
| Account recovery | The new Codex probe makes a successful real request on this installation. Fixtures prove Codex failures hold the right run, stale/Claude proof cannot release it, and partial-batch recovery plus drain/pause gates preserve work. |
| Persistent context | A second cycle reuses the Codex session and reads the task memory. A fresh respawn reads the handover/memory instead of losing the task's state. |
| Compaction | Exercise existing [`spawn_codex_rollout_watch`](../daemon/src/transcript_detect.rs) support: `/compact` works, rotation updates the persisted rollout identity, and restart resumes post-compact history. The maintenance fire closes, claims no queue batch, and the next prompt arrives. Keep existing cadences unless a measured incompatibility requires a scoped fix. |
| Restart | A brain restart re-adopts the live Codex session. A tested restore path resumes the correct Codex rollout without reviving retired Claude sessions. Exercise destructive restore scenarios in the disposable harness, not by restarting the production host. |
| Consumer correctness | Items are staged, delivered, processed and recorded once; a no-work cycle closes; interrupted delivery and partial processing retain enough evidence to reconcile unfinished IDs. |

Account and wedge tests validate B4/B5's concrete producer, parser and recovery implementation. They cannot be waived because a happy-path turn called `report_done`. Validate that their diagnostic pass remains useful during per-task drain without admitting or interrupting work.

Run focused Rust tests for the migration, scheduler, restore and transcript changes, and the Python monitor/final-completion/queue tests affected by the implementation. Record versions, fixtures and observed results. Readable transcripts and a successful one-turn answer alone are insufficient.

Deploy required daemon changes with the [holder/brain runbook](../HOWTO_HOLDER_BRAIN_SPLIT.md), using a brain restart and its health checks. Deploy the MCP changes on cm-manager before launching the canary so its new MCP processes load the intended version. The holder and existing production sessions remain in place. If a change requires a hard restart that would interrupt them, wait for those sessions to drain first; do not substitute it for a brain restart to accelerate migration.

Treat the bundled workflow TOMLs as an explicit deployment item. Stage their production switch until the canary passes, and validate the newly loaded Codex roles as part of the deployment. Likewise, do not create new production continuous tasks under the changed creation default before this gate. Record the installed CLI pin and daemon/MCP/workflow revisions together so rollback does not accidentally mix an untested CLI with the new delivery/probe code.

**Exit gate:** actual model resolution on the pinned remote CLI, a complete Codex orchestrator → worker → final-monitor → `report_done` cycle, a persistent second cycle, graceful drain/resume, investigator, compaction, restart, wedge closure, account-probe/failure and queue-recovery checks all pass. No production task moves before this gate; B4/B5 implementation and this gate are mandatory before Wave 3.

## 6. Phase D — per-task cutover procedure

Run retirement and engine commits for one task at a time. Under the September 7 morning direction, independent legacy drains/handovers were prepared concurrently, and new healthy Codex cycles could continue while the next task cut over. Never overlap two orchestrator generations for one task.

1. **Request graceful drain.** Use `continuous.drain` to prevent new periods/batches while the admitted run finishes. Wait for the drain request to be acknowledged, for `in_flight` to clear, and for execution/deliveries to settle. A task that fires between the inventory and the request gets to finish that admitted work. No Ctrl-C, kill, forced completion, or runtime reduction to meet a cutover deadline.
2. **Drain work and write the handover.** Let current workers complete their active turn/work, reconcile their outputs, settle outstanding monitor notifications, and save the task's open backlog. Retire completed Claude workers after saving their artifacts and checkpoints, keeping their subtask records intact for later Codex follow-up. Do not retire a worker merely because its transcript has an interim answer. If a long-running worker cannot drain, leave this task paused and defer its migration instead of force-closing the run.
3. **Checkpoint.** Write `HANDOVER_CODEX.md` in the task's existing memory directory. Include mandate, open issue/subtask IDs and stage, next action, operator directives, relevant artifact paths, queue progress, and unresolved questions. Link the old transcript for optional reference. Summarize any essential Claude-only memory into ordinary files. For a paused/exited session, build the handover from disk and transcripts without unpausing it. For structured creation, include seq 1's completed batch, SP-1 and any subsequent outcomes, its missing-parent repair, and specialized-workflow blockers.
4. **Retire the old orchestrator.** After the checkpoint and settled deliveries, terminate only that session through the normal session lifecycle and wait for exit/tombstone persistence. Confirm no late worker notification can restart work in it and no old live session remains for the task. Preserve its transcript.
5. **Apply and commit.** Apply the prepared prompt/presets and any planning-parent repair while paused; verify by re-read. Run the migration dry run against a refreshed expected-state snapshot, then commit `engine=codex`. Verify preserved identity/history, cleared current UID, and continued pause. No raw live `state.json` edits or delete/recreate.
6. **Resume according to its original state.** Previously active tasks unpause and let the scheduler perform the next eligible fire. The first Codex session is fresh and must read the handover. Previously paused tasks stay paused and do not fire. Avoid an unpause-plus-`run_now` pair that races a due scheduled fire; any accelerated validation needs a serialized trigger path.
7. **Observe and record.** Verify actual engine/model, new session UID, correct worktree/parent, next sequence number, completed run, worker engine and task grouping. Update the migration manifest with the result, prompt revision, old/new UIDs, and rollback checkpoint.

The task's pending backlog does not need to be empty. Its live execution and delivery obligations must be settled, and unfinished backlog must have enough written state for the new orchestrator to continue safely.

## 7. Phase E — rollout order and gates

### Wave 1: `api-update`

This was the first production pilot because it is an investigation-oriented periodic task and avoids consumer batch handling. The original waiting gate required two completed Codex cycles, including a naturally scheduled cycle and a full normal cadence after first success (at least 24 hours). The September 7 morning operator direction superseded that wait after healthy runtime checks. The scheduled/cadence observation remains required before declaring this task production-verified.

Compare its source/checkpoint progression, deduplication, task proposals, scope discipline and operator handoffs against recent Claude cycles. A legitimate no-change result is fine. Worker lifecycle coverage still comes from Phase C if the live source produces no work. Do not manufacture findings or queue entries to satisfy a count.

### Wave 2: periodic triage

The active periodic cutovers followed `bug-triage`, `perf-triage`, `scraper-triage`, then `code-bughunt`. After API and bug-triage completed real worker cycles, independent cutovers could advance on healthy, correctly bound fresh-runtime evidence while normal Codex work continued. Require two completed productive cycles and one naturally scheduled cycle for each before marking it production-verified. Keep the 72-hour code-bughunt cadence: its first Codex turn verifies handover/binding only and does not count as a productive hunt or advance its baseline. Its scheduled observation remains open.

Check each task's normal output and review boundary: actionable evidence, no repeated proposals for existing issues, correct `blocked` versus `running` usage, and preserved operator acknowledgments. `code-bughunt` also needs explicit verification of its existing direct-fix versus reviewed-subtask rules.

### Wave 3: consumers

The consumer cutovers followed `momentum-detective`, `scraper-creation`, then `structured-scraper-creation`, with reconciled old queue state and healthy fresh-runtime evidence at each transition. Under the morning direction, an empty queue or ongoing healthy new Codex batch did not block an independent cutover. Require two completed nonempty batches per task before marking it production-verified. An empty cycle is recorded as “awaiting production batch”; keep disposable-harness evidence separate and never manufacture or replay completed work.

**Order rationale:** momentum goes first to deliberately exercise frequent consumer completion/admission and its same-task worker/monitor path after Phase C has validated that behavior in isolation. This front-loads the scheduling case; it is not a lowest-volume selection. In the seven-day run-log window ending approximately 21:42 UTC on 2026-09-06, momentum recorded **91 nonempty batches / 127 items**, compared with scraper creation's **48 / 241**. Momentum has more batch boundaries but fewer total items in this sample. Structured creation recorded only **1 / 1**, has little operating history and still needs parent/workflow repairs, so its small sample does not make it the preferred first mover. Refresh these measurements and readiness before execution; change the order if the evidence changes rather than citing lifetime run counts as a volume estimate.

Before this wave, require the deployed Codex tail/wedge closer and real Codex account-probe producer/recovery consumer from B4/B5, with the remote-version evidence from Phase C. An RPC-only engine switch is insufficient for consumers.

For `momentum-detective`, verify same-task workers and the referenced detective worker brief. For scraper creation, verify that the existing test/registration/review procedures are followed. For structured creation, let any admitted work finish, reconcile seq 1 and any later batches, bind its missing planning parent while drained, and verify the new session carries that planning UUID. Validate the specialized worker path and resolve the run-reported recorder/validation prerequisites before resuming. Its completed reuse-only batch did not exercise subtask creation and must not be replayed or counted as Codex validation. Never enable a scraper, merge a change, or loosen review rules merely to validate the migration.

### Wave 4: paused tasks

Migrate the saved definitions and prompts for `behavior-triage` and `scraper-opt` using the same drain/checkpoint/retirement rules, leaving `paused=true`. Report them as **configured for Codex, production validation deferred until resumed**. Do not wake them just to achieve a fleet-wide green result.

The wave order and observation counts are conservative starting points. Change them only based on recorded evidence and operator direction, rather than silently reducing gates to finish sooner.

## 8. Rollback and stopping conditions

Stop advancing the rollout on lost or duplicated work, failure to close runs or wake the orchestrator, persistent session/model mismatch, account-failure loops, queue discrepancies, or a material regression in task quality. A failed task does not require reverting unrelated successful migrations.

For the affected task:

1. Request graceful drain and reconcile its current Codex run, workers and staged batch. Let healthy work finish; a blocked task stays paused pending reconciliation. Preserve diagnostics; do not use `force_done` to conceal unfinished queue work or expedite rollback.
2. Checkpoint the work completed since migration and retire Codex through the same guarded lifecycle.
3. Restore the prior Claude prompt/configuration fields through supported operations and migrate the engine back to Claude. Preserve the **current** run count, logs, queue progress and project files. The pre-migration backup is reference evidence, not a whole-state restore image.
4. Start a fresh Claude conversation in the same worktree with an updated handover when the task is ready to resume. The original Claude transcript remains available for reference but must not override newer disk/queue state. Keep originally paused tasks paused.

Leave cm-manager's Codex model setting in place during a task rollback: it does not affect Claude and remains the desired setting for other Codex tasks. Log the rollback as an additional event without deleting Codex run history.

## 9. Completion criteria and deliverables

Ship the per-task graceful-drain RPC/UI/status, migration RPC/tooling, create-parameter default and engine-aware investigator, Codex tail/wedge closer and account-probe producer/recovery consumer, targeted tests, versioned prompts and drift checks, the updated creation/operations runbook, and completed per-task and host migration manifests. Record the tested CLI pin and the deployed workflow TOMLs explicitly.

Engine cutover is complete when every definition and future worker-spawn instruction targets Codex, investigators follow the task engine, the old generation is retired after its work finishes, all active replacements have verified identity/model/provider, and originally paused tasks remain paused. These transition checks passed on September 7. Production validation remains open until all eight originally active tasks pass the applicable scheduled/repeated-cycle gates above; paused-task validation is deferred until resumed. Any unobserved batch or scheduled cycle stays listed as pending rather than being inferred from configuration or a canary.

New continuous tasks default to Codex through the create-parameter default; explicit engine selections, required persisted engine fields and historical session restore remain correct. Real model requests show `gpt-5.6-sol` on the pinned cm-manager CLI, local model selection is unchanged, and task identities, history, queue state and worktrees are intact. A Codex completed turn without `report_done` has a tested closer, and a Codex account hold has its own tested positive-recovery path.

Implementation spans claude-manager (daemon lifecycle/scheduler, MCP monitors, tests and runbook) and predictionTrading (orchestrator/worker prompts, prompt-application mapping and project-specific validation). Completing the code phase is not the same as completing the live rollout.

## Evidence and references

- Live `continuous.list` on cm-manager and local, plus per-task `state.json` fields, read 2026-09-06. cm-manager refreshed at approximately 21:20 UTC. Treat the inventory above as a dated snapshot.
- Structured-creation seq 1: cm-manager `~/.cm/continuous-tasks/structured-scraper-creation/{state.json,runs.jsonl}`, its worktree's `.queue/batch-1.json`, the session binding in `~/.cm/daemon-sessions.json`, and Claude transcript `d8d2bb60-36d4-4dc7-94b2-2677e2a0a9dc.jsonl`, inspected read-only at 21:20–21:21 UTC. Tool records include one successful `report_done` and no subtask/worker-launch attempt; preserve the source transcript privately.
- Initial `command -v codex` / `codex --version`: cm-manager `/usr/bin/codex`, 0.134.0; local `/home/lucas/.nvm/versions/node/v24.1.0/bin/codex`, 0.153.2. After the authorized upgrade, a fresh cm-manager login-shell check at 22:02:47 UTC returned `/usr/bin/codex`, **0.153.4**; the npm package metadata agrees. Phase A still captures the daemon's actual launch environment and binary hash.
- Read-only cm-manager `crontab -l` at approximately 21:42 UTC: Claude's external probe runs every 30 minutes under `flock`, using `/home/lucas/.cm/bin/claude-usage-probe`. Seven-day consumer counts use `runs.jsonl` entries with `event=fired`, `status=running` and positive `detail.batch_count`; they measure recorded batch admission, not completed item outcomes.
- Companion designs: [graceful task drain](continuous-task-drain.md) and [external Codex account probe](codex-account-probe.md). [Audit-line backlog](../TODO.md) records the persistent-run `report_done` mode defect.
- [Continuous task record and persistence](../daemon/src/continuous/task.rs).
- [Trigger, migration-adjacent CRUD, batch delivery and session restore](../daemon/src/control/methods.rs).
- [Continuous scheduler and account/wedge gates](../daemon/src/continuous/scheduler.rs), [current tail probe](../daemon/src/continuous/probe.rs).
- [Codex pre-trust](../daemon/src/codex_trust.rs), [prompt submission and investigator spawn](../daemon/src/control/methods.rs), [Codex rollout rotation watcher](../daemon/src/transcript_detect.rs), [holder adoption](../daemon/src/holder_mode.rs), [restart restoration](../daemon/src/reexec.rs).
- [MCP monitor semantics](../mcp_server/monitor.py), [asynchronous delivery](../mcp_server/async_monitor.py), [Codex transcript reader](../mcp_server/transcripts/codex.py).
- [Continuous-task operator runbook](../HOWTO_CONTINUOUS_TASKS.md), [holder/brain deployment runbook](../HOWTO_HOLDER_BRAIN_SPLIT.md), [agent task/session conventions](../AGENT_ORCHESTRATION.md).
- predictionTrading's `scripts/python/apply_orchestrator_prompt.py`, read from the live scraper-creation checkout; source mapping recorded in Phase A.
