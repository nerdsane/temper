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

/// CREATE TABLE statement for pending authorization decisions.
///
/// Stores Cedar authorization denials awaiting human approval.
/// The full PendingDecision is stored as JSON in the `data` column.
pub const CREATE_PENDING_DECISIONS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS pending_decisions (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'pending',
    data TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_PENDING_DECISIONS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_pending_decisions_tenant
    ON pending_decisions(tenant);";

pub const CREATE_PENDING_DECISIONS_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_pending_decisions_status
    ON pending_decisions(status);";

/// Cedar policy storage per tenant.
pub const CREATE_TENANT_POLICIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_policies (
    tenant TEXT PRIMARY KEY,
    policy_text TEXT NOT NULL,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

// ---------------------------------------------------------------------------
// Phase 0: Turso as single source of truth — new tables + trajectory extensions
// ---------------------------------------------------------------------------

/// ALTER TABLE migrations for the `trajectories` table.
///
/// These add columns that were previously only tracked in-memory
/// (agent_id, session_id, authz_denied, etc.). Each statement uses
/// try-and-ignore semantics in SQLite (duplicate column is a no-op error).
pub const ALTER_TRAJECTORIES_ADD_AGENT_ID: &str =
    "ALTER TABLE trajectories ADD COLUMN agent_id TEXT";
pub const ALTER_TRAJECTORIES_ADD_SESSION_ID: &str =
    "ALTER TABLE trajectories ADD COLUMN session_id TEXT";
pub const ALTER_TRAJECTORIES_ADD_AUTHZ_DENIED: &str =
    "ALTER TABLE trajectories ADD COLUMN authz_denied INTEGER";
pub const ALTER_TRAJECTORIES_ADD_DENIED_RESOURCE: &str =
    "ALTER TABLE trajectories ADD COLUMN denied_resource TEXT";
pub const ALTER_TRAJECTORIES_ADD_DENIED_MODULE: &str =
    "ALTER TABLE trajectories ADD COLUMN denied_module TEXT";
pub const ALTER_TRAJECTORIES_ADD_SOURCE: &str = "ALTER TABLE trajectories ADD COLUMN source TEXT";
pub const ALTER_TRAJECTORIES_ADD_SPEC_GOVERNED: &str =
    "ALTER TABLE trajectories ADD COLUMN spec_governed INTEGER";

/// Index on agent_id for agent-scoped trajectory queries.
pub const CREATE_TRAJECTORIES_AGENT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_agent
    ON trajectories(agent_id);";

/// Feature request records generated from trajectory analysis.
pub const CREATE_FEATURE_REQUESTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS feature_requests (
    id TEXT PRIMARY KEY,
    category TEXT NOT NULL,
    description TEXT NOT NULL,
    frequency INTEGER NOT NULL DEFAULT 0,
    trajectory_refs TEXT NOT NULL DEFAULT '[]',
    disposition TEXT NOT NULL DEFAULT 'Open',
    developer_notes TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// Evolution record chain (O/P/A/D/I records).
pub const CREATE_EVOLUTION_RECORDS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS evolution_records (
    id TEXT PRIMARY KEY,
    record_type TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'Open',
    created_by TEXT NOT NULL,
    derived_from TEXT,
    data TEXT NOT NULL,
    timestamp TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_EVOLUTION_RECORDS_TYPE_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_type
    ON evolution_records(record_type);";

pub const CREATE_EVOLUTION_RECORDS_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_evolution_records_status
    ON evolution_records(status);";

/// Design-time events emitted during spec loading and verification.
pub const CREATE_DESIGN_TIME_EVENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS design_time_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    tenant TEXT NOT NULL,
    summary TEXT NOT NULL,
    level TEXT,
    passed INTEGER,
    step_number INTEGER,
    total_steps INTEGER,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_design_time_events_tenant
    ON design_time_events(tenant, entity_type);";

// ---------------------------------------------------------------------------
// Platform DB tables (tenant registry + user access)
// ---------------------------------------------------------------------------

/// Registry of provisioned tenant databases.
pub const CREATE_TENANT_REGISTRY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_registry (
    tenant_id TEXT PRIMARY KEY,
    turso_db_url TEXT NOT NULL,
    turso_auth_token TEXT,
    status TEXT NOT NULL DEFAULT 'active',
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

/// User-to-tenant access mappings.
pub const CREATE_TENANT_USERS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS tenant_users (
    tenant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    role TEXT NOT NULL DEFAULT 'member',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant_id, user_id)
);";

/// Index for looking up tenants by user.
pub const CREATE_TENANT_USERS_USER_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_tenant_users_user
    ON tenant_users(user_id);";

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
        assert!(CREATE_PENDING_DECISIONS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_PENDING_DECISIONS_TENANT_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_PENDING_DECISIONS_STATUS_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_TRAJECTORIES_AGENT_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_FEATURE_REQUESTS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_EVOLUTION_RECORDS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_EVOLUTION_RECORDS_TYPE_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_EVOLUTION_RECORDS_STATUS_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_DESIGN_TIME_EVENTS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_DESIGN_TIME_EVENTS_TENANT_INDEX.contains("IF NOT EXISTS"));
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
