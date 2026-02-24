//! SQL schema definitions for the PostgreSQL persistence store.
//!
//! These constants define the DDL statements used to create the `events` and
//! `snapshots` tables that back the event-sourced persistence layer.

/// CREATE TABLE statement for the events journal.
///
/// Each row stores a single domain event for a particular entity. The
/// `(tenant, entity_type, entity_id, sequence_nr)` UNIQUE constraint enforces
/// optimistic concurrency — two writers attempting to persist the same
/// sequence number will conflict, and only one will succeed.
///
/// The `tenant` column defaults to `'default'` for backward compatibility
/// with single-tenant deployments.
pub const CREATE_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS events (
    id            BIGSERIAL    NOT NULL,
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    event_type    TEXT         NOT NULL,
    payload       JSONB        NOT NULL,
    metadata      JSONB        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    UNIQUE (tenant, entity_type, entity_id, sequence_nr)
);";

/// CREATE TABLE statement for the snapshots table.
///
/// Snapshots store the serialised state of an entity at a given sequence
/// number. Only the latest snapshot per entity is kept — the UPSERT in
/// `save_snapshot` replaces older rows via the composite primary key.
pub const CREATE_SNAPSHOTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS snapshots (
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    state         BYTEA        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id)
);";

/// CREATE INDEX statement for efficient `list_entity_ids` scans.
///
/// This covering index supports filtering by `tenant` and returning distinct
/// `(entity_type, entity_id)` pairs from the events journal.
pub const CREATE_ENTITY_LISTING_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_events_tenant_entity
    ON events (tenant, entity_type, entity_id);";

/// CREATE TABLE statement for persisted specs and verification status.
pub const CREATE_SPECS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS specs (
    id                  BIGSERIAL    PRIMARY KEY,
    tenant              TEXT         NOT NULL,
    entity_type         TEXT         NOT NULL,
    ioa_source          TEXT         NOT NULL,
    csdl_xml            TEXT,
    version             INT          NOT NULL DEFAULT 1,
    verified            BOOLEAN      NOT NULL DEFAULT false,
    verification_status TEXT         NOT NULL DEFAULT 'pending',
    levels_passed       INT,
    levels_total        INT,
    verification_result JSONB,
    created_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (tenant, entity_type)
);";

