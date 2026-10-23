-- Strict boards (doc/strict-boards.md). Rerunnable DDL only.

-- false = the board holds only running or blocked work: waiting, open and
-- unheld new items are refused (doc/items-board.md §2 rule 11).
ALTER TABLE boards ADD COLUMN IF NOT EXISTS backlog_allowed BOOLEAN NOT NULL DEFAULT TRUE;
-- An active item with no live holder on it this long is `unlaunched`.
ALTER TABLE boards ADD COLUMN IF NOT EXISTS launch_s INTEGER NOT NULL DEFAULT 900;
-- A session holding more active items than this (outside a job's ETA) is
-- warned `holder_overloaded`.
ALTER TABLE boards ADD COLUMN IF NOT EXISTS max_active_per_holder INTEGER NOT NULL DEFAULT 2;
-- The furthest check_back a strict board accepts.
ALTER TABLE boards ADD COLUMN IF NOT EXISTS check_back_max_s INTEGER NOT NULL DEFAULT 259200;
-- The job in flight on an active item (with its eta).
ALTER TABLE items ADD COLUMN IF NOT EXISTS job TEXT;
-- When the item last became actionable: created active, entered active,
-- unblocked, handed back or assigned. The unlaunched clock starts here.
ALTER TABLE items ADD COLUMN IF NOT EXISTS actionable_at TIMESTAMPTZ;
