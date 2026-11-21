# How to run a continuous team

A continuous team is an ordinary org team with a durable lead, a wake promise,
and health reporting. The lead is a Claude Opus session on the continuous host.
Its workers are ordinary Codex Terra or Luna sessions, reused across items. Work
belongs on the team's normal board and channel.

The old `continuous.create` scheduler creates legacy tasks. It refuses new task
ids now; keep it only for records that are still running. Read
[doc/continuous-teams.md](doc/continuous-teams.md) for the model and migration
status.

## Stand up a team

1. Create the org team and lead member/session with the normal org and session
   tools. Give the lead a charter, task/workspace, and the team's board.
2. Mark the team continuous. The marker names the durable member and host:

   ```text
   org(action="mark_continuous", team="reliability",
       continuous={"lead_member_id":"mem_…", "host_id":"cm-manager"})
   ```

3. Add a daemon-owned cadence to the lead:

   ```text
   session_wake_schedule(action="create", interval_secs=21600,
                         wake_message="Run the reliability sweep")
   ```

   Read schedules with `session_wake_schedules()`. Runs record delivery or a
   missed outcome and survive a daemon restart or lead respawn.

4. Bind event-driven work to the lead:

   ```text
   queue_bind(queue="momentum", consumer="mem_…")
   ```

   The first enqueue in a burst sends one durable native wake. Only the bound
   member claims and acknowledges the queue. `queue_list()` reports depth, age,
   binding, and wake state.

5. Create or join the team's task channel (`ct/<slug>`) and keep the shared
   `#continuous` channel enabled. Workers post handoffs there mentioning the
   lead; the mention wakes the lead before the next scheduled scan. The daemon's
   deterministic `#continuous` monitor checks every 30 seconds for missed or
   late wakes, old queue items, and a dead lead.

6. Triage on the team's board. Check existing owners first, raise cross-team
   work to the org lead, and reuse workers where possible.

## Watch health

The Sessions view shows a marked team inside its org section as a tinted box.
The header reports cadence, next/last run, and health (`ok`, `late`, `missed`,
`blocked`, or `unknown`). The continuous-teams dashboard is on `Alt+C`, or F9
Global Settings followed by `c`; it lists the lead, wake history, missed runs,
bound queue depth/age, blocked state, and open items.

Legacy scheduler orchestrators and their workers are ordinary main-sidebar rows.
There is no dedicated continuous column. `A-h` and `A-l` are free, `A-g` searches
the single sidebar, and an old persisted column flag is ignored.

## Legacy records

`continuous.list` remains the health read for legacy records. Existing records
keep their scheduler, worktree, and run history; this phase does not remove
scheduler internals. `continuous.pause`, `continuous.run_now`,
`continuous.update`, and `continuous.delete` remain available for migration and
operator recovery.

A **new** `continuous.create` returns an invalid-parameters error pointing to
this document. It performs no repo clone, worktree creation, workspace
registration, or channel creation. Create the ordinary team, mark it continuous,
add its wake schedule and queue binding, verify the team/channel/dashboard, then
retire the old record through the normal drain procedure.

## Useful reads

```text
org.snapshot()
org.continuous_status(team="reliability")
session_wake_schedules()
queue_list()
continuous.list()
```

See [doc/continuous-review-routing.md](doc/continuous-review-routing.md) for
review handoffs and [doc/AGENT_QUICKSTART.md](doc/AGENT_QUICKSTART.md) for
cross-host session and messaging details.
