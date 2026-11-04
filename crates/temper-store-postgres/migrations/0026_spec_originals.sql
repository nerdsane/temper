-- The source a spec was stored with before a boot converted it to the current
-- syntax. Kept by the first conversion and never replaced, so the conversion
-- can be undone by writing it back to `specs`. A new table: no existing row
-- is touched.
CREATE TABLE IF NOT EXISTS spec_originals (
    tenant       TEXT         NOT NULL,
    entity_type  TEXT         NOT NULL,
    ioa_source   TEXT         NOT NULL,
    kept_at      TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type)
);

ALTER TABLE spec_originals ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON spec_originals;
CREATE POLICY tenant_isolation ON spec_originals USING (tenant = current_setting('app.current_tenant', true));
