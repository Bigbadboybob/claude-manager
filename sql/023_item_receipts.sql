-- Gate receipts on board items (doc/gate-receipts.md, v1 cut §11).
-- Rerunnable DDL only: no row UPDATEs, no backfill.

-- One row per gate run.  Append-only, except the staleness, re-evaluation
-- and refresh columns.
CREATE TABLE IF NOT EXISTS item_receipts (
    id                BIGSERIAL PRIMARY KEY,
    board_id          UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    item_id           BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    seq               INTEGER NOT NULL,          -- per item: r-<board>-<n>-<seq>
    gate              TEXT NOT NULL DEFAULT 'gate',
    class             TEXT,
    profile_version   INTEGER,
    head_sha          TEXT NOT NULL,
    mf_sha            TEXT,
    runner            JSONB,                     -- {ref, sha}
    ruling_id         TEXT,
    base              JSONB NOT NULL DEFAULT '{}'::jsonb,
    closure           JSONB,                     -- {paths, digest, count, cmd}
    worktree          JSONB NOT NULL DEFAULT '{}'::jsonb,
    inputs            JSONB NOT NULL DEFAULT '[]'::jsonb,
    steps             JSONB NOT NULL,            -- incl. CM-parsed fields
    waivers           TEXT[] NOT NULL DEFAULT '{}',
    repo              TEXT,
    branch            TEXT,
    log_path          TEXT,
    attestation       TEXT NOT NULL,
    computed          JSONB NOT NULL,            -- {verdict, reasons} at post time
    verdict           JSONB NOT NULL,            -- the latest evaluation (grants change it)
    brief             JSONB NOT NULL DEFAULT '[]'::jsonb,  -- [{name, state, rc, parsed}] for board reads
    stale_at          TIMESTAMPTZ,
    stale_reason      JSONB,                     -- {reason, prev, new}
    checked_at        TIMESTAMPTZ,               -- last refresh observation
    poster_pid        TEXT NOT NULL,
    poster_daemon_id  TEXT,
    request_id        TEXT,
    posted_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT item_receipts_seq_key UNIQUE (item_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_item_receipts_latest
    ON item_receipts (item_id, gate, id DESC);
CREATE INDEX IF NOT EXISTS idx_item_receipts_board_live
    ON item_receipts (board_id) WHERE stale_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_item_receipts_poster
    ON item_receipts (poster_daemon_id) WHERE stale_at IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_item_receipts_request
    ON item_receipts (board_id, request_id) WHERE request_id IS NOT NULL;

-- Input identities, for input_changed, input_void and the derived-artifact
-- cascade.  `locator` is the scoping key: a resolved local path or object URL,
-- else `item:<item id>:<name>`, so unrelated items never share a key.
CREATE TABLE IF NOT EXISTS item_receipt_inputs (
    receipt_id    BIGINT NOT NULL REFERENCES item_receipts(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    locator       TEXT NOT NULL,
    content_id    TEXT,
    state         TEXT NOT NULL,
    produced_by   BIGINT REFERENCES item_receipts(id) ON DELETE SET NULL,
    PRIMARY KEY (receipt_id, name)
);
CREATE INDEX IF NOT EXISTS idx_item_receipt_inputs_locator ON item_receipt_inputs (locator);
CREATE INDEX IF NOT EXISTS idx_item_receipt_inputs_producer ON item_receipt_inputs (produced_by);

-- Waiver and variance grants, attached only by their grantor.
CREATE TABLE IF NOT EXISTS item_grants (
    id            BIGSERIAL PRIMARY KEY,
    board_id      UUID NOT NULL REFERENCES boards(id) ON DELETE CASCADE,
    item_id       BIGINT NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    seq           INTEGER NOT NULL,              -- per item: g-<board>-<n>-<seq>
    kind          TEXT NOT NULL CHECK (kind IN ('waiver', 'variance')),
    gate          TEXT NOT NULL DEFAULT 'gate',
    grantor_pid   TEXT NOT NULL,
    head_sha      TEXT NOT NULL,
    base_sha      TEXT,
    step          TEXT NOT NULL,
    nodes         TEXT[],
    cmd_digest    TEXT,
    reason        TEXT NOT NULL,
    risk          TEXT NOT NULL,
    message_id    TEXT NOT NULL,
    message_check JSONB NOT NULL,                -- {found, sender, contains_head, contains_base}
    revoked_at    TIMESTAMPTZ,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT item_grants_seq_key UNIQUE (item_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_item_grants_item ON item_grants (item_id);

-- What the item says is current, and which profile class it uses.
ALTER TABLE items  ADD COLUMN IF NOT EXISTS gate_class TEXT;
ALTER TABLE items  ADD COLUMN IF NOT EXISTS gate_binding JSONB NOT NULL DEFAULT '{}'::jsonb;
-- {class: {version, steps, waivable, require, base, inputs, ...}}
ALTER TABLE boards ADD COLUMN IF NOT EXISTS gate_profiles JSONB NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE boards ADD COLUMN IF NOT EXISTS gate_default_class TEXT;
ALTER TABLE boards ADD COLUMN IF NOT EXISTS gate_editors TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE boards ADD COLUMN IF NOT EXISTS waiver_grantors TEXT[] NOT NULL DEFAULT '{}';
