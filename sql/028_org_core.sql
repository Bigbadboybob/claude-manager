-- Org model S2: members, occupancy, managers, teams (doc/org-model.md §4, §6,
-- §14). Rerunnable: migrations run at every API startup, so everything here
-- is IF NOT EXISTS DDL, "replace only if missing" constraint blocks, CREATE OR
-- REPLACE functions/triggers, and one ON CONFLICT DO NOTHING seed row.
--
-- Nothing here enforces anything outside the org tables: holds stay keyed by
-- pid and session scope is unchanged (S2 is display, routing and records).
--
-- Serialisation. Every org table takes the task feed's advisory lock
-- (task_changes_serialize, sql/016) before its rows, the same lock
-- `initiatives` already takes. One lock for both feeds keeps the lock order
-- uniform (a transaction that writes members and then initiatives cannot
-- deadlock against one that writes initiatives and then members), makes the
-- org_changes seq commit-ordered like task_changes, and serialises the
-- manager and parent-team cycle checks below.

-- ---- teams: initiatives evolve in place ------------------------------------

ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS parent_team_id UUID;
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS lead_member_id TEXT;
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS charter TEXT NOT NULL DEFAULT '';
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS standing_rules JSONB NOT NULL DEFAULT '[]';
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS team_status TEXT NOT NULL DEFAULT 'active';
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS org_id TEXT NOT NULL DEFAULT 'main';
ALTER TABLE initiatives ADD COLUMN IF NOT EXISTS team_revision INTEGER NOT NULL DEFAULT 0;
-- A team (kind 'team') may have no coordinator task. Old clients keep
-- receiving the stored id wherever there is one.
ALTER TABLE initiatives ALTER COLUMN coordinator_task_id DROP NOT NULL;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conname = 'initiatives_parent_team_fkey'
                      AND conrelid = 'initiatives'::regclass) THEN
        ALTER TABLE initiatives ADD CONSTRAINT initiatives_parent_team_fkey
            FOREIGN KEY (parent_team_id) REFERENCES initiatives(id);
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conname = 'initiatives_team_status_check'
                      AND conrelid = 'initiatives'::regclass) THEN
        ALTER TABLE initiatives ADD CONSTRAINT initiatives_team_status_check
            CHECK (team_status IN ('active', 'standby', 'archived'));
    END IF;
    -- sql/022 creates initiatives_kind_check only when it is missing, so the
    -- wider definition installed here survives every later startup.
    IF EXISTS (SELECT 1 FROM pg_constraint
                WHERE conname = 'initiatives_kind_check'
                  AND conrelid = 'initiatives'::regclass
                  AND pg_get_constraintdef(oid) NOT LIKE '%''team''%') THEN
        ALTER TABLE initiatives DROP CONSTRAINT initiatives_kind_check;
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conname = 'initiatives_kind_check'
                      AND conrelid = 'initiatives'::regclass) THEN
        ALTER TABLE initiatives ADD CONSTRAINT initiatives_kind_check
            CHECK (kind IN ('initiative', 'division', 'team'));
    END IF;
END $$;

-- ---- members ----------------------------------------------------------------

