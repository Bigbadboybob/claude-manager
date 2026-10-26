-- Owner-authorized auto-placement for fresh top-level agent sessions.
-- The daemon reports the durable intent in each org heartbeat; a transient
-- API outage must not lose that intent before the member is created.
ALTER TABLE org_session_facts ADD COLUMN IF NOT EXISTS auto_place_owner BOOLEAN NOT NULL DEFAULT FALSE;
CREATE INDEX IF NOT EXISTS idx_org_session_facts_auto_place
    ON org_session_facts (pid) WHERE auto_place_owner AND member_id IS NULL;
