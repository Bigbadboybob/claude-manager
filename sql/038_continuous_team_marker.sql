-- Phase 3 continuous-team marker. The marker is declarative; live schedule and
-- queue fields are joined by the daemon from the lead's current host.
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS continuous_marker JSONB;
