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
/// One row per (declared key, entity), co-committed with the journal append (unlike
/// the eventually-consistent `entity_field_index`). A keyed read is a single
/// O(log n) probe: hit -> entity_id, miss -> authoritatively absent — so the read
/// plane no longer scans a whole entity type to prove absence (the 413, ARN-68).
/// The PRIMARY KEY enforces declared-key uniqueness (reject-and-surface on conflict);
/// `key_hash` is a canonical, type-tagged hash of the declared key's values.
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
/// f32; `model_tag` partitions the space. Turso co-commits these rows and their
/// retained per-entity sequence fence with the journal append (ADR-0171).
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

/// ADR-0171 retained per-entity vector reconciliation fence. This row remains even
/// when reconciliation produces no vector rows, preventing stale resurrection.
pub const CREATE_ENTITY_VECTOR_INDEX_VERSION_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_vector_index_version (
    tenant                    TEXT NOT NULL,
    entity_type               TEXT NOT NULL,
    entity_id                 TEXT NOT NULL,
    reconciliation_generation INTEGER NOT NULL DEFAULT 0,
    sequence_nr               INTEGER NOT NULL,
    PRIMARY KEY (tenant, entity_type, entity_id)
);";

/// Idempotent-at-bootstrap upgrade for databases created before ADR-0171 gained
/// declaration-set generations. Duplicate-column errors are ignored by the caller.
pub const ALTER_ENTITY_VECTOR_INDEX_VERSION_ADD_GENERATION: &str = "\
ALTER TABLE entity_vector_index_version
ADD COLUMN reconciliation_generation INTEGER NOT NULL DEFAULT 0";

/// Durable ordering token for overlapping declaration-set reconciliation.
pub const CREATE_VECTOR_RECONCILIATION_GENERATION_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_vector_reconciliation_generation (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    generation   INTEGER NOT NULL,
    vector_set   TEXT NOT NULL,
    PRIMARY KEY (tenant, entity_type)
);";

/// Seed the retained fence when upgrading a database that already has vector rows.
pub const SEED_ENTITY_VECTOR_INDEX_VERSION_TABLE: &str = "\
INSERT INTO entity_vector_index_version
    (tenant, entity_type, entity_id, reconciliation_generation, sequence_nr)
SELECT tenant, entity_type, entity_id, 0, MAX(sequence_nr)
FROM entity_vector_index
GROUP BY tenant, entity_type, entity_id
ON CONFLICT(tenant, entity_type, entity_id)
DO UPDATE SET
    reconciliation_generation = MAX(
        entity_vector_index_version.reconciliation_generation,
        excluded.reconciliation_generation
    ),
    sequence_nr = CASE
        WHEN entity_vector_index_version.reconciliation_generation
             = excluded.reconciliation_generation
        THEN MAX(entity_vector_index_version.sequence_nr, excluded.sequence_nr)
        ELSE entity_vector_index_version.sequence_nr
    END;";

/// Per-(tenant, entity_type) vector-index backfill watermark (ADR-0155): records
/// the covered vector-path set so a keyed read knows when the index is complete and
/// re-indexes on a set change. Mirrors `key_index_backfill_watermark`.
pub const CREATE_VECTOR_INDEX_BACKFILL_WATERMARK: &str = "\
CREATE TABLE IF NOT EXISTS vector_index_backfill_watermark (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    vector_set   TEXT NOT NULL DEFAULT '',
    completed_at TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (tenant, entity_type)
);";
