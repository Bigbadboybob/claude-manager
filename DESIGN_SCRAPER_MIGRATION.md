# Scraper-pipeline migration: creation → queue-fed continuous task, optimization → reviewed always-on app

Status: **approved 2026-07-04** (defaults confirmed). This is the sub-design deferred from
`DESIGN_CONTINUOUS_TASKS.md` §16 ("scraper-gen cross-project migration, its own sub-design").
Wave 1 (the generic queue core) is claude-manager work done in this repo; Waves 2–4 are
predictionTrading work handed off with the specs below.

---

## 1. Current state (investigated 2026-07-04)

One asyncio process — `scraperCreation.service` on **aux-east4**, running from
`~/predictionTrading-service` (worktree on the `scrapers` branch) — containing:

- **Creation pipeline** (event-driven): `momentum_events` (Postgres, 10s poll, in-memory
  cursor) → AttributionMerger (Serper + Perplexity + vector-DB RAG; Gemini causation) →
  AttributionFilter (causation ≥60, composite ≥0.45, SKIP_DOMAINS, has-scraper dedup via
  file-scan) → OriginalityFilter (Serper syndication check) → **Telegram HumanReview**
  (24h timeout = reject) → ScraperCreationService: in-process `claude_agent_sdk.query`
  (sonnet, $20 budget, `bypassPermissions`, cwd = repo root, prompt =
  `SCRAPER_CREATION.WORKFLOW.md` → `.claude/skills/scraper-workflow`).
- **Optimization loop** (same process, every 90s): runs all ~148 scrapers **in-process for
  real**, per-scraper `status.json`/`articles.jsonl`, state machine `pending → optimizing →
  success | finished | broken`, spawns headless Claude (opus, $1.5, one-edit-≤30-lines then
  exit; priority ERROR_FIX > INITIAL_ANALYSIS > EVALUATE_EDIT). Accept signal = **locked
  latency target + 10 consecutive good** (main anti-Goodhart device). Edits **hot-reload
  straight into the live loop — no review gate exists.**
- **Value scoring** (separate loop): price-shock × attribution composite → Postgres
  `attribution_scores`; leaderboard/daily-email only — never gates edits.
- **GitManager**: every 12h auto-commits everything under `prediction/ingest/scrapers/` to
  the service branch. Merge to main is manual via `/review-scraper-changes` (also resets
  worktree + restarts service).
- New scrapers auto-enter optimization via class-registry discovery — "file on disk" is the
  only registration.

**Live status:** effectively dormant. 0 creator spawns in 7 days; last auto-commit June 19
(5 new + 5 modified scrapers, still unmerged after 15 days); optimization parked at
0 pending / 8 optimizing / 3 success / 28 finished / **109 broken** with 0 Claude active.
No cutover-continuity concern.

**claude-manager infra state:** `trigger` verb + fire_token idempotency + fresh/persistent
executors + report_done/watchdog/supervision all shipped. The queue layer is Phase-4
design-only: no `enqueue` verb anywhere, no queue table, `Schedule::Consumer` defined but
skipped by the scheduler (`daemon/src/continuous/task.rs:89`, `scheduler.rs` due-check),
`enqueue_to` stored-but-never-read, `trigger.args` accepted but not threaded.

---

## 2. Target architecture

One shared predictionTrading worktree on cm-manager hosts everything that touches scraper
code:

```
                        ┌──────────────────────── cm-manager ─────────────────────────┐
aux (slimmed app):      │  Queue: scraper-creation-proposals (cm API + Postgres)       │
 momentum→attribution→  │    ↓ Consumer schedule: depth ≥ N OR window elapsed          │
 filters ──HTTP POST──► │  Creation orchestrator (persistent, queue-fed)               │
                        │    · dedup vs articles DB + own ledger                       │
signal-source-          │    · decide CREATE vs MODIFY-existing vs REJECT (no human)   │
investigation ──enqueue─┤    · spawn subtask per approved → scraper-workflow skill     │
(Wave 4)                │    · review subtask output                                   │
                        │                                                              │
                        │  scraperOptimization app (always-on, split from creation):   │
                        │    runs scrapers periodically + scores + state machine —     │
                        │    does NOT spawn Claude itself                              │
                        │    ↑ hot-reloads approved edits & new scrapers (same wt)     │
                        │  Optimization orchestrator (persistent) + edit subagents     │
                        │    · per-edit review gate: approve / reject / feedback       │
                        │    · anti-Goodhart: latency target AND value-score sanity    │
                        │  Merge path: pause-app → diff vs main → merge skill          │
                        └──────────────────────────────────────────────────────────────┘
Portal dashboard ← optimization state (status.json / DB table)
```

