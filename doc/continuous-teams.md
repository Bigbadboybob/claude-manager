# Continuous tasks become continuous teams

Status: plan agreed with Owner 2026-11-06. Phases below; nothing retired yet.

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
- An F9 page lists every continuous team with:
  - last run and next run;
  - missed runs;
  - blocked state;
  - open items.
- A Luna monitor agent watches it and posts problems in `#continuous`.

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
5. **Retire** the continuous column and the old scheduler paths. Rewrite
   the docs on creating a continuous team.

## Prerequisites

- cm-manager's Claude login must be restored before Opus leads can run.
- Codex quota in cm-manager's pool is needed for the workers.
