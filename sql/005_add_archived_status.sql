-- Add 'archived' status for tasks that are done but should be hidden by default.
-- Archived tasks remain queryable but the TUI filters them out unless the user
-- explicitly toggles the show-archived view.
ALTER TABLE tasks DROP CONSTRAINT IF EXISTS tasks_status_check;
ALTER TABLE tasks ADD CONSTRAINT tasks_status_check
    CHECK (status IN ('draft', 'backlog', 'running', 'blocked', 'done', 'archived'));
