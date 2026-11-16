-- Work items and boards (doc/items-board.md).  Items are small units of swarm
-- work held by sessions; they are not planning tasks and get no task_changes
-- triggers.  Migrations run at every API startup: only IF NOT EXISTS DDL, no
-- row UPDATEs, no backfill.

CREATE TABLE IF NOT EXISTS boards (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    -- One board per initiative, else one per top-level task.  No CHECK that
    -- one of them is set: ON DELETE SET NULL would make deleting the anchor
    -- fail, and an orphaned board stays readable by slug.
    initiative_id       UUID UNIQUE REFERENCES initiatives(id) ON DELETE SET NULL,
    root_task_id        UUID UNIQUE REFERENCES tasks(id) ON DELETE SET NULL,
    slug                TEXT NOT NULL UNIQUE,
    name                TEXT NOT NULL,
    orchestrator_pid    TEXT,
    idle_s              INTEGER NOT NULL DEFAULT 1200,
    stale_s             INTEGER NOT NULL DEFAULT 7200,
    unassigned_s        INTEGER NOT NULL DEFAULT 300,
    repush_s            INTEGER NOT NULL DEFAULT 1800,
    escalate_s          INTEGER NOT NULL DEFAULT 3600,
    digest_s            INTEGER NOT NULL DEFAULT 300,
    holder_idle_enabled BOOLEAN NOT NULL DEFAULT false,
    next_number         INTEGER NOT NULL DEFAULT 1,
    last_digest_at      TIMESTAMPTZ,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS items (
    id              BIGSERIAL PRIMARY KEY,
    board_id        UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    number          INTEGER NOT NULL,
    title           TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    note            TEXT,
    grp             TEXT,
    blocked_on      TEXT,
    check_back_at   TIMESTAMPTZ,
    eta_at          TIMESTAMPTZ,
    waiting_set_at  TIMESTAMPTZ,
    links           JSONB NOT NULL DEFAULT '[]'::jsonb,
    touched_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    clock_reset_at  TIMESTAMPTZ,
    closed_at       TIMESTAMPTZ,
    archived_at     TIMESTAMPTZ,
    created_by      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT items_board_number_key UNIQUE (board_id, number),
    CONSTRAINT items_status_check
      CHECK (status IN ('open', 'active', 'waiting', 'blocked', 'done', 'dropped'))
);

CREATE INDEX IF NOT EXISTS idx_items_board_live
    ON items (board_id) WHERE archived_at IS NULL;

CREATE TABLE IF NOT EXISTS item_holders (
    item_id     BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    pid         TEXT NOT NULL,
    session_uid TEXT,
    daemon_id   TEXT,
    name        TEXT,
    added_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (item_id, pid)
);

CREATE INDEX IF NOT EXISTS idx_item_holders_pid ON item_holders (pid);

CREATE TABLE IF NOT EXISTS item_deps (
    item_id     BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    blocker_id  BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (item_id, blocker_id),
    CONSTRAINT item_deps_not_self CHECK (item_id <> blocker_id)
);

CREATE INDEX IF NOT EXISTS idx_item_deps_blocker ON item_deps (blocker_id);

-- History of every change; max(id) per board is the board version.
CREATE TABLE IF NOT EXISTS item_events (
    id          BIGSERIAL PRIMARY KEY,
    board_id    UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    item_id     BIGINT REFERENCES items(id) ON DELETE SET NULL,
    item_number INTEGER,
    actor_pid   TEXT NOT NULL,
    actor_name  TEXT,
    type        TEXT NOT NULL,
    prev        JSONB,
    new         JSONB,
    reason      TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_item_events_board ON item_events (board_id, id);
CREATE INDEX IF NOT EXISTS idx_item_events_item ON item_events (item_id, id);

CREATE TABLE IF NOT EXISTS item_flags (
    id             BIGSERIAL PRIMARY KEY,
    item_id        BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    kind           TEXT NOT NULL,
    detail         JSONB,
    raised_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at    TIMESTAMPTZ,
    resolved_by    TEXT,
    resolution     TEXT,
    snooze_until   TIMESTAMPTZ,
    last_pushed_at TIMESTAMPTZ,
    push_count     INTEGER NOT NULL DEFAULT 0,
    escalated_at   TIMESTAMPTZ
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_item_flags_open
    ON item_flags (item_id, kind) WHERE resolved_at IS NULL;

-- One row per session, written by its daemon's heartbeat (server clock only).
CREATE TABLE IF NOT EXISTS session_states (
    pid           TEXT PRIMARY KEY,
    daemon_id     TEXT NOT NULL,
    session_uid   TEXT NOT NULL,
    host_label    TEXT,
    task_id       UUID,
    name          TEXT,
    engine        TEXT,
    state         TEXT NOT NULL,
    state_since   TIMESTAMPTZ,
    idle_since    TIMESTAMPTZ,
    reported_done BOOLEAN NOT NULL DEFAULT false,
    killed_by     TEXT,
    agent_state   JSONB,
    started_at    TIMESTAMPTZ,
    exited_at     TIMESTAMPTZ,
    reported_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_session_states_task ON session_states (task_id);
CREATE INDEX IF NOT EXISTS idx_session_states_daemon ON session_states (daemon_id);

-- Push outbox; each daemon collects its own rows in the heartbeat reply.
CREATE TABLE IF NOT EXISTS item_pushes (
    id           BIGSERIAL PRIMARY KEY,
    board_id     UUID REFERENCES boards(id) ON DELETE CASCADE,
    daemon_id    TEXT NOT NULL,
    session_uid  TEXT NOT NULL,
    pid          TEXT,
    kind         TEXT NOT NULL,
    text         TEXT NOT NULL,
    dedupe       TEXT NOT NULL UNIQUE,
    owner_alert  BOOLEAN NOT NULL DEFAULT false,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    delivered_at TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS idx_item_pushes_pending
    ON item_pushes (daemon_id) WHERE delivered_at IS NULL;
