# How to run a continuous team

A continuous team is an ordinary org team with a durable lead, a wake promise, and
health reporting. The lead is a Claude Opus session on the continuous host. Its
workers are ordinary Codex or Luna sessions, reused across items. Work belongs on
the team's normal board and in the team's channel.

The old `continuous.create` scheduler creates legacy tasks. It refuses new task
ids now; keep it only for the few records that are still running (see **Legacy
records** below). Read [doc/continuous-teams.md](doc/continuous-teams.md) for the
model and phase history.

## Stand up a team

1. Create the org team and its lead member/session using the normal org and
   session tools. Give the lead a short charter and a task/workspace so it is
   visible with the rest of its org.
2. Mark that team continuous. The marker names the durable member and host; it
   does not create a second task or copy runtime state:

   ```text
   org(action="mark_continuous", team="reliability",
       continuous={"lead_member_id":"mem_…", "host_id":"cm-manager"})
   ```

3. Add the cadence to the lead session. An interval is usually enough; a daily
   UTC wake can use `daily_at` instead:

   ```text
   session_wake_schedule(action="create", interval_secs=21600,
                         wake_message="Run the reliability sweep")
   ```

   The schedule is daemon-owned and survives a brain restart or session respawn.
   Each run records `scheduled_at`, delivery, and either `delivered` or
   `missed`. Read it with `session_wake_schedules()`.

4. Bind event-driven work to the lead. The queue binding follows the member's
   replacement main session:

   ```text
   queue_bind(queue="momentum", consumer="mem_…")
   ```

   The first enqueue in a burst sends one durable native wake. Only the bound
   member claims and acknowledges that queue. Use `queue_list()` for pending
   depth, oldest age, wake state, and the binding.

5. Create or join the team's task channel (`ct/<slug>`) and keep the shared
   `#continuous` channel enabled. Workers post handoffs in the team channel and
   mention the lead; the mention wakes the lead before the next scheduled scan.
   The daemon also keeps a deterministic `#continuous` monitor. It checks every
   30 seconds for missed or late wakes, old queue items, and a dead lead, then
   posts one alert and one clear per incident.

6. Put the team on its org board. The lead triages there, checks existing owners,
   and raises work to the org lead before staffing a new fix. Reuse workers when
   possible and keep the task channel as the handoff record.

## Watch health

The Sessions view shows a marked team inside its org section as a tinted box.
Its header reports cadence, next/last run, and health (`ok`, `late`, `missed`,
`blocked`, or `unknown`). Queue depth and open items are shown with the team.
All sessions, including old scheduler orchestrators and their workers, are in
the main sidebar; there is no dedicated continuous column. `Alt+C` opens the
continuous-teams dashboard (F9 Global Settings, then `c`, is equivalent).

The dashboard lists each team's lead, schedule, missed runs, blocked state,
bound queue age/depth, and open items. `org.snapshot` and `org.team` carry the
same live projection. `org.continuous_status(team="…")` is the small host-local
read used by monitors.

A healthy run means the lead consumed the wake and recorded a turn. A late or
missed run is an operational signal, not a reason to create a replacement lead.
Check the lead session, schedule, queue binding, and channel history first.

## Legacy records

`continuous.list` remains the health read for legacy records. Existing records
keep their scheduler, worktree, and run history; this phase does not drain or
remove those internals. `continuous.pause`, `continuous.run_now`,
`continuous.update`, and `continuous.delete` remain available for migration and
operator recovery.

A **new** `continuous.create` returns an invalid-parameters error that points to
this document. It does not clone a repo, create a worktree, register a workspace,
or create a channel. To migrate a record, first create the ordinary team and
lead, mark it continuous, add the wake schedule, bind its queue, verify the
team/channel/dashboard, and only then retire the old record under the normal
continuous-task drain procedure.

## Useful checks

```text
org.snapshot()
org.continuous_status(team="reliability")
session_wake_schedules()
queue_list()
continuous.list()
```

Keep the short shared review policy in
[doc/continuous-review-routing.md](doc/continuous-review-routing.md), and use
[doc/AGENT_QUICKSTART.md](doc/AGENT_QUICKSTART.md) for cross-host session and
messaging details.
