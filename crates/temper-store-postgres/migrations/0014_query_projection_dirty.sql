-- ADR-0192 / ARN-238: make derived query-projection lag explicit and durable.
--
-- Journal and snapshot writers mark their entity dirty in the same stream-fenced
-- transaction as the source mutation. A source-fenced catalog/EAV repair clears
-- the row. Readers repair these bounded rows before trusting native projection
-- pages, so a delete/recreate or same-sequence snapshot rewrite cannot become a
-- silent false negative.
CREATE TABLE IF NOT EXISTS query_projection_dirty (
    tenant TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_query_projection_dirty_type
    ON query_projection_dirty (tenant, entity_type, entity_id);

-- Bridge writes from older binaries during the required drain/upgrade cutover.
-- Source triggers cover old journal/snapshot writers. The catalog trigger
-- covers their delayed asynchronous projector after a new reader has repaired
-- and cleared an earlier marker. Older readers still cannot interpret the new
-- materialization control event, so ADR-0192 requires draining them before the
-- new binary is enabled; these triggers do not replace that reader barrier.
CREATE OR REPLACE FUNCTION temper_mark_query_projection_dirty()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP <> 'INSERT' THEN
        INSERT INTO query_projection_dirty (tenant, entity_type, entity_id)
        VALUES (OLD.tenant, OLD.entity_type, OLD.entity_id)
        ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING;
    END IF;
    IF TG_OP <> 'DELETE' THEN
        INSERT INTO query_projection_dirty (tenant, entity_type, entity_id)
        VALUES (NEW.tenant, NEW.entity_type, NEW.entity_id)
        ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING;
    END IF;
    RETURN NULL;
END;
$$;

DROP TRIGGER IF EXISTS events_mark_query_projection_dirty ON events;
CREATE TRIGGER events_mark_query_projection_dirty
AFTER INSERT OR UPDATE OR DELETE ON events
FOR EACH ROW EXECUTE FUNCTION temper_mark_query_projection_dirty();

DROP TRIGGER IF EXISTS snapshots_mark_query_projection_dirty ON snapshots;
CREATE TRIGGER snapshots_mark_query_projection_dirty
AFTER INSERT OR UPDATE OR DELETE ON snapshots
FOR EACH ROW EXECUTE FUNCTION temper_mark_query_projection_dirty();

DROP TRIGGER IF EXISTS catalog_mark_query_projection_dirty ON entity_catalog;
CREATE TRIGGER catalog_mark_query_projection_dirty
AFTER INSERT OR UPDATE OR DELETE ON entity_catalog
FOR EACH ROW EXECUTE FUNCTION temper_mark_query_projection_dirty();

-- This migration is the publication boundary for the ledger. Seed every
-- authoritative source row so missing projections are repaired, and every
-- existing catalog row so stale projections are fenced before native reads.
-- Catalog-only compatibility rows are source-fenced acknowledgements: repair
-- clears their marker without deleting the row.
INSERT INTO query_projection_dirty (tenant, entity_type, entity_id)
SELECT tenant, entity_type, entity_id FROM events
UNION
SELECT tenant, entity_type, entity_id FROM snapshots
UNION
SELECT tenant, entity_type, entity_id FROM entity_catalog
ON CONFLICT (tenant, entity_type, entity_id) DO NOTHING;
