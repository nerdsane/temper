-- The durable gap between the journal and the field projection.
--
-- One row per entity whose catalog row and field index cannot answer an
-- indexed equality read on their own. An append marks its entity at the new
-- sequence in the append's transaction; a projection write removes the mark
-- only through the sequence it applied, so a delayed projection never hides a
-- newer commit. A deleted entity keeps a terminal row: a late projection then
-- cannot bring it back into an indexed match.
--
-- The table and its backfill commit in this one migration transaction, so the
-- table's existence proves the gap is complete. The backfill is the only read
-- of the whole corpus: it marks every entity whose catalog row is missing,
-- behind its journal, deleted, without a field index row (projected before the
-- field index existed), or holding its fields wrapped under `fields`, which
-- the field index does not look inside.
CREATE TABLE IF NOT EXISTS entity_projection_delta (
    tenant      TEXT    NOT NULL,
    entity_type TEXT    NOT NULL,
    entity_id   TEXT    NOT NULL,
    sequence_nr BIGINT  NOT NULL,
    terminal    BOOLEAN NOT NULL DEFAULT false,
    PRIMARY KEY (tenant, entity_type, entity_id)
);

-- The pending range of one type, read by every reconciled equality read.
-- Terminal rows are left out, so deletes never lengthen it.
CREATE INDEX IF NOT EXISTS idx_projection_delta_pending
    ON entity_projection_delta (tenant, entity_type, entity_id) WHERE NOT terminal;

-- The tombstone predicate is `schema::tombstone_sql!()` (a test keeps them equal).
WITH heads AS (
    SELECT tenant, entity_type, entity_id, MAX(sequence_nr) AS sequence_nr FROM (
        SELECT tenant, entity_type, entity_id, MAX(sequence_nr) AS sequence_nr
        FROM events GROUP BY tenant, entity_type, entity_id
        UNION ALL SELECT tenant, entity_type, entity_id, sequence_nr FROM snapshots
        UNION ALL SELECT tenant, entity_type, entity_id, sequence_nr FROM entity_catalog
    ) s GROUP BY tenant, entity_type, entity_id
), deleted AS (
    SELECT DISTINCT d.tenant, d.entity_type, d.entity_id FROM events d
    WHERE d.event_type = 'Deleted'
      AND (d.payload->>'kind' = 'tombstone'
        OR (d.payload->'kind' IS NULL
          AND d.payload->>'to_status' = 'Deleted'
          AND d.payload->'params' = '{}'::jsonb
          AND d.payload->>'idempotency_key' IS NULL))
)
INSERT INTO entity_projection_delta (tenant, entity_type, entity_id, sequence_nr, terminal)
SELECT h.tenant, h.entity_type, h.entity_id, h.sequence_nr, x.entity_id IS NOT NULL
FROM heads h
LEFT JOIN entity_catalog c
  ON c.tenant = h.tenant AND c.entity_type = h.entity_type AND c.entity_id = h.entity_id
LEFT JOIN deleted x
  ON x.tenant = h.tenant AND x.entity_type = h.entity_type AND x.entity_id = h.entity_id
WHERE x.entity_id IS NOT NULL
   OR c.entity_id IS NULL
   OR c.sequence_nr < h.sequence_nr
   OR NOT EXISTS (SELECT 1 FROM entity_field_index f
        WHERE f.tenant = h.tenant AND f.entity_type = h.entity_type AND f.entity_id = h.entity_id)
   OR jsonb_typeof(c.fields -> 'fields') = 'object'
ON CONFLICT (tenant, entity_type, entity_id) DO UPDATE SET
    sequence_nr = GREATEST(entity_projection_delta.sequence_nr, EXCLUDED.sequence_nr),
    terminal = entity_projection_delta.terminal OR EXCLUDED.terminal;

ALTER TABLE entity_projection_delta ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON entity_projection_delta;
CREATE POLICY tenant_isolation ON entity_projection_delta USING (tenant = current_setting('app.current_tenant', true));
