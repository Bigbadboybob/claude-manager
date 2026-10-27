-- One host owns an adopted planning task. Request receipts survive retries and
-- later rebindings; history on the planning task and continuous run log remains.
CREATE TABLE IF NOT EXISTS continuous_registrations (
    task_id uuid PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    binding jsonb NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS continuous_registration_requests (
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    request_id text NOT NULL,
    request jsonb NOT NULL,
    binding jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, request_id)
);
