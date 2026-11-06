# Ingest org: charter draft

Status: **draft v2.** It has Owner's first review folded in (Swarm-Coord,
2026-11-06). No Ingest lead starts until Owner signs off and claudelb is
ready. Lines marked **Owner:** are the decisions still open. Plan context:
[continuous-teams.md](continuous-teams.md).

## Purpose

Ingest owns **every information source** we trade on: news, scrapers, APIs
and structured feeds. Its job has two halves.

1. **Alpha discovery.** Find out which news and information moves the
   markets. Then make sure we track it. The momentum pipeline is the core
   of this half.
2. **Delivery.** Get that information as fast and as reliably as possible,
   by any means necessary. Ingest owns the whole path from the source to
   the stored event:
   - coverage;
   - freshness and latency;
   - reliability;
   - quality: published-at accuracy, dedupe and completeness.

## Blockers are problems to solve, never reasons to stop

A source we want that blocks us is a bug for Ingest to fix. Ingest never
stops scraping a wanted source because it got blocked. The full toolbox is
in bounds:

- headers, user agents and browser fingerprints, rotated as needed;
- other scraping libraries, browser impersonation and headless browsers;
- proxies and IP rotation;
- other endpoints for the same content: RSS, sitemaps, AMP pages, APIs,
  mirrors and syndication partners;
- request timing, caching headers and conditional requests;
- anything else that works.

Auto-disabling a source after it gets blocked is a temporary backoff, and
it always comes with an open item to restore the source. It is never the
end state. Only Owner retires a source we want.

Any older ruling, prompt, skill or doc that says "if blocked, stop", "do not
work around blocks" or similar is **void** (Owner, 2026-11-06). Such rules
are being purged from the repo and from the continuous tasks' standing
instructions.

**Owner:** is there a monthly spend cap for proxies and paid scraping
services that Ingest may use without asking? Is there any hard line, such
as never using logged-in accounts?

## The scraping playbook

All scraping know-how lives in **one folder**, the scraping playbook in
predictionTrading. It holds:

- how to get around each kind of blocker;
- what worked for which source, and what didn't;
- library and proxy notes;
- how to scrape fresh and cheap.

Every Ingest worker reads it before touching a scraper and adds to it when
it learns something. Older learnings scattered across `NOTES-*` files,
triage reports and prompts are merged into it.

## Teams

| Team | Comes from | Wakes on |
|---|---|---|
| Scraper fixes | scraper-triage + scraper-opt; health-alert-triage's scraper alerts | Schedule (now every 6h) |
| Scraper creation | scraper-creation + structured-scraper-creation | Proposal queues |
| Momentum and alpha discovery | momentum-detective | Momentum queue (consumed by the Ingest lead only) |
| API updates | api-update | Daily |

The lead is Claude Opus. Workers are Codex Terra or Luna. A worker may take
several items, and workers are reused.

## What Ingest decides alone

- Fixing a broken, blocked or degraded scraper, with any tool in the
  blockers section.
- Re-enabling a source.
- Scraper tuning for freshness, reliability and quality.

## What goes to Owner

- Retiring a source we want.
- Spend beyond the cap above.

**Owner:** do new sources need your OK, or can Ingest add any source the
momentum work shows moves markets?

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
3. Alpha discovery: finding what moves markets.
4. Coverage: new sources.
5. Cost and efficiency.

## Done means

**Owner:** what tells you Ingest is working? For example: no wanted source
dark for more than N hours without an open item, or a weekly digest.

## Starting backlog

The open items tagged `KEEP->Ingest` in the phase-1 inventory, mostly from
scraper-triage and health-alert-triage.
