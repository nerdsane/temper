//! OTS trajectory schema used by the GEPA self-improvement loop.
//!
//! Rows are keyed by `(tenant, trajectory_id)`. A global id let one tenant's
//! upload land on an id another tenant already used, and the upsert would
//! rewrite that row's tenant along with its data; the migration constants
//! below rebuild legacy tables onto the tenant-scoped key.

/// Full OTS trajectory storage for GEPA self-improvement loop.
///
/// Stores complete agent execution traces (tool calls, decisions, reasoning)
/// captured by the MCP server during agent sessions. The `data` column holds
/// the full OTS JSON blob; indexed columns enable efficient filtering.
///
/// Keyed by `(tenant, trajectory_id)`. One store holds every tenant's rows and
/// the id is chosen by the uploading harness, so a global key lets one tenant's
/// upload land on an id another tenant already used — and the upsert would
/// rewrite that row's tenant along with its data. Reads are already
/// tenant-scoped; the identity has to be too.
pub const CREATE_OTS_TRAJECTORIES_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS ots_trajectories (
    trajectory_id TEXT NOT NULL,
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
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, trajectory_id)
);";

/// Whether an existing `ots_trajectories` table is already tenant-scoped.
///
/// SQLite cannot alter a primary key, so the migration below rebuilds the
/// table. This reads the stored DDL to decide whether it has to.
pub const SELECT_OTS_TRAJECTORIES_DDL: &str = "\
SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'ots_trajectories';";

/// Marker the tenant-scoped DDL contains and the legacy DDL does not.
pub const OTS_TRAJECTORIES_TENANT_IDENTITY_MARKER: &str = "PRIMARY KEY(tenant, trajectory_id)";

/// Migration step 1: the tenant-scoped table, under a staging name.
pub const CREATE_OTS_TRAJECTORIES_REBUILD_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS ots_trajectories_rebuild (
    trajectory_id TEXT NOT NULL,
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
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, trajectory_id)
);";

/// Migration step 2: carry every stored row across.
///
/// The old key was global, so no two rows can collide on the new one; the
/// column list is explicit so a table with extra legacy columns still copies.
pub const COPY_OTS_TRAJECTORIES_INTO_REBUILD: &str = "\
INSERT INTO ots_trajectories_rebuild
    (trajectory_id, tenant, agent_id, session_id, outcome, entity_type, turn_count, data,
     persistence_status, persist_attempts, last_error, created_at, updated_at)
SELECT trajectory_id, tenant, agent_id, session_id, outcome, entity_type, turn_count, data,
       persistence_status, persist_attempts, last_error, created_at, updated_at
FROM ots_trajectories;";

/// Migration step 3: drop the globally keyed table.
pub const DROP_OTS_TRAJECTORIES_LEGACY_TABLE: &str = "DROP TABLE ots_trajectories;";

/// Migration step 4: put the rebuilt table in its place.
///
/// The indexes are recreated after this by the same bootstrap that runs the
/// migration, because dropping the old table dropped them with it.
pub const RENAME_OTS_TRAJECTORIES_REBUILD: &str = "\
ALTER TABLE ots_trajectories_rebuild RENAME TO ots_trajectories;";

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
