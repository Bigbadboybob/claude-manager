-- One-shot backfill: a task has one brief, its description (doc/org-model.md
-- §13 S1). A prompt that holds real instructions (non-empty, not the name,
-- not already in the description) is appended under `## Instructions`, the
-- same fold the API applies to an old client's CREATE (dispatch/task_text.py).
-- The prompt column itself is kept and never dropped.
--
-- Skipped: continuous rows (their contract lives in the continuous record's
-- standing_instructions; the task copies are stale and must never be
-- delivered) and backtest rows (a synthesized prompt nothing delivers).
--
-- Marker-gated like 009: the claim CTE makes exactly one caller run it, once
-- per database, as one statement (one task_changes burst; clients reset).
WITH claim AS (
    INSERT INTO migrations_applied (name) VALUES ('027_prompt_into_description')
    ON CONFLICT (name) DO NOTHING
    RETURNING 1
)
UPDATE tasks SET description = CASE
        WHEN btrim(COALESCE(description, ''), E' \t\r\n') = ''
            THEN E'## Instructions\n\n' || btrim(prompt, E' \t\r\n')
        ELSE rtrim(description, E' \t\r\n') || E'\n\n## Instructions\n\n' || btrim(prompt, E' \t\r\n')
    END
WHERE btrim(COALESCE(prompt, ''), E' \t\r\n') <> ''
  AND btrim(prompt, E' \t\r\n') <> btrim(COALESCE(name, ''), E' \t\r\n')
  AND strpos(COALESCE(description, ''), btrim(prompt, E' \t\r\n')) = 0
  AND COALESCE(kind, 'oneshot') NOT IN ('continuous', 'backtest')
  AND EXISTS (SELECT 1 FROM claim);