/// CREATE TABLE statement for persisted trajectory action outcomes.
pub const CREATE_TRAJECTORIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS trajectories (
    id            BIGSERIAL    PRIMARY KEY,
    tenant        TEXT         NOT NULL,
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL DEFAULT '',
    action        TEXT         NOT NULL,
    success       BOOLEAN      NOT NULL,
    from_status   TEXT,
    to_status     TEXT,
    error         TEXT,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE INDEX statement for trajectory success filtering.
pub const CREATE_TRAJECTORIES_SUCCESS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_success ON trajectories (success, created_at DESC);";

/// CREATE INDEX statement for trajectory action/entity grouping.
pub const CREATE_TRAJECTORIES_ENTITY_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_entity ON trajectories (entity_type, action);";

/// CREATE TABLE statement for persisted design-time workflow events.
pub const CREATE_DESIGN_TIME_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS design_time_events (
    id            BIGSERIAL    PRIMARY KEY,
    kind          TEXT         NOT NULL,
    entity_type   TEXT         NOT NULL,
    tenant        TEXT         NOT NULL,
    summary       TEXT         NOT NULL,
    level         TEXT,
    passed        BOOLEAN,
    step_number   SMALLINT,
    total_steps   SMALLINT,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE INDEX statement for tenant-scoped design-time history queries.
pub const CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_dt_events_tenant ON design_time_events (tenant, created_at DESC);";

/// CREATE TABLE statement for tenant-level cross-entity constraint definitions.
pub const CREATE_TENANT_CONSTRAINTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_constraints (
    tenant                TEXT         NOT NULL PRIMARY KEY,
    cross_invariants_toml TEXT         NOT NULL,
    version               INT          NOT NULL DEFAULT 1,
    updated_at            TIMESTAMPTZ  NOT NULL DEFAULT now()
);";

/// CREATE TABLE statement for WASM module storage.
///
/// Stores compiled WASM binaries for agent-generated integration handlers.
/// Keyed by (tenant, module_name) with version tracking and SHA-256 integrity.
pub const CREATE_WASM_MODULES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS wasm_modules (
    tenant        TEXT         NOT NULL,
    module_name   TEXT         NOT NULL,
    wasm_bytes    BYTEA        NOT NULL,
    sha256_hash   TEXT         NOT NULL,
    version       INT          NOT NULL DEFAULT 1,
    size_bytes    INT          NOT NULL,
    updated_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    UNIQUE (tenant, module_name)
);";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_table_contains_required_columns() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        for col in &[
            "TENANT",
            "ENTITY_TYPE",
            "ENTITY_ID",
            "SEQUENCE_NR",
            "EVENT_TYPE",
            "PAYLOAD",
            "METADATA",
            "CREATED_AT",
        ] {
            assert!(sql.contains(col), "events schema missing column: {col}");
        }
    }

    #[test]
    fn events_table_has_unique_constraint() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        assert!(
            sql.contains("UNIQUE"),
            "events schema should enforce a UNIQUE constraint on (tenant, entity_type, entity_id, sequence_nr)"
        );
        let unique_pos = sql.find("UNIQUE").unwrap();
        let after_unique = &sql[unique_pos..];
        assert!(
            after_unique.contains("TENANT"),
            "UNIQUE constraint missing tenant"
        );
        assert!(
            after_unique.contains("ENTITY_TYPE"),
            "UNIQUE constraint missing entity_type"
        );
        assert!(
            after_unique.contains("ENTITY_ID"),
            "UNIQUE constraint missing entity_id"
        );
        assert!(
            after_unique.contains("SEQUENCE_NR"),
            "UNIQUE constraint missing sequence_nr"
        );
    }

    #[test]
    fn snapshots_table_contains_required_columns() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        for col in &[
            "TENANT",
            "ENTITY_TYPE",
            "ENTITY_ID",
            "SEQUENCE_NR",
            "STATE",
            "CREATED_AT",
        ] {
            assert!(sql.contains(col), "snapshots schema missing column: {col}");
        }
    }

    #[test]
    fn snapshots_table_has_composite_primary_key() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        let pk_pos = sql
            .find("PRIMARY KEY")
            .expect("snapshots schema missing PRIMARY KEY");
        let after_pk = &sql[pk_pos..];
        assert!(after_pk.contains("TENANT"), "PRIMARY KEY missing tenant");
        assert!(
            after_pk.contains("ENTITY_TYPE"),
            "PRIMARY KEY missing entity_type"
        );
        assert!(
            after_pk.contains("ENTITY_ID"),
            "PRIMARY KEY missing entity_id"
        );
    }

    #[test]
    fn events_table_uses_jsonb_for_payload_and_metadata() {
        let sql = CREATE_EVENTS_TABLE.to_uppercase();
        // Find PAYLOAD line and verify it uses JSONB
        assert!(
            sql.contains("PAYLOAD") && sql.contains("JSONB"),
            "payload should be JSONB"
        );
    }

    #[test]
    fn snapshots_table_uses_bytea_for_state() {
        let sql = CREATE_SNAPSHOTS_TABLE.to_uppercase();
        assert!(
            sql.contains("STATE") && sql.contains("BYTEA"),
            "state should be BYTEA"
        );
    }

    #[test]
    fn schemas_use_if_not_exists() {
        assert!(
            CREATE_EVENTS_TABLE.contains("IF NOT EXISTS"),
            "events schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_SNAPSHOTS_TABLE.contains("IF NOT EXISTS"),
            "snapshots schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_SPECS_TABLE.contains("IF NOT EXISTS"),
            "specs schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_TRAJECTORIES_TABLE.contains("IF NOT EXISTS"),
            "trajectories schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_DESIGN_TIME_EVENTS_TABLE.contains("IF NOT EXISTS"),
            "design_time_events schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_TENANT_CONSTRAINTS_TABLE.contains("IF NOT EXISTS"),
            "tenant_constraints schema should use IF NOT EXISTS"
        );
        assert!(
            CREATE_WASM_MODULES_TABLE.contains("IF NOT EXISTS"),
            "wasm_modules schema should use IF NOT EXISTS"
        );
    }

    #[test]
    fn wasm_modules_table_has_required_columns() {
        let sql = CREATE_WASM_MODULES_TABLE.to_uppercase();
        for col in &["TENANT", "MODULE_NAME", "WASM_BYTES", "SHA256_HASH", "VERSION", "SIZE_BYTES"] {
            assert!(sql.contains(col), "wasm_modules schema missing column: {col}");
        }
    }

    #[test]
    fn wasm_modules_table_has_unique_constraint() {
        let sql = CREATE_WASM_MODULES_TABLE.to_uppercase();
        assert!(sql.contains("UNIQUE"), "wasm_modules should have UNIQUE constraint");
    }

    #[test]
    fn entity_listing_index_targets_tenant_type_and_id() {
        let sql = CREATE_ENTITY_LISTING_INDEX.to_uppercase();
        assert!(
            sql.contains("CREATE INDEX IF NOT EXISTS"),
            "index DDL should be idempotent"
        );
        assert!(
            sql.contains("IDX_EVENTS_TENANT_ENTITY"),
            "index name should be stable"
        );
        assert!(
            sql.contains("ON EVENTS"),
            "index should target events table"
        );
        assert!(
            sql.contains("(TENANT, ENTITY_TYPE, ENTITY_ID)"),
            "index should cover tenant/entity_type/entity_id"
        );
    }
}
