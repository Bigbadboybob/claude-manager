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

-- Expand status to include 'draft' for planning tasks. Idempotent: only
-- add the constraint when it's missing entirely. The unconditional
-- DROP + ADD form here was a latent bomb — once a later migration
-- (005) widens the constraint to include 'archived' AND any rows get
-- marked archived, every subsequent restart re-runs 004 and tries to
-- re-add the narrow constraint, which fails CheckViolationError
-- because the archived rows violate it. Using the DO/IF NOT EXISTS
-- pattern here defers to whatever the latest migration left in place.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'tasks_status_check'
    ) THEN
        ALTER TABLE tasks ADD CONSTRAINT tasks_status_check
            CHECK (status IN ('draft', 'backlog', 'running', 'blocked', 'done'));
    END IF;
END $$;

-- Unique slug per project (NULLs excluded — legacy cloud-only tasks don't need slugs).
CREATE UNIQUE INDEX IF NOT EXISTS idx_tasks_project_slug
    ON tasks (project, slug) WHERE slug IS NOT NULL;

-- Filter by project.
CREATE INDEX IF NOT EXISTS idx_tasks_project
    ON tasks (project) WHERE project IS NOT NULL;

-- The is_cloud backfill that used to live here moved to
-- 009_004_backfill_marker.sql, which gates it on the migrations_applied
-- table from 008 so it runs exactly once instead of on every API restart.
