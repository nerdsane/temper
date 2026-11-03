ALTER TABLE entity_catalog ADD COLUMN IF NOT EXISTS first_event_at TEXT;
CREATE TABLE IF NOT EXISTS first_event_backfill (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    tenant TEXT NOT NULL DEFAULT '', entity_type TEXT NOT NULL DEFAULT '', entity_id TEXT NOT NULL DEFAULT '',
    records BIGINT NOT NULL DEFAULT 0, null_records BIGINT NOT NULL DEFAULT 0,
    complete BOOLEAN NOT NULL DEFAULT false
);
