-- Idempotent item creation (doc/items-board.md §6): a retried create with the
-- same request_id returns the items the first attempt made instead of adding
-- duplicates.  Rerunnable DDL only.

CREATE TABLE IF NOT EXISTS item_requests (
    board_id    UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    request_id  TEXT NOT NULL,
    actor_pid   TEXT NOT NULL,
    numbers     INTEGER[] NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (board_id, request_id)
);
