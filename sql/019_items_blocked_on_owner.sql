-- Item status blocked_on_owner (doc/items-board.md §1): waiting on a decision
-- only Owner can make.  Rerunnable: the CHECK is replaced only while it still
-- lacks the new status, so later startups do nothing.

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
         WHERE conname = 'items_status_check'
           AND pg_get_constraintdef(oid) LIKE '%blocked_on_owner%'
    ) THEN
        ALTER TABLE items DROP CONSTRAINT IF EXISTS items_status_check;
        ALTER TABLE items ADD CONSTRAINT items_status_check
            CHECK (status IN ('open', 'active', 'waiting', 'blocked', 'blocked_on_owner',
                              'done', 'dropped'));
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_items_owner_blocked
    ON items (board_id) WHERE status = 'blocked_on_owner';
