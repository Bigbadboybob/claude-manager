-- Continuous Tasks Phase 4: generic named queues (DESIGN_SCRAPER_MIGRATION.md §3,
-- DESIGN_CONTINUOUS_TASKS.md §9). External producers enqueue free-form JSON
-- payloads via POST /queues/{queue}/items; the daemon's Consumer schedule
-- claims batches atomically (FOR UPDATE SKIP LOCKED) and acks them consumed.
--
-- States: pending -> claimed (batch handed to an orchestrator fire) ->
-- consumed (delivery succeeded). A crash between claim and delivery leaves
-- items claimed; POST /queues/{queue}/requeue flips them back to pending.
--
-- Dedup: a `dedupe_key` collides only against not-yet-consumed items in the
-- same queue (coalesce bursts while pending/claimed); after consumption the
-- same key may re-enqueue (new signal). Enforced by the partial unique index —
-- the enqueue INSERT catches UniqueViolation and reports {deduped: true}.
--
-- Idempotent — runs on every API startup.

CREATE TABLE IF NOT EXISTS queue_items (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    queue        TEXT NOT NULL,
    payload      JSONB NOT NULL,
    dedupe_key   TEXT,
    source       TEXT,
    state        TEXT NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'claimed', 'consumed')),
    enqueued_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    claimed_at   TIMESTAMPTZ,
    claimed_by   TEXT,
    consumed_at  TIMESTAMPTZ
);

-- Burst-coalescing dedup window: unique per (queue, dedupe_key) among
-- not-yet-consumed items only.
CREATE UNIQUE INDEX IF NOT EXISTS idx_queue_items_dedupe
    ON queue_items (queue, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND state IN ('pending', 'claimed');

-- Claim scan: oldest-first pending items per queue.
CREATE INDEX IF NOT EXISTS idx_queue_items_pending
    ON queue_items (queue, enqueued_at)
    WHERE state = 'pending';
