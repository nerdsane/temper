-- Durable high-water marks for query projection removals.
--
-- A projection write is asynchronous relative to the authoritative journal.
-- Keeping the delete sequence after the catalog row is gone prevents a delayed
-- older upsert from recreating a terminal entity in the read plane.

CREATE TABLE IF NOT EXISTS query_projection_tombstones (
    tenant       TEXT NOT NULL,
    entity_type  TEXT NOT NULL,
    entity_id    TEXT NOT NULL,
    sequence_nr  BIGINT NOT NULL,
    deleted_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type, entity_id)
);

ALTER TABLE query_projection_tombstones ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON query_projection_tombstones;
CREATE POLICY tenant_isolation ON query_projection_tombstones
    USING (tenant = current_setting('app.current_tenant', true))
    WITH CHECK (tenant = current_setting('app.current_tenant', true));

CREATE OR REPLACE FUNCTION suppress_stale_entity_catalog_projection()
RETURNS TRIGGER AS $$
BEGIN
    IF EXISTS (
        SELECT 1
          FROM query_projection_tombstones tombstone
         WHERE tombstone.tenant = NEW.tenant
           AND tombstone.entity_type = NEW.entity_type
           AND tombstone.entity_id = NEW.entity_id
           AND tombstone.sequence_nr >= NEW.sequence_nr
    ) THEN
        RETURN NULL;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS suppress_stale_entity_catalog_projection_insert ON entity_catalog;
CREATE TRIGGER suppress_stale_entity_catalog_projection_insert
BEFORE INSERT ON entity_catalog
FOR EACH ROW EXECUTE FUNCTION suppress_stale_entity_catalog_projection();

DROP TRIGGER IF EXISTS suppress_stale_entity_catalog_projection_update ON entity_catalog;
CREATE TRIGGER suppress_stale_entity_catalog_projection_update
BEFORE UPDATE ON entity_catalog
FOR EACH ROW EXECUTE FUNCTION suppress_stale_entity_catalog_projection();

CREATE OR REPLACE FUNCTION suppress_orphan_entity_field_projection()
RETURNS TRIGGER AS $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
          FROM entity_catalog catalog
         WHERE catalog.tenant = NEW.tenant
           AND catalog.entity_type = NEW.entity_type
           AND catalog.entity_id = NEW.entity_id
    ) THEN
        RETURN NULL;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS suppress_orphan_entity_field_projection_insert ON entity_field_index;
CREATE TRIGGER suppress_orphan_entity_field_projection_insert
BEFORE INSERT ON entity_field_index
FOR EACH ROW EXECUTE FUNCTION suppress_orphan_entity_field_projection();

DROP TRIGGER IF EXISTS suppress_orphan_entity_field_projection_update ON entity_field_index;
CREATE TRIGGER suppress_orphan_entity_field_projection_update
BEFORE UPDATE ON entity_field_index
FOR EACH ROW EXECUTE FUNCTION suppress_orphan_entity_field_projection();
