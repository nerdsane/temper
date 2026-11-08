-- Tombstones only.
--
-- Entity listings keep an entity unless its journal has a 'Deleted' event.
-- Postgres answers that NOT EXISTS with a hash anti-join over a scan of every
-- event; this index makes the probe side read only the tombstones. The Turso
-- store carries the same index, where its absence made listings quadratic.
CREATE INDEX IF NOT EXISTS idx_events_deleted
    ON events (tenant, entity_type, entity_id) WHERE event_type = 'Deleted';
