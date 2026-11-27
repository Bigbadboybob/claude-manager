-- Per-lease snapshots from node status; released results remain on actions.
CREATE TABLE IF NOT EXISTS compute_lease_run_details (
    lease_id uuid PRIMARY KEY REFERENCES compute_leases(id),
    detail jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
