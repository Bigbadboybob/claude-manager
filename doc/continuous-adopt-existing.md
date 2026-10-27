# Attach a live agent to a continuous schedule

`continuous.adopt_existing` is an **operator-only** RPC on the daemon that
already owns the session. It registers an existing planning task and binds that
same process and workspace to a persistent continuous task. It sends no prompt,
spawns no session, creates no worktree, and performs no engine migration.

This is an explicit exception to the usual manager-host convention: a live agent
on `cm-sessions` stays on `cm-sessions`, whose scheduler must be enabled. A task
registered to another daemon/host is refused. The workspace is pinned and its
planning mapping is retained. No session permissions are granted or widened.

## Preconditions

- The owning daemon has strong operator authentication, an enabled scheduler,
  durable session-manifest storage, and an open messaging store. The planning API
  includes migration `034_continuous_registration.sql` and the registration route.
- The selected agent is a live local Claude or Codex process with the exact
  planning task, workspace, worktree and engine requested. Its engine reports
  idle with a complete background census, no active jobs or pending input. PTY
  silence and `unknown` are insufficient. Shells and workflow participants are
  refused, including active workflow records whose session tag was lost.
- Existing entries in the current task/workspace map and durable manifest
  binding must agree with the requested workspace. A workspace guarded by `binding_epoch`
  must already have an explicit matching durable binding; the agent's task ID
  alone cannot create one. Preview reports `workspace_binding_epoch` and commit
  must echo it. Adoption preserves the epoch and persists the verified binding,
  including for headless restart.
- No other continuous record owns the session or planning task. An existing
  continuous record must be on this host, paused, settled, and free of admission,
  drain, reconciliation, retirement and recovery holds. Its engine, planning
  task and workspace must match. A different live orchestrator is refused.
- Native crons are owned by their engine. **CM cannot invoke Claude CronDelete or
  retire a cron on another engine's behalf.** The cron owner must remove it using
  its supported engine interface and verify removal. The operator supplies the
  owner UID, cron IDs and evidence. A still-observed cron on the task, selected
  agent or named old owner returns `unsupported_owner`. External schedulers and
  remote cron owners remain the operator's verification responsibility.
- A task channel must be available before activation. On a messaging replica,
  the operator may need to create `ct/<continuous-task-id>` through the hub first
  and wait for replication. Adoption uses the existing channel reconciler and
  waits until the orchestrator is bound and joined. Naming/other member joins
  can converge later and are disclosed in the channel report. An adoption-created
  task subscription persists its starting checkpoint once: historical messages
  stay readable but do not replay as an initial native wake. New messages after
  that checkpoint remain eligible, including across crashes and retries.

Ownership checks cover the target daemon's local records, a strict inventory of
legacy records on the planning API host (normally manager), and central adoption
claims. A task directory with no `state.json` is ignored because scheduler lock
or run-log remnants do not establish ownership. A present `state.json` is opened
without following symlinks; unreadable, malformed, non-regular, dangling or
symlinked state entries fail closed. The API's best-effort `/continuous` display
is not used to establish absence. This is **not
global atomic exclusion** of legacy schedulers: an unregistered record on any
other remote host, or a concurrent legacy `continuous.create` that bypasses the
new claim route, still requires explicit operator reconciliation before use.
Do not use adoption as a general cross-host registry migration.

## Preview, review and commit

Use the maintained operator helper on the **existing session's host**. Do not
run `continuous.create`, `continuous.run_now`, `trigger` or `migrate_engine` as a
substitute. The latter migrates an existing retired continuous record, clears
its current session and cannot adopt a live agent. `continuous.update` only
edits configuration; `ensure_channel` only repairs channel bindings. Workflow
existing-session binding shares the adoption exclusion lock but is not a
continuous scheduling route.

Prepare `preview.json` with a new request ID and the identities you inspected:

```json
{
  "request_id": "planning-adoption-1",
  "task_id": "planning",
  "expected": {
    "planning_task_id": "00000000-0000-0000-0000-000000000001",
    "session_uid": "ts-existing",
    "workspace_id": "existing-workspace",
    "worktree_path": "/absolute/existing/task/workspace",
    "engine": "codex"
  },
  "schedule": {"kind": "periodic", "every_secs": 10800},
  "first_fire_at": 1793100000,
  "label": "Planning",
  "default_prompt": "Reconcile the planning queue.",
  "native_cron": {
    "status": "removed_by_owner",
    "owner_session_uid": "ts-old-cron-owner",
    "cron_ids": ["old-cron-id"],
    "evidence": "Reference to the owner's verified removal receipt"
  },
  "dry_run": true
}
```

The timestamp above is illustrative; choose an actual future UNIX second.
Only `periodic` with a positive `every_secs` is supported. `cron` returns
`unsupported_schedule`: CM's Cron variant currently has no scheduler evaluator.
For a task with no native cron, use
`{"status":"none","evidence":"Operator's verification reference"}`. A request
to retire a cron automatically returns `unsupported_owner`.

```bash
scripts/cm-op --ssh cm-sessions continuous.adopt_existing "$(cat preview.json)" > preview-reply.json
```