CREATE TABLE IF NOT EXISTS members (
    id                   TEXT PRIMARY KEY,                -- mem_<ulid>; Owner is mem_owner
    org_id               TEXT NOT NULL DEFAULT 'main',
    handle               TEXT NOT NULL,
    kind                 TEXT NOT NULL,                   -- agent | continuous | owner
    status               TEXT NOT NULL DEFAULT 'active',  -- active | vacant | suspended | retired
    manager_member_id    TEXT REFERENCES members(id),     -- NULL only for owner; acyclic (trigger)
    primary_team_id      UUID REFERENCES initiatives(id),
    occupancy_generation INTEGER NOT NULL DEFAULT 0,      -- bumps on each replacement (S6 fencing)
    profile              JSONB NOT NULL DEFAULT '{}',     -- authored fields {value, set_by, via, set_at}
    standing_rules       JSONB NOT NULL DEFAULT '[]',     -- [{text, source}]
    revision             INTEGER NOT NULL DEFAULT 0,
    defaults             JSONB,                           -- engine, model, effort, home_workspace
    created_by           TEXT,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at           TIMESTAMPTZ,
    CONSTRAINT members_org_handle_key UNIQUE (org_id, handle),
    CONSTRAINT members_id_check CHECK (id ~ '^mem_[a-z0-9_]{1,60}$'),
    CONSTRAINT members_handle_check CHECK (handle ~ '^[a-z0-9][a-z0-9-]{0,38}[a-z0-9]$'),
    CONSTRAINT members_kind_check CHECK (kind IN ('agent', 'continuous', 'owner')),
    CONSTRAINT members_status_check
      CHECK (status IN ('active', 'vacant', 'suspended', 'retired')),
    CONSTRAINT members_manager_root CHECK ((kind = 'owner') = (manager_member_id IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_members_manager ON members (manager_member_id);

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conname = 'initiatives_lead_member_fkey'
                      AND conrelid = 'initiatives'::regclass) THEN
        ALTER TABLE initiatives ADD CONSTRAINT initiatives_lead_member_fkey
            FOREIGN KEY (lead_member_id) REFERENCES members(id);
    END IF;
END $$;

-- Every handle a member has ever had, plus permanent aliases (a continuous
-- member's task slug and `<slug>-orchestrator`). The primary key makes a
-- handle unique against every alias too, and rows are never deleted, so a
-- retired handle is never reused.
CREATE TABLE IF NOT EXISTS member_aliases (
    org_id     TEXT NOT NULL,
    alias      TEXT NOT NULL,
    member_id  TEXT NOT NULL REFERENCES members(id),
    kind       TEXT NOT NULL DEFAULT 'handle',  -- handle | former_handle | continuous_slug | orchestrator
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (org_id, alias)
);
CREATE INDEX IF NOT EXISTS idx_member_aliases_member ON member_aliases (member_id);

-- A continuous task is host-local: (daemon_id, continuous record id), with a
-- separate nullable planning-task link.
CREATE TABLE IF NOT EXISTS continuous_members (
    daemon_id        TEXT NOT NULL,
    continuous_id    TEXT NOT NULL,
    member_id        TEXT NOT NULL UNIQUE REFERENCES members(id),
    planning_task_id UUID,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (daemon_id, continuous_id)
);

-- Occupancy: one row per (member, session, role). S2 opens `main` rows from
-- the roster and adoption; S6 maintains them on spawn, revive and fires.
CREATE TABLE IF NOT EXISTS incarnations (
    id                   BIGSERIAL PRIMARY KEY,
    member_id            TEXT NOT NULL REFERENCES members(id),
    role                 TEXT NOT NULL,
    occupancy_generation INTEGER,
    pid                  TEXT NOT NULL,
    daemon_id            TEXT NOT NULL,
    session_uid          TEXT NOT NULL,
    workspace_id         TEXT,
    predecessor          BIGINT REFERENCES incarnations(id),
    started_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    ended_at             TIMESTAMPTZ,
    end_reason           TEXT,
    CONSTRAINT incarnations_role_check CHECK (role IN ('main', 'assistant', 'standby'))
);
CREATE UNIQUE INDEX IF NOT EXISTS incarnations_one_main ON incarnations (member_id)
    WHERE role = 'main' AND ended_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS incarnations_open_pid ON incarnations (pid)
    WHERE ended_at IS NULL;

CREATE TABLE IF NOT EXISTS team_memberships (
    team_id   UUID NOT NULL REFERENCES initiatives(id),
    member_id TEXT NOT NULL REFERENCES members(id),
    role      TEXT NOT NULL,                     -- primary | guest | deputy
    since     TIMESTAMPTZ NOT NULL DEFAULT now(),
    until     TIMESTAMPTZ,
    PRIMARY KEY (team_id, member_id, role),
    CONSTRAINT team_memberships_role_check CHECK (role IN ('primary', 'guest', 'deputy'))
);
CREATE INDEX IF NOT EXISTS idx_team_memberships_member ON team_memberships (member_id);

-- Audit: hire, adopt, roster decisions, moves, team edits. Every reporting
-- change has a reason.
CREATE TABLE IF NOT EXISTS org_events (
    id           BIGSERIAL PRIMARY KEY,
    subject_type TEXT NOT NULL,                  -- member | team | roster
    subject_id   TEXT NOT NULL,
    type         TEXT NOT NULL,
    actor        JSONB,
    prev         JSONB,
    new          JSONB,
    reason       TEXT,
    request_id   TEXT,
    at           TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_org_events_subject ON org_events (subject_type, subject_id, at);

-- Idempotent writes: a retried request_id returns the stored response.
CREATE TABLE IF NOT EXISTS org_requests (
    request_id TEXT PRIMARY KEY,
    actor_pid  TEXT NOT NULL,
    method     TEXT NOT NULL,
    response   JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Roster proposals Owner declined, so they stop being proposed.
CREATE TABLE IF NOT EXISTS org_roster_declines (
    proposal_id TEXT PRIMARY KEY,
    declined_by TEXT NOT NULL,
    reason      TEXT,
    declined_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS org_meta (
    singleton         BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
    roster_applied_at TIMESTAMPTZ
);

-- Facts a daemon reports per session beside the items heartbeat
-- (session_states): what the roster needs to find workspaces, continuous
-- orchestrators and long-lived sessions. Not part of the change feed.
CREATE TABLE IF NOT EXISTS org_session_facts (
    pid                TEXT PRIMARY KEY,
    daemon_id          TEXT NOT NULL,
    session_uid        TEXT NOT NULL,
    workspace_id       TEXT,
    worktree_path      TEXT,
    continuous_task_id TEXT,
    managed_by_uid     TEXT,
    workflow_run_id    TEXT,
    spawned_at         TIMESTAMPTZ,
    reported_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_org_session_facts_daemon ON org_session_facts (daemon_id);

-- What each daemon advertises (doc §12 capabilities, e.g. org_preserve_v1).
CREATE TABLE IF NOT EXISTS org_daemons (
    daemon_id    TEXT PRIMARY KEY,
    capabilities TEXT[] NOT NULL DEFAULT '{}',
    seen_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Workspace kind marks (`home` from the roster and adoption). Recorded
-- centrally at once; pushed in the owning daemon's heartbeat reply only once
-- that daemon advertises org_preserve_v1, and confirmed by its ack.
CREATE TABLE IF NOT EXISTS org_workspace_marks (
    id           BIGSERIAL PRIMARY KEY,
    daemon_id    TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    path         TEXT,
    kind         TEXT NOT NULL DEFAULT 'home',
    member_id    TEXT NOT NULL REFERENCES members(id),
    state        TEXT NOT NULL DEFAULT 'central',  -- central | pushed | confirmed
    attempts     INTEGER NOT NULL DEFAULT 0,
    pushed_at    TIMESTAMPTZ,
    confirmed_at TIMESTAMPTZ,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT org_workspace_marks_key UNIQUE (daemon_id, workspace_id),
    CONSTRAINT org_workspace_marks_kind_check CHECK (kind IN ('home', 'stream')),
    CONSTRAINT org_workspace_marks_state_check CHECK (state IN ('central', 'pushed', 'confirmed'))
);

-- ---- invariants --------------------------------------------------------------

-- Manager tree: no cycles, the manager is a live (non-retired) member of the
-- same org. Runs under the advisory lock the statement trigger took, so two
-- concurrent edits cannot each pass and together close a loop.
CREATE OR REPLACE FUNCTION members_check_manager() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    cur TEXT := NEW.manager_member_id;
    depth INTEGER := 0;
    mgr RECORD;
BEGIN
    IF cur IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT status, org_id INTO mgr FROM members WHERE id = cur;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'org_unknown_manager: % does not exist', cur USING ERRCODE = 'check_violation';
    END IF;
    IF mgr.status = 'retired' THEN
        RAISE EXCEPTION 'org_retired_manager: % is retired', cur USING ERRCODE = 'check_violation';
    END IF;
    IF mgr.org_id <> NEW.org_id THEN
        RAISE EXCEPTION 'org_cross_org_manager: % is in another org', cur USING ERRCODE = 'check_violation';
    END IF;
    WHILE cur IS NOT NULL LOOP
        IF cur = NEW.id THEN
            RAISE EXCEPTION 'org_cycle: % would manage itself', NEW.id USING ERRCODE = 'check_violation';
        END IF;
        depth := depth + 1;
        IF depth > 1000 THEN
            RAISE EXCEPTION 'org_cycle: manager chain too deep' USING ERRCODE = 'check_violation';
        END IF;
        SELECT manager_member_id INTO cur FROM members WHERE id = cur;
    END LOOP;
    RETURN NEW;
END $$;

CREATE OR REPLACE TRIGGER members_check_manager
    BEFORE INSERT OR UPDATE OF manager_member_id ON members
    FOR EACH ROW EXECUTE FUNCTION members_check_manager();

-- Team tree: parent_team_id is acyclic.
CREATE OR REPLACE FUNCTION initiatives_check_parent_team() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    cur UUID := NEW.parent_team_id;
    depth INTEGER := 0;
BEGIN
    WHILE cur IS NOT NULL LOOP
        IF cur = NEW.id THEN
            RAISE EXCEPTION 'org_cycle: team % would contain itself', NEW.id USING ERRCODE = 'check_violation';
        END IF;
        depth := depth + 1;
        IF depth > 1000 THEN
            RAISE EXCEPTION 'org_cycle: team chain too deep' USING ERRCODE = 'check_violation';
        END IF;
        SELECT parent_team_id INTO cur FROM initiatives WHERE id = cur;
    END LOOP;
    RETURN NEW;
END $$;

CREATE OR REPLACE TRIGGER initiatives_check_parent_team
    BEFORE INSERT OR UPDATE OF parent_team_id ON initiatives
    FOR EACH ROW EXECUTE FUNCTION initiatives_check_parent_team();

-- ---- change feed ---------------------------------------------------------------

-- One row per changed member or team; readers re-query the subject. Org
-- writes are rare (roster, adoption, moves), so the log is not pruned.
CREATE TABLE IF NOT EXISTS org_changes (
    seq          BIGSERIAL PRIMARY KEY,
    subject_type TEXT NOT NULL,                  -- member | team
    subject_id   TEXT NOT NULL,
    op           TEXT NOT NULL CHECK (op IN ('upsert', 'delete')),
    changed_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE OR REPLACE FUNCTION org_changes_note(subject_type TEXT, subject_id TEXT, op TEXT)
RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    new_seq BIGINT;
BEGIN
    IF subject_id IS NULL THEN
        RETURN;
    END IF;
    INSERT INTO org_changes (subject_type, subject_id, op)
        VALUES (subject_type, subject_id, op) RETURNING seq INTO new_seq;
    PERFORM pg_notify('cm_org_changes', new_seq::text);
END $$;

CREATE OR REPLACE FUNCTION org_changes_record() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
    rec RECORD;
BEGIN
    IF TG_OP = 'DELETE' THEN
        rec := OLD;
    ELSE
        rec := NEW;
    END IF;
    IF TG_TABLE_NAME = 'members' THEN
        PERFORM org_changes_note('member', rec.id, CASE WHEN TG_OP = 'DELETE' THEN 'delete' ELSE 'upsert' END);
    ELSIF TG_TABLE_NAME = 'team_memberships' THEN
        PERFORM org_changes_note('member', rec.member_id, 'upsert');
        PERFORM org_changes_note('team', rec.team_id::text, 'upsert');
    ELSE
        -- member_aliases, continuous_members, incarnations, org_workspace_marks
        PERFORM org_changes_note('member', rec.member_id, 'upsert');
    END IF;
    RETURN NULL;
END $$;

-- Initiatives feed the org snapshot's teams: insert, delete, and edits of the
-- fields a team shows.
CREATE OR REPLACE FUNCTION org_changes_record_team() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        PERFORM org_changes_note('team', OLD.id::text, 'delete');
        RETURN NULL;
    END IF;
    IF TG_OP = 'UPDATE' AND (
        NEW.slug IS NOT DISTINCT FROM OLD.slug
        AND NEW.name IS NOT DISTINCT FROM OLD.name
        AND NEW.kind IS NOT DISTINCT FROM OLD.kind
        AND NEW.status IS NOT DISTINCT FROM OLD.status
        AND NEW.shared_channel IS NOT DISTINCT FROM OLD.shared_channel
        AND NEW.coordinator_task_id IS NOT DISTINCT FROM OLD.coordinator_task_id
        AND NEW.parent_team_id IS NOT DISTINCT FROM OLD.parent_team_id
        AND NEW.lead_member_id IS NOT DISTINCT FROM OLD.lead_member_id
        AND NEW.charter IS NOT DISTINCT FROM OLD.charter
        AND NEW.standing_rules IS NOT DISTINCT FROM OLD.standing_rules
        AND NEW.team_status IS NOT DISTINCT FROM OLD.team_status
        AND NEW.org_id IS NOT DISTINCT FROM OLD.org_id
        AND NEW.team_revision IS NOT DISTINCT FROM OLD.team_revision
    ) THEN
        RETURN NULL;
    END IF;
    PERFORM org_changes_note('team', NEW.id::text, 'upsert');
    RETURN NULL;
END $$;

CREATE OR REPLACE TRIGGER members_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON members
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();
CREATE OR REPLACE TRIGGER members_org_changes
    AFTER INSERT OR UPDATE OR DELETE ON members
    FOR EACH ROW EXECUTE FUNCTION org_changes_record();

CREATE OR REPLACE TRIGGER member_aliases_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON member_aliases
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();
CREATE OR REPLACE TRIGGER member_aliases_org_changes
    AFTER INSERT OR UPDATE OR DELETE ON member_aliases
    FOR EACH ROW EXECUTE FUNCTION org_changes_record();

CREATE OR REPLACE TRIGGER continuous_members_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON continuous_members
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();
CREATE OR REPLACE TRIGGER continuous_members_org_changes
    AFTER INSERT OR UPDATE OR DELETE ON continuous_members
    FOR EACH ROW EXECUTE FUNCTION org_changes_record();

CREATE OR REPLACE TRIGGER incarnations_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON incarnations
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();
CREATE OR REPLACE TRIGGER incarnations_org_changes
    AFTER INSERT OR UPDATE OR DELETE ON incarnations
    FOR EACH ROW EXECUTE FUNCTION org_changes_record();

CREATE OR REPLACE TRIGGER team_memberships_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON team_memberships
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();
CREATE OR REPLACE TRIGGER team_memberships_org_changes
    AFTER INSERT OR UPDATE OR DELETE ON team_memberships
    FOR EACH ROW EXECUTE FUNCTION org_changes_record();

CREATE OR REPLACE TRIGGER org_workspace_marks_serialize
    BEFORE INSERT OR UPDATE OR DELETE ON org_workspace_marks
    FOR EACH STATEMENT EXECUTE FUNCTION task_changes_serialize();
-- A push attempt only bumps `attempts`; a mark changes for readers when its
-- state, member or path does.
CREATE OR REPLACE TRIGGER org_workspace_marks_org_changes
    AFTER INSERT OR DELETE ON org_workspace_marks
    FOR EACH ROW EXECUTE FUNCTION org_changes_record();
CREATE OR REPLACE TRIGGER org_workspace_marks_org_changes_update
    AFTER UPDATE ON org_workspace_marks
    FOR EACH ROW WHEN (OLD.state IS DISTINCT FROM NEW.state
                       OR OLD.member_id IS DISTINCT FROM NEW.member_id
                       OR OLD.path IS DISTINCT FROM NEW.path)
    EXECUTE FUNCTION org_changes_record();

CREATE OR REPLACE TRIGGER initiatives_org_changes
    AFTER INSERT OR UPDATE OR DELETE ON initiatives
    FOR EACH ROW EXECUTE FUNCTION org_changes_record_team();

-- Owner is a member: the root of the manager tree, with no sessions.
INSERT INTO members (id, handle, kind, status, created_by)
    VALUES ('mem_owner', 'owner', 'owner', 'active', 'migration')
    ON CONFLICT DO NOTHING;
INSERT INTO member_aliases (org_id, alias, member_id, kind)
    VALUES ('main', 'owner', 'mem_owner', 'handle')
    ON CONFLICT DO NOTHING;
INSERT INTO org_meta (singleton) VALUES (TRUE) ON CONFLICT DO NOTHING;
