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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_are_idempotent() {
        assert!(CREATE_EVENTS_TABLE.contains("IF NOT EXISTS"));
        assert!(CREATE_EVENTS_ENTITY_INDEX.contains("IF NOT EXISTS"));
        assert!(CREATE_SNAPSHOTS_TABLE.contains("IF NOT EXISTS"));
    }
}
