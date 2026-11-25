-- Notice acknowledgement is tied to the notice action, including when the
-- delivery heartbeat arrives after the owner responds. Rerunnable; no backfill.
ALTER TABLE compute_lease_notices ADD COLUMN IF NOT EXISTS answered_at timestamptz;
ALTER TABLE compute_lease_notices ADD COLUMN IF NOT EXISTS invalidated_at timestamptz;
-- Notices issued before this migration cannot prove whether an answer raced
-- the receipt. New notices opt in explicitly; legacy notices never reclaim.
ALTER TABLE compute_lease_notices ADD COLUMN IF NOT EXISTS answer_tracking boolean NOT NULL DEFAULT false;
-- Existing samples may predate interval-level activity tracking. Require a
-- fresh observation window after upgrade before considering reclamation.
CREATE TABLE IF NOT EXISTS compute_lease_sampling_epochs (
    lease_id uuid PRIMARY KEY REFERENCES compute_leases(id),
    initialized_at timestamptz NOT NULL DEFAULT now()
);
