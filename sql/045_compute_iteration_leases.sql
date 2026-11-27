-- Persistent iteration affinity and per-request directory selection.
ALTER TABLE compute_leases ADD COLUMN IF NOT EXISTS iteration boolean NOT NULL DEFAULT false;
ALTER TABLE compute_leases ADD COLUMN IF NOT EXISTS iteration_slug text;
ALTER TABLE compute_leases ADD COLUMN IF NOT EXISTS previous_iteration_box text;
ALTER TABLE compute_leases ADD COLUMN IF NOT EXISTS previous_iteration_workdir text;
CREATE TABLE IF NOT EXISTS compute_iteration_affinity (
    owner text PRIMARY KEY,
    slug text NOT NULL UNIQUE,
    box text,
    workdir text
);
