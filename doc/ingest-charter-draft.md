# Ingest org: charter draft

Status: **draft for Owner to edit** (Swarm-Coord, 2026-11-06). No Ingest
lead starts until Owner has marked this up. Lines marked **Owner:** are the
decisions only Owner makes. Plan context: [continuous-teams.md](continuous-teams.md).

## Purpose

Keep the data coming in. Scrapers should be up, fresh and complete, new
sources should be added, and momentum and API changes should be noticed.
Ingest owns everything from a source to the stored event. It does not own
what downstream teams do with that event.

**Owner:** is that the right boundary, or does Ingest also own the
quality of events, such as published-at accuracy and dedupe?

## Teams

| Team | Comes from | Wakes on |
|---|---|---|
| Scraper fixes | scraper-triage + scraper-opt; health-alert-triage's scraper alerts | Schedule (now every 6h) |
| Scraper creation | scraper-creation + structured-scraper-creation | Proposal queues |
| Momentum detection | momentum-detective | Momentum queue (consumed by the Ingest lead only) |
| API updates | api-update | Daily |

The lead is Claude Opus. Workers are Codex Terra or Luna. A worker may take
several items, and workers are reused.

## What Ingest decides alone

- Fixing a broken or degraded scraper.
- Re-enabling a recovered source.
- Routine scraper tuning.

**Owner:** which of these still need your sign-off, for example disabling
a source, or a source that needs paid access or a proxy.

## What goes to Owner

- New sources: does every new scraper need your OK, or only some kinds
  (paid, legally grey, high volume)?
- Dropping a source for good.
- Anything that costs money.

**Owner:** fill in the rule.

## What gets relayed

- API changes that affect trading or the prob pipeline go to that org's
  lead as a board item. Ingest does not fix them.
- Bugs Ingest finds outside ingestion are relayed, not fixed.
- Before staffing anything, Ingest checks the org boards and channels. If
  someone already holds the problem, Ingest links to it and stops.

## Priorities

**Owner:** rank these, or add your own.

1. Sources the live trader depends on.
2. Freshness: delay from published to stored.
3. Coverage: new sources.
4. Cost and efficiency.

## Done means

**Owner:** what tells you Ingest is working? For example: no source dark
for more than N hours without an open item, or a weekly digest.

## Starting backlog

The open items tagged `KEEP->Ingest` in the phase-1 inventory, mostly from
scraper-triage and health-alert-triage.
