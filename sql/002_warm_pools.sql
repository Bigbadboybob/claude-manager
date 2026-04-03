-- Warm pool: always-on VMs for frequently used repos
CREATE TABLE IF NOT EXISTS warm_pools (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repo_url        TEXT NOT NULL,
    repo_branch     TEXT NOT NULL DEFAULT 'main',
    pool_size       INTEGER NOT NULL DEFAULT 1,     -- how many VMs to keep warm
    vm_machine_type TEXT NOT NULL DEFAULT 'e2-medium',

    -- Managed by dispatch daemon
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Track warm VMs
CREATE TABLE IF NOT EXISTS warm_vms (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pool_id         UUID NOT NULL REFERENCES warm_pools(id),
    vm_name         TEXT NOT NULL,
    vm_zone         TEXT NOT NULL,
    external_ip     TEXT,
    ttyd_url        TEXT,
    status          TEXT NOT NULL DEFAULT 'booting'
                    CHECK (status IN ('booting', 'ready', 'busy', 'dead')),
    current_task_id UUID REFERENCES tasks(id),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_heartbeat  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_warm_vms_ready ON warm_vms (pool_id) WHERE status = 'ready';
