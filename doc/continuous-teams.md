# Continuous tasks become continuous teams

Status: plan agreed with Owner 2026-11-06. Phases 1–4 are live; phase 5
(retire the legacy TUI column and reject new legacy creates) landed 2026-11-21.
Phase 6, the two latency optimization teams, went live the same day. The Ingest lead (Claude Opus, member
`ingest`, division `ingest`, marked continuous) runs on cm-manager under
[ingest-charter-draft.md](ingest-charter-draft.md) v3. A few legacy scheduler
records remain and keep running until migration is complete.

## Why

Continuous tasks run apart from the org. Orchestrators pick work without
seeing what the rest of the org is doing, so they duplicate or drift.
Branches and worktrees pile up. Triage stalls until Owner kicks it off by
hand. Each item gets its own fresh worker, which is too rigid.

## The model

A continuous task becomes an ordinary **team** in an org, with two extras:

- its lead runs on the continuous host (cm-manager) and wakes on a
  **schedule** or a **queue**;
- CM promises that schedule: it records runs, detects missed runs, and
  shows them on a dashboard.

Everything else is shared with ordinary teams:

- Leads are Claude Opus.
- Workers are Codex Terra or Luna under the staffing policy.
- Workers are reused, and one worker may take several items.
- Items live on the team's normal board.

### Triage flow

1. The continuous lead finds a problem.
2. It checks the org's channels and board. If someone already holds the
   problem, it links there and stops.
3. Otherwise it surfaces the problem to its **org lead** as a board item.
4. The org lead decides who handles it: the continuous team, the org
   lead itself, or another team or org.
5. Only then does the continuous team staff the fix.

Items that belong to another org are relayed to that org.

## Feature placement

The test for each feature: would we want it outside continuous tasks?

| Feature | Where it lives |
|---|---|
| Periodic wake | Any session. Signal-tracking audit and decision watch use it too. |
| Queue wake (e.g. the momentum queue) | A queue tool bound to one lead; only that lead may consume it. |
| Item tracking | The team board (`doc/items-board.md`). |
| Usage-limit detection and auto-resume | Every session. |
| Per-task channel `ct/<slug>` | The team channel, plus one shared `#continuous` channel. |
| Run history, missed-run detection, dashboard, host placement | **Continuous only.** This is what defines a continuous team. |
| Engine migration, drain | Continuous admin; keep, simplify later. |

## Team map

**Ingest (new org).** One Claude Opus lead; everything below runs on
Terra or Luna. Owner writes the org's guidelines before any lead starts.

- Scraper fixes: scraper-triage and scraper-opt merged.
- Scraper creation: scraper-creation and structured-scraper-creation merged.
- Momentum detection: momentum-detective; the queue is consumed by its lead.
- API updates: api-update. This is not strictly ingest work; Ingest relays
  findings for other orgs to those orgs.

**Execution policy.** One reliability team covering bug triage, perf
triage and code bug hunt for the execution side.

**Prob pipeline.** The same reliability team, for the prob pipeline side.

**Latency optimization (Owner 2026-11-15; set up 2026-11-21).** The goal is
to shorten the time from receiving an article to the first trade decision.
Owner chose two continuous teams, one under each pipeline lead (option A),
over one cross-functional org:

| Team | Lead (manager) | Stretch | Target |
|---|---|---|---|
| `latency-pp` | LatencyPP-Lead (PP-Lead) | article stored → signal emitted | set from the baseline |
| `latency-ep` | LatencyEP-Lead (EP) | signal emitted → first decision | p99 ≤ 30 s |

- Each team's lead member is a Claude Opus session (Owner, 2026-11-21:
  the leads reason about the whole stack and launch workers). It is marked
  continuous on host `sessions` with a 3 h `session_wake_schedule`. The
  Codex Terra workers that ran the first passes stay on as its measurement
  workers, and it starts Codex Terra workers for fix lanes. The pipeline
  lead admits fix lanes, and code lands through that lead's normal gates.
- The handoff point is the signal-emission timestamp as PP records it.
  Reports give p50/p90/p99 with n over a stated window, from replica rows,
  and name the slowest step.
- The two teams share `#latency` (EP, PP-Lead, Ingest-Lead, Owner). The brief
  is pinned there. Ingest keeps published → stored and posts its numbers there
  weekly.
