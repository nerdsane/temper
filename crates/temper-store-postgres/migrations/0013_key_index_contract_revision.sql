-- ADR-0171 / ARN-238: fence declared-key coverage against live key-contract changes.
--
-- A durable write records the versioned key signature used to derive its exact rows.
-- Changing that signature advances `revision` and invalidates the coverage watermark.
-- Backfill publishes a new watermark only if this revision stayed unchanged while it
-- replayed the type, preventing remove/re-add and A -> B -> A ABA certification.
CREATE TABLE IF NOT EXISTS key_index_contract_state (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    key_set TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision >= 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type)
);
