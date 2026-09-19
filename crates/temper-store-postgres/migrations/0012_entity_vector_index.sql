-- ADR-0155: declared vector access path — the exact-scan kNN index.
--
-- One row per (declared vector path, model tag, entity), co-committed with the
-- journal append in the same transaction (like entity_key_index, unlike the
-- eventually-consistent entity_field_index). Unlike a key row this has NO
-- uniqueness constraint beyond the primary key identity — vectors are derived,
-- rebuildable ranking state, and two entities may hold identical vectors.
--
-- `vector` is packed little-endian f32 (the journal keeps the human-readable JSON
-- on the action event; this blob is the derived copy). `model_tag` partitions the
-- space: a kNN query resolves against exactly one tag, so vectors from different
-- embedding models are never compared. `sequence_nr` carries the journal position.
CREATE TABLE IF NOT EXISTS entity_vector_index (
    tenant       TEXT   NOT NULL,
    entity_type  TEXT   NOT NULL,
    decl_name    TEXT   NOT NULL,
    model_tag    TEXT   NOT NULL,
    entity_id    TEXT   NOT NULL,
    vector       BYTEA  NOT NULL,
    sequence_nr  BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, entity_type, decl_name, model_tag, entity_id)
);

-- The kNN candidate scan: every vector in one (tenant, type, decl, model) partition,
-- read in entity_id order for deterministic ranking.
CREATE INDEX IF NOT EXISTS idx_evi_partition
    ON entity_vector_index (tenant, entity_type, decl_name, model_tag, entity_id);

-- Reverse lookup: all vector rows for an entity, so the write path can drop an
-- entity's prior rows for a decl when its vector or model tag changes.
CREATE INDEX IF NOT EXISTS idx_evi_entity
    ON entity_vector_index (tenant, entity_type, entity_id);

-- Per-(tenant, entity_type) backfill watermark (ADR-0155), mirroring
-- key_index_backfill_watermark. A row asserts every existing entity of the type has
-- had its declared vectors indexed, and records the covered vector-path set so a
-- newly declared path is detected as a set change and re-indexed. vector_set is the
-- sorted, comma-joined declared vector-path names the backfill covered.
CREATE TABLE IF NOT EXISTS vector_index_backfill_watermark (
    tenant       TEXT        NOT NULL,
    entity_type  TEXT        NOT NULL,
    vector_set   TEXT        NOT NULL DEFAULT '',
    completed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type)
);
