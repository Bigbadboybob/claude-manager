-- Persistent reservations and the FIFO queue. No existing rows are changed.
CREATE TABLE IF NOT EXISTS compute_leases (
    id uuid PRIMARY KEY,
    owner text NOT NULL,
    team text,
    purpose text NOT NULL,
    priority text NOT NULL CHECK (priority IN ('v221-gate', 'normal', 'low')),
    class text NOT NULL CHECK (class IN ('burst', 'long')),
    cpus integer NOT NULL CHECK (cpus > 0),
    mem_gb integer NOT NULL CHECK (mem_gb > 0),
    duration_seconds integer NOT NULL CHECK (duration_seconds > 0),
    pinned boolean NOT NULL DEFAULT false,
    status text NOT NULL CHECK (status IN ('queued', 'granting', 'active', 'releasing', 'released', 'failed')),
    release_requested boolean NOT NULL DEFAULT false,
    box text,
    cpuset text,
    slot_ids text[],
    ssh_target text,
    workdir text,
    created_at timestamptz NOT NULL DEFAULT now(),
    granted_at timestamptz,
    expires_at timestamptz,
    released_at timestamptz,
    last_activity_at timestamptz NOT NULL DEFAULT now(),
    error text
);
CREATE INDEX IF NOT EXISTS compute_leases_queue_idx ON compute_leases (priority, created_at, id) WHERE status = 'queued';
CREATE INDEX IF NOT EXISTS compute_leases_box_idx ON compute_leases (box, status) WHERE status IN ('granting', 'active', 'releasing');

-- Executor actions are durable; retries retain the same action ID.
CREATE TABLE IF NOT EXISTS compute_lease_actions (
    id uuid PRIMARY KEY,
    lease_id uuid NOT NULL REFERENCES compute_leases(id),
    kind text NOT NULL CHECK (kind IN ('grant', 'extend', 'release', 'idle_notice', 'reclaim_notice', 'expiry_notice')),
    payload jsonb NOT NULL,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'claimed', 'done')),
    created_at timestamptz NOT NULL DEFAULT now(),
    claimed_until timestamptz,
    result jsonb
);
CREATE INDEX IF NOT EXISTS compute_lease_actions_pending_idx ON compute_lease_actions (created_at) WHERE status <> 'done';

-- Empty after migration: the executor must establish a drained cutover first.
CREATE TABLE IF NOT EXISTS compute_boxes (
    name text PRIMARY KEY,
    enabled boolean NOT NULL DEFAULT false,
    checked_at timestamptz NOT NULL,
    evidence jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS compute_lease_samples (
    lease_id uuid NOT NULL REFERENCES compute_leases(id),
    sampled_at timestamptz NOT NULL DEFAULT now(),
    usage_usec bigint NOT NULL CHECK (usage_usec >= 0),
    PRIMARY KEY (lease_id, sampled_at)
);
CREATE TABLE IF NOT EXISTS compute_lease_notices (
    action_id uuid PRIMARY KEY REFERENCES compute_lease_actions(id),
    lease_id uuid NOT NULL REFERENCES compute_leases(id),
    push_id bigint NOT NULL REFERENCES item_pushes(id),
    sent_at timestamptz
);
