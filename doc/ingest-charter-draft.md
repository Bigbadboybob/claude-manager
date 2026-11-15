# Ingest org: charter draft

Status: **draft v3.** Owner answered the open questions 2026-11-15
(Swarm-Coord). claudelb is ready. The lead launches once cm-manager has Codex (bender2, after its
2026-11-15 23:27Z refill). Plan context:
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
- proxies and IP rotation, but only once the other options here fail
  (see the spend cap);
- other endpoints for the same content: RSS, sitemaps, AMP pages, APIs,
  mirrors and syndication partners;
- request timing, caching headers and conditional requests;
- anything else that works.

Auto-disabling a source after it gets blocked is a temporary backoff, and
it always comes with an open item to restore the source. It is never the
end state. Only Owner retires a source we want.

Any older ruling, prompt, skill or doc that says "if blocked, stop", "do not
work around blocks" or similar is **void** (Owner, 2026-11-06). Such rules
were purged from the predictionTrading repo in 0853ed43f45. The same rules
still sit in the continuous tasks' state and memory on cm-manager. They are
rewritten when those tasks move to Ingest. Ten code paths still pause or
retire blocked sources; changing them is Ingest's first backlog.

**Spend cap (Owner, 2026-11-15):** Ingest may use the current Oxylabs plan
freely, within its limits:

| Oxylabs product | Monthly limit | IPs |
|---|---|---|
| ISP proxies | no GB limit | 10 |
| Datacenter proxies | 5 GB | 5 |
| Residential proxies | 5 GB | rotating |
| Web Unblocker | 8 GB | n/a (fallback only, after a direct fetch fails) |

**Never exceed a monthly limit (Owner, 2026-11-15).** If a pool goes over
its limit, every scraper using that pool stops working until the monthly
reset. So:

- **Proxies are a last resort (Owner, 2026-11-15).** If a source works
  without a proxy, it gets no proxy. That includes direct fetches with the
  right headers, timing or fingerprint, another endpoint, RSS, or an API.
  A proxy is added only after the non-proxy options in the blockers section
  have failed. The playbook records what was tried.
- When a proxy is needed, prefer ISP proxies, which have no GB limit. Keep the 5 GB datacenter and
  residential pools for sources that truly need them.
- Before routing a source through a metered pool, estimate its monthly GB:
  bytes per fetch times fetches per month. Record that estimate in the
  playbook.
- Track usage per pool against its limit, as a share of the month's budget
  spent so far.
- Keep headroom. When a pool reaches 70% of its limit, stop adding sources
  to it. At 85%, move the heaviest non-essential sources off it, or slow
  their fetch rate. Sources the live trader depends on keep their access
  last.
- Tell Owner as soon as a pool looks set to run out before its reset.
- Usage source: the Oxylabs Dashboard API (`GET https://api.oxylabs.io/stats/v1/usage`)
  once Owner obtains a key from Oxylabs support. The key lives in
  `~/.config/oxylabs/dashboard-api-key` on cm-manager (0600), and nobody
  prints it. Until the key arrives, and for any product the API does not
  cover, Ingest counts proxied bytes per pool in the scrapers and keeps a
  wider margin. The following go to Owner first: a plan upgrade,
overage, more IPs, or any new paid scraping service or provider.

## The scraping playbook

All scraping know-how lives in **one folder**, the scraping playbook at
`prediction/ingest/playbook/` in predictionTrading (landed 0853ed43f45). It holds:

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
- Adding a new source.
- Scraper tuning for freshness, reliability and quality.

## What goes to Owner

- Retiring a source we want.
- Spend beyond the cap above.

**New sources (Owner, 2026-11-15):** Ingest adds them on its own. No Owner
OK is needed.

## What gets relayed

- API changes that affect trading or the prob pipeline go to that org's
  lead as a board item. Ingest does not fix them.
- Bugs Ingest finds outside ingestion are relayed, not fixed.
- Before staffing anything, Ingest checks the org boards and channels. If
  someone already holds the problem, Ingest links to it and stops.

## Priorities

Order confirmed by Owner, 2026-11-15.

1. Sources the live trader depends on.
2. Freshness: delay from published to stored.
3. Alpha discovery: finding what moves markets.
4. Coverage: new sources.
5. Cost and efficiency.

## Done means

Agreed with Owner, 2026-11-15: no wanted source is dark for more than 12
hours without an open item, and Ingest posts a weekly digest.

## Starting backlog

The open items tagged `KEEP->Ingest` in the phase-1 inventory, mostly from
scraper-triage and health-alert-triage.
