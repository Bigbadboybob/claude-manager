-- Add planning fields to tasks table so planning tasks live in the DB
-- alongside cloud dispatch tasks.

-- Planning metadata
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS project TEXT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS slug TEXT;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS description TEXT DEFAULT '';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS difficulty INTEGER;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS depends TEXT[] DEFAULT '{}';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS source TEXT NOT NULL DEFAULT 'user';
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS is_cloud BOOLEAN NOT NULL DEFAULT false;

-- Add check constraint for source (can't use ADD ... CHECK with IF NOT EXISTS,
-- so use DO block to be idempotent).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'tasks_source_check'
    ) THEN
        ALTER TABLE tasks ADD CONSTRAINT tasks_source_check
            CHECK (source IN ('user', 'claude'));
    END IF;
END $$;

-- Expand status to include 'draft' for planning tasks.
ALTER TABLE tasks DROP CONSTRAINT IF EXISTS tasks_status_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_status_check
    CHECK (status IN ('draft', 'backlog', 'running', 'blocked', 'done'));

-- Unique slug per project (NULLs excluded — legacy cloud-only tasks don't need slugs).
CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_project_slug
    ON tasks (project, slug) WHERE slug IS NOT NULL;

-- Filter by project.
CREATE INDEX IF NOT EXISTS idx_tasks_project
    ON tasks (project) WHERE project IS NOT NULL;

-- Mark all existing tasks as cloud (they were created by the dispatch system).
UPDATE tasks SET is_cloud = true WHERE is_cloud = false AND worker_vm IS NOT NULL;
