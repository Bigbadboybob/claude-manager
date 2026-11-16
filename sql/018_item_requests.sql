-- Items follow-ups (doc/items-board.md §5, §6).  Rerunnable DDL only.

-- Idempotent item creation: a retried create with the same request_id
-- returns the items the first attempt made instead of adding duplicates.
CREATE TABLE IF NOT EXISTS item_requests (
    board_id    UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    request_id  TEXT NOT NULL,
    actor_pid   TEXT NOT NULL,
    numbers     INTEGER[] NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (board_id, request_id)
);

-- Push delivery attempts: a push handed out repeatedly without an ack is
-- settled as dropped so it cannot starve newer pushes.
ALTER TABLE item_pushes ADD COLUMN IF NOT EXISTS attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE item_pushes ADD COLUMN IF NOT EXISTS dropped_reason TEXT;
