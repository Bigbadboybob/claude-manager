-- One policy for the organization served by this planning API. A stable key
-- keeps the policy separate from projects/teams and ready for explicit org IDs.
CREATE TABLE IF NOT EXISTS staffing_policy (
    org_key TEXT PRIMARY KEY,
    version BIGINT NOT NULL CHECK (version > 0),
    policy JSONB NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by TEXT NOT NULL
);
INSERT INTO staffing_policy (org_key, version, policy, updated_by)
VALUES ('default', 1, '{"default_engine":"codex","rules":{"math":"codex","design":"codex","architecture":"codex","metric":"codex","coding":"claude-code","fanout":"claude-code"},"preference":null}', 'initial')
ON CONFLICT (org_key) DO NOTHING;
