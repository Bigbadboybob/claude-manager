-- Item freshness from observed activity (doc/items-board.md §4).
-- Rerunnable DDL only.

-- A holder busy in background work suppresses stale/holder_idle; after this
-- long without a touch the softer stale_background flag is raised.
ALTER TABLE boards ADD COLUMN IF NOT EXISTS stale_background_s INTEGER NOT NULL DEFAULT 21600;
-- Stale nudges the holder first and flags only after this grace.
ALTER TABLE boards ADD COLUMN IF NOT EXISTS nudge_grace_s INTEGER NOT NULL DEFAULT 1800;
-- When the holder was nudged for the current staleness episode.
ALTER TABLE items ADD COLUMN IF NOT EXISTS stale_nudged_at TIMESTAMPTZ;
