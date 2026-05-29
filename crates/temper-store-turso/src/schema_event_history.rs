pub const ALTER_EVENTS_ADD_SEGMENT_INDEX: &str =
    "ALTER TABLE events ADD COLUMN segment_index INTEGER NOT NULL DEFAULT 0";

pub const CREATE_EVENT_SEGMENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS event_segments (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    segment_index INTEGER NOT NULL,
    start_sequence_nr INTEGER NOT NULL,
    end_sequence_nr INTEGER,
    snapshot_sequence INTEGER,
    event_count INTEGER NOT NULL DEFAULT 0,
    sealed_at TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, entity_type, entity_id, segment_index)
);";

pub const CREATE_EVENT_SEGMENTS_OPEN_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_event_segments_open
    ON event_segments(tenant, entity_type, entity_id, segment_index DESC)
    WHERE sealed_at IS NULL;";

pub const CREATE_SNAPSHOT_HISTORY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS snapshot_history (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    sequence_nr INTEGER NOT NULL,
    snapshot BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY(tenant, entity_type, entity_id, sequence_nr)
);";

pub const CREATE_SNAPSHOT_HISTORY_ENTITY_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_snapshot_history_entity
    ON snapshot_history(tenant, entity_type, entity_id, sequence_nr DESC);";