- Owner's sidebar groups both teams in one "Latency" section, assigned per
  workspace. The teams themselves stay in their leads' orgs.

Baselines and the reports from each pass are in
`~/.local/share/latency-ep/BASELINE-1121.md` and
`~/.local/share/latency-pp/BASELINE-1121.md`.

**Leads (Owner, 2026-11-15).** Every continuous team lead is Claude Opus,
the Ingest lead included.

**Retired.**

- health-alert-triage: replaced by a scheduled check session.
- The four codex-migration-* canaries.
- behavior-triage: paused; retiring (Owner, 2026-11-06).

## Sidebar and dashboard

- The continuous column is retired. Continuous teams render in the main
  sidebar, inside their org's section.
- Each continuous team is a **boxed block with its own background tint**.
- The box header reads `⟳ every 6h · next 1h12m · ok`, so the team is
  clearly marked at a glance.
- The continuous-teams dashboard is available from `Alt+C`, or from the F9
  Global Settings dialog with `c`. It lists every continuous team with:
  - its lead session;
  - last run and next run;
  - missed runs;
  - blocked state;
  - bound queue depth and oldest item age;
  - open items.
- The daemon also ensures a shared `#continuous` channel at startup. Its
  monitor checks every 30 seconds and posts one alert, then one clear, for a
  missed run, a run late by more than half its period, an old queue item (one
  hour), or a dead lead. The monitor is deterministic daemon code; no Luna
  session is needed for these checks. Its open-problem latch is persisted
  under `~/.cm`; each re-raised episode gets a new notice key, so a problem
  that clears and returns is visible again after a restart.

Org-model continuous teams use the `continuous` marker on their team in
`org.snapshot`. Its stable read shape carries `lead_member_id`,
`host_id`, `lead_session_uid`, `next_run`, `last_run`,
`missed_count`, `health` (`ok|late|missed|blocked`), cadence metadata,
and `queues` filtered to that lead. The dashboard projects those teams
alongside legacy continuous-task rows; their missed alerts come from the wake
run's `missed` outcome, not from `next_fire_at` arithmetic.

### Continuous-team marker and live read

Owner/operator (or a session with global permissions) marks an ordinary org
team with `org(action="mark_continuous", team="<id>",
continuous={"lead_member_id":"mem_…", "host_id":"cm-manager"})`.
The marker names the durable lead member and the host; it does not copy a
schedule or queue state. `org.snapshot` and `org.team` decorate the marker
with live state from that lead's daemon-owned wake schedules and queues:

```json
{
  "lead_member_id": "mem_…", "host_id": "cm-manager",
  "lead_session_uid": "ts-…", "next_run": 1731675600.0,
  "last_run": {"outcome": "delivered", "scheduled_at": 1731654000.0},
  "missed_count": 0, "health": "ok",
  "queues": [{"queue": "momentum", "pending": 3, "claimed": 0,
               "binding": {"consumer": "mem_…"}}]
}
```

`health` is one of `ok`, `late`, `missed`, `blocked` or `unknown`. A missing
lead on its host or an unavailable local schedule is `blocked`; an unreachable
marker host is `unknown` until that host's projection is read. Queue rows retain the
stable `queue_list()` fields and are filtered to queues bound to the lead (or
explicit queue names in the marker). The Task sidebar renders the lead's
sessions in the org section as a tinted box. Its header is `⟳ every 6h · next
1h12m · ok`; a queue-only lead uses `⟳ queue · 3 pending · ok`. The dedicated legacy continuous column is retired in phase 5; old scheduler
sessions now render in this same org section.

Live fields are refreshed even when the planning org version is unchanged.
The laptop merges each marked team's projection from its `host_id` daemon;
when that host is unreachable the team remains `unknown` rather than being
reported as blocked. `org.continuous_status(team=...)` is the small direct
read for a host-local dashboard or monitor.

## Phases

1. **Clear the backlog.** Inventory the open items, branches and worktrees
   of all continuous tasks. Close what is stale and assign the rest to the
   target teams.
2. **Ingest pilot.** Draft the charter outline, then wait for Owner's
   guidelines. Then stand up the Ingest lead and its teams.
