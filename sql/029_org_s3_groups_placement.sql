-- Org model S3 (doc/org-model.md §7, §14): board groups and epics, link-first
-- placement of planning tasks on boards, item roles, launch grants and the
-- epic close plan (daemon capabilities are org S2's org_daemons). Rerunnable
-- DDL only; the one-time data
-- moves (items.grp -> heading groups, holder roles) are in 029b, behind the
-- migrations_applied marker.

-- Groups: a heading (topic label, as today's free-text items.grp) or an epic
-- (a body of work with a goal, a status and, once org teams exist, a team
-- whose lead is the epic's PoC). Both nest.
CREATE TABLE IF NOT EXISTS groups (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    board_id        UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    parent_group_id UUID REFERENCES groups(id) ON DELETE SET NULL,
    name            TEXT NOT NULL,
    description     TEXT,
    ord             INTEGER NOT NULL DEFAULT 0,
    kind            TEXT NOT NULL DEFAULT 'heading',
    goal            TEXT,
    epic_status     TEXT,
    team_id         UUID REFERENCES initiatives(id) ON DELETE SET NULL,
    revision        INTEGER NOT NULL DEFAULT 0,
    created_by      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    closed_at       TIMESTAMPTZ,
    closed_by       TEXT,
    close_record    JSONB,
    CONSTRAINT groups_kind_check CHECK (kind IN ('heading', 'epic')),
    CONSTRAINT groups_epic_status_check
      CHECK (epic_status IS NULL OR epic_status IN ('active', 'done', 'cancelled')),
    CONSTRAINT groups_not_own_parent CHECK (parent_group_id IS NULL OR parent_group_id <> id)
);
CREATE INDEX IF NOT EXISTS idx_groups_board ON groups (board_id);
-- A name is unique among its siblings (the top level counts as one parent).
CREATE UNIQUE INDEX IF NOT EXISTS groups_sibling_name
    ON groups (board_id, COALESCE(parent_group_id, '00000000-0000-0000-0000-000000000000'::uuid),
               lower(name));

-- Link-first placement: an item linked to the planning task it was placed
-- from. At most one live link per task; unplacing archives the link (the
-- item stays, read-only, so its number still resolves for history).
ALTER TABLE items ADD COLUMN IF NOT EXISTS task_id UUID REFERENCES tasks(id) ON DELETE SET NULL;
ALTER TABLE items ADD COLUMN IF NOT EXISTS link_archived_at TIMESTAMPTZ;
CREATE UNIQUE INDEX IF NOT EXISTS items_task_link ON items (task_id)
    WHERE task_id IS NOT NULL AND link_archived_at IS NULL;
-- Board-native items carry their own description and metadata; a placed
-- item reads the task's.
ALTER TABLE items ADD COLUMN IF NOT EXISTS description TEXT;
ALTER TABLE items ADD COLUMN IF NOT EXISTS description_rev INTEGER NOT NULL DEFAULT 0;
ALTER TABLE items ADD COLUMN IF NOT EXISTS metadata JSONB;
ALTER TABLE items ADD COLUMN IF NOT EXISTS group_id UUID REFERENCES groups(id) ON DELETE SET NULL;
ALTER TABLE items ADD COLUMN IF NOT EXISTS team_id UUID REFERENCES initiatives(id) ON DELETE SET NULL;

-- The task side: its description revision (holders are told when it moves)
-- and a copy of its placement, written in the same transaction as every
-- place / unplace / mirrored status, so the task change feed carries it.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS description_rev INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS placement JSONB;

-- Holder roles: owner | contributor | reviewer | approver, or the legacy
-- `holder` (co-owner) every multi-holder item migrated with. member_id /
-- via_member_id are filled from S6.
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS role TEXT;
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS member_id TEXT;
ALTER TABLE item_holders ADD COLUMN IF NOT EXISTS via_member_id TEXT;

-- Launch grants (§7.4): the owner-role holder of a placed item may launch an
-- assistant for that task even where the scope rule would refuse. Written
-- when the owner changes; handed to the holder's daemon in its heartbeat.
CREATE TABLE IF NOT EXISTS launch_grants (
    id          BIGSERIAL PRIMARY KEY,
    task_id     UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    pid         TEXT NOT NULL,
    daemon_id   TEXT,
    session_uid TEXT,
    member_id   TEXT,
    item_id     BIGINT REFERENCES items(id) ON DELETE SET NULL,
    granted_by  TEXT,
    granted_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ,
    revoked_by  TEXT,
    reason      TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS launch_grants_live
    ON launch_grants (task_id, pid) WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_launch_grants_daemon
    ON launch_grants (daemon_id) WHERE revoked_at IS NULL;

-- The persisted, resumable epic close plan (§7.6).
CREATE TABLE IF NOT EXISTS epic_close_plans (
    id          BIGSERIAL PRIMARY KEY,
    group_id    UUID NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    outcome     TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'planned',
    revision    INTEGER NOT NULL DEFAULT 0,
    plan        JSONB NOT NULL,
    choices     JSONB,
    progress    JSONB NOT NULL DEFAULT '[]'::jsonb,
    created_by  TEXT,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    applied_at  TIMESTAMPTZ,
    CONSTRAINT epic_close_plans_outcome_check CHECK (outcome IN ('done', 'cancelled')),
    CONSTRAINT epic_close_plans_status_check
      CHECK (status IN ('planned', 'applying', 'applied', 'abandoned'))
);
CREATE UNIQUE INDEX IF NOT EXISTS epic_close_plans_open
    ON epic_close_plans (group_id) WHERE status IN ('planned', 'applying');
