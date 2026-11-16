-- Org model O5: durable assistants (doc/org-model.md §4.2, §4.3).
-- A daemon reports which member each of its sessions is an assistant of;
-- the API reads it for authority (close_assistant), `org.member` and free
-- capacity. `acting_as` is 'assistant', or 'closed' once deliberately closed.
ALTER TABLE org_session_facts ADD COLUMN IF NOT EXISTS member_id TEXT;
ALTER TABLE org_session_facts ADD COLUMN IF NOT EXISTS acting_as TEXT;
CREATE INDEX IF NOT EXISTS idx_org_session_facts_member
    ON org_session_facts (member_id) WHERE member_id IS NOT NULL;
