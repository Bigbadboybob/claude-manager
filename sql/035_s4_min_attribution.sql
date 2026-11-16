-- Optional display evidence; no authorization or lifecycle key.
ALTER TABLE item_events ADD COLUMN IF NOT EXISTS actor_attribution JSONB;
ALTER TABLE item_events ADD COLUMN IF NOT EXISTS holder_attributions JSONB;
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS attribution JSONB;
