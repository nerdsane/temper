//! SQLite-compatible schema for the Turso/libSQL event store.

pub const CREATE_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    event_type TEXT NOT NULL,
    payload TEXT NOT NULL,
    metadata TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, entity_type, entity_id, sequence_nr)
);";

pub const CREATE_EVENTS_ENTITY_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_events_entity
    ON events(tenant, entity_type, entity_id, sequence_nr);";

pub const CREATE_SNAPSHOTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS snapshots (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    snapshot BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, entity_type, entity_id)
);";

pub const CREATE_SPECS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS specs (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    ioa_source TEXT NOT NULL,
    csdl_xml TEXT,
    version INTEGER NOT NULL DEFAULT 1,
    verified INTEGER NOT NULL DEFAULT 0,
    verification_status TEXT NOT NULL DEFAULT 'pending',
    levels_passed INTEGER,
    levels_total INTEGER,
    verification_result TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, entity_type)
);";

pub const CREATE_TRAJECTORIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS trajectories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    action TEXT NOT NULL,
    success INTEGER NOT NULL DEFAULT 0,
    from_status TEXT,
    to_status TEXT,
    error TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_TRAJECTORIES_SUCCESS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_success
    ON trajectories(success);";

pub const CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_entity_action
    ON trajectories(tenant, entity_type, action);";

pub const CREATE_TENANT_CONSTRAINTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_constraints (
    tenant TEXT NOT NULL PRIMARY KEY,
    cross_invariants_toml TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// CREATE TABLE statement for WASM module storage.
///
/// Stores compiled WASM binaries for agent-generated integration handlers.
/// Keyed by (tenant, module_name) with version tracking and SHA-256 integrity.
pub const CREATE_WASM_MODULES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS wasm_modules (
    tenant TEXT NOT NULL,
    module_name TEXT NOT NULL,
    wasm_bytes BLOB NOT NULL,
    sha256_hash TEXT NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    size_bytes INTEGER NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(tenant, module_name)
);";

/// CREATE TABLE statement for WASM invocation logs.
///
/// Records every WASM integration invocation for observability and
/// persistence across server restarts.
pub const CREATE_WASM_INVOCATION_LOGS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS wasm_invocation_logs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    module_name TEXT NOT NULL,
    trigger_action TEXT NOT NULL,
    callback_action TEXT,
    success INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    duration_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// CREATE INDEX for filtering invocation logs by tenant.
pub const CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_tenant
    ON wasm_invocation_logs(tenant);";

/// CREATE INDEX for filtering invocation logs by module name.
pub const CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_module
    ON wasm_invocation_logs(module_name);";

/// CREATE INDEX for ordering invocation logs by creation time (newest first).
pub const CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_wasm_invocation_logs_created
    ON wasm_invocation_logs(created_at DESC);";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_idempotent() {
        assert!(CREATE_EVENTS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_EVENTS_ENTITY_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_SNAPSHOTS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_SPECS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_TRAJECTORIES_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_TRAJECTORIES_SUCCESS_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_TRAJECTORIES_ENTITY_ACTION_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_TENANT_CONSTRAINTS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_WASM_MODULES_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_WASM_INVOCATION_LOGS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_WASM_INVOCATION_LOGS_TENANT_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_WASM_INVOCATION_LOGS_MODULE_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_WASM_INVOCATION_LOGS_CREATED_INDEX.contains("IF NOT EXISTS"));
    }

    #[test]
    fn wasm_modules_table_has_required_columns() {
        let sql = CREATE_WASM_MODULES_TABLE.to_uppercase();
        for col in &[
            "TENANT",
            "MODULE_NAME",
            "WASM_BYTES",
            "SHA256_HASH",
            "VERSION",
            "SIZE_BYTES",
        ] {
            assert!(
                sql.contains(col),
                "wasm_modules schema missing column: {col}"
            );
        }
    }
}
