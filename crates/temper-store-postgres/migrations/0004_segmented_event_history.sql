-- 0004_segmented_event_history.sql
--
-- Add storage-level event segment metadata and immutable snapshot history.
-- Existing event rows remain authoritative and default into segment 0.

ALTER TABLE events
    ADD COLUMN IF NOT EXISTS segment_index BIGINT NOT NULL DEFAULT 0;

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
);

CREATE INDEX IF NOT EXISTS idx_event_segments_open
    ON event_segments (tenant, entity_type, entity_id, segment_index DESC)
    WHERE sealed_at IS NULL;

CREATE TABLE IF NOT EXISTS snapshot_history (
    tenant        TEXT         NOT NULL DEFAULT 'default',
    entity_type   TEXT         NOT NULL,
    entity_id     TEXT         NOT NULL,
    sequence_nr   BIGINT       NOT NULL,
    state         BYTEA        NOT NULL,
    created_at    TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id, sequence_nr)
);

CREATE INDEX IF NOT EXISTS idx_snapshot_history_entity
    ON snapshot_history (tenant, entity_type, entity_id, sequence_nr DESC);

ALTER TABLE event_segments ENABLE ROW LEVEL SECURITY;
ALTER TABLE snapshot_history ENABLE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON event_segments;
CREATE POLICY tenant_isolation ON event_segments USING (tenant = current_setting('app.current_tenant', true));

DROP POLICY IF EXISTS tenant_isolation ON snapshot_history;
CREATE POLICY tenant_isolation ON snapshot_history USING (tenant = current_setting('app.current_tenant', true));
