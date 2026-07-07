-- Cloud auto-backtest: structured result artifacts attached to tasks
-- (DESIGN_CONTINUOUS_TASKS.md §17 Phase 6; PT-side spec in predictionTrading
-- analysis/backtests/docs/CM_FEATURE_REQUEST.md, feature 2).
--
-- The worker uploads bulk output (grid CSVs, per-market CSVs, logs) to GCS
-- under `gcs_prefix` and POSTs a compact JSON summary here; agents read it
-- back via the `get_backtest_result` MCP verb. `summary` is capped at 64 KiB
-- in the API (same idiom as the queue_items payload cap) — artifacts are
-- summaries + pointers, not a blob store.
--
-- `partial` marks results published from an interrupted run (spot preemption,
-- pipeline failure); a task may accumulate several rows (partial then final),
-- newest-first reads take the latest.
--
-- Idempotent — runs on every API startup.

CREATE TABLE IF NOT EXISTS task_artifacts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    task_id     UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    kind        TEXT NOT NULL DEFAULT 'backtest-result',
    summary     JSONB NOT NULL,
    gcs_prefix  TEXT,
    partial     BOOLEAN NOT NULL DEFAULT false,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Newest-first per-task reads (GET /tasks/{id}/artifacts).
CREATE INDEX IF NOT EXISTS idx_task_artifacts_task
    ON task_artifacts (task_id, created_at DESC);
