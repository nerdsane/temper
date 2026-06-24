-- ADR-0153: declared composite-key index — the negative-existence access path.
--
-- One row per (declared key, entity), co-committed with the journal append in the
-- same transaction (unlike the eventually-consistent entity_field_index). A keyed
-- read becomes a single O(log n) probe: hit -> entity_id, miss -> authoritatively
-- absent, so the read plane no longer has to scan a whole entity type to prove
-- absence (the cause of the 413, ARN-68).
--
-- The PRIMARY KEY (tenant, entity_type, key_name, key_hash) enforces declared-key
-- uniqueness: a second entity claiming the same key tuple conflicts, which the
-- write path surfaces as a typed error (reject-and-surface). key_hash is a
-- canonical, type-tagged hash of the declared key's values. sequence_nr carries
-- the journal position for the staleness guard on upsert.
CREATE TABLE IF NOT EXISTS entity_key_index (
    tenant       TEXT   NOT NULL,
    entity_type  TEXT   NOT NULL,
    key_name     TEXT   NOT NULL,
    key_hash     TEXT   NOT NULL,
    entity_id    TEXT   NOT NULL,
    sequence_nr  BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, entity_type, key_name, key_hash)
);

-- Reverse lookup: all key rows for an entity, so the write path can upsert/delete
-- an entity's declared-key rows when it changes or is removed.
CREATE INDEX IF NOT EXISTS idx_eki_entity
    ON entity_key_index (tenant, entity_type, entity_id);
