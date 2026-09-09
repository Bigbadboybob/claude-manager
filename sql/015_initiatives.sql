-- First-class initiatives.  Projects remain the existing codebase-name
-- strings; an initiative may link one or more of them through the join table.
-- All statements are idempotent because migrations run at every API startup.

CREATE TABLE IF NOT EXISTS initiatives (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    slug                TEXT NOT NULL UNIQUE,
    name                TEXT NOT NULL,
    description         TEXT NOT NULL DEFAULT '',
    status              TEXT NOT NULL DEFAULT 'draft',
    color               TEXT,
    coordinator_task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
    coordinator_project TEXT,
    docs_path           TEXT NOT NULL DEFAULT 'cm-initiative',
    shared_channel      TEXT,
    approved_at         TIMESTAMPTZ,
    approved_by         TEXT,
    metadata            JSONB,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT initiatives_status_check
      CHECK (status IN ('draft', 'active', 'paused', 'completed', 'archived', 'cancelled'))
);

CREATE TABLE IF NOT EXISTS initiative_projects (
    initiative_id  UUID NOT NULL REFERENCES initiatives(id) ON DELETE CASCADE,
    project        TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'proposed',
    project_channel TEXT,
    role           TEXT NOT NULL DEFAULT '',
    proposed_by    TEXT,
    approved_at    TIMESTAMPTZ,
    approved_by    TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (initiative_id, project),
    CONSTRAINT initiative_projects_status_check
      CHECK (status IN ('proposed', 'approved', 'removed'))
);

CREATE TABLE IF NOT EXISTS initiative_events (
    id              BIGSERIAL PRIMARY KEY,
    initiative_id   UUID NOT NULL REFERENCES initiatives(id) ON DELETE CASCADE,
    actor           TEXT NOT NULL,
    event_type      TEXT NOT NULL,
    project         TEXT,
    previous_value  JSONB,
    new_value       JSONB,
    reason          TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE tasks ADD COLUMN IF NOT EXISTS initiative_id UUID;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'tasks_initiative_id_fkey'
    ) THEN
        ALTER TABLE tasks ADD CONSTRAINT tasks_initiative_id_fkey
            FOREIGN KEY (initiative_id) REFERENCES initiatives(id) ON DELETE SET NULL;
    END IF;
END $$;

CREATE INDEX IF NOT EXISTS idx_tasks_initiative_id
    ON tasks (initiative_id) WHERE initiative_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_initiative_projects_project
    ON initiative_projects (project) WHERE status = 'approved';
CREATE INDEX IF NOT EXISTS idx_initiative_events_initiative
    ON initiative_events (initiative_id, created_at);
