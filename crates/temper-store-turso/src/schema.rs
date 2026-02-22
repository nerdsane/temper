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
    }
}
