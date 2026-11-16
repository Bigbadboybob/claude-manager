-- Board membership by an explicit join (#28): a session added to a board by
-- someone with authority over it (a creator at spawn, Owner, the board's
-- orchestrator or steward, or set_poc for an epic's PoC). It counts as being
-- on the board, beside a task that resolves to it and holding an item there:
-- it may edit the board and is free capacity when idle.
CREATE TABLE IF NOT EXISTS board_members (
    board_id    UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    pid         TEXT NOT NULL,
    name        TEXT,
    session_uid TEXT,
    daemon_id   TEXT,
    group_id    UUID,
    added_by    TEXT NOT NULL,
    reason      TEXT,
    added_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (board_id, pid)
);
CREATE INDEX IF NOT EXISTS idx_board_members_pid ON board_members (pid);
