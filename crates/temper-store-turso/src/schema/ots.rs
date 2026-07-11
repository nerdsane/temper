//! OTS trajectory schema used by the GEPA self-improvement loop.

/// Full OTS trajectory storage for complete agent execution traces.
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

/// Add durable outbox persistence status to legacy OTS tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_PERSISTENCE_STATUS: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN persistence_status TEXT NOT NULL DEFAULT 'persisted';";

/// Add durable outbox attempt accounting to legacy OTS tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_PERSIST_ATTEMPTS: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN persist_attempts INTEGER NOT NULL DEFAULT 0;";

/// Add the last persistence error to legacy OTS tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_LAST_ERROR: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN last_error TEXT;";

/// Add the outbox update timestamp to legacy OTS tables.
pub const ALTER_OTS_TRAJECTORIES_ADD_UPDATED_AT: &str = "\
ALTER TABLE ots_trajectories ADD COLUMN updated_at TEXT NOT NULL DEFAULT (datetime('now'));";

/// Index OTS trajectories by agent.
pub const CREATE_OTS_TRAJECTORIES_AGENT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_agent
    ON ots_trajectories(agent_id);";

/// Index OTS trajectories by tenant.
pub const CREATE_OTS_TRAJECTORIES_TENANT_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_tenant
    ON ots_trajectories(tenant);";

/// Index OTS trajectories by outcome.
pub const CREATE_OTS_TRAJECTORIES_OUTCOME_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_outcome
    ON ots_trajectories(outcome);";

/// Index the durable OTS outbox scan by status and update time.
pub const CREATE_OTS_TRAJECTORIES_STATUS_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_ots_trajectories_status
    ON ots_trajectories(persistence_status, updated_at);";
