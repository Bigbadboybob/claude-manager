-- Subtask support for Phase 5 of agent orchestration.
-- Adds parent_task_id (FK to tasks.id) and worktree_mode columns.
-- Both fields are nullable / have a default so existing rows are
-- valid without migration. Idempotent — runs on every API startup.

ALTER TABLE tasks ADD COLUMN IF NOT EXISTS parent_task_id UUID;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS worktree_mode TEXT NOT NULL DEFAULT 'inherit';

-- Self-referential FK so deleting a parent isn't silently allowed
-- when children exist. ON DELETE SET NULL keeps the orphaned children
-- around (the agent or user can reassign or close them); we'd rather
-- have detached orphans than cascade-delete a tree of children when
-- a parent is dropped.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'tasks_parent_task_id_fkey'
    ) THEN
        ALTER TABLE tasks ADD CONSTRAINT tasks_parent_task_id_fkey
            FOREIGN KEY (parent_task_id) REFERENCES tasks(id) ON DELETE SET NULL;
    END IF;
END $$;

-- Constrain worktree_mode to the two allowed values per
-- AGENT_ORCHESTRATION.md. Idempotent via the DO block.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'tasks_worktree_mode_check'
    ) THEN
        ALTER TABLE tasks ADD CONSTRAINT tasks_worktree_mode_check
            CHECK (worktree_mode IN ('inherit', 'branch'));
    END IF;
END $$;

-- Lookups by parent (planning view tree, list_subtasks MCP tool).
CREATE INDEX IF NOT EXISTS idx_tasks_parent_task_id
    ON tasks (parent_task_id) WHERE parent_task_id IS NOT NULL;
