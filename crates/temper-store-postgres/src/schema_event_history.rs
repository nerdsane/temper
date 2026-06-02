//! Event history segmentation schema for the PostgreSQL persistence store.

/// ALTER TABLE migration for existing event journals created before segment metadata.
pub const ALTER_EVENTS_ADD_SEGMENT_INDEX: &str =
    "ALTER TABLE events ADD COLUMN IF NOT EXISTS segment_index BIGINT NOT NULL DEFAULT 0";

/// CREATE TABLE statement for event segment metadata.
///
/// A segment groups a bounded tail of lifetime event rows. Snapshot saves seal
/// the current segment and open the next segment; event rows remain the
/// authoritative audit history.
pub const CREATE_EVENT_SEGMENTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS event_segments (
    tenant            TEXT         NOT NULL DEFAULT 'default',
    entity_type       TEXT         NOT NULL,
    entity_id         TEXT         NOT NULL,
    segment_index     BIGINT       NOT NULL,
    start_sequence_nr BIGINT       NOT NULL,
    end_sequence_nr   BIGINT,
    snapshot_sequence BIGINT,
    event_count       BIGINT       NOT NULL DEFAULT 0,
    sealed_at         TIMESTAMPTZ,
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id, segment_index)
);";

/// CREATE INDEX statement for finding the open segment for an entity.
pub const CREATE_EVENT_SEGMENTS_OPEN_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_event_segments_open
    ON event_segments (tenant, entity_type, entity_id, segment_index DESC)
    WHERE sealed_at IS NULL;";

/// CREATE TABLE statement for immutable snapshot history.
///
/// The `snapshots` table remains the latest-snapshot fast path; this table
/// keeps every durable snapshot boundary for audit and segment reconstruction.
pub const CREATE_SNAPSHOT_HISTORY_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS snapshot_history (
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    state         BYTEA        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id, sequence_nr)
);";

/// CREATE INDEX statement for latest-first snapshot history scans.
pub const CREATE_SNAPSHOT_HISTORY_ENTITY_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_snapshot_history_entity
    ON snapshot_history (tenant, entity_type, entity_id, sequence_nr DESC);";
