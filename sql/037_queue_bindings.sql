-- Lead-bound queue wake (continuous teams phase 3, item 2).
-- The binding is keyed by a durable org member id or handle, never a session
-- uid, so replacing a member's main session does not orphan the queue.

CREATE TABLE IF NOT EXISTS queue_bindings (
    queue              TEXT PRIMARY KEY,
    consumer           TEXT NOT NULL,
    bound_by           TEXT NOT NULL,
    bound_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_wake_at       TIMESTAMPTZ,
    last_consume_at    TIMESTAMPTZ,
    wake_pending       BOOLEAN NOT NULL DEFAULT FALSE,
    wake_id            UUID,
    wake_delivery      TEXT NOT NULL DEFAULT 'pending'
                       CHECK (wake_delivery IN ('pending', 'delivered', 'failed')),
    wake_error         TEXT
);

ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS claim_token UUID;
ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS lease_until TIMESTAMPTZ;
ALTER TABLE queue_items ADD COLUMN IF NOT EXISTS claimed_consumer TEXT;
ALTER TABLE queue_bindings ADD COLUMN IF NOT EXISTS wake_id UUID;

CREATE INDEX IF NOT EXISTS idx_queue_items_lease
    ON queue_items (queue, lease_until)
    WHERE state = 'claimed';
