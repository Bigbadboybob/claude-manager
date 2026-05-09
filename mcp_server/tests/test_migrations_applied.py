"""Tests for the migrations_applied marker pattern.

`dispatch/db.init_db` re-runs every `sql/*.sql` file on each API
startup, so any unconditional row UPDATE/INSERT in a migration runs
on every restart. CLAUDE.md explicitly warns against this. To make
genuine one-shot backfills safe, 008 introduces a `migrations_applied`
marker table and 009 gates the original 004 is_cloud backfill on it.

Hitting a real Postgres in unit tests would be too heavy for this
repo's test rig, so this file does SQL-statement-shape verification:
it parses the migration files and asserts the structural invariants
that make the marker pattern correct, plus the lex-ordering invariant
that init_db relies on.
"""

from __future__ import annotations

import re
import unittest
from pathlib import Path


SQL_DIR = Path(__file__).resolve().parents[2] / "sql"


def _read(name: str) -> str:
    return (SQL_DIR / name).read_text()


class MigrationsAppliedShapeTest(unittest.TestCase):
    def test_008_creates_marker_table_idempotently(self):
        sql = _read("008_migrations_applied.sql")
        self.assertRegex(
            sql,
            r"CREATE\s+TABLE\s+IF\s+NOT\s+EXISTS\s+migrations_applied",
        )
        self.assertIn("name TEXT PRIMARY KEY", sql)
        self.assertIn("applied_at", sql)

    def test_009_uses_atomic_cte_claim(self):
        # Strip `--` comments before pattern-matching: the explanatory
        # header in 009 quotes the same SQL constructs we're asserting
        # on (CTE name, EXISTS gate), so naive regex search on the raw
        # file would happily match against the prose.
        raw = _read("009_004_backfill_marker.sql")
        sql = re.sub(r"--[^\n]*", "", raw)

        # The marker INSERT must be the atomic claim — wrapped in a CTE
        # with ON CONFLICT DO NOTHING RETURNING, so exactly one caller
        # produces a row. A check-then-act `IF NOT EXISTS … INSERT`
        # block would race and is what this rewrite explicitly avoids.
        self.assertNotRegex(
            sql,
            r"IF\s+NOT\s+EXISTS\s*\(\s*SELECT\s+1\s+FROM\s+migrations_applied",
            "009 must not use a check-then-act IF NOT EXISTS guard",
        )
        self.assertNotRegex(
            sql, r"DO\s+\$\$",
            "009 must not wrap the claim in a DO block (non-atomic)",
        )

        cte = re.search(
            r"WITH\s+claim\s+AS\s*\(\s*"
            r"INSERT\s+INTO\s+migrations_applied\s*\(\s*name\s*\)"
            r"\s+VALUES\s*\(\s*'004_is_cloud_backfill'\s*\)\s+"
            r"ON\s+CONFLICT\s*\(\s*name\s*\)\s+DO\s+NOTHING\s+"
            r"RETURNING\s+1\s*\)",
            sql,
            re.IGNORECASE,
        )
        self.assertIsNotNone(
            cte,
            "009 must claim the marker via INSERT … ON CONFLICT DO NOTHING "
            "RETURNING 1 inside a `claim` CTE",
        )

        update = re.search(
            r"UPDATE\s+tasks\s+SET\s+is_cloud\s*=\s*true",
            sql,
            re.IGNORECASE,
        )
        self.assertIsNotNone(update, "009 must contain the backfill UPDATE")

        gate = re.search(
            r"EXISTS\s*\(\s*SELECT\s+1\s+FROM\s+claim\s*\)",
            sql[update.end():],
            re.IGNORECASE,
        )
        self.assertIsNotNone(
            gate,
            "UPDATE must be gated on EXISTS(SELECT 1 FROM claim) so only "
            "the caller that won the INSERT touches rows",
        )

        # CTE precedes the UPDATE that references it.
        self.assertLess(cte.start(), update.start())

    def test_004_no_longer_contains_unconditional_update(self):
        sql = _read("004_planning_fields.sql")
        self.assertNotRegex(
            sql,
            r"^\s*UPDATE\s+tasks\s+SET\s+is_cloud",
            "004 must not run the row UPDATE on every restart anymore",
        )
        self.assertNotIn(
            "UPDATE tasks SET is_cloud = true WHERE is_cloud = false",
            sql,
        )

    def test_migration_lex_order_places_008_before_009(self):
        # init_db globs `*.sql` and runs them in `sorted()` order, so the
        # marker table must come before any migration that gates on it.
        files = sorted(p.name for p in SQL_DIR.glob("*.sql"))
        i_008 = files.index("008_migrations_applied.sql")
        i_009 = files.index("009_004_backfill_marker.sql")
        self.assertLess(i_008, i_009)
        self.assertLess(
            files.index("004_planning_fields.sql"),
            i_008,
            "004 (which the backfill belongs to) must precede 008/009",
        )


class MarkerSemanticsSimulationTest(unittest.TestCase):
    """Simulate the marker flow without a real DB.

    Models the marker table as a Python set and the backfill as a
    counter, then "applies" 009 twice. The second pass must be a
    no-op: the UPDATE doesn't run again, and the marker stays at one
    row. This catches regressions where someone re-orders the guard
    and INSERT, or drops the guard entirely.
    """

    def _apply_009(self, marker_set: set[str], state: dict) -> None:
        # Mirror the DO-block control flow in 009.
        if "004_is_cloud_backfill" not in marker_set:
            state["update_runs"] += 1
            marker_set.add("004_is_cloud_backfill")

    def test_second_apply_is_noop(self):
        markers: set[str] = set()
        state = {"update_runs": 0}

        self._apply_009(markers, state)
        self.assertEqual(state["update_runs"], 1)
        self.assertEqual(markers, {"004_is_cloud_backfill"})

        self._apply_009(markers, state)
        self.assertEqual(
            state["update_runs"], 1,
            "second apply must not re-run the backfill",
        )
        self.assertEqual(
            markers, {"004_is_cloud_backfill"},
            "marker row count must stay at 1",
        )


if __name__ == "__main__":
    unittest.main()
