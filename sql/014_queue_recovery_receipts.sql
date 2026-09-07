-- Recovery idempotency outlives the queue's pending/claimed dedupe window.
-- Preserve the original item ID and keep a receipt even after it is consumed
-- again. No existing queue row is changed by installing this table.
CREATE TABLE IF NOT EXISTS queue_recovery_receipts (
    queue TEXT NOT NULL,
    recovery_key TEXT NOT NULL,
    item_id UUID NOT NULL,
    claimed_by TEXT NOT NULL,
    recovered_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (queue, recovery_key)
);
