-- Preserve the last committed catalog while replacement bytes are verified.
--
-- A staging row must never overwrite the only restorable committed spec. The
-- verifier promotes one exact IOA+CSDL pair into `specs` atomically.
CREATE TABLE IF NOT EXISTS staged_specs (
    tenant              TEXT        NOT NULL,
    entity_type         TEXT        NOT NULL,
    ioa_source          TEXT        NOT NULL,
    csdl_xml            TEXT,
    content_hash        TEXT        NOT NULL,
    version             INT         NOT NULL DEFAULT 1,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, entity_type)
);

-- Preserve interrupted staging rows created by the previous single-row
-- protocol. Those rows never carried committed authority.
INSERT INTO staged_specs
    (tenant, entity_type, ioa_source, csdl_xml, content_hash, version, created_at, updated_at)
SELECT tenant, entity_type, ioa_source, csdl_xml, content_hash, version, created_at, updated_at
FROM specs
WHERE committed = false
ON CONFLICT (tenant, entity_type) DO UPDATE SET
    ioa_source = EXCLUDED.ioa_source,
    csdl_xml = EXCLUDED.csdl_xml,
    content_hash = EXCLUDED.content_hash,
    version = EXCLUDED.version,
    updated_at = EXCLUDED.updated_at;

DELETE FROM specs WHERE committed = false;

ALTER TABLE staged_specs ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON staged_specs;
CREATE POLICY tenant_isolation ON staged_specs
    USING (tenant = current_setting('app.current_tenant', true))
    WITH CHECK (tenant = current_setting('app.current_tenant', true));
