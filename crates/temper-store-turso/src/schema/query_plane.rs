/// One row per live entity in the durable query plane.
pub const CREATE_ENTITY_CATALOG_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_catalog (
    tenant             TEXT NOT NULL,
    entity_type        TEXT NOT NULL,
    entity_id          TEXT NOT NULL,
    status             TEXT NOT NULL,
    fields             TEXT NOT NULL DEFAULT '{}',
    state              TEXT,
    updated_at         TEXT NOT NULL,
    sequence_nr        INTEGER NOT NULL DEFAULT 0,
    projection_version INTEGER NOT NULL DEFAULT 2,
    projection_hash    TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (tenant, entity_type, entity_id)
);";

/// Fast path for collection lookups by tenant/entity type.
pub const CREATE_ENTITY_CATALOG_TYPE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_entity_catalog_type
    ON entity_catalog(tenant, entity_type);";

/// Fast path for status-based collection filtering.
pub const CREATE_ENTITY_CATALOG_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_entity_catalog_status
    ON entity_catalog(tenant, entity_type, status);";

/// Entity-Attribute-Value field index for SQL-level OData filter push-down.
///
/// Mirrors top-level scalar fields from entity state so that `$filter`
/// expressions can be translated to SQL WHERE clauses, avoiding full
/// materialization of every actor in a collection query.
pub const CREATE_ENTITY_FIELD_INDEX_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_field_index (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    entity_id    TEXT NOT NULL,
    field_name   TEXT NOT NULL,
    field_value  TEXT,
    status       TEXT,
    PRIMARY KEY (tenant, entity_type, entity_id, field_name)
);";

/// Composite index for field-value lookups (the hot path for filter push-down).
pub const CREATE_ENTITY_FIELD_INDEX_LOOKUP: &str = "\
CREATE INDEX IF NOT EXISTS idx_efi_lookup
    ON entity_field_index(tenant, entity_type, field_name, field_value);";

/// Index for status-based filtering.
pub const CREATE_ENTITY_FIELD_INDEX_STATUS: &str = "\
CREATE INDEX IF NOT EXISTS idx_efi_status
    ON entity_field_index(tenant, entity_type, status);";

/// ADR-0153: declared composite-key index — the negative-existence access path.
///
/// Shared schema for one row per (declared key, entity). Turso does not yet maintain
/// these rows on every live write and therefore never advertises authoritative keyed
/// absence; Postgres owns that capability. The PRIMARY KEY still expresses the
/// eventual uniqueness contract, and `key_hash` is the canonical type-tagged hash.
pub const CREATE_ENTITY_KEY_INDEX_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_key_index (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    key_name     TEXT NOT NULL,
    key_hash     TEXT NOT NULL,
    entity_id    TEXT NOT NULL,
    sequence_nr  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, entity_type, key_name, key_hash)
);";

/// Reverse lookup: all key rows for an entity, so the write path can upsert/delete
/// an entity's declared-key rows when it changes or is removed.
pub const CREATE_ENTITY_KEY_INDEX_ENTITY: &str = "\
CREATE INDEX IF NOT EXISTS idx_eki_entity
    ON entity_key_index(tenant, entity_type, entity_id);";

/// ADR-0155: declared vector access path — the exact-scan kNN index. One row per
/// (declared vector path, model tag, entity). `vector` is packed little-endian
/// f32; `model_tag` partitions the space. Turso's single-stream path maintains this
/// write-behind with journal-sequence fencing; atomic multi-stream batches co-commit
/// rows in their transaction. Startup always reconciles because no durable completion
/// watermark is trusted after a potentially exhausted write-behind retry.
pub const CREATE_ENTITY_VECTOR_INDEX_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_vector_index (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    decl_name    TEXT NOT NULL,
    model_tag    TEXT NOT NULL,
    entity_id    TEXT NOT NULL,
    vector       BLOB NOT NULL,
    sequence_nr  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant, entity_type, decl_name, model_tag, entity_id)
);";

/// The kNN candidate scan: every vector in one partition, in entity_id order.
pub const CREATE_ENTITY_VECTOR_INDEX_PARTITION: &str = "\
CREATE INDEX IF NOT EXISTS idx_evi_partition
    ON entity_vector_index(tenant, entity_type, decl_name, model_tag, entity_id);";

/// Reverse lookup: all vector rows for an entity, so the write path can replace an
/// entity's rows for a decl when its vector or model tag changes.
pub const CREATE_ENTITY_VECTOR_INDEX_ENTITY: &str = "\
CREATE INDEX IF NOT EXISTS idx_evi_entity
    ON entity_vector_index(tenant, entity_type, entity_id);";

/// Legacy/reserved vector-index watermark schema. Current Turso code deliberately
/// does not read or write this table; startup replay is the repair authority. The
/// table remains for on-disk compatibility and a future fully co-committed backend.
pub const CREATE_VECTOR_INDEX_BACKFILL_WATERMARK: &str = "\
CREATE TABLE IF NOT EXISTS vector_index_backfill_watermark (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    vector_set   TEXT NOT NULL DEFAULT '',
    completed_at TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (tenant, entity_type)
);";
