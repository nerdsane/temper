/// One row per live entity in the durable query plane.
pub const CREATE_ENTITY_CATALOG_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS entity_catalog (
    tenant             TEXT NOT NULL,
    entity_type        TEXT NOT NULL,
    entity_id          TEXT NOT NULL,
    status             TEXT NOT NULL,
    fields             TEXT NOT NULL DEFAULT '{}',
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
