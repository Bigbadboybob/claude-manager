-- One-shot backfill: mark legacy cloud-dispatched tasks as is_cloud=true.
--
-- This UPDATE used to live at the bottom of 004_planning_fields.sql,
-- where it ran on every API restart. Most of the time it was a no-op
-- (the rows were already updated), but it still re-scanned the table
-- and is the kind of unconditional row UPDATE that CLAUDE.md warns
-- against. Moving it here, gated on the migrations_applied marker
-- table from 008, means it runs exactly once per database.
--
-- The CTE pattern below makes the marker INSERT itself the atomic
-- claim: `ON CONFLICT DO NOTHING RETURNING 1` produces exactly one
-- row for the single caller that wins the unique-name race, and zero
-- rows for everyone else. The UPDATE's `EXISTS (SELECT 1 FROM claim)`
-- predicate then evaluates to true only for the winner, so concurrent
-- callers (e.g. `init_db` racing `scripts/migrate_planning_tasks.py`)
-- can't double-run the backfill or trip a PK violation. A check-then-
-- act `IF NOT EXISTS … INSERT` block would have that race.
WITH claim AS (
    INSERT INTO migrations_applied (name) VALUES ('004_is_cloud_backfill')
    ON CONFLICT (name) DO NOTHING
    RETURNING 1
)
UPDATE tasks SET is_cloud = true
WHERE is_cloud = false
  AND worker_vm IS NOT NULL
  AND EXISTS (SELECT 1 FROM claim);
