-- ADR-0192 / ARN-238: reject delayed writers from an older hot-swapped spec.
--
-- `key_set` tracks the latest contract used for reconciliation and revision
-- fencing. `activated_key_set` is stronger: once a spec activation publishes
-- it, a live writer carrying any other signature must fail instead of changing
-- the contract back and reintroducing rows that the new spec released.
ALTER TABLE key_index_contract_state
    ADD COLUMN IF NOT EXISTS activated_key_set TEXT,
    ADD COLUMN IF NOT EXISTS activated_spec_fingerprint TEXT,
    ADD COLUMN IF NOT EXISTS activation_epoch BIGINT NOT NULL DEFAULT 0
        CHECK (activation_epoch >= 0);

-- Composite retries must not rescan an ever-growing parent journal. The claim
-- and content hash commit in the same transaction as every participating
-- stream, so a retry is a constant-time exact no-op and key reuse cannot hide
-- different work.
CREATE TABLE IF NOT EXISTS persistence_batch_idempotency (
    persistence_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    intent_hash TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (persistence_id, idempotency_key)
);
