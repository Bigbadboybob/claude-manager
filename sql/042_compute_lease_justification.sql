-- Explicit justification for requests above the normal per-lease slot cap.
-- Rerunnable; existing rows are left unchanged.
ALTER TABLE compute_leases ADD COLUMN IF NOT EXISTS justification text;
