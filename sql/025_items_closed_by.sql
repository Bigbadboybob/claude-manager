-- Who closed an item (doc/items-board.md §2 rule 7): named in the refusal of
-- a write that would reopen a done item without reopen=true. Rerunnable DDL
-- only; rows closed before this column read their closer from item_events.
ALTER TABLE items ADD COLUMN IF NOT EXISTS closed_by TEXT;