3. **CM features.**
   - Periodic wake for any session.
   - Lead-bound queue wake.
   - The continuous-team marker.
   - The boxed sidebar block.
   - The dashboard, `#continuous`, and the monitor agent.
4. **Move the execution policy and prob pipeline teams.**
5. **Retire** the continuous column and reject new legacy scheduler creates.
   Existing records keep running while teams are migrated; scheduler internals
   remain until the legacy record count reaches zero. Docs now describe team
   setup.
6. **Latency optimization teams** (Owner, 2026-11-16: after phases 4 and 5).
   Live since 2026-11-21: `latency-ep` and `latency-pp`, as in the team map.

## Prerequisites

- cm-manager's Claude login must be restored before Opus leads can run.
- Codex quota in cm-manager's pool is needed for the workers.

## Lead-bound queue wake

Named queues can be bound to a durable org member id or handle with
`queue_bind(queue, consumer)`. The binding follows that member's replacement
main session. The member itself, Owner/operator, or a global-permissions
orchestrator may bind or rebind it; the binding records the actor.

`enqueue` keeps the existing producer contract. For a bound queue, the first
enqueue in a burst publishes one durable native `[cm-queue …]` wake to the
member's current main session and includes the pending depth. Further enqueues
coalesce while the wake is pending. A missing main session or failed publish
leaves `wake_pending` and `wake_error` visible for retry; items remain queued.

Only the bound member may call `queue_claim` and `queue_ack`. A successful
claim clears the coalesced `wake_pending` latch (even when other items remain
pending), so a later enqueue can request the next wake; an empty queue also has
no pending wake. Claims carry a lease and a claim token; an expired lease
returns the item to `pending`, while an acknowledgement moves it to
`consumed`. Unbound continuous-task queues use the existing scheduler
claim/ack behavior unchanged. Binding a queue hands it from the legacy
continuous orchestrator to the lead; the scheduler skips that queue after
recording the handoff.

`queue_list()` (and the `queue.list` RPC) returns one stable row per queue:
`queue`, `pending`, `claimed`, `binding` (`consumer`, `bound_by`, `bound_at` or
null), `oldest_pending_at`, `oldest_item_age_seconds`, `last_wake`,
`last_consume`, `wake_pending`, `wake_delivery`, `wake_error`, and the current
coalescing `wake_id`.

## Scheduled wake for any session

Any live session can keep a daemon-owned wake schedule. Use
`session_wake_schedule(action="create", interval_secs=21600)` for an interval
or `daily_at="13:00Z"` for a UTC daily wake. `jitter_secs` adds bounded,
deterministic jitter and `wake_message` supplies the short notice text. The
schedule targets the caller by default; a global-permissions session or a
parent may pass a live descendant's `session_uid`. Update and cancel with the
same tool and `schedule_id`; read with `session_wake_schedules()`.

Schedules are persisted in the daemon's `~/.cm/session-wake-schedules.json`
store and survive a brain restart and respawn at the same uid. Delivery uses
the native notification queue, with a marker of `[cm-schedule <id> <run>]`.
Each run records `scheduled_at`, `delivered_at` (when queued), and an outcome:
`delivered` or `missed` with `reason` (`session_dead`, `delivery_failed`,
`stale`, or `brain_down`). A busy target does not drop a wake: the native queue
holds the notification until the session can consume it. If the previous wake
from this schedule has not produced a later completed turn, the next due slot
is recorded as `coalesced` against that wake and is not published separately.
After one full period without a later turn, the old wake is marked `missed` with
`stale` and the next slot is published. Coalesced slots do not increment
`missed_count`.

A crash leaves a pending run that is retried once with the same dedupe key after
restart, so elapsed intervals do not fan out into a burst. The scheduler keeps
the schedule store in memory and checkpoints it only when state changes.

The read shape is stable:

```json
{
  "id": "wake-…", "session_uid": "ts-…",
  "kind": {"type": "interval", "every_secs": 21600},
  "enabled": true, "state": "active", "next_run": 1731675600.0,
  "last_run": {"run_id": "…", "scheduled_at": 1731654000.0,
    "delivered_at": 1731654000.2, "outcome": "delivered", "reason": null},
  "missed_count": 0
}
```
