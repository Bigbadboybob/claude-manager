-- Task change log: the server-side source for incremental task updates
-- (GET /tasks/changes). Every committed insert/update/delete on `tasks` —
-- through the HTTP API, the in-process dispatch daemon, or any other DB
-- client — appends one row here via trigger, and initiative display-metadata
-- edits fan out to the tasks that reference the initiative. Clients keep a
-- cursor (the last `seq` they applied) and fetch only rows above it.
--
-- Commit ordering. A BIGSERIAL alone is NOT commit-ordered: a transaction can
-- draw seq 100, stall, and commit after another transaction's seq 101 is
-- already visible, so a reader that advanced its cursor to 101 would never
-- see 100. The BEFORE ... FOR EACH STATEMENT triggers below take one
-- transaction-scoped advisory lock before any row lock or sequence draw, and
-- PostgreSQL releases it only at commit/abort. That serialises every
-- task/initiative-writing transaction through the log, so seq order equals
-- commit order and a visible seq N implies every seq < N is already visible
-- (or belongs to an aborted transaction and never will be). Taking the lock
-- BEFORE the statement's row locks — on both tables — also keeps lock order
-- uniform (advisory first, rows second) so the serialisation cannot deadlock
-- with ordinary row-level locking.
--
-- All statements are idempotent: migrations run at every API startup.

CREATE TABLE IF NOT EXISTS task_changes (
    seq        BIGSERIAL PRIMARY KEY,
    task_id    UUID NOT NULL,
    op         TEXT NOT NULL CHECK (op IN ('upsert', 'delete')),
    changed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_task_changes_changed_at ON task_changes (changed_at);

-- Single-row metadata: `epoch` identifies this log's lineage (a restored or
-- re-created log gets a new one, so clients holding a cursor from the old
-- lineage resnapshot instead of applying it), `pruned_through` is the highest
-- seq that retention has deleted (a cursor at or below it is expired).
CREATE TABLE IF NOT EXISTS task_change_meta (
    singleton      BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    epoch          TEXT NOT NULL DEFAULT gen_random_uuid()::text,
    pruned_through BIGINT NOT NULL DEFAULT 0
);
INSERT INTO task_change_meta (singleton) VALUES (TRUE) ON CONFLICT DO NOTHING;

CREATE OR REPLACE FUNCTION task_changes_serialize() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    -- One global key; held until this transaction commits or aborts.
    PERFORM pg_advisory_xact_lock(hashtext('cm_task_changes'));
    RETURN NULL;
END $$;

CREATE OR REPLACE FUNCTION task_changes_record() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    new_seq BIGINT;
BEGIN
    IF TG_OP = 'DELETE' THEN
        INSERT INTO task_changes (task_id, op) VALUES (OLD.id, 'delete')
            RETURNING seq INTO new_seq;
    ELSE
        INSERT INTO task_changes (task_id, op) VALUES (NEW.id, 'upsert')
            RETURNING seq INTO new_seq;
    END IF;
    -- Delivered at commit; the payload is only a wake-up hint, readers
    -- always re-query the table.
    PERFORM pg_notify('cm_task_changes', new_seq::text);
    RETURN NULL;
END $$;

-- Initiative slug/name/status/color/coordinator feed the `initiative` object
-- embedded in every task row, so an edit must re-deliver the member tasks.
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

CREATE OR REPLACE TRIGGER task_changes_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON tasks
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();

CREATE OR REPLACE TRIGGER task_changes_record
    AFTER INSERT OR UPDATE OR DELETE ON tasks
    FOR EACH ROW EXECUTE FUNCTION task_changes_record();

CREATE OR REPLACE TRIGGER task_changes_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON initiatives
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();

CREATE OR REPLACE TRIGGER task_changes_record_initiative
    AFTER UPDATE OR DELETE ON initiatives
    FOR EACH ROW EXECUTE FUNCTION task_changes_record_initiative();
