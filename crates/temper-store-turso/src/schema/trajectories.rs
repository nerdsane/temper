//! Trajectory and OTS-trajectory table schema.

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

pub const ALTER_TRAJECTORIES_ADD_REQUEST_BODY: &str =
    "ALTER TABLE trajectories ADD COLUMN request_body TEXT";

pub const ALTER_TRAJECTORIES_ADD_INTENT: &str = "ALTER TABLE trajectories ADD COLUMN intent TEXT";

pub const ALTER_TRAJECTORIES_ADD_MATCHED_POLICY_IDS: &str =
    "ALTER TABLE trajectories ADD COLUMN matched_policy_ids TEXT";

/// Index on agent_id for agent-scoped trajectory queries.
pub const CREATE_TRAJECTORIES_AGENT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_trajectories_agent
    ON trajectories(agent_id);";

/// Full OTS trajectory storage for GEPA self-improvement loop.
///
/// Stores complete agent execution traces (tool calls, decisions, reasoning)
/// captured by the MCP server during agent sessions. The `data` column holds
/// the full OTS JSON blob; indexed columns enable efficient filtering.
pub const CREATE_OTS_TRAJECTORIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS ots_trajectories (
    trajectory_id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    session_id TEXT,
    outcome TEXT NOT NULL DEFAULT 'unknown',
    entity_type TEXT,
    turn_count INTEGER NOT NULL DEFAULT 0,
    data TEXT NOT NULL,
    persistence_status TEXT NOT NULL DEFAULT 'persisted',
    persist_attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);";

pub const ALTER_OTS_TRAJECTORIES_ADD_PERSISTENCE_STATUS: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN persistence_status TEXT NOT NULL DEFAULT 'persisted';";

pub const ALTER_OTS_TRAJECTORIES_ADD_PERSIST_ATTEMPTS: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN persist_attempts INTEGER NOT NULL DEFAULT 0;";

pub const ALTER_OTS_TRAJECTORIES_ADD_LAST_ERROR: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN last_error TEXT;";

pub const ALTER_OTS_TRAJECTORIES_ADD_UPDATED_AT: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN updated_at TEXT NOT NULL DEFAULT (datetime('now'));";

pub const CREATE_OTS_TRAJECTORIES_AGENT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_agent
    ON ots_trajectories(agent_id);";

pub const CREATE_OTS_TRAJECTORIES_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_tenant
    ON ots_trajectories(tenant);";

pub const CREATE_OTS_TRAJECTORIES_OUTCOME_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_outcome
    ON ots_trajectories(outcome);";

pub const CREATE_OTS_TRAJECTORIES_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_status
    ON ots_trajectories(persistence_status, updated_at);";
