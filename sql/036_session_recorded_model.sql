-- Optional recorded launch/resume settings, not a claim about the active model.
-- Old heartbeat writers omit these; the upsert clears prior values in that case.
ALTER TABLE session_states ADD COLUMN IF NOT EXISTS recorded_model TEXT;
ALTER TABLE session_states ADD COLUMN IF NOT EXISTS recorded_model_source TEXT;
-- Only roster attribution consumes these current-beat observations. Unlike
-- org_session_facts' intentionally retained values, omission clears them.
ALTER TABLE session_states ADD COLUMN IF NOT EXISTS roster_stamp_member TEXT;
ALTER TABLE session_states ADD COLUMN IF NOT EXISTS roster_stamp_role TEXT;
