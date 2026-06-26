-- Task `kind` marker. Distinguishes continuous tasks from legacy one-shot
-- dispatch rows so the GCP `dispatch_daemon` (which claims `status='backlog'
-- AND is_cloud=true AND project IS NULL`) does not sweep up a continuous row.
-- `claim_next_task` / `count_dispatchable` in `dispatch/db.py` add
-- `AND kind <> 'continuous'` to exclude them.
--
-- Idempotent — runs on every API startup.

ALTER TABLE tasks ADD COLUMN IF NOT EXISTS kind TEXT NOT NULL DEFAULT 'oneshot';