Preview performs reads only: no directories or lock files, task/session writes,
channel joins, registration claims, initial prompt or spawn. It returns the
observed host/daemon/process identity, continuous-state hash (null for a new
record), and planning-registration snapshot. The target task/session/workspace/
engine must already match. Host, daemon ID and process fields may be omitted
only in preview; explicitly incorrect values are refused.

After reviewing an `ok: true` preview, copy its `result.expected`,
`result.expected_state_hash` and `result.expected_registration` into the request,
set `dry_run` to false, and save the exact resulting JSON as `commit.json`.
The commit must echo all process fields, including PID, kernel process start
time, holder incarnation and workspace binding epoch. Keep the same ID and
**identical payload** for all retries. Changed identities require a new preview
before any intent exists.

```bash
scripts/cm-op --ssh cm-sessions continuous.adopt_existing "$(cat commit.json)" > adoption-reply.json
scripts/cm-op --ssh cm-sessions continuous.list '{}'
```

A successful reply has `adopted: true`, `receipt`, and `current` readback with
`current_session_uid`, workspace, engine, schedule and `next_fire_at`.
Read back through the **owning daemon**: the manager API's `/continuous` listing
reads manager-local records and does not aggregate schedules on `cm-sessions`.
`receipt.initial_fire` and `receipt.spawned` are false. Existing run counts,
run history, label, prompt and model configuration are retained on rebind;
new registrations inherit the live agent's model and effort. Adoption disables
immediate supervision; subsequent scheduled fires use the normal persistent
executor. Planning status,
lead, models, description and unrelated metadata/history remain intact.

## Retry and failure behavior

The daemon fsyncs an intent in `~/.cm/continuous-adoptions/<request-id>.json`
before claiming planning ownership. A paused record fences every intermediate
step. The API reserves the planning task under a row lock, atomically changes
ONESHOT to continuous and merges its continuous metadata; an immutable request
receipt makes a timeout or lost response safe to retry. The daemon then persists
the session binding, reconciles the channel, rechecks current planning ownership,
kind, status and scheduler/engine metadata, and finally fsyncs activation.
An old API receipt never overrides a task closure or changed planning metadata.
No startup recovery automatically resumes an incomplete adoption.

Task-channel reconciliation uses the same local binding gate as adoption before
reading task/session state and through channel, naming, membership, subscription
and fencing effects. Background refresh skips pending intents, even an
intent-only crash before staging, preserving existing subscriptions. Deferred
replica joins recheck the binding generation under that gate before forwarding.
Only the adopting request can reconcile its own staged channel.

A skipped refresh is reported as deferred, never as proof of subscription
authority. Task-monitor reads, acknowledgments and native wakes independently
validate the authoritative task owner, channel and subscription generation under
nonblocking per-task lifecycle and state-file guards. Busy/unknown authority
leaves task hits and pending native work unconsumed for retry. Ordinary chat and
personal monitors and their native wakes remain available. Only batches holding
uncertain task keys wait; independent wakes bypass their latch. Never-published
mixed batches are separated in one durable checkpoint before personal publication.
During same-UID re-adoption, that retention covers queued prior generations of
the exact subscription for the same actor: the channel checkpoint can advance
the cached generation before activation. Obsolete generations reconcile only
after authority becomes known; retention does not authorize reading or acking
the old task monitor.
Already-published or ambiguous batches keep their identities and receipts; they
are not duplicated or retracted on unknown authority. The delivery path retains
the guards through local publication, without blocking on session/store locks.
The reserved `-orchestrator` suffix requires current authority when an agent
claims a name; this does not revoke an already-published name globally.

`chat_open().continuous` is an orientation snapshot loaded before the effect
guards, not proof of current ownership. It can briefly show an orchestrator
while `task_subscriptions` is empty during pending adoption or handover; use the
guarded subscription result and owning-daemon readback for authority.

The deferred-join gate covers the local send attempt. A timeout can leave the
remote membership join outcome unknown; this does not introduce a remote lease
or grant scheduler subscription ownership.

The final planning read rejects observed drift; it is **not atomic** with local
activation and does not introduce a distributed lease. Operators must serialize
planning-close, ownership and continuous-metadata edits with adoption. A closed
planning row alone does not revoke the local scheduler: use the owning daemon's
pause/drain controls to stop scheduler admission.

A channel prerequisite returns `adopted: false, pending: true`. Other errors
(including uncertain API responses) can also leave a durable pending intent;
read `continuous.list` and retry the **same** saved request after resolving the
reported prerequisite. Pending adoption blocks update, resume, deletion,
migration, workflow binding, scheduler admission and manual fire. Do not edit
state files or use another request ID to bypass it. If the original process
identity is gone or its planning claim has changed, the request remains fenced
for operator recovery; this first version deliberately has no force/abort route.

A retry that crosses the requested first slot skips whole missed periods and
chooses the next future slot. It never catches up immediately. After the first
scheduled fire, normal Periodic scheduling uses the actual fire time plus the
period, so this is **not a strict wall-clock cron replacement**.

A completed retry returns its original receipt plus current readback. It never
re-enables a later pause, resets a later schedule, steals a subsequent binding,
or recreates a deleted record. Receipts outlive continuous-record deletion.
The adopted workspace cannot be garbage-collected through `continuous.delete`;
workspace cleanup remains a separate explicit operation.

Implementation/tests do not authorize live adoption. Native-cron reconciliation,
activation, deployment and restart each remain with the designated operator.
