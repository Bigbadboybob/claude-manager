-- An explicit reassignment is authoritative over stale daemon heartbeats.
ALTER TABLE org_session_facts ADD COLUMN IF NOT EXISTS assignment_authoritative BOOLEAN NOT NULL DEFAULT FALSE;
