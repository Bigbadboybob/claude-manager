-- Divisions: an initiative with kind = 'division' is a permanent org unit
-- (no done state) rooted at its coordinator task. Idempotent: migrations run
-- at every API startup.

ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'initiative';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'initiatives_kind_check' AND conrelid = 'initiatives'::regclass
    ) THEN
        ALTER TABLE initiatives ADD CONSTRAINT initiatives_kind_check
            CHECK (kind IN ('initiative', 'division'));
    END IF;
END $$;

-- `kind` joins the display object embedded in task rows (TASK_ROW_SELECT), so
-- a kind change must re-deliver the member tasks like the 016 fields do.
-- Replaces the 016 definition; this file sorts after it on every startup.
CREATE OR REPLACE FUNCTION task_changes_record_initiative() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    max_seq BIGINT;
BEGIN
    IF TG_OP = 'UPDATE' AND (
        NEW.slug IS NOT DISTINCT FROM OLD.slug
        AND NEW.name IS NOT DISTINCT FROM OLD.name
        AND NEW.status IS NOT DISTINCT FROM OLD.status
        AND NEW.color IS NOT DISTINCT FROM OLD.color
        AND NEW.coordinator_task_id IS NOT DISTINCT FROM OLD.coordinator_task_id
        AND NEW.kind IS NOT DISTINCT FROM OLD.kind
    ) THEN
        RETURN NULL;
    END IF;
    WITH ins AS (
        INSERT INTO task_changes (task_id, op)
        SELECT id, 'upsert' FROM tasks WHERE initiative_id = OLD.id
        ORDER BY id
        RETURNING seq
    )
    SELECT max(seq) INTO max_seq FROM ins;
    IF max_seq IS NOT NULL THEN
        PERFORM pg_notify('cm_task_changes', max_seq::text);
    END IF;
    RETURN NULL;
END $$;
