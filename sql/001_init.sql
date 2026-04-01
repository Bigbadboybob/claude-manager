CREATE TABLE IF NOT EXISTS tasks (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- What to do
    repo_url        TEXT NOT NULL,
    repo_branch     TEXT NOT NULL DEFAULT 'main',
    prompt          TEXT NOT NULL,

    -- State
    status          TEXT NOT NULL DEFAULT 'backlog'
                    CHECK (status IN ('backlog', 'running', 'blocked', 'done')),
    priority        INTEGER NOT NULL DEFAULT 0,

    -- Worker
    worker_vm       TEXT,
    worker_zone     TEXT,
    ttyd_url        TEXT,
    blocked_at      TIMESTAMPTZ,

    -- Preemption recovery
    session_id      TEXT,
    wip_branch      TEXT,
    resume_metadata JSONB
);

CREATE INDEX IF NOT EXISTS idx_tasks_backlog ON tasks (priority, created_at) WHERE status = 'backlog';
CREATE INDEX IF NOT EXISTS idx_tasks_blocked ON tasks (blocked_at) WHERE status = 'blocked';
