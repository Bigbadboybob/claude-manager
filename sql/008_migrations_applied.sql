-- Marker table for one-shot data backfills.
--
-- `dispatch/db.init_db` re-runs every *.sql in `sql/` on each API
-- startup, so any unconditional `UPDATE` / `INSERT` in a migration
-- runs over and over. Most migrations sidestep this by using
-- DDL-with-IF-NOT-EXISTS (idempotent), but data backfills genuinely
-- need to run exactly once on each fresh deploy. They gate their
-- bodies on a row in this table:
--
--   IF NOT EXISTS (SELECT 1 FROM migrations_applied
--                  WHERE name = '<id>') THEN ... END IF;
--   INSERT INTO migrations_applied (name) VALUES ('<id>');
--
-- This file must land before any migration that uses the table; the
-- 008_ prefix puts it after the existing 001-007 schema migrations
-- and before the first marker-gated migration in 009.
CREATE TABLE IF NOT EXISTS migrations_applied (
    name TEXT PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
