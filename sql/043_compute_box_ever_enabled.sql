-- Remember successful enables even if a box is later disabled or quarantined.
-- Existing rows remain unchanged; the API also recognizes prior enable evidence.
ALTER TABLE compute_boxes ADD COLUMN IF NOT EXISTS ever_enabled boolean NOT NULL DEFAULT false;
