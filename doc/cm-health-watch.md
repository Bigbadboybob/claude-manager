# CM fleet and Codex pool notifications

`scripts/cm-health-watch` is a read-only cron observer. It sends through the
existing `cm-notify` command, independently of any session. It never launches,
pauses, resumes, repairs, or retires work and makes no model requests.

## Pool alerts

The cm-manager and laptop pools contain the same upstream accounts. Run one
notifier on cm-manager: account quota is shared even though requests and OAuth
enrollments are independent. The dashboard's zero purchased-credit balance does
not mean the subscription quota is exhausted.

- Warn when every usable account has at most 10% headroom in its limiting
  reported quota window. Clear the warning above 20% to avoid repeated alerts.
- Escalate when all enabled accounts have exhausted their reported quota.
  Include the earliest expected account reset, accounting for every blocking
  window on each account.
- Report unavailable authentication/status separately from quota exhaustion.
- Missing or stale usage is unknown, never zero. Three consecutive failed
  monitoring checks produce a diagnostic. Existing incidents remain active.
- Deduplicate incidents across cron runs and send one recovery notification.

The reader selects only status and quota columns from the LB SQLite database in
read-only mode. It does not read or export OAuth tokens or account email addresses.
Window durations come from LB; `primary` is not assumed to mean five hours.
Positive purchased/unlimited capacity prevents a false subscription-exhaustion
alarm. Model-specific and conversation-affinity failures remain task diagnostics;
an individual session failure does not prove that the whole pool is empty.

## Fleet and migration observations

The operator RPC checks daemon/holder health, missing configured task definitions,
active-task holds/failures, and eligible idle work overdue by ten minutes. Consumer
eligibility respects queue age and depth thresholds. The overdue grace starts after both queue eligibility and the scheduler's `next_fire_at`: old queued work must not trigger an alert during an intentional post-compaction delay or scheduler backoff. Work still waiting after that deadline plus the grace is overdue. Paused tasks stay paused.

Migration observations correlate successful `report_done` audit events with their
admission by sequence, session UID, and fire token. Handover-only cycles do not
count; consumer gates require completed nonempty batches. Scheduled observations
use the `ft_sched_` admission token because historical trigger-source fields can
say `operator` for scheduled work. API observation also requires a full 24 hours
after its first successful cycle. A runtime-ready notification is not artifact
or scope approval. Originally paused tasks require separate validation when their
owner resumes them.

## Installation and operation

Install the executable as `~/.cm/bin/cm-health-watch` and configure
`~/.cm/health-watch-config.json` (mode 0600). The configuration contains
`notify_command`, optional `pool` (`database`, `low_percent`, `recovery_percent`,
`stale_seconds`), optional `fleet` (`tasks`, `overdue_seconds`), and optional
`migration` rules keyed by task ID (`baseline_seq`, `completed`, `scheduled`,
`nonempty`, `observe_seconds`). Defaults are 10%, 20%, 1200 seconds, ten minutes,
and three consecutive monitoring failures. Migration defaults require two
completed cycles; other counts default to zero. Thresholds are operator policy,
not OpenAI guarantees of remaining requests.

Dry-run using a separate state file, inspect its snapshot, then enable delivery:

```sh
~/.cm/bin/cm-health-watch --state /tmp/cm-health-watch-review.json
~/.cm/bin/cm-health-watch --notify
```

The durable state defaults to `~/.cm/health-watch-state.json`, written atomically
under an exclusive nonblocking lock. Keep it when upgrading to preserve incident
deduplication. It includes private task metadata and must not enter source control.
The log contains only check time, failure counters, and delivery enablement;
exception payloads are deliberately omitted.

```cron
*/5 * * * * /home/lucas/.cm/bin/cm-health-watch --notify >> /home/lucas/.cm/health-watch.log 2>&1 # cm-health-watch
```

Replace only the marked temporary `cm-codex-migration-pilot-watch` cron line,
preserving a backup and all independent probe/update jobs. This observer has no
expiry. Neither installation nor replacement requires a daemon or LB restart.
After final manual migration signoff, remove the optional `migration` configuration block; pool and fleet monitoring continue. Disable the marked cron line to stop notifications; task scheduling is unaffected.

Tests: `python3 -m unittest discover -s tests -p test_cm_health_watch.py`.
