-- Flag dedupe and chat-derived activity (doc/items-board.md §4, §5).
-- Rerunnable DDL only.

-- Per item, the state each flag kind had when it last reached the
-- orchestrator ({kind: fingerprint}). A flag is pushed again only when the
-- item's state changes, never on a timer.
ALTER TABLE items ADD COLUMN IF NOT EXISTS flag_pushed JSONB NOT NULL DEFAULT '{}'::jsonb;
-- The board's steward: like the orchestrator, its #N posts count as
-- activity on any item of the board.
ALTER TABLE boards ADD COLUMN IF NOT EXISTS steward_pid TEXT;
-- The conversation bound to the board: #N in that channel means this
-- board's item N.
ALTER TABLE boards ADD COLUMN IF NOT EXISTS channel TEXT;
CREATE INDEX IF NOT EXISTS idx_boards_channel ON boards (channel) WHERE channel IS NOT NULL;