Preserved properties: creation + optimization share one worktree (new scrapers flow into
optimization within a cycle, as today); the app runs scrapers itself; agents never commit.
New property: **every optimization edit passes an orchestrator review before rejoining the
loop** (replaces today's nothing).

### Decided defaults (approved)

- **Ack semantics v1 — claimed-at-fire.** Items are `claimed` when the daemon hands the
  batch to the orchestrator and `consumed` on successful delivery. A crash between claim
  and delivery leaves them `claimed` (visible; manual requeue endpoint). No ack-on-
  report_done / reclaim timers in v1 — the orchestrator's own ledger catches losses.
- **Queue shape — generic named queues, free-form JSON payloads, optional `dedupe_key`.**
  The queue is a transport; schema is soft-enforced by convention between producer and
  consumer prompt. Queue items are NOT bound to planning-task rows — the orchestrator
  files planning tasks itself when it spawns subtasks.
- **Access path — the API owns the table** (resolves DESIGN_CONTINUOUS_TASKS.md §18 open
  question #2). Producers POST over HTTPS with `CM_API_TOKEN` (aux → cm-manager static IP
  works cross-project); the daemon claims via the same API using its existing
  `api_url`/`api_token` config. The daemon gets no psql client.
- **Batch delivery — file, not paste.** The daemon writes the claimed batch to
  `<worktree>/.queue/batch-<seq>.json` and delivers a short prompt referencing that path.
  Avoids giant-paste/compact hazards on the PTY.
- **Dedup window — pending + claimed only.** A `dedupe_key` collides only against
  not-yet-consumed items (coalesce bursts); after consumption the same key may re-enqueue
  (new signal). Decision-level dedup ("we already rejected this domain") is the
  orchestrator ledger's job, not the queue's.
- **`enqueue_to` chaining stays inert.** Wave 4's producer is an agent → it uses the MCP
  `enqueue` tool; no end-of-run auto-chaining needed yet.

---

## 3. Wave 1 — generic queue core (claude-manager, THIS repo)

**Status: IMPLEMENTED 2026-07-04** (daemon 925 / mcp 139 tests green; deploy +
e2e smoke pending). One naming delta: the table is **`queue_items`** (no `cm_`
prefix, matching the `tasks`/`warm_pools` convention). Deliberately
scraper-agnostic. Slices:

**1a. SQL** — `sql/012_queue_items.sql` (idempotent):
`queue_items(id, queue, payload jsonb, dedupe_key, source, state
pending|claimed|consumed, enqueued_at, claimed_at, claimed_by, consumed_at)`.
Partial unique index on `(queue, dedupe_key) WHERE state IN ('pending','claimed') AND
dedupe_key IS NOT NULL`; index on `(queue, state, enqueued_at)`.

**1b. API** (`api/main.py` + `dispatch/db.py`):
- `POST /queues/{queue}/items` `{payload, dedupe_key?, source?}` →
  `{enqueued, deduped, id?, depth}`. Dedupe = insert-on-conflict-ignore.
- `GET /queues/{queue}` → `{queue, pending, claimed, oldest_pending_at}`.
- `POST /queues/{queue}/claim` `{max_items, claimed_by}` → `{items:[…]}` — atomic
  `FOR UPDATE SKIP LOCKED`, oldest-first.
- `POST /queues/{queue}/ack` `{ids}` → claimed→consumed.
- `POST /queues/{queue}/requeue` → claimed→pending (recovery). The selection is
  ALWAYS explicit: `{ids}` for those items (empty list = nothing), or `{"all": true}`
  for every claimed item; an empty/omitted body is a 400. *(Shipped as "no ids = all",
  which made a bare `POST .../requeue` — and `{"ids": []}` — re-pend the batch an
  in-flight Consumer fire had just claimed. The daemon only ever sends `{ids}`.)*
All under the existing bearer-token auth.

**1c. Daemon** (`daemon/src/continuous/queue.rs` + scheduler + fire path):
- `queue.rs`: thin API client (existing HTTP + api_url/api_token plumbing) — `depth`,
  `claim`, `ack`, `requeue`.
- Scheduler due-logic for `Schedule::Consumer{queue, batch_max, window_secs,
  depth_threshold}`: due when `depth ≥ depth_threshold` OR (`depth > 0` AND
  `now − last_fired_at ≥ window_secs`). Depth polls throttled (per-task cache, ~30s) so
  the tick loop doesn't hammer the API.
- Fire path (shared by scheduled fire / `trigger` / `run_now`): for Consumer tasks —
  claim up to `batch_max` → write `<worktree>/.queue/batch-<seq>.json` → deliver prompt
  (default_prompt + one-line suffix naming the batch file + item count) → ack claimed →
  advance. Claim/write failure → release claim (best effort), count as Failed fire
  (existing backoff + circuit breaker apply). Manual `trigger`/`run_now` on a Consumer
  task claims whatever is pending (possibly 0 items — still fires; prompt must handle an
  empty batch).
- Existing skip-active semantics unchanged: a Running last_run blocks re-fire; depth
  accumulates meanwhile.
- RPC: `enqueue {queue, payload, dedupe_key?, source?}` (Session + Operator) and
  `queue.stats {queue}` (Operator) routed through queue.rs.

**1d. MCP** (`mcp_server/server.py`): `enqueue` + `queue_depth` tools → daemon RPC
(`DAEMON_METHODS` in `control_client.py`). NOTE the two-copies rule: predictionTrading's
`scripts/mcp/claude_manager_server.py` needs the same tools before Wave 4 agents can
enqueue — that edit belongs to the predictionTrading handoff, not this repo.

**1e. Tests + deploy + e2e**: scheduler unit tests for Consumer due-logic + fire
(mockable queue client); API tests for dedupe/claim atomicity; MCP routing test. Deploy
api+sql+daemon+mcp to cm-manager (ship `sql/` with the api per the deploy memory), then an
operator-socket e2e: create a throwaway bash-engine Consumer task, POST 3 items, watch it
fire once with a batch file, verify ack + depth 0 + runlog.

---

## 4. Wave 2 — creation migration (predictionTrading handoff)

App side (aux):
- Replace `HumanReviewService` + `ScraperCreationService` with a **QueueEmitter**:
  serialize the (already msgspec) `AttributionBatch` → `POST
  {CM_API}/queues/scraper-creation-proposals/items`, `dedupe_key` = registrable domain,
  `source` = "scraperGeneration/aux". Add a durable proposal ledger (today's `_domains_seen`
  cursor/dedup state is in-memory).
- Keep momentum→attribution→filters unchanged, but **revisit thresholds**: 0 proposals in
  7 days means the hard gates (composite ≥0.45 / causation ≥60) may be too tight now that
  a rejected proposal costs one queue item instead of a Telegram ping. Consider loosening
  and letting the orchestrator judge.

Orchestrator side (cm-manager):
- Persistent continuous task `scraper-creation`, `Schedule::Consumer` on
  `scraper-creation-proposals` (suggest batch_max 10, window 6h, depth_threshold 3 to
  start), `review_kind: fix_first`, memory dir `.scraper-creation/` (ledger + index.yaml,
  triage-pattern).
- Cycle: read batch file → dedup vs articles DB + ledger → per proposal decide
  **CREATE / MODIFY-existing / REJECT** (the modify check: does an existing scraper
  *should*-cover this domain but miss the article? — absorbs the old Telegram gate and the
  worth-it logic currently inside `SCRAPER_CREATION.WORKFLOW.md`) → spawn subtask per
  approved item running the `scraper-workflow` skill → review subtask output → ledger +
  summary.
- Producer-side AND consumer-side dedup vs articles DB, per the design discussion.

Ops (from claude-manager, the proven triage-migration recipe): planning task →
`continuous.create` → verify auto-fire → flip `creation_enabled: false` on aux →
disposition of the orphaned June-19 batch (review/merge once the old way, or let the
Wave-3 worktree reset absorb it).

Interim accepted cost: until Wave 3, new scrapers reach aux's optimizer only after
merge-to-main + service reset.

---

## 5. Wave 3 — optimization split (predictionTrading handoff, own sub-design doc)

Extract the optimization loop into a standalone `scraperOptimization` app (always-on,
runs on cm-manager in the shared worktree) with the Claude-spawn seam replaced by
**emitting an optimization-needed item** (naturally: a cm queue → the optimization
orchestrator). Sub-design must cover:
- **Review gate mechanics**: today `reload_changed_scrapers` hot-adopts any edit; the gate
  goes exactly there — orchestrator approves/rejects/feedbacks each subagent edit before
  the app reloads it (staging dir, or app only reloads on orchestrator token).
- **Anti-Goodhart**: review scores against BOTH the locked latency target (system A) and
  value/attribution sanity (system B) + the instructions.md "never remove X" rules.
- **Vector-DB coupling**: optimization feeds articles into creation's in-process Chroma
  RAG today (single-process constraint, DESIGN.md:61-64). Likely resolve via the existing
  Postgres article-sync path.
- **The pause-review-merge skill**: stop app → diff worktree vs main → operator review →
  merge → restart (replaces `/review-scraper-changes`).
- **Portal dashboard**: scraper states/latency/value-score surface (from status.json or a
  DB table) + "focus on scraper X" operator → orchestrator channel.
- **Capacity**: cm-manager is e2-standard-4, ~9.4 GB free; the aux app runs 3.6 GB RSS +
  ⅓ core. Feasible but tight — consider resize or lowering
  `optimization_max_concurrent_scrapers`. Also: env keys (proxies, LLM, Postgres via the
  existing :5433 tunnel) must be provisioned on cm-manager.

## 6. Wave 4 — signal-source-investigation (new continuous task)

Pure config + prompt once Waves 1–2 exist. Per new DB signal: websearch provenance chase
(find the earliest article for the event) → did we scrape it? did we signal it? →
- scraped + signaled: done.
- scraped, no signal: spawn subtask to investigate + fix signal production (or backlog
  task if hard/unpromising).
- not scraped: **`enqueue` into scraper-creation-proposals** (MCP tool; needs the
  predictionTrading MCP-copy edit from §3.1d).
No new daemon code — the payoff of the generic queue.

---

## 7. Delegation

- **Wave 1**: this session, this repo, then merge+deploy via `/merge-main remote`.
- **Waves 2–3**: handoff docs + planning tasks for predictionTrading agents (app
  internals, prompts, portal). Wave 3 opens with its own sub-design doc.
- **Migration ops** (continuous.create, queue creation, aux service/cron changes, env
  keys): driven from claude-manager operator context when each wave lands.
