-- S4 attribution is evidence alongside the existing PID, never a new key
-- or a substitute for the current authorization checks. Legacy rows stay
-- null; the daemon supplies fields only when it knows the identity.
ALTER TABLE item_events ADD COLUMN IF NOT EXISTS actor_member_id TEXT;
ALTER TABLE item_events ADD COLUMN IF NOT EXISTS actor_acting_as TEXT;
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS acting_as TEXT;
ALTER TABLE item_receipts ADD COLUMN IF NOT EXISTS poster_member_id TEXT;
ALTER TABLE item_receipts ADD COLUMN IF NOT EXISTS poster_acting_as TEXT;
ALTER TABLE item_grants ADD COLUMN IF NOT EXISTS grantor_member_id TEXT;
ALTER TABLE item_grants ADD COLUMN IF NOT EXISTS grantor_acting_as TEXT;
ALTER TABLE board_members ADD COLUMN IF NOT EXISTS added_by_member_id TEXT;
ALTER TABLE board_members ADD COLUMN IF NOT EXISTS added_by_acting_as TEXT;
ALTER TABLE session_states ADD COLUMN IF NOT EXISTS attribution_member_id TEXT;
ALTER TABLE session_states ADD COLUMN IF NOT EXISTS attribution_acting_as TEXT;
